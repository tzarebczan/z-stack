# Build your first browser wallet

Start with a working app, then replace its screens one step at a time. The wallet
engine runs locally. Accounts, cloud backups and payment integrations are optional
application services; they are not needed for this walkthrough.

**Spending limitation:** the published preview verifies empty-wallet setup and sync.
It does not currently provide a supported funded public-testnet walkthrough.
The faucet and wallet server must agree on the chain after block **4,465,026**
before you request test coins. See [funded testing](#funded-testing) for the fork
check and the separate source-checkout regtest path.

## 1. Get matching archives

The published **alpha.4** preview includes NU7 support. Use Node 22.18+ and npm;
no Rust toolchain or npm account is needed for these built archives. The SDK
archive includes compiled single-threaded and threaded WASM engines, bindings,
workers, integrity manifests, and bundled core and passkey helpers. Building
the engine yourself is optional.
Download and verify both checksum layers. `--quiet` suppresses successful
per-file lines (including the offline API files); a zero exit status means the
check passed. Mismatches still print an error:

```sh
gh release download v0.1.0-alpha.4 --repo tzarebczan/z-stack --dir sdk-alpha
cd sdk-alpha
sha256sum --quiet -c SHA256SUMS-alpha.4 # macOS: shasum -q -a 256 -c SHA256SUMS-alpha.4
tar -xzf z-stack-preview-0.1.0-alpha.4-*.tgz
cd z-stack-preview
sha256sum --quiet -c SHA256SUMS # macOS: shasum -q -a 256 -c SHA256SUMS
```

Continue from **inside the extracted bundle**, using its scripts and matching
archives. Do not mix its packages with the current source scaffolder. Checksums
detect changed bytes; authenticate the GitHub release and source revision separately.
For a source build, follow [the build instructions](../README.md#build-from-source)
through `pnpm pack:sdk`, then use that checkout’s scaffolder and archives.

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
isolation the SDK uses its single-thread engine.
Default browser startup downloads roughly **30 MB of uncompressed WASM**:
about 10.9 MB for key/UI bindings and the single-thread fallback, plus 19.2 MB
for the threaded scanner. Transfers depend on host compression and cache headers.
These are two engine variants. Each initialization reuses its integrity-verified
bytes; workers can request the same cached asset. The Vite demo
shows `wallet.runtime`; its final scanner mode is available after loading a wallet
or syncing. Set `VITE_ZSTACK_MULTICORE=false` before startup to use only the
single-thread engine, reducing download size at the cost of parallel scanning.
Applications can use the SDK's `preferMulticore: false` option. See
[engine loading](INTEGRATION.md#engine-loading). See [the browser matrix](SUPPORT.md)
for tested limits; an emulated mobile viewport is not a physical-device test.

## 4. Create or restore, then sync

Create displays a one-time phrase and waits for acknowledgement before its
first durable save. Save it privately. In the Vite demo, a blank **Birthday
height** uses the light server's latest height minus 100, so Create needs a
working server. Enter an explicit positive height to create offline; Sync still
needs the server. A new wallet's birthday must precede its first deposit. The
Next demo uses the automatic birthday.

Restore needs a height or date before the first deposit. Invalid words have the
`invalid_recovery_phrase` code and fixed display copy. A refused restore retains
the input; a successful restore clears it. Restore is hidden when a saved wallet
exists. **Remove local wallet** has its own backup confirmation; it deletes
this browser's viewing data and local vault, not on-chain funds. Save any payment
receipts too. The Vite demo blocks removal until pending payments confirm or
expire, including after a reload or another tab’s send. It uses
`wallet.forget({ passkey: true, pending: "reject" })`; the SDK policy is optional
for other applications. Reopening loads viewing data with spending locked. No phrase is
posted to Next or a backend.

## Funded testing

The preview bundle includes the scaffolder and built packages, not the native
fixture or funded test runner. **There is no supported funded receive/send path
using only the published preview until the faucet and wallet's gRPC-Web server
are verified to agree past height 4,465,026.** Empty sync is not spending evidence.
For reproducible funded acceptance now, use a source checkout and the isolated
regtest recipe below. Local source test tools are separate from installing the
prebuilt SDK in your application.

**Before requesting test funds:** the public funding path is not certified.
On 2026-10-08, ChainSafe and Zexplorer agree at height **4,465,025**, then report
different hashes at NU7 activation, **4,465,026**. ChainSafe reports
`000089ba27100beede16d64b34e3d1b626b428cb7ee9fe6dcfdc217ce24e78af`;
Zexplorer reports `0713a6429dc1cef50224668082022ca8881593e09a3170717e6a725491c03b96`.
An explorer link from a faucet does not establish which chain its payment uses.
Compare a block hash at the same post-activation height on your wallet server and
the faucet's node before requesting TAZ. If you cannot establish agreement, stop
here for public funding; use the isolated regtest steps below for receive/send checks.
An empty balance on another chain is not evidence that a deposit was lost.

Once the faucet and wallet server agree, copy the receive address and request
TAZ (testnet coins). `connection.ts` must select a gRPC-Web endpoint with CORS
for your origin. ChainSafe's root website redirects to `testnet.zec.rocks`, but
its gRPC-Web RPC paths still respond. Do not change the configured URL based on
that website redirect: native `application/grpc` is not browser gRPC-Web.
Sync reports the configured server's view; it cannot prove a faucet payment
exists on that chain. Balance and activity stay visible during updates.

The engine supports the pinned NU7 testnet schedule at block **4,465,026**.
Mainnet's NU7 height is not set upstream. Updating the engine does not reconcile
providers following different chains. Empty-wallet sync does not verify spending;
use the [NU7 regtest recipe](RELEASE.md#nu7-acceptance) for a funded walkthrough.

To investigate a missing payment:

1. Record its receipt’s transaction ID and mined height. Querying a public explorer
   can link that transaction to your IP; do not submit your phrase or viewing key.
2. Compare the hash of a common block on the faucet’s explorer and your light
   server. Heights alone do not identify the same chain. From a built source
   checkout, `node scripts/check-testnet-chain.mjs --height 4465026` compares public
   block data without wallet queries. A different hash needs an aligned server or
   funding source; changing the birthday cannot repair it.
3. If the receipt is on your server’s chain but below **Wallet birthday**, choose
   **Scan an earlier range**. This keeps the wallet and receiving address, locks
   spending in the demo and rebuilds balance/history. Pending outgoing payments
   must confirm or expire first. A reset is saved before the new scan starts;
   cancelling sync leaves progress you can resume.
4. The default deep-scan limit is 150,000 blocks. The SDK rejects a larger reset
   before changing state unless the client permits deep syncing (`deepSync`; local pipe clients permit it by default).
   Dates estimate a height with a safety margin; use the receipt’s exact block
   height or an earlier one when available.

For repeatable funding without a public faucet, run a native loopback validator
(no Docker required):

```sh
pnpm regtest:native:up
pnpm test:funded-demos
pnpm regtest:native:down
```

These commands are not included in the preview bundle. Run them from the source
checkout after building/packing the SDK. The test
scaffolds both demos, funds disposable wallets, proves shielded payments and
checks confirmation and reload. See [native fixture prerequisites](NODE.md)
and [verification limits](SUPPORT.md). It is regtest evidence, not public-testnet certification.
Public chain providers can see IP addresses and request timing; memo retrieval
remains on-demand. Never use production funds in this walkthrough.

A loopback regtest fixture is used by our funded acceptance tests. Its public
fixture phrases and tiny activation heights are for that isolated chain only.
Do not change a running wallet's network or reuse its storage with another chain.
Use a separate origin/profile or [your own namespace](STORAGE.md).

## 5. Review and send

Continue only after the chain/funding check above, or in the isolated regtest
fixture. The public preview does not certify this step.

Enter a shielded recipient address, a positive test-coin amount and an optional memo.
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

This optional step is for SDK maintainers preparing a custom bundle. Skip it
when using the published archives. From a clean SDK source revision:

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
