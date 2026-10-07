# Licensing

Original z-stack code is licensed under Apache-2.0. This is compatible with the
Apache-2.0 Ledger adaptation and the MIT/Apache options offered by the Zakura
dependencies. It permits commercial integration and modification and includes
the contributors' express patent grant. It does not grant trademark rights.

Third-party code keeps its existing terms. Preserve LICENSE, NOTICE and
THIRD_PARTY_LICENSES.txt when redistributing the browser packages. Mark changes
to adapted Apache-licensed files. The license choice does not authorize npm
publication or change the repository's visibility.

[The browser inventory](../licenses/WASM-dependencies.json) records exact
runtime/build dependencies, declared licenses (including legacy Cargo expressions) and hashes of retained
license texts. It is deliberately based on `cargo tree -p z-wasm` for wasm32,
rather than all features of the desktop workspace. Build dependencies are
included conservatively. The aggregate is shipped with each npm archive.

Use Python 3.11+ with the built engine’s Cargo sources available. Regenerate
after changing Cargo.lock or dependencies:

```sh
python3 scripts/license-inventory.py
python3 scripts/license-inventory.py --check
```

The generator is offline and fails for unaudited missing texts. Supplemental
texts in `licenses/upstream` come from pinned sources listed in `sources.json`.
Some published crates omit legal files. PCZT originates in zcash/librustzcash;
Zakura vendors it and publishes `zakura-pczt`. The rc3 crate declares
MIT OR Apache-2.0. We retain the original root license texts from the exact
librustzcash revision vendored for that release, including its copyright notices.
These texts and pinned provenance ship in every SDK archive. Missing legal files
in an upstream archive are a packaging defect, not a request for a new license
grant. [The upstream fix](upstream/PCZT-LICENSE-PACKAGING.md) remains open; retain
our fallback until a compatible published crate includes the correct files.

The inventory does not cover a separately distributed native executable or
desktop app. Those have additional dependencies, including MPL-2.0 components,
and need their own artifact-specific notices and source-availability check.
Rust crate publication also needs a separate review of workspace patches.

The optional `@z-stack/base` package has a separate [runtime inventory](../licenses/base/dependencies.json) and [retained notices](../licenses/base/LICENSES.txt). Regenerate with `node scripts/base-license-inventory.mjs` and verify with `--check`; its archive does not ship the WASM inventory.
