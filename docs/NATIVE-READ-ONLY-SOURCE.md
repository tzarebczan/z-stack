# Native read-only public-data source

`z_engine::native::NativeReadOnlySource` accepts a caller-provided
`tonic::transport::Channel` and explicit `Network`. The application authenticates
its source, verifies publication freshness and network/genesis identity, and
persists the accepted tip and retained anchor before exposing responses through
that channel. The SDK does not authenticate a channel constructor by itself.

Use `NativeWallet::create_with_source` or `restore_with_source` to create wallet
state with this capability. They reuse the ordinary native seed encryption,
account derivation and SQLite initialization. `create_with_source` takes an
optional birthday; restore requires a birthday. Both check the source's reported
network, tip and birthday tree through the supplied channel.

`NativeWallet::open_with_source` explicitly enrolls an existing ordinary wallet
under its exclusive database lease. A wallet with pending verified recovery
cannot enter the ordinary scanner. Enrollment adds a singleton
`ext_native_source_policy_v1` table without resetting wallet data or receipts.
A strict versioned `native-source-policy` sidecar protects interrupted seed-only
setup before a database exists. Any retained database
refuses source setup rather than being treated as an unfinished ordinary wallet. A
seed-only create retry authenticates and reuses the retained encrypted seed;
seed-only restore must match it before making any source request. Setup cannot
replace an existing source seed or quarantine a retained source database.
`native-source-setup.json` binds network, birthday and account before creating a
source seed. An automatic birthday uses the authenticated tip once, and retries
retain that birthday even if the tip advances. Changed setup scope, malformed
binding, or a retained seed without its binding refuses before source access. Either marker keeps the wallet read-only. A
copied marked database without its sidecar remains read-only; malformed or
non-singleton markers refuse. There is no source-to-direct handover API.
`uses_read_only_source` re-reads both markers under a shared lease without
networking. Applications must refuse replacement enrollment if a marked wallet
lacks its original authenticated checkpoint or publisher binding. Those
application-specific records do not belong in the SDK capability marker.

Call `sync_with_source_reported_cancellable` with that source, optional shared
progress and the owning session's `Arc<AtomicBool>` cancellation flag. Every
fetch and control worker clones the supplied channel. Retry waits and stall
progress do not probe stored endpoints. Dropping the scan future preserves
completed SQLite batches; cancellation wins over a concurrently ready result.
The application must also discard callbacks from a closed wallet session.

The transport admits only these existing lightwallet RPC methods:

- `GetLightdInfo(Empty) -> LightdInfo`
- `GetLatestBlock(ChainSpec) -> BlockId`
- `GetTreeState(BlockId) -> TreeState`
- `GetBlock(BlockId) -> CompactBlock`, for validated tree lookup compatibility
- `GetBlockRange(BlockRange) -> stream CompactBlock`
- `GetSubtreeRoots(GetSubtreeRootsArg) -> stream SubtreeRoot`

Range bounds are inclusive. The scanner sends `pool_types=[]`; an application
adapter can split a large range into its bounded authenticated requests while
streaming each original protobuf in order. Subtree pages request two entries for
Sapling, Orchard and Ironwood (enum values 0, 1 and 2). Tree states retain all
three separate frontiers. Source mode does not fabricate an empty Ironwood
response when a server rejects that pool.

Source scans omit transparent address refresh, transaction-ID memo enhancement,
funding classification and saved-send rebroadcast. The current native compact
scanner handles shielded pools; it does not import transparent outputs or
reconcile transparent spends from these compact ranges. Fresh transparent
balances are therefore unsupported, and retained transparent balances must not
be presented as verified by a source scan. Remote lookups, ordinary sync,
send, payment preparation, signing, submission and maximum-send selection refuse
with `native_read_only_source`. Cached viewing history and addresses remain
available. Cold `open`, `open_offline`, stored endpoint edits and handles opened
before enrollment retain the restriction. Ordinary wallets without either marker
keep their existing network and offline signing policy; the Mainnet send gate is
unchanged.

The source is a read-only capability, not an independent consensus verifier or a
private-send implementation. Applications must qualify their transport and
publications against the networks and node schedules they support. The SDK tests
use a memory-only HTTP/2 source and disposable public-vector wallets, with no TCP
listener, production wallet or production node.
