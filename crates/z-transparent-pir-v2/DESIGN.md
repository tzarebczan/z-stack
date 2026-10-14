# Transparent v12 acquisition contract

The crate preserves the c9262e8 Rust walker/codec/crypto and converts only its
network suspension points to a genuinely asynchronous walker. A Pass owns that
future. `advance()` polls it and returns one owned Step; `accept(request_id,
reply)` supplies a single bounded response; no synchronous replay/cache loop.

Public serde wire contract (snake_case `kind`, deny unknown fields):
Operation = init | map | filter {shard_id:u64} |
manifest {shard_id,revision:hex64} |
setup {shard_id,revision,table,segment:u32} |
query {shard_id,revision,table,body:base64}.
Table = {kind:"directory"} | {kind:"pages",child:u32}, child <32.
Reply = data {body:base64} | stale {map_digest_hint?:hex64} |
overloaded {retry_after?:u64} | misrouted | unavailable | refused.
Only these six public routes are admitted. Query never selects a segment.

`Pass::new(scope: Scope, checkpoint: Option<Checkpoint>, limits: Limits)`;
`advance()->Result<Step>`;
`accept(request_id:u64, reply:Reply)->Result<()>`;
`acknowledge(checkpoint_id:&str)->Result<()>`;
`finish_budget()->Result<()>` (active resource deadline only);
`cancel()` (user/session cancellation, never persistence).
Step = Exchange {request_id,operation} | NeedChain {height,hash} |
Checkpoint {checkpoint:Checkpoint} | Stopped {status:Status}.

Scope serde fields: source_id:String (stable digest), generation:u64,
scope_id:String (wallet inventory digest), anchor:{height:u64,hash:String},
scripts:[{script:String(hex),required_from:u64}],
accepted_headers:[{height:u64,hash:String}]. The wallet supplies its independently
accepted chain, not publisher metadata. Missing historical points yield NeedChain;
accept_chain(height,hash) only accepts independently validated wallet evidence.

Checkpoint serde fields: source_id, generation, scope_id,
checkpoint_id:String, previous_checkpoint_id:Option<String>,
anchor:{height,hash}, covered_through:u64, complete:bool,
pending_pages:u64, unresolved_spends:u64,
records:[{script:String(hex),event:String(hex self-contained TransparentEvent bytes),
shard_id:u64,revision:String}],
coverage:[{script:String(hex),required_from:u64,through:u64,revision:String}],
rewind_to:Option<{height,hash}>,
continuation:String(base64 versioned internal logical journal).
Records are a full source-evidence snapshot, deduplicated by upstream semantics;
the engine projects idempotently in its canonical wallet transaction. Continuation
contains durable source-evidence operations/cursors, not query secrets. No public
balance or spend authorization is exposed. Engine validates source/scope/generation
and accepted anchor before projecting and persisting continuation atomically.
Only after durable acknowledgment may a Complete status be published. Cancellation
or source/generation change retires the future and all private query contexts.

The private source-evidence journal replays the exact upstream WalletStore
contract. It is not a second wallet balance authority: the canonical WalletDb or
WebWallet is the sole product amount/history owner. Its internal ledger is used
only by inherited completeness checks and never exported as product balance.

Private query contexts remain opaque Rust memory between Exchange and accept.
Reopen discards contexts and regenerates fresh ciphertext from the acknowledged
logical continuation. Retries and malformed-content dummy obligations remain the
upstream walker's. Busy yields bounded delayed progress, never thread sleep.


The private continuation is version 3; version 2 is accepted only after its original checkpoint digest is verified, then migrated without changing its source journal or cover-query obligations. A malformed page run records its remaining
cover-query count on the existing pending item, bound to that script, revision,
and accepted target. The failure count and this obligation commit together before
another exchange is yielded. Reopening pays that debt before another real query
for the entry; a query budget can pay only a returned prefix. A refused round
retains its obligation, matching an honest round whose local replies were lost.
Normal anchor advancement preserves debt under the same revision. Public revision
retirement/reorg handling still retires the corresponding old pending work.
No query bytes, random coins, NativeSecret, or in-flight response context is saved.
Incomplete failure checkpoints never publish completion. Unpublished version-1
transparent continuations are rejected; existing wallet/payment data is unchanged.

