//! Compact-block HTTP pipe for the WASM lab.
//!
//! Browsers will not speak cleartext HTTP/2 (`h2c`), so gRPC-Web on Traefik `:80`
//! is HTTP/1.1 and Chrome caps that at six sockets to one host. This process
//! talks **native gRPC / HTTP/2** to Zaino (many concurrent `GetBlockRange`
//! streams on two channels) and exposes chunked HTTP/1.1 `/lwd/blocks` to the
//! tab. The tab may prefetch several GETs (≤`PIPE_MAX_BLOCKS` each); in-flight
//! RPCs are capped process-wide so overlapping HTTP does not pin Zaino.

use crate::error::{EngineError, Result};
use crate::native::lwd::LwdClient;
use crate::native::rpc::pick_local_validator;
use crate::native::wallet::NativeWallet;
use crate::{LightServer, Network};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, info, warn};
use zcash_client_backend::proto::service::{
    BlockId, BlockRange as ProtoBlockRange, ChainSpec, Empty,
};

/// In-flight native `GetBlockRange` RPCs across **all** HTTP `/lwd/blocks`.
/// HTTP/2 multiplexes these; Chrome may show several chunked HTTP/1.1 bodies.
///
/// Some Zaino versions rebuild compact blocks with `block_in_place` /
/// JSON-RPC. Past ~8 in-flight RPCs, wall time does not improve and CPU spikes.
pub const PIPE_CONCURRENCY: usize = 8;
/// Parallel tonic channels. 8 streams fit on 2 HTTP/2 connections.
pub const PIPE_CHANNELS: usize = 2;
/// 1000-block RPCs stay inside Zaino’s ~120s deadline at tens of blk/s.
pub const PIPE_CHUNK: u32 = 1_000;
/// One HTTP `/lwd/blocks` call. Matches the JSON bridge and `docs/WEB.md`.
/// WASM `blockStream(from, tip)` must page at this size — 100k+ in one GET
/// holds a TCP body long enough for Windows to abort it (10053) and used
/// to take the whole process down via `accept ?`.
pub const PIPE_MAX_BLOCKS: u32 = 8_000;
/// Bound optional subtree-root lookups, including when an HTTP client leaves
/// before Zaino responds. Otherwise abandoned requests can retain gRPC tasks.
const PIPE_ROOTS_TIMEOUT: Duration = Duration::from_secs(30);
/// Coalesce HTTP chunks so empty blocks do not become one syscall each.
const HTTP_FLUSH: usize = 64 * 1024;
/// Accept / write errors that mean “that client left”, not “stop listening”.
const CLIENT_GONE_OS: &[i32] = &[
    32,    // EPIPE
    54,    // ECONNRESET (BSD)
    103,   // ECONNABORTED (Linux)
    104,   // ECONNRESET (Linux)
    110,   // ETIMEDOUT
    10052, // WSAENETRESET
    10053, // WSAECONNABORTED
    10054, // WSAECONNRESET
    10060, // WSAETIMEDOUT
];

#[derive(Clone)]
pub struct LwdPipeOpts {
    pub bind: String,
    pub zaino: LightServer,
    pub network: Network,
    pub concurrency: usize,
    pub chunk: u32,
    pub channels: usize,
    /// Loopback Zakura/Zebra JSON-RPC for `sendraw` / mempool. `None` probes local.
    pub rpc: Option<String>,
}

pub fn chunk_ranges(start: u32, end: u32, chunk: u32) -> Vec<(u32, u32)> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut h = start;
    while h <= end {
        let last = h.saturating_add(chunk - 1).min(end);
        out.push((h, last));
        if last == u32::MAX {
            break;
        }
        h = last + 1;
    }
    out
}

/// Reject a single HTTP `/lwd/blocks` that would hold the TCP body for 100k+ heights.
fn http_block_span(start: u32, end: u32) -> std::result::Result<u32, String> {
    if end < start {
        return Err("end < start".into());
    }
    let span = end.saturating_sub(start).saturating_add(1);
    if span > PIPE_MAX_BLOCKS {
        return Err(format!(
            "range {start}..={end} is {span} blocks (max {PIPE_MAX_BLOCKS})"
        ));
    }
    Ok(span)
}

