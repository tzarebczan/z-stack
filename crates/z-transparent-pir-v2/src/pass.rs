use crate::{
    adapters::{Acceptance, ChainView, StaticScripts},
    client::Table as NativeTable,
    memory_store::MemoryStore,
    shared_store::{SharedStore, MAX_CONTINUATION_BYTES, MAX_RETAINED_EVENTS, MAX_STORE_BYTES},
    store::{ScriptEntry, ScriptOrigin, WalletStore},
    sync::{self, Completion, GeometryParams, ServiceGeometry, WorkLimits},
    transport::{BoxError, FilterSource, Overloaded, ShardTransport, StaleRevision},
    types::*,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bincode::Options;
use futures::{
    future::{poll_fn, LocalBoxFuture},
    task::noop_waker_ref,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};
#[derive(Serialize, Deserialize)]
struct LegacyContinuation {
    version: u32,
    source_id: String,
    state: MemoryStore,
}
// Independently observed selected-chain points, including publisher mismatches.
// Comparison with a publication is derived by ChainView, never persisted as trust.
const MAX_MAP_SHARDS: usize = 1024;
const MAX_OLD_ENDPOINTS: usize = 4096;
const MAX_OBSERVED_POINTS: usize = 2 * MAX_MAP_SHARDS + MAX_OLD_ENDPOINTS + 3;
#[derive(Clone, Serialize, Deserialize)]
struct ObservedChain {
    anchor: Anchor,
    genesis: Option<String>,
    #[serde(deserialize_with = "bounded_headers")]
    headers: BTreeMap<u64, String>,
}
fn bounded_headers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<BTreeMap<u64, String>, D::Error> {
    struct Headers;
    impl<'de> serde::de::Visitor<'de> for Headers {
        type Value = BTreeMap<u64, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a bounded map of independently observed headers")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            if map.size_hint().is_some_and(|n| n > MAX_OBSERVED_POINTS) {
                return Err(serde::de::Error::custom("header limit"));
            }
            let mut headers = BTreeMap::new();
            while let Some((height, hash)) = map.next_entry()? {
                if headers.len() >= MAX_OBSERVED_POINTS || headers.insert(height, hash).is_some() {
                    return Err(serde::de::Error::custom("header limit"));
                }
            }
            Ok(headers)
        }
    }
    d.deserialize_map(Headers)
}
#[derive(Serialize, Deserialize)]
struct LegacyContinuationV3 {
    version: u32,
    source_id: String,
    state: MemoryStore,
    chain: ObservedChain,
}
#[derive(Serialize, Deserialize)]
struct Continuation {
    version: u32,
    source_id: String,
    state: MemoryStore,
    chain: ObservedChain,
    policy_limit: bool,
}
// Coverage compaction is per script and revision: old recuts and partial
// targets across scripts have no map-size bound. Refuse an explicit supported
// metadata limit without truncating evidence or asking the walker to guess.
fn retained_endpoints(state: &MemoryStore) -> Result<std::collections::BTreeSet<u64>> {
    let mut endpoints = std::collections::BTreeSet::new();
    for script in state.scripts().map_err(|_| Error("store"))? {
        for range in state.coverage(&script.script).map_err(|_| Error("store"))? {
            endpoints.insert(range.end_height);
            if endpoints.len() > MAX_OLD_ENDPOINTS {
                return Err(Error("history_metadata_limit"));
            }
        }
    }
    Ok(endpoints)
}
fn needed_points(
    boundaries: impl IntoIterator<Item = u64>,
    old: &std::collections::BTreeSet<u64>,
    current: u64,
    prior: Option<u64>,
) -> std::collections::BTreeSet<u64> {
    let mut needed = std::collections::BTreeSet::from([0, current]);
    needed.extend(prior);
    needed.extend(old);
    needed.extend(boundaries);
    needed
}
fn reason_code(r: sync::IncompleteReason) -> &'static str {
    use sync::IncompleteReason::*;
    match r {
        QueryBudget => "query_budget",
        ByteBudget => "byte_budget",
        PendingLimit => "pending_limit",
        Overloaded { .. } => "overloaded",
        ChainUnknown { .. } => "chain_unknown",
        DiscoveryUnbounded => "scope_growing",
        ContentFailed => "content_failed",
        PublicationBehind { .. } => "publication_lag",
        UnresolvedSpends => "unresolved_spends",
    }
}

