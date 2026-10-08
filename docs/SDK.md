# Wallet API and behavior

This SDK runs a Zcash wallet in the browser. Keys, scanning and zero-knowledge
proofs run in a Rust engine compiled to WebAssembly. Keys and decrypted payment
notes stay on your device. The light server sees block requests and broadcasts;
enabled transaction or address lookups disclose those identifiers.

The native loopback client is separate: `createNativeWallet` from
`@z-stack/sdk/native`. It has a different capability surface and does not load WASM.

## Browser setup (Vite)

```ts
// vite.config.ts
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zStack } from "@z-stack/sdk/vite";

export default defineConfig({ plugins: [react(), zStack({ coep: "require-corp" })] });
```

```ts
// wallet.ts
import { createWallet } from "@z-stack/sdk";

export const wallet = await createWallet({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev", // browser-compatible gRPC-Web
});
```

```ts
// Open the saved wallet, or restore one.
const saved = await wallet.load();
if (!saved) await wallet.restore(mnemonic, { birthday: "2024-06-01" }); // birthday: date, height, or "auto"

wallet.on("sync", (e) => setProgress(e.percent ?? 0));
wallet.on("balance", (b) => setBalance(b.availableZat));
await wallet.sync();

await wallet.unlock(mnemonic); // Start from a separate user action after review.
const { txid } = await wallet.send("u1…", "0.25", "thanks!");
```

`createWallet` loads the engine, picks the right transport for `server`, and
returns a network-bound browser wallet. Only one active client is supported;
creating another fails with `busy`. Await `close()` before opening the next.
Close interrupts the captured proof actor instead of waiting for its RPC timeout.
An unfinished software proof has no durable reservation; the saved wallet stays
intact. Interrupted hardware proving reloads the latest durable state, releases
its unsubmitted reservations and persists that rollback before close completes.
A rollback storage failure is reported while ownership is still released.
Already submitted transactions remain pending because their outcome may be unknown.
The default local store is IndexedDB; configure its namespace or supply a
transactional storage adapter. See [storage and ownership](STORAGE.md).

Snapshots larger than 32 KiB use content-defined chunks (at most 32 KiB each)
and bounded index pages, with one atomic manifest/preview commit. Changed
snapshots reuse byte-for-byte verified data, including after insertions and
deletions; unused chunks retire in that same transaction. There is no delta
replay chain or accumulating checkpoint history. This avoids Chromium private-context failures
reading repeatedly replaced large values. Existing raw, Blob and format-2 snapshots are
readable and migrate on the next successful save; unreadable legacy records
are preserved for recovery. Older SDK versions cannot hydrate the new
`z-stack-snapshot-ref-3` format, so rolling an app back requires a compatible
SDK. Chunking changes persistence only; it adds no encryption or IP protection.

## What `zStack()` does, and what production needs

The plugin configures dev and preview:

- Workers are ES modules. The scan worker, the prove worker and the Rayon thread pool are all split modules.
- The SDK is not pre-bundled, because pre-bundling breaks `new URL(…, import.meta.url)` for the WASM and worker files.
- A linked SDK checkout may be served from outside your app.
- COOP and COEP headers are sent. Pass `zStack({ crossOriginIsolation: false })` if you already send them.

Your production host must send the same headers on the HTML:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

**Multi-threading.** With those headers the page is cross-origin isolated, and
the engine scans and proves on a bounded worker pool (`wallet.runtime.mode === "multi-thread"`).
Without them it still works, on one thread.

**Content Security Policy.** If you have one, it needs three things:

- `script-src 'wasm-unsafe-eval'`
- `worker-src 'self' blob:`
- your light server's origin in `connect-src`

The WASM and worker files are emitted under `assets/` with hashed names, so
they can be cached forever.

## Wallet lifecycle

