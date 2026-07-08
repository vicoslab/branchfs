"""Build a platform wheel containing the prebuilt vicoslab BranchFS binary."""

import os
import pathlib
import shutil
import stat

from setuptools import find_packages, setup

HERE = pathlib.Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
PKG_DIR = HERE / "src" / "branchfs_bin"
BIN_DIR = PKG_DIR / "bin"
BINARY_PATH = BIN_DIR / "branchfs"


def _read_cargo_version():
    cargo_toml = REPO_ROOT / "Cargo.toml"
    for line in cargo_toml.read_text().splitlines():
        line = line.strip()
        if line.startswith("version"):
            return line.split("=", 1)[1].strip().strip('"')
    raise RuntimeError("could not find package version in Cargo.toml")


def _version():
    return os.environ.get("BRANCHFS_BIN_VERSION") or _read_cargo_version()


def _stage_binary_from_env():
    src = os.environ.get("BRANCHFS_BIN_BINARY")
    if not src:
        return
    src_path = pathlib.Path(src)
    if not src_path.exists():
        raise RuntimeError("BRANCHFS_BIN_BINARY does not exist: %s" % src)
    BIN_DIR.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src_path, BINARY_PATH)
    mode = BINARY_PATH.stat().st_mode
    BINARY_PATH.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


VERSION = _version()
_stage_binary_from_env()

if not BINARY_PATH.exists():
    raise RuntimeError(
        "missing %s; build BranchFS first and either copy target/.../branchfs "
        "there or set BRANCHFS_BIN_BINARY" % BINARY_PATH)

setup(
    name="vicoslab-branchfs-bin",
    version=VERSION,
    description="Prebuilt vicoslab BranchFS CLI binary",
    long_description=(HERE / "README.md").read_text(),
    long_description_content_type="text/markdown",
    license="MIT",
    url="https://github.com/vicoslab/branchfs",
    package_dir={"": "src"},
    packages=find_packages("src"),
    package_data={"branchfs_bin.bin": ["branchfs"]},
    include_package_data=True,
    zip_safe=False,
    entry_points={"console_scripts": ["branchfs=branchfs_bin:main"]},
    python_requires=">=3.9",
    platforms=["Linux"],
)
