# Public range retrieval

The hosted browser can discover transparent deposits and read shielded payment
memos without submitting its address or its matched transaction IDs. This is
shared retrieval, not PIR, IP anonymity, or a new Zcash consensus protocol.

## Browser integration

```ts
const wallet = await createWallet({
  network: "mainnet",
  server: "https://your-gateway.example",
  transparentScan: "compact",
  memoFetch: "shared",
  sharedMemos: true, // This gateway implements /zstack/memos.
});
```

These modes are opt-in for SDK integrators. The consumer application defaults to compact
deposit scanning and automatic individual memo retrieval. Shared memo ranges
remain an Advanced choice; previously saved shared preferences are retained.
Individual retrieval sends locally matched transaction IDs to the server. `setTransparentScan("compact" | "off")` and
`setMemoFetch("shared" | "auto" | "on-demand")` affect subsequent work.
`fetchMemos()` in shared mode retrieves the next public batch; the other modes
expose up to 500 wallet transaction IDs per invocation, with eight requests
in flight and periodic durable checkpoints. `memoFetchStatus` reports pending,
unavailable or completed work so auto-sync retries even at an unchanged tip.

The server capability is separate from the memo privacy choice. URL transports
default `sharedMemos` to false, including loopback servers. Enable it only for a
gateway that implements the extension with CORS for the wallet's origin;
`GetLightdInfo` does not advertise it. The same option is accepted by
`lightServer` and `grpcWebTransport`. A custom `BlockTransport` declares support
by providing its `sharedMemos` method. Without support, shared mode reports
`unsupported` and makes no custom-route or individual-transaction request.
Definitive HTTP 403/404/405/501 responses are remembered for that transport
instance. Network/CORS failures and temporary server errors remain retryable;
they never switch retrieval modes or follow a redirect.

Transparent scanning requires GetLightdInfo protocol version >= v0.5.0 and an
adapter that forwards pool selection. The SDK requests pools 1,2,3,4. Rust
matches scripts derived from issued external/internal receivers plus a 32-index
gap, records outputs and spends, and enforces coinbase maturity even on regtest.
Missing coinbase data is rejected as incomplete coverage. Derivation is bounded
at index 10,000; this is not an unbounded HD account discovery algorithm.
The native loopback pipe forwards `allPools=1`; native wallet sync defaults are
unchanged. The Node gateway supports both public ranges and gRPC-Web.

Coverage is persisted separately for shielded scanning, transparent scanning and
memos. Older snapshots backfill from birthday. While compact transparent
coverage is incomplete, its outputs cannot be used for shielding. Reorgs rewind
coverage with the wallet. Switching to an explicit address lookup discards
compact coverage so switching back replays potentially missed spends.

When deposit coverage is aligned with shielded scanning, a negotiated all-pool
Zaino response feeds both Rust scanners before the normal snapshot checkpoint.
This avoids a second history download on fresh restores and ordinary tip sync.
Combined scans cap ranges at 500 blocks and four requests in flight; dense
all-pool responses otherwise queue long enough to hit browser deadlines.
Existing snapshots with a deposit gap backfill at most 20,000 transparent blocks
per sync in 1,000-block pages, checkpointing every 5,000 blocks and at completion.
Shared memo retrieval still backfills at most 100 memo blocks per sync. Auto-sync continues while coverage is catching up,
even when no new chain block appears. Restoring a very old wallet may require
many passes and substantial bandwidth. Shielded funds remain usable while
memos backfill. Coverage/status fields on WalletSnapshot distinguish complete,
scanning, unsupported and temporarily unavailable; an unsupported service never
causes an automatic address/transaction-ID fallback.

## SDK transport bounds

Compact HTTP and gRPC-Web responses are read incrementally. The SDK refuses a
response above 32 MiB, a compact/data frame above 4 MiB, or a gRPC trailer above
64 KiB. Content-Length is checked before reading, and actual streamed bytes are
counted independently, including when that header is absent or incorrect. The
4 MiB frame ceiling allows protobuf overhead beyond a serialized block; these
are SDK resource limits, not new consensus limits or an exact heap-size bound.