| Call | What it does |
|---|---|
| `wallet.load()` | Returns the saved wallet on this device, or `null`. It makes no network request. |
| `wallet.create({ birthday?, replace?, beforeCommit? })` | Creates a wallet and returns `{ wallet, recoveryPhrase }`. Optional preparation confirms recovery before saving. |
| `wallet.restore(mnemonic, { birthday?, replace? })` | Restores from a recovery phrase. |
| `wallet.restoreUfvk(ufvk, { birthday?, replace?, signal?, assertCurrent?, beforeCommit? })` | Creates a view-only wallet. `unlock` enables spending later. |
| `wallet.sync()` | Catches up to the chain tip. Progress arrives through `on("sync")`, and `onProgress` gives scan detail. |
| `wallet.getWallet()` | Returns the snapshot: addresses, per-pool balances, scanned height and recent history. |
| `wallet.history(limit?)` | Returns transactions, newest first. `classifyHistory(entry)` gives the action (sent, received, shielding, …) and the display amount. Classify at read time; do not persist the result (`type`), since the engine refines it as it learns more. |
| `wallet.maxSend(to?)` / `wallet.estimateFee(to, amount?)` | Gives the ZIP-317 fee from the engine's current proposal. Ordinary send builds a fresh proposal; this estimate is not a binding fee cap. |
| `wallet.send(to, amountZec, memo?)` | Sends to a unified address or a `zcash:` ZIP-321 URI (multi-recipient). Returns `{ …snapshot, txid }`. |
| `wallet.supportsTransparentSend()` | Probes the loaded browser engine for explicit transparent swap-output support; it does not prove ownership, funds or signing readiness. Hardware wallets return false; the native client does not expose this method. |
| `wallet.estimateTransparentFee(to, amountZec)` | Low-level fee preparation for one bare, network-correct P2PKH/P2SH output funded from shielded notes. |
| `wallet.sendTransparent(to, amountZec, { maxFeeZat, beforeBroadcast }?)` | Low-level software-wallet transparent output; exposes its public destination and amount while keeping change shielded. See the restrictions below. |
| `wallet.shield(thresholdZat?)` | Moves transparent funds into the shielded pool. |
| `wallet.forget()` | Deletes this device's wallet. Passkeys are kept unless `{ passkey: true }`. |

### Transparent-send primitives

Ordinary browser `send` remains shielded-only. `sendTransparent` is a separate,
privacy-reducing primitive for adapter authors. It spends shielded
notes to one bare P2PKH/P2SH address on the wallet’s network and keeps change
shielded. It rejects TEX addresses, unified addresses, ZIP-321 URIs/multipay,
memos and hardware signing. It does not enable transparent-source spending.
Only the WASM software wallet supports it, and it still needs the matching
spending seed and eligible notes. Probe `supportsTransparentSend()` first.

Applications own their swap adapters. `packages/swaps` provides an optional
experimental interface, with no provider implementation; these wallet
primitives do not constitute a ready-to-use swap flow. A funding adapter must
review the public recipient, amount and exact engine fee before invoking them.
`maxFeeZat` is an exact integer zatoshi string: the engine rejects a prepared
source fee above that ceiling. It covers the Zcash source fee, not conversion
fees, slippage, destination gas or settlement time.

`beforeBroadcast` is a synchronous predicate checked repeatedly before network
submission, including after proof, reservation persistence and synchronous
broadcast listeners. Return false (or throw) when the reviewed receipt, account,
wallet or deadline is no longer current. Before submission begins, cancellation
restores reserved notes and pending history; it does not undo application writes
made by the predicate. Keep this callback side-effect free. Once submission has
started, a lost acknowledgement preserves the pending transaction and its
transaction ID because acceptance is uncertain. Do not automatically retry or
interpret that error as non-broadcast. This callback is not an after-broadcast
cancellation guarantee.

Browser creation can prepare recovery before its first durable snapshot:

```ts
const created = await wallet.create({
  birthday: "auto",
  beforeCommit: async ({ wallet: preview, recoveryPhrase, signal }) => {
    // Application UI or encrypted backup, with cancellation handling.
    await confirmRecovery({ address: preview.unifiedAddress, recoveryPhrase, signal });
  },
});
```

`beforeCommit` is optional. Its snapshot is a copy; the wallet is not yet saved.
Rejecting the callback or closing the client before commit cancels creation and
preserves any prior saved wallet. The signal aborts when SDK ownership is lost;
use it to dismiss prompts and cancel backup work. The SDK can cancel its wait
even if the callback ignores the signal, but cannot undo external writes: retain
an ownership receipt and roll those back in your application. Do not report a
wallet as durable until `create` resolves. After physical commit, creation returns
the captured phrase even if close races the completion event. Native clients
reject this browser-only hook before issuing a request. The plain TypeScript and
Next.js examples confirm the phrase before committing, so page teardown cannot
leave an unbacked-up saved wallet merely because an unload warning was suppressed.

Required foreground checkpoints fail if configured storage becomes unavailable,
including before transaction submission. Never treat `wallet_db` as a successful
save or retry an uncertain broadcast as another payment. Background saves remain
best effort; memory adapters and raw storage-free lab clients are deliberately
ephemeral. See [storage](STORAGE.md) for adapter durability requirements.

