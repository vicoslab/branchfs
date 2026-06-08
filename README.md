# BranchFS

BranchFS is a FUSE-based filesystem that enables speculative branching on top of any existing filesystem. It gives AI agents isolated workspaces with instant copy-on-write branching, atomic commit-to-parent, and zero-cost abort, no root privileges required.

## Features

| Feature | Description |
|---------|-------------|
| Lazy or Snapshot Isolation | Branches default to lazy inherited views for O(1) creation; legacy eager snapshots are available with `branchfs create --snapshot` |
| Commit to Parent | Changes merge into immediate parent branch (or base if parent is main) |
| Atomic Abort | Instantly discards leaf branch, parent and siblings unaffected |
| Atomic Commit | Merges leaf branch into parent atomically |
| mmap Invalidation | Memory-mapped files trigger SIGBUS after commit/abort |
| @branch Virtual Paths | Access any branch directly via `/@branch-name/` without switching; hide these with `branchfs mount --agent` for untrusted agent mounts |
| Portable | Works on any underlying filesystem (ext4, xfs, nfs, etc.) |

## Architecture

BranchFS is a FUSE-based filesystem that requires no root privileges for ordinary FUSE mounts. It implements file-level copy-on-write over an inherited parent view. By default, new branches use lazy inheritance: branch creation writes metadata and empty delta/tombstone stores without walking the inherited tree, and lookups resolve `branch delta > branch tombstone > parent/base` at access time. Legacy eager snapshot inheritance remains available with `branchfs create --snapshot`. When a file is modified on a branch, the file is copied to the branch's delta storage. Deletions are tracked via tombstone markers. On commit, changes from a leaf branch are merged into its immediate parent (or applied to the base directory if the parent is main); on abort, the leaf branch's delta storage is discarded.

### Why not overlayfs?

Overlayfs only supports a single upper layer (no nested branches), and lacks commit-to-root semantics—changes remain in the upper layer rather than being applied back to the base. It also has no cross-mount cache invalidation needed for speculative execution workflows.

### Why not btrfs subvolumes?

Btrfs subvolumes are tied to the btrfs filesystem, making them non-portable across ext4, xfs, or network filesystems. Snapshots create independent copies rather than branches that commit back to a parent, and there's no mechanism for automatic cache invalidation when one snapshot's changes should affect others.

### Why not dm-snapshot?

Device mapper snapshots operate at the block level, requiring a block device, so they can't work on NFS, existing FUSE mounts, or arbitrary filesystems. Merging a snapshot back to its origin is complex and destructive, and like overlayfs, dm-snapshot only supports single-level snapshots without nested branches.

### What about FUSE overhead?

FUSE adds userspace-kernel context switches per operation, which is slower than native kernel filesystems. However, for speculative execution with AI agents, the bottleneck is typically network latency (LLM API calls at 100ms-10s) and GPU compute, not file I/O. FUSE overhead is negligible in comparison.

## Prerequisites

- Linux with FUSE support or macOS with macFUSE
- libfuse3 development libraries (Linux) or macFUSE (macOS)
- Rust toolchain (1.70 or later)

### Installing Dependencies

**Debian/Ubuntu:**
```bash
sudo apt install libfuse3-dev pkg-config
```

**Fedora:**
```bash
sudo dnf install fuse3-devel pkg-config
```

**Arch Linux:**
```bash
sudo pacman -S fuse3 pkg-config
```

**macOS:**
```bash
brew install macfuse pkg-config
```

### macOS Support

BranchFS supports macOS via **macFUSE**. 

1. **Install macFUSE**: `brew install macfuse pkg-config`.
2. **System Extension**: You must approve the `macFUSE` system extension in System Settings. On Apple Silicon Macs, you may need to enable third-party kernel extensions in Recovery Mode.
3. **Control Interface**: Since `ioctl` support can be inconsistent on macOS, BranchFS provides a reliable write-based interface. You can send commands to `.branchfs_ctl` via direct writes:
   - `echo "create:name" > .branchfs_ctl`
   - `echo "commit" > .branchfs_ctl`
   - `echo "abort" > .branchfs_ctl`
4. **FUSE ABI**: On macOS, BranchFS targets FUSE ABI 7.31 for maximum compatibility and to resolve path resolution issues.
5. **Advanced Features**: Linux-specific features like FUSE passthrough and `RENAME_EXCHANGE` are currently disabled on macOS.

## Building

```bash
git clone https://github.com/user/branchfs.git
cd branchfs
cargo build --release
```

The binary is located at `target/release/branchfs`.

## Usage Examples

### Basic Workflow

```bash
# Mount filesystem (auto-starts daemon, starts on main branch)
branchfs mount --base ~/project /mnt/workspace

# Create a branch (auto-switches to it)
branchfs create experiment /mnt/workspace

# Work in the branch (files modified here are isolated)
cd /mnt/workspace
echo "new code" > feature.py

# List branches
branchfs list

# Commit changes to base (switches back to main, stays mounted)
branchfs commit /mnt/workspace

# Or abort to discard (switches back to main, stays mounted)
branchfs abort /mnt/workspace

# Unmount when done (branch storage persists; daemon exits when last mount removed)
branchfs unmount /mnt/workspace
```

### CCC / Agent-Safe Lazy Branch Mode

For large CCC-style writable mounts, create branches lazily and mount the selected branch directly. Lazy is the default for new branches; use `--snapshot` only when you explicitly want the legacy eager inherited-tree copy.