#[derive(Debug, thiserror::Error)]
#[error("transparent recovery: {0}")]
pub struct Error(pub(crate) &'static str);
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone)]
enum Pending {
    Exchange { id: u64, operation: Operation },
    Chain { height: u64, hash: String },
}
#[derive(Default)]
struct Io {
    next: u64,
    pending: Option<Pending>,
    reply: Option<Reply>,
    chain_reply: Option<Anchor>,
    exchanges: u64,
    chain_reads: u64,
    download: u64,
    upload: u64,
    stop: Option<&'static str>,
}
#[derive(Clone)]
struct Driver {
    io: Rc<RefCell<Io>>,
    limits: Limits,
    chain: Rc<RefCell<BTreeMap<u64, String>>>,
    supplied: Rc<BTreeMap<u64, String>>,
}
impl Driver {
    fn retain_points(&self, needed: &std::collections::BTreeSet<u64>) {
        self.chain
            .borrow_mut()
            .retain(|height, _| needed.contains(height));
    }
    async fn send(&self, operation: Operation) -> std::result::Result<Vec<u8>, BoxError> {
        let upload = if let Operation::Query { body, .. } = &operation {
            body.len() as u64
        } else {
            0
        };
        {
            let mut io = self.io.borrow_mut();
            if io.exchanges >= self.limits.max_exchanges
                || io.upload.saturating_add(upload) > self.limits.max_upload_bytes
            {
                io.stop = Some("budget");
                return Err(Box::new(Error("budget")));
            }
            io.exchanges += 1;
            io.upload += upload;
            io.next += 1;
            let id = io.next;
            io.pending = Some(Pending::Exchange {
                id,
                operation: operation.clone(),
            });
        }
        let reply = poll_fn(|_| {
            let mut io = self.io.borrow_mut();
            match io.reply.take() {
                Some(r) => {
                    io.pending = None;
                    Poll::Ready(r)
                }
                None => Poll::Pending,
            }
        })
        .await;
        match reply {
            Reply::Data { body } => {
                let max = match operation {
                    Operation::Filter { .. } => 8 * 1024 * 1024,
                    Operation::Setup { .. } => 128 * 1024,
                    Operation::Query { .. } => 512 * 1024,
                    _ => 1024 * 1024,
                };
                if body.len() > max * 4 / 3 + 8 {
                    return Err(Box::new(Error("response_limit")));
                }
                let bytes = STANDARD
                    .decode(body)
                    .map_err(|_| Error("response_encoding"))?;
                if bytes.len() > max {
                    return Err(Box::new(Error("response_limit")));
                }
                Ok(bytes)
            }
            Reply::Stale { map_digest_hint } => {
                let (shard_id, revision) = match operation {
                    Operation::Manifest { shard_id, revision }
                    | Operation::Setup {
                        shard_id, revision, ..
                    }
                    | Operation::Query {
                        shard_id, revision, ..
                    } => (shard_id, revision),
                    _ => (0, String::new()),
                };
                Err(Box::new(StaleRevision {
                    shard_id,
                    revision,
                    map_sha256: map_digest_hint,
                }))
            }
            Reply::Overloaded { retry_after } => Err(Box::new(Overloaded {
                retry_after: retry_after.map(|s| Duration::from_secs(s.clamp(1, 60))),
            })),
            Reply::Misrouted => Err(Box::new(Error("misrouted"))),
            Reply::Unavailable => Err(Box::new(Error("unavailable"))),
            Reply::Refused => Err(Box::new(Error("refused"))),
        }
    }
    async fn point(&self, height: u64, expected: &str) -> Result<()> {
        if self.chain.borrow().contains_key(&height) {
            // Existing independently accepted points remain authoritative; the
            // walker must see Rejected for obsolete coverage on a reorg.
            return Ok(());
        }
        if self.chain.borrow().len() >= MAX_OBSERVED_POINTS {
            return Err(Error("chain_limit"));
        }
        if let Some(observed) = self.supplied.get(&height) {
            self.chain.borrow_mut().insert(height, observed.clone());
            return Ok(());
        }
        {
            let mut io = self.io.borrow_mut();
            if io.chain_reads >= self.limits.max_chain_reads {
                io.stop = Some("chain_budget");
                return Err(Error("chain_budget"));
            }
            io.chain_reads += 1;
        }
        self.io.borrow_mut().pending = Some(Pending::Chain {
            height,
            hash: expected.to_string(),
        });
        let point = poll_fn(|_| {
            let mut io = self.io.borrow_mut();
            match io.chain_reply.take() {
                Some(a) => {
                    io.pending = None;
                    Poll::Ready(a)
                }
                None => Poll::Pending,
            }
        })
        .await;
        if point.height != height {
            return Err(Error("chain_point"));
        }
        self.chain.borrow_mut().insert(height, point.hash);
        Ok(())
    }
}
fn native_table(t: NativeTable) -> Table {
    match t {
        NativeTable::Directory => Table::Directory,
        NativeTable::Pages(child) => Table::Pages { child },
    }
}
impl FilterSource for Driver {
    async fn shard_map(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        let b = self.send(Operation::Map).await?;
        let n = b.len() as u64;
        Ok((b, n))
    }
    async fn filter(&mut self, shard_id: u64) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        let b = self.send(Operation::Filter { shard_id }).await?;
        let n = b.len() as u64;
        Ok((b, n))
    }
}
impl ShardTransport for Driver {
    async fn init(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        let b = self.send(Operation::Init).await?;
        let n = b.len() as u64;
        Ok((b, n))
    }
    async fn manifest(
        &mut self,
        shard_id: u64,
        revision: &str,
    ) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        let b = self
            .send(Operation::Manifest {
                shard_id,
                revision: revision.to_string(),
            })
            .await?;
        let n = b.len() as u64;
        Ok((b, n))
    }
    async fn setup(
        &mut self,
        shard_id: u64,
        revision: &str,
        table: NativeTable,
        segment: u32,
    ) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        let b = self
            .send(Operation::Setup {
                shard_id,
                revision: revision.to_string(),
                table: native_table(table),
                segment,
            })
            .await?;
        let n = b.len() as u64;
        Ok((b, n))
    }
    async fn query(
        &mut self,
        shard_id: u64,
        revision: &str,
        table: NativeTable,
        body: &[u8],
    ) -> std::result::Result<Vec<u8>, BoxError> {
        self.send(Operation::Query {
            shard_id,
            revision: revision.to_string(),
            table: native_table(table),
            body: STANDARD.encode(body),
        })
        .await
    }
}
struct Chain(Rc<RefCell<BTreeMap<u64, String>>>);
impl ChainView for Chain {
    fn is_accepted(&self, h: u64, s: &str) -> Acceptance {
        match self.0.borrow().get(&h) {
            Some(held) if held == s => Acceptance::Accepted,
            Some(_) => Acceptance::Rejected,
            None => Acceptance::Unknown,
        }
    }
    fn hash_at(&self, h: u64) -> Option<String> {
        self.0.borrow().get(&h).cloned()
    }
    fn tip(&self) -> Option<crate::store::Anchor> {
        self.0
            .borrow()
            .iter()
            .next_back()
            .map(|(h, s)| crate::store::Anchor {
                height: *h,
                hash: s.clone(),
            })
    }
}

