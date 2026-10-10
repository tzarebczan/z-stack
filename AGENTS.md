# Contribution guidance

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, and
[docs/README.md](docs/README.md) for integration contracts.

## Architecture

- Zcash cryptography, transaction construction and hardware signature verification
  belong in `z-engine`. TypeScript handles transport, workers, storage and events.
  Optional Base operations use viem; do not implement cryptographic primitives.
- Keep the wallet independent of registration, authentication and remote backup.
  These are optional, application-owned services. Base, swap and fiat integrations
  must not become requirements for a Zcash wallet.
- SDK root exports are the supported integration surface. `/engine` and `/lab`
  are experimental. Consumers use built `dist/` exports; repository tools may use
  the `@z-stack/source` condition. Verify installed archives after export changes.
- WebAuthn and PRF vault operations belong in `@z-stack/passkey`, which has no
  runtime dependencies. Keep browser vault `navigator.credentials` calls there.
  Server-side WebAuthn verification belongs to the app.
- Keep native I/O behind Rust feature gates. `z-wasm` disables default features;
  both single-threaded and threaded WASM must build. Keep Sapling proving
  parameters separate and integrity-checked. Run proving off the UI thread.
- Keep native wallet bridges and development block pipes bound to loopback.
  Browser deployments use an explicitly configured gRPC-Web server or transport.

## Security and correctness

- Never commit seeds, private keys, wallet databases, credentials or secret `.env`
  files. Use empty wallets or disposable regtest fixtures for reproductions.
  Never write a plaintext `mnemonic.txt`; use encrypted seed storage or an OS keystore.
- Preserve committed state on cancellation, quota failures and cross-tab conflicts.
  Persist spending reservations before broadcast; reconcile uncertain submissions
  rather than constructing replacement payments. Never log private error payloads.
- Lock removes spending access, not persisted viewing history. Account logout,
  local deletion and remote backup deletion are separate operations.
- Registration and backup policies must remain explicit. Automatic shielding and
  transaction-ID memo retrieval are opt-in; do not change privacy defaults silently.
- Match every regtest client's activation schedule to its fixture. Compose and
  native fixtures use different NU6.3 heights; do not run competing funded tests
  against the same faucet.

## Changes and verification

- Add regression coverage for persistence, authorization, spending and concurrency
  changes. Rebuild both WASM variants after Rust changes; typed stubs do not verify
  engine behavior. Use [the verification guide](docs/RELEASE.md) for relevant checks.
- Keep SDK, core, passkey, Base, workspace and `SDK_VERSION` versions in sync.
  Update public documentation and examples when changing an integration contract.
- Use pinned Zakura dependencies. Extend `z-engine` before patching upstream;
  record necessary patches, provenance, tests and removal criteria in
  [docs/UPSTREAM.md](docs/UPSTREAM.md). Preserve original third-party legal notices.
- Sign commits with your registered signing key. Keep commit messages focused on
  the change. Report vulnerabilities through [SECURITY.md](SECURITY.md).

## Cursor Cloud specific instructions

- The diagnostic bench is `pnpm web:dev` (Vite on port 5174). Key derivation
  runs locally in the browser. Restore also queries the light server; Create does too with an automatic or date-based birthday. Only Create
  with an explicit block height can run offline. Sync always needs the configured
  server. For regtest, start the matching loopback fixture separately.
- `pnpm test`, package builds, and the bench need both WASM engines from
  `pnpm build:sdk`. Keep `CARGO_BUILD_JOBS` at 1 so release compiles stay within
  this VM's memory. The environment start script enables an 8G `/swapfile`.
- CI uses Node 22.23.3. `/exec-daemon/node` on the base image is older; the
  environment install puts 22.23.3 first on `PATH`.
