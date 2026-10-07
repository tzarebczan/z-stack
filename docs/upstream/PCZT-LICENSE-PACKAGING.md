# zakura-pczt legal files

[Upstream PR 109](https://github.com/zakura-core/wallet-libraries/pull/109) was merged
on October 6, 2026, as `cf1dcfec88062c420322578226d5240328002d3d`. It changes
only the two license files; no API, crypto or license selection changes.

## Provenance

PCZT originates in `zcash/librustzcash`. Zakura vendors that library and publishes
`zakura-pczt`; the 0.1.0-rc3 archive declares `MIT OR Apache-2.0` but omits the
legal files linked by its README. The original PCZT manifest inherits the root
license. The root `COPYING.md` records the dual license.

The patch copies the original root license texts byte for byte from
[`34f2b1e810ac8d00e671f0d254c7af7048c8985c`](https://github.com/zcash/librustzcash/tree/34f2b1e810ac8d00e671f0d254c7af7048c8985c),
which Zakura's vendor commit records as the source revision for this release.
The MIT notice is copyright 2017–2021 The Electric Coin Company. The backend's
separate MIT notice is not the source used for PCZT.

[Prepared patch](pczt-license-files.patch) applies from the Zakura repository
root. Our [source ledger](../../licenses/upstream/sources.json) and browser
inventory retain these original texts for offline consumers. This corrects
packaging and attribution; a new license grant is not needed.

## Verification and follow-up

Both `cargo package -p zakura-pczt --list --allow-dirty` and actual packaging
with `--no-verify` passed in the upstream checkout. Both legal files are in the
resulting `.crate`, match the original texts byte for byte, and satisfy the
README's relative links. This verifies archive contents, not a full upstream
build or maintainer acceptance.

Upstream acceptance is complete. Our pinned rc3 archive still omits the files.
Keep our retained texts until a compatible published crate includes the correct
files and passes our license checks.
Do not upgrade crypto dependencies solely to remove the fallback.