Coverage walks from the script's inclusive required height using an optional end,
so a range covering genesis alone produces a revision-bearing 0..0 coverage row.
A completing checkpoint must carry every enrolled script through its accepted anchor.


Historical chain validation has an independent core resource limit:
`Limits.max_chain_reads` defaults to 32 (also when absent from older JSON).
Each new independent historical read, including prior-anchor revalidation, counts
against it. Reaching the limit produces an acknowledged incomplete checkpoint
with reason `chain_budget`, then stops that pass. The host's separate 64-read
limit remains a defense guard. Header work alone never changes source coverage,
completion, rewind markers, receive/spend facts, or query-cover debt. The first
header-only checkpoint reports no coverage when no evidence exists.

The continuation persists one map of independently observed public height/hash
pairs, including points that disagree with publisher metadata. ChainView derives
acceptance or rejection by comparison; a stored observation is never a claim
that the publisher is correct. Version-three matched-only checkpoints remain a
valid subset of this evidence and retain their original digest validation.
Source, network genesis and accepted target anchor bind every observation. On
an advancing target, the old anchor is independently revalidated on the current
selected chain before any older observation is inherited. A mismatch discards
that cache and uses fresh observations through the unchanged rollback rules.
Same-height anchor changes also invalidate inheritance; lower targets are refused.
While prior-anchor lineage is undecided, Init/Map failures and resource deadlines
stop without emitting a replacement checkpoint. The canonical owner keeps the
exact prior continuation, facts, coverage and cover debt. Receiving an old-anchor
reply alone does not authorize replacement: the Rust future must process the
independent acceptance/rejection and atomically merge or discard its old cache
before any new-target checkpoint is possible. An unpolled reply or dropped future
therefore cannot erase durable progress. Cancellation still wins without a drain.

The supported metadata domain is explicit: at most 1024 current shards (2048
boundary heights), 4096 distinct retained coverage endpoints, and genesis,
current anchor and prior anchor. The resulting observation-map bound is 6147;
network/read/time budgets are unchanged. Cache inheritance is pruned to the
exact needed set before merging. Scope-supplied extra independently accepted
points remain ephemeral until needed, so unrelated supplied points cannot
exhaust the durable map. Old coverage compacts by same-script containment and
exact range identity; cross-script or partial-target endpoints have no 1024
invariant. Exceeding 4096 retained endpoints stops with
`history_metadata_limit`, without IO, checkpoint replacement, evidence truncation
or wallet-state changes. This is an unsupported historical metadata size for
this client; it requires a future resource-policy/support upgrade. A rescan is
not promised to resolve it, and no fallback is permitted. Oversized serialized
observation maps are rejected before use. No PIR query bytes or randomness are
cached. Acknowledged chain or elapsed-budget progress survives cold restart,
including mismatches needed to reach a verified rollback.

A host resource deadline may call `finish_budget` while the same wallet session
is still active, then drain only Checkpoint/ack/Stopped. This abandons live IO and
query contexts while preserving previously validated logical progress. A held
checkpoint remains held until its ordinary durable acknowledgment. User lock,
user cancellation or an obsolete generation must use `cancel`, which cannot be
converted into a budget finish. An elapsed deadline may therefore commit a
shorter accepted header prefix without raising either read limit.

Budget checkpoints retain the full source projection and serialize it once per
bounded pass; large wallet checkpoint cost remains proportional to that journal.
They do not prove the publisher indexed every block. The real-service fixture
first recovers receive/spend facts, then validates more than 130 boundary heights
across cold bounded passes and advancing tips, preserving those facts during
header-only progress.

An incomplete result with an unchanged durable continuation and identical
source/scope/generation/anchor returns Stopped without rewriting the prior
checkpoint. A changed binding, accepted header, journal or cover obligation still
checkpoints. Complete retains its normal checkpoint/ack path. If the complete
logical payload has the same ID, the validated prior metadata (including receipt
link and rewind) is reused exactly; conflicting metadata is refused. A repeated
upstream walk may increment its source-journal commit counter and produce a new
ID while preserving the same canonical facts.
