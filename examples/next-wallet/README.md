# Next.js wallet demo

A testnet wallet using Next.js 16.3.8, React 19.3 and the installed z-stack
archives. Create or restore, save the one-time phrase, copy a receive address,
sync, review and send shielded payments, track receipts, unlock/lock, reopen
and remove local data. Balance and activity stay visible
while sync runs. There is no account, cloud backup, payment service or analytics.
The Next CLI wrapper disables its telemetry during dev/build/start.

## Run

Use [the setup helper](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md), or follow the
[SDK build instructions](https://github.com/tzarebczan/z-stack/blob/main/README.md), then copy this directory
outside the SDK checkout. From your copy:

```sh
npm install /path/to/artifacts/z-stack-core-0.1.0-alpha.1.tgz \
  /path/to/artifacts/z-stack-passkey-0.1.0-alpha.1.tgz \
  /path/to/artifacts/z-stack-sdk-0.1.0-alpha.1.tgz
npm run build
npm run start
```

Open the localhost URL. `npm run dev` is also available. Never enter a mainnet
phrase. Install generates a local lockfile; it does not change the SDK checkout.
Use a fresh browser profile when switching between examples on the same origin.

## Framework boundary

`app/page.tsx` prerenders the shell. `lib/use-wallet.ts` calls `createWallet` in a
client effect, subscribes before actions and waits for `close()` before another
mount can open. Imports and server rendering perform no wallet work. A failed
close is observed; reopening still checks SDK ownership and loads durable state.
This follows Next’s [client component behavior](https://nextjs.org/docs/app/getting-started/server-and-client-components).

`next.config.mjs` uses the webpack compiler and emits `.wasm` as an asset. The SDK
owns integrity verification and instantiation. Webpack emits module workers and
matching WASM/manifests; no copied assets, source aliases or Vite plugin are used.
Build with `--webpack`, as the CLI scripts do. Turbopack is a separate unverified
integration. See [Next’s webpack configuration](https://nextjs.org/docs/app/api-reference/config/next-config-js/webpack).

Isolation headers use `same-origin` / `require-corp` to enable two-thread scanning.
Serve over HTTPS or localhost. Keep these headers, WASM MIME types, workers and
integrity files intact on your production host. `Z_STACK_ISOLATION=off` at build
time deliberately verifies single-thread fallback; it is not needed normally.
The app does not set a broad permissive CSP. Add your host’s CSP using the
[deployment guide](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#production-host), including Next’s
inline-script nonce requirements, and test the production build.

When replacing unpublished archives without changing their version, remove the
generated `.next` directory before rebuilding. Webpack can retain cached SDK
code from the prior archive. Always validate a fresh app for a release candidate.

Webpack reports circular chunk-hash warnings for the generated Rayon worker
bootstrap, which imports its WASM glue. The browser gate exercises the actual
thread pool; these warnings remain visible in build logs.

## Recovery and privacy

Creation uses the SDK's optional `beforeCommit` hook. The app displays the phrase
and waits for explicit acknowledgement before the SDK saves its first snapshot.
Cancelling or leaving before acknowledgement leaves no new saved wallet. After
acknowledgement the app clears the words and commits; reopening loads the saved
viewing data locked, even if teardown raced the storage completion notification.
The pending confirmation lives only in page memory and is never posted to Next.
App links and other wallet actions are blocked until confirmation. Unload warnings
are helpful but browsers can suppress them; safety comes from withholding the
commit, not from the warning. Pagehide hides the phrase and locks spending.
This demo asks the user to keep an offline copy; it does not verify that copy or
implement a production backup workflow. [The local passkey](https://github.com/tzarebczan/z-stack/blob/main/examples/local-passkey/README.md)
and [remote backup](https://github.com/tzarebczan/z-stack/blob/main/examples/remote-backup/README.md) examples cover optional encrypted vaults.

Phrase inputs are uncontrolled, are cleared before the first await and on
pagehide/unmount, and are never posted to Next. Reload opens viewing data locked.
Unlock checks the phrase against the saved wallet. Removing the local wallet
requires acknowledgement; it does not move funds or remove copies elsewhere.
Lock removes spending access and leaves saved viewing history readable.

`lib/connection.ts` configures ChainSafe’s public testnet gRPC-Web server.
A native gRPC endpoint is not usable by browser fetch; the endpoint must support
gRPC-Web and your app’s CORS origin. `NEXT_PUBLIC_ZSTACK_SERVER`
can select another public testnet endpoint at build time; never place credentials
in public environment variables. Opening the app contacts no chain server.
Create/restore check the chain tip; sync downloads compact blocks. Memo queries
stay on-demand and this demo does not request them. The chain provider can see
your IP and request timing. Payment review is frozen, fee/balance are checked again before proving, and a
five-minute review deadline is checked before broadcast. Receipts preserve
unknown submission outcomes; do not automatically send the payment again.
The app locks spending after every send attempt.

The setup button deliberately downloads and verifies engine assets and briefly
starts a worker. Reports contain fixed categories only and are never uploaded.

## Acceptance

`pnpm test:packages` installs fresh archives, builds this app outside the checkout,
starts its production server and checks its prerendered shell, headers and missing
asset behavior. `pnpm test:packages:browser` adds actual Chromium/Firefox/WebKit
startup and ST/MT flows. Its disposable empty-regtest fixture replaces only the
app-owned chain connection; the wallet, storage and Rust engines remain real.
It is not funded payment, physical-device, hosting-platform or Turbopack certification.
