#!/usr/bin/env python3
"""Reconstruct the pinned async adaptation without modifying its input checkout."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

crate = Path(__file__).resolve().parents[1]
upstream = Path(sys.argv[1]).resolve()
manifest = json.loads((crate / "upstream-sources.json").read_text())
with tempfile.TemporaryDirectory(prefix="z-transparent-source-check-") as temporary:
    temporary = Path(temporary)
    for name, expected in manifest["files"].items():
        source = upstream / "transparent/crates/transparent-wallet/src" / name
        content = source.read_bytes()
        if hashlib.sha256(content).hexdigest() != expected:
            raise SystemExit(f"source differs from pinned file: {name}")
        target = temporary / "src" / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content.decode().split("#[cfg(test)]")[0])
    subprocess.run(["patch", "--batch", "--silent", "-p1", "-i", str(crate / "upstream.patch")], cwd=temporary, check=True)
    for name in manifest["files"]:
        if (temporary / "src" / name).read_bytes() != (crate / "src" / name).read_bytes():
            raise SystemExit(f"adapted file differs from committed patch: {name}")
print("Pinned source hashes and async adaptation match.")
