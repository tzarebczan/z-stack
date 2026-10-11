# Contributing to z-stack

Use Node.js 22.18+, pnpm 12.6.0, Rust 1.91, wasm-pack 0.15.0, and
`nightly-2026-09-22` with `rust-src` for threaded WASM. See the
[README setup](README.md#build-from-source). All required source patches
are checked in; no separate fork checkout is needed.

## Build and test

```sh
pnpm install --frozen-lockfile
pnpm check:patches
cargo fmt --all -- --check
cargo test -p z-engine --features native --lib
cargo check -p z-wasm --target wasm32-unknown-unknown
pnpm build:sdk
pnpm release:check
```

After TypeScript-only changes, `pnpm build:packages` reuses existing WASM.
After any Rust change, rebuild both WASM variants before running SDK or consumer
tests. CI builds real production artifacts and verifies packaged consumers.
Use `--locked` for dependency checks where possible; do not update crypto pins
as a side effect of routine cleanup.

`packages/web` is a diagnostic app. It resolves source through the
`@z-stack/source` export condition. Consumers resolve `dist/`; test both.
The [browser example](examples/browser-wallet/README.md) uses the installed
distribution exports, without aliases or a source condition.
`pnpm web:dev` opens the local regtest bench, which expects the loopback fixture
and optional native bridge. Start with the installed browser example for a public
testnet walkthrough. A missing local service is a setup condition, not a testnet
wallet failure.

## CI verification

CI builds both production WASM engines and packs one SDK/core/passkey/Base set.
The installed-consumer checks and three parallel browser jobs use those same
archives. Each consumer gets a fresh app and dependency installation; workspace
links are never substituted for the packaged SDK. Browser jobs verify the
archive checksums before installing. The remote-backup virtual-authenticator
scenario runs in Chromium, where Playwright exposes the required CDP interface.

The SDK job compiles the production examples and their browser fixtures once.
Browser jobs install fresh archive consumers, verify their inputs and compiled
file checksums, and reuse those same-run builds. Missing or changed builds fail;
they never trigger an unnoticed rebuild. Local checks compile normally unless
the CI-only `Z_STACK_CONSUMER_BUILD_MODE` and `Z_STACK_CONSUMER_BUILDS` are set.

Production ST/MT engines can be reused when all Rust inputs, both compiler builds
and wasm-pack match exactly. CI verifies the source fingerprint, module bytes
and generated bindings before package builds, then runs the full SDK and browser
tests. Only `main` runs passing every required job write engine caches. A Rust or compiler
change requires a rebuild. Native dependency caches include the hash of
`.github/ci/native-profile.toml`, so profile changes create a fresh cache. The
first successful `main` run fills new cache entries for subsequent runs.

The native job summary records each Cargo command's elapsed time, CPU time
and exit status, alongside the exact cache hit and runner CPU/memory capacity.
Its `native-timings` artifact contains Cargo's per-crate compilation reports.
Use both to distinguish compilation from test execution; CPU seconds and
elapsed seconds are different measurements. Timing does not change profiles,
features or which checks run.

Native CI also uses a bounded, content-based compiler cache for patched path
dependencies and their downstream crates. Cargo still checks inputs, builds
each feature configuration and runs the tests. Compiler statistics show hits,
misses and non-cacheable work separately. PRs only read saved caches; successful
native runs on `main` publish new snapshots. Source or CI changes select a new
snapshot and may reuse matching compiler entries from the preceding snapshot.

To reuse an already-built matching archive set locally:

```sh
pnpm build:sdk
pnpm pack:sdk
pnpm pack:base
node scripts/test-packages.mjs --archives=artifacts
node scripts/test-packages.mjs --browser --browsers=firefox --archives=artifacts
node scripts/test-base-package.mjs --browser --browsers=firefox --archives=artifacts
node scripts/test-combined-package.mjs --archives=artifacts
```

Without `--archives=DIR`, each check packs its own set as before. An incomplete
or mismatched-version set fails instead of rebuilding silently.

PRs confined to maintained public Markdown run documentation checks without
Rust or browser builds. The executable guides in `docs/SDK.md` and
`docs/SERVICES.md` always select the full route so their marked examples compile
against installed packages. Code, dependencies, workflow files, unknown paths and
code-to-docs renames also select the full route. Pushes to `main` always run the full
suite and can populate the Rust caches; PRs only restore them. The required
`rust` check runs on either route and rejects failures, cancellations and
unexpected skips.

## Code boundaries

- Cryptography, signature checks, and transaction construction belong in Rust.
- TypeScript owns transport, worker orchestration, storage, events, and UI helpers.
- `@z-stack/passkey` owns WebAuthn/PRF and remains free of runtime dependencies.
- Root SDK exports are the integration surface. Put diagnostics in `/lab`.
- Keep the native wallet bridge bound to loopback. Do not expose it through a proxy.
- Preserve committed wallet data on cancellation, quota failures, and tab conflicts.

Add a regression test when changing wallet persistence, authorization, spending,
or concurrency. Do not weaken an assertion to silence a failure. Use regtest
funds for transaction tests, and keep fixtures isolated from existing wallets.

## Dependency changes

Extend `z-engine` before patching upstream. If a patch is necessary, record its
purpose, exact baseline, license, feature scope, tests, and removal criteria in
[UPSTREAM.md](docs/UPSTREAM.md). The checked-in patch and manifest must reproduce
every vendored file. General fixes should be proposed upstream through a
separately reviewed contribution.

## Changes and releases

Contributions to original z-stack code are accepted under Apache-2.0. Keep the
existing licenses and copyright notices on third-party code; patching a vendored
dependency does not relicense it. Review [licensing](docs/LICENSING.md) and
regenerate the browser inventory when dependencies change.

Describe the user-visible behavior, package/API impact, and checks run. For a
breaking change, update the example and wallet consumer in the same release.
Keep SDK, core, passkey, Base, the workspace, and `SDK_VERSION` versions in sync. Before distributing
archives, run [the release checks](docs/RELEASE.md) from a clean checkout.

Never commit a seed, wallet database, cookie, credential, `.env`, or private
test receipt. Use [private reporting](SECURITY.md) for vulnerabilities.
