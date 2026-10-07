# Changelog

## 0.1.0-alpha.1 (unpublished)

The first alpha is distributed through source builds and matching package
archives. Packages are not published to npm or Cargo. Pin exact alpha versions;
integration contracts may change. `/engine` and `/lab` remain experimental.

### Wallet SDK

- Network-bound wallet clients with awaited close, explicit replacement,
  transactional application-owned storage and optional unlockers.
- Local Rust/WASM derivation, scanning, proving and signing, with production
  single-threaded and threaded engines and integrity manifests.
- Durable spending reservations and receipt reconciliation after uncertain
  submission; cancellation and stale-tab guards preserve committed wallet state.
- Recovery confirmation before the first durable create, explicit storage-loss
  errors and worker recovery after close, failure or interrupted hardware proofs.
- Recovery phrases returned by creation; lock removes spending access while
  persisted viewing history remains readable.
- On-demand memo retrieval, opt-in automatic shielding and fixed-label runtime
  diagnostics that do not log private error payloads.
- Optional account and backup interfaces, independent passkey vaults, and an
  app-owned WebAuthn/SQLite remote-backup example.
- Lightweight setup diagnostics and experimental adapter acceptance helpers.

### Optional Base package

- Separate `@z-stack/base` ETH/USDC wallet with application-owned RPC, unlock,
  durable reservations and runtime license notices.
- Same-phrase EVM derivation without retaining a signer or recovery phrase.
- Payment review distinguishes signed L2 limits from variable fee estimates;
  finalized canonical receipts reconcile unknown submissions before retry.
- Experimental smart-account and sponsorship primitives with explicit delegation
  approval, application-owned bundlers/paymasters and durable signature journals.
- Base-only and opt-in combined Vite/Next examples.

### Integration and tooling

- Installed-archive TypeScript, React, Next.js webpack and local-passkey examples.
- Browser acceptance for Chromium, Firefox and WebKit with real WASM engines;
  separate funded loopback regtest and isolated local-EVM checks.
- Reproducible Zakura patches, retained third-party notices, Apache-2.0 project
  terms and separate browser/Base license inventories.
- Matching package/runtime versions, runnable preview bundles, checksums and an
  offline API reference.
- Public integration, privacy and support guides with explicit testing limits.
