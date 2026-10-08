# Changelog

## 0.1.0-alpha.3

- NU7-capable Common 2.2.0, wallet backend/SQLite rc7 and PCZT rc4; protocol, address and transparent dependencies now use matching Zakura aliases.
- Testnet NU7 activation at 4,465,026; mainnet activation remains unset upstream.
- Network-aware birthday estimates span Blossom and NU7 spacing changes while preserving the date safety margin in seconds.
- Native HTTP date birthdays resolve against the selected server’s live tip; CLI date restores use the same safety margin.
- Diagnostic birthday previews and reverse-date estimates use the selected network’s block spacing.
- Optional regtest NU7 scheduling reaches page, scan and proving instances; incompatible runtime configurations are rejected.
- NU7 hardware PCZTs select the post-NU6.3 proof circuit.
- Native memo and status queues use the wallet libraries' separate APIs without public fallback for private-routed work.
- Optional pinned NU7 light-server fixture for reproducible native and browser acceptance.
- Rebases of the three WASM performance patches onto verified 2.2.0 archives, with refreshed dependency/legal inventories.

## 0.1.0-alpha.2

- Prebuilt downloadable archives and a Rust-free example quickstart; npm and Cargo publication remain deferred.
- Safe earlier-birthday browser rescans with pending-payment, persistence and stale-tab guards.
- Committed rescans reset transparent and memo completion indicators and reload subtree roots; failed saves preserve the previous state.
- Numeric birthdays such as zero, NaN and unsafe heights now fail with `invalid_birthday` instead of silently selecting a default. Rescans require an explicit date or height.
- Typed birthday validation and payment-link hints; forms retain the phrase on local birthday errors.
- Visible network/server/birthday/scan context, accurate runtime labels, informational ownership checks, TAZ units and favicon assets.
- Scaffolder install instructions, consistent privacy defaults and Next agent-rule generation disabled.
- Public testnet faucet/server mismatch documented; funded acceptance remains reproducible on isolated regtest.

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