Browser `restoreUfvk` accepts options with `signal`, `assertCurrent`,
and an async `beforeCommit(snapshot)` callback. The callback can read
`wallet.history()` and prepare application state before the SDK commits its
first snapshot. Abort the signal when the caller leaves or its account changes;
the guard also revalidates ownership after asynchronous preparation. A failure
before the atomic snapshot commit restores the previous saved wallet. If the
callback writes application storage, retain an exact ownership receipt and undo
only those writes on failure. Publish UI/session state synchronously after the
restore resolves, without another awaited task. Cancellation after a completed
commit does not undo the restored wallet. These hooks are rejected for native
seed restore; they are never silently ignored. `close()` locks and cancels
the entire client. Use a restore signal to cancel only that import attempt.

Amounts are decimal ZEC strings at the API edge. Use `parseZecToZatoshis` and
`formatZatoshis` to convert.

Browser checkpoints skip serialization and IndexedDB writes when that exact
session revision already committed. They still validate the saved wallet's
identity and revision, so another tab's save or Forget is observed. Failed saves
remain retryable, and spend/restore commits always perform their atomic save.
Hydration may repair or migrate state, so the first sync after reload still
saves once, reusing unchanged payload chunks. See [storage](STORAGE.md).
Subtree roots are refreshed for existing wallets too, allowing Rust to prune
settled leaves without a reset. Browser quota failures report `storage_full`
and preserve the last committed snapshot. See [error handling](ERRORS.md).

`HistoryEntry.memoStatus` distinguishes pending, available, empty (no recovered
text), unavailable, unknown (legacy/provider), and notApplicable payment notes.
The status does not trigger extra lookups just to upgrade older records.

Shared memo retrieval requires a gateway implementing the optional
`/zstack/memos` extension. For a URL server, set both `memoFetch: "shared"`
and `sharedMemos: true` only when that gateway supports it. The capability
defaults to false, even on loopback; standard gRPC-Web servers do not advertise
it. Shared mode on an unsupported server makes no memo requests and never
falls back to sending wallet transaction IDs. `lightServer` and
`grpcWebTransport` accept the same capability option. See
[public range retrieval](PUBLIC-RETRIEVAL.md) for the gateway protocol.

### Spending seed

The default `unlockPolicy: "each-spend"` leaves create/import locked. Call
`await wallet.unlock(words)` before sending; the seed is dropped after a spend
attempt. `"session"` explicitly retains the seed in memory until `lock()` or
`close()`. Reload always locks. No plaintext seed is stored in sessionStorage,
localStorage or IndexedDB, and saved wallet metadata cannot override app policy.

`wallet.unlock()` can call an application-owned `WalletUnlocker`. It verifies the
returned seed against the wallet before spending. A late provider result is
rejected after lock, close or replacement. Local passphrase and passkey vaults
are optional; configure them through `/services`, independently of registration.
See [accounts, vaults and backups](SERVICES.md).

For passkeys, prepare any account options before displaying the button and start
the ceremony from the click. A promise can be passed to `wallet.unlock(...)` so
the ceremony starts before wallet reads. Browser/device behavior still needs
acceptance testing; the SDK does not promise that an asynchronous database read
preserves user activation on every device.

## Hardware wallets

Keystone and Ledger (Zcash app 3.9.3+ to sign, 3.9.4+ to connect) spend
Orchard and Ironwood notes. The spending key never leaves the device: the
engine builds a PCZT for the send, proves it while the user reviews on the
device, applies the device's signatures (each verified against the
transaction before it is kept), and broadcasts.

```ts
import TransportWebHID from "@ledgerhq/hw-transport-webhid";
import { ledgerAccount, ledgerSigner, webHidLedger } from "@z-stack/sdk/hardware";

const ledger = webHidLedger(await TransportWebHID.create()); // a user gesture
await wallet.restoreHardware(await ledgerAccount(ledger, { network: "mainnet" }), { birthday: "2024-06-01" });
await wallet.send(to, "0.25", "thanks", { signer: ledgerSigner(ledger), onStage: showStage });
```

Keystone signs over QR codes, so the app supplies the round trip:

```ts
// The account comes from Keystone's `zcash-accounts` QR: UFVK, index and ZIP-32 seed fingerprint.
await wallet.restoreHardware({ device: "keystone", ufvk, seedFingerprint, accountIndex: 0 }, { birthday });
const signer = keystoneSigner(async (pczt) => {
  showAnimatedQr(encodeUr("zcash-pczt", pczt));   // your UR encoder
  return await scanSignedPczt();                  // reject with hardwareCancelled() on back
});
await wallet.send(to, "0.25", undefined, { signer });
```

