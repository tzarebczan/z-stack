#!/usr/bin/env python3
"""Offline inventory of the selected browser engine's runtime/build dependencies.

Cargo's full workspace resolve graph is NOT the browser's selected graph. Keep
upstream texts for crates that omit them in licenses/upstream; never fetch a
moving upstream branch during a release build.
"""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import os
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def cargo(*args):
    return subprocess.check_output(["cargo", *args, "--locked", "--offline"], cwd=ROOT, text=True)


def sha(data):
    return hashlib.sha256(data).hexdigest()


tree = cargo("tree", "-p", "z-wasm", "--target", "wasm32-unknown-unknown", "--edges", "normal,build", "--prefix", "none", "--format", "{p}")
selected = set(re.findall(r"^(\S+) v(\S+)", tree, re.M))
# Read manifests already downloaded for this exact build. Whole-workspace
# `cargo metadata` can require irrelevant platform packages on a clean CI host.
registry = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "registry/src"
paths = {(name, version): Path(path) for name, version, path in re.findall(r"^(\S+) v(\S+) \(([^)]+)\)", tree, re.M) if Path(path).is_absolute()}
lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
git_sources = {}
for locked in lock["package"]:
    source = locked.get("source", "")
    if (locked["name"], locked["version"]) not in selected or not source.startswith("git+"):
        continue
    assert source.startswith("git+https://github.com/valargroup/wallet-pir?"), f"review new browser git source: {source}"
    revision = source.rsplit("#", 1)[1]
    assert re.fullmatch(r"[0-9a-f]{40}", revision)
    checkouts = list((registry.parent.parent / "git/checkouts").glob(f"wallet-pir-*/{revision[:7]}"))
    assert len(checkouts) == 1, f"missing exact git source: {source}"
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=checkouts[0], text=True).strip()
    assert actual == revision, "git source revision mismatch"
    relative_source = "shared/pir-native" if locked["name"] == "pir-native" else "transparent/crates/" + locked["name"]
    directory = checkouts[0] / relative_source
    assert directory.is_dir(), f"review git crate layout: {locked['name']}"
    paths[(locked["name"], locked["version"])] = directory
    git_sources[locked["name"]] = f"https://github.com/valargroup/wallet-pir/tree/{revision}/{relative_source}"
packages = []
for name, version in sorted(selected):
    if name in {"z-engine", "z-wasm", "z-enhance-pir", "z-transparent-pir-v2"}:
        continue
    candidates = [paths[(name, version)]] if (name, version) in paths else list(registry.glob(f"*/{name}-{version}"))
    assert len(candidates) == 1, f"ambiguous/missing source for {name} {version}: build the engine first"
    manifest = candidates[0] / "Cargo.toml"
    package = tomllib.loads(manifest.read_text())["package"]
    package["manifest_path"] = str(manifest)
    packages.append(package)
supplemental = ROOT / "licenses/upstream"
sources = {item["file"]: item["url"] for item in json.loads((supplemental / "sources.json").read_text())}
fallbacks = {
    "bitcoin_hashes": "bitcoin_hashes-",
    "base58ck": "bitcoin_hashes-",
    "hex_lit": "hex_lit-",
    "bitcoin": "bitcoin_hashes-",
    "bitcoin-io": "bitcoin_hashes-",
    "bitcoin-units": "bitcoin_hashes-",
    "ipir-sp": "ipir-standard-",
    "valar-spiral-rs": "ipir-standard-",
    "inspiring": "ipir-standard-",
    "reinspiring": "ipir-standard-",
    "simplepir-kernel": "ipir-standard-",
    "blake2b_simd": "blake2_simd-",
    "blake2s_simd": "blake2_simd-",
    "equihash": "librustzcash-",
    "tonic-prost-build": "tonic-",
    "zakura-pczt": "pczt-librustzcash-",
    "zakura-pir-enhance-types": "pir-enhance-types-",
    "zcash_script": "zcash_script-",
    **{name: "transparent-wallet-pir-" for name in git_sources},
}
records = []
text = ["z-stack browser engine and SDK: third-party license texts\n",
        "Includes selected runtime AND build dependencies; inclusion does not mean every package's code is shipped.\n",
        "Original project code: Apache-2.0. Third-party code retains the following licenses.\n"]
