# Private Ironwood enhancement

The optional Enhance v7 integration retrieves Ironwood ciphertext suffixes without sending a selected transaction ID to a server. Rust creates each PIR query, decodes its reply and authenticates the resulting note before changing wallet state. The JavaScript host transports public routing metadata and encrypted query/reply bytes.

This integration matches wallet-pir [`24f7694cd4f8274172a11cbff43c158bcab5d500`](https://github.com/valargroup/wallet-pir/tree/24f7694cd4f8274172a11cbff43c158bcab5d500), with its default v7/q48 profile and schema 11. It does not support the experimental v9 profile. A service must use the matching profile; there is no public endpoint fallback.

## Browser transport

Set `BlockTransport.enhance` to a `PrivateEnhanceTransport`. Its `exchange(request, signal)` method accepts only these requests:

| Request | Host-visible fields | Maximum accepted response |
| --- | --- | --- |
| `init` | None | 1 MiB manifest |
| `session` | Canonical 64-character lowercase hexadecimal `sessionId` | 256 KiB public material |
| `query` | Rust query `body`, historical `anchor.height` and display-order `anchor.hash` | 30,836 bytes |

The result contains `body: Uint8Array` and status `200`, `409`, `410`, `429` or `503`. A refresh status (`409` or `410`) retires the current pass. Other non-success statuses fail the pass. There is no automatic retry. The application supplies OHTTP, authenticated provider identity, consent, endpoint restrictions and response padding; the SDK does not infer these guarantees from a transport label.

`SDK_TRANSPORT_CAPABILITIES.enhanceV7 === true` identifies this API. The wallet sync host calls `syncPrivateEnhance` after compact scanning and before the public memo phase. A pass processes at most 24 queued actions and checkpoints after each accepted reply. It performs no provider I/O when no work is queued. Configure a complete public memo range source separately when Sapling, Orchard or mixed transactions require it. That range remains complete, including Ironwood transactions; combining it with Enhance can fetch redundant data.

The production WASM host exposes one closed `privateEnhance` operation: `pending`, `begin`, `material`, `prepare`, `finish`, `anchor` or `cancel`. Positions, action identities, query secrets and decoded records stay inside Rust. Standalone `protocol-bindings` are qualification tools, not the production wallet API.

## Native API

Enable the `z-engine` feature `private-enhance`. Use an enrolled `NativeWalletSource` with the same exact source binding as the wallet:

1. Fetch a manifest through the approved transport. Call `begin_private_enhance_with_source(source, manifest, now_seconds)`.
2. Fetch each public identifier from `NativeEnhancePass::session_ids()` and call `add_session(material, now_seconds)`.
3. Call `prepare_private_enhance_with_source(source, pass, now_seconds)`. An empty vector ends the pass. Otherwise send its ciphertext with the public historical anchor returned by `pass.anchor()`.
4. Call `apply_private_enhance_with_source(source, pass, reply, now_seconds)` and persist the application's normal checkpoint. Repeat within the 24-action bound.
5. Drop the pass or call `cancel()` on cancellation, lock or failure.

The pass retains a shared database lease. Every preparation and application checks the wallet's current sticky source binding and independently scanned historical anchor. The lease prevents concurrent reset or source enrollment. The caller still owns operation cancellation and must prevent an obsolete wallet session from starting new work.

## Native public memo buckets

`begin_shared_memos_with_source(source)` creates a separate `NativeSharedMemosPass` under the same exact source binding and a retained database lease. It selects at most two common ten-block buckets from durable `Public` enhancement work. `PrivateIronwood` routing remains enabled while planning and applying, so private work cannot become a public request.

`pass.ranges()` returns at most 20 one-block ranges, ordered across those buckets. Fetch every range through the closed public memo capability and call `apply_shared_memos_with_source(source, pass, bundle_bytes)`. Each strict JSON bundle contains `start`, `end`, hexadecimal original u32 big-endian framed `blocks`, and ordered hexadecimal `transactions` for every shielded transaction in that block. No selected transaction ID leaves Rust.

The pass validates complete coverage, retained chain identities, links between blocks, transaction ordering, all raw transaction IDs and absence of trailing bytes. A bundle cannot omit a locally known shielded transaction from both its compact list and its raw list. Prefix blocks before the wallet birthday are permitted; the complete bucket must connect to its retained scanned anchors. The accepted JSON byte total is bounded to 64 MiB per pass.

Each attempted range is consumed. The wallet buffers validated records and atomically applies only currently `Public` matches after the whole bucket arrives. Partial buckets, cancellation and malformed responses do not update wallet memos. A complete public response can contain private Ironwood transactions, but this path does not decrypt, persist or reroute them. Dropping or cancelling the pass releases ownership. These methods perform no network requests.

## Validation and privacy limits

The accepted historical anchor includes height, block hash and Ironwood tree size. It must match independently scanned wallet state; the indexer's anchor can lag the current tip. The wire header stores the hash in display order, without reversal. A manifest cannot authorize its own chain history.

A query is at most 282,740 bytes. Replies must be exactly 30,836 bytes and decode to a 653-byte record. Each query uses fresh Rust randomness and has one response context. A successful reply, malformed reply, expiry or cancellation consumes that context. Routing material is reaccepted only within the client freshness window.

The wallet rechecks the retained compact action, mined identity, commitment and authenticated decryption before applying a record. Indexer flags, fee and expiry are assertions, not authority to request a selected transaction or change consensus spendability. In the web wallet, authenticated incoming change completes independently of external outgoing recovery. Its undecryptable outgoing padding retires only when linked spent value equals authenticated incoming value plus recovered external outputs plus the nonnegative value-balance fee derived from canonical pure-Ironwood transaction bytes. Transparent, Sapling, Orchard and Sprout bundles cannot grant this pool-local eligibility. For a locally saved send, the builder's total fee must also equal that pure-Ironwood fee; a mismatch remains unresolved. Retirement runs both after a PIR application and when full bytes or locally saved send evidence arrives later; suspended actions do not require another query. The durable `ironwood_fee_from_full_transaction` amount records that narrower provenance independently of general history fee metadata; a total builder fee or PIR assertion never grants it. Full-raw decryption resolves only action indices whose outputs it actually authenticated. If this wallet spent no notes and every locally known incoming Ironwood output was authenticated, its incoming memo work is complete even with unrecoverable foreign actions; those actions are not certified as padding. A linked wallet spend still requires the value-balance rule, so a hidden positive outgoing output remains pending. A web cold restore without that fee evidence can retain unresolved padding after recovering its memos. The native wrapper keeps incoming-only work eligible. It refuses every linked wallet-spend query unless the database already contains canonical pure-Ironwood full transaction bytes with a matching independently derived fee. It retains that evidence classification through the pass and rechecks it before query preparation and application; a record with a different fee is rejected. Restored spends without this evidence remain queued and pending. The wrapper does not change the pinned SQLite backend's value-balance rule or claim complete retrospective outgoing history. A positive transparent flag is refused before a native database mutation. Missing public data requires a complete ordinary public range; it never enables a selected transaction ID or plaintext address fallback. This API currently supports Mainnet pure-Ironwood enhancement, not transparent PIR or a complete memo replacement for every pool.

The server sees domain/shard and session routing metadata, request timing and query count. PIR hides the selected row and slot within that domain. OHTTP can hide the client's IP from the origin when relay and origin operators are independent; it does not remove these routing disclosures.

## Reproduction and evidence

- `node tools/check-enhance-upstream.mjs` reverses the documented HTTP-wrapper patch in a disposable directory and checks every recorded upstream source digest. No PIR math or parameter patch is applied.
- `cargo test --locked -p z-enhance-pir --test interop` checks actual native q48 query/reply interoperability and one-use contexts.
- `cargo test --locked -p z-engine --lib --features native-pir,private-enhance` includes native SQLite authenticated application and lease exclusion, plus web-store authenticated note and cancellation tests.
- Build ST and MT with the repository's standard WASM scripts. `node tools/enhance-wallet-interop.mjs` exercises the production wallet bindings and SDK driver in Chromium and Firefox using a disposable local server.

The local browser fixture uses a public watch-only account, a genuinely encrypted synthetic note and a 4,096-row native q48 server. It proves authenticated wallet application, persistence, cancellation and ST/MT interoperability. It does not establish consensus Mainnet history, all supported database geometries, production controller ingestion, remote CORS, load capacity or service deployment readiness. Those require separate application and operator qualification.
