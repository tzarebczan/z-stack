# Package builds and verification

The current preview is [`0.1.0-alpha.6`](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.6),
with NU7 support and downloadable archives. Alpha.1 and alpha.2 predate NU7. Packages are not published to npm or Cargo. The browser SDK includes
production WASM engines, so an app consuming built packages does not need Rust. See the
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
`sha256sum --quiet -c SHA256SUMS` inside `z-stack-preview/`, then scaffold and build an app
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

## NU7 acceptance

Use [Zakura 1.6.0](https://github.com/zakura-core/zakura/releases/tag/v1.6.0)
or a compatible later validator and a matching light server. The official
Zaino 0.10.1 image passed `ironwood_turnstile_shield_send` before NU7, but rejects
the published NU7 branch ID on this fixture; see the
[official release check](../infra/nu7/README.md#official-release-check).
Use the digest-pinned [NU7 container](../infra/nu7/README.md#download-the-container)
with `Z_STACK_NU7_ZAINO_IMAGE`, or build the adapted server and set `ZAINOD`.
Verify downloaded binaries against their release checksums. A local server is
optional for SDK integration and separate from the prebuilt wallet WASM.
After building/packing the SDK, choose a separate local chain directory and run
the following. For the container server, replace the `ZAINOD` line with
`Z_STACK_NU7_ZAINO_IMAGE` set to the documented verified digest:

```sh
export ZAKURAD=/path/to/zakurad
export ZAINOD=/path/to/zaino-nu7/target/debug/zainod
export Z_STACK_REGTEST_DIR=/path/to/disposable-nu7-chain
export Z_STACK_REGTEST_NU6_3=150
export Z_STACK_REGTEST_NU7=250
pnpm regtest:native:up
pnpm regtest:mine 300
pnpm test:funded-demos
pnpm regtest:native:down
```

The browser demos read both activation heights from the validator and configure
every WASM instance. Omitting `Z_STACK_REGTEST_NU7` leaves NU7 unscheduled in the
engine. Do not reuse a chain directory with another activation schedule;
`--fresh` deletes its chain data. Stop the owned fixture even if a test fails.

For additional native-engine acceptance, start a separate fresh fixture with the
same activation settings. Run these tests sequentially before manually mining
past the upgrade heights; the first exercises the prior branch, and the second
crosses into NU7 and spends shielded notes:

```sh
export Z_STACK_REGTEST_DIR=/path/to/disposable-native-nu7-chain
pnpm regtest:native:up
Z_STACK_REGTEST=1 cargo test -p z-engine --features native --test regtest \
  ironwood_turnstile_shield_send --release -- --ignored --nocapture
Z_STACK_REGTEST=1 cargo test -p z-engine --features native --test regtest \
  nu7_shielded_roundtrip --release -- --ignored --nocapture
pnpm regtest:native:down
```

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

## Assemble downloadable release assets

From a clean, signed, verified source commit, run the build/check commands above,
then pack all four packages and the preview bundle. The bundle uses the release version in its filename,
records the source revision in `PREVIEW.json`, and has a matching one-file `.sha256` sidecar. Create the full-set outer checksums in `artifacts/`:

```sh
pnpm pack:sdk
pnpm pack:base
pnpm bundle:preview
cd artifacts
sha256sum z-stack-core-0.1.0-alpha.6.tgz z-stack-passkey-0.1.0-alpha.6.tgz \
  z-stack-sdk-0.1.0-alpha.6.tgz z-stack-base-0.1.0-alpha.6.tgz \
  z-stack-preview-0.1.0-alpha.6.tgz > SHA256SUMS-alpha.6
sha256sum --quiet -c SHA256SUMS-alpha.6
# macOS: generate with shasum -a 256; verify with shasum -q -a 256 -c SHA256SUMS-alpha.6.
```

Use a directory with exactly one preview bundle for this version. Upload these
five archives, the preview `.sha256` sidecar and the full-set checksums to the prerelease tagged `v0.1.0-alpha.6`, targeting
the verified commit. Download them again and compare all hashes before publishing.
Scaffold an app from the downloaded bundle as the final acceptance check. Publishing
a GitHub prerelease does not publish to npm or Cargo. Keep source tags and signed
commits separate from checksums: neither authenticates a compromised host by itself.

## Build a custom preview

From a clean SDK source revision:

```sh
pnpm release:check
pnpm pack:sdk
pnpm pack:base
pnpm bundle:preview
```
