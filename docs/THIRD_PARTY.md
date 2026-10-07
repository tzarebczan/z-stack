# Third-party code

## Vizor (chainapsis/vizor-wallet)

`crates/z-engine/src/ledger/{apdu,parse,serializer}.rs` are adapted from
Vizor's `rust/src/wallet/ledger/` (commit e5f6bea, 2026-09-23), licensed under
the Apache License, Version 2.0 (<https://www.apache.org/licenses/LICENSE-2.0>).

Changes from the original:

- Commands are plain data for a browser transport (WebHID) instead of a desktop
  HID session; device management, cancellation and timeouts live in the SDK.
- Transparent inputs, and transparent outputs with a BIP-32 derivation, are
  refused (the web wallet does not spend transparent funds from a device).
- The Ledger account fingerprint uses BLAKE2b with a z-stack personalization
  instead of SHA-256.
- Signature decoding checks status words before counting responses.

## Wallet history helpers

`packages/core/src/history.ts` adapts transaction classification and display
helpers from an earlier TypeScript wallet library under MIT. Its original
copyright and permission notice are retained in
[the full license text](../licenses/wallet-history-MIT.txt), `NOTICE` and every
core/SDK package notice bundle. [Source adaptation records](../licenses/source-adaptations.json)
identify the original revision and license checksum. This includes only the
history helpers; no earlier WASM fork is vendored. Original SDK code and the
modifications remain Apache-2.0.