Each range must contain exactly its inclusive requested block count. Individual
responses are limited to 8,000 data frames; normal shielded pages of 1,000 and
combined pages of 500 remain within that ceiling. The loopback HTTP transport
can page larger requests, but its combined return is still limited to 32 MiB.
Unary gRPC calls accept at most one data message; subtree responses accept at
most the requested count (or 8,000 when no count is supplied). Trailers must be
final and appear at most once.

Scan prefetch also enforces a 256 MiB aggregate reservation for in-flight,
ready and currently applied compact ranges. Each in-flight request reserves
64 MiB for a maximum-size response and parsing/conversion storage; completed
buffers reserve their actual backing-store bytes. The applied range remains
counted until the sequential consumer asks for the next one. Larger queue
settings therefore permit at most four initial requests. Small completed ranges
free reservations for subsequent requests; memory pressure pauses launches
instead of discarding valid results. This trades some initial concurrency for
bounded staging, including while birthday-tree setup delays Rust application.

The 32 MiB ceiling leaves room above the approximately 14 MiB per 1,000 blocks
that motivated the existing 500-block all-pool tuning, but is not a guarantee
for every dense range. Oversized ranges require an explicit smaller request.
These reservations bound SDK-owned staging; they are not a promise about total
browser, WASM, network-stack or garbage-collector memory. Custom transports must
also respect the range ceiling; an oversized returned backing buffer is refused
and aborts the prefetch siblings before any result is handed to Rust.

A limit violation cancels the reader/request and is not treated as a transient
outage. No automatic smaller-range retry, alternate transaction-hash probe,
selective lookup or retrieval-mode fallback follows it. Integrators can request
a smaller range explicitly. gRPC deadlines and the HTTP block-download idle
watchdog remain in effect; continued HTTP byte progress has no total deadline.

The internal consumeDelimitedStream helper is not an installed-package entry
point. It uses the same byte/frame ceilings and an 8,000-block ceiling. It awaits each page callback before continuing.
Already completed callbacks remain applied if a later frame fails; buffered
unapplied pages are discarded. Full-body range methods return no bytes until
the complete response passes transport checks. Rust still validates protobuf
contents, block heights, hashes and wallet effects before advancing coverage.

## Native accepted mainnet windows

The native Rust API `z_engine::native::verify_mainnet_block_window` validates a
fixed window without opening a wallet, modifying SQLite, querying a server or
providing transactions for signing/broadcast. It is separate from regtest
canonical recovery and does not enable mainnet Private Cloud wallet recovery.

Construct `AcceptedMainnetWindow::from_local_node` with canonical mainnet
genesis, a lower-case SHA256 source-policy/context digest and exactly seventeen
ordered `(height, display_hash)` entries: one predecessor and sixteen blocks.
The first block must be at height `1 mod 16`; checked arithmetic bounds the end.
The caller must obtain hashes from its accepted node independently of candidate
publication metadata, and recheck that snapshot before accepting the result.
The constructor validates shape and genesis, not the node's trustworthiness.

The candidate frame contains ASCII `COFFER-MAINNET-BLOCK-WINDOW-V1\n`, a little-endian
u32 start and count of sixteen, then sixteen little-endian u32 lengths and full
serialized blocks. Each block is limited to 2,000,000 bytes. Extra bytes,
missing/extra blocks and noncanonical serialization are refused. Mainnet
height-derived transaction branches, accepted header hashes, predecessor links
and transaction-ID Merkle roots must match. Parsing retains one block at a time;
cancellation discards the result without wallet changes.

The opaque result exposes the range, predecessor/target hashes, source identity,
accepted identity, candidate SHA256 and parser policy
`z-stack-mainnet-accepted-block-window-v1`. The accepted identity hashes that
policy's ASCII bytes, NUL, canonical genesis ASCII, NUL, source digest ASCII,
then seventeen concatenated little-endian u32 heights and decoded 32-byte
display-order hashes. Hash text case is normalized; the candidate digest covers
the exact complete frame. Executable/source provenance remains the caller's
responsibility.

