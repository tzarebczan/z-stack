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

## Gateway protocol v1

Run the local development adapter against native gRPC:

```sh
node scripts/grpc-web-proxy.mjs --upstream http://127.0.0.1:28137 --port 28138
```

`GET /zstack/memos?start=H&end=J` accepts inclusive public ranges of 1–10 blocks.
No other query parameters or caller identities are forwarded upstream. Normal
SDK pages end on ten-block boundaries; the first birthday and trailing tip
pages can be shorter. The response is JSON:

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