pub async fn serve_lwd_pipe(opts: LwdPipeOpts) -> Result<()> {
    let addr: SocketAddr = opts
        .bind
        .parse()
        .map_err(|e| EngineError::Transport(format!("bind {}: {e}", opts.bind)))?;
    if !addr.ip().is_loopback() {
        return Err(EngineError::Transport(format!(
            "lwd-pipe bind {addr} is not loopback"
        )));
    }
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| EngineError::Transport(format!("bind {addr}: {e}")))?;
    let skip = NativeWallet::skip_ironwood_subtrees(opts.network, &opts.zaino.as_url());
    let channels = opts.channels.max(1);
    let mut clients = Vec::with_capacity(channels);
    for _i in 0..channels {
        let c = NativeWallet::connect_url_hot(&opts.zaino.as_url(), skip).await?;
        info!("lwd-pipe gRPC channel ready");
        clients.push(c);
    }
    let rpc = resolve_pipe_rpc(&opts);
    match rpc.as_deref() {
        Some(_u) => info!("lwd-pipe validator RPC for sendraw/mempool"),
        None => warn!("lwd-pipe has no loopback validator RPC; /lwd/mempool returns [] and /lwd/sendraw is 501"),
    }
    info!("lwd-pipe (native gRPC fan-out → HTTP/1.1 stream)");
    let conc = opts.concurrency.max(1);
    let state = Arc::new(PipeState {
        clients,
        next: AtomicUsize::new(0),
        zaino: opts.zaino,
        network: opts.network,
        concurrency: conc,
        chunk: opts.chunk.max(1),
        rpc,
        rpc_slots: Arc::new(Semaphore::new(conc)),
    });
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) = handle(state, stream).await {
                        if is_client_gone(&e) {
                            warn!("lwd-pipe client gone");
                        } else {
                            warn!("lwd-pipe request");
                        }
                    }
                });
            }
            Err(_e) => {
                // Windows returns 10053/10054 on accept after a client abort.
                // Never take down the process — keep listening.
                warn!("lwd-pipe accept failed; continuing");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

fn io_is_client_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::UnexpectedEof
            | ErrorKind::NotConnected
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    ) || e
        .raw_os_error()
        .is_some_and(|c| CLIENT_GONE_OS.contains(&c))
}

fn is_client_gone(err: &anyhow::Error) -> bool {
    if err.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(io_is_client_gone)
    }) {
        return true;
    }
    let s = err.to_string();
    s.contains("10053")
        || s.contains("10054")
        || s.contains("10052")
        || s.contains("broken pipe")
        || s.contains("Broken pipe")
        || s.contains("Connection reset")
        || s.contains("Connection aborted")
        || s.contains("forcibly closed")
        || s.contains("os error 32")
        || s.contains("os error 54")
        || s.contains("os error 104")
}

struct PipeState {
    clients: Vec<LwdClient>,
    next: AtomicUsize,
    zaino: LightServer,
    network: Network,
    concurrency: usize,
    chunk: u32,
    rpc: Option<String>,
    rpc_slots: Arc<Semaphore>,
}

impl PipeState {
    fn client(&self) -> LwdClient {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len().max(1);
        self.clients[i].clone()
    }
}

