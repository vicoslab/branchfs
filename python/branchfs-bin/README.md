# vicoslab-branchfs-bin

Platform wheel wrapper for the vicoslab BranchFS command-line binary.

This package is built by the `vicoslab/branchfs` release workflow from the same
Git tag as the Rust crate. It contains the `branchfs` executable and exposes:

- a `branchfs` console script that execs the bundled binary;
- `branchfs_bin.branchfs_path()` for tools such as `ccc-agent` that want the
  exact packaged binary path.

The wheel intentionally does **not** bundle `libfuse3`. Install the host/runtime
package separately, for example `libfuse3-3` on Debian/Ubuntu systems.
