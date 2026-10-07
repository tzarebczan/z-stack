# Build your first browser wallet

Start with a working app, then replace its screens one step at a time. The wallet
engine runs locally. Accounts, cloud backups and payment integrations are optional
application services; they are not needed for this walkthrough.

## 1. Get matching archives

From a checkout, install the prerequisites in [the README](../README.md)
and run:

```sh
pnpm install --frozen-lockfile
pnpm build:sdk
pnpm pack:sdk
```

This builds the real Rust single-thread and multi-thread engines and all three
TypeScript packages. It produces matching `core`, `passkey` and `sdk` archives in
`artifacts/`. It does not publish anything. If you received a built preview bundle,
unpack it and start at step 2; no Rust toolchain or npm scope is needed.

## 2. Generate an app outside the checkout

Use Node 22.18+ and npm. Choose Vite/TypeScript or Next.js:

```sh
node scripts/create-example.mjs browser-wallet /path/to/my-wallet --install
# Or:
node scripts/create-example.mjs next-wallet /path/to/my-next-wallet --install
```

The helper verifies archive versions, license files and both WASM integrity
hashes, copies the archives into your app's `vendor/`, and adds local file
dependencies. It refuses to overwrite a nonempty directory. `--install` runs npm
with lifecycle scripts disabled. Omit it to inspect the app first, then run
`npm install --ignore-scripts` yourself. `--archives /path/to/artifacts` selects
another directory containing the current matching version. No auth token is used.

Other templates are `react-wallet`, `local-passkey` and `remote-backup`. The last
one includes an example server for optional encrypted storage; it is not required
by the base wallet. [Examples](../examples/README.md) explains their boundaries.

## 3. Open the demo and check its host

```sh
cd /path/to/my-wallet
npm run dev
```

Open the localhost URL. Both wallet demos default to testnet. Never enter a
mainnet recovery phrase in an example. The Next demo's setup checks deliberately
fetch and verify its engine assets and test a worker; they do not upload a report.

For production-shaped verification, use `npm run build`, then `npm run preview`
(Vite) or `npm run start` (Next). HTTPS or localhost is required. Your host must
serve WASM, workers and integrity manifests without SPA fallback, and preserve
[the isolation/security headers](INTEGRATION.md#production-host). Without cross-origin
isolation the SDK uses its single-thread engine. See [the browser matrix](SUPPORT.md)
for tested limits; an emulated mobile viewport is not a physical-device test.

## 4. Create or restore, then sync

Create displays a one-time phrase and waits for your acknowledgement before the
first durable wallet save. Save it privately. Restore clears the input before
awaiting the SDK and uses a birthday from before your first deposit. Reopening
loads viewing data with spending locked. No phrase is posted to Next or a backend.

Copy the receive address and obtain **testnet ZEC** using your chosen testnet
funding source. Sync explicitly to recover the deposit and activity. Balance and
activity remain visible while updates run. The endpoint in the app-owned
`connection.ts` must support gRPC-Web and the host's CORS origin. Public chain
providers can see IP addresses and request timing; memo lookup remains on-demand.

A loopback regtest fixture is used by our funded acceptance tests. Its public
fixture phrases and tiny activation heights are for that isolated chain only.
Do not change a running wallet's network or reuse its storage with another chain.
Use a separate origin/profile or [your own namespace](STORAGE.md).

## 5. Review and send

Enter a shielded recipient address, a positive ZEC amount and an optional memo.
Review the exact recipient, amount, memo and estimated fee. The demo freezes this
review, refreshes the balance and fee before proving, and expires review after
five minutes. Fee estimates are not a cryptographically binding fee cap.

Enter the phrase for this payment; it is cleared from the input before any
await. The app unlocks for one spend, proves locally, saves the pending transaction
before submission, and shows its transaction ID. Cancel before submission prevents
a later broadcast; an already-running proof may finish before cleanup completes.
After submission, cancellation is no longer offered. Sync to see confirmation.

A lost submission acknowledgement is **unknown**, not rejection. The receipt
keeps its transaction ID and directs the user to check activity. Do not silently
resend the payment. A node's explicit rejection is a separate SDK error. The SDK
preserves pending reservations and reconciles them against later chain data.

## 6. Replace the example UI

Keep one client owner per page and await `close()` before opening another.
Subscribe before actions; unsubscribe during teardown. Leave cryptography in
Rust. Keep inputs, recovery confirmation, network choice and receipts in your
application. The examples' `send.ts` is app code, not a required SDK UI abstraction.

Use [the API guide](SDK.md), [framework integration](INTEGRATION.md),
[storage contracts](STORAGE.md) and [optional services](SERVICES.md) while replacing
one concern at a time. Applications can add an iframe API around their wallet;
that API and any account or fiat services stay outside z-stack.

## 7. Prepare a reviewable preview

From a clean SDK source revision:

```sh
pnpm release:check
pnpm pack:sdk
pnpm pack:base
pnpm bundle:preview
```

The versioned bundle contains the four archives (including optional Base), all example templates, this
walkthrough, an offline API reference, license provenance and `SHA256SUMS`.
`PREVIEW.json` records the source commit and archive hashes. The bundle helper
requires the system `tar` command. `--allow-dirty` is for a local rehearsal and
marks the artifact dirty; it is not a release candidate. Building a bundle does
not publish it or alter repository visibility.

## Optional Base account

To add a Base wallet without a second recovery secret, use the separate
[`@z-stack/base` integration](BASE-WALLET.md). It has its own archive and
Base-only demo. Pure Zcash integrations do not depend on it.
