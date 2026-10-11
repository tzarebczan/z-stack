# Native accepted Mainnet compact windows

The native Rust API `derive_mainnet_compact_window` derives sixteen compact blocks
from full blocks accepted against a caller's local-node snapshot. It performs no
I/O, opens no wallet, and changes no wallet or publisher state. This experimental
API is the first bounded derivation step toward a public history prefill. It does
not qualify a production private-cloud service or a full Mainnet archive.

The caller supplies the same candidate and `AcceptedMainnetWindow` used by
`verify_mainnet_block_window`, an immutable `MainnetCompactCheckpoint`, and an
`AtomicBool` cancellation flag. The complete candidate passes the existing
verifier before the compiler reparses those same immutable bytes. The verifier
checks accepted header identities, branch/version rules, canonical serialization
and transaction-ID Merkle inclusion. Its trust authority remains the caller's
accepted local node. It does not independently validate consensus, proof of work,
or ZIP244 authorizing-data commitments.

`MainnetCompactCheckpoint::canonical_genesis()` creates the empty shielded prefix
at canonical Mainnet genesis. The only other public source of a checkpoint is a
successful window's `successor_checkpoint()`. Its fields are private. Clone
preserves the same prefix state. The next window must begin at predecessor+1 and
name the exact predecessor hash. There is no arbitrary-height or arbitrary-size
constructor, checkpoint deserialization, or resume-from-file API.

The policy is `z-stack-mainnet-accepted-compact-window-v1`. It includes every
original transaction index and protocol-order txid, Sapling spends and outputs,
separate Orchard and Ironwood actions, and transparent inputs and outputs.
Coinbase null inputs are omitted. Transparent previous-output IDs remain in
protocol order. Output values and scripts come directly from the accepted native
transaction. Fees are unset because this API does not resolve previous outputs.
Block headers are empty. Human block-hash identities use display order; protobuf
block hashes, previous hashes and transaction IDs use protocol order.

The pinned compact schema has no Sprout fields. Retaining old heights and their
transparent effects does not add Sprout wallet scanning. The pinned Mainnet
schedule activates Sapling at 419200, NU5 at 1687104 and NU6.3 at 3428143. V6 and
Ironwood are valid from NU6.3. Mainnet NU7 is a separate unset activation in this
pin. Pool data follows the pinned native parser, not an inferred schedule.

Each checkpoint retains separate Sapling, Orchard and Ironwood frontiers. The
compiler appends every accepted commitment in transaction order. Every block's
`ChainMetadata` contains cumulative sizes from that prefix, with checked uint32
conversion and frontier capacity. Refusal, budget exhaustion or cancellation
leaves the input checkpoint unchanged and returns no partial window.

`VerifiedMainnetCompactWindow` exposes its existing block-window evidence,
derivation policy, predecessor checkpoint identity, length-delimited
`CompactBlock` bytes, payload SHA256 and opaque successor checkpoint. Checkpoint
identities bind the policy, canonical genesis, height, display-order block hash,
and the three sizes and roots. These identities describe accepted effects; they
are not independent chain authentication or a durable publication manifest.
The API does not expose full transactions for replay, signing or broadcast.

The existing candidate limit remains 2,000,000 bytes per full block and sixteen
blocks per window. Compact output is bounded to 4 MiB per protobuf message and
sixteen times 4 MiB plus five length bytes per message for the whole payload. Those
are policy budgets, not measured final archive sizes. The compiler retains one
parsed block at a time and one bounded window of derived output.

Windows start at height 1 modulo 16. There is no partial current-tip window or
compact genesis message. The genesis checkpoint is empty prefix state, not a
solution to the pinned full-block parser's separate genesis limitation. Disk
windows, checkpoint restoration, a catalog, immutable serving, hot-tail and
reorg publication, and native wallet recovery integration remain later units.
Provider independence, retention, enrollment, privacy, consensus/PoW, ZIP244
validation and Mainnet spending retain their separate qualification gates.

The focused native tests use synthetic accepted hashes and transactions with
dummy authorizing data, plus a maintained public Sapling parser fixture. Private
fixture checkpoints exercise nonzero cumulative state and upgrade boundaries;
they are not public checkpoint import APIs or actual Mainnet qualification.
Run them with:

```sh
cargo +1.91.0 test --locked -p z-engine --features native --lib native::compact_prefill -- --test-threads=2
```

The existing accepted-window tests must remain green. SDK distribution still
requires the native and both WASM build checks in [the release guide](RELEASE.md).