```bash
# Trusted launcher/reviewer side: mount with controls visible and create a lazy branch.
branchfs mount \
  --base /__real/storage_user \
  --storage /__branchfs_store/storage_user \
  /__branchfs_admin/storage_user
branchfs create \
  train-run-123 \
  /__branchfs_admin/storage_user \
  --storage /__branchfs_store/storage_user

# Agent side: expose the selected branch at mount root and hide .branchfs_ctl/@branch.
branchfs mount \
  --base /__real/storage_user \
  --storage /__branchfs_store/storage_user \
  --branch train-run-123 \
  --agent \
  /__branchfs_mounts/storage_user

# Trusted review/commit flow.
branchfs freeze train-run-123 --storage /__branchfs_store/storage_user
branchfs status train-run-123 --storage /__branchfs_store/storage_user
branchfs commit-branch train-run-123 --storage /__branchfs_store/storage_user
# or: branchfs abort-branch train-run-123 --storage /__branchfs_store/storage_user
```

Useful commands for this mode:

- `branchfs mount --branch <name>` exposes a selected branch as the mount root.
- `branchfs mount --agent` or `--no-control` hides `.branchfs_ctl` and `@branch` paths from an untrusted mount.
- `branchfs status <name> --json` reports branch deltas and tombstones without scanning the base tree.
- `branchfs freeze <name>` makes a branch read-only for stable review; `branchfs thaw <name>` reopens it.
- `branchfs commit-branch <name>` and `branchfs abort-branch <name>` are trusted-control operations that do not require exposing `.branchfs_ctl` inside the agent mount.

Relaxed multi-writer usage is intended for distributed jobs where nodes write different files in the same branch, e.g. per-host/per-rank logs and checkpoint shards. Concurrent writes/deletes/renames of the same path are not guaranteed.

`--agent` is a security boundary helper: it hides the mounted control file and virtual branch namespace from the agent-visible tree. The trusted review/commit container must still keep real underlays and the BranchFS store/control channel out of the untrusted agent container.

### Nested Branches

```bash
# Mount and create hierarchy
branchfs mount --base ~/project /mnt/workspace
branchfs create level1 /mnt/workspace           # auto-switches to level1
branchfs create level2 /mnt/workspace -p level1 # auto-switches to level2

# Now on level2, work in it
echo "deep change" > /mnt/workspace/file.txt

# Commit from level2 merges into level1, switches to level1
branchfs commit /mnt/workspace

# Now on level1, commit to base
branchfs commit /mnt/workspace
# Changes from both level2 and level1 are now in base, switches to main
```

### @branch Virtual Paths

Every non-main branch is accessible as a virtual directory at the mount root, without switching the current branch:

```bash
branchfs mount --base ~/project /mnt/workspace

# Create two branches
branchfs create feature-a /mnt/workspace
branchfs create feature-b /mnt/workspace

# Access both branches simultaneously via @branch paths
cat /mnt/workspace/@feature-a/file.txt
cat /mnt/workspace/@feature-b/file.txt

# Write to a specific branch without switching
echo "change" > /mnt/workspace/@feature-a/src/main.rs

# Each @branch has its own control file
echo "commit" > /mnt/workspace/@feature-a/.branchfs_ctl
echo "abort" > /mnt/workspace/@feature-b/.branchfs_ctl
```

Nested branches can be accessed at both `/@child/` and `/@parent/@child/`:

```bash
branchfs create child /mnt/workspace -p feature-a

# Both paths reach the same branch
cat /mnt/workspace/@child/file.txt
cat /mnt/workspace/@feature-a/@child/file.txt
```

This is useful for multi-agent workflows where each agent can bind-mount a different `@branch` path to work on isolated branches in parallel within the same mount.

### Parallel Speculation (Multiple Agents)

With `@branch` virtual paths, multiple agents can work in parallel through a single mount:

```bash
# Mount once
branchfs mount --base ~/project /mnt/workspace

# Create branches for each agent
branchfs create agent-a /mnt/workspace
branchfs create agent-b /mnt/workspace

# Each agent works via its own @branch path (no switching needed)
echo "approach a" > /mnt/workspace/@agent-a/solution.py
echo "approach b" > /mnt/workspace/@agent-b/solution.py

# Commit one agent's work
echo "commit" > /mnt/workspace/@agent-a/.branchfs_ctl

# agent-b is unaffected
cat /mnt/workspace/@agent-b/solution.py  # still works
```

## Semantics

### Shared Branch Namespace

All control-enabled mounts share a single branch namespace managed by the daemon. Branches created through any such mount are visible via `@branch` virtual paths. Mounts started with `--agent`/`--no-control` hide these control paths from the mounted tree. This simplifies multi-agent workflows — each agent accesses its branch via `/@branch-name/` without needing separate mount points.

### Commit

Committing merges a **leaf branch** into its immediate parent:

1. Only leaf branches can be committed, attempting to commit a branch with children returns an error
2. If the parent is **main**: tombstone deletions are applied to the base filesystem, then delta files are copied to base
3. If the parent is **another branch**: child's delta files are merged into the parent's delta directory, and tombstones are merged (child tombstones shadow parent deltas, child deltas un-tombstone parent tombstones)
4. The committed branch is removed; epoch increments
5. **Mount automatically switches to the parent branch** (stays mounted)
6. Memory-mapped regions trigger `SIGBUS` on next access

### Abort

Aborting discards only the **leaf branch** without affecting the parent:

1. Only leaf branches can be aborted, attempting to abort a branch with children returns an error
2. The leaf branch's delta storage is discarded
3. Other branches (including the parent) continue operating normally
4. **Mount automatically switches to the parent branch** (stays mounted)
5. Memory-mapped regions in the aborted branch trigger `SIGBUS`

### Unmount

Unmounting removes the FUSE mount:

1. The FUSE session is torn down
2. The daemon automatically exits when the last mount is removed