struct HttpReq {
    method: String,
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

async fn handle(state: Arc<PipeState>, mut stream: TcpStream) -> anyhow::Result<()> {
    let req = read_http(&mut stream).await?;
    if !origin_permitted(req.origin.as_deref()) {
        // No CORS headers: the page may not read this or any other answer.
        let body = br#"{"error":"origin not allowed"}"#;
        let head = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        return Ok(());
    }
    if req.method == "OPTIONS" {
        write_empty(&mut stream, 204, "No Content", req.origin.as_deref()).await?;
        return Ok(());
    }
    let path = req.path.split('?').next().unwrap_or("/");
    match (req.method.as_str(), path) {
        ("GET", "/health") | ("GET", "/") => {
            let body = json!({
                "ok": true,
                "mode": "lwd-pipe",
                "zaino": state.zaino.as_url(),
                "network": state.network.as_str(),
                "concurrency": state.concurrency,
                "chunk": state.chunk,
                "channels": state.clients.len(),
                "validatorRpc": state.rpc,
            });
            write_json(&mut stream, 200, "OK", req.origin.as_deref(), body).await?;
        }
        ("GET", "/lwd/tip") => match tip(&state).await {
            Ok(tip) => {
                write_json(
                    &mut stream,
                    200,
                    "OK",
                    req.origin.as_deref(),
                    json!({ "tip": tip, "server": state.zaino.as_url() }),
                )
                .await?;
            }
            Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
        },
        ("GET", "/lwd/info") => match info(&state).await {
            Ok(v) => write_json(&mut stream, 200, "OK", req.origin.as_deref(), v).await?,
            Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
        },
        ("GET", "/lwd/blocks") => {
            let start = query_u32(&req.path, "start");
            let end = query_u32(&req.path, "end");
            let (Some(start), Some(end)) = (start, end) else {
                write_err(
                    &mut stream,
                    req.origin.as_deref(),
                    EngineError::Message("start and end query params required".into()),
                )
                .await?;
                return Ok(());
            };
            if let Err(e) = stream_blocks(
                &state,
                &mut stream,
                req.origin.as_deref(),
                start,
                end,
                query_u32(&req.path, "allPools") == Some(1),
            )
            .await
            {
                if is_client_gone(&e) {
                    warn!("lwd-pipe client dropped GetBlockRange");
                } else {
                    warn!("lwd-pipe GetBlockRange");
                }
            }
        }
        ("GET", "/lwd/treestate") => {
            let Some(height) = query_u32(&req.path, "height") else {
                write_err(
                    &mut stream,
                    req.origin.as_deref(),
                    EngineError::Message("height query param required".into()),
                )
                .await?;
                return Ok(());
            };
            match NativeWallet::fetch_tree_state(&state.zaino, height).await {
                Ok(v) => write_json(&mut stream, 200, "OK", req.origin.as_deref(), v).await?,
                Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
            }
        }
        ("GET", "/lwd/subtreeroots") => {
            let proto = query_param(&req.path, "protocol").unwrap_or_else(|| "sapling".into());
            let start = query_u32(&req.path, "startIndex")
                .or_else(|| query_u32(&req.path, "start"))
                .unwrap_or(0);
            let max_entries = query_u32(&req.path, "maxEntries").unwrap_or(0);
            match tokio::time::timeout(
                PIPE_ROOTS_TIMEOUT,
                NativeWallet::fetch_subtree_roots(
                    &state.zaino,
                    state.network,
                    &proto,
                    start,
                    max_entries,
                ),
            )
            .await
            {
                Ok(Ok(roots)) => {
                    write_json(
                        &mut stream,
                        200,
                        "OK",
                        req.origin.as_deref(),
                        json!({ "roots": roots }),
                    )
                    .await?;
                }
                Ok(Err(e)) => write_err(&mut stream, req.origin.as_deref(), e).await?,
                Err(_) => {
                    write_json(
                        &mut stream,
                        504,
                        "Gateway Timeout",
                        req.origin.as_deref(),
                        json!({ "error": "GetSubtreeRoots timed out" }),
                    )
                    .await?;
                }
            }
        }
        ("GET", "/lwd/utxos") => {
            let addrs = query_param(&req.path, "addresses")
                .map(|s| {
                    s.split(',')
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if addrs.is_empty() {
                write_err(
                    &mut stream,
                    req.origin.as_deref(),
                    EngineError::Message("addresses query param required".into()),
                )
                .await?;
                return Ok(());
            }
            let start = query_u32(&req.path, "start").unwrap_or(0);
            match NativeWallet::fetch_address_utxos(&state.zaino, &addrs, start).await {
                Ok(utxos) => {
                    write_json(
                        &mut stream,
                        200,
                        "OK",
                        req.origin.as_deref(),
                        json!({ "utxos": utxos }),
                    )
                    .await?;
                }
                Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
            }
        }
        ("GET", "/lwd/tx") => {
            let Some(txid) = query_param(&req.path, "txid") else {
                write_err(
                    &mut stream,
                    req.origin.as_deref(),
                    EngineError::Message("txid query param required".into()),
                )
                .await?;
                return Ok(());
            };
            match NativeWallet::fetch_raw_transaction(&state.zaino, &txid).await {
                Ok(raw) => {
                    write_json(
                        &mut stream,
                        200,
                        "OK",
                        req.origin.as_deref(),
                        json!({
                            "txid": txid,
                            "hex": crate::web::to_hex(&raw),
                        }),
                    )
                    .await?;
                }
                Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
            }
        }
        ("GET", "/lwd/mempool") => match mempool_txs(state.rpc.as_deref()).await {
            Ok(txs) => {
                write_json(
                    &mut stream,
                    200,
                    "OK",
                    req.origin.as_deref(),
                    json!({ "txs": txs }),
                )
                .await?;
            }
            Err(e) => write_err(&mut stream, req.origin.as_deref(), e).await?,
        },
        ("POST", "/lwd/sendraw") => {
            let Some(hex) = parse_sendraw_hex(&req.body) else {
                write_json(
                    &mut stream,
                    400,
                    "Bad Request",
                    req.origin.as_deref(),
                    json!({ "error": "body must be raw hex or {\"hex\":\"...\"}" }),
                )
                .await?;
                return Ok(());
            };
            match send_raw(state.rpc.as_deref(), &hex).await {
                Ok(txid) => {
                    write_json(
                        &mut stream,
                        200,
                        "OK",
                        req.origin.as_deref(),
                        json!({ "txid": txid }),
                    )
                    .await?;
                }
                Err(e) => {
                    let (code, reason) = if e.to_string().contains("no validator RPC") {
                        (501, "Not Implemented")
                    } else {
                        (500, "Error")
                    };
                    // The node's explicit refusal carries `rejected` (see
                    // bridge.rs lwd_send_err); anything else is an unknown outcome.
                    let body = match &e {
                        EngineError::BroadcastRejected { code, message } => json!({
                            "error": e.to_string(),
                            "rejected": { "code": code, "message": message },
                        }),
                        _ => json!({ "error": e.to_string() }),
                    };
                    write_json(&mut stream, code, reason, req.origin.as_deref(), body).await?;
                }
            }
        }
        _ => {
            write_err(
                &mut stream,
                req.origin.as_deref(),
                EngineError::Message("unknown lwd-pipe route".into()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn tip(state: &PipeState) -> Result<u32> {
    let mut client = state.client();
    client
        .get_latest_block(ChainSpec::default())
        .await
        .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
        .into_inner()
        .height
        .try_into()
        .map_err(|_| EngineError::Message("tip out of range".into()))
}

async fn info(state: &PipeState) -> Result<Value> {
    let mut client = state.client();
    let inf = client
        .get_lightd_info(Empty {})
        .await
        .map_err(|e| EngineError::Transport(format!("GetLightdInfo: {e}")))?
        .into_inner();
    Ok(json!({
        "chain": inf.chain_name,
        "blockHeight": inf.block_height,
        "vendor": inf.vendor,
        "version": inf.version,
        "protocolVersion": inf.lightwallet_protocol_version,
        "transparentCompact": true,
        "tScan": state.zaino.allows_transparent_query(),
        "concurrency": state.concurrency,
        "chunk": state.chunk,
        "channels": state.clients.len(),
        "mode": "lwd-pipe",
    }))
}

async fn stream_blocks(
    state: &PipeState,
    stream: &mut TcpStream,
    origin: Option<&str>,
    start: u32,
    end: u32,
    all_pools: bool,
) -> anyhow::Result<()> {
    if end < start {
        write_err(stream, origin, EngineError::Message("end < start".into())).await?;
        return Ok(());
    }
    if let Err(msg) = http_block_span(start, end) {
        write_json(stream, 400, "Bad Request", origin, json!({ "error": msg })).await?;
        return Ok(());
    }
    let ranges = chunk_ranges(start, end, state.chunk);
    write_chunked_headers(stream, origin).await?;
    let conc = state.concurrency.max(1);
    let buffer_cap = conc.saturating_mul(2).max(conc);
    let (tx, mut rx) = mpsc::channel::<PipeMsg>(conc.saturating_mul(64).max(512));
    let mut to_launch: VecDeque<(u32, u32)> = ranges.iter().copied().collect();
    let mut in_flight = 0usize;
    let mut merge = RangeMerge::new(start, end, ranges);
    let mut out_buf = Vec::new();

    loop {
        while in_flight < conc && merge.buffered() < buffer_cap && !to_launch.is_empty() {
            let (a, b) = to_launch.pop_front().expect("to_launch");
            in_flight += 1;
            let mut client = state.client();
            let send = tx.clone();
            let slots = Arc::clone(&state.rpc_slots);
            tokio::spawn(async move {
                let permit = match slots.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = send
                            .send(PipeMsg::Failed {
                                start: a,
                                err: "pipe shutting down".into(),
                            })
                            .await;
                        return;
                    }
                };
                fetch_range_stream(&mut client, a, b, send, all_pools).await;
                drop(permit);
            });
        }
        if merge.want > end {
            break;
        }
        if in_flight == 0 && to_launch.is_empty() {
            if merge.want <= end {
                match merge.retry_want() {
                    Ok(a) => {
                        if let Some(b) = merge.last_of(a) {
                            warn!("lwd-pipe stream stalled; retrying");
                            to_launch.push_front((a, b));
                            continue;
                        }
                    }
                    Err(_e) => {
                        warn!("lwd-pipe stream stalled");
                        return Err(anyhow::anyhow!(
                            "lwd-pipe stalled at {} (end {end})",
                            merge.want
                        ));
                    }
                }
            }
            break;
        }
        let msg = match tokio::time::timeout(Duration::from_secs(180), rx.recv()).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                warn!("lwd-pipe channel closed");
                return Err(anyhow::anyhow!(
                    "lwd-pipe stalled at {} (end {end})",
                    merge.want
                ));
            }
            Err(_) => {
                warn!("lwd-pipe GetBlockRange timed out");
                match merge.retry_want() {
                    Ok(a) => {
                        if let Some(b) = merge.last_of(a) {
                            to_launch.push_front((a, b));
                            continue;
                        }
                    }
                    Err(e) => return Err(anyhow::anyhow!("{e}")),
                }
                return Err(anyhow::anyhow!(
                    "lwd-pipe stalled at {} (end {end})",
                    merge.want
                ));
            }
        };
        match msg {
            PipeMsg::Failed { start: a, err } => {
                in_flight = in_flight.saturating_sub(1);
                match merge.on_failed(a) {
                    Ok(retry_a) => {
                        warn!("lwd-pipe retry GetBlockRange");
                        if let Some(b) = merge.last_of(retry_a) {
                            to_launch.push_front((retry_a, b));
                        }
                    }
                    Err(_e) => {
                        warn!("lwd-pipe GetBlockRange failed");
                        return Err(anyhow::anyhow!("GetBlockRange {a}: {err}"));
                    }
                }
            }
            PipeMsg::Bytes { start: a, data } => {
                if let Some(data) = merge.on_bytes(a, data) {
                    push_http(stream, &mut out_buf, &data).await?;
                }
            }
            PipeMsg::Done { start: a } => {
                in_flight = in_flight.saturating_sub(1);
                match merge.on_done(a) {
                    DoneAction::Buffered => {}
                    DoneAction::Retry { start: retry_a } => match merge.bump_retry(retry_a) {
                        Ok(_) => {
                            warn!("lwd-pipe empty range; retrying without skipping");
                            if let Some(b) = merge.last_of(retry_a) {
                                to_launch.push_front((retry_a, b));
                            }
                        }
                        Err(e) => {
                            warn!("lwd-pipe empty range");
                            return Err(anyhow::anyhow!("{e}"));
                        }
                    },
                    DoneAction::Advance { leftover, drain } => {
                        if !leftover.is_empty() {
                            push_http(stream, &mut out_buf, &leftover).await?;
                        }
                        flush_http(stream, &mut out_buf).await?;
                        if !drain.is_empty() {
                            push_http(stream, &mut out_buf, &drain).await?;
                            flush_http(stream, &mut out_buf).await?;
                        }
                        if merge.want > end {
                            write_last_chunk(stream).await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    flush_http(stream, &mut out_buf).await?;
    write_last_chunk(stream).await?;
    Ok(())
}

const MAX_RANGE_RETRIES: u8 = 4;

#[derive(Default)]
struct PendingRange {
    data: Vec<u8>,
    done: bool,
}

enum PipeMsg {
    Bytes { start: u32, data: Vec<u8> },
    Done { start: u32 },
    Failed { start: u32, err: String },
}

#[derive(Debug)]
enum DoneAction {
    Buffered,
    Retry { start: u32 },
    Advance { leftover: Vec<u8>, drain: Vec<u8> },
}

/// Ordered merge: never emit range `a+chunk` until range `a` has real bytes.
struct RangeMerge {
    want: u32,
    end: u32,
    ranges: Vec<(u32, u32)>,
    pending: BTreeMap<u32, PendingRange>,
    wrote_current: bool,
    retries: BTreeMap<u32, u8>,
}

impl RangeMerge {
    fn new(start: u32, end: u32, ranges: Vec<(u32, u32)>) -> Self {
        Self {
            want: start,
            end,
            ranges,
            pending: BTreeMap::new(),
            wrote_current: false,
            retries: BTreeMap::new(),
        }
    }

    fn last_of(&self, start: u32) -> Option<u32> {
        self.ranges
            .iter()
            .find(|(s, _)| *s == start)
            .map(|(_, l)| *l)
    }

    fn buffered(&self) -> usize {
        self.pending.len()
    }

    fn bump_retry(&mut self, start: u32) -> std::result::Result<u32, String> {
        let n = self.retries.entry(start).or_insert(0);
        *n = n.saturating_add(1);
        if *n > MAX_RANGE_RETRIES {
            return Err(format!("range {start} failed after {} tries", *n));
        }
        Ok(start)
    }

    fn on_bytes(&mut self, start: u32, data: Vec<u8>) -> Option<Vec<u8>> {
        if start == self.want {
            self.wrote_current = true;
            Some(data)
        } else {
            self.pending
                .entry(start)
                .or_default()
                .data
                .extend_from_slice(&data);
            None
        }
    }

    fn on_done(&mut self, start: u32) -> DoneAction {
        if start != self.want {
            let p = self.pending.entry(start).or_default();
            if p.data.is_empty() {
                return DoneAction::Retry { start };
            }
            p.done = true;
            return DoneAction::Buffered;
        }
        let p = self.pending.remove(&start).unwrap_or_default();
        if !self.wrote_current && p.data.is_empty() {
            return DoneAction::Retry { start };
        }
        let Some(last) = self.last_of(start) else {
            return DoneAction::Retry { start };
        };
        self.wrote_current = false;
        self.want = if last == self.end {
            self.end.saturating_add(1)
        } else {
            last.saturating_add(1)
        };
        let leftover = p.data;
        let drain = self.drain_ready();
        DoneAction::Advance { leftover, drain }
    }

    fn on_failed(&mut self, start: u32) -> std::result::Result<u32, String> {
        self.pending.remove(&start);
        if start == self.want && self.wrote_current {
            return Err("already streamed a prefix of this range".into());
        }
        self.bump_retry(start)
    }

    fn retry_want(&mut self) -> std::result::Result<u32, String> {
        if self.wrote_current {
            return Err("already streamed a prefix of this range".into());
        }
        self.pending.remove(&self.want);
        self.bump_retry(self.want)
    }

    fn drain_ready(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let Some(p) = self.pending.remove(&self.want) else {
                return out;
            };
            if p.done && p.data.is_empty() {
                return out;
            }
            if !p.data.is_empty() {
                out.extend_from_slice(&p.data);
                self.wrote_current = true;
            }
            if !p.done {
                return out;
            }
            let Some(last) = self.last_of(self.want) else {
                return out;
            };
            self.wrote_current = false;
            self.want = if last == self.end {
                self.end.saturating_add(1)
            } else {
                last.saturating_add(1)
            };
        }
    }
}

async fn fetch_range_stream(
    client: &mut LwdClient,
    start: u32,
    end: u32,
    tx: mpsc::Sender<PipeMsg>,
    all_pools: bool,
) {
    debug!("lwd-pipe GetBlockRange");
    let result = async {
        let mut rpc = client
            .get_block_range(ProtoBlockRange {
                start: Some(BlockId {
                    height: u64::from(start),
                    hash: vec![],
                }),
                end: Some(BlockId {
                    height: u64::from(end),
                    hash: vec![],
                }),
                pool_types: if all_pools { vec![1, 2, 3, 4] } else { vec![] },
            })
            .await
            .map_err(|e| format!("GetBlockRange: {e}"))?
            .into_inner();
        let mut next = start;
        let mut prev_hash: Option<Vec<u8>> = None;
        loop {
            match rpc.message().await {
                Ok(Some(block)) => {
                    let h = u32::try_from(block.height)
                        .map_err(|_| "block height overflow".to_string())?;
                    if h != next {
                        return Err(format!(
                            "GetBlockRange gap: expected height {next}, got {h}"
                        ));
                    }
                    if let Some(prev) = prev_hash.as_ref() {
                        if !block.prev_hash.is_empty() && &block.prev_hash != prev {
                            return Err(format!("GetBlockRange prev_hash mismatch at {h}"));
                        }
                    }
                    prev_hash = Some(block.hash.clone());
                    let data = crate::web::encode_one(&block);
                    if tx.send(PipeMsg::Bytes { start, data }).await.is_err() {
                        return Ok(());
                    }
                    if next == end {
                        next = next.saturating_add(1);
                    } else {
                        next += 1;
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(format!("GetBlockRange stream: {e}")),
            }
        }
        let want_next = if end == u32::MAX {
            0
        } else {
            end.saturating_add(1)
        };
        if next != want_next {
            return Err(format!(
                "GetBlockRange {start}..={end}: ended at {next}, want {want_next}"
            ));
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            let _ = tx.send(PipeMsg::Done { start }).await;
        }
        Err(err) => {
            let _ = tx.send(PipeMsg::Failed { start, err }).await;
        }
    }
}

async fn push_http(stream: &mut TcpStream, buf: &mut Vec<u8>, data: &[u8]) -> anyhow::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    buf.extend_from_slice(data);
    if buf.len() >= HTTP_FLUSH {
        flush_http(stream, buf).await?;
    }
    Ok(())
}

async fn flush_http(stream: &mut TcpStream, buf: &mut Vec<u8>) -> anyhow::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    write_chunk(stream, buf).await?;
    buf.clear();
    Ok(())
}

/// Only pages served from this machine may read pipe answers. The pipe is
/// loopback-only, but a public website open in the same browser could
/// otherwise read node health, UTXO and mempool answers and relay
/// transactions. Requests without an Origin (non-browser clients) pass;
/// `Z_STACK_PIPE_ORIGIN` adds comma-separated origins.
fn origin_permitted(origin: Option<&str>) -> bool {
    let Some(origin) = origin.filter(|o| !o.is_empty()) else {
        return true;
    };
    loopback_origin(origin)
        || std::env::var("Z_STACK_PIPE_ORIGIN")
            .ok()
            .is_some_and(|list| list.split(',').any(|o| o.trim() == origin))
}

fn loopback_origin(origin: &str) -> bool {
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some(parts) => parts,
            None => return false,
        },
        None => match authority.find(':') {
            Some(i) => authority.split_at(i),
            None => (authority, ""),
        },
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn cors(origin: Option<&str>) -> String {
    match origin {
        Some(o) if !o.is_empty() => format!(
            "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\nAccess-Control-Allow-Credentials: true\r\n"
        ),
        _ => "Access-Control-Allow-Origin: *\r\n".into(),
    }
}

fn common_headers(origin: Option<&str>) -> String {
    format!(
        "{}Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type, authorization, x-grpc-web, x-z-stack-token\r\nX-Z-Stack-Pipe: 1\r\n",
        cors(origin)
    )
}

async fn write_empty(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    origin: Option<&str>,
) -> anyhow::Result<()> {
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n{}\r\n",
        common_headers(origin)
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(http_write_err)?;
    stream.flush().await.map_err(http_write_err)?;
    Ok(())
}

async fn write_json(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    origin: Option<&str>,
    body: Value,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&body)?;
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        bytes.len(),
        common_headers(origin)
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(http_write_err)?;
    stream.write_all(&bytes).await.map_err(http_write_err)?;
    stream.flush().await.map_err(http_write_err)?;
    Ok(())
}

/// An unreachable or timed-out light server is 503 so the browser keeps its
/// outage grace and backoff; a plain 500 read as permanent and failed the
/// sync the moment Zaino restarted behind a live pipe.
fn error_status(e: &EngineError) -> (u16, &'static str) {
    if e.to_string().contains("unknown") {
        (404, "Not Found")
    } else if super::pipeline::is_light_connection_outage(e) {
        (503, "Service Unavailable")
    } else {
        (500, "Error")
    }
}

async fn write_err(
    stream: &mut TcpStream,
    origin: Option<&str>,
    e: EngineError,
) -> anyhow::Result<()> {
    let (code, reason) = error_status(&e);
    write_json(
        stream,
        code,
        reason,
        origin,
        json!({ "error": e.to_string() }),
    )
    .await
}

async fn write_chunked_headers(stream: &mut TcpStream, origin: Option<&str>) -> anyhow::Result<()> {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nConnection: close\r\n{}\r\n",
        common_headers(origin)
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(http_write_err)?;
    stream.flush().await.map_err(http_write_err)?;
    Ok(())
}

async fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> anyhow::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let head = format!("{:x}\r\n", data.len());
    write_all_http(stream, head.as_bytes()).await?;
    write_all_http(stream, data).await?;
    write_all_http(stream, b"\r\n").await?;
    stream.flush().await.map_err(http_write_err)?;
    Ok(())
}

async fn write_last_chunk(stream: &mut TcpStream) -> anyhow::Result<()> {
    write_all_http(stream, b"0\r\n\r\n").await?;
    stream.flush().await.map_err(http_write_err)?;
    Ok(())
}

async fn write_all_http(stream: &mut TcpStream, data: &[u8]) -> anyhow::Result<()> {
    stream.write_all(data).await.map_err(http_write_err)
}

fn http_write_err(e: std::io::Error) -> anyhow::Error {
    anyhow::Error::from(e)
}

const MAX_BODY: usize = 2 * 1024 * 1024;

async fn read_http(stream: &mut TcpStream) -> anyhow::Result<HttpReq> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("client closed");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            anyhow::bail!("request too large");
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        }
    };
    let header = std::str::from_utf8(&buf[..header_end])?;
    let mut lines = header.split("\r\n");
    let req = lines.next().unwrap_or("");
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut origin = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("Origin") {
            origin = Some(v.trim().to_string());
        } else if k.trim().eq_ignore_ascii_case("Content-Length") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY {
        anyhow::bail!("body too large");
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > MAX_BODY {
            anyhow::bail!("body too large");
        }
    }
    body.truncate(content_length);
    Ok(HttpReq {
        method,
        path,
        origin,
        body,
    })
}

fn resolve_pipe_rpc(opts: &LwdPipeOpts) -> Option<String> {
    let explicit = opts
        .rpc
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let url = explicit.or_else(|| pick_local_validator(opts.network).map(|p| p.url));
    url.filter(|u| rpc_is_loopback(u))
}

fn rpc_is_loopback(url: &str) -> bool {
    let rest = url
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = rest.split([':', '/', ']']).next().unwrap_or("");
    let host = host.trim_start_matches('[');
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

async fn mempool_txs(rpc: Option<&str>) -> Result<Vec<Value>> {
    let Some(rpc) = rpc.map(str::to_string) else {
        return Ok(Vec::new());
    };
    tokio::task::spawn_blocking(move || crate::native::rpc::fetch_mempool_txs(&rpc, 32))
        .await
        .map_err(|e| EngineError::Transport(format!("mempool: {e}")))?
}

async fn send_raw(rpc: Option<&str>, hex: &str) -> Result<String> {
    let Some(rpc) = rpc.map(str::to_string) else {
        return Err(EngineError::Message(
            "no validator RPC (set --rpc or Z_STACK_VALIDATOR_RPC to loopback Zakura/Zebra)".into(),
        ));
    };
    let raw = crate::web::from_hex(hex).map_err(EngineError::Message)?;
    tokio::task::spawn_blocking(move || crate::native::rpc::send_raw_transaction(&rpc, &raw))
        .await
        .map_err(|e| EngineError::Transport(format!("sendraw: {e}")))?
}

fn parse_sendraw_hex(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        if let Some(h) = v.get("hex").and_then(|x| x.as_str()) {
            return Some(h.trim().trim_start_matches("0x").to_string());
        }
    }
    let t = s.trim_start_matches("0x");
    if t.bytes().all(|c| c.is_ascii_hexdigit()) && t.len() % 2 == 0 {
        Some(t.to_string())
    } else {
        None
    }
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    for part in q.split('&') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k == key {
            return Some(urlencoding_decode(v));
        }
    }
    None
}

fn query_u32(path: &str, key: &str) -> Option<u32> {
    query_param(path, key)?.parse().ok()
}

fn urlencoding_decode(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h =
                    u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16)
                        .unwrap_or(b'?');
                out.push(h as char);
                i += 3;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_ranges, error_status, http_block_span, io_is_client_gone, is_client_gone,
        origin_permitted, parse_sendraw_hex, rpc_is_loopback, DoneAction, RangeMerge,
        PIPE_MAX_BLOCKS,
    };
    use crate::error::EngineError;

    #[test]
    fn an_unreachable_light_server_is_a_retryable_answer() {
        let refused =
            EngineError::Transport("connect http://127.0.0.1:8138: transport error".into());
        assert_eq!(error_status(&refused).0, 503);
        let timeout = EngineError::Transport("GetTreeState at 10 timed out after 45s".into());
        assert_eq!(error_status(&timeout).0, 503);
        let unknown = EngineError::Message("unknown transaction".into());
        assert_eq!(error_status(&unknown).0, 404);
        let invalid = EngineError::Message("invalid height".into());
        assert_eq!(error_status(&invalid).0, 500);
    }

    #[test]
    fn only_pages_served_from_this_machine_may_read_pipe_answers() {
        for ok in [
            "http://localhost:5174",
            "http://127.0.0.1:15176",
            "http://[::1]:5174",
            "https://localhost",
        ] {
            assert!(origin_permitted(Some(ok)), "{ok}");
        }
        assert!(origin_permitted(None), "non-browser clients send no Origin");
        for bad in [
            "https://example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.nip.io:5174",
            "http://localhost:5174@example.com",
            "http://[::1].example.com",
            "null",
            "file://",
        ] {
            assert!(!origin_permitted(Some(bad)), "{bad}");
        }
    }

    #[test]
    fn splits_inclusive_ranges() {
        assert_eq!(
            chunk_ranges(1, 2500, 1000),
            vec![(1, 1000), (1001, 2000), (2001, 2500)]
        );
        assert_eq!(chunk_ranges(10, 10, 1000), vec![(10, 10)]);
        assert_eq!(chunk_ranges(1, 1000, 1000), vec![(1, 1000)]);
    }

    #[test]
    fn http_range_rejects_100k_in_one_get() {
        assert_eq!(http_block_span(1, 8000).unwrap(), 8000);
        assert!(http_block_span(1, PIPE_MAX_BLOCKS + 1).is_err());
        assert!(http_block_span(3_368_308, 3_472_343).is_err());
        assert!(http_block_span(10, 9).is_err());
    }

    #[test]
    fn empty_first_chunk_does_not_emit_next() {
        let ranges = chunk_ranges(3_368_308, 3_370_307, 1_000);
        assert_eq!(ranges[0], (3_368_308, 3_369_307));
        assert_eq!(ranges[1], (3_369_308, 3_370_307));
        let mut m = RangeMerge::new(3_368_308, 3_370_307, ranges);
        assert!(m.on_bytes(3_369_308, b"second".to_vec()).is_none());
        match m.on_done(3_368_308) {
            DoneAction::Retry { start } => assert_eq!(start, 3_368_308),
            other => panic!("must retry first chunk, got skip-like {other:?}"),
        }
        assert_eq!(m.want, 3_368_308);
        assert!(m.on_failed(3_368_308).is_ok());
        assert_eq!(m.want, 3_368_308);
        let first = m.on_bytes(3_368_308, b"first".to_vec());
        assert_eq!(first.as_deref(), Some(b"first".as_slice()));
        match m.on_done(3_368_308) {
            DoneAction::Advance { leftover, drain } => {
                assert!(leftover.is_empty());
                assert_eq!(drain, b"second");
            }
            other => panic!("expected advance after real first chunk: {other:?}"),
        }
        assert_eq!(m.want, 3_369_308);
    }

    #[test]
    fn windows_abort_is_client_gone_not_fatal() {
        let e = std::io::Error::from_raw_os_error(10053);
        assert!(io_is_client_gone(&e));
        let e = std::io::Error::from_raw_os_error(10054);
        assert!(io_is_client_gone(&e));
        let wrapped = anyhow::Error::from(std::io::Error::from_raw_os_error(10053));
        assert!(is_client_gone(&wrapped));
        assert!(is_client_gone(&anyhow::anyhow!(
            "An established connection was aborted by the software in your host machine. (os error 10053)"
        )));
    }

    #[test]
    fn sendraw_hex_json_or_raw() {
        assert_eq!(
            parse_sendraw_hex(br#"{"hex":"deadbeef"}"#).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(parse_sendraw_hex(b"0xab").as_deref(), Some("ab"));
        assert!(parse_sendraw_hex(b"nope").is_none());
    }

    #[test]
    fn validator_rpc_must_be_loopback() {
        assert!(rpc_is_loopback("http://127.0.0.1:8232"));
        assert!(rpc_is_loopback("http://localhost:8232"));
        assert!(!rpc_is_loopback("http://203.0.113.9:8232"));
    }
}
