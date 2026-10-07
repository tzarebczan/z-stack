# Package builds and verification

The current version is `0.1.0-alpha.1`. Packages are not published to npm or Cargo;
use source builds or matching local archives. The browser SDK includes production
WASM engines, so an app consuming built packages does not need Rust. See the
[walkthrough](GETTING-STARTED.md) and [support limits](SUPPORT.md).

## Build archives and a preview bundle

Install the toolchains listed in [CONTRIBUTING.md](../CONTRIBUTING.md), then run:

```sh
pnpm install --frozen-lockfile
pnpm build:sdk
pnpm release:check
pnpm pack:sdk
pnpm pack:base
pnpm bundle:preview
```

The Zcash package set is `@z-stack/core`, `@z-stack/passkey` and `@z-stack/sdk`.
Optional `@z-stack/base` has its own archive and runtime license inventory; a
Zcash-only app does not install it. Keep all four package versions, the workspace
and `SDK_VERSION` aligned. Pin exact alpha versions in consuming apps.

Archives contain built JavaScript, declarations, source maps and retained legal
notices. The SDK also includes both production WASM engines, workers and integrity
manifests. `workspace:*` dependencies become exact versions when packed. Internal
experiments and the repository-only `@z-stack/source` condition are excluded.
Stub engines are rejected by the packing checks.

The preview bundle adds runnable templates, the setup helper, offline API docs,
license provenance, `PREVIEW.json` and an internal `SHA256SUMS` inventory. Build it
from a clean committed revision. `--allow-dirty` produces a marked local rehearsal,
which must not be treated as a release artifact. After unpacking a bundle, run
`sha256sum -c SHA256SUMS` inside `z-stack-preview/`, then scaffold and build an app
outside the SDK checkout. None of these commands publishes to a registry.

## Local checks

`pnpm release:check` verifies licenses, documentation links, patch provenance,
types, generated API docs, regressions and external archive consumers. Consumers
build the examples, check server-safe imports and render React under browser API
traps. Optional Base and combined examples have separate archive checks.

For Rust or engine changes, also run:

```sh
cargo fmt --all -- --check
cargo test -p z-engine --features native --lib
cargo check -p z-wasm --target wasm32-unknown-unknown
```

Rebuild both WASM variants after Rust changes. TypeScript-only changes can reuse
existing engines with `pnpm build:packages`. Keep dependency updates deliberate;
do not substitute a newer crypto engine during routine packaging.

## Browser acceptance

Install Playwright Chromium, Firefox and WebKit, then run:

```sh
pnpm test:packages:browser
pnpm test:base:browser
```

These checks use fresh archive consumers, real single-threaded/threaded engines,
disposable regtest keys and controlled chain/provider fixtures. They cover worker
startup, storage, create/restore/sync/reload, recovery and lifecycle. They are not
funded mainnet, physical mobile, physical passkey or hardware-wallet certification.

## Funded regtest acceptance

Use the isolated loopback fixture, never a production seed:

```sh
pnpm regtest:up
pnpm test:regtest
node scripts/test-packages.mjs --funded-regtest
pnpm test:funded-demos
pnpm regtest:down
```

Stop the fixture even if a test fails. Do not run competing funded tests against
the same faucet. `test:funded-demos` works with either the compose or native
fixture and reads its activation height from the chain. Those fixtures have
different activation schedules;
match each client's `Z_STACK_REGTEST_NU6_3` to the fixture. Override
`Z_STACK_ZEBRA_RPC` and `Z_STACK_REGTEST_LWD` only for another deliberately isolated
local fixture. See [node tooling](NODE.md).

The browser fixture uses compact transparent scanning and a loopback validator
submission fallback. Keep that fallback out of production gateways. Combined
Zcash/Base acceptance additionally needs an isolated Anvil EVM; its mock token
and fee oracle do not validate Circle's contract, live OP Stack fees, bundlers or
paymasters. See [Base testing](BASE-WALLET.md).

Before distributing a candidate, test a separate application with its exact
source revision or package archives pinned. Record engine hashes, actual
checks and limitations in the change review. Compiling with typed WASM stubs is
not an engine runtime test. Passing checks is not an independent security audit.

## Version changes

Alpha contract changes increment the alpha number and belong in the
[changelog](../CHANGELOG.md), with migration guidance for integrators. Before 1.0,
breaking supported-API changes increment the minor version. `/engine` and `/lab`
remain experimental and may change between previews. Registry publication is a
separate distribution step; Cargo consumers must also account for the workspace
patch boundary described in [UPSTREAM.md](UPSTREAM.md).
