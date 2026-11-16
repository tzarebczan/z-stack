# Local storage and ownership

The browser engine has one active owner per JavaScript realm. A second
`createWallet()` returns `busy`, including while the first is initializing or
closing. Await `wallet.close()` to release it. This is an enforced single-wallet
contract, not concurrent multi-wallet support. Different tabs share the durable
generation/revision checks and origin spend lock.

```ts
import { createWallet, indexedDbWalletStorage } from "@z-stack/sdk";

const wallet = await createWallet({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev",
  storage: indexedDbWalletStorage({ name: "my-app-wallet" }),
});
// Later: await wallet.close();
```

IndexedDB is the default. Its database name belongs to your app. Supply an
`IDBFactory` for a compatible platform implementation; importing the package
does not open storage, start a worker or initialize WASM. Start a wallet only in
browser code after client mounting. `memoryWalletStorage()` is available for
tests and disposable wallets; it is not a durable recovery backup.

The SDK saves during wallet operations and attempts an additional snapshot flush
on visibility hide/pagehide. That last flush is best-effort, not an unload-time
durability guarantee. Lock on hide and clear recovery inputs; closing the client
there can terminate the worker needed to serialize its snapshot. Use `close()`
for an awaited teardown while the document is active. Back/forward-cache returns
can keep the locked owner or reload; do not create a second owner in the same realm.

## Optional encryption at rest

Pass `storageKey` to keep the store unreadable without that key, for example
while your user is locked out. Without `storageKey` nothing changes: the store
keeps its existing unencrypted format.

```ts
import { createWallet, encryptWalletStore, indexedDbWalletStorage, inspectWalletStore } from "@z-stack/sdk";

const storage = indexedDbWalletStorage({ name: "my-app-wallet" });
const status = await inspectWalletStore({ storage }); // No key needed.
if (status.encrypted) showUnlockScreen();
const storageKey = await appUnlock(); // 32 random bytes or an AES-GCM-256 CryptoKey.
if (status.exists && !status.encrypted) await encryptWalletStore({ storage, key: storageKey });
const wallet = await createWallet({ network: "testnet", server: "https://zcash-testnet.chainsafe.dev", storage, storageKey });
```

- Every record, including the preview with the viewing key and address, the
  snapshot chunks and their index, the generation records, the seed and passkey
  vault records, is sealed with AES-256-GCM (WebCrypto). Each write uses a fresh
  random nonce. The associated data binds the store, the record name and the
  commit sequence; an encrypted head record lists the sequence of every record
  that can be overwritten. A flipped byte, two swapped records, a deleted record
  or an older copy of a record fails with `store_corrupt`.
- Record names are fixed labels and random identifiers. They never contain
  addresses, keys or fingerprints. One plaintext marker record says the store
  is encrypted and lets the SDK check a key; it holds no wallet data.
- A wrong key fails with `store_key_rejected`. Opening an encrypted store
  without a key fails with `store_locked`, and so does any write to it. A key
  for an existing unencrypted store fails with `store_not_encrypted`. None of
  these is reported as an empty wallet, so create and restore cannot overwrite
  the store.
- `encryptWalletStore({ storage, key })` and `decryptWalletStore({ storage, key })`
  convert a store in place. Each verifies what it read, then replaces every
  record in one storage transaction, so an interruption leaves the complete old
  store and a retry starts again. No unencrypted copy remains in the store. Both
  need an adapter that lists its records (`WalletStorageTransaction.keys`); the
  IndexedDB and memory adapters do. Run them while no wallet client is open.
- `inspectWalletStore({ storage })` returns `{ exists, encrypted }` without a key.
- `eraseWalletStore({ storage })` deletes every record without a key and leaves
  a fresh generation tombstone that fences other open pages. The erased store
  then starts again unencrypted or with any key. Forgetting an encrypted wallet
  keeps the store encrypted with the same key. Migration and erase refuse to run
  while a wallet client is open in the page (`owner_conflict`).
- An invalid key (not 32 bytes, or a `CryptoKey` that is not AES-GCM-256 with
  encrypt and decrypt usages) rejects with `store_key_rejected`.

