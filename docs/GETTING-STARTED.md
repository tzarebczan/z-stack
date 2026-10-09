# Build your first browser wallet

Start with a working app, then replace its screens one step at a time. The wallet
engine runs locally. Accounts, cloud backups and payment integrations are optional
application services; they are not needed for this walkthrough.

The preview supports NU7 testnet create, restore and sync. Payment review is
verified on the isolated regtest fixture.
An external alpha.6 run reported a Valar-funded public receive; outgoing public
spending remains unverified. [Funded testing](#funded-testing) explains the current faucet results and the reproducible regtest alternative.

## 1. Get matching archives

The published **alpha.7** preview includes NU7 support. Use Node 22.18+ and npm;
no Rust toolchain or npm account is needed for these built archives. The SDK
archive includes compiled single-threaded and threaded WASM engines, bindings,
workers, integrity manifests, and bundled core and passkey helpers. Building
the engine yourself is optional.
Download and verify both checksum layers. `--quiet` suppresses successful
per-file lines (including the offline API files); a zero exit status means the
check passed. Mismatches still print an error:

```sh
gh release download v0.1.0-alpha.7 --repo tzarebczan/z-stack --dir sdk-alpha \
  --pattern 'z-stack-preview-0.1.0-alpha.7.tgz*'
cd sdk-alpha
sha256sum --quiet -c z-stack-preview-0.1.0-alpha.7.tgz.sha256 # macOS: shasum -q -a 256 -c z-stack-preview-0.1.0-alpha.7.tgz.sha256
tar -xzf z-stack-preview-0.1.0-alpha.7.tgz
cd z-stack-preview
sha256sum --quiet -c SHA256SUMS # macOS: shasum -q -a 256 -c SHA256SUMS
```

The bundle's `.sha256` file checks only the preview download. The separate
`SHA256SUMS-alpha.7` asset covers all five archives if you download the full set.

Without GitHub CLI, open the [release downloads](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.7),
choose `z-stack-preview-0.1.0-alpha.7.tgz` and its `.sha256` file, and verify both checksum layers as above.
The [README](../README.md#run-the-published-preview) also provides curl commands.

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
for the threaded scanner (approximately **4.3 MB + 4.7 MB with gzip** in the preview build). Transfers depend on host compression and cache headers.
These are two engine variants. Each initialization reuses its integrity-verified
bytes; workers can request the same cached asset. The Vite demo
shows engine download/startup progress and subscribes to `wallet.on("runtime")`
so scanner readiness updates without a click or polling. Set `VITE_ZSTACK_MULTICORE=false` before startup to use only the
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

### Public testnet status

SDK NU7 support is implemented; public testnet activated at block **4,465,026**.
An external alpha.6 integration reported a **0.125 TAZ receive** from the Valar
faucet at block **4,482,837** through ChainSafe. Its transaction ID was
`bf822338ef93a10be7494b465d541010613f40a5b2452ebb3a5b0548f9d62a03`.
The selected light server also returned that transaction during our follow-up.
The integrator discarded the recovery phrase, so this did not exercise an outgoing
public payment. Public proving, broadcast and mined send confirmation remain
unverified; this is not an established SDK send failure.

Checked on **2026-10-09**: ChainSafe and [Zecblock](https://testnet.zecblock.com/block/4465026)
report activation hash `000089ba27100beede16d64b34e3d1b626b428cb7ee9fe6dcfdc217ce24e78af`;
[Zexplorer](https://zexplorer.app/) reports `0713a6429dc1cef50224668082022ca8881593e09a3170717e6a725491c03b96`.
Provider disagreement alone does not prove that a faucet payment cannot arrive.
A faucet's explorer link also does not identify its underlying node's chain.

Use the [Valar testnet faucet](https://faucet.testnet.valargroup.dev/). It sends
from a shielded wallet to Zakura nodes and provides a transaction receipt. Check
its availability and daily limits before requesting coins. On **2026-10-09**, its
status reported a synced wallet, but our disposable-wallet claim returned HTTP 429:
“The faucet has reached its daily payout cap.” That earlier claim sent no coins. The later external integration above received
funds, but did not test sending. Respect the retry time; do not resubmit to bypass
limits. Do not reuse a mainnet phrase for this test.

### Try a funded public walkthrough

1. Scaffold and install the preview, then check the light server from the generated app:

   ```sh
   npm run check:chain
   ```

   This reads one public block; it is not full consensus validation and does not
   verify the faucet. By default it checks the published NU7 activation hash at
   4,465,026. Use `--compare-explorer --height <height>` for a separate provider
   comparison; disagreement exits 1, lookup or usage errors exit 2. A different
   height needs that explicit comparison or `--expected-hash <hash>`.
   `--server <HTTPS gRPC-Web URL>` selects your server. The checker ships in generated
   wallets; a built source checkout can also run `node scripts/check-testnet-chain.mjs`.
2. Create a disposable wallet, save its phrase privately, and copy or scan its
   receive address. Use a birthday before the first deposit. Paste the address into
   the [Valar faucet](https://faucet.testnet.valargroup.dev/) and request test coins.
   Retain the receipt, transaction ID and mined height. The faucet sees your IP
   and receiving address; never send it your phrase or viewing key. If it reports
   a daily limit, wait until the retry time before requesting again.
3. Confirm the receipt exists on your selected server's chain, then Sync. An
   unavailable faucet, a pending receipt, an absent transaction and a wrong-chain
   payment are different outcomes. Stop and record that outcome if funding fails;
   do not treat an empty balance as a successful receive test.
4. Incoming testnet funds require three confirmations by default. **Confirming**
   shows funds still waiting; **Available** is the spendable balance. At the mined
   block plus one the receive has 2/3 confirmations; scan the next block for 3/3.
   Activity shows amounts and confirmation counts. The SDK exposes this policy in
   `snapshot.confirmations` and waiting funds in `balance.totalPending`. After
   shielded funds become spendable, follow [payment review and sending](#5-review-and-send)
   with a small amount. Keep the outgoing receipt and sync until confirmation.
   A self-send to the same disposable wallet is sufficient to exercise local proving
   and broadcast without giving another party funds. Reload and check identity/history.

ChainSafe's root website redirects to `testnet.zec.rocks`, but its gRPC-Web RPC
paths still respond. Keep the configured ChainSafe origin; the redirect target's
native `application/grpc` endpoint is not browser gRPC-Web. Sync follows the
configured server's view. Changing the birthday cannot reconcile different chains.

### Reproducible funded regtest

The preview includes the app scaffolder, built packages and public chain checker.
The native validator and funded fixture runner remain source-checkout tools.
Use the [NU7 regtest recipe](RELEASE.md#nu7-acceptance) for reproducible proving,
broadcast and confirmation tests without relying on public faucet availability.

To investigate a missing payment:

1. Record its receipt’s transaction ID and mined height. Querying a public explorer
   can link that transaction to your IP; do not submit your phrase or viewing key.
2. Check whether the receipt’s transaction is available on your light server.
   Compare public block hashes with `npm run check:chain -- --height 4465026`
   from the generated app. Heights alone do not identify a chain, and an explorer
   disagreement does not identify the faucet’s node. If the payment is confirmed
   on a different chain, use an aligned server or funding source; changing the
   birthday cannot repair that.
3. If the receipt is on your server’s chain but below **Wallet birthday**, choose
   **Scan an earlier range**. This keeps the wallet and receiving address, locks
   spending in the demo and rebuilds balance/history. Pending outgoing payments
   must confirm or expire first. A reset is saved before the new scan starts;
   cancelling sync leaves progress you can resume.
4. The default deep-scan limit is 150,000 blocks. The SDK rejects a larger reset
   before changing state unless the client permits deep syncing (`deepSync`; local pipe clients permit it by default).
   Dates estimate a height with a safety margin; use the receipt’s exact block
   height or an earlier one when available.

The [NU7 regtest recipe](RELEASE.md#nu7-acceptance) starts an isolated validator,
mines mature fixture funds and tests both generated demos. Run it from the source
checkout after building/packing the SDK; the fixture tools are not included in
this preview bundle. The test proves shielded payments and checks confirmation,
uncertain submissions and reload. It is regtest evidence, not public-testnet certification.
Public chain providers can see IP addresses and request timing; memo retrieval
remains on-demand. Never use production funds in this walkthrough.

A loopback regtest fixture is used by our funded acceptance tests. Its public
fixture phrases and tiny activation heights are for that isolated chain only.
Do not change a running wallet's network or reuse its storage with another chain.
Use a separate origin/profile or [your own namespace](STORAGE.md).

## 5. Review and send

Continue only after the chain/funding check above, or in the isolated regtest
fixture. The public outgoing-send path remains unverified; the source regtest fixture verifies this step.

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
