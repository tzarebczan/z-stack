# Transparent acquisition source provenance

The production acquisition code uses wallet-pir commit
`c9262e8431b4caa7cf2e2c55edbf0084c1f2ddb2`, the SDK's existing compatible v12 pin.
Native ReinspiRING, record codecs, filters, tags and manifests remain dependencies
at that commit. This crate implements no cryptographic arithmetic.

Six private modules adapt `transparent/crates/transparent-wallet/src`:
`adapters.rs`, `client.rs`, `store.rs`, `memory_store.rs`, `sync.rs` and
`sync_ahead.rs`. Their original unit-test sections are omitted. The patch adds
async functions and await points, replaces synchronous transport traits with
local async traits, and changes the refusal closure to `AsyncFnMut`. It retains
the upstream directory/page walk, content-failure dummy requests, failure caps,
revision/re-cut rules and coverage checks. The initial driver permits one
outstanding exchange. The unused one-shot balance facade, default chain helper,
backoff sleep and unused store helpers are removed. Product balances remain in
the owning engine.

An overloaded read returns incomplete immediately. The host schedules the next
bounded pass. The synchronous upstream facade instead sleeps and retries inside
its pass. No sleep, synchronous XHR, Atomics wait or synchronous replay loop is
used here. Schema/segment/size ceilings are additional host-policy refusals.

The source-evidence store adds bounded versioned serialization. Events use the
upstream self-contained event codec. Receive/spend indexes rebuild from checked
events after restore. Query secrets and live futures are never serialized.
Rollback records an explicit independently accepted ancestor. Acknowledged
checkpoints contain a continuation and facts for the canonical wallet owner.

`upstream-sources.json` pins the original full-file SHA256 values.
`upstream.patch` describes changes from those six files after test removal.
To reconstruct and verify the adaptation against an existing local checkout:

```sh
python3 crates/z-transparent-pir-v2/tools/verify-upstream.py \
  /path/to/wallet-pir-at-c9262e8
```

The command checks the full source hashes, strips only the original test
sections, applies the committed patch in a temporary directory and compares each
result with this crate. No checkout or dependency source is modified. Replace
this adaptation when upstream supplies an equivalent yielding or async walker
that passes the same native/WASM, differential and canonical-wallet fixtures.

The fixture publisher and in-process server adapter in `tests/fixture.rs` adapt
`zakura/pir-transparent/src/testing/service.rs` at wallet-libraries commit
`3acb18a2f3eaff8f4739d1b26edc73095b80ff26`. The local changes allow a test to name
its independently known network/genesis, support start height zero and give cold
correctness fixtures a 180-second query deadline. This timeout is not a product
capacity or latency claim. The fixture opens no socket. Its MIT notice is in
`LICENSE-FIXTURE-MIT`.

New Coffer SDK code is Apache-2.0. Adapted wallet-pir modules retain Roman
Akhtariev's MIT notice in each file and in `LICENSE-MIT`. The package's combined
SPDX expression is `Apache-2.0 AND MIT`. `LICENSE-APACHE` contains Apache-2.0.

Core tests use a real native PIR server against nonzero receive/spend data,
upstream MemoryStore as a separate protocol oracle, and literal fixture events.
The engine integration adds an independent canonical full-block oracle. Fixture
agreement cannot prove that a production publisher has indexed every block.


The yielding adaptation additionally makes malformed-content dummy debt durable
on PendingPages. This is a Coffer suspension-boundary extension: the upstream
one-shot Failures.owed vector is not sufficient after an acknowledged checkpoint
or an out-of-call overload. Only logical child/count debt is saved, never crypto
contexts. Tests exercise the real server with malformed but digest-bound page
heads, partial budgets, cancellation, overload and cold anchor advancement.
The wrapper also uses inclusive optional coverage accumulation for genesis-only
ranges; saturating_sub(1) at zero is zero, not an unsigned wraparound.

The Coffer suspension wrapper persists independently observed canonical-chain
points, including publisher mismatches, in one anchor-bound map. This is public
chain evidence, not a publisher-verification assertion. It prevents bounded cold
passes from repeating rejected old coverage endpoints before upstream rollback.
The explicit supported domain is 1024 current shards and 4096 distinct old
coverage endpoints plus three anchor/genesis points (6147 observations); excess
old metadata fails incomplete without altering the evidence journal. The actual
136-old-shard to one-wide-shard fork fixture records elapsed and chain-budget
cold passes and requires the unchanged upstream walker to reach verified rewind.

Before prior-anchor lineage is decided, the wrapper emits no replacement
checkpoint on Init/Map failure or resource interruption. The canonical owner
retains the exact prior continuation. Cache inheritance/invalidation becomes
checkpointable only after Rust processes the independent anchor decision. This
is a suspension-boundary rule; upstream validation and rollback are unchanged.

The suspension profile also bounds retained records and serialized store growth.
The adapted MemoryStore validates the unique-event cap before mutation; its
runtime-only cap does not change the stored MemoryStore encoding. A typed local
policy refusal becomes a durable version-4 `history_metadata_limit` checkpoint.
The exact existing-pending malformed-content update may use reserved space to
persist newly owed dummy traffic before stopping. It cannot alter cursors,
fragments, facts, coverage or caches through that reserve. Versions 2 and 3 keep
their original digest validation before migration. The full event, byte and
serializer-reserve bounds and tests are described in `DESIGN.md`.

The reserved failure path accepts the inert publication-anchor stamp added by
`commit_bounded`; coverage, records and cache changes remain forbidden. When a
checked run cannot grow the store after a bad next head was already validated,
only the typed local cap preserves that known content failure and owed query
count. The failure update attaches to the unchanged durable pending cursor.
Real Walker regressions cover stamped anchors and both event/byte refusals with
publisher-built page fragments, followed by terminal checkpoint and cold resume.
