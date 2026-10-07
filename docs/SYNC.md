# Sync design

## Layers

| Layer | Fast path |
|-------|-----------|
| Full node | Zakura **snapshots** + pruned mode |
| Indexer | Zaino compact-block DB (local preferred) |
| Light wallet | `zakura-client-backend` sync + orchestration in `z-engine` |

## Wallet sync

Restore imports the account keys, records the birthday tree state, and saves the
wallet. It does not discover historical notes or balances. The diagnostic apps start sync after restore returns. SDK consumers explicitly
call `sync()` or enable auto-sync; sync downloads and trial-decrypts
compact blocks from the birthday, persists scan progress, then enhances mined
transactions. The balance shown before sync completes is provisional. A later
sync resumes from the committed scan island instead of importing keys again.

### Fetch, decrypt, and persist

Desktop and WASM share `z_engine::scan` tuning and empty-block detection. Native
sync uses gRPC directly, with six overlapping 2,000-block ranges on two hot
HTTP/2 channels. The windows are 8 MiB per stream and 64 MiB per connection;
`http2_adaptive_window` remains off because it would reset them to 64 KiB.
Root retrieval, prefetch, Rayon trial decryption, and the preceding SQLite
persist overlap. Decryption starts after 256 blocks arrive rather than waiting
for a complete 4,000-block scan batch. The scan pool is separate from the
two-worker gRPC runtime. Fetch tasks run on Tokio, avoiding a blocking thread
join that could deadlock a current-thread desktop runtime.

Empty compact blocks skip trial decryption, but every height retains compact
metadata and missing `chain_metadata` is filled. Persistence coalesces 8,000
heights into note/transaction rows, a scan-queue watermark, and a final block
row. It must receive contiguous height runs, never a shielded-only slice with
gaps. Empty persistence skips the frontier walk. Wallet SQLite connections use
WAL and `synchronous=NORMAL`.

Native sync does not run stock `scan_cached_blocks` for every ten-block Verify
lookahead. Far-from-tip Verify becomes ChainTip work and is coalesced. The
birthday frontier is grafted once on restore or first resume. After skipped
commitments, a note-bearing batch fetches its actual preceding state for row
validation and frontier append. Keep that frontier separate from the original
graft: never relabel a birthday frontier as a later checkpoint. Note-bearing
batches persist before a dependent batch's decryption so nullifiers stay correct.

WASM prefetches four ranges. The loopback pipe uses four overlapping 2,000-block
HTTP requests, or two on a constrained connection, with a two-range buffer and
an 8,000-block request cap. Native block cache files are packed range files.
Sync performance depends on the server, wallet and hardware; see [performance tuning](PERFORMANCE.md).

### Roots and selective shard scanning

Subtree retrieval begins at the shard containing the scan's first leaf, using
the scan-start tree state or a longer SQLite prefix. Earlier shards are never
fed to the selective scanner. Sapling, Orchard and Ironwood roots are fetched
concurrently in two-root pages, four in flight, and delivered in shard order.
Each stored run expands the root-covered range immediately. Requesting every
root from shard zero can exceed older Zaino stream deadlines.

Only birthday, note-bearing, and tip shards need local hashing when verified
subtree roots cover interior shards. Each persist flushes note-bearing or
birthday/tip partial shards in the same transaction as scan completion. Unmarked
interior shards remain buffered until complete, then can be dropped.

### Continuity, checkpoints, and recovery

The selective scanner calls `scan_block` without a prior block, so orchestration
checks chain links. A range's first `prev_hash` must match the preceding range,
stored block row, or grafted parent tree state. A mismatch takes the existing
rewind to a stored row at least ten blocks back. Ten rewinds in one sync fail
instead of repeatedly discarding committed progress.

After reset, birthday wipe, or a sparse watermark, truncate only to an existing
block row. Never rewind to an absent persisted height. Checkpoint repair needs
a conflicting tree size at the same block; unchanged positions across empty
blocks remain valid. Wipe rebuilds the database while preserving the encrypted
seed. A file lock preventing replacement preserves the original wallet and asks
the user to close other instances before retrying.

### Progress

Download, scan, and persist overlap; show a stable catch-up headline with
concurrent progress rather than a sequential checklist. Progress remains at
zero until download or scanning leaves the birthday. Download ticks follow the
stream, at roughly 250 ms intervals. Native UI rates use a two-second window
for both downloaded and scanned blocks; it does not show a clock ETA.


### Native retry and crash safety

An unreachable or timed-out light server retries from committed progress with
jittered 2, 4, 8, and 16 second backoff. A continuous outage has a 120-second
grace; advancing the scanned height starts a fresh grace. Loopback retries wake
when the port becomes reachable. After the grace, the desktop catch-up loop
can start another attempt.

