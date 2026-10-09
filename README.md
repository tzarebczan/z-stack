# z-stack

A Zcash wallet engine and TypeScript SDK for web apps. Key derivation, scanning,
transaction construction, and proofs run locally in Rust/WebAssembly. Apps own
their UI, authentication, and backup service.

**Alpha.** The packages are not published to npm. Original code is Apache-2.0;
see [licensing](docs/LICENSING.md) and
[release status](docs/RELEASE.md).
The [alpha.6 preview](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.6)
includes NU7 support and prebuilt browser SDK archives, with both single-threaded
and threaded WASM engines. No Rust toolchain is needed to use these archives.
Alpha.1 and alpha.2 predate NU7.

## Start here

| You want to… | Read |
| --- | --- |
| Build a first working app | [Step-by-step walkthrough](docs/GETTING-STARTED.md) |
| Add a browser wallet | [Integration guide](docs/INTEGRATION.md) |
| Run a complete example | [Vite + TypeScript example](examples/browser-wallet/README.md) |
| Integrate React lifecycle | [React example](examples/react-wallet/README.md) |
| Try a Next.js production app | [Next.js wallet demo](examples/next-wallet/README.md) |
| Add passkeys without a backend | [Local passkey example](examples/local-passkey/README.md) |
| Add your own remote backup service | [Remote backup example](examples/remote-backup/README.md) |
| Diagnose hosting and test custom adapters | [Setup checks](docs/DIAGNOSTICS.md), [adapter acceptance](docs/ADAPTERS.md) |
| Handle errors and retries | [Error handling](docs/ERRORS.md) |
| Check browser/framework coverage | [Support matrix](docs/SUPPORT.md) |
| Understand wallet methods and events | [Wallet API](docs/SDK.md) |
| Choose privacy and backup settings | [Security and privacy](docs/SECURITY.md) |
| Build or change the engine | [Contributing](CONTRIBUTING.md) |
| Review dependencies and patches | [Upstream dependencies](docs/UPSTREAM.md) |

## Run the published preview

