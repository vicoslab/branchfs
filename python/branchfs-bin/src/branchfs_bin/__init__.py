"""Helpers for locating and executing the bundled vicoslab BranchFS binary."""

import os
import stat
import sys
from importlib import metadata, resources

try:
    __version__ = metadata.version("vicoslab-branchfs-bin")
except metadata.PackageNotFoundError:  # pragma: no cover - source-tree import
    __version__ = "0+source"


def branchfs_path():
    """Return the installed bundled BranchFS executable path."""
    return str(resources.files("branchfs_bin").joinpath("bin", "branchfs"))


def _ensure_executable(path):
    try:
        mode = os.stat(path).st_mode
        if not mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH):
            os.chmod(path, mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    except OSError:
        # Let execv report the actual failure below.
        pass


def main(argv=None):
    """Console entry point that execs the packaged Rust binary."""
    argv = list(sys.argv[1:] if argv is None else argv)
    binary = branchfs_path()
    _ensure_executable(binary)
    os.execv(binary, [binary] + argv)