for package in packages:
    name, version = package["name"], package["version"]
    if (name, version) not in selected or name in {"z-engine", "z-wasm", "z-enhance-pir", "z-transparent-pir-v2"}:
        continue
    assert package["license"], f"missing license declaration: {name}"
    directory = Path(package["manifest_path"]).parent
    files = sorted(p for p in directory.iterdir() if p.is_file() and re.match(r"^(LICENSE|COPYING|COPYRIGHT|NOTICE)", p.name, re.I))
    files += sorted(p for d in directory.iterdir() if d.is_dir() and d.name.lower() == "licenses" for p in d.rglob("*") if p.is_file())
    upstream = not files
    if upstream:
        prefix = fallbacks.get(name)
        assert prefix, f"review and retain missing license texts: {name} {version}"
        files = sorted(p for p in supplemental.iterdir() if p.name.startswith(prefix))
        assert files, f"missing retained upstream license: {name}"
    record = {"name": name, "version": version, "license": package["license"],
              "source": f"https://crates.io/crates/{name}/{version}", "repository": package.get("repository"), "texts": []}
    if name in git_sources:
        record["source"] = git_sources[name]
    if name == "enhance-pir":
        record["source"] = "https://github.com/valargroup/wallet-pir/tree/24f7694cd4f8274172a11cbff43c158bcab5d500/enhance/crates/enhance-pir"
    text.append(f"\n{'=' * 72}\n{name} {version} — {package['license']}\n{record['source']}\n")
    # Preserve the published declaration when its own legal files are absent.
    if upstream:
        record["licenseTextOrigin"] = "retained pinned repository license; crate directory omits legal files" if name in git_sources else "retained upstream texts; published archive omits legal files"
        if name in {"ipir-sp", "inspiring", "reinspiring", "simplepir-kernel", "valar-spiral-rs"}:
            record["note"] = "The published crate declares MIT OR Apache-2.0 and omits legal files. Retained standard Apache-2.0 text is used under that license option, without inventing a copyright notice."
            text.append(record["note"] + "\n")
        if name == "zakura-pir-enhance-types":
            record["note"] = "The published crate declares MIT OR Apache-2.0 and omits legal files. Retained standard Apache-2.0 text is used under that license option, without inventing a copyright notice. Source: zakura-core/wallet-libraries at d60d1442526afce96a40bfbea0f35c63f116d2af, zakura/pir-enhance-types."
        if name == "zakura-pczt":
            record["note"] = "The crate declares MIT OR Apache-2.0 in Cargo.toml and README. PCZT originates in zcash/librustzcash. Texts are retained from its root at 34f2b1e810ac8d00e671f0d254c7af7048c8985c, the source revision vendored by Zakura for this release; pczt's README links to absent legal files. Upstream packaging fix: https://github.com/zakura-core/wallet-libraries/pull/109."
        text.append("Published Cargo license declaration: " + package["license"] + "\n")
        if name in {"zakura-pczt", "zakura-pir-enhance-types"}:
            text.append(record["note"] + "\n" + (directory / "README.md").read_text() + "\n")
    for file in files:
        data = file.read_bytes()
        origin = sources[file.name] if upstream else (
            record["source"] + "/" + file.relative_to(directory).as_posix() if name == "enhance-pir" else
            f"https://docs.rs/crate/{name}/{version}/source/{file.relative_to(directory).as_posix()}")
        record["texts"].append({"file": file.name, "sha256": sha(data), "origin": origin})
        text.append(f"\n--- {file.name} ({origin}) ---\n" + data.decode("utf-8") + "\n")
    records.append(record)

# This source adaptation is project code, not a Cargo dependency.
vizor = supplemental / "vizor-LICENSE"
text.append("\n" + "=" * 72 + "\nAdapted Vizor Ledger code — Apache-2.0\nCopyright 2026 Vizor contributors.\n" + sources[vizor.name] + "\n" + vizor.read_text() + "\n")
adaptations = json.loads((ROOT / "licenses/source-adaptations.json").read_text())
for adaptation in adaptations:
    license_file = ROOT / adaptation["licenseText"]["file"]
    data = license_file.read_bytes()
    assert sha(data) == adaptation["licenseText"]["sha256"], "source adaptation license changed; review its provenance"
    text.append("\n" + "=" * 72 + "\nAdapted " + adaptation["component"] + " — " + adaptation["license"] + "\n"
                + adaptation["copyright"] + "\nOriginal source revision: " + adaptation["originalRevision"] + "\n"
                + data.decode("utf-8") + "\n")
inventory = {"target": "wasm32-unknown-unknown", "edges": "normal,build", "cargoLockSha256": sha((ROOT / "Cargo.lock").read_bytes()), "dependencies": records, "sourceAdaptations": adaptations}
outputs = {ROOT / "licenses/WASM-dependencies.json": json.dumps(inventory, indent=2) + "\n", ROOT / "THIRD_PARTY_LICENSES.txt": "".join(text).replace("\r\n", "\n")}
for file, contents in outputs.items():
    if "--check" in sys.argv:
        assert file.read_text() == contents, f"stale {file.name}; run python3 scripts/license-inventory.py and review new dependencies"
    else:
        file.write_text(contents)
print(f"Browser license inventory: {len(records)} third-party runtime/build dependencies")