pub struct Pass {
    scope: Scope,
    store: SharedStore,
    driver: Driver,
    // Prior evidence remains owned by the external durable checkpoint until
    // its anchor is independently accepted or rejected on this target chain.
    lineage_pending: Rc<Cell<bool>>,
    future: Option<LocalBoxFuture<'static, Result<Status>>>,
    previous: Option<String>,
    // Validated prior metadata, with large records/continuation owned by store.
    prior: Option<Checkpoint>,
    prior_baseline_matches: bool,
    baseline: Vec<u8>,
    state_baseline: Vec<u8>,
    held: Option<Checkpoint>,
    finished: Option<Status>,
    cancelled: bool,
    final_ack: bool,
    terminal_without_checkpoint: bool,
}
fn encoded(store: &MemoryStore) -> Result<Vec<u8>> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(store)
        .map_err(|_| Error("checkpoint_encode"))
}
fn durable(
    scope: &Scope,
    state: &MemoryStore,
    headers: &BTreeMap<u64, String>,
    policy_limit: bool,
) -> Result<Vec<u8>> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&Continuation {
            version: 4,
            source_id: scope.source_id.clone(),
            state: state.clone(),
            chain: ObservedChain {
                anchor: scope.anchor.clone(),
                genesis: headers.get(&0).cloned(),
                headers: headers.clone(),
            },
            policy_limit,
        })
        .map_err(|_| Error("checkpoint_encode"))
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn checkpoint_id(
    source: &str,
    scope: &str,
    generation: u64,
    anchor: &Anchor,
    complete: bool,
    bytes: &[u8],
) -> String {
    let mut digest = Sha256::new();
    digest.update(source.as_bytes());
    digest.update(scope.as_bytes());
    digest.update(generation.to_le_bytes());
    digest.update(anchor.height.to_le_bytes());
    digest.update(anchor.hash.as_bytes());
    digest.update([u8::from(complete)]);
    digest.update(bytes);
    hex::encode(digest.finalize())
}
impl Pass {
    pub fn new(scope: Scope, checkpoint: Option<Checkpoint>, limits: Limits) -> Result<Self> {
        if limits.max_chain_reads == 0
            || limits.max_chain_reads > 4096
            || limits.max_exchanges == 0
            || limits.max_exchanges > 4096
            || limits.max_scripts > 4096
            || !hash(&scope.source_id)
            || !hash(&scope.scope_id)
            || !hash(&scope.anchor.hash)
            || scope.accepted_headers.len() > 4096
            || scope.scripts.len() > limits.max_scripts
            || scope.scripts.is_empty()
        {
            return Err(Error("scope"));
        }
        let mut seen = std::collections::BTreeSet::new();
        if scope.scripts.iter().any(|s| !seen.insert(&s.script)) {
            return Err(Error("duplicate_script"));
        }
        let mut restored_chain = None;
        let mut restored_policy = false;
        let mut memory = if let Some(cp) = &checkpoint {
            if cp.anchor.height > scope.anchor.height {
                return Err(Error("anchor_rewound"));
            }
            if cp.source_id != scope.source_id {
                return Err(Error("source_changed"));
            }
            if cp.continuation.len() > 96 * 1024 * 1024 {
                return Err(Error("checkpoint_limit"));
            }
            let bytes = STANDARD
                .decode(&cp.continuation)
                .map_err(|_| Error("checkpoint_encoding"))?;
            let version = bytes.get(..4).ok_or(Error("checkpoint_decode"))?;
            let (mut state, original) = match u32::from_le_bytes(version.try_into().unwrap()) {
                2 => {
                    let restored: LegacyContinuation = bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .with_limit(64 * 1024 * 1024)
                        .reject_trailing_bytes()
                        .deserialize(&bytes)
                        .map_err(|_| Error("checkpoint_decode"))?;
                    if restored.source_id != scope.source_id {
                        return Err(Error("checkpoint_source"));
                    }
                    let original = encoded(&restored.state)?;
                    (restored.state, original)
                }
                3 | 4 => {
                    let options = bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .with_limit(MAX_CONTINUATION_BYTES as u64)
                        .reject_trailing_bytes();
                    let restored: Continuation = if version == 3u32.to_le_bytes() {
                        let old: LegacyContinuationV3 = options
                            .deserialize(&bytes)
                            .map_err(|_| Error("checkpoint_decode"))?;
                        Continuation {
                            version: 4,
                            source_id: old.source_id,
                            state: old.state,
                            chain: old.chain,
                            policy_limit: false,
                        }
                    } else {
                        options
                            .deserialize(&bytes)
                            .map_err(|_| Error("checkpoint_decode"))?
                    };
                    restored_policy = restored.policy_limit;
                    if restored.source_id != scope.source_id
                        || restored.chain.anchor != cp.anchor
                        || restored.chain.headers.len() > MAX_OBSERVED_POINTS
                        || restored
                            .chain
                            .headers
                            .iter()
                            .any(|(h, s)| *h > cp.anchor.height || !hash(s))
                        || restored.chain.headers.get(&cp.anchor.height) != Some(&cp.anchor.hash)
                        || restored.chain.genesis.as_ref() != restored.chain.headers.get(&0)
                    {
                        return Err(Error("checkpoint_source"));
                    }
                    restored_chain = Some(restored.chain);
                    (restored.state, bytes)
                }
                _ => return Err(Error("checkpoint_source")),
            };
            if checkpoint_id(
                &cp.source_id,
                &cp.scope_id,
                cp.generation,
                &cp.anchor,
                cp.complete,
                &original,
            ) != cp.checkpoint_id
            {
                return Err(Error("checkpoint_id"));
            }
            state.rebuild_indexes();
            state
        } else {
            MemoryStore::new()
        };
        let current_scripts = scope
            .scripts
            .iter()
            .map(|s| hex::decode(&s.script).map_err(|_| Error("script")))
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        memory.retain_scope(&current_scripts);
        let old_endpoints = retained_endpoints(&memory);
        let unsupported_history = old_endpoints.is_err();
        let terminal_without_checkpoint = unsupported_history
            || memory.events().map_err(|_| Error("store"))?.len() > MAX_RETAINED_EVENTS
            || encoded(&memory)?.len() > MAX_STORE_BYTES;
        memory.set_event_limit(MAX_RETAINED_EVENTS);
        let old_endpoints = old_endpoints.unwrap_or_default();

        let store = SharedStore {
            inner: Rc::new(RefCell::new(memory)),
            rewind: Rc::new(RefCell::new(None)),
            policy_limit: Rc::new(Cell::new(restored_policy)),
        };
        let mut headers = BTreeMap::new();
        for point in &scope.accepted_headers {
            if !hash(&point.hash) || point.height > scope.anchor.height {
                return Err(Error("chain"));
            }
            if let Some(old) = headers.insert(point.height, point.hash.clone()) {
                if old != point.hash {
                    return Err(Error("chain"));
                }
            }
        }
        if headers.get(&scope.anchor.height) != Some(&scope.anchor.hash) {
            return Err(Error("anchor_not_accepted"));
        }
        let driver = Driver {
            io: Rc::new(RefCell::new(Io::default())),
            limits: limits.clone(),
            chain: Rc::new(RefCell::new(
                headers
                    .iter()
                    .filter(|(h, _)| **h == 0 || **h == scope.anchor.height)
                    .map(|(h, s)| (*h, s.clone()))
                    .collect(),
            )),
            supplied: Rc::new(headers),
        };
        // A moving tip may reuse older points only after independently checking
        // that its previously accepted anchor still belongs to the selected chain.
        let restored_chain = restored_chain.filter(|old| {
            old.anchor.height <= scope.anchor.height
                && old.genesis.is_some()
                && old.genesis.as_ref() == driver.chain.borrow().get(&0)
        });
        let lineage_pending = Rc::new(Cell::new(
            restored_chain
                .as_ref()
                .is_some_and(|old| old.anchor != scope.anchor),
        ));
        if let Some(old) = &restored_chain {
            if old.anchor == scope.anchor {
                for (height, hash) in &old.headers {
                    if driver.supplied.get(height).is_some_and(|held| held != hash) {
                        return Err(Error("chain_point"));
                    }
                    driver.chain.borrow_mut().insert(*height, hash.clone());
                }
            }
        }
        let mut filters = driver.clone();
        let mut transport = driver.clone();
        let owned = driver.clone();
        let chain = Chain(driver.chain.clone());
        let mut state = store.clone();
        let request = scope.clone();
        let lineage = lineage_pending.clone();
        let future = Box::pin(async move {
            let (init, _) = transport.init().await.map_err(|_| Error("init"))?;
            let parsed =
                transparent_wallet::parse_init(&init).map_err(|_| Error("init_invalid"))?;
            let geometry = ServiceGeometry {
                schema: parsed.schema,
                geometries: parsed
                    .geometries
                    .into_iter()
                    .map(|g| GeometryParams {
                        name: g.name,
                        directory_rows: g.directory_rows,
                        directory_row_bytes: g.directory_row_bytes,
                        directory_scheme: g.directory_scheme,
                        directory_scheme_dq44: g.directory_scheme_dq44,
                        directory_setup_seed: g.directory_setup_seed,
                        page_rows: g.page_rows,
                        page_row_bytes: g.page_row_bytes,
                        pages_scheme: g.pages_scheme,
                        pages_scheme_dq44: g.pages_scheme_dq44,
                        pages_setup_seed: g.pages_setup_seed,
                    })
                    .collect(),
            };
            let (raw, cost) = filters.shard_map().await.map_err(|_| Error("map"))?;
            let map: transparent_filter::ShardMap =
                serde_json::from_slice(&raw).map_err(|_| Error("map_invalid"))?;
            map.check_shape().map_err(|_| Error("map_invalid"))?;
            let genesis = match map.network.as_str() {
                "main" | "mainnet" => transparent_filter::MAINNET_GENESIS_DISPLAY,
                "test" | "testnet" => {
                    "05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38"
                }
                "regtest" => "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
                _ => {
                    return Ok(Status::Incomplete {
                        reason: "publication_profile".into(),
                    })
                }
            };
            if map.genesis_hash != genesis
                || map.profile != transparent_filter::RANGE_PROFILE_V2.name
            {
                return Ok(Status::Incomplete {
                    reason: "publication_profile".into(),
                });
            }
            owned.point(0, genesis).await?;
            if !matches!(chain.is_accepted(0, genesis), Acceptance::Accepted) {
                return Ok(Status::Incomplete {
                    reason: "chain_identity".into(),
                });
            }
            if map.shards.len() > MAX_MAP_SHARDS
                || request
                    .scripts
                    .iter()
                    .any(|s| s.required_from < map.start_height)
            {
                return Ok(Status::Incomplete {
                    reason: "publication_incomplete".into(),
                });
            }
            let needed = needed_points(
                map.shards.iter().flat_map(|entry| {
                    (entry.end_height <= request.anchor.height)
                        .then_some(entry.end_height)
                        .into_iter()
                        .chain(
                            entry
                                .start_height
                                .checked_sub(1)
                                .filter(|h| *h <= request.anchor.height),
                        )
                }),
                &old_endpoints,
                request.anchor.height,
                restored_chain.as_ref().map(|old| old.anchor.height),
            );
            owned.retain_points(&needed);
            if let Some(old) = restored_chain {
                if old.anchor != request.anchor && old.genesis.as_deref() == Some(genesis) {
                    owned.point(old.anchor.height, &old.anchor.hash).await?;
                    if matches!(
                        chain.is_accepted(old.anchor.height, &old.anchor.hash),
                        Acceptance::Accepted
                    ) {
                        for (height, hash) in
                            old.headers.into_iter().filter(|(h, _)| needed.contains(h))
                        {
                            if owned
                                .chain
                                .borrow()
                                .get(&height)
                                .is_some_and(|held| held != &hash)
                            {
                                return Err(Error("chain_point"));
                            }
                            owned.chain.borrow_mut().insert(height, hash.clone());
                        }
                    }
                }
            }
            // No await separates the accepted-cache merge / rejected-cache
            // discard from this decision. Only now may a replacement checkpoint
            // bind the journal and observations to the new target.
            lineage.set(false);
            for e in &map.shards {
                if e.end_height <= request.anchor.height {
                    owned.point(e.end_height, &e.terminal_block_hash).await?;
                }
                if e.start_height > 0 && e.start_height - 1 <= request.anchor.height {
                    owned
                        .point(e.start_height - 1, &e.parent_block_hash)
                        .await?;
                }
            }
            for e in state.scripts().map_err(|_| Error("store"))? {
                for range in state.coverage(&e.script).map_err(|_| Error("store"))? {
                    owned
                        .point(range.end_height, &range.terminal_block_hash)
                        .await?;
                }
            }
            let scripts = request
                .scripts
                .iter()
                .map(|s| {
                    Ok(ScriptEntry {
                        script: hex::decode(&s.script).map_err(|_| Error("script"))?,
                        origin: ScriptOrigin::Derived,
                        required_from: s.required_from,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if scripts.iter().any(|s| {
                s.script.is_empty()
                    || s.script.len() > 40
                    || s.required_from > request.anchor.height
            }) {
                return Err(Error("script"));
            }
            let mut scripts = StaticScripts(scripts);
            let target = crate::store::Anchor {
                height: request.anchor.height,
                hash: request.anchor.hash,
            };
            match sync::sync_into(
                &mut state,
                &map,
                cost,
                &geometry,
                &chain,
                &mut scripts,
                &mut filters,
                &mut transport,
                &WorkLimits {
                    max_queries: Some(limits.max_queries),
                    max_private_bytes: Some(limits.max_private_bytes),
                },
                &target,
            )
            .await
            {
                Ok(report) => Ok(match report.completion {
                    Completion::Complete => Status::Complete,
                    Completion::Incomplete { reason, .. } => Status::Incomplete {
                        reason: reason_code(reason).into(),
                    },
                }),
                Err(_) => Ok(Status::Incomplete {
                    reason: "recovery_failed".into(),
                }),
            }
        });
        let baseline = durable(
            &scope,
            &store.inner.borrow(),
            &driver.chain.borrow(),
            store.policy_limit.get(),
        )?;
        let state_baseline = encoded(&store.inner.borrow())?;
        let prior_baseline_matches = checkpoint.as_ref().is_some_and(|cp| {
            cp.scope_id == scope.scope_id
                && cp.generation == scope.generation
                && cp.anchor == scope.anchor
                && STANDARD.decode(&cp.continuation).ok().as_deref() == Some(baseline.as_slice())
        });
        let previous = checkpoint.as_ref().map(|cp| cp.checkpoint_id.clone());
        let prior = checkpoint.map(|mut cp| {
            cp.records = Vec::new();
            cp.continuation = String::new();
            cp
        });
        Ok(Self {
            scope,
            store,
            driver,
            lineage_pending,
            future: (!terminal_without_checkpoint && !restored_policy).then_some(future),
            previous,
            prior,
            prior_baseline_matches,
            baseline,
            state_baseline,
            held: None,
            finished: (terminal_without_checkpoint || restored_policy).then(|| {
                Status::Incomplete {
                    reason: "history_metadata_limit".into(),
                }
            }),
            cancelled: false,
            final_ack: terminal_without_checkpoint || restored_policy,
            terminal_without_checkpoint,
        })
    }
    fn stop_for_policy(&mut self) {
        self.future = None;
        self.finished = Some(Status::Incomplete {
            reason: "history_metadata_limit".into(),
        });
        let mut io = self.driver.io.borrow_mut();
        io.pending = None;
        io.reply = None;
        io.chain_reply = None;
    }
    pub fn advance(&mut self) -> Result<Step> {
        if self.cancelled {
            return Ok(Step::Stopped {
                status: Status::Cancelled,
            });
        }
        if self.terminal_without_checkpoint {
            return Ok(Step::Stopped {
                status: Status::Incomplete {
                    reason: "history_metadata_limit".into(),
                },
            });
        }
        if self.store.policy_limit.get() {
            self.stop_for_policy();
        }
        if let Some(cp) = &self.held {
            return Ok(Step::Checkpoint {
                checkpoint: cp.clone(),
            });
        }
        if self.finished.is_none() {
            if let Some(future) = self.future.as_mut() {
                let mut cx = Context::from_waker(noop_waker_ref());
                if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                    self.finished = Some(if let Some(reason) = self.driver.io.borrow().stop {
                        Status::Incomplete {
                            reason: reason.into(),
                        }
                    } else {
                        result.unwrap_or_else(|_| Status::Incomplete {
                            reason: "unavailable".into(),
                        })
                    });
                    self.future = None;
                }
            }
        }
        if self.store.policy_limit.get() {
            self.stop_for_policy();
        }
        if self.lineage_pending.get() && self.finished.is_some() {
            // The caller keeps its exact acknowledged prior checkpoint. An Init,
            // Map or old-anchor failure/deadline is not an invalidation decision.
            return Ok(Step::Stopped {
                status: self.finished.clone().unwrap(),
            });
        }
        let state_changed = encoded(&self.store.inner.borrow())? != self.state_baseline;
        if state_changed && retained_endpoints(&self.store.inner.borrow()).is_err() {
            // Preserve every accepted fact and outstanding cover obligation.
            self.store.policy_limit.set(true);
            self.stop_for_policy();
        }
        if state_changed || self.finished.is_some() {
            let bytes = durable(
                &self.scope,
                &self.store.inner.borrow(),
                &self.driver.chain.borrow(),
                self.store.policy_limit.get(),
            )?;
            if self.prior_baseline_matches
                && bytes == self.baseline
                && matches!(self.finished, Some(Status::Incomplete { .. }))
            {
                return Ok(Step::Stopped {
                    status: self.finished.clone().unwrap(),
                });
            }
            // A final status checkpoint is emitted once, then removed by acknowledge.
            if self.finished.is_some()
                && self.future.is_none()
                && self.baseline == bytes
                && self.final_ack
            {
                return Ok(Step::Stopped {
                    status: self.finished.clone().unwrap(),
                });
            }
            let cp = self.checkpoint(&bytes)?;
            self.held = Some(cp.clone());
            return Ok(Step::Checkpoint { checkpoint: cp });
        }
        match self.driver.io.borrow().pending.clone() {
            Some(Pending::Exchange { id, operation }) => Ok(Step::Exchange {
                request_id: id,
                operation,
            }),
            Some(Pending::Chain { height, hash }) => Ok(Step::NeedChain { height, hash }),
            None => Err(Error("no_progress")),
        }
    }
    fn checkpoint(&self, bytes: &[u8]) -> Result<Checkpoint> {
        if self.lineage_pending.get() {
            return Err(Error("lineage_pending"));
        }
        let state = self.store.inner.borrow();
        let complete = self.finished == Some(Status::Complete);
        let records = state
            .events()
            .map_err(|_| Error("store"))?
            .into_iter()
            .map(|e| Record {
                script: hex::encode(e.script),
                event: hex::encode(e.event.to_bytes()),
                shard_id: e.shard_id,
                revision: e.revision_digest,
            })
            .collect();
        if state.events().map_err(|_| Error("store"))?.len() > MAX_RETAINED_EVENTS {
            return Err(Error("event_limit"));
        }
        let mut coverage = Vec::new();
        let mut through = if state.scripts().map_err(|_| Error("store"))?.is_empty() {
            0
        } else {
            self.scope.anchor.height
        };
        for script in state.scripts().map_err(|_| Error("store"))? {
            let ranges = state.coverage(&script.script).map_err(|_| Error("store"))?;
            let mut end: Option<u64> = None;
            let mut revision = String::new();
            for r in ranges {
                let next = end
                    .map(|height| height.saturating_add(1))
                    .unwrap_or(script.required_from);
                if r.start_height <= next && r.end_height >= next {
                    end = Some(r.end_height);
                    revision = r.revision_digest;
                }
            }
            through = through.min(end.unwrap_or(script.required_from.saturating_sub(1)));
            if let Some(end) = end {
                coverage.push(Coverage {
                    script: hex::encode(script.script),
                    required_from: script.required_from,
                    through: end,
                    revision,
                });
            }
        }
        if complete
            && (coverage.len() != self.scope.scripts.len()
                || coverage
                    .iter()
                    .any(|range| range.through < self.scope.anchor.height))
        {
            return Err(Error("coverage_incomplete"));
        }
        if bytes.len() > MAX_CONTINUATION_BYTES {
            return Err(Error("checkpoint_limit"));
        }
        let continuation = STANDARD.encode(bytes);
        let id = checkpoint_id(
            &self.scope.source_id,
            &self.scope.scope_id,
            self.scope.generation,
            &self.scope.anchor,
            complete,
            bytes,
        );
        let checkpoint = Checkpoint {
            source_id: self.scope.source_id.clone(),
            generation: self.scope.generation,
            scope_id: self.scope.scope_id.clone(),
            checkpoint_id: id.clone(),
            previous_checkpoint_id: if self.previous.as_ref() == Some(&id) {
                self.prior
                    .as_ref()
                    .and_then(|cp| cp.previous_checkpoint_id.clone())
            } else {
                self.previous.clone()
            },
            anchor: self.scope.anchor.clone(),
            covered_through: through,
            complete,
            pending_pages: state.pending().map_err(|_| Error("store"))?.len() as u64,
            unresolved_spends: state
                .ledger()
                .map_err(|_| Error("store"))?
                .unresolved()
                .len() as u64,
            records,
            coverage,
            rewind_to: if self.previous.as_ref() == Some(&id) {
                self.prior.as_ref().and_then(|cp| cp.rewind_to.clone())
            } else {
                self.store.rewind.borrow().clone().map(|a| Anchor {
                    height: a.height,
                    hash: a.hash,
                })
            },
            continuation,
        };
        if self.previous.as_ref() == Some(&id) {
            let mut prior = self.prior.clone().ok_or(Error("checkpoint_missing"))?;
            prior.records = checkpoint.records.clone();
            prior.continuation = checkpoint.continuation.clone();
            if prior != checkpoint {
                return Err(Error("checkpoint_conflict"));
            }
            Ok(prior)
        } else {
            Ok(checkpoint)
        }
    }
    pub fn accept(&mut self, request_id: u64, reply: Reply) -> Result<()> {
        if self.held.is_some() || self.cancelled {
            return Err(Error("not_waiting"));
        }
        let mut io = self.driver.io.borrow_mut();
        if !matches!(io.pending,Some(Pending::Exchange{id,..})if id==request_id)
            || io.reply.is_some()
        {
            return Err(Error("request_id"));
        }
        let bytes = serde_json::to_vec(&reply)
            .map_err(|_| Error("reply"))?
            .len() as u64;
        io.download = io.download.saturating_add(bytes);
        if io.download > self.driver.limits.max_download_bytes {
            io.stop = Some("budget");
            io.reply = Some(Reply::Unavailable)
        } else {
            io.reply = Some(reply)
        }
        Ok(())
    }
    pub fn accept_chain(&mut self, height: u64, hash: String) -> Result<()> {
        if !self.held.is_none() || !crate::pass::hash(&hash) {
            return Err(Error("chain_point"));
        }
        let mut io = self.driver.io.borrow_mut();
        if !matches!(io.pending,Some(Pending::Chain{height:h,..})if h==height)
            || io.chain_reply.is_some()
        {
            return Err(Error("chain_point"));
        }
        io.chain_reply = Some(Anchor { height, hash });
        Ok(())
    }
    pub fn acknowledge(&mut self, id: &str) -> Result<()> {
        let cp = self.held.as_ref().ok_or(Error("checkpoint_missing"))?;
        if cp.checkpoint_id != id {
            return Err(Error("checkpoint_id"));
        }
        self.baseline = durable(
            &self.scope,
            &self.store.inner.borrow(),
            &self.driver.chain.borrow(),
            self.store.policy_limit.get(),
        )?;
        self.state_baseline = encoded(&self.store.inner.borrow())?;
        self.prior_baseline_matches = true;
        self.previous = Some(id.into());
        self.final_ack = self.finished.is_some();
        self.store.rewind.borrow_mut().take();
        let mut cp = self.held.take().ok_or(Error("checkpoint_missing"))?;
        cp.records = Vec::new();
        cp.continuation = String::new();
        self.prior = Some(cp);
        Ok(())
    }
    /// End an active resource-limited pass without discarding validated logical
    /// progress. The caller must still durably acknowledge any yielded checkpoint.
    /// This must never be used for user cancellation or a retired wallet session.
    pub fn finish_budget(&mut self) -> Result<()> {
        if self.cancelled {
            return Err(Error("cancelled"));
        }
        if self.final_ack && self.finished.is_some() {
            return Ok(());
        }
        self.future = None;
        if self.finished.is_none() {
            self.finished = Some(Status::Incomplete {
                reason: "budget".into(),
            });
        }
        let mut io = self.driver.io.borrow_mut();
        io.pending = None;
        io.reply = None;
        io.chain_reply = None;
        if self.lineage_pending.get() {
            self.final_ack = true;
            return Ok(());
        }
        // With no new accepted work, the preceding durable checkpoint already
        // represents this pass; do not emit a conflicting same-ID replay.
        if self.held.is_none() {
            self.final_ack = self.prior_baseline_matches
                && self.baseline
                    == durable(
                        &self.scope,
                        &self.store.inner.borrow(),
                        &self.driver.chain.borrow(),
                        self.store.policy_limit.get(),
                    )?;
        }
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.future = None;
        self.held = None;
        self.driver.io.borrow_mut().pending = None;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod long_fork_tests;
