# Changelog

## 0.1.0-alpha.9

- Refresh per-pool history totals once after scanning wallet activity, including mined self-sends, without repeated catch-up rebuilds.
- Recognize Zebra's duplicate mempool and queued-download responses without encouraging a replacement payment.
- Preserve app-owned payment guard errors through rollback, and display fresh sync state before validating or unlocking a payment.
- Report interrupted syncs as cancellation instead of an unknown error.
- Cancel the pre-send sync with the payment; sync before rechecking example payments and preserve clear expired-review errors through the public SDK's error normalization and pre-broadcast guard.
- Run example setup guards after installation and before dev, build and type checking; clarify transaction ID byte order and scanner state after local removal.
- Split the browser wallet into documented create, recovery, restore, sync, payment, removal and rendering modules, with checked DOM lookups and shared action coordination.

## 0.1.0-alpha.8

- On-demand memo retrieval reports remaining batches; a completed visible history page no longer claims the whole queue is loaded.
- Recognize Zebra's already-mined broadcast response as delivered in browser and native wallets.
- Put recovery words before acknowledgement and hide unused wallet panels during backup.
- Explain confirming funds on the send form and keep payment errors beside the fields.
- Add explicit activity detail/memo retrieval, accurate self-send fee labels, and compact-history movement copy.
- Document a reported public testnet receive/self-send and shorten the first-session walkthrough.
- Use ChainSafe's browser gRPC-Web origin in the diagnostic testnet preset.


## 0.1.0-alpha.7

- Separate available and confirming balances in Vite and Next demos; show activity amounts and confirmation counts.
- Retain restore words on failed Next attempts and identify invalid recovery inputs accessibly in both demos.
- Compress WASM in Vite preview and apply immutable caching consistently to GET, HEAD and 304 responses.
- Default the public chain checker to the published NU7 activation hash; make explorer comparisons explicit.
- Clarify successful public receive evidence, remaining public-send verification and generated setup instructions.

## 0.1.0-alpha.6

- Show SDK version/source identity in generated Vite and Next apps; generate receive QR locally.
- Provide preview-only downloads/checksums and an installed-app public chain checker.
- Recommend the Valar testnet faucet, with daily-limit handling and current funding evidence.
- Clarify public funding evidence, remove maintainer steps from the beginner path, and fix stale validation/scan copy.
- Demonstrate immutable hashed-asset caching in Vite preview; document compressed WASM sizes.

## 0.1.0-alpha.5

- Add scanner runtime events and local engine download/verification/startup progress.
- Lock on pagehide in wallet examples; preserve the worker for best-effort snapshot saves and back/forward-cache returns.
- Clarify local diagnostic setup, faucet chain checks, missing explorer blocks and portable archive verification.

## 0.1.0-alpha.4

- Stable `invalid_recovery_phrase` errors for restore and payment unlock, with fixed display copy; shared errors no longer prescribe native CLI commands or wiping wallet data.
- Optional `wallet.forget({ pending: "reject" })` preserves unresolved outgoing reservations across tabs and atomically refuses deletion if the inspected snapshot changes.
- Numeric-birthday wallet creation skips the tip request entirely; automatic-tip failures use the stable `transport` code.
- Vite demo creation with an explicit offline birthday, a visible runtime and funding warning, receive/scan context, confirmed local removal guarded while payments remain pending, hidden restore after creation, and payment errors beside the form.
- Documented engine download costs and the optional single-thread setting.
- Prebuilt-first integration docs and preview bundle aligned with this release; clearer missing-archive recovery and quiet checksum commands.
- Public-testnet funded spending remains uncertified while providers disagree after NU7. Source-checkout funded regtest remains a separate verification path.

## 0.1.0-alpha.3

- Self-contained SDK archive with matching core/passkey helpers; no registry lookup for unpublished packages.
- First-scan payment readiness, restore-input retention on failure, explicit phrase/address copy controls and safe error diagnostics.
- Generated example instructions retain setup links and use vendored archives; documentation fragments are validated.
- Next.js tracing stays inside the generated app; recovery and address copy feedback remain separate.

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