Block streams have inactivity limits of 45, 90, or 120 seconds, not a total
request deadline. Older Zaino streams can end after roughly 180 seconds; keep
a valid delivered prefix and request only the remainder. Tree-state and root
RPCs use a separate connection. Each sync uses one sampled tip; a later
load-balanced response below it does not invalidate completed scanning.

Pending raw transactions are resubmitted while unmined and unexpired. An
uncertain submission stays pending; the user must not construct a second send.
Native regtest can fall back to the light server when the validator refuses the
connection before bytes leave.

Wipe/rescan writes a replacement SQLite database and renames it over the old
one. Wallet metadata and encrypted seed files use temporary-file/fsync/rename
replacement. Create and restore refuse an existing wallet folder or recoverable
backup rather than overwriting its seed. A database with no wallet metadata is
an unfinished create and is set aside. The desktop displays new recovery words
before the first scan.

A native wallet's first remote scan is capped at 150,000 blocks unless explicitly
allowed with `Z_STACK_ALLOW_DEEP_SYNC=1`. Existing native wallets can catch up
across longer gaps. Browser deep-sync policy is a separate SDK option. Regtest
defaults to NU6.3 at 1,000,000; the native fixture overrides it to 150, and every
engine instance must use the fixture's actual activation schedule.

A finished native scan waits at most two seconds for roots still in flight.
Regtest skips Ironwood subtree requests; an unsupported server response becomes
an empty root stream. `z-wallet verify-trees` compares roots and spendable-note
witnesses with `GetTreeState` without modifying the wallet. Quiet catch-up clips
ranges already scanned by SQLite.

### Memo retrieval and privacy

Native enhancement processes a bounded transaction-request list (up to 24 per
sync). Browser memo modes and shared ranges are described in [the SDK guide](SDK.md).
Compact blocks do not carry full 512-byte memos. Fetching a wallet transaction
by ID discloses that ID to the server. Tor/SOCKS is not wired into the SDK or
native sync; public servers see IP addresses and timing. Prefer a user-run node
when that fits the application threat model.

## Sparse persistence and spend anchors

Historical trials share immutable viewing keys and nullifiers. Keys initialize
only when shielded work appears; successful wallet-active commits refresh the
nullifiers. Each batch carries the exact identities it read. Persistence compares
them with current database state inside the write transaction before publishing
results, so another wallet connection cannot silently invalidate the cache.
Mismatch or a SQLite snapshot-upgrade conflict asks for a fresh sync. Deferred
tree flushes also check the committed identity and retained block metadata.
Note batches capture real prior frontiers for contiguous wallet row runs during
one commitment walk; persistence reuses those boundaries instead of hashing again.
The shared offload accumulator emits only the consumer's representation: typed
runs for native SQLite or encoded leaves for web snapshots. This prevents unused
duplicate outputs accumulating across batches and being copied by native's
transactional candidate clones. Output selection does not change shard retention.

A sparse scan still needs current checkpoints in every pool. Zakura selects a
common Sapling/Orchard anchor, so an empty Sapling tree must advance alongside an
Orchard receipt. After flushing retained leaves, the engine installs checkpoints
at the exact sizes from recent compact metadata and persists a bounded
100-block lookback plus the current height. Historic blocks remain sparse. Future downloaded subtree roots
must never determine the checkpoint position: root coverage is checked only for
the actual block's prefix. The all-pool operation is transactional, preserves
existing removed-mark metadata and uses upstream checkpoint pruning.

Recent empty heights may refer to a prefix whose last commitment is older than
that window. Before inserting more leaves, retain each distinct recent prefix
at its latest verified compact height. This protects boundaries in the new batch,
buffered shards and SQLite before compaction can merge them into a larger node.
Marked/reference flags survive retagging; stored boundaries require exact-prefix
coverage. Retention uses the bounded recent window, not a walk of old history.

Already-scanned wallets from older builds recover checkpoints from their stored
block sizes. This cannot invent missing heights or tree nodes; repeated successful
recovery performs no writes. A checkpoint at the same position as an earlier
height is valid when no commitments were appended.

## Frontier-skip live gate

Before further scanner optimization, run the native restore/wipe/spend regression:

```bash
pnpm regtest:up
Z_STACK_REGTEST=1 cargo test -p z-engine --release --features native --test regtest frontier_skip_restore_wipe_spend -- --ignored --exact --nocapture --test-threads=1
```

The test uses temporary wallets with encrypted seeds. It mines unrelated Orchard
commitments after the recipient's birthday, waits over 9,000 heights before its
first note, checks sparse persistence, restores from seed, wipes from birthday,
checks rewind with no stored block row, rescans, and requires a mined Orchard
spend received by the faucet. A second cold restore shifts the first note across
the 256-block streaming boundary, recovers a receipt and spend in the same batch,
and spends the recovered change. Unrelated shielded suffix transactions exercise
the ordered note tail. It advances the local regtest chain: run these tests
serially. On an existing chain, a fresh recipient starts 9,000 heights back so
the gate can reuse history while retaining the full required interval. The
optional `Z_STACK_REGTEST_FAUCET_DIR` reuses an encrypted faucet wallet under the
system temporary directory; recipient and restored wallets always remain fresh.
`pnpm test:regtest` uses release mode and includes this gate.

