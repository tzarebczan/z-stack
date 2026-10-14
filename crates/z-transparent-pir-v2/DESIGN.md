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
`cancel()`.
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


The private continuation is version 2. A malformed page run records its remaining
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