This verifies transaction-effect/txid inclusion against the supplied accepted
headers. It does not validate consensus, ZIP244 authorizing-data commitments,
shielded tree state, wallet coverage or production transport privacy. It does not
relax regtest recovery or application spending gates. This API is native Rust
only and adds no installed TypeScript SDK export.

## Gateway protocol v1

Run the local development adapter against native gRPC:

```sh
node scripts/grpc-web-proxy.mjs --upstream http://127.0.0.1:28137 --port 28138
```

`GET /zstack/memos?start=H&end=J` accepts inclusive public ranges of 1–10 blocks.
No other query parameters or caller identities are forwarded upstream. SDK
requests never cross the common ten-block boundaries; the first birthday and
trailing tip pages can be shorter. A custom transport's
`readonly maxSharedMemoBlocksPerRequest` can reduce each request to 1 through 10
blocks. The SDK validates and captures it before owner or storage setup, with a
default of 10. Smaller requests preserve the common boundaries and checkpoint
every completed range. They do not change the response byte limit or permit a
selective fallback. Check
`SDK_TRANSPORT_CAPABILITIES.maxSharedMemoBlocksPerRequest === true` on the root
SDK export before relying on the hint from a separately supplied archive.
The response is JSON:

```json
{"start":1,"end":9,"blocks":"<hex u32-BE-length-delimited CompactBlocks>","transactions":["<raw tx hex>"]}
```

Transactions include **every shielded transaction** in block/index order,
including unrelated payments. Sapling spends/outputs, Orchard actions and
Ironwood actions all count. The proxy obtains these by public-range fan-out to
Zaino's GetTransaction. Zaino sees this public fan-out, not a browser's list of
matched transactions. Transparent-only transactions need no memo payload.

Rust validates contiguous coverage, available stored block hashes, transaction
ordering/identifiers and complete correspondence to the compact list. Known
shielded wallet transactions cannot be omitted. Every raw transaction is parsed
and its cryptographic txid checked before wallet mutation; only locally matched
transactions are enhanced/decrypted. This retains compact sync's trust in the
light server for the chain; it is not independent header/consensus verification.

This initial format sends full transactions, including proofs, as hex JSON.
It deliberately avoids inventing a ciphertext/proof-stripping format. A smaller
format needs its own Rust verification design. Responses are bounded to 64 MiB
at the SDK/engine, upstream data to 32 MiB, concurrent uncached jobs to four,
cache bytes to 64 MiB and cache lifetime to ten seconds. A large or unavailable
range fails without advancing its cursor; no selective fallback occurs.
The public cache has no per-wallet keys. Disconnects/timeouts cancel upstream
work. Failed ranges are retryable on a later sync/manual request.

This endpoint must be installed at the configured browser gateway; an ordinary
LWD, the native JSON pipe or the old WARP experiment does not gain it merely by
updating the wallet. The adapter binds to localhost. Public deployment still
needs deliberate capacity/admission, TLS and ingress/logging configuration.
No production deployment or attestation is included in this change.

## Evidence and cost

A live mainnet Zaino sample, blocks 3,496,900–3,496,909, returned 30 shielded
transactions. Delimited compact data was 14,337 bytes; raw transactions were
305,042 bytes; the complete hex-JSON response was 638,908 bytes (HTTP headers
excluded). This is a small measured sample, not a daily/mobile cost estimate.

The regtest acceptance script `scripts/sdk-public-data-regtest.mjs` checks
address-free detection, mature coinbase shielding, send/mining, fresh restore
and memo recovery while rejecting every browser GetTransaction/GetAddress/
GetTaddress call. Run after `pnpm regtest:native:up` and a WASM build with
`Z_STACK_REGTEST_NU6_3=150 node --conditions=@z-stack/source --import tsx scripts/sdk-public-data-regtest.mjs`.

## What this does not hide

Servers still see IP addresses, requested ranges, birthday/catch-up patterns,
request sizes and timing. Broadcasts, name resolution, account login, backups,
swaps and chat are separate disclosure surfaces. Shared ranges reduce selective
query leakage but do not hide transparent on-chain activity. A malicious or
incomplete light server can still deny data. Memo text, viewing keys and seeds
remain local. Enabling compatibility lookups intentionally restores selective
query disclosure.
