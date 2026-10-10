# Browser integration

The SDK supplies the wallet engine and TypeScript API. Your app supplies screens,
authentication, backup transport, and any swap or fiat integration. Account
and backup services are optional and configured by your application.

Start with [the guided example setup](GETTING-STARTED.md) for a fresh app using
local archives. The steps below cover adapting your own application.

## Install

Download and [verify the preview bundle](GETTING-STARTED.md#1-get-matching-archives)
from the [alpha.8 release](https://github.com/tzarebczan/z-stack/releases/tag/v0.1.0-alpha.8).
The SDK archive is inside its `artifacts/` directory. Install that `.tgz` in your app. It includes both single-threaded and threaded WASM
engines, JavaScript bindings, workers, integrity manifests, and matching core
and passkey helpers. No Rust toolchain or engine compilation is required.

[Building from source](../README.md#build-from-source) is an optional alternative
for engine changes or custom WASM builds. No registry package is currently
published. Import helpers through SDK re-exports in wallet apps to
share the same error classes. Standalone core/passkey archives are for apps
using those packages directly. Node 22.18+ is required for setup tools; npm’s
`engines` check is advisory, so an install warning does not certify older Node.

```sh
npm install /path/to/z-stack-preview/artifacts/z-stack-sdk-0.1.0-alpha.8.tgz
```

Use the [runnable Vite example](../examples/browser-wallet/README.md) as a starting
point. Its TypeScript imports use the built distribution with no source aliases.
The SDK is ESM-only. Browser wallet initialization belongs in client code;
`@z-stack/core` helpers can also be used in server code.

Configure a compatible hosted gRPC-Web provider for browser sync. Running a
validator or light server is optional; the separate
[NU7 container fixture](https://github.com/tzarebczan/z-stack/blob/main/infra/nu7/README.md) is for local testing and is not
needed to use the prebuilt WASM.

## Configure Vite

```ts
import { defineConfig } from "vite";
import { zStack } from "@z-stack/sdk/vite";

export default defineConfig({ plugins: [zStack()] });
```

The plugin enables ES-module workers, excludes the SDK from dependency
pre-bundling, and allows a linked checkout to be served in development. It sends
COOP/COEP headers in dev and preview. It does not deploy headers to your host.
Use the installed package exports; importing `src/`, generated bindings, or
individual worker files bypasses the integration contract.

Vite and the [Next.js webpack demo](../examples/next-wallet/README.md) have installed-archive
acceptance checks. Other bundlers must preserve the SDK's
`new URL(..., import.meta.url)` assets and module workers, including nested
Rayon workers. Do not assume support from a successful TypeScript check alone.

## Configure Next.js

Start with the [Next.js demo](../examples/next-wallet/README.md). It uses the App
Router with client-effect initialization and a static server-rendered shell.
Build with `next build --webpack`; the example's configuration adds:

```js
webpack(config) {
  config.module.rules.push({ test: /\.wasm$/, type: "asset/resource" });
  return config;
}
```

Next's webpack compiler emits the SDK's worker URLs and WASM files. The SDK
verifies and instantiates the bytes. Preserve the [production headers](#production-host)
on your host; the example supplies them through `headers()`. Its lifetime queue
awaits the prior close before initializing another owner during navigation.
Turbopack and other Next deployment targets need their own acceptance.

## Open one wallet client

```ts
import { createWallet } from "@z-stack/sdk";

const wallet = await createWallet({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev",
  memoFetch: "on-demand",
  autoSync: false,
  autoShield: false,
});
const offSync = wallet.on("sync", (progress) => renderProgress(progress));
const offBalance = wallet.on("balance", (balance) => renderBalance(balance));
const saved = await wallet.load();
```

The browser endpoint must serve gRPC-Web and allow your app's CORS origin.
Native gRPC endpoints such as `testnet.zec.rocks:443` need a gRPC-Web gateway;
being reachable by a desktop wallet does not make them browser-compatible.

Automatic shielding is an explicit opt-in. `autoShield: true` can shield eligible
transparent funds after sync under either unlock policy, but only while a spending
seed is attached. With the default `each-spend` policy, that shield consumes the
unlock, including a failed attempt; unlock again before sending. Sync never calls
your unlocker automatically. Explain this before enabling background shielding.

`renderProgress` and `renderBalance` are your app's functions. `load()` reads
local storage; a null result means the user needs Create or Restore. Call
`createWallet` once per page, including during framework re-renders. Only one client may own the page engine; a second active `createWallet` fails
with `busy`. Await `close()` before opening another. The default IndexedDB
namespace holds one network-bound saved wallet. Applications may configure
different local namespaces, but concurrent active wallets in one page are not
supported. Tabs sharing a namespace must respect revision/ownership conflicts.

## Create, restore, and sync

Use the network selected when creating the client. For a new wallet:

```ts
const created = await wallet.create({ birthday: "auto" });
// Show created.recoveryPhrase once and have the user save it before proceeding.
// Do not put the phrase in logs, analytics, or persistent application state.
```

For an existing wallet:

```ts
await wallet.restore(words, { birthday });
// Clear the phrase input. Keep the user's existing recovery phrase.
```

`words` and `birthday` come from the user's recovery flow. Use a known birthday
height or date before the first transaction. `auto` is suitable for new wallets;
it is not a reliable recovery birthday for old funds. Restores more than 150,000
blocks behind a remote server need `deepSync: true`; request that bandwidth
explicitly in your UI rather than retrying without explaining it.

Once the wallet exists:

```ts
await wallet.sync();
const history = await wallet.history(50);
wallet.startAutoSync();
```

Show `snapshot.balance.totalAvailable` as spendable, and
`snapshot.balance.totalPending` (or `pendingZec`) separately as confirming funds.
The default mainnet/testnet policy needs three confirmations for incoming funds;
read `snapshot.confirmations.untrusted` rather than hardcoding that threshold.
A mined transaction may still be confirming. Use `entry.confirmations`, or the
snapshot's scanned height and mined height, for activity counts. Confirmation
counts describe history; they do not replace the balance or authorize a send.

Keep the last balance and history visible during background sync. Progress does
not mean local transactions disappeared. Auto-sync follows new blocks while the
page is visible and catches up when it returns. `cancelSync()` stops a scan;
it does not roll back previously committed progress.

Passkey registration must start from a user gesture. A local passkey record is
not a confirmed off-device backup. See the [vault guide](../packages/passkey/README.md)
and [recovery boundaries](SECURITY.md#recovery-and-locking).

## Spend and handle errors

Pass decimal ZEC strings to `estimateFee` and `send`. The engine calculates
fees. Use unified shielded destinations for ordinary sends. Transparent spend
primitives are intended for explicitly approved swap deposit adapters; see
[the API](SDK.md) and [swaps](SWAPS.md).

```ts
import { WalletError } from "@z-stack/sdk";

try {
  await wallet.unlock(words); // Explicit user reauthentication for this spend.
  const result = await wallet.send(destination, "0.25", "Thank you");
  showPending(result.txid);
} catch (error) {
  if (error instanceof WalletError && error.code === "broadcast_failed") {
    showPending(error.txid); // Check this receipt; do not construct another payment.
  } else {
    showError(error);
  }
}
```

These UI callbacks are application code. `broadcast_rejected` means the node
explicitly refused the transaction and the reservation was undone.
`wallet_changed` means another tab committed first; reload the UI and ask the
user to retry. Check a transaction receipt before another payment when submission
already started. Avoid using error-message text as an API.

When leaving the wallet screen:

```ts
offSync();
offBalance();
await wallet.close();
```

`close()` locks, cancels work and releases the engine owner. Await it before
opening another client. `lock()` drops the cached seed while keeping history
and scanning available. `forget()` deletes local data but leaves the client
usable; a passkey record stays unless `forget({ passkey: true })` is requested.
Provider logout is a separate explicit application action.

On `pagehide`, lock spending and clear phrase inputs. Do not call `close()` from
that event: the SDK starts a best-effort snapshot flush on hide, and terminating
the scanner interrupts it. Keep the locked client for back/forward-cache returns,
or reload the page on a persisted `pageshow`. Await `close()` during an explicit
component/app teardown while the document is still alive. Save during normal
operations; browsers can stop a page before any hide callback finishes.

## Production host

For threaded WASM, serve HTML with:

```http
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Ensure every cross-origin resource is CORS-enabled or sends an appropriate CORP
header. `require-corp` is verified across Chromium, Firefox and WebKit. The plugin
default remains `credentialless`, which did not enable isolation in the tested
WebKit engine; configure `require-corp` when you need threaded WebKit support.
Configure `zStack({ coep: "require-corp" })` to match development. COOP can sever
`window.opener` relationships used by OAuth or payments; test those flows before
enabling it. Apps that cannot isolate use `zStack({ crossOriginIsolation: false })`
and single-thread scanning. An iframe also needs permission from its parent.

Serve `.wasm` as `application/wasm` and worker scripts as JavaScript. Keep hashed
assets available across rolling deployments and cache them immutably; avoid
serving an app-shell fallback for missing WASM or integrity manifests. Integrity
checking detects mismatched bytes, not a malicious replacement of both the
manifest and binary by a compromised host.

If your app uses CSP, permit WASM compilation with `'wasm-unsafe-eval'` in
`script-src`, module workers in `worker-src 'self' blob:`, and the configured
light server in `connect-src`. Combine these entries with the rest of your
application policy; do not replace an existing policy with this partial list.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Worker URL points at `node_modules` or a `.ts` file in production | Use `/vite`, rebuild packages, and import distribution exports |
| WASM fetch returns HTML or 404 | Host routing, asset base URL, missing deployment assets |
| Integrity mismatch | Engine binary and manifest came from different builds |
| Runtime stays single-threaded | HTTPS/localhost, isolation headers, iframe permissions, worker console errors. Subscribe to `on("runtime")`; the initial mode describes key bindings while `scanner` is `"starting"` |
| Reload logs `snapshot save on hide failed` | Lock on `pagehide`; calling `close()` there interrupts the scanner needed for the best-effort hide flush. Await `close()` during active app teardown |
| CORS failure | gRPC-Web and CORS support at the selected server; isolation does not grant CORS |
| `/zstack/memos` returns 404 | Server does not support shared ranges; do not set `sharedMemos: true` |
| Old restore rejected | Supply a real birthday and explicitly opt into deep sync |
| Passkey works but another device has no wallet | Transfer the encrypted vault record; a synced credential alone is insufficient |
| Saved wallet is another network | Use the saved network or intentionally replace the wallet after confirming its backup |

For custom assets, pass `wasmBasePath` to `createWallet` before the engine
initializes. Conflicting engine configuration is rejected; reload to change it. The directory must
contain matching ST bindings' binary and `integrity.json`; custom assets select
single-thread mode. Advanced transport and lifecycle hooks are documented in
[the wallet API](SDK.md).

Optional registration, authentication and encrypted backup adapters are available
through `@z-stack/sdk/services`. See [accounts and backups](SERVICES.md) for the boundary
between the local wallet and your server. Your application configures account
and backup providers.

For opt-in deployment probes, see [setup diagnostics](DIAGNOSTICS.md). Custom
stores can use [adapter acceptance](ADAPTERS.md); UI retries and external device
teardown are covered in [error handling](ERRORS.md).

### Refresh saved state across tabs

Call `wallet.load()` to reload the current saved wallet. A replacement saved by another tab retires the old session and spending seed; a newer snapshot of the same wallet refreshes its state. An active spend holds the origin lock; loading reports `busy` until it finishes. Use `wallet.getWallet()` for the current in-memory snapshot.

The pinned nightly threaded source build may warn that `target-feature=atomics`
is unstable. Atomics are required for shared WASM memory. The build verifies the
real threaded engine; do not remove the feature to silence the warning. Recheck
both engines and browser acceptance before updating the toolchain. Downloading
the prebuilt alpha avoids this source-build requirement.

## Engine loading

With workers and cross-origin isolation, default startup fetches both WASM
variants: roughly 10.9 MB single-threaded for key/UI helpers and fallback, and
19.2 MB threaded for scanning (uncompressed sizes). Each engine initialization reuses its
integrity-verified bytes for instantiation. Separate worker contexts can request
the same asset; configure compression and caching for the emitted hashed assets. Do not skip
integrity verification to reduce network work.

`createWallet({ ...options, preferMulticore: false })` avoids the threaded-engine
download. Scanning stays in a worker, but uses one thread. Apply this option
before initialization; changing runtime settings requires a reload. `threads: 2`
limits the threaded scanner but does not select the smaller engine. Runtime
mode can change while the background scanner starts. Subscribe without a timer:

```ts
const wallet = await createWallet({
  network: "testnet", server: "https://zcash-testnet.chainsafe.dev",
  onLoadProgress: progress => showEngineLoading(progress),
});
const offRuntime = wallet.on("runtime", runtime => showRuntime(runtime));
// runtime.scanner is "starting", "ready" (worker), or "main-thread".
// Before app teardown: offRuntime(); await wallet.close();
```

`on("runtime")` immediately supplies the current state and then reports worker
readiness, restart and fallback. A failed scan generation still needs explicit
wallet-load recovery; a runtime event does not replay failed wallet operations.
`onLoadProgress` reports local key/scanner download, verification and startup,
including background scanner work. Its callback is released on close or failed
creation. It contains no wallet identifiers or provider payloads. Byte counts
are decoded WASM bytes; totals are omitted when compressed transfer lengths
cannot describe decoded size, including cross-origin responses that may hide
compression headers. Do not show 100% or allow wallet operations before
integrity verification and initialization finish. Handler exceptions never
skip verification. Native engine clients do not emit browser runtime events.

Creating with `birthday: "auto"` requires a tip request. A numeric
`birthday: 4480403` can create offline; choose a known height before the wallet's
first deposit and sync later. Date birthdays need the selected server's tip.
Never substitute an arbitrary height when automatic creation cannot reach a
server: doing so can omit deposits from the later scan.

### Why two browser engines load

In multicore mode the current SDK keeps synchronous key/address and wallet bindings
on the page using the single-thread engine, while the threaded scanner runs in a
worker with separate WebAssembly memory. Reusing one engine for both requires a
change to those ownership or API boundaries; removing a download without that
change would break initialization or worker isolation. This release retains both
integrity-checked engines. Use `preferMulticore: false` for the existing smaller
startup, or enable HTTP compression and immutable hashed-asset caching. Moving
key operations behind an asynchronous worker API is a separate architecture change.

## Transaction ID byte order

`BlockTransport.tx(txid)` accepts the 64-character transaction ID shown by explorers and returned by wallet history. The built-in transports convert it to the wire byte order expected by the light server. Do not reverse it before calling `tx()`. A custom transport must perform that conversion itself. This fetch remains an explicit privacy choice: the server learns the requested transaction ID.