To retain a disposable encrypted replay fixture, set
`Z_STACK_REGTEST_REPLAY_DIR=/tmp/z-stack-frontier-replay` on the first command.
The directory must not exist and must be under the system temporary directory.
It contains only encrypted `seed.enc` and nonsecret fixture metadata. Subsequent
changes can repeat restore, wipe/rescan, confirmed spend and independent cold
recovery without mining the long interval again:

```bash
Z_STACK_REGTEST=1 Z_STACK_REGTEST_REPLAY_DIR=/tmp/z-stack-frontier-replay \
  cargo test -p z-engine --release --features native --test regtest \
  frontier_replay_restore_wipe_spend -- --ignored --exact --nocapture --test-threads=1
```

Each replay consumes a small amount of the disposable recipient's regtest balance.
The test refuses a chain behind the saved fixture. Without a replay directory,
the replay-only test skips; the original long-gap gate still runs normally.

The basic `create_sync_shield_send` test additionally restores its recipient from
birthday 1. It requires shielding on an unfunded chain, reuses already-confirmed
Orchard funds on later runs, and always proves, broadcasts and confirms an exact
50,000-zatoshi payment. Set `Z_STACK_REGTEST_KEEP_RECIPIENT=1` only when diagnosing
a failure to retain its disposable encrypted wallet in the system temporary
directory. The default removes that fixture on completion or failure.


## Web / WASM

See [`WEB.md`](WEB.md). Trial-decrypt compact blocks in `z-engine::web` (`scan_block`). A batch blob is decrypted on Rayon (wasm: `initThreadPool` when SAB/COOP/COEP, including isolated iOS Safari). Persist a versioned snapshot to IndexedDB and resume from scanned height. Compact-block **download** may use the loopback `/lwd/*` pipe or gRPC-Web; the wallet never runs in that proxy. WASM `GetSubtreeRoots` is incremental (`startIndex`) like native; gRPC-Web implements the same RPC. Web reload paints the last snapshot from IndexedDB immediately and skips subtree-root fetch when trees are already in the snapshot. The Zaino pipe serves `/lwd/mempool` and `POST /lwd/sendraw` via loopback validator RPC (empty mempool if none). Bring-your-own Zaino: `z-wallet pipe --zaino <url> --network mainnet|testnet|regtest [--rpc <zakura-or-zebra>]`. Desktop skips the pipe.

After finalize, a transient pool mask records which trees were actually built.
Unused pools continue deferred retention during catch-up; their first owned note
invalidates live trees and rebuilds the retained siblings before proving. Used
pools continue hashing incrementally. Successful rebuild publishes the mask;
reset, rewind, hydration and failed updates cannot reuse an invalid tree. The
snapshot format is unchanged, including selective shard scanning compaction of root-covered
unmarked shards.

Snapshot capture stays on the owning scan worker. IndexedDB commits a unique
payload and its small preview manifest atomically. Reload can paint the manifest
before reading the full payload. Saves coalesce to one active and one replaceable
pending request; large payloads use Blob to reduce main-thread copying. Successful
blank memo enhancement is persisted so reload and caught-up sync do not refetch it.

Do not use crates.io `zcash_client_memory` (wrong orchard). Wait for `zakura-client-memory` to replace the snapshot store.

History is the same `HistoryEntry` / `v_transactions` columns as native. WASM fills what compact scan can see; `feeZat` / `isShielding` wait on enhance + transparent scan.

## Storage

- Desktop: `zakura-client-sqlite` (`data.sqlite`)
- Seed: encrypted `seed.enc` (passphrase) and/or Windows Credential Manager — never `mnemonic.txt`
- Compact blocks: `wallet-data/blocks/{start}-{end}.pack` (`FsBlockCache`; legacy `{height}.pb` still read)
- Web: IndexedDB snapshot (`zstk1`) + optional OPFS compact-block cache

## CLI

```bash
cargo run -p z-engine --features native,cli -- tip
cargo run -p z-engine --features native,cli -- --passphrase '…' create --wallet ./wallet-data
# Windows Hello / Credential Locker:
cargo run -p z-engine --features native,cli -- --windows-credential create --wallet ./wallet-data
cargo run -p z-engine --features native,cli -- sync --wallet ./wallet-data
# Wipe notes/trees/history + compact-block cache; keys and birthday stay:
cargo run -p z-engine --features native,cli -- reset-scan --wallet ./wallet-data
cargo run -p z-engine --features native,cli -- reset-scan --and-sync --wallet ./wallet-data
# WASM lab only (loopback). Desktop talks native gRPC to Zaino and skips this:
z-wallet pipe --zaino http://127.0.0.1:8138 --network mainnet --rpc http://127.0.0.1:8232
```
