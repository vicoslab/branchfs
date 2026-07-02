# AGENTS.md — BranchFS CCC Agent Mode Work

This checkout is being modified for a CCC-oriented BranchFS prototype.

## Primary objective

Add a lazy, relaxed multi-writer mode suitable for huge NFS-backed CCC writable mounts (`/storage/user`, `/storage/group`, and `/home/$USER` as a subpath of `/storage/user`). Branch creation must be O(1) and must not recursively scan/copy base trees.

## Required behavior for this pass

1. Add lazy inheritance mode as the default for new branches.
2. Preserve legacy eager snapshot behavior behind `branchfs create --snapshot`.
3. In lazy mode, branch resolution should be `branch delta > branch tombstone > parent/base` at lookup time.
4. In lazy mode, `readdir` should union only the requested base/parent directory and delta directory; never scan whole base.
5. Allow mounting a selected branch as root via `branchfs mount --branch <name>`.
6. Add a status/change-list command that walks branch delta/tombstone data only.
7. Add freeze/thaw/read-only branch state if feasible; mutating FUSE ops on frozen branches should fail.
8. Add tests where possible.

## Relaxed multi-writer semantics

Support multiple nodes writing different files in the same branch store. Do not attempt strict same-file concurrent write correctness in this pass. Document same-path write/delete/rename races as undefined or future work.

## Security boundary

Untrusted agents must not be able to commit. If `.branchfs_ctl` remains commit-capable, document that agent containers must not expose it or must use a future agent-safe control mode.

## Performance

Never introduce full-tree scans for branch creation/status/diff. Common new-file artifact writes should be direct delta writes.

## Validation

Run `cargo fmt` and `cargo test` if a usable Rust toolchain is available. If not, keep code syntactically careful and document the environment limitation.
