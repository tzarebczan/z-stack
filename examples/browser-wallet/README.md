# Browser wallet example

A minimal Vite + TypeScript wallet with local Create/Restore, sync progress,
balance, activity, shielded payment review/proving/receipts and spending lock. It uses testnet only and makes network actions explicit.
It demonstrates SDK integration, not a production recovery or login flow.

Prefer [the guided setup helper](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md). Source compilation is optional; for custom engines follow the [root instructions](https://github.com/tzarebczan/z-stack/blob/main/README.md).

For manual setup, first download and verify the published SDK archive as that
guide describes. Copy this directory outside the workspace, then install the
downloaded archive. A fresh clone has no `artifacts/` directory; plain
`npm install` in a copied template does not add the unpublished SDK:

```sh
cd /path/to/copied/browser-wallet
npm install /path/to/sdk-alpha/z-stack-sdk-0.1.0-alpha.4.tgz
npm run dev
```

Open the localhost URL Vite prints. Create a testnet wallet or restore testnet
words with a known birthday. Blank creation birthday uses the server tip minus
100 and requires a working server. An explicit positive height creates offline;
Sync still needs a server. Use a birthday before the first deposit. Save the phrase before closing the page. Sync keeps
the last balance and activity visible. The example refuses to replace an existing
wallet. Use **Remove local wallet** after saving the phrase and payment receipts,
then restore into the empty slot. Never enter a mainnet seed.

The example imports only installed package exports, with no workspace aliases or
`@z-stack/source` condition. `npm run check` validates the types and `npm run build`
builds production assets. Hosting still needs the
[production configuration](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#production-host).

The release consumer test copies this example into a temporary external app and
installs the actual tarballs. Its optional browser harness uses a separate mock
regtest page; no test hook or mock provider is shipped in this example.

## Sending

The form reviews a shielded address, exact decimal amount, memo and estimated
fee. It rechecks balance/fee before proving and rejects expired review before
submission. A phrase unlocks this payment only; its DOM input is cleared before
awaiting the SDK. Cancel prevents submission after the current proof finishes.
The submitted receipt is retained while syncing and becomes confirmed when mined.
Unknown acknowledgement keeps the known transaction ID and blocks a new-payment
shortcut until the user checks chain activity. No blind retry is offered.

The demo displays the network, server, birthday and scanned height. Use **Scan an
earlier range** for older deposits on the same chain; it retains the wallet and
address. Check [the current public funding limitation](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md#funded-testing)
before claiming test coins. On testnet, amounts are TAZ. The amount parser still
uses Zcash’s eight-decimal units. `zcash:` links require a dedicated review UI;
this example asks for the recipient address itself.

Payment review is disabled until the first block scan. A refused restore keeps
the pasted words; a successful import clears them. Copy actions
are explicit. Copying recovery words puts the secret on the system clipboard.
Startup diagnostics log a fixed error code only; raw provider errors may contain
private data and are not printed.

## Loading and funding limits

The demo shows `wallet.runtime` beside the balance. It stays on "Starting scanner…" until `scanWorker` is set, because the threaded worker loads after the key bindings and the first read is otherwise single-thread. Default isolated startup
uses roughly 30 MB of uncompressed WASM across its two engine variants. Set
`VITE_ZSTACK_MULTICORE=false` before starting Vite to download only the roughly
10.9 MB single-thread engine; scanning then uses one thread. Both modes keep
integrity checks and run scanning in a worker.

The funding warning is visible before Create and Send. Empty testnet sync
verifies only the selected light server. The preview cannot provide a supported
funded receive/send walkthrough until a faucet and that server agree past NU7.
[Funded testing](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md#funded-testing)
explains the chain check and the separate source-checkout regtest runner. Payment
errors stay beside the payment form; engine details are not display copy.

Local removal is disabled while payment history contains pending transactions.
Sync until they confirm or expire before removing the browser’s saved wallet.
