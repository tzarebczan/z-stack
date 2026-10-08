# Browser wallet example

A minimal Vite + TypeScript wallet with local Create/Restore, sync progress,
balance, activity, shielded payment review/proving/receipts and spending lock. It uses testnet only and makes network actions explicit.
It demonstrates SDK integration, not a production recovery or login flow.

Use [the guided setup helper](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md), or first build and pack z-stack using the [root instructions](https://github.com/tzarebczan/z-stack/blob/main/README.md).

Copy this directory outside the z-stack workspace, then install the archives:

```sh
cd /path/to/copied/browser-wallet
npm install /path/to/z-stack/artifacts/z-stack-sdk-0.1.0-alpha.3.tgz
npm run dev
```

Open the localhost URL Vite prints. Create a testnet wallet or restore testnet
words with a known birthday. Save the phrase before closing the page. Sync keeps
the last balance and activity visible. The example refuses to replace an existing
wallet; use a fresh profile for recovery experiments. Never enter a mainnet seed.

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
address. Check [the current public funding limitation](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md#4-create-or-restore-then-sync)
before claiming test coins. On testnet, amounts are TAZ. The amount parser still
uses Zcash’s eight-decimal units. `zcash:` links require a dedicated review UI;
this example asks for the recipient address itself.

Payment review is disabled until the first block scan. A refused restore keeps
the pasted words; a successful import clears them. Copy actions
are explicit. Copying recovery words puts the secret on the system clipboard.
Startup diagnostics log a fixed error code only; raw provider errors may contain
private data and are not printed.