The SDK never derives, stores or logs the key; your application owns key
management. Losing the key makes the store unreadable: erase it and restore the
wallet from its recovery phrase, which rescans the chain. Keep the key outside
this store. While a client is open the SDK holds a non-extractable `CryptoKey`;
`close()` releases it. Pass the same `storageKey` to `forgetWallet`.

Limits: an attacker who can write to the store can roll the whole store back to
an earlier complete copy, which no local check can detect. Browser storage
engines may keep overwritten bytes on disk until they compact, so encrypting an
existing store does not erase earlier unencrypted copies from the device. Data
another package keeps elsewhere, such as `@z-stack/passkey` stores or gift-card
wallets with their own database names, is not covered. The record count and
sizes stay visible. In a Node benchmark with an in-memory IndexedDB double and
an 8 MB snapshot, reading took 25 ms instead of 4 ms and an incremental save
60 ms instead of 38 ms.

## Custom adapter contract

Implement `WalletStorage.transaction(mode, body, options)` using the exported
`WalletStorageTransaction` interface. The callback queues reads/writes; its read
callbacks may synchronously queue further operations. Do not make network calls
or await unrelated work inside these callbacks.

Every transaction must provide:

- Atomic reads, writes and deletions, including all snapshot chunks and their
  manifest/preview. Any error or cancellation before commit changes nothing.
- Serialized overlapping transactions for every connection to the same store.
  In-process exclusion alone is insufficient for an adapter shared across tabs.
- Acknowledgement after commit. With `commitWinsCancellation`, a physical commit
  that won an abort race returns its result; it must never report rollback after
  durable replacement bytes were written.
- Persistent generation tombstones after forget, and atomic compare-and-swap
  checks queued by the SDK. Never clear the store out-of-band or skip those checks.
- Cloned values on reads/writes; caller mutations cannot alter committed bytes.
  Quota/disk failures must reject and preserve the previous committed state.

The SDK owns record keys, chunking, atomic wallet replacement, and
saved-revision comparisons. Adapters own transaction mechanics and
durability. These guarantees cannot be implemented with independent async
`get`/`set` calls to localStorage or a remote object service.

Create/import refuse a populated slot with `already_exists`. `replace: true`
authorizes a replacement after your UI confirms the existing backup. The check
also runs during preparation and at the atomic replacement commit. The SDK
checks both the prior generation and snapshot revision, so a concurrent late
save cannot be silently overwritten. Preparation leaves the prior identity,
snapshot and pending payments readable; the new identity, snapshot and policy
commit together. Closing or discarding the page before that commit preserves
the old wallet. Loading may repair an interrupted earlier generation claim to
the retained snapshot without changing its bytes. Forget atomically deletes the
snapshot and leaves its tombstone; it is never repaired into a wallet. Required
foreground checkpoints fail with `wallet_db` if storage becomes unavailable;
this includes the checkpoint before submitting a transaction. Background saves
are best effort. Availability does not waive an adapter's durability obligations.

## Separate local state from remote backup

Wallet snapshots contain viewing keys and decrypted history. They are not an
encrypted spending-key backup. A remote provider should receive only an
application-chosen encrypted vault envelope with explicit consent. It must not
participate in every local transaction or unlock. Registration, authorization,
remote retention and deletion belong to your application; see [services](SERVICES.md).

Test your adapter with the SDK's replacement, abort, cross-tab revision, quota,
forget-tombstone and submission regressions before using it with funds. The
reference IndexedDB and memory adapters are covered by the repository tests;
third-party storage engines need their own crash/durability acceptance.

## Pending-aware removal

Use `await wallet.forget({ passkey: true, pending: "reject" })` for a removal UI
that must preserve unresolved outgoing payments. The SDK adopts newer saved
state under the origin spend lock, checks outgoing reservations, then deletes
only if the inspected snapshot revision and wallet generation still match in
the storage transaction. A competing save returns `wallet_changed`; a pending
payment returns `forget_pending`. Reload the saved wallet and reconcile it.

This policy is optional. Omitting `pending` keeps explicit local deletion;
standalone `forgetWallet()` also performs explicit deletion without loading the
engine. A phrase alone does not preserve pending-payment receipts or reservations.
