#!/usr/bin/env python3
"""Set the version in BOTH release manifests (Cargo.toml + pyproject.toml).

The two-manifest rule (CLAUDE.md "Release versioning"): the GitHub
release workflow triggers on v* tags and maturin builds wheels from
pyproject.toml. Bumping only Cargo.toml makes the tag rebuild the
*previous* PyPI version, which PyPI rejects with "400 File already
exists". This script makes the desync impossible.

Usage: python3 scripts/bump.py X.Y.Z
"""

import re
import sys
from pathlib import Path

MANIFESTS = ("Cargo.toml", "pyproject.toml")


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: bump.py X.Y.Z")
    version = sys.argv[1]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        sys.exit(f"not a plain semver (X.Y.Z, no leading v): {version!r}")

    repo_root = Path(__file__).resolve().parent.parent.parent
    for name in MANIFESTS:
        path = repo_root / name
        text = path.read_text(encoding="utf-8")
        # First `version = "..."` line wins: [package] in Cargo.toml and
        # [project] in pyproject.toml both put it before any dependency
        # tables; dependency version keys are never at line start.
        new_text, n = re.subn(
            r'^version = "[^"]+"',
            f'version = "{version}"',
            text,
            count=1,
            flags=re.M,
        )
        if n != 1:
            sys.exit(f"{name}: no `version = \"...\"` line found")
        path.write_text(new_text, encoding="utf-8")
        print(f"{name} -> {version}")


if __name__ == "__main__":
    main()
