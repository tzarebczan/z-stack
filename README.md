# z-stack

A Zcash wallet engine and TypeScript SDK for web apps. Key derivation, scanning,
transaction construction, and proofs run locally in Rust/WebAssembly. Apps own
their UI, authentication, and backup service.

**Alpha.** The packages are not published to npm. Original code is Apache-2.0;
see [licensing](docs/LICENSING.md) and
[release status](docs/RELEASE.md).
Download matching [alpha archives](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.2)
and run an example without a Rust toolchain.

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

## Run a prebuilt example

Download the preview bundle and checksums from the
[alpha release](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.2).
Or use GitHub CLI to download the complete matching set:

```sh
gh release download v0.1.0-alpha.2 --repo tzarebczan/z-stack --dir sdk-alpha
cd sdk-alpha
sha256sum -c SHA256SUMS-alpha.2 # macOS: shasum -a 256 -c SHA256SUMS-alpha.2
tar -xzf z-stack-preview-0.1.0-alpha.2-*.tgz
cd z-stack-preview
sha256sum -c SHA256SUMS # macOS: shasum -a 256 -c SHA256SUMS
node scripts/create-example.mjs next-wallet ../my-wallet --install
cd ../my-wallet
npm run dev
```

Requires Node.js 22.18+ and npm. Choose `browser-wallet` for Vite/TypeScript.
Use a new or empty app directory. Both demos use testnet, label test coins TAZ,
and display the server, wallet birthday and scanned height. See the
[funding and chain-view checks](docs/GETTING-STARTED.md#4-create-or-restore-then-sync)
before requesting a faucet payment. The published alpha is not an npm release.

## Build from source

Install Node.js 22.18+, pnpm 12.6.0, Rust 1.91, and wasm-pack 0.15.0.
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
```

`artifacts/` contains matching SDK, core, and passkey archives. Install all three
in your app; the SDK archive includes both WASM engines and their workers:

```sh
npm install /path/to/artifacts/z-stack-core-0.1.0-alpha.2.tgz \
  /path/to/artifacts/z-stack-passkey-0.1.0-alpha.2.tgz \
  /path/to/artifacts/z-stack-sdk-0.1.0-alpha.2.tgz
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
