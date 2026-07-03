use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use fuser::Notifier;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::error::{BranchError, Result};
use crate::inode::ROOT_INO;
use crate::storage;

const PATH_LOCK_STRIPES: usize = 256;

/// Tracks storage usage across all branches and enforces an optional quota.
/// Only counts delta files (the actual disk cost of branching).
pub struct StorageQuota {
    /// Current total bytes used in the storage directory (all branches' deltas).
    used_bytes: AtomicI64,
    /// Maximum allowed bytes. None = unlimited.
    max_bytes: Option<u64>,
}

impl StorageQuota {
    pub fn new(max_bytes: Option<u64>) -> Self {
        Self {
            used_bytes: AtomicI64::new(0),
            max_bytes,
        }
    }

    /// Walk the storage directory to compute initial usage.
    pub fn scan_usage(storage_path: &Path, max_bytes: Option<u64>) -> Self {
        let quota = Self::new(max_bytes);
        let branches_dir = storage_path.join("branches");
        if branches_dir.exists() {
            let bytes = Self::dir_size(&branches_dir);
            quota.used_bytes.store(bytes as i64, Ordering::Relaxed);
        }
        quota
    }

    fn dir_size(path: &Path) -> u64 {
        let mut total = 0u64;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    total += Self::dir_size(&p);
                } else if let Ok(meta) = p.symlink_metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// Check if `additional` bytes can be allocated. Returns Err(ENOSPC) if not.
    pub fn check(&self, additional: u64) -> std::result::Result<(), i32> {
        if let Some(max) = self.max_bytes {
            let current = self.used_bytes.load(Ordering::Relaxed);
            if current as u64 + additional > max {
                return Err(libc::ENOSPC);
            }
        }
        Ok(())
    }

    /// Record that `bytes` were added to storage.
    pub fn add(&self, bytes: u64) {
        self.used_bytes.fetch_add(bytes as i64, Ordering::Relaxed);
    }

    /// Record that `bytes` were removed from storage.
    pub fn sub(&self, bytes: u64) {
        self.used_bytes.fetch_sub(bytes as i64, Ordering::Relaxed);
    }

    pub fn used(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed).max(0) as u64
    }

    pub fn max(&self) -> Option<u64> {
        self.max_bytes
    }
}

/// Remove a file or directory at `path`, following symlinks for the type check.
/// Returns `Ok(())` even if the path doesn't exist; propagates real I/O errors.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_dir() => remove_branch_store_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Temporarily add owner rwx bits to an internal branch-store directory so the
/// supervisor can inspect it even if the agent-visible mode is 000. The original
/// mode is restored when the guard is dropped.
struct StoreDirModeGuard {
    path: PathBuf,
    original_mode: Option<u32>,
}

impl StoreDirModeGuard {
    fn new(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            original_mode: add_owner_rwx_if_dir(path)?,
        })
    }
}

impl Drop for StoreDirModeGuard {
    fn drop(&mut self) {
        if let Some(mode) = self.original_mode {
            use std::os::unix::fs::PermissionsExt;

            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(mode));
        }
    }
}

fn guard_existing_parent_dirs(path: &Path) -> std::io::Result<Vec<StoreDirModeGuard>> {
    let mut dirs: Vec<PathBuf> = path.ancestors().skip(1).map(Path::to_path_buf).collect();
    dirs.reverse();
    let mut guards = Vec::new();
    for dir in dirs {
        match dir.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => guards.push(StoreDirModeGuard::new(&dir)?),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(guards)
}

fn add_owner_rwx_if_dir(path: &Path) -> std::io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;

    let meta = path.symlink_metadata()?;
    if !meta.file_type().is_dir() {
        return Ok(None);
    }

    let mode = meta.permissions().mode();
    if mode & 0o700 == 0o700 {
        return Ok(None);
    }

    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    Ok(Some(mode))
}

fn make_branch_store_tree_removable(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    let _ = add_owner_rwx_if_dir(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if child
            .symlink_metadata()
            .map(|meta| meta.file_type().is_dir())
            .unwrap_or(false)
        {
            make_branch_store_tree_removable(&child)?;
        }
    }

    Ok(())
}

fn remove_branch_store_dir_all(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_dir() => {
            make_branch_store_tree_removable(path)?;
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn invalid_offset() -> BranchError {
    BranchError::Io(std::io::Error::from_raw_os_error(libc::EINVAL))
}

fn file_too_large() -> BranchError {
    BranchError::Io(std::io::Error::from_raw_os_error(libc::EFBIG))
}

fn write_data_to_file(path: &Path, offset: u64, data: &[u8]) -> Result<u32> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    Ok(file.write(data)? as u32)
}

/// An in-progress merge that prepares all of its destructive work as
/// rollback-able side files, then publishes it in one near-infallible phase.
///
/// Two kinds of work are staged:
/// - **copies** — each source file is copied to a temp sibling of its final
///   destination (`.branchfs-tmp.<name>`); the only error-prone step.
/// - **deletions** — each entry to be removed is renamed aside to a trash
///   sibling (`.branchfs-trash.<name>`) instead of being deleted outright.
///
/// `commit` then renames every copy temp into place and discards the trashed
/// entries. If dropped without `commit` (e.g. an error propagated during
/// staging), every copy temp is removed and every trashed entry is renamed
/// back, leaving the destination exactly as it was — so a failed merge can
/// neither be reported as success nor partially mutate the destination.
///
/// Staging a deletion before staging copies under the same path also clears any
/// type conflict (e.g. replacing a directory with a file): the conflicting
/// entry is moved out of the way before the copy's parent directories are
/// created and before the temp is renamed into place.
///
/// Note: a copy's `copy_entry` creates the destination's parent directories, so
/// a rolled-back merge may leave empty directories behind — this is benign (no
/// data is written or lost).
#[derive(Default)]
struct StagedMerge {
    /// (temp path, final destination) for each copied file.
    copies: Vec<(PathBuf, PathBuf)>,
    /// (trash path, original path) for each deletion staged aside.
    deletes: Vec<(PathBuf, PathBuf)>,
    /// Relative paths copied, returned to the caller on `commit` for bookkeeping.
    merged: Vec<String>,
    /// Total bytes copied, for the `[BENCH]` log line.
    bytes: u64,
    committed: bool,
}

impl StagedMerge {
    fn new() -> Self {
        Self::default()
    }

    /// Stage a deletion: rename `target` aside to a trash sibling so it can be
    /// restored on rollback. No-op if `target` does not exist.
    fn stage_delete(&mut self, target: &Path) -> Result<()> {
        if target.symlink_metadata().is_err() {
            return Ok(()); // nothing to delete
        }
        let trash = commit_side_path(target, "trash");
        // Clear any stale trash left by a previously crashed commit so the
        // rename below cannot fail with EEXIST/ENOTEMPTY.
        let _ = remove_entry(&trash);
        fs::rename(target, &trash)?;
        self.deletes.push((trash, target.to_path_buf()));
        Ok(())
    }

    /// Stage a copy: copy `src` to a temp sibling of its final destination under
    /// `dest_root`. The only error-prone step; no existing destination data is
    /// touched (the temp is published only in `commit`).
    fn stage_copy(&mut self, rel_path: &str, src: &Path, dest_root: &Path) -> Result<()> {
        let dest = dest_root.join(rel_path.trim_start_matches('/'));
        // A file or symlink cannot be renamed over an existing directory. If the
        // destination is currently a directory (a path that changed from dir to
        // file, e.g. via rename, which leaves no tombstone), stage its removal
        // first so the publish rename can place the file. Renaming over an
        // existing file or symlink is fine, so those are left to `commit`.
        if dest
            .symlink_metadata()
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
        {
            self.stage_delete(&dest)?;
        }
        let tmp = commit_side_path(&dest, "tmp");
        let _source_guards = guard_existing_parent_dirs(src)?;
        storage::copy_entry(src, &tmp)?;
        if let Ok(meta) = src.symlink_metadata() {
            self.bytes += meta.len();
        }
        self.copies.push((tmp, dest));
        self.merged.push(rel_path.to_string());
        Ok(())
    }

    /// Publish: rename every staged copy into its final place and discard the
    /// trashed deletions. Returns the relative paths that were merged.
    fn commit(mut self) -> Result<Vec<String>> {
        for (tmp, dest) in &self.copies {
            fs::rename(tmp, dest)?;
        }
        for (trash, _) in &self.deletes {
            remove_entry(trash)?;
        }
        self.committed = true;
        Ok(std::mem::take(&mut self.merged))
    }
}

impl Drop for StagedMerge {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Roll back: drop copy temps, restore trashed deletions to their
        // original paths so the destination is left untouched.
        for (tmp, _) in &self.copies {
            let _ = remove_entry(tmp);
        }
        for (trash, original) in &self.deletes {
            let _ = fs::rename(trash, original);
        }
    }
}