- One spend at a time per origin: while a send (seed or hardware) runs in one
  tab, a send from another tab fails fast instead of picking the same notes,
  and a tab whose copy is older than the last save reloads it before spending.
  A tab never overwrites a newer save: its save fails with `wallet_changed`
  and it takes the saved wallet instead (retry the operation).
- The notes a send spends stay reserved while the device signs. Cancelling
  (`hardwareCancelled()`) or any failure before broadcast releases them and
  saves that; syncing while the device is busy is fine. A background checkpoint
  can save reservations, and finalized sends are saved before submission. If
  rollback or close reports a storage error, reopen and inspect the saved state
  before spending again; failure does not prove a reservation was removed.
- A hardware account never spends with a phrase: `unlock` and seed sends
  are refused, even if the same seed is known on this origin.
- Only the device's signatures are used from what it returns; a signature for
  another transaction or key fails with `hardware_mismatch`.
- Device errors are `WalletError`s: `hardware_rejected`, `hardware_locked`,
  `hardware_app` (Zcash app not open or too old), `hardware_unsupported`,
  `hardware_mismatch`, `hardware_cancelled`.
- Not yet: transparent inputs (so no shielding from a hardware wallet),
  Sapling notes, and Ledger on test networks (its app exports mainnet keys
  only). A Ledger send cannot move legacy Orchard funds into Ironwood yet
  (a Zcash app limitation), so the send is refused with `hardware_unsupported`.

## Light servers and privacy

`server` accepts:

- a gRPC-Web URL (such as `https://zcash-testnet.chainsafe.dev` for testnet)
- a local `z-wallet pipe` URL (`http://127.0.0.1:1239`), which is faster for a node you run yourself
- any `BlockTransport`. Its `submit` throws `BroadcastRejection(code, reason)`
  when the node refuses a transaction; any other error counts as an unknown
  outcome, so the send is kept and resent rather than undone.

Compact-block scanning and memo decryption run locally; the server receives no
seed or viewing key. Requests still expose metadata to the server:

- **Broadcasts.** Every transaction you send passes through it, so it sees your IP address and the timing.
- **Shared retrieval.** `transparentScan: "compact"` discovers deposits locally from all-pool compact ranges (protocol >= v0.5.0). `memoFetch: "shared"` retrieves public ranges from the gateway's `/zstack/memos` endpoint. Both persist independent coverage and never fall back to selective requests. See [protocol, limits and setup](PUBLIC-RETRIEVAL.md).
- **Selective lookups.** Transparent address lookup is available on local servers or with `transparent: true`. The public browser wallet defaults to `memoFetch: "on-demand"`, requesting wallet transaction IDs only after `fetchMemos()`. Explicit `auto` requests them during sync; the experimental raw browser client retains that default. `setMemoFetch` also accepts `shared`; `setTransparentScan` changes between `compact` and `off`. Selective enhancement processes up to 500 missing transactions with eight concurrent requests, persists progress, and retries unfinished work at an unchanged tip. `memoFetchStatus` distinguishes complete, scanning and unavailable states. Native wallet enhancement remains automatic.
- **Scan activity.** Requested block ranges, birthday tree height, subtree offsets and connection timing reveal when and how far back a wallet scans. They do not identify which shielded outputs it decrypts.

SDK browser chain and native-bridge requests omit ambient cookies and referrers
and reject redirects. An explicitly supplied loopback bridge bearer token is
still sent to that bridge. These controls do not hide the client's IP address
from a remote server or provide an anonymity relay. A custom `BlockTransport`
owns its own network policy.

The repository's `scripts/grpc-web-proxy.mjs` binds loopback and converts binary
gRPC-Web to a fixed native gRPC upstream. It forwards protocol headers only and
streams responses with cancellation and backpressure. For a remote upstream,
transparent methods are denied unless `--allow-transparent` is supplied: a
loopback URL must not silently bypass the remote-address disclosure policy.
This development adapter does not implement public-service quotas, TEE
attestation, oblivious transport or PIR. Explicit remote HTTP configuration is
still possible; it is not a guarantee of encrypted upstream transport.

## Errors

Every failure is a `WalletError` with a stable `code` and a user-facing
message (`walletErrorMessage`). Switch on `code`; do not parse `message`, which
is diagnostic text that changes. `broadcast_failed` means the outcome is
unknown: the transaction (`error.txid`) is saved and resent, so show it as
pending and do not offer a retry, which could pay twice. `broadcast_rejected`
means the node refused it and the send was undone. `wallet_changed` means
another tab saved the wallet first; this tab has reloaded it, so retry.
Passkey failures are `PasskeyError`s. Use `isPasskeyCancel(e)` to ignore
cancellations.

