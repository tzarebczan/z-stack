# Browser wallet example

A testnet wallet built with Vite, TypeScript and the public SDK. It demonstrates
integration, not a production login or recovery service. Never enter a mainnet phrase.

## Quickstart

Use the [verified prebuilt preview](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md#1-get-matching-archives)
and its scaffolder. Rust is optional. Copying this source template alone does not install the SDK.

For manual setup, copy this directory outside the checkout and install the archive from the extracted preview:

```sh
cd /path/to/copied/browser-wallet
npm install /path/to/z-stack-preview/artifacts/z-stack-sdk-0.1.0-alpha.9.tgz
npm run dev
```

Open the localhost URL, create a wallet, save its numbered recovery words, then
finish backup. Sync to find payments. For test coins and a small self-send, follow
[funded testing](https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md#funded-testing).

## Checks

```sh
npm run setup:check
npm run check
npm run build
npm run preview
npm run check:chain
```

Dev, check and build run the setup guard explicitly, even with install lifecycle
scripts disabled. Preview compresses WASM and caches hashed assets; missing engine
files return 404. Configure your real host with the
[production policy](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#production-host).

## Read the code

Start at `src/main.ts` and `src/app.ts`, then open the flow you are changing:

| Flow | Files |
| --- | --- |
| Create and phrase acknowledgement | `create.ts`, `recovery.ts` |
| Restore and input validation | `restore.ts` |
| Sync, cancellation and memo retrieval | `sync.ts` |
| Review, prove, submit and retain receipts | `payment.ts`, `send.ts` |
| Local removal | `remove.ts` |
| Balance, receive QR and history | `screen.ts`, `receive-qr.ts`, `wallet-view.ts` |

See [how the example works](HOW-IT-WORKS.md) for recovery, payment and privacy
behavior. `app-context.ts` is the small coordination contract; each flow owns its
transient state. The optional `base.ts` adapter is separate.