/// A side-file path next to `target`: `<dir>/.branchfs-<tag>.<name>`.
/// The side file lives in the same directory as `target`, so renaming between
/// them is an atomic same-filesystem operation. Built from the raw `OsStr`
/// (not a lossy `String`) so distinct non-UTF8 names never collide.
fn commit_side_path(target: &Path, tag: &str) -> PathBuf {
    let mut name = std::ffi::OsString::from(format!(".branchfs-{}.", tag));
    name.push(target.file_name().unwrap_or_default());
    target.with_file_name(name)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InheritanceMode {
    /// Resolve inherited paths recursively from the parent/base at lookup time.
    /// Branch creation is O(1) and does not scan or copy the inherited tree.
    Lazy,
    /// Preserve the legacy behavior: recursively snapshot the visible parent
    /// tree into this branch's inherited directory at branch creation time.
    Snapshot,
}

impl Default for InheritanceMode {
    fn default() -> Self {
        Self::Lazy
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    Open,
    Frozen,
}

impl Default for BranchState {
    fn default() -> Self {
        Self::Open
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BranchMetadata {
    name: String,
    parent: Option<String>,
    inheritance: InheritanceMode,
    state: BranchState,
    parent_version_at_fork: u64,
    commit_count: u64,
    /// Inherited paths masked from this branch's view (agent-safe secret
    /// hiding). Stored as normalized `/`-prefixed relative paths.
    #[serde(default)]
    hide_paths: Vec<String>,
}

/// Normalize a hide path to a `/`-prefixed, no-trailing-slash relative path.
/// Returns None for entries that would be empty or hide the entire root.
fn normalize_hide_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("/{}", trimmed))
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    pub op: String,
    pub path: String,
    pub kind: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchStatus {
    pub name: String,
    pub parent: Option<String>,
    pub inheritance: InheritanceMode,
    pub state: BranchState,
    pub parent_version_at_fork: u64,
    pub commit_count: u64,
    pub delta_entries: usize,
    pub tombstones: usize,
    pub diff: Vec<DiffEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathIdentity {
    pub exists: bool,
    pub kind: String,
    pub bytes: u64,
    pub mtime_ns: Option<i128>,
}

impl PathIdentity {
    fn missing() -> Self {
        Self {
            exists: false,
            kind: "missing".to_string(),
            bytes: 0,
            mtime_ns: None,
        }
    }

    fn from_path(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::missing();
        };
        let Ok(meta) = path.symlink_metadata() else {
            return Self::missing();
        };
        let kind = if meta.file_type().is_dir() {
            "dir"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.file_type().is_file() {
            "file"
        } else {
            "other"
        };
        let mtime_ns = meta.modified().ok().and_then(|t| {
            t.duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as i128)
        });
        Self {
            exists: true,
            kind: kind.to_string(),
            bytes: if meta.file_type().is_file() {
                meta.len()
            } else {
                0
            },
            mtime_ns,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchRecord {
    pub path: String,
    pub base_at_first_touch: PathIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_content_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitAutoMerge {
    pub path: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitConflict {
    pub path: String,
    pub session_action: String,
    pub resolution: String,
    pub reason: String,
    pub base_at_first_touch: PathIdentity,
    pub base_at_commit: PathIdentity,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitOutcome {
    pub parent: String,
    pub auto_merges: Vec<CommitAutoMerge>,
    pub conflicts: Vec<CommitConflict>,
}

const MAX_TEXT_MERGE_BYTES: u64 = 1024 * 1024;

fn touch_content_key(rel_path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(rel_path.len() * 2 + 5);
    for &byte in rel_path.as_bytes() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out.push_str(".base");
    out
}

fn read_bounded_text(path: &Path) -> Result<Option<Vec<u8>>> {
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_file() || meta.len() > MAX_TEXT_MERGE_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn split_lines_keepends(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(|s| s.to_string()).collect()
}

fn changed_range(base: &[String], changed: &[String]) -> (usize, usize, Vec<String>) {
    let mut prefix = 0;
    while prefix < base.len() && prefix < changed.len() && base[prefix] == changed[prefix] {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < base.len().saturating_sub(prefix)
        && suffix < changed.len().saturating_sub(prefix)
        && base[base.len() - 1 - suffix] == changed[changed.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let base_end = base.len() - suffix;
    let changed_end = changed.len() - suffix;
    (prefix, base_end, changed[prefix..changed_end].to_vec())
}

fn try_three_way_text_merge(base: &[u8], current: &[u8], session: &[u8]) -> Option<Vec<u8>> {
    if current == session {
        return Some(session.to_vec());
    }
    if base == current {
        return Some(session.to_vec());
    }
    if base == session {
        return Some(current.to_vec());
    }

    let base_text = std::str::from_utf8(base).ok()?;
    let current_text = std::str::from_utf8(current).ok()?;
    let session_text = std::str::from_utf8(session).ok()?;
    let base_lines = split_lines_keepends(base_text);
    let current_lines = split_lines_keepends(current_text);
    let session_lines = split_lines_keepends(session_text);

    let (cur_start, cur_end, cur_repl) = changed_range(&base_lines, &current_lines);
    let (ses_start, ses_end, ses_repl) = changed_range(&base_lines, &session_lines);

    let non_overlapping = cur_end <= ses_start || ses_end <= cur_start;
    if !non_overlapping {
        return None;
    }

    let mut merged = Vec::new();
    if cur_start <= ses_start {
        merged.extend_from_slice(&base_lines[..cur_start]);
        merged.extend(cur_repl);
        merged.extend_from_slice(&base_lines[cur_end..ses_start]);
        merged.extend(ses_repl);
        merged.extend_from_slice(&base_lines[ses_end..]);
    } else {
        merged.extend_from_slice(&base_lines[..ses_start]);
        merged.extend(ses_repl);
        merged.extend_from_slice(&base_lines[ses_end..cur_start]);
        merged.extend(cur_repl);
        merged.extend_from_slice(&base_lines[cur_end..]);
    }

    Some(merged.concat().into_bytes())
}

pub struct Branch {
    pub name: String,
    pub parent: Option<String>,
    pub files_dir: PathBuf,
    pub inherited_dir: PathBuf,
    pub tombstones_file: PathBuf,
    tombstones_log_file: PathBuf,
    touches_log_file: PathBuf,
    touch_content_dir: PathBuf,
    staging_dir: PathBuf,
    meta_file: PathBuf,
    pub inheritance: InheritanceMode,
    state: RwLock<BranchState>,
    tombstones: RwLock<HashSet<String>>,
    touches: RwLock<HashMap<String, TouchRecord>>,
    /// How many children have been committed (merged) into this branch.
    pub commit_count: AtomicU64,
    /// Parent's commit_count at the time this branch was forked.
    pub parent_version_at_fork: u64,
    /// Inherited paths masked from this branch's view. The branch's own
    /// deltas always stay visible; hiding only blocks parent/base
    /// fallthrough, so hidden underlay data can never be read or COWed.
    pub hide_paths: Vec<String>,
}

impl Branch {
    pub fn new(
        name: &str,
        parent: Option<&str>,
        storage_path: &Path,
        parent_version_at_fork: u64,
        inheritance: InheritanceMode,
        hide_paths: Vec<String>,
    ) -> Result<Self> {
        let branch_dir = storage_path.join("branches").join(name);
        let files_dir = branch_dir.join("files");
        let inherited_dir = branch_dir.join("inherited");
        let tombstones_file = branch_dir.join("tombstones");
        let tombstones_log_file = branch_dir.join("tombstones.log");
        let touches_file = branch_dir.join("touches.json");
        let touches_log_file = branch_dir.join("touches.log");
        let touch_content_dir = branch_dir.join("touch-content");
        let staging_dir = branch_dir.join("staging");
        let meta_file = branch_dir.join("meta.json");

        fs::create_dir_all(&files_dir)?;
        fs::create_dir_all(&inherited_dir)?;
        fs::create_dir_all(&touch_content_dir)?;
        fs::create_dir_all(&staging_dir)?;
        if !tombstones_file.exists() {
            File::create(&tombstones_file)?;
        }

        let tombstones = Self::load_tombstones(&tombstones_file, &tombstones_log_file)?;
        let touches = Self::load_touches(&touches_file, &touches_log_file)?;

        let branch = Self {
            name: name.to_string(),
            parent: parent.map(|s| s.to_string()),
            files_dir,
            inherited_dir,
            tombstones_file,
            tombstones_log_file,
            touches_log_file,
            touch_content_dir,
            staging_dir,
            meta_file,
            inheritance,
            state: RwLock::new(BranchState::Open),
            tombstones: RwLock::new(tombstones),
            touches: RwLock::new(touches),
            commit_count: AtomicU64::new(0),
            parent_version_at_fork,
            hide_paths: hide_paths
                .iter()
                .filter_map(|p| normalize_hide_path(p))
                .collect(),
        };
        branch.write_metadata()?;
        Ok(branch)
    }

    pub fn load(storage_path: &Path, name: &str) -> Result<Self> {
        let branch_dir = storage_path.join("branches").join(name);
        let files_dir = branch_dir.join("files");
        let inherited_dir = branch_dir.join("inherited");
        let tombstones_file = branch_dir.join("tombstones");
        let tombstones_log_file = branch_dir.join("tombstones.log");
        let touches_file = branch_dir.join("touches.json");
        let touches_log_file = branch_dir.join("touches.log");
        let touch_content_dir = branch_dir.join("touch-content");
        let staging_dir = branch_dir.join("staging");
        let meta_file = branch_dir.join("meta.json");

        let meta_data = fs::read_to_string(&meta_file)?;
        let metadata: BranchMetadata = serde_json::from_str(&meta_data)?;

        fs::create_dir_all(&files_dir)?;
        fs::create_dir_all(&inherited_dir)?;
        fs::create_dir_all(&touch_content_dir)?;
        fs::create_dir_all(&staging_dir)?;
        if !tombstones_file.exists() {
            File::create(&tombstones_file)?;
        }
        let tombstones = Self::load_tombstones(&tombstones_file, &tombstones_log_file)?;
        let touches = Self::load_touches(&touches_file, &touches_log_file)?;

        Ok(Self {
            name: metadata.name,
            parent: metadata.parent,
            files_dir,
            inherited_dir,
            tombstones_file,
            tombstones_log_file,
            touches_log_file,
            touch_content_dir,
            staging_dir,
            meta_file,
            inheritance: metadata.inheritance,
            state: RwLock::new(metadata.state),
            tombstones: RwLock::new(tombstones),
            touches: RwLock::new(touches),
            commit_count: AtomicU64::new(metadata.commit_count),
            parent_version_at_fork: metadata.parent_version_at_fork,
            hide_paths: metadata.hide_paths,
        })
    }

    pub fn write_metadata(&self) -> Result<()> {
        let metadata = BranchMetadata {
            name: self.name.clone(),
            parent: self.parent.clone(),
            inheritance: self.inheritance,
            state: *self.state.read(),
            parent_version_at_fork: self.parent_version_at_fork,
            commit_count: self.commit_count.load(Ordering::SeqCst),
            hide_paths: self.hide_paths.clone(),
        };
        let data = serde_json::to_vec_pretty(&metadata)?;
        storage::ensure_parent_dirs(&self.meta_file)?;
        let tmp = self
            .meta_file
            .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &self.meta_file)?;
        Ok(())
    }

    /// True if `rel_path` (or an ancestor of it) is masked by a hide rule.
    /// Hiding only affects inherited resolution: the branch's own deltas
    /// remain visible even at hidden paths.
    pub fn is_hidden(&self, rel_path: &str) -> bool {
        if self.hide_paths.is_empty() {
            return false;
        }
        let Some(path) = normalize_hide_path(rel_path) else {
            return false;
        };
        self.hide_paths
            .iter()
            .any(|hidden| path == *hidden || path.starts_with(&format!("{}/", hidden)))
    }

    pub fn state(&self) -> BranchState {
        *self.state.read()
    }

    pub fn is_writable(&self) -> bool {
        self.state() == BranchState::Open
    }

    pub fn set_state(&self, state: BranchState) -> Result<()> {
        *self.state.write() = state;
        self.write_metadata()
    }

    fn load_tombstones(snapshot_path: &Path, log_path: &Path) -> Result<HashSet<String>> {
        let mut set = HashSet::new();
        if snapshot_path.exists() {
            let file = File::open(snapshot_path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    set.insert(line);
                }
            }
        }

        if log_path.exists() {
            let file = File::open(log_path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(path) = line.strip_prefix("+ ") {
                    set.insert(path.to_string());
                } else if let Some(path) = line.strip_prefix("- ") {
                    set.remove(path);
                } else {
                    // Tolerate a bare-path log line as an add for forward/backward
                    // compatibility with simple append-only tombstone stores.
                    set.insert(line);
                }
            }
        }
        Ok(set)
    }

    fn append_tombstone_op(&self, op: char, path: &str) -> Result<()> {
        storage::ensure_parent_dirs(&self.tombstones_log_file)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.tombstones_log_file)?;
        writeln!(file, "{} {}", op, path)?;
        Ok(())
    }

    fn load_touches(snapshot_path: &Path, log_path: &Path) -> Result<HashMap<String, TouchRecord>> {
        let mut touches = if !snapshot_path.exists() {
            HashMap::new()
        } else {
            let data = fs::read(snapshot_path)?;
            if data.is_empty() {
                HashMap::new()
            } else {
                serde_json::from_slice(&data)?
            }
        };

        if log_path.exists() {
            let file = File::open(log_path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: TouchRecord = serde_json::from_str(&line)?;
                touches.insert(record.path.clone(), record);
            }
        }

        Ok(touches)
    }

    fn append_touch(&self, record: &TouchRecord) -> Result<()> {
        storage::ensure_parent_dirs(&self.touches_log_file)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.touches_log_file)?;
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
        Ok(())
    }

    fn touch_content_path(&self, key: &str) -> PathBuf {
        self.touch_content_dir.join(key)
    }

    pub fn get_touch_record(&self, rel_path: &str) -> Option<TouchRecord> {
        self.touches.read().get(rel_path).cloned()
    }

    pub fn record_first_touch(
        &self,
        rel_path: &str,
        inherited: Option<&Path>,
        capture_base_content: bool,
    ) -> Result<()> {
        let mut touches = self.touches.write();
        if touches.contains_key(rel_path) {
            return Ok(());
        }

        let base_identity = PathIdentity::from_path(inherited);
        let mut base_content_key = None;
        if capture_base_content {
            if let Some(path) = inherited {
                if let Some(bytes) = read_bounded_text(path)? {
                    let key = touch_content_key(rel_path);
                    let content_path = self.touch_content_path(&key);
                    storage::ensure_parent_dirs(&content_path)?;
                    fs::write(&content_path, bytes)?;
                    base_content_key = Some(key);
                }
            }
        }

        let record = TouchRecord {
            path: rel_path.to_string(),
            base_at_first_touch: base_identity,
            base_content_key,
        };
        touches.insert(rel_path.to_string(), record.clone());
        self.append_touch(&record)
    }

    pub fn read_touch_base_content(&self, record: &TouchRecord) -> Result<Option<Vec<u8>>> {
        let Some(key) = &record.base_content_key else {
            return Ok(None);
        };
        let path = self.touch_content_path(key);
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_deleted(&self, path: &str) -> bool {
        let tombstones = self.tombstones.read();
        if tombstones.contains(path) {
            return true;
        }

        // A tombstoned directory hides inherited descendants too. This keeps
        // lazy lookup from falling through to parent/base children after an
        // inherited directory is removed in this branch.
        path.trim_start_matches('/')
            .split('/')
            .scan(String::new(), |prefix, part| {
                if part.is_empty() {
                    None
                } else {
                    prefix.push('/');
                    prefix.push_str(part);
                    Some(prefix.clone())
                }
            })
            .any(|ancestor| tombstones.contains(&ancestor))
    }

    pub fn add_tombstone(&self, path: &str) -> Result<()> {
        let mut tombstones = self.tombstones.write();
        if tombstones.insert(path.to_string()) {
            self.append_tombstone_op('+', path)?;
        }
        Ok(())
    }

    pub fn remove_tombstone(&self, path: &str) -> Result<()> {
        let mut tombstones = self.tombstones.write();
        if tombstones.remove(path) {
            self.append_tombstone_op('-', path)?;
        }
        Ok(())
    }

    /// Rewrite the tombstones snapshot from the in-memory set and clear the
    /// incremental log. This is used by commit/compaction-style paths, not by
    /// the per-file delete/write hot path.
    fn rewrite_tombstones(&self, tombstones: &HashSet<String>) -> Result<()> {
        let mut file = File::create(&self.tombstones_file)?;
        for t in tombstones {
            writeln!(file, "{}", t)?;
        }
        match fs::remove_file(&self.tombstones_log_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub fn get_tombstones(&self) -> HashSet<String> {
        self.tombstones.read().clone()
    }

    /// Replace the in-memory tombstone set and rewrite the tombstones file.
    pub fn set_tombstones(&self, new_tombstones: HashSet<String>) -> Result<()> {
        let mut tombstones = self.tombstones.write();
        *tombstones = new_tombstones;
        self.rewrite_tombstones(&tombstones)?;
        Ok(())
    }

    pub fn delta_path(&self, rel_path: &str) -> PathBuf {
        self.files_dir.join(rel_path.trim_start_matches('/'))
    }

    pub fn staging_path(&self) -> PathBuf {
        self.staging_dir
            .join(format!("cow-{}.tmp", uuid::Uuid::new_v4()))
    }

    pub fn inherited_path(&self, rel_path: &str) -> PathBuf {
        self.inherited_dir.join(rel_path.trim_start_matches('/'))
    }

    pub fn has_delta(&self, rel_path: &str) -> bool {
        self.delta_path(rel_path).symlink_metadata().is_ok()
    }
}

fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(BranchError::Invalid("branch name cannot be empty".into()));
    }
    if name == "." || name == ".." {
        return Err(BranchError::Invalid(format!(
            "'{}' is not a valid branch name",
            name
        )));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(BranchError::Invalid(
            "branch name cannot contain '/' or null bytes".into(),
        ));
    }
    if name.starts_with('@') {
        return Err(BranchError::Invalid(
            "branch name cannot start with '@' (reserved for virtual paths)".into(),
        ));
    }
    if name.len() > 255 {
        return Err(BranchError::Invalid(
            "branch name cannot exceed 255 characters".into(),
        ));
    }
    Ok(())
}

pub struct BranchManager {
    pub storage_path: PathBuf,
    pub base_path: PathBuf,
    pub workspace_path: PathBuf,
    branches: RwLock<HashMap<String, Branch>>,
    pub epoch: AtomicU64,
    /// Notifiers for invalidating kernel cache on commit/abort
    /// Maps (branch_name, mountpoint) -> Notifier
    notifiers: Mutex<HashMap<(String, PathBuf), Arc<Notifier>>>,
    /// Track opened file inodes per branch for cache invalidation
    /// Maps branch_name -> Set of inodes
    opened_inodes: Mutex<HashMap<String, HashSet<u64>>>,
    /// Current branch per mount — single source of truth
    mount_branches: RwLock<HashMap<PathBuf, String>>,
    /// Storage quota enforcement
    pub quota: StorageQuota,
    /// Striped path locks serialize same-file COW/delete/write operations
    /// without allocating one lock per path in large generated trees.
    path_locks: Vec<Mutex<()>>,
}

impl BranchManager {
    pub fn new(
        storage_path: PathBuf,
        base_path: PathBuf,
        workspace_path: PathBuf,
        max_storage: Option<u64>,
    ) -> Result<Self> {
        fs::create_dir_all(&storage_path)?;

        let quota = StorageQuota::scan_usage(&storage_path, max_storage);

        // Load any branches already represented on disk. This keeps branch
        // metadata/deltas durable across daemon restarts and is a step toward a
        // shared-store/NFS model. Legacy branch directories without meta.json are
        // ignored rather than deleted.
        let mut branches = HashMap::new();
        let branches_dir = storage_path.join("branches");
        if let Ok(entries) = fs::read_dir(&branches_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.path().join("meta.json").exists() {
                    match Branch::load(&storage_path, &name) {
                        Ok(branch) => {
                            branches.insert(name, branch);
                        }
                        Err(e) => log::warn!("failed to load branch '{}': {}", name, e),
                    }
                }
            }
        }

        if !branches.contains_key("main") {
            let main_branch = Branch::new(
                "main",
                None,
                &storage_path,
                0,
                InheritanceMode::Lazy,
                Vec::new(),
            )?;
            branches.insert("main".to_string(), main_branch);
        }

        Ok(Self {
            storage_path,
            base_path,
            workspace_path,
            branches: RwLock::new(branches),
            epoch: AtomicU64::new(0),
            notifiers: Mutex::new(HashMap::new()),
            opened_inodes: Mutex::new(HashMap::new()),
            mount_branches: RwLock::new(HashMap::new()),
            quota,
            path_locks: (0..PATH_LOCK_STRIPES).map(|_| Mutex::new(())).collect(),
        })
    }

    fn path_lock(&self, branch_name: &str, rel_path: &str) -> &Mutex<()> {
        let mut hasher = DefaultHasher::new();
        branch_name.hash(&mut hasher);
        rel_path.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % self.path_locks.len();
        &self.path_locks[idx]
    }

    /// Register a mount's initial branch (called before FUSE spawn).
    pub fn set_mount_branch(&self, mountpoint: &Path, branch: &str) {
        self.mount_branches
            .write()
            .insert(mountpoint.to_path_buf(), branch.to_string());
    }

    /// Read the current branch for a mount.
    pub fn get_mount_branch(&self, mountpoint: &Path) -> Option<String> {
        self.mount_branches.read().get(mountpoint).cloned()
    }

    /// Atomically switch a mount's branch, re-keying the notifier map.
    pub fn switch_mount_branch(&self, mountpoint: &Path, new_branch: &str) {
        // Lock order: mount_branches → notifiers (never reversed)
        let mut mb = self.mount_branches.write();
        let old_branch = mb.insert(mountpoint.to_path_buf(), new_branch.to_string());

        // Re-key notifier from (old, mount) → (new, mount)
        if let Some(old) = old_branch {
            let mut notifiers = self.notifiers.lock();
            if let Some(notifier) = notifiers.remove(&(old.clone(), mountpoint.to_path_buf())) {
                notifiers.insert((new_branch.to_string(), mountpoint.to_path_buf()), notifier);
            }
            log::info!(
                "switch_mount_branch: {:?} '{}' -> '{}'",
                mountpoint,
                old,
                new_branch
            );
        }
    }

    /// Remove a mount's branch tracking and notifier (used on unmount).
    pub fn unregister_mount(&self, mountpoint: &Path) {
        let mut mb = self.mount_branches.write();
        let old_branch = mb.remove(mountpoint);
        if let Some(old) = old_branch {
            self.notifiers
                .lock()
                .remove(&(old, mountpoint.to_path_buf()));
        }
    }

    pub fn create_branch(&self, name: &str, parent: &str) -> Result<()> {
        self.create_branch_with_mode(name, parent, InheritanceMode::Lazy)
    }

    pub fn create_branch_with_mode(
        &self,
        name: &str,
        parent: &str,
        inheritance: InheritanceMode,
    ) -> Result<()> {
        self.create_branch_with_options(name, parent, inheritance, Vec::new())
    }

    pub fn create_branch_with_options(
        &self,
        name: &str,
        parent: &str,
        inheritance: InheritanceMode,
        hide_paths: Vec<String>,
    ) -> Result<()> {
        let start = Instant::now();
        validate_branch_name(name)?;

        let mut branches = self.branches.write();

        if branches.contains_key(name) {
            return Err(BranchError::AlreadyExists(name.to_string()));
        }

        let parent_branch = branches
            .get(parent)
            .ok_or_else(|| BranchError::ParentNotFound(parent.to_string()))?;
        let parent_version = parent_branch.commit_count.load(Ordering::SeqCst);

        let branch = Branch::new(
            name,
            Some(parent),
            &self.storage_path,
            parent_version,
            inheritance,
            hide_paths,
        )?;
        if inheritance == InheritanceMode::Snapshot {
            if let Err(e) = self.snapshot_visible_tree(&branches, parent, &branch.inherited_dir) {
                let _ = remove_branch_store_dir_all(&self.storage_path.join("branches").join(name));
                return Err(e);
            }
            branch.write_metadata()?;
        }
        branches.insert(name.to_string(), branch);
        self.epoch.fetch_add(1, Ordering::SeqCst);

        let elapsed = start.elapsed();
        log::debug!(
            "[BENCH] create_branch '{}' ({:?}): {:?} ({} us)",
            name,
            inheritance,
            elapsed,
            elapsed.as_micros()
        );

        Ok(())
    }

    pub fn get_branch(&self, _name: &str) -> Option<std::sync::Arc<Branch>> {
        // Note: This is a simplified version. In production, use Arc properly.
        None
    }

    pub fn with_branch<F, R>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Branch) -> Result<R>,
    {
        let branches = self.branches.read();
        let branch = branches
            .get(name)
            .ok_or_else(|| BranchError::NotFound(name.to_string()))?;
        f(branch)
    }

    pub fn get_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn is_branch_valid(&self, name: &str) -> bool {
        self.branches.read().contains_key(name)
    }

    pub fn list_branches(&self) -> Vec<(String, Option<String>)> {
        self.branches
            .read()
            .iter()
            .map(|(name, branch)| (name.clone(), branch.parent.clone()))
            .collect()
    }

    /// Register a notifier for a mounted branch
    pub fn register_notifier(
        &self,
        branch_name: &str,
        mountpoint: PathBuf,
        notifier: Arc<Notifier>,
    ) {
        self.notifiers
            .lock()
            .insert((branch_name.to_string(), mountpoint), notifier);
    }

    /// Unregister a notifier when unmounting
    pub fn unregister_notifier(&self, branch_name: &str, mountpoint: &Path) {
        self.notifiers
            .lock()
            .remove(&(branch_name.to_string(), mountpoint.to_path_buf()));
    }

    /// Register an opened file inode for cache invalidation tracking
    pub fn register_opened_inode(&self, branch_name: &str, ino: u64) {
        self.opened_inodes
            .lock()
            .entry(branch_name.to_string())
            .or_default()
            .insert(ino);
    }

    /// Invalidate kernel cache for all mounts
    fn invalidate_all_mounts(&self) {
        let notifiers = self.notifiers.lock();
        let opened_inodes = self.opened_inodes.lock();

        for ((branch, mountpoint), notifier) in notifiers.iter() {
            // Invalidate root inode first (directory cache)
            if let Err(e) = notifier.inval_inode(ROOT_INO, 0, -1) {
                log::debug!(
                    "Failed to invalidate root inode for branch '{}' at {:?}: {}",
                    branch,
                    mountpoint,
                    e
                );
            }

            // Invalidate all opened file inodes for this branch
            if let Some(inodes) = opened_inodes.get(branch) {
                for &ino in inodes {
                    if ino != ROOT_INO {
                        if let Err(e) = notifier.inval_inode(ino, 0, -1) {
                            log::debug!(
                                "Failed to invalidate inode {} for branch '{}': {}",
                                ino,
                                branch,
                                e
                            );
                        } else {
                            log::debug!(
                                "Invalidated inode {} for branch '{}' at {:?}",
                                ino,
                                branch,
                                mountpoint
                            );
                        }
                    }
                }
            }

            log::info!(
                "Invalidated cache for branch '{}' at {:?}",
                branch,
                mountpoint
            );
        }
    }

    /// Invalidate kernel cache for specific branches
    pub fn invalidate_branches(&self, branch_names: &[String]) {
        let notifiers = self.notifiers.lock();
        let opened_inodes = self.opened_inodes.lock();

        for ((branch, mountpoint), notifier) in notifiers.iter() {
            if branch_names.contains(branch) {
                // Invalidate root inode
                if let Err(e) = notifier.inval_inode(ROOT_INO, 0, -1) {
                    log::debug!(
                        "Failed to invalidate root inode for branch '{}' at {:?}: {}",
                        branch,
                        mountpoint,
                        e
                    );
                }

                // Invalidate all opened file inodes
                if let Some(inodes) = opened_inodes.get(branch) {
                    for &ino in inodes {
                        if ino != ROOT_INO {
                            if let Err(e) = notifier.inval_inode(ino, 0, -1) {
                                log::debug!(
                                    "Failed to invalidate inode {} for branch '{}': {}",
                                    ino,
                                    branch,
                                    e
                                );
                            }
                        }
                    }
                }

                log::info!(
                    "Invalidated cache for branch '{}' at {:?}",
                    branch,
                    mountpoint
                );
            }
        }
    }

    fn read_dir_names(path: &Path) -> HashSet<String> {
        let mut names = HashSet::new();
        if let Ok(dir) = fs::read_dir(path) {
            for entry in dir.flatten() {
                names.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
        names
    }

    fn inherited_dir_names_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch: &Branch,
        rel_path: &str,
    ) -> Result<HashSet<String>> {
        match branch.parent.as_deref() {
            None => Ok(Self::read_dir_names(
                &self.base_path.join(rel_path.trim_start_matches('/')),
            )),
            Some(parent) if branch.inheritance == InheritanceMode::Lazy => {
                self.collect_dir_names_locked(branches, parent, rel_path)
            }
            Some(_) => Ok(Self::read_dir_names(&branch.inherited_path(rel_path))),
        }
    }

    fn inherited_resolve_path_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch: &Branch,
        rel_path: &str,
    ) -> Result<Option<PathBuf>> {
        match branch.parent.as_deref() {
            None => {
                let inherited = self.base_path.join(rel_path.trim_start_matches('/'));
                if inherited.symlink_metadata().is_ok() {
                    Ok(Some(inherited))
                } else {
                    Ok(None)
                }
            }
            Some(parent) if branch.inheritance == InheritanceMode::Lazy => {
                self.resolve_path_locked(branches, parent, rel_path)
            }
            Some(_) => {
                let inherited = branch.inherited_path(rel_path);
                if inherited.symlink_metadata().is_ok() {
                    Ok(Some(inherited))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn collect_dir_names_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch_name: &str,
        rel_path: &str,
    ) -> Result<HashSet<String>> {
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;
        let mut names = HashSet::new();

        let delta_dir = branch.files_dir.join(rel_path.trim_start_matches('/'));
        names.extend(Self::read_dir_names(&delta_dir));
        for name in self.inherited_dir_names_locked(branches, branch, rel_path)? {
            let child_rel = if rel_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", rel_path.trim_end_matches('/'), name)
            };
            if branch.is_hidden(&child_rel) {
                continue;
            }
            names.insert(name);
        }

        Ok(names)
    }

    fn resolve_path_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch_name: &str,
        rel_path: &str,
    ) -> Result<Option<PathBuf>> {
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        if branch.has_delta(rel_path) {
            return Ok(Some(branch.delta_path(rel_path)));
        }

        if branch.is_deleted(rel_path) {
            return Ok(None);
        }

        // Hide rules mask inherited data only: without a delta above, the
        // path must not resolve through the parent/base chain.
        if branch.is_hidden(rel_path) {
            return Ok(None);
        }

        self.inherited_resolve_path_locked(branches, branch, rel_path)
    }

    fn snapshot_visible_tree(
        &self,
        branches: &HashMap<String, Branch>,
        source_branch: &str,
        dst_root: &Path,
    ) -> Result<()> {
        if dst_root.exists() {
            fs::remove_dir_all(dst_root)?;
        }
        fs::create_dir_all(dst_root)?;
        self.snapshot_visible_dir(branches, source_branch, "/", dst_root)
    }

    fn snapshot_visible_dir(
        &self,
        branches: &HashMap<String, Branch>,
        source_branch: &str,
        rel_path: &str,
        dst_root: &Path,
    ) -> Result<()> {
        let mut names: Vec<String> = self
            .collect_dir_names_locked(branches, source_branch, rel_path)?
            .into_iter()
            .collect();
        names.sort();

        for name in names {
            let child_rel = if rel_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", rel_path, name)
            };

            let Some(src) = self.resolve_path_locked(branches, source_branch, &child_rel)? else {
                continue;
            };
            let meta = src.symlink_metadata()?;
            let dst = dst_root.join(child_rel.trim_start_matches('/'));

            if meta.file_type().is_dir() {
                fs::create_dir_all(&dst)?;
                fs::set_permissions(&dst, meta.permissions())?;
                self.snapshot_visible_dir(branches, source_branch, &child_rel, dst_root)?;
            } else {
                storage::copy_entry(&src, &dst)?;
            }
        }

        Ok(())
    }

    /// Collect all candidate file/directory names visible in a directory.
    /// Branches resolve against their own deltas plus the frozen inherited
    /// snapshot captured at fork time; main resolves against its delta plus
    /// the live base directory.
    pub fn collect_dir_names(&self, branch_name: &str, rel_path: &str) -> Result<HashSet<String>> {
        let branches = self.branches.read();
        self.collect_dir_names_locked(&branches, branch_name, rel_path)
    }

    pub fn resolve_path(&self, branch_name: &str, rel_path: &str) -> Result<Option<PathBuf>> {
        let branches = self.branches.read();
        self.resolve_path_locked(&branches, branch_name, rel_path)
    }

    fn inherited_path_exists_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch: &Branch,
        rel_path: &str,
    ) -> Result<bool> {
        if branch.is_hidden(rel_path) {
            return Ok(false);
        }
        Ok(self
            .inherited_resolve_path_locked(branches, branch, rel_path)?
            .is_some())
    }

    pub fn inherited_path_exists(&self, branch_name: &str, rel_path: &str) -> Result<bool> {
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;
        self.inherited_path_exists_locked(&branches, branch, rel_path)
    }

    fn resolve_inherited_for_touch_locked(
        &self,
        branches: &HashMap<String, Branch>,
        branch: &Branch,
        rel_path: &str,
    ) -> Result<Option<PathBuf>> {
        if branch.is_hidden(rel_path) {
            Ok(None)
        } else {
            self.inherited_resolve_path_locked(branches, branch, rel_path)
        }
    }

    fn record_first_touch_locked(
        &self,
        branch: &Branch,
        rel_path: &str,
        inherited: Option<&Path>,
        capture_base_content: bool,
    ) -> Result<()> {
        branch.record_first_touch(rel_path, inherited, capture_base_content)
    }

    pub fn ensure_delta_path_for_write(
        &self,
        branch_name: &str,
        rel_path: &str,
    ) -> Result<PathBuf> {
        let _path_guard = self.path_lock(branch_name, rel_path).lock();
        self.ensure_delta_path_for_write_locked(branch_name, rel_path)
    }

    fn ensure_delta_path_for_write_locked(
        &self,
        branch_name: &str,
        rel_path: &str,
    ) -> Result<PathBuf> {
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        if !branch.is_writable() {
            return Err(BranchError::Invalid(format!(
                "branch '{}' is frozen/read-only",
                branch_name
            )));
        }

        let delta = branch.delta_path(rel_path);
        if delta.symlink_metadata().is_err() {
            let inherited = self.resolve_inherited_for_touch_locked(&branches, branch, rel_path)?;
            self.record_first_touch_locked(branch, rel_path, inherited.as_deref(), true)?;
            if let Some(src) = inherited {
                if let Ok(meta) = src.symlink_metadata() {
                    if meta.file_type().is_symlink() || meta.file_type().is_file() {
                        let src_size = meta.len();
                        self.quota.check(src_size).map_err(|errno| {
                            BranchError::Io(std::io::Error::from_raw_os_error(errno))
                        })?;
                        storage::copy_entry(&src, &delta)?;
                        self.quota.add(src_size);
                    }
                }
            }
        }

        branch.remove_tombstone(rel_path)?;
        storage::ensure_parent_dirs(&delta)?;
        Ok(delta)
    }

    pub fn write_data_to_branch(
        &self,
        branch_name: &str,
        rel_path: &str,
        offset: i64,
        data: &[u8],
    ) -> Result<u32> {
        let offset = u64::try_from(offset).map_err(|_| invalid_offset())?;
        let write_end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(file_too_large)?;

        let _path_guard = self.path_lock(branch_name, rel_path).lock();
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        if !branch.is_writable() {
            return Err(BranchError::Invalid(format!(
                "branch '{}' is frozen/read-only",
                branch_name
            )));
        }

        let delta = branch.delta_path(rel_path);
        if delta.symlink_metadata().is_err() {
            let inherited = self.resolve_inherited_for_touch_locked(&branches, branch, rel_path)?;
            self.record_first_touch_locked(branch, rel_path, inherited.as_deref(), true)?;

            if let Some(src) = inherited {
                if let Ok(meta) = src.symlink_metadata() {
                    if meta.file_type().is_file() {
                        let final_size = meta.len().max(write_end);
                        self.quota.check(final_size).map_err(|errno| {
                            BranchError::Io(std::io::Error::from_raw_os_error(errno))
                        })?;
                        storage::ensure_parent_dirs(&delta)?;
                        fs::create_dir_all(&branch.staging_dir)?;
                        let staging = branch.staging_path();
                        let result = (|| -> Result<(u32, u64)> {
                            fs::copy(&src, &staging)?;
                            let written = write_data_to_file(&staging, offset, data)?;
                            let final_len = staging.metadata()?.len();
                            fs::rename(&staging, &delta)?;
                            Ok((written, final_len))
                        })();
                        match result {
                            Ok((written, final_len)) => {
                                self.quota.add(final_len);
                                branch.remove_tombstone(rel_path)?;
                                return Ok(written);
                            }
                            Err(e) => {
                                let _ = fs::remove_file(&staging);
                                return Err(e);
                            }
                        }
                    } else if meta.file_type().is_symlink() {
                        let src_size = meta.len();
                        self.quota.check(src_size).map_err(|errno| {
                            BranchError::Io(std::io::Error::from_raw_os_error(errno))
                        })?;
                        storage::copy_entry(&src, &delta)?;
                        self.quota.add(src_size);
                    }
                }
            }

            storage::ensure_parent_dirs(&delta)?;
        }

        branch.remove_tombstone(rel_path)?;
        let old_size = delta.metadata().map(|m| m.len()).unwrap_or(0);
        if write_end > old_size {
            self.quota
                .check(write_end - old_size)
                .map_err(|errno| BranchError::Io(std::io::Error::from_raw_os_error(errno)))?;
        }
        let written = write_data_to_file(&delta, offset, data)?;
        let new_size = delta.metadata().map(|m| m.len()).unwrap_or(old_size);
        if new_size > old_size {
            self.quota.add(new_size - old_size);
        }
        Ok(written)
    }

    fn prune_empty_delta_parents(files_dir: &Path, start: Option<&Path>) {
        let Some(start) = start else {
            return;
        };
        let mut current = start.to_path_buf();
        while current.starts_with(files_dir) && current != files_dir {
            match fs::remove_dir(&current) {
                Ok(()) => {
                    if let Some(parent) = current.parent() {
                        current = parent.to_path_buf();
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    pub fn delete_path_in_branch(&self, branch_name: &str, rel_path: &str) -> Result<()> {
        let _path_guard = self.path_lock(branch_name, rel_path).lock();
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        let inherited = self.resolve_inherited_for_touch_locked(&branches, branch, rel_path)?;
        self.record_first_touch_locked(branch, rel_path, inherited.as_deref(), false)?;
        let inherited_exists = inherited.is_some();
        let delta = branch.delta_path(rel_path);
        if let Ok(meta) = delta.symlink_metadata() {
            let freed = if meta.file_type().is_dir() {
                StorageQuota::dir_size(&delta)
            } else {
                meta.len()
            };
            remove_entry(&delta)?;
            self.quota.sub(freed);
            Self::prune_empty_delta_parents(&branch.files_dir, delta.parent());
        }

        if inherited_exists {
            branch.add_tombstone(rel_path)?;
        } else {
            branch.remove_tombstone(rel_path)?;
        }
        Ok(())
    }

    pub fn is_branch_writable(&self, branch_name: &str) -> bool {
        self.branches
            .read()
            .get(branch_name)
            .map(|b| b.is_writable())
            .unwrap_or(false)
    }

    pub fn branch_has_delta(&self, branch_name: &str, rel_path: &str) -> bool {
        self.branches
            .read()
            .get(branch_name)
            .map(|b| b.has_delta(rel_path))
            .unwrap_or(false)
    }

    pub fn freeze_branch(&self, branch_name: &str) -> Result<()> {
        if branch_name == "main" {
            return Err(BranchError::CannotOperateOnMain);
        }
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;
        branch.set_state(BranchState::Frozen)?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(branches);
        self.invalidate_branches(&[branch_name.to_string()]);
        Ok(())
    }

    pub fn thaw_branch(&self, branch_name: &str) -> Result<()> {
        if branch_name == "main" {
            return Err(BranchError::CannotOperateOnMain);
        }
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;
        branch.set_state(BranchState::Open)?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(branches);
        self.invalidate_branches(&[branch_name.to_string()]);
        Ok(())
    }

    fn collect_delta_diff(dir: &Path, prefix: &str, out: &mut Vec<DiffEntry>) -> Result<()> {
        match dir.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        let _guard = StoreDirModeGuard::new(dir)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let rel_path = if prefix.is_empty() {
                format!("/{}", name)
            } else {
                format!("{}/{}", prefix, name)
            };
            let meta = path.symlink_metadata()?;
            if meta.file_type().is_dir() {
                Self::collect_delta_diff(&path, &rel_path, out)?;
                continue;
            }
            let kind = if meta.file_type().is_symlink() {
                "symlink"
            } else {
                "file"
            };
            out.push(DiffEntry {
                op: "delta".to_string(),
                path: rel_path,
                kind: kind.to_string(),
                bytes: if meta.file_type().is_file() {
                    meta.len()
                } else {
                    0
                },
            });
        }
        Ok(())
    }

    pub fn branch_status(&self, branch_name: &str) -> Result<BranchStatus> {
        let branches = self.branches.read();
        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        let mut diff = Vec::new();
        Self::collect_delta_diff(&branch.files_dir, "", &mut diff)?;
        for tombstone in branch.get_tombstones() {
            diff.push(DiffEntry {
                op: "delete".to_string(),
                path: tombstone,
                kind: "tombstone".to_string(),
                bytes: 0,
            });
        }
        diff.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.op.cmp(&b.op)));

        Ok(BranchStatus {
            name: branch.name.clone(),
            parent: branch.parent.clone(),
            inheritance: branch.inheritance,
            state: branch.state(),
            parent_version_at_fork: branch.parent_version_at_fork,
            commit_count: branch.commit_count.load(Ordering::SeqCst),
            delta_entries: diff.iter().filter(|e| e.op == "delta").count(),
            tombstones: diff.iter().filter(|e| e.op == "delete").count(),
            diff,
        })
    }

    /// Returns true if no other branch has `parent == name`.
    fn is_leaf(name: &str, branches: &std::collections::HashMap<String, Branch>) -> bool {
        !branches.values().any(|b| b.parent.as_deref() == Some(name))
    }

    /// Commit a leaf branch into its immediate parent.
    /// Returns the parent branch name on success.
    pub fn commit(&self, branch_name: &str) -> Result<String> {
        Ok(self.commit_with_report(branch_name)?.parent)
    }

    /// Commit a leaf branch and return machine-readable auto-merge/conflict records.
    pub fn commit_with_report(&self, branch_name: &str) -> Result<CommitOutcome> {
        let start = Instant::now();
        if branch_name == "main" {
            return Err(BranchError::CannotOperateOnMain);
        }

        let mut branches = self.branches.write();

        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        if !Self::is_leaf(branch_name, &branches) {
            return Err(BranchError::NotALeaf(branch_name.to_string()));
        }

        let parent_name = branch
            .parent
            .clone()
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        let child_version_at_fork = branch.parent_version_at_fork;
        let parent_changed = {
            let parent = branches
                .get(&parent_name)
                .ok_or_else(|| BranchError::NotFound(parent_name.to_string()))?;
            parent.commit_count.load(Ordering::SeqCst) != child_version_at_fork
        };

        let child_tombstones = branch.get_tombstones();
        let child_files_dir = branch.files_dir.clone();
        let branch_dir = self.storage_path.join("branches").join(branch_name);
        let merge_content_dir = branch_dir.join("merge-content");

        let mut delta_files: Vec<(String, PathBuf)> = Vec::new();
        self.walk_files(&child_files_dir, "", &mut |rel_path, src_path| {
            delta_files.push((rel_path.to_string(), src_path.to_path_buf()));
            Ok(())
        })?;

        let mut outcome = CommitOutcome {
            parent: parent_name.clone(),
            auto_merges: Vec::new(),
            conflicts: Vec::new(),
        };
        let mut merge_sources: HashMap<String, PathBuf> = HashMap::new();

        if parent_changed {
            for path in &child_tombstones {
                let record = branch
                    .get_touch_record(path)
                    .unwrap_or_else(|| TouchRecord {
                        path: path.clone(),
                        base_at_first_touch: PathIdentity::missing(),
                        base_content_key: None,
                    });
                let current_parent_path = if branch.is_hidden(path) {
                    None
                } else {
                    self.inherited_resolve_path_locked(&branches, branch, path)?
                };
                let current_identity = PathIdentity::from_path(current_parent_path.as_deref());
                if record.base_at_first_touch != current_identity {
                    outcome.conflicts.push(CommitConflict {
                        path: path.clone(),
                        session_action: "delete".to_string(),
                        resolution: "session_won".to_string(),
                        reason: "parent_changed_after_first_touch".to_string(),
                        base_at_first_touch: record.base_at_first_touch,
                        base_at_commit: current_identity,
                    });
                }
            }

            for (rel_path, src_path) in &delta_files {
                let record = branch
                    .get_touch_record(rel_path)
                    .unwrap_or_else(|| TouchRecord {
                        path: rel_path.clone(),
                        base_at_first_touch: PathIdentity::missing(),
                        base_content_key: None,
                    });
                let current_parent_path = if branch.is_hidden(rel_path) {
                    None
                } else {
                    self.inherited_resolve_path_locked(&branches, branch, rel_path)?
                };
                let current_identity = PathIdentity::from_path(current_parent_path.as_deref());
                if record.base_at_first_touch == current_identity {
                    continue;
                }

                let mut merged_cleanly = false;
                if record.base_at_first_touch.kind == "file" && current_identity.kind == "file" {
                    if let (Some(base_bytes), Some(current_path), Some(session_bytes)) = (
                        branch.read_touch_base_content(&record)?,
                        current_parent_path.as_ref(),
                        read_bounded_text(src_path)?,
                    ) {
                        if let Some(current_bytes) = read_bounded_text(current_path)? {
                            if let Some(merged) = try_three_way_text_merge(
                                &base_bytes,
                                &current_bytes,
                                &session_bytes,
                            ) {
                                let key = touch_content_key(rel_path);
                                let merged_path = merge_content_dir.join(key);
                                storage::ensure_parent_dirs(&merged_path)?;
                                fs::write(&merged_path, merged)?;
                                merge_sources.insert(rel_path.clone(), merged_path);
                                outcome.auto_merges.push(CommitAutoMerge {
                                    path: rel_path.clone(),
                                    resolution: "clean_text_merge".to_string(),
                                });
                                merged_cleanly = true;
                            }
                        }
                    }
                }

                if !merged_cleanly {
                    let action = if record.base_at_first_touch.exists {
                        "modify"
                    } else {
                        "create"
                    };
                    outcome.conflicts.push(CommitConflict {
                        path: rel_path.clone(),
                        session_action: action.to_string(),
                        resolution: "session_won".to_string(),
                        reason: "parent_changed_after_first_touch".to_string(),
                        base_at_first_touch: record.base_at_first_touch,
                        base_at_commit: current_identity,
                    });
                }
            }
        }

        if parent_name == "main" {
            // Direct child of main: apply to the base filesystem atomically.
            let mut staged = StagedMerge::new();
            for path in &child_tombstones {
                let full_path = self.base_path.join(path.trim_start_matches('/'));
                staged.stage_delete(&full_path)?;
            }
            for (rel_path, src_path) in &delta_files {
                let source = merge_sources.get(rel_path).unwrap_or(src_path);
                staged.stage_copy(rel_path, source, &self.base_path)?;
            }

            let total_bytes = staged.bytes;
            let committed_paths = staged.commit()?;
            let num_files = committed_paths.len() as u64;

            if let Some(main_branch) = branches.get("main") {
                let main_files_dir = &main_branch.files_dir;
                for rel_path in committed_paths.iter().chain(&child_tombstones) {
                    let main_delta = main_files_dir.join(rel_path.trim_start_matches('/'));
                    let _ = remove_entry(&main_delta);
                }
            }

            if let Some(main_branch) = branches.get("main") {
                main_branch.commit_count.fetch_add(1, Ordering::SeqCst);
                main_branch.write_metadata()?;
            }

            branches.remove(branch_name);
            let branch_dir = self.storage_path.join("branches").join(branch_name);
            if branch_dir.exists() {
                remove_branch_store_dir_all(&branch_dir)?;
            }

            self.epoch.fetch_add(1, Ordering::SeqCst);

            drop(branches);
            self.invalidate_all_mounts();

            let elapsed = start.elapsed();
            log::debug!(
                "[BENCH] commit '{}' to base: {:?} ({} us), {} deletions, {} files, {} bytes, {} auto-merges, {} conflicts",
                branch_name,
                elapsed,
                elapsed.as_micros(),
                child_tombstones.len(),
                num_files,
                total_bytes,
                outcome.auto_merges.len(),
                outcome.conflicts.len()
            );
        } else {
            // Nested branch: merge delta into parent's delta.
            let parent = branches
                .get(&parent_name)
                .ok_or_else(|| BranchError::NotFound(parent_name.to_string()))?;

            let parent_files_dir = parent.files_dir.clone();
            let mut parent_tombstones = parent.get_tombstones();
            let mut deleted_parent_delta_parents = Vec::new();

            let mut staged = StagedMerge::new();
            for tombstone in &child_tombstones {
                let parent_delta = parent_files_dir.join(tombstone.trim_start_matches('/'));
                let deleted_parent_delta_parent = parent_delta
                    .symlink_metadata()
                    .is_ok()
                    .then(|| parent_delta.parent().map(Path::to_path_buf))
                    .flatten();
                let parent_inherited_exists =
                    self.inherited_path_exists_locked(&branches, parent, tombstone)?;
                staged.stage_delete(&parent_delta)?;
                if let Some(parent_dir) = deleted_parent_delta_parent {
                    deleted_parent_delta_parents.push(parent_dir);
                }
                if parent_inherited_exists {
                    parent_tombstones.insert(tombstone.clone());
                } else {
                    parent_tombstones.remove(tombstone);
                }
            }
            for (rel_path, src_path) in &delta_files {
                let source = merge_sources.get(rel_path).unwrap_or(src_path);
                staged.stage_copy(rel_path, source, &parent_files_dir)?;
            }

            let copied_paths = staged.commit()?;
            for parent_dir in deleted_parent_delta_parents {
                Self::prune_empty_delta_parents(&parent_files_dir, Some(&parent_dir));
            }

            for path in &copied_paths {
                parent_tombstones.remove(path);
            }

            parent.set_tombstones(parent_tombstones)?;
            parent.commit_count.fetch_add(1, Ordering::SeqCst);
            parent.write_metadata()?;

            branches.remove(branch_name);
            let branch_dir = self.storage_path.join("branches").join(branch_name);
            if branch_dir.exists() {
                remove_branch_store_dir_all(&branch_dir)?;
            }

            self.epoch.fetch_add(1, Ordering::SeqCst);

            let affected = vec![branch_name.to_string(), parent_name.clone()];
            drop(branches);
            self.invalidate_branches(&affected);

            let elapsed = start.elapsed();
            log::debug!(
                "[BENCH] commit '{}' into parent '{}': {:?} ({} us), {} auto-merges, {} conflicts",
                branch_name,
                parent_name,
                elapsed,
                elapsed.as_micros(),
                outcome.auto_merges.len(),
                outcome.conflicts.len(),
            );
        }

        Ok(outcome)
    }

    /// Abort a leaf branch, discarding only that branch.
    /// Returns the parent branch name on success.
    pub fn abort(&self, branch_name: &str) -> Result<String> {
        let start = Instant::now();
        if branch_name == "main" {
            return Err(BranchError::CannotOperateOnMain);
        }

        let mut branches = self.branches.write();

        let branch = branches
            .get(branch_name)
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        if !Self::is_leaf(branch_name, &branches) {
            return Err(BranchError::NotALeaf(branch_name.to_string()));
        }

        let parent_name = branch
            .parent
            .clone()
            .ok_or_else(|| BranchError::NotFound(branch_name.to_string()))?;

        // Remove only this branch
        branches.remove(branch_name);
        let branch_dir = self.storage_path.join("branches").join(branch_name);
        if branch_dir.exists() {
            remove_branch_store_dir_all(&branch_dir)?;
        }

        // Invalidate kernel cache for this branch only
        drop(branches);
        self.invalidate_branches(&[branch_name.to_string()]);

        let elapsed = start.elapsed();
        log::debug!(
            "[BENCH] abort '{}': {:?} ({} us)",
            branch_name,
            elapsed,
            elapsed.as_micros()
        );

        Ok(parent_name)
    }

    fn walk_files<F>(&self, dir: &Path, prefix: &str, f: &mut F) -> Result<()>
    where
        F: FnMut(&str, &Path) -> Result<()>,
    {
        match dir.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        let _guard = StoreDirModeGuard::new(dir)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let rel_path = if prefix.is_empty() {
                format!("/{}", name)
            } else {
                format!("{}/{}", prefix, name)
            };

            let is_dir = path
                .symlink_metadata()
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            if is_dir {
                self.walk_files(&path, &rel_path, f)?;
            } else {
                f(&rel_path, &path)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod branch_manager_tests {
    use super::{BranchManager, BranchState, InheritanceMode};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::thread;

    #[cfg(unix)]
    struct ModeReset(Vec<PathBuf>);

    #[cfg(unix)]
    impl Drop for ModeReset {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;

            for path in self.0.iter().rev() {
                if path.exists() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                }
            }
        }
    }

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("branchfs-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }

    #[cfg(unix)]
    fn make_mode_000_dir(path: &Path) -> ModeReset {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        ModeReset(vec![path.to_path_buf()])
    }

    fn manager(tmp: &TmpDir) -> BranchManager {
        let base = tmp.path().join("base");
        let storage = tmp.path().join("storage");
        let work = tmp.path().join("work");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&work).unwrap();
        BranchManager::new(storage, base, work, None).unwrap()
    }

    fn write_delta(mgr: &BranchManager, branch: &str, rel_path: &str, data: &[u8]) {
        let delta = mgr.ensure_delta_path_for_write(branch, rel_path).unwrap();
        write(&delta, data);
    }

    #[test]
    fn first_touch_appends_incrementally_without_rewriting_legacy_snapshot() {
        let tmp = TmpDir::new();
        let base = tmp.path().join("base");
        let storage = tmp.path().join("storage");
        let work = tmp.path().join("work");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&work).unwrap();

        let mgr = BranchManager::new(storage.clone(), base.clone(), work.clone(), None).unwrap();
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        drop(mgr);

        // Simulate an existing large legacy touches.json snapshot from an old
        // session. Recording one more first-touch must not rewrite this file:
        // on NFS-backed CCC stores, repeatedly rewriting multi-MB JSON made
        // deleting even an empty old .scratch/.ccc-storage directory take
        // seconds.
        let touches_file = storage.join("branches/work/touches.json");
        let legacy_snapshot = serde_json::json!({
            "/already-touched": {
                "path": "/already-touched",
                "base_at_first_touch": {
                    "exists": false,
                    "kind": "missing",
                    "bytes": 0,
                    "mtime_ns": null
                }
            }
        });
        fs::write(
            &touches_file,
            serde_json::to_vec_pretty(&legacy_snapshot).unwrap(),
        )
        .unwrap();
        let before = fs::read(&touches_file).unwrap();

        fs::create_dir_all(base.join("empty-old-scratch/.ccc-storage")).unwrap();
        let mgr = BranchManager::new(storage.clone(), base, work, None).unwrap();
        mgr.delete_path_in_branch("work", "/empty-old-scratch")
            .unwrap();

        assert_eq!(
            fs::read(&touches_file).unwrap(),
            before,
            "recording an additional first-touch should append to touches.log, not rewrite touches.json"
        );

        let touches_log = storage.join("branches/work/touches.log");
        let log = fs::read_to_string(&touches_log).expect("touches.log should be appended");
        assert!(
            log.contains("/empty-old-scratch"),
            "incremental touch log should contain the newly deleted directory"
        );

        let reloaded = BranchManager::new(
            storage,
            tmp.path().join("base"),
            tmp.path().join("work"),
            None,
        )
        .unwrap();
        let branches = reloaded.branches.read();
        let branch = branches.get("work").unwrap();
        assert!(
            branch.get_touch_record("/already-touched").is_some(),
            "legacy snapshot touch should still load"
        );
        assert!(
            branch.get_touch_record("/empty-old-scratch").is_some(),
            "incremental log touch should load on daemon restart"
        );
    }

    #[test]
    fn tombstone_removal_appends_incrementally_without_rewriting_legacy_snapshot() {
        let tmp = TmpDir::new();
        let base = tmp.path().join("base");
        let storage = tmp.path().join("storage");
        let work = tmp.path().join("work");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&work).unwrap();

        let mgr = BranchManager::new(storage.clone(), base.clone(), work.clone(), None).unwrap();
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        drop(mgr);

        let tombstones_file = storage.join("branches/work/tombstones");
        fs::write(
            &tombstones_file,
            "/old-empty-scratch\n/old-empty-scratch/.ccc-storage\n",
        )
        .unwrap();
        let before = fs::read(&tombstones_file).unwrap();

        let mgr = BranchManager::new(storage.clone(), base, work, None).unwrap();
        // The inherited path is gone, so this delete is effectively clearing
        // stale BranchFS overlay state. It must not rewrite a huge tombstones
        // snapshot just to remove one stale path.
        mgr.delete_path_in_branch("work", "/old-empty-scratch")
            .unwrap();

        assert_eq!(
            fs::read(&tombstones_file).unwrap(),
            before,
            "removing one stale tombstone should append to tombstones.log, not rewrite tombstones"
        );

        let tombstones_log = storage.join("branches/work/tombstones.log");
        let log = fs::read_to_string(&tombstones_log).expect("tombstones.log should be appended");
        assert!(
            log.contains("- /old-empty-scratch"),
            "incremental tombstone log should record the removal"
        );

        let reloaded = BranchManager::new(
            storage,
            tmp.path().join("base"),
            tmp.path().join("work"),
            None,
        )
        .unwrap();
        let branches = reloaded.branches.read();
        let branch = branches.get("work").unwrap();
        assert!(
            !branch.is_deleted("/old-empty-scratch"),
            "incremental tombstone removal should load on daemon restart"
        );
        assert!(
            branch.is_deleted("/old-empty-scratch/.ccc-storage"),
            "unrelated descendant tombstone should remain"
        );
    }

    #[test]
    fn delete_first_touch_does_not_copy_inherited_text_content() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"base text\n");
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();

        mgr.delete_path_in_branch("work", "/file.txt").unwrap();

        let branches = mgr.branches.read();
        let branch = branches.get("work").unwrap();
        let record = branch
            .get_touch_record("/file.txt")
            .expect("delete should record first touch identity");
        assert!(
            record.base_content_key.is_none(),
            "delete touches should not read/copy inherited text contents"
        );
    }

    #[test]
    fn write_first_touch_keeps_inherited_text_content_for_merge() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"base text\n");
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();

        write_delta(&mgr, "work", "/file.txt", b"changed\n");

        let branches = mgr.branches.read();
        let branch = branches.get("work").unwrap();
        let record = branch
            .get_touch_record("/file.txt")
            .expect("write should record first touch identity");
        assert!(
            record.base_content_key.is_some(),
            "write touches should keep inherited text contents for 3-way merge"
        );
    }

    #[test]
    fn cow_write_to_inherited_file_preserves_unwritten_ranges() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"0123456789");
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();

        let written = mgr
            .write_data_to_branch("work", "/file.txt", 3, b"ABC")
            .unwrap();

        assert_eq!(written, 3);
        let delta = tmp.path().join("storage/branches/work/files/file.txt");
        assert_eq!(fs::read(&delta).unwrap(), b"012ABC6789");
    }

    #[test]
    fn concurrent_cow_writes_to_same_inherited_file_preserve_both_writes() {
        let tmp = TmpDir::new();
        let mgr = Arc::new(manager(&tmp));
        write(&tmp.path().join("base/file.txt"), b"0123456789");
        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();

        let a = mgr.clone();
        let t1 = thread::spawn(move || a.write_data_to_branch("work", "/file.txt", 0, b"AA"));
        let b = mgr.clone();
        let t2 = thread::spawn(move || b.write_data_to_branch("work", "/file.txt", 8, b"BB"));

        assert_eq!(t1.join().unwrap().unwrap(), 2);
        assert_eq!(t2.join().unwrap().unwrap(), 2);
        let delta = tmp.path().join("storage/branches/work/files/file.txt");
        assert_eq!(fs::read(&delta).unwrap(), b"AA234567BB");
    }

    #[test]
    fn sibling_disjoint_commits_do_not_conflict_after_parent_version_changes() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        mgr.create_branch_with_mode("a", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.create_branch_with_mode("b", "main", InheritanceMode::Lazy)
            .unwrap();

        write_delta(&mgr, "a", "/a.txt", b"a\n");
        write_delta(&mgr, "b", "/b.txt", b"b\n");

        let b_outcome = mgr.commit_with_report("b").unwrap();
        assert!(
            b_outcome.conflicts.is_empty(),
            "unexpected b conflicts: {:?}",
            b_outcome.conflicts
        );
        assert!(
            b_outcome.auto_merges.is_empty(),
            "unexpected b auto-merges: {:?}",
            b_outcome.auto_merges
        );

        let a_outcome = mgr.commit_with_report("a").unwrap();
        assert!(
            a_outcome.conflicts.is_empty(),
            "disjoint commit should not conflict: {:?}",
            a_outcome.conflicts
        );
        assert!(
            a_outcome.auto_merges.is_empty(),
            "disjoint commit should not auto-merge: {:?}",
            a_outcome.auto_merges
        );
        assert_eq!(fs::read(tmp.path().join("base/a.txt")).unwrap(), b"a\n");
        assert_eq!(fs::read(tmp.path().join("base/b.txt")).unwrap(), b"b\n");
    }

    #[test]
    fn sibling_same_text_file_non_overlapping_edits_auto_merge_cleanly() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(
            &tmp.path().join("base/file.txt"),
            b"line1\nbase-a\nline3\nbase-b\nline5\n",
        );
        mgr.create_branch_with_mode("a", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.create_branch_with_mode("b", "main", InheritanceMode::Lazy)
            .unwrap();

        write_delta(
            &mgr,
            "a",
            "/file.txt",
            b"line1\nA-a\nline3\nbase-b\nline5\n",
        );
        write_delta(
            &mgr,
            "b",
            "/file.txt",
            b"line1\nbase-a\nline3\nB-b\nline5\n",
        );

        mgr.commit_with_report("b").unwrap();
        let a_outcome = mgr.commit_with_report("a").unwrap();

        assert!(
            a_outcome.conflicts.is_empty(),
            "clean merge should not be a conflict: {:?}",
            a_outcome.conflicts
        );
        assert_eq!(
            a_outcome.auto_merges.len(),
            1,
            "expected one auto-merge: {:?}",
            a_outcome.auto_merges
        );
        assert_eq!(a_outcome.auto_merges[0].path, "/file.txt");
        assert_eq!(
            fs::read(tmp.path().join("base/file.txt")).unwrap(),
            b"line1\nA-a\nline3\nB-b\nline5\n"
        );
    }

    #[test]
    fn sibling_same_text_file_overlapping_edits_report_conflict_but_latest_wins() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"one\nbase\nthree\n");
        mgr.create_branch_with_mode("a", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.create_branch_with_mode("b", "main", InheritanceMode::Lazy)
            .unwrap();

        write_delta(&mgr, "a", "/file.txt", b"one\nA\nthree\n");
        write_delta(&mgr, "b", "/file.txt", b"one\nB\nthree\n");

        mgr.commit_with_report("b").unwrap();
        let a_outcome = mgr.commit_with_report("a").unwrap();

        assert_eq!(
            a_outcome.conflicts.len(),
            1,
            "expected one conflict: {:?}",
            a_outcome.conflicts
        );
        assert_eq!(a_outcome.conflicts[0].path, "/file.txt");
        assert_eq!(a_outcome.conflicts[0].resolution, "session_won");
        assert_eq!(
            fs::read(tmp.path().join("base/file.txt")).unwrap(),
            b"one\nA\nthree\n"
        );
    }

    #[test]
    fn sibling_delete_vs_modify_reports_conflict_but_latest_delete_wins() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"base\n");
        mgr.create_branch_with_mode("a", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.create_branch_with_mode("b", "main", InheritanceMode::Lazy)
            .unwrap();

        mgr.delete_path_in_branch("a", "/file.txt").unwrap();
        write_delta(&mgr, "b", "/file.txt", b"b\n");

        mgr.commit_with_report("b").unwrap();
        let a_outcome = mgr.commit_with_report("a").unwrap();

        assert_eq!(
            a_outcome.conflicts.len(),
            1,
            "expected one conflict: {:?}",
            a_outcome.conflicts
        );
        assert_eq!(a_outcome.conflicts[0].path, "/file.txt");
        assert_eq!(a_outcome.conflicts[0].session_action, "delete");
        assert!(!tmp.path().join("base/file.txt").exists());
    }

    #[test]
    fn running_branch_delta_survives_foreign_same_path_commit() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/file.txt"), b"base\n");
        mgr.create_branch_with_mode("a", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.create_branch_with_mode("b", "main", InheritanceMode::Lazy)
            .unwrap();

        write_delta(&mgr, "a", "/file.txt", b"a\n");
        write_delta(&mgr, "b", "/file.txt", b"b\n");
        mgr.commit_with_report("b").unwrap();

        let resolved = mgr.resolve_path("a", "/file.txt").unwrap().unwrap();
        assert_eq!(fs::read(resolved).unwrap(), b"a\n");
    }

    #[test]
    fn lazy_branch_resolves_base_without_inherited_snapshot_and_prefers_delta() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        let base_file = tmp.path().join("base/dir/base.txt");
        write(&base_file, b"base");

        mgr.create_branch_with_mode("lazy", "main", InheritanceMode::Lazy)
            .unwrap();

        mgr.with_branch("lazy", |branch| {
            assert!(!branch.inherited_path("/dir/base.txt").exists());
            Ok(())
        })
        .unwrap();

        let resolved = mgr.resolve_path("lazy", "/dir/base.txt").unwrap().unwrap();
        assert_eq!(resolved, base_file);
        assert_eq!(fs::read(&resolved).unwrap(), b"base");

        mgr.with_branch("lazy", |branch| {
            write(&branch.delta_path("/dir/base.txt"), b"delta");
            branch.add_tombstone("/dir/base.txt")?;
            Ok(())
        })
        .unwrap();

        let resolved = mgr.resolve_path("lazy", "/dir/base.txt").unwrap().unwrap();
        assert_eq!(fs::read(&resolved).unwrap(), b"delta");
    }

    #[test]
    fn removed_tombstone_does_not_reappear_after_manager_reload() {
        let tmp = TmpDir::new();
        write(&tmp.path().join("base/existing.txt"), b"base");

        {
            let mgr = manager(&tmp);
            mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
                .unwrap();
            mgr.with_branch("work", |branch| {
                // Delete followed by rewrite leaves a delta at the same path;
                // the old tombstone is only an implementation detail and must
                // not survive daemon/manager reload through the on-disk store.
                branch.add_tombstone("/existing.txt")?;
                branch.remove_tombstone("/existing.txt")?;
                write(&branch.delta_path("/existing.txt"), b"rewritten");
                Ok(())
            })
            .unwrap();
        }

        let mgr = manager(&tmp);
        let status = mgr.branch_status("work").unwrap();
        assert!(
            status
                .diff
                .iter()
                .any(|entry| entry.op == "delta" && entry.path == "/existing.txt"),
            "rewritten file delta missing from status: {:?}",
            status.diff
        );
        assert!(
            !status
                .diff
                .iter()
                .any(|entry| entry.op == "delete" && entry.path == "/existing.txt"),
            "stale tombstone came back after reload: {:?}",
            status.diff
        );
    }

    #[test]
    fn lazy_directory_tombstone_hides_inherited_descendants_but_not_delta_children() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/dir/base.txt"), b"base");

        mgr.create_branch_with_mode("lazy", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.with_branch("lazy", |branch| {
            branch.add_tombstone("/dir")?;
            write(&branch.delta_path("/dir/new.txt"), b"new");
            Ok(())
        })
        .unwrap();

        assert!(mgr.resolve_path("lazy", "/dir/base.txt").unwrap().is_none());

        let resolved = mgr.resolve_path("lazy", "/dir/new.txt").unwrap().unwrap();
        assert_eq!(fs::read(&resolved).unwrap(), b"new");
    }

    #[test]
    fn freeze_and_thaw_state_persist_across_manager_reloads() {
        let tmp = TmpDir::new();
        {
            let mgr = manager(&tmp);
            mgr.create_branch_with_mode("review", "main", InheritanceMode::Lazy)
                .unwrap();
            mgr.freeze_branch("review").unwrap();
            assert_eq!(
                mgr.branch_status("review").unwrap().state,
                BranchState::Frozen
            );
            assert!(!mgr.is_branch_writable("review"));
        }

        {
            let mgr = manager(&tmp);
            assert_eq!(
                mgr.branch_status("review").unwrap().state,
                BranchState::Frozen
            );
            assert!(!mgr.is_branch_writable("review"));
            mgr.thaw_branch("review").unwrap();
            assert_eq!(
                mgr.branch_status("review").unwrap().state,
                BranchState::Open
            );
            assert!(mgr.is_branch_writable("review"));
        }

        let mgr = manager(&tmp);
        assert_eq!(
            mgr.branch_status("review").unwrap().state,
            BranchState::Open
        );
        assert!(mgr.is_branch_writable("review"));
    }

    #[test]
    fn branch_status_reports_only_delta_and_tombstone_changes() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/unchanged.txt"), b"unchanged");
        write(&tmp.path().join("base/deleted.txt"), b"deleted");

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.with_branch("work", |branch| {
            write(&branch.delta_path("/new.txt"), b"new");
            write(&branch.delta_path("/nested/file.txt"), b"nested");
            branch.add_tombstone("/deleted.txt")?;
            Ok(())
        })
        .unwrap();

        let status = mgr.branch_status("work").unwrap();
        let entries: Vec<_> = status
            .diff
            .iter()
            .map(|entry| (entry.op.as_str(), entry.kind.as_str(), entry.path.as_str()))
            .collect();

        assert_eq!(status.delta_entries, 2);
        assert_eq!(status.tombstones, 1);
        assert!(entries.contains(&("delta", "file", "/new.txt")));
        assert!(entries.contains(&("delta", "file", "/nested/file.txt")));
        assert!(entries.contains(&("delete", "tombstone", "/deleted.txt")));
        assert!(!entries.iter().any(|(_, kind, _)| *kind == "dir"));
        assert!(!status
            .diff
            .iter()
            .any(|entry| entry.path == "/unchanged.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn branch_status_survives_mode_000_delta_dir_and_reports_accessible_deltas() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        let _reset = mgr
            .with_branch("work", |branch| {
                write(&branch.delta_path("/new.txt"), b"new");
                let scratch = branch.delta_path("/.scratch/cap-probe/kern-ovl-test/work/work");
                Ok(make_mode_000_dir(&scratch))
            })
            .unwrap();

        let status = mgr.branch_status("work").unwrap();

        assert!(status.diff.iter().any(|entry| {
            entry.op == "delta" && entry.kind == "file" && entry.path == "/new.txt"
        }));
    }

    #[cfg(unix)]
    #[test]
    fn abort_discards_branch_with_mode_000_nested_delta_dir() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        let _reset = mgr
            .with_branch("work", |branch| {
                let scratch = branch.delta_path("/.scratch/cap-probe/kern-ovl-test/work/work");
                Ok(make_mode_000_dir(&scratch))
            })
            .unwrap();

        assert_eq!(mgr.abort("work").unwrap(), "main");
        assert!(!mgr.is_branch_valid("work"));
        assert!(!tmp.path().join("storage/branches/work").exists());
    }

    #[cfg(unix)]
    #[test]
    fn commit_walks_mode_000_delta_dir_and_commits_files_inside_it() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        let _reset = mgr
            .with_branch("work", |branch| {
                let private = branch.delta_path("/private");
                write(&private.join("inside.txt"), b"inside");
                Ok(make_mode_000_dir(&private))
            })
            .unwrap();

        assert_eq!(mgr.commit("work").unwrap(), "main");
        assert_eq!(
            fs::read(tmp.path().join("base/private/inside.txt")).unwrap(),
            b"inside"
        );
        assert!(!mgr.is_branch_valid("work"));
        assert!(!tmp.path().join("storage/branches/work").exists());
    }

    #[test]
    fn deleting_branch_local_temp_file_leaves_no_tombstone_or_empty_parent_delta() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.with_branch("work", |branch| {
            write(
                &branch.delta_path("/domen-cuda10/.bash_history-00002.tmp"),
                b"transient history",
            );
            Ok(())
        })
        .unwrap();

        mgr.delete_path_in_branch("work", "/domen-cuda10/.bash_history-00002.tmp")
            .unwrap();

        let status = mgr.branch_status("work").unwrap();
        assert!(status.diff.is_empty(), "unexpected diff: {:?}", status.diff);
        mgr.with_branch("work", |branch| {
            assert!(!branch
                .delta_path("/domen-cuda10/.bash_history-00002.tmp")
                .exists());
            assert!(!branch.delta_path("/domen-cuda10").exists());
            assert!(branch.get_tombstones().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deleting_branch_local_mode_000_dir_leaves_no_tombstone_or_delta() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        let _reset = mgr
            .with_branch("work", |branch| {
                let private = branch.delta_path("/scratch/private");
                write(&private.join("inside.txt"), b"inside");
                Ok(make_mode_000_dir(&private))
            })
            .unwrap();

        mgr.delete_path_in_branch("work", "/scratch/private")
            .unwrap();

        let status = mgr.branch_status("work").unwrap();
        assert!(status.diff.is_empty(), "unexpected diff: {:?}", status.diff);
        mgr.with_branch("work", |branch| {
            assert!(!branch.delta_path("/scratch/private").exists());
            assert!(!branch.delta_path("/scratch").exists());
            assert!(branch.get_tombstones().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn nested_delete_of_parent_created_file_leaves_no_parent_tombstone() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        write_delta(&mgr, "work", "/tmp/session-only.tmp", b"transient");
        mgr.create_branch_with_mode("child", "work", InheritanceMode::Lazy)
            .unwrap();

        mgr.delete_path_in_branch("child", "/tmp/session-only.tmp")
            .unwrap();
        let child_status = mgr.branch_status("child").unwrap();
        assert_eq!(child_status.tombstones, 1);

        assert_eq!(mgr.commit("child").unwrap(), "work");

        let parent_status = mgr.branch_status("work").unwrap();
        assert!(
            parent_status.diff.is_empty(),
            "nested create/delete should collapse to no-op, got {:?}",
            parent_status.diff
        );
        mgr.with_branch("work", |branch| {
            assert!(!branch.delta_path("/tmp/session-only.tmp").exists());
            assert!(!branch.delta_path("/tmp").exists());
            assert!(branch.get_tombstones().is_empty());
            Ok(())
        })
        .unwrap();

        assert_eq!(mgr.commit("work").unwrap(), "main");
        assert!(!tmp.path().join("base/tmp/session-only.tmp").exists());
    }

    #[test]
    fn deleting_modified_inherited_file_reports_only_delete_tombstone() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/existing.txt"), b"base");

        mgr.create_branch_with_mode("work", "main", InheritanceMode::Lazy)
            .unwrap();
        mgr.with_branch("work", |branch| {
            write(&branch.delta_path("/existing.txt"), b"modified");
            Ok(())
        })
        .unwrap();

        mgr.delete_path_in_branch("work", "/existing.txt").unwrap();

        let status = mgr.branch_status("work").unwrap();
        assert_eq!(status.delta_entries, 0);
        assert_eq!(status.tombstones, 1);
        assert_eq!(status.diff.len(), 1);
        assert_eq!(status.diff[0].op, "delete");
        assert_eq!(status.diff[0].path, "/existing.txt");
    }

    #[test]
    fn hidden_paths_do_not_resolve_through_inheritance() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/.ssh/id_rsa"), b"SECRET");
        write(&tmp.path().join("base/.env"), b"TOKEN=x");
        write(&tmp.path().join("base/Projects/ok.py"), b"fine");

        mgr.create_branch_with_options(
            "agent",
            "main",
            InheritanceMode::Lazy,
            vec![".ssh".into(), "/.env".into()],
        )
        .unwrap();

        // hidden file and everything below a hidden dir are unresolvable
        assert!(mgr.resolve_path("agent", "/.ssh").unwrap().is_none());
        assert!(mgr.resolve_path("agent", "/.ssh/id_rsa").unwrap().is_none());
        assert!(mgr.resolve_path("agent", "/.env").unwrap().is_none());
        // non-hidden inherited paths still resolve
        assert!(mgr
            .resolve_path("agent", "/Projects/ok.py")
            .unwrap()
            .is_some());
        // the same paths stay visible on main (per-branch masking)
        assert!(mgr.resolve_path("main", "/.ssh/id_rsa").unwrap().is_some());
    }

    #[test]
    fn hidden_paths_filtered_from_readdir_but_delta_shadow_visible() {
        let tmp = TmpDir::new();
        let mgr = manager(&tmp);
        write(&tmp.path().join("base/.env"), b"TOKEN=real");
        write(&tmp.path().join("base/visible.txt"), b"data");

        mgr.create_branch_with_options("agent", "main", InheritanceMode::Lazy, vec![".env".into()])
            .unwrap();

        let names = mgr.collect_dir_names("agent", "/").unwrap();
        assert!(!names.contains(".env"), "hidden name listed: {:?}", names);
        assert!(names.contains("visible.txt"));

        // an agent-created delta at the hidden path shadows it: visible to
        // the agent, but never exposing the real underlay content
        write(
            &tmp.path().join("storage/branches/agent/files/.env"),
            b"TOKEN=agent-own",
        );
        let resolved = mgr.resolve_path("agent", "/.env").unwrap().unwrap();
        assert_eq!(std::fs::read(&resolved).unwrap(), b"TOKEN=agent-own");
        let names = mgr.collect_dir_names("agent", "/").unwrap();
        assert!(names.contains(".env"));
        // status reports the shadow as a delta so review policy can flag it
        let status = mgr.branch_status("agent").unwrap();
        assert!(status
            .diff
            .iter()
            .any(|e| e.path == "/.env" && e.op == "delta"));
    }

    #[test]
    fn hide_paths_survive_metadata_reload() {
        let tmp = TmpDir::new();
        write(&tmp.path().join("base/.netrc"), b"machine x login y");
        {
            let mgr = manager(&tmp);
            mgr.create_branch_with_options(
                "agent",
                "main",
                InheritanceMode::Lazy,
                vec![".netrc".into()],
            )
            .unwrap();
        }
        let mgr = manager(&tmp); // fresh manager over same storage
        assert!(mgr.resolve_path("agent", "/.netrc").unwrap().is_none());
        assert!(mgr.resolve_path("main", "/.netrc").unwrap().is_some());
    }
}

#[cfg(test)]
mod staged_merge_tests {
    use super::{commit_side_path, StagedMerge};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A unique temp directory removed on drop (even if a test panics).
    /// Matches the manual-cleanup style of the integration tests and avoids a
    /// dev-dependency, reusing the `uuid` crate already in `[dependencies]`.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("branchfs-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, data: &[u8]) {
        fs::write(path, data).unwrap();
    }

    fn tmp_of(dest: &std::path::Path) -> std::path::PathBuf {
        commit_side_path(dest, "tmp")
    }

    fn trash_of(target: &std::path::Path) -> std::path::PathBuf {
        commit_side_path(target, "trash")
    }

    #[test]
    fn commit_publishes_all_staged_files() {
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write(&src.join("a"), b"new-a");
        write(&src.join("b"), b"new-b");

        let mut staged = StagedMerge::new();
        staged.stage_copy("/a", &src.join("a"), &dst).unwrap();
        staged.stage_copy("/b", &src.join("b"), &dst).unwrap();
        let merged = staged.commit().unwrap();

        assert_eq!(merged.len(), 2);
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"new-a");
        assert_eq!(fs::read(dst.join("b")).unwrap(), b"new-b");
        // temps are gone after publish
        assert!(!tmp_of(&dst.join("a")).exists());
        assert!(!tmp_of(&dst.join("b")).exists());
    }

    #[test]
    fn commit_publishes_nested_paths() {
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write(&src.join("sub/x"), b"new-x");

        let mut staged = StagedMerge::new();
        staged
            .stage_copy("/sub/x", &src.join("sub/x"), &dst)
            .unwrap();
        staged.commit().unwrap();

        assert_eq!(fs::read(dst.join("sub/x")).unwrap(), b"new-x");
        assert!(!tmp_of(&dst.join("sub/x")).exists());
    }

    #[test]
    fn drop_without_commit_removes_temps_and_preserves_dest() {
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write(&src.join("a"), b"new-a");
        write(&dst.join("a"), b"old-a"); // pre-existing destination data

        {
            let mut staged = StagedMerge::new();
            staged.stage_copy("/a", &src.join("a"), &dst).unwrap();
            // temp exists while staged, dest still holds old data
            assert!(tmp_of(&dst.join("a")).exists());
            assert_eq!(fs::read(dst.join("a")).unwrap(), b"old-a");
            // dropped here without commit()
        }

        // temp cleaned up; destination data untouched
        assert!(!tmp_of(&dst.join("a")).exists());
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"old-a");
    }

    #[test]
    fn failed_stage_propagates_and_cleans_up() {
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write(&src.join("a"), b"new-a");
        write(&dst.join("a"), b"old-a");

        let mut staged = StagedMerge::new();
        staged.stage_copy("/a", &src.join("a"), &dst).unwrap();
        // staging a non-existent source makes copy_entry fail
        let err = staged.stage_copy("/missing", &src.join("missing"), &dst);
        assert!(err.is_err());
        drop(staged);

        // first temp removed, original dest data preserved (no partial publish)
        assert!(!tmp_of(&dst.join("a")).exists());
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"old-a");
    }

    #[test]
    fn stage_delete_then_commit_removes_target() {
        let tmp = TmpDir::new();
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        write(&dst.join("a"), b"old-a");

        let mut staged = StagedMerge::new();
        staged.stage_delete(&dst.join("a")).unwrap();
        // target is moved aside (gone from its path) but not yet destroyed
        assert!(!dst.join("a").exists());
        assert!(trash_of(&dst.join("a")).exists());
        staged.commit().unwrap();

        // target deleted, trash discarded
        assert!(!dst.join("a").exists());
        assert!(!trash_of(&dst.join("a")).exists());
    }

    #[test]
    fn stage_delete_rolled_back_on_drop() {
        let tmp = TmpDir::new();
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        write(&dst.join("a"), b"old-a");

        {
            let mut staged = StagedMerge::new();
            staged.stage_delete(&dst.join("a")).unwrap();
            assert!(!dst.join("a").exists());
            // dropped without commit()
        }

        // original restored with its data; trash gone
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"old-a");
        assert!(!trash_of(&dst.join("a")).exists());
    }

    #[test]
    fn stage_delete_missing_target_is_noop() {
        let tmp = TmpDir::new();
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&dst).unwrap();

        let mut staged = StagedMerge::new();
        staged.stage_delete(&dst.join("does-not-exist")).unwrap();
        staged.commit().unwrap();
    }

    #[test]
    fn replace_directory_with_file() {
        // Regression for the dir->file commit case: a base directory tombstoned
        // and replaced by a file at the same path. Staging the deletion first
        // clears the directory so the file can be published.
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst.join("p")).unwrap();
        write(&dst.join("p/inner"), b"inner"); // base dir has contents
        write(&src.join("p"), b"now-a-file"); // delta file replacing it

        let mut staged = StagedMerge::new();
        staged.stage_delete(&dst.join("p")).unwrap(); // tombstone the dir
        staged.stage_copy("/p", &src.join("p"), &dst).unwrap(); // file at same path
        staged.commit().unwrap();

        let meta = dst.join("p").symlink_metadata().unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(fs::read(dst.join("p")).unwrap(), b"now-a-file");
    }

    #[test]
    fn stage_copy_replaces_directory_without_explicit_tombstone() {
        // A delta file at a path where the destination is a directory, with no
        // staged deletion (reachable via rename, which clears the tombstone):
        // stage_copy must clear the directory itself so publish can place the file.
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst.join("d")).unwrap();
        write(&dst.join("d/inner"), b"inner");
        write(&src.join("d"), b"now-a-file");

        let mut staged = StagedMerge::new();
        staged.stage_copy("/d", &src.join("d"), &dst).unwrap(); // no stage_delete
        staged.commit().unwrap();

        assert!(dst
            .join("d")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(fs::read(dst.join("d")).unwrap(), b"now-a-file");
    }

    #[test]
    fn replace_directory_with_file_rolls_back_on_failure() {
        // If a copy fails after the directory deletion was staged, the dropped
        // StagedMerge must restore the original directory and its contents.
        let tmp = TmpDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst.join("p")).unwrap();
        write(&dst.join("p/inner"), b"inner");

        let mut staged = StagedMerge::new();
        staged.stage_delete(&dst.join("p")).unwrap();
        // copy of a non-existent source fails
        assert!(staged.stage_copy("/p", &src.join("p"), &dst).is_err());
        drop(staged);

        // directory and its contents restored exactly
        assert!(dst
            .join("p")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(fs::read(dst.join("p/inner")).unwrap(), b"inner");
        assert!(!trash_of(&dst.join("p")).exists());
    }

    #[cfg(unix)]
    #[test]
    fn distinct_non_utf8_names_get_distinct_side_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = std::path::Path::new("/dir");
        let p1 = dir.join(OsStr::from_bytes(b"\xff"));
        let p2 = dir.join(OsStr::from_bytes(b"\xfe"));
        // Lossy conversion would map both to U+FFFD and collide; raw OsStr must not.
        assert_ne!(commit_side_path(&p1, "tmp"), commit_side_path(&p2, "tmp"));
    }
}