## Native engine

<!-- sdk-example: native-wallet -->
```ts
import { createNativeWallet, NATIVE_BRIDGE_URL } from "@z-stack/sdk/native";

export function connectDesktop(token: string) {
  return createNativeWallet(NATIVE_BRIDGE_URL, { token });
}
```

Supply the desktop bridge's bearer token from your application. This separate
native surface calls the desktop engine over loopback; it does not expose browser
storage, hardware restore or browser-only transparent-send methods.
Do not call `initialize()` or `createWallet()` for it. The `:8787` JSON API
holds the wallet: never bind or proxy it off localhost.

## Advanced

- **`@z-stack/sdk/engine`** exposes experimental `initialize` and `createWasmClient` primitives. Prefer `createWallet` with its thread options, transactional storage and custom `BlockTransport` for browser integrations.
- **`@z-stack/sdk/lab`** holds lab and benchmark helpers: loopback endpoints, scan-worker controls and thread tuning. It is not a stable surface.
- **Building from source:** in z-stack, run `pnpm build:wasm && pnpm build:wasm:mt`, then build `@z-stack/core`, `@z-stack/passkey`, and `@z-stack/sdk` in that order with `pnpm --filter <package> build`. Rebuild the WASM artifacts after Rust changes; the package build copies the existing artifacts.

WASM seed sends emit `wallet.on("broadcast", ({ kind, txid }) => …)` immediately
before submission, after the pending transaction has been saved. Consumers can
keep uncertain sends pending even if a later snapshot refresh fails. Construction
and proving failures emit no broadcast event. This event is specific to the WASM
seed send/shield path; hardware sends retain their per-operation `onStage` callback.

Create/import return `already_exists` when a saved wallet is present unless
`replace: true` was explicitly supplied. Confirm the existing wallet's backup
before setting that option. The final claim checks the slot atomically, so a
late save in another tab cannot be silently overwritten.

Every public browser method normalizes errors to `WalletError`. Lifecycle codes
include `busy`, `closed`, `cancelled`, `not_initialized` and `not_found`.
After submission starts, `broadcast_failed` carries `txid`, including when the
client closes or the wallet changes. Check that transaction; do not construct
another payment merely because the UI could not refresh.

`send(..., { signal, beforeBroadcast })` also guards software and hardware sends.
The signal/review predicate prevents submission when cancelled before the final
check. Proof work may finish before cancellation is observed. Cancellation
cannot retract a request that has already started.

## Rescan an earlier range

```ts
import { validateBirthdayInput } from "@z-stack/sdk";
validateBirthdayInput(input); // Local syntax/calendar check before clearing secret inputs.
wallet.lock();
await wallet.rescan({ birthday: input });
await wallet.sync();
```

`rescan` is a browser-wallet method. It keeps account keys, hardware metadata,
view-only status, current addresses and the next address index. It removes scan
state and old birthday frontiers, then saves the new birthday atomically before
returning. A failed save restores the in-memory state; a stale tab adopts the
newer committed wallet. Close, deletion and replacement never revive the old session.
It rejects later birthdays, unresolved unmined outgoing transactions and ranges
beyond the default scan limit unless the client permits deep syncing (`deepSync`; local pipe clients permit it by default).
It does not unlock the seed or enable automatic shielding. Use the older
`resetScan()` only for same-birthday repair with a deliberately reviewed workflow.
A rescan cannot recover a transaction absent from the configured server’s chain.


## Network-aware date estimates

Wallet create/restore/rescan operations resolve date birthdays using their own
network. Standalone helpers default to mainnet; pass the network explicitly for
another chain:

<!-- sdk-example: network-birthday -->
```ts
import { blockSpacingSeconds, heightFromDate } from '@z-stack/sdk';
declare const tipHeight: number;
const seconds = blockSpacingSeconds('testnet', tipHeight);
const birthday = heightFromDate('2026-09-20', tipHeight, 'testnet');
```

The estimate spans historic spacing changes, including NU7's 25-second blocks.
An exact birthday height remains preferable. Regtest helpers accept
`{ network: 'regtest', regtestNu7Height: 250 }` for a matching custom schedule.
Wallet runtime options use `regtestNu63Height` and optional `regtestNu7Height`;
NU7 must activate after NU6.3. Set these before opening a wallet.
