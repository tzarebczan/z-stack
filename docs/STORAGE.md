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
