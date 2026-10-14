use crate::{
    adapters::{Acceptance, ChainView, StaticScripts},
    client::Table as NativeTable,
    memory_store::MemoryStore,
    shared_store::SharedStore,
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
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};
#[derive(Serialize, Deserialize)]
struct Continuation {
    version: u32,
    source_id: String,
    state: MemoryStore,
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
    download: u64,
    upload: u64,
    stop: Option<&'static str>,
}
#[derive(Clone)]
struct Driver {
    io: Rc<RefCell<Io>>,
    limits: Limits,
    chain: Rc<RefCell<BTreeMap<u64, String>>>,
}
impl Driver {
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
            return Ok(());
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
    future: Option<LocalBoxFuture<'static, Result<Status>>>,
    previous: Option<String>,
    baseline: Vec<u8>,
    held: Option<Checkpoint>,
    finished: Option<Status>,
    cancelled: bool,
    final_ack: bool,
}
fn encoded(store: &MemoryStore) -> Result<Vec<u8>> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(store)
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
        if limits.max_exchanges == 0
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
        let mut memory = if let Some(cp) = &checkpoint {
            if cp.source_id != scope.source_id {
                return Err(Error("source_changed"));
            }
            if cp.continuation.len() > 96 * 1024 * 1024 {
                return Err(Error("checkpoint_limit"));
            }
            let bytes = STANDARD
                .decode(&cp.continuation)
                .map_err(|_| Error("checkpoint_encoding"))?;
            let restored: Continuation = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(64 * 1024 * 1024)
                .reject_trailing_bytes()
                .deserialize(&bytes)
                .map_err(|_| Error("checkpoint_decode"))?;
            if restored.version != 2 || restored.source_id != scope.source_id {
                return Err(Error("checkpoint_source"));
            }
            let mut state = restored.state;
            state.rebuild_indexes();
            state
        } else {
            MemoryStore::new()
        };
        if let Some(cp) = &checkpoint {
            let original = encoded(&memory)?;
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
        }
        let current_scripts = scope
            .scripts
            .iter()
            .map(|s| hex::decode(&s.script).map_err(|_| Error("script")))
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        memory.retain_scope(&current_scripts);
        let baseline = encoded(&memory)?;
        let store = SharedStore {
            inner: Rc::new(RefCell::new(memory)),
            rewind: Rc::new(RefCell::new(None)),
        };
        let mut headers = BTreeMap::new();
        for point in &scope.accepted_headers {
            if !hash(&point.hash) {
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
            chain: Rc::new(RefCell::new(headers)),
        };
        let mut filters = driver.clone();
        let mut transport = driver.clone();
        let owned = driver.clone();
        let chain = Chain(driver.chain.clone());
        let mut state = store.clone();
        let request = scope.clone();
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
            if map.shards.len() > 1024
                || request
                    .scripts
                    .iter()
                    .any(|s| s.required_from < map.start_height)
            {
                return Ok(Status::Incomplete {
                    reason: "publication_incomplete".into(),
                });
            }
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
        Ok(Self {
            scope,
            store,
            driver,
            future: Some(future),
            previous: checkpoint.map(|c| c.checkpoint_id),
            baseline,
            held: None,
            finished: None,
            cancelled: false,
            final_ack: false,
        })
    }
    pub fn advance(&mut self) -> Result<Step> {
        if self.cancelled {
            return Ok(Step::Stopped {
                status: Status::Cancelled,
            });
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
        let bytes = encoded(&self.store.inner.borrow())?;
        if bytes != self.baseline || self.finished.is_some() {
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
        if state.events().map_err(|_| Error("store"))?.len() > 65536 {
            return Err(Error("event_limit"));
        }
        let mut coverage = Vec::new();
        let mut through = self.scope.anchor.height;
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
        let durable = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .serialize(&Continuation {
                version: 2,
                source_id: self.scope.source_id.clone(),
                state: state.clone(),
            })
            .map_err(|_| Error("checkpoint_encode"))?;
        if durable.len() > 64 * 1024 * 1024 {
            return Err(Error("checkpoint_limit"));
        }
        let continuation = STANDARD.encode(durable);
        let id = checkpoint_id(
            &self.scope.source_id,
            &self.scope.scope_id,
            self.scope.generation,
            &self.scope.anchor,
            complete,
            bytes,
        );
        Ok(Checkpoint {
            source_id: self.scope.source_id.clone(),
            generation: self.scope.generation,
            scope_id: self.scope.scope_id.clone(),
            checkpoint_id: id,
            previous_checkpoint_id: self.previous.clone(),
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
            rewind_to: self.store.rewind.borrow().clone().map(|a| Anchor {
                height: a.height,
                hash: a.hash,
            }),
            continuation,
        })
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
        self.baseline = encoded(&self.store.inner.borrow())?;
        self.previous = Some(id.into());
        self.final_ack = self.finished.is_some();
        self.store.rewind.borrow_mut().take();
        self.held = None;
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.future = None;
        self.held = None;
        self.driver.io.borrow_mut().pending = None;
    }
}