The current prebuilt preview is **alpha.6**, with NU7 support. Public-testnet
funded receive/send remains unverified; current faucet attempts did not fund a wallet. Check
[the funding limitation](docs/GETTING-STARTED.md#funded-testing)
before requesting test coins. Node 22.18+ is required.

Use the prebuilt WASM to avoid compiling the wallet engine. The SDK archive in
the bundle contains both WASM engines, their bindings, workers, integrity
manifests, and bundled core and passkey helpers. Download the bundle, verify
both checksum layers, then generate an app:

```sh
gh release download v0.1.0-alpha.6 --repo tzarebczan/z-stack --dir sdk-alpha \
  --pattern 'z-stack-preview-0.1.0-alpha.6.tgz*'
cd sdk-alpha
sha256sum --quiet -c z-stack-preview-0.1.0-alpha.6.tgz.sha256
tar -xzf z-stack-preview-0.1.0-alpha.6.tgz
cd z-stack-preview
sha256sum --quiet -c SHA256SUMS
node scripts/create-example.mjs browser-wallet ../my-wallet --install
cd ../my-wallet
npm run dev
```

On macOS, use `shasum -q -a 256 -c z-stack-preview-0.1.0-alpha.6.tgz.sha256` for the outer check and
`shasum -q -a 256 -c SHA256SUMS` inside the bundle. The preview checksum covers the one bundle; the separate `SHA256SUMS-alpha.6`
asset covers all five archives for a full download. Use the scripts **inside that
bundle**; a scaffolder from a different source version expects different archives.
See [the walkthrough](docs/GETTING-STARTED.md) for Next.js and funding limitations.

Without GitHub CLI, use the [release downloads](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.6)
or download the two files with curl before the same checksum/extraction steps:

```sh
mkdir sdk-alpha && cd sdk-alpha
curl -fLO https://github.com/tzarebczan/z-stack/releases/download/v0.1.0-alpha.6/z-stack-preview-0.1.0-alpha.6.tgz
curl -fLO https://github.com/tzarebczan/z-stack/releases/download/v0.1.0-alpha.6/z-stack-preview-0.1.0-alpha.6.tgz.sha256
```

## Build from source

Building the engine is optional. Use this path when changing Rust code or
producing your own WASM builds; otherwise use the published preview above.

Install Node.js 22.18+, pnpm 12.6.0, Rust 1.91, wasm-pack 0.15.0 and `tar`.
The checked-in Rust toolchain file installs the stable WASM target. The threaded
build also uses a pinned nightly toolchain:

```sh
git clone https://github.com/tzarebczan/z-stack.git
cd z-stack
corepack enable
pnpm install --frozen-lockfile
cargo install wasm-pack --version 0.15.0 --locked
rustup toolchain install nightly-2026-09-22 --profile minimal --component rust-src
rustup target add wasm32-unknown-unknown --toolchain nightly-2026-09-22
pnpm build:sdk
pnpm pack:sdk
node scripts/create-example.mjs browser-wallet ../my-wallet --install
cd ../my-wallet
npm run dev
```

`artifacts/` contains matching archives. The SDK archive bundles core and passkey
helpers, both WASM engines and their workers. For a wallet app, install it alone:

```sh
npm install /path/to/artifacts/z-stack-sdk-0.1.0-alpha.6.tgz
```

Configure Vite once:

```ts
import { defineConfig } from "vite";
import { zStack } from "@z-stack/sdk/vite";

export default defineConfig({ plugins: [zStack()] });
```

Then initialize and open the wallet in browser code:

```ts
import { createWallet } from "@z-stack/sdk";

const wallet = await createWallet({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev",
  memoFetch: "on-demand",
  autoSync: false,
  autoShield: false,
});
const saved = await wallet.load();
// If saved is null, let the user choose Create or Restore.
// Set up event handlers before starting sync; await wallet.close() on teardown.
```

Production hosting, backup handling, and the complete lifecycle are covered in
the [integration guide](docs/INTEGRATION.md). Run browser code on HTTPS or
localhost. A Vite development build alone does not configure production headers.

## Packages and engine

| Component | Responsibility |
| --- | --- |
| [`@z-stack/sdk`](packages/sdk/README.md) | Browser wallet, events, transport, native bridge client, Vite plugin |
| [`@z-stack/core`](packages/core/README.md) | Platform-independent amounts, addresses, history, errors |
| [`@z-stack/passkey`](packages/passkey/README.md) | WebAuthn PRF vaults; no runtime dependencies |
| `z-engine` | Zakura-backed wallet engine; native SQLite and browser snapshot stores |
| `z-wasm` | WASM bindings; single-thread and threaded builds |
| `z-desktop`, `z-node-launcher` | Native app and node tooling; separate from browser integration |

Import wallet APIs from `@z-stack/sdk`. `/vite` configures Vite; `/lab` is an
unstable diagnostic surface. Swap adapters and keytool utilities are internal
experiments and are excluded from the distribution bundle. Fiat services,
account login, cloud backups, and UI components are application concerns.

The engine uses pinned published Zakura packages. Three field crates carry
wasm32 optimizations under `vendor/zakura/`, with retained licenses, published
archive checksums, and standalone patches. No private submodule is needed.
[The dependency ledger](docs/UPSTREAM.md) explains Rust package aliases,
feature gates, downstream Cargo patches, and the upstreaming workflow.

## Verify

```sh
pnpm release:check
cargo test -p z-engine --features native --lib
cargo check -p z-wasm --target wasm32-unknown-unknown
```

The package checks install the actual archives in a temporary app, typecheck
the example, and build its worker/WASM assets. See [release checks](docs/RELEASE.md)
for browser and funded regtest verification. Never use a production seed in an
example or test fixture.

Further reading: [documentation index](docs/README.md),
[architecture](docs/ARCHITECTURE.md), [contribution guide](CONTRIBUTING.md),
[security reporting](SECURITY.md), [third-party notices](NOTICE).

Optional registration, authentication and encrypted backup adapters are available
through `@z-stack/sdk/services`. See [accounts and backups](docs/SERVICES.md) for the boundary
between the local wallet and your server. Your application configures account
and backup providers.

## Optional Base wallet

[`@z-stack/base`](packages/base/README.md) adds a separate Base ETH/USDC wallet
and explicit paymaster primitives. It can reuse the recovery phrase without
requiring the Zcash engine or an account service. Follow the [Base integration
guide](docs/BASE-WALLET.md) or run the [Base-only demo](examples/base-wallet/README.md).
