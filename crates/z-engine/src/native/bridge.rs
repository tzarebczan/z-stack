//! Loopback JSON API so the local web UI can drive [`NativeWallet`].
//!
//! Bind **127.0.0.1 only**. This is not a public wallet server; seed unlock uses
//! the same `SeedAuth` as the CLI. Browser gRPC-Web remains the hosted path.

use crate::error::{EngineError, Result};
use crate::native::rpc::{pick_local_validator, probe_validator};
use crate::native::wallet::{NativeWallet, SeedAuth};
use crate::{format_zatoshis, parse_zec_to_zatoshis, zip321_uri, Balance, LightServer, Network};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

const DEFAULT_ORIGINS: &[&str] = &["http://127.0.0.1:5173", "http://localhost:5173"];

pub struct Bridge {
    wallet: PathBuf,
    auth: Arc<StdMutex<SeedAuth>>,
    lock: Mutex<()>,
    token: String,
}

#[derive(Debug)]
struct HttpReq {
    method: String,
    path: String,
    origin: Option<String>,
    token: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize, Default)]
struct ShieldBody {
    threshold: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct MaxSendBody {
    to: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct InspectBody {
    address: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SendBody {
    to: Option<String>,
    #[serde(rename = "amountZec")]
    amount_zec: Option<String>,
    amount: Option<u64>,
    memo: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CreateBody {
    network: Option<String>,
    birthday: Option<serde_json::Value>,
    server: Option<String>,
    #[serde(rename = "validatorRpc")]
    validator_rpc: Option<String>,
    passphrase: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RestoreBody {
    mnemonic: Option<String>,
    ufvk: Option<String>,
    /// Height, `"YYYY-MM-DD"`, or omitted (regtest → 1).
    birthday: Option<serde_json::Value>,
    network: Option<String>,
    server: Option<String>,
    #[serde(rename = "validatorRpc")]
    validator_rpc: Option<String>,
    passphrase: Option<String>,
    #[serde(rename = "unlockPolicy")]
    unlock_policy: Option<crate::native::UnlockPolicy>,
}

#[derive(Debug, Deserialize, Default)]
struct AttachBody {
    mnemonic: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UnlockPolicyBody {
    policy: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SetupBody {
    network: Option<String>,
    server: Option<String>,
    #[serde(rename = "validatorRpc")]
    validator_rpc: Option<String>,
}

impl Bridge {
    pub fn new(wallet: impl Into<PathBuf>, auth: SeedAuth) -> Self {
        Self::from_shared(wallet, Arc::new(StdMutex::new(auth)))
    }

    pub fn from_shared(wallet: impl Into<PathBuf>, auth: Arc<StdMutex<SeedAuth>>) -> Self {
        Self::from_shared_token(wallet, auth, random_bridge_token())
    }

    pub fn from_shared_token(
        wallet: impl Into<PathBuf>,
        auth: Arc<StdMutex<SeedAuth>>,
        token: String,
    ) -> Self {
        Self {
            wallet: wallet.into(),
            auth,
            lock: Mutex::new(()),
            token,
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn current_auth(&self) -> SeedAuth {
        self.auth.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn token_ok(&self, req: &HttpReq) -> bool {
        if req.token.as_deref() == Some(self.token.as_str()) {
            return true;
        }
        query_param(&req.path, "token").as_deref() == Some(self.token.as_str())
    }

    /// Serve until the process is killed. `bind` must be a loopback socket.
    pub async fn serve(self, bind: &str) -> Result<()> {
        let addr = parse_loopback_bind(bind)?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| EngineError::Transport(format!("bind {addr}: {e}")))?;
        info!("native wallet bridge");
        let this = Arc::new(self);
        loop {
            let (stream, peer) = listener
                .accept()
                .await
                .map_err(|e| EngineError::Transport(format!("accept: {e}")))?;
            if !peer.ip().is_loopback() {
                warn!("dropped non-loopback client");
                continue;
            }
            let this = Arc::clone(&this);
            tokio::spawn(async move {
                if let Err(_e) = this.handle(stream).await {
                    warn!("bridge request");
                }
            });
        }
    }

    async fn handle(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let req = read_http(&mut stream).await?;
        let allowed = origin_allowed(req.origin.as_deref());
        if req.method == "OPTIONS" {
            write_http(
                &mut stream,
                204,
                "No Content",
                req.origin.as_deref(),
                allowed,
                b"",
            )
            .await?;
            return Ok(());
        }
        if !allowed {
            let body = json!({ "error": "origin not allowed" }).to_string();
            write_http(
                &mut stream,
                403,
                "Forbidden",
                req.origin.as_deref(),
                false,
                body.as_bytes(),
            )
            .await?;
            return Ok(());
        }
        let path = req.path.split('?').next().unwrap_or("/");
        let open = req.method == "GET" && (path == "/" || path == "/health");
        if !open && !self.token_ok(&req) {
            let msg = if req.token.is_none() {
                "bridge token required — paste the token printed by `z-wallet serve` (Authorization: Bearer). This is not a Zaino password."
            } else {
                "bridge token invalid — serve prints a new token each start; copy it into the web lab Bridge token field."
            };
            let body = json!({ "error": msg }).to_string();
            write_http(
                &mut stream,
                401,
                "Unauthorized",
                req.origin.as_deref(),
                allowed,
                body.as_bytes(),
            )
            .await?;
            return Ok(());
        }
        let path = req.path.split('?').next().unwrap_or(&req.path);
        if path.starts_with("/lwd/") {
            let (code, reason, ctype, payload) = self.dispatch_lwd(&req).await;
            write_http_typed(
                &mut stream,
                code,
                reason,
                req.origin.as_deref(),
                true,
                ctype,
                &payload,
            )
            .await?;
            return Ok(());
        }
        let (code, reason, payload) = self.dispatch(&req).await;
        write_http(
            &mut stream,
            code,
            reason,
            req.origin.as_deref(),
            true,
            payload.as_bytes(),
        )
        .await?;
        Ok(())
    }

    async fn dispatch_lwd(&self, req: &HttpReq) -> (u16, &'static str, &'static str, Vec<u8>) {
        let path = req.path.split('?').next().unwrap_or(&req.path);
        let network = query_param(&req.path, "network")
            .and_then(|s| Network::parse(&s))
            .unwrap_or(Network::Regtest);
        let server = match query_param(&req.path, "server") {
            Some(u) if !u.trim().is_empty() => LightServer::parse(&u, network),
            _ => {
                if self.wallet_exists() {
                    match NativeWallet::open(&self.wallet) {
                        Ok(w) => LightServer::parse(&w.server_url(), network),
                        Err(_) => LightServer::for_network(network),
                    }
                } else {
                    LightServer::for_network(network)
                }
            }
        };
        match (req.method.as_str(), path) {
            ("GET", "/lwd/tip") => match NativeWallet::fetch_tip(&server).await {
                Ok(tip) => {
                    json_ok_bytes(serde_json::json!({ "tip": tip, "server": server.as_url() }))
                }
                Err(e) => lwd_err(e),
            },
            ("GET", "/lwd/blocks") => {
                let start = query_param(&req.path, "start").and_then(|s| s.parse().ok());
                let end = query_param(&req.path, "end").and_then(|s| s.parse().ok());
                let (Some(start), Some(end)) = (start, end) else {
                    return lwd_msg(400, "start and end query params required");
                };
                match NativeWallet::fetch_compact_block_blob(&server, start, end).await {
                    Ok(blob) => (200, "OK", "application/octet-stream", blob),
                    Err(e) => lwd_err(e),
                }
            }
            ("POST", "/lwd/sendraw") => {
                let hex = parse_sendraw_hex(&req.body);
                let Some(hex) = hex else {
                    return lwd_msg(400, "body must be raw hex or {\"hex\":\"...\"}");
                };
                let raw = match crate::web::from_hex(&hex) {
                    Ok(b) => b,
                    Err(e) => return lwd_msg(400, &e),
                };
                let Some(rpc) = lwd_validator_rpc(req, &server, &self.wallet) else {
                    return lwd_msg(
                        501,
                        "no validator RPC (set local Zakura / Zebra, or Z_STACK_VALIDATOR_RPC)",
                    );
                };
                match crate::native::rpc::send_raw_transaction(&rpc, &raw) {
                    Ok(hash) => json_ok_bytes(serde_json::json!({ "txid": hash })),
                    Err(e) => lwd_send_err(e),
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
                    return lwd_msg(400, "addresses query param required");
                }
                let start = query_param(&req.path, "start")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                match NativeWallet::fetch_address_utxos(&server, &addrs, start).await {
                    Ok(utxos) => json_ok_bytes(serde_json::json!({ "utxos": utxos })),
                    Err(e) => lwd_err(e),
                }
            }
            ("GET", "/lwd/treestate") => {
                let height = query_param(&req.path, "height").and_then(|s| s.parse().ok());
                let Some(height) = height else {
                    return lwd_msg(400, "height query param required");
                };
                match NativeWallet::fetch_tree_state(&server, height).await {
                    Ok(v) => json_ok_bytes(v),
                    Err(e) => lwd_err(e),
                }
            }
            ("GET", "/lwd/subtreeroots") => {
                let proto = query_param(&req.path, "protocol").unwrap_or_else(|| "sapling".into());
                let start = query_param(&req.path, "startIndex")
                    .or_else(|| query_param(&req.path, "start"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let max_entries = query_param(&req.path, "maxEntries")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                match NativeWallet::fetch_subtree_roots(
                    &server,
                    network,
                    &proto,
                    start,
                    max_entries,
                )
                .await
                {
                    Ok(roots) => json_ok_bytes(serde_json::json!({ "roots": roots })),
                    Err(e) => lwd_err(e),
                }
            }
            ("GET", "/lwd/tx") => {
                let Some(txid) = query_param(&req.path, "txid") else {
                    return lwd_msg(400, "txid query param required");
                };
                match NativeWallet::fetch_raw_transaction(&server, &txid).await {
                    Ok(raw) => json_ok_bytes(serde_json::json!({
                        "txid": txid,
                        "hex": crate::web::to_hex(&raw),
                    })),
                    Err(e) => lwd_err(e),
                }
            }
            ("GET", "/lwd/mempool") => {
                let Some(rpc) = lwd_validator_rpc(req, &server, &self.wallet) else {
                    return lwd_msg(
                        501,
                        "no validator RPC (set local Zakura / Zebra, or Z_STACK_VALIDATOR_RPC)",
                    );
                };
                match crate::native::rpc::fetch_mempool_txs(&rpc, 32) {
                    Ok(txs) => json_ok_bytes(serde_json::json!({ "txs": txs })),
                    Err(e) => lwd_err(e),
                }
            }
            ("POST", "/lwd/mine") => {
                let n = parse_mine_n(&req.path, &req.body);
                let Some(rpc) = lwd_validator_rpc(req, &server, &self.wallet) else {
                    return lwd_msg(
                        501,
                        "no validator RPC (set local Zakura / Zebra, or Z_STACK_VALIDATOR_RPC)",
                    );
                };
                match crate::native::rpc::generate_blocks(&rpc, n) {
                    Ok(result) => {
                        let mined = result.as_array().map(|a| a.len() as u32).unwrap_or(n);
                        json_ok_bytes(serde_json::json!({ "mined": mined, "result": result }))
                    }
                    Err(e) => lwd_err(e),
                }
            }
            _ => lwd_msg(404, "unknown /lwd route"),
        }
    }

    async fn dispatch(&self, req: &HttpReq) -> (u16, &'static str, String) {
        let path = req.path.split('?').next().unwrap_or(&req.path);
        match (req.method.as_str(), path) {
            ("GET", "/health") | ("GET", "/") => (
                200,
                "OK",
                json!({
                    "ok": true,
                    "wallet": self.wallet_exists(),
                    "bind": "loopback",
                    "mode": "native-bridge",
                })
                .to_string(),
            ),
            ("GET", "/setup/probe") => {
                let network = query_param(&req.path, "network")
                    .and_then(|s| Network::parse(&s))
                    .unwrap_or(Network::Testnet);
                let server = match query_param(&req.path, "server") {
                    Some(u) if !u.trim().is_empty() => LightServer::parse(&u, network),
                    _ => LightServer::local_for_network(network),
                };
                let light = NativeWallet::probe_light(&server).await;
                let validator = match query_param(&req.path, "rpc") {
                    Some(u) if !u.trim().is_empty() => probe_validator(&u),
                    _ => pick_local_validator(network).unwrap_or_else(|| {
                        probe_validator(LightServer::local_validator_rpc(network))
                    }),
                };
                (
                    200,
                    "OK",
                    json!({
                        "network": network.as_str(),
                        "light": light,
                        "validator": validator,
                        "defaults": {
                            "light": LightServer::local_for_network(network).as_url(),
                            "validatorRpc": LightServer::local_validator_rpc(network),
                            "publicLight": match network {
                                Network::Testnet => LightServer::TESTNET_PUBLIC_LWD,
                                Network::Mainnet => "https://zec.rocks:443",
                                Network::Regtest => LightServer::LOCAL_REGTEST_GRPC,
                            },
                        },
                    })
                    .to_string(),
                )
            }
            ("POST", "/setup") => match parse_setup(&req.body) {
                Err(e) => json_err(400, &e),
                Ok((network, server, rpc)) => {
                    let dir = self.wallet.clone();
                    self.locked(async move {
                        let mut w = NativeWallet::open(&dir)?;
                        if w.network() != network {
                            return Err(anyhow::anyhow!(
                                "wallet is {} — create/restore for {}",
                                w.network().as_str(),
                                network.as_str()
                            ));
                        }
                        w.set_light_server(server)?;
                        if let Some(r) = rpc {
                            w.set_validator_rpc(if r.trim().is_empty() { None } else { Some(r) })?;
                        }
                        snapshot_json(&dir)
                    })
                    .await
                }
            },
            ("GET", "/wallet") => {
                let dir = self.wallet.clone();
                self.locked(async move { snapshot_json(&dir) }).await
            }
            ("POST", "/sync") => {
                let dir = self.wallet.clone();
                let auth = self.current_auth();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let (_h, _p, _ran) = w.catch_up().await?;
                    if w.unlock_policy() != crate::native::UnlockPolicy::EachSpend {
                        if let Err(_e) = w.maintain(&auth).await {
                            warn!("auto-shield after sync did not finish");
                        }
                    }
                    snapshot_json(&dir)
                })
                .await
            }
            ("POST", "/scan/reset") => {
                let dir = self.wallet.clone();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    w.reset_scan().await?;
                    snapshot_json(&dir)
                })
                .await
            }
            ("GET", "/history") => {
                let dir = self.wallet.clone();
                let limit = parse_limit(&req.path);
                let status = query_param(&req.path, "status")
                    .as_deref()
                    .and_then(crate::HistoryStatusFilter::parse);
                let txid = query_param(&req.path, "txid");
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let rows: Vec<_> = w
                        .query_history(limit, status, txid.as_deref())?
                        .iter()
                        .map(|e| e.to_json())
                        .collect();
                    Ok(json!({ "transactions": rows }))
                })
                .await
            }
            ("GET", "/tx") => {
                let dir = self.wallet.clone();
                let txid = query_param(&req.path, "txid").unwrap_or_default();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    match w.transaction(&txid)? {
                        Some(row) => Ok(row.to_json()),
                        None => Err(anyhow::anyhow!("transaction not found")),
                    }
                })
                .await
            }
            ("GET", "/tip") => {
                let dir = self.wallet.clone();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let scanned = w.scanned_height()?;
                    let tip = NativeWallet::fetch_tip(&LightServer::Url(w.server_url())).await?;
                    Ok(json!({
                        "tip": tip,
                        "scanned": scanned,
                        "behind": tip.saturating_sub(scanned),
                    }))
                })
                .await
            }
            ("POST", "/address/next") => {
                let dir = self.wallet.clone();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let ua = w.next_unified_address()?;
                    let mut v = snapshot_json(&dir)?;
                    if let Value::Object(ref mut m) = v {
                        m.insert("unifiedAddress".into(), json!(ua));
                    }
                    Ok(v)
                })
                .await
            }
            ("POST", "/shield") => {
                let body: ShieldBody = serde_json::from_slice(if req.body.is_empty() {
                    b"{}"
                } else {
                    &req.body
                })
                .unwrap_or_default();
                let threshold = body.threshold.unwrap_or(100_000);
                let auth = self.current_auth();
                let dir = self.wallet.clone();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let txids = w.shield(&auth, threshold).await?;
                    let mut v = snapshot_json(&dir)?;
                    if let Value::Object(ref mut m) = v {
                        m.insert("txids".into(), json!(txids));
                    }
                    Ok(v)
                })
                .await
            }
            ("POST", "/send/estimate") => match parse_send(&req.body) {
                Err(e) => json_err(400, &e),
                Ok((to, zat, memo)) => {
                    let dir = self.wallet.clone();
                    self.locked(async move {
                        let w = NativeWallet::open(&dir)?;
                        let fee = w.estimate_fee(&to, zat, memo.as_deref())?;
                        Ok(crate::fee_estimate_json(fee))
                    })
                    .await
                }
            },
            ("POST", "/send/max") => {
                let body: MaxSendBody = serde_json::from_slice(if req.body.is_empty() {
                    b"{}"
                } else {
                    &req.body
                })
                .unwrap_or_default();
                let to = body
                    .to
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let dir = self.wallet.clone();
                self.locked(async move {
                    let w = NativeWallet::open(&dir)?;
                    let (max, fee) = w.max_send(to.as_deref())?;
                    Ok(crate::max_send_json(max, fee))
                })
                .await
            }
            ("GET", "/address/inspect") | ("POST", "/address/inspect") => {
                let from_q = query_param(&req.path, "address");
                let from_body: InspectBody = serde_json::from_slice(if req.body.is_empty() {
                    b"{}"
                } else {
                    &req.body
                })
                .unwrap_or_default();
                let addr = from_q
                    .or(from_body.address)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                match addr {
                    None => json_err(400, "missing address"),
                    Some(a) => match crate::keys::inspect_address(&a) {
                        Ok(v) => match serde_json::to_value(&v) {
                            Ok(j) => (200, "OK", j.to_string()),
                            Err(e) => json_err(500, &e.to_string()),
                        },
                        Err(e) => json_err(400, &e.to_string()),
                    },
                }
            }
            ("POST", "/send") => match parse_send(&req.body) {
                Err(e) => json_err(400, &e),
                Ok((to, zat, memo)) => {
                    let auth = self.current_auth();
                    let dir = self.wallet.clone();
                    self.locked(async move {
                        let w = NativeWallet::open(&dir)?;
                        let txids = w.send(&auth, &to, zat, memo.as_deref()).await?;
                        let mut v = snapshot_json(&dir)?;
                        if let Value::Object(ref mut m) = v {
                            m.insert("txids".into(), json!(txids));
                        }
                        Ok(v)
                    })
                    .await
                }
            },
            ("POST", "/create") => match parse_create(&req.body) {
                Err(e) => json_err(400, &e),
                Ok((network, birthday, server, rpc, pass)) => {
                    let auth = auth_with_pass(self.current_auth(), pass.clone());
                    let dir = self.wallet.clone();
                    let remembered = Arc::clone(&self.auth);
                    self.locked(async move {
                        let birthday = match birthday {
                            Some(input) => Some(
                                resolve_birthday(input, network, NativeWallet::fetch_tip(&server))
                                    .await?,
                            ),
                            None => None,
                        };
                        let (mut w, created) =
                            NativeWallet::create(&dir, network, Some(server), birthday, auth, 0)
                                .await?;
                        w.set_validator_rpc(rpc)?;
                        remember_passphrase(&remembered, pass);
                        let mut v = snapshot_json(&dir)?;
                        if let Value::Object(ref mut m) = v {
                            m.insert("mnemonic".into(), json!(created.mnemonic));
                        }
                        Ok(v)
                    })
                    .await
                }
            },
            ("POST", "/restore") => match parse_restore(&req.body) {
                Err(e) => json_err(400, &e),
                Ok((mnemonic, ufvk, birthday, network, server, rpc, pass, policy)) => {
                    let mut auth = auth_with_pass(self.current_auth(), pass.clone());
                    if let Some(policy) = policy {
                        auth.unlock_policy = policy;
                    }
                    let dir = self.wallet.clone();
                    let remembered = Arc::clone(&self.auth);
                    self.locked(async move {
                        let birthday =
                            resolve_birthday(birthday, network, NativeWallet::fetch_tip(&server))
                                .await?;
                        if let Some(ufvk) = ufvk {
                            let (mut w, _) = NativeWallet::restore_ufvk(
                                &dir,
                                &ufvk,
                                network,
                                Some(server),
                                birthday,
                                0,
                            )
                            .await?;
                            if let Some(policy) = policy {
                                w.set_unlock_policy(policy)?;
                            }
                            w.set_validator_rpc(rpc)?;
                        } else {
                            let (mut w, _) = NativeWallet::restore(
                                &dir,
                                &mnemonic,
                                network,
                                Some(server),
                                birthday,
                                auth,
                                0,
                            )
                            .await?;
                            w.set_validator_rpc(rpc)?;
                        }
                        remember_passphrase(&remembered, pass);
                        snapshot_json(&dir)
                    })
                    .await
                }
            },
            ("POST", "/attach-seed") => match parse_attach(&req.body) {
                Err(e) => json_err(400, &e),
                Ok(mnemonic) => {
                    let auth = self.current_auth();
                    let dir = self.wallet.clone();
                    self.locked(async move {
                        let mut w = NativeWallet::open(&dir)?;
                        w.attach_seed(&mnemonic, &auth)?;
                        snapshot_json(&dir)
                    })
                    .await
                }
            },
            ("POST", "/unlock-policy") => match parse_unlock_policy(&req.body) {
                Err(e) => json_err(400, &e),
                Ok(policy) => {
                    let dir = self.wallet.clone();
                    self.locked(async move {
                        let mut w = NativeWallet::open(&dir)?;
                        w.set_unlock_policy(policy)?;
                        snapshot_json(&dir)
                    })
                    .await
                }
            },
            _ => json_err(404, "not found"),
        }
    }

    fn wallet_exists(&self) -> bool {
        self.wallet.join("data.sqlite").exists() && self.wallet.join("wallet.json").exists()
    }

    async fn locked<F>(&self, work: F) -> (u16, &'static str, String)
    where
        F: std::future::Future<Output = anyhow::Result<Value>>,
    {
        let _g = self.lock.lock().await;
        match work.await {
            Ok(v) => (200, "OK", v.to_string()),
            Err(e) => map_err(e),
        }
    }
}

fn parse_network_opt(s: Option<&str>) -> std::result::Result<Network, String> {
    match s.map(str::trim).filter(|x| !x.is_empty()) {
        None => Ok(Network::Regtest),
        Some(n) => Network::parse(n).ok_or_else(|| format!("unknown network: {n}")),
    }
}

fn parse_light(server: Option<&str>, network: Network, default_local: bool) -> LightServer {
    match server.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => LightServer::parse(s, network),
        None if default_local => LightServer::local_for_network(network),
        None => LightServer::for_network(network),
    }
}

fn optional_pass(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn remember_passphrase(auth: &StdMutex<crate::native::SeedAuth>, pass: Option<String>) {
    let Some(p) = pass.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let mut g = auth.lock().unwrap_or_else(|e| e.into_inner());
    g.passphrase = Some(p);
}

fn auth_with_pass(
    mut auth: crate::native::SeedAuth,
    pass: Option<String>,
) -> crate::native::SeedAuth {
    if let Some(p) = pass {
        auth.passphrase = Some(p);
    }
    auth
}

fn parse_create(
    body: &[u8],
) -> std::result::Result<
    (
        Network,
        Option<BridgeBirthday>,
        LightServer,
        Option<String>,
        Option<String>,
    ),
    String,
> {
    let b: CreateBody = if body.is_empty() {
        CreateBody::default()
    } else {
        serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?
    };
    let network = parse_network_opt(b.network.as_deref())?;
    let server = parse_light(b.server.as_deref(), network, true);
    Ok((
        network,
        b.birthday
            .as_ref()
            .map(|raw| parse_birthday_value(Some(raw), network))
            .transpose()?,
        server,
        b.validator_rpc,
        optional_pass(b.passphrase),
    ))
}

#[derive(Debug, PartialEq)]
enum BridgeBirthday {
    Height(u32),
    Estimate(String),
}

fn parse_birthday_value(
    raw: Option<&serde_json::Value>,
    network: Network,
) -> std::result::Result<BridgeBirthday, String> {
    let invalid = || "birthday must be a block height or YYYY-MM-DD".to_string();
    match raw {
        None if network == Network::Regtest => Ok(BridgeBirthday::Height(1)),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|h| u32::try_from(h).ok())
            .filter(|h| *h > 0)
            .map(BridgeBirthday::Height)
            .ok_or_else(invalid),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                return s
                    .parse::<u32>()
                    .ok()
                    .filter(|h| *h > 0)
                    .map(BridgeBirthday::Height)
                    .ok_or_else(invalid);
            }
            // Validate syntax only. Resolve the estimate later against the live
            // server tip, before creating files or saving a seed.
            crate::parse_birthday_input_for_network(s, 1, network).map_err(|e| e.to_string())?;
            Ok(BridgeBirthday::Estimate(s.to_string()))
        }
        _ => Err(invalid()),
    }
}

async fn resolve_birthday(
    input: BridgeBirthday,
    network: Network,
    live_tip: impl std::future::Future<Output = Result<u32>>,
) -> Result<u32> {
    match input {
        BridgeBirthday::Height(height) => Ok(height),
        BridgeBirthday::Estimate(raw) => {
            crate::parse_birthday_input_for_network(&raw, live_tip.await?, network)
        }
    }
}

fn parse_restore(
    body: &[u8],
) -> std::result::Result<
    (
        String,
        Option<String>,
        BridgeBirthday,
        Network,
        LightServer,
        Option<String>,
        Option<String>,
        Option<crate::native::UnlockPolicy>,
    ),
    String,
> {
    let b: RestoreBody = if body.is_empty() {
        RestoreBody::default()
    } else {
        serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?
    };
    let network = parse_network_opt(b.network.as_deref())?;
    let birthday = parse_birthday_value(b.birthday.as_ref(), network)?;
    let server = parse_light(b.server.as_deref(), network, true);
    let ufvk = b
        .ufvk
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    if let Some(u) = ufvk {
        if !u.to_ascii_lowercase().starts_with("uview") {
            return Err("restore ufvk must be a unified full viewing key".into());
        }
        return Ok((
            String::new(),
            Some(u),
            birthday,
            network,
            server,
            b.validator_rpc,
            optional_pass(b.passphrase),
            b.unlock_policy,
        ));
    }
    let mnemonic = b
        .mnemonic
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let n = mnemonic.split_whitespace().count();
    if n != 12 && n != 24 {
        return Err("restore needs a 12 or 24 word mnemonic, or a UFVK".into());
    }
    Ok((
        mnemonic,
        None,
        birthday,
        network,
        server,
        b.validator_rpc,
        optional_pass(b.passphrase),
        b.unlock_policy,
    ))
}

fn parse_attach(body: &[u8]) -> std::result::Result<String, String> {
    let b: AttachBody = serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?;
    let mnemonic = b
        .mnemonic
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let n = mnemonic.split_whitespace().count();
    if n != 12 && n != 24 {
        return Err("attach-seed needs a 12 or 24 word mnemonic".into());
    }
    Ok(mnemonic)
}

fn parse_unlock_policy(body: &[u8]) -> std::result::Result<crate::native::UnlockPolicy, String> {
    let b: UnlockPolicyBody = serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?;
    crate::native::UnlockPolicy::parse(b.policy.as_deref().unwrap_or(""))
        .ok_or_else(|| "unlock policy must be session, each-spend, or always".into())
}

fn parse_setup(body: &[u8]) -> std::result::Result<(Network, LightServer, Option<String>), String> {
    let b: SetupBody = if body.is_empty() {
        SetupBody::default()
    } else {
        serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?
    };
    let network = parse_network_opt(b.network.as_deref())?;
    let server = parse_light(b.server.as_deref(), network, true);
    Ok((network, server, b.validator_rpc))
}

fn lwd_validator_rpc(req: &HttpReq, server: &LightServer, wallet: &Path) -> Option<String> {
    if let Some(u) = query_param(&req.path, "rpc") {
        let u = u.trim().to_string();
        if !u.is_empty() {
            return Some(u);
        }
    }
    NativeWallet::open(wallet)
        .ok()
        .and_then(|w| w.validator_rpc_url())
        .or_else(|| server.zebra_rpc_url())
}

fn parse_limit(path: &str) -> usize {
    path.split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("limit=")?.parse::<usize>().ok())
        })
        .unwrap_or(50)
        .clamp(1, 500)
}

fn parse_send(body: &[u8]) -> std::result::Result<(String, u64, Option<String>), String> {
    let b: SendBody = if body.is_empty() {
        SendBody::default()
    } else {
        serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?
    };
    let to = b.to.unwrap_or_default().trim().to_string();
    if to.is_empty() {
        return Err("missing to".into());
    }
    let zat = if let Some(zec) = b
        .amount_zec
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parse_zec_to_zatoshis(zec)?
    } else {
        b.amount.unwrap_or(0)
    };
    let zip = to.to_ascii_lowercase().starts_with("zcash:");
    if !zip && zat == 0 {
        return Err("amount must be greater than 0".into());
    }
    let memo = b
        .memo
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok((to, zat, memo))
}

fn snapshot_json(root: &Path) -> anyhow::Result<Value> {
    let w = NativeWallet::open(root)?;
    let addr = w.unified_address()?;
    let t = w.transparent_address()?.unwrap_or_default();
    let b = w.balance().unwrap_or_else(|_| Balance::default());
    let zip = zip321_uri(&addr, None).unwrap_or_default();
    let rows: Vec<_> = w
        .history(40)
        .unwrap_or_default()
        .iter()
        .map(|e| e.to_json())
        .collect();
    Ok(json!({
        "network": w.network().as_str(),
        "server": w.server_url(),
        "validatorRpc": w.validator_rpc_url(),
        "birthdayHeight": w.birthday_height(),
        "unifiedAddress": addr,
        "transparentAddress": if t.is_empty() { Value::Null } else { json!(t) },
        "zip321": zip,
        "scannedHeight": w.scanned_height().unwrap_or(0),
        "viewOnly": w.is_view_only(),
        "ufvk": w.viewing_key().ok(),
        "unlockPolicy": w.unlock_policy().as_str(),
        "transactions": rows,
        "balance": {
            "saplingAvailable": b.sapling_available,
            "orchardAvailable": b.orchard_available,
            "ironwoodAvailable": b.ironwood_available,
            "transparentAvailable": b.transparent_available,
            "totalAvailable": b.total_available,
            "saplingPending": b.sapling_pending,
            "orchardPending": b.orchard_pending,
            "ironwoodPending": b.ironwood_pending,
            "transparentPending": b.transparent_pending,
            "totalPending": b.total_pending,
            "saplingZec": format_zatoshis(b.sapling_available),
            "orchardZec": format_zatoshis(b.orchard_available),
            "ironwoodZec": format_zatoshis(b.ironwood_available),
            "transparentZec": format_zatoshis(b.transparent_available),
            "totalZec": format_zatoshis(b.total_available),
            "pendingZec": format_zatoshis(b.total_pending),
            "orchardPendingZec": format_zatoshis(b.orchard_pending),
            "transparentPendingZec": format_zatoshis(b.transparent_pending),
        }
    }))
}

fn map_err(e: anyhow::Error) -> (u16, &'static str, String) {
    let msg = format!("{e:#}");
    let lower = msg.to_lowercase();
    let code = if lower.contains("not found") {
        404
    } else if lower.contains("already exists") {
        409
    } else if lower.contains("insufficient") || lower.contains("origin") {
        400
    } else {
        500
    };
    json_err(code, &msg)
}

fn json_err(code: u16, msg: &str) -> (u16, &'static str, String) {
    let reason = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    (code, reason, json!({ "error": msg }).to_string())
}

pub fn parse_loopback_bind(bind: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| EngineError::Transport(format!("bind address {bind}: {e}")))?;
    if !addr.ip().is_loopback() {
        return Err(EngineError::Message(
            "bridge must bind a loopback address (127.0.0.1 / ::1)".into(),
        ));
    }
    Ok(addr)
}

pub fn origin_allowed(origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => {
            if DEFAULT_ORIGINS.contains(&o) {
                return true;
            }
            std::env::var("Z_STACK_BRIDGE_ORIGIN")
                .ok()
                .map(|s| s.split(',').any(|x| x.trim() == o))
                .unwrap_or(false)
        }
    }
}

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
        if let Some(i) = find_double_crlf(&buf) {
            break i;
        }
    };
    let header = std::str::from_utf8(&buf[..header_end])?;
    let mut lines = header.split("\r\n");
    let req = lines.next().unwrap_or("");
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut origin = None;
    let mut token = None;
    let mut content_len = 0usize;
    for line in lines {
        let (k, v) = match line.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let k = k.trim();
        let v = v.trim();
        if k.eq_ignore_ascii_case("Origin") {
            origin = Some(v.to_string());
        } else if k.eq_ignore_ascii_case("Content-Length") {
            content_len = v.parse().unwrap_or(0);
        } else if k.eq_ignore_ascii_case("Authorization") {
            if let Some(rest) = v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
            {
                token = Some(rest.trim().to_string());
            }
        } else if k.eq_ignore_ascii_case("X-Z-Stack-Token") {
            token = Some(v.to_string());
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > 64 * 1024 {
            anyhow::bail!("body too large");
        }
    }
    body.truncate(content_len);
    Ok(HttpReq {
        method,
        path,
        origin,
        token,
        body,
    })
}

fn random_bridge_token() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    for part in q.split('&') {
        let (k, v) = part.split_once('=')?;
        if k == key {
            return Some(urlencoding_decode(v));
        }
    }
    None
}

fn parse_mine_n(path: &str, body: &[u8]) -> u32 {
    let from_q = query_param(path, "n")
        .or_else(|| query_param(path, "blocks"))
        .and_then(|s| s.parse().ok());
    let from_body = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        v.get("blocks")
            .or_else(|| v.get("n"))
            .and_then(|x| x.as_u64())
    });
    from_q.or(from_body).unwrap_or(1).clamp(1, 200) as u32
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

fn urlencoding_decode(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { ' ' } else { b[i] as char });
        i += 1;
    }
    out
}

fn json_ok_bytes(v: Value) -> (u16, &'static str, &'static str, Vec<u8>) {
    (
        200,
        "OK",
        "application/json; charset=utf-8",
        v.to_string().into_bytes(),
    )
}

fn lwd_err(e: EngineError) -> (u16, &'static str, &'static str, Vec<u8>) {
    let (code, reason, s) = map_err(anyhow::Error::msg(e.to_string()));
    (
        code,
        reason,
        "application/json; charset=utf-8",
        s.into_bytes(),
    )
}

/// A `/lwd/sendraw` failure. The node's explicit refusal carries `rejected`,
/// so a client can tell it from an unknown outcome (which it must not undo).
fn lwd_send_err(e: EngineError) -> (u16, &'static str, &'static str, Vec<u8>) {
    let (code, reason, body) = map_err(anyhow::Error::msg(e.to_string()));
    let body = match &e {
        EngineError::BroadcastRejected { code, message } => json!({
            "error": e.to_string(),
            "rejected": { "code": code, "message": message },
        })
        .to_string(),
        _ => body,
    };
    (
        code,
        reason,
        "application/json; charset=utf-8",
        body.into_bytes(),
    )
}

fn lwd_msg(code: u16, msg: &str) -> (u16, &'static str, &'static str, Vec<u8>) {
    let (code, reason, s) = json_err(code, msg);
    (
        code,
        reason,
        "application/json; charset=utf-8",
        s.into_bytes(),
    )
}

async fn write_http(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    origin: Option<&str>,
    allow: bool,
    body: &[u8],
) -> anyhow::Result<()> {
    write_http_typed(
        stream,
        code,
        reason,
        origin,
        allow,
        "application/json; charset=utf-8",
        body,
    )
    .await
}

async fn write_http_typed(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    origin: Option<&str>,
    allow: bool,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type, authorization, x-z-stack-token\r\n",
        body.len()
    );
    if allow {
        if let Some(o) = origin {
            headers.push_str(&format!(
                "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"
            ));
        } else {
            headers.push_str("Access-Control-Allow-Origin: *\r\n");
        }
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_only() {
        assert!(parse_loopback_bind("127.0.0.1:8787").is_ok());
        assert!(parse_loopback_bind("[::1]:8787").is_ok());
        assert!(parse_loopback_bind("0.0.0.0:8787").is_err());
        assert!(parse_loopback_bind("192.168.1.4:8787").is_err());
    }

    #[tokio::test]
    async fn date_bodies_resolve_against_live_tip_for_create_and_both_restore_keys() {
        // A future date clips to the live tip, avoiding wall-clock rounding races.
        let date = "9999-12-31".to_string();
        let create = json!({"network":"testnet", "birthday": date});
        let (_, input, _, _, _) = parse_create(create.to_string().as_bytes()).unwrap();
        let input = input.unwrap();
        assert_eq!(input, BridgeBirthday::Estimate(date.clone()));
        let first = resolve_birthday(input, Network::Testnet, async { Ok(4_500_000) })
            .await
            .unwrap();
        let expected =
            crate::parse_birthday_input_for_network(&date, 4_500_000, Network::Testnet).unwrap();
        assert_eq!(first, expected);
        for key in [
            json!({"mnemonic":crate::keys::REGTEST_FAUCET_MNEMONIC}),
            json!({"ufvk":"uview1fixture"}),
        ] {
            let mut body = key;
            body["network"] = json!("testnet");
            body["birthday"] = json!(date);
            let (_, _, input, _, _, _, _, _) = parse_restore(body.to_string().as_bytes()).unwrap();
            let second = resolve_birthday(input, Network::Testnet, async { Ok(4_501_000) })
                .await
                .unwrap();
            assert_eq!(
                second - first,
                1_000,
                "live tips must move the estimated birthday"
            );
        }
        let exact = parse_birthday_value(Some(&json!(12345)), Network::Testnet).unwrap();
        assert_eq!(
            resolve_birthday(exact, Network::Testnet, async {
                panic!("exact heights must not fetch a tip")
            })
            .await
            .unwrap(),
            12345
        );
        assert!(parse_create(br#"{"birthday":4294967297}"#).is_err());
        assert!(parse_create(br#"{"birthday":0}"#).is_err());
        assert!(parse_restore(br#"{"ufvk":"uview1fixture","birthday":"2026-13-40"}"#).is_err());
        assert!(
            resolve_birthday(BridgeBirthday::Estimate(date), Network::Testnet, async {
                Err(EngineError::Transport("unavailable".into()))
            })
            .await
            .is_err()
        );
    }

    #[test]
    fn create_restore_remember_passphrase() {
        let auth = StdMutex::new(SeedAuth::windows_credential());
        remember_passphrase(&auth, Some("secret".into()));
        assert_eq!(auth.lock().unwrap().passphrase.as_deref(), Some("secret"));
        remember_passphrase(&auth, None);
        assert_eq!(auth.lock().unwrap().passphrase.as_deref(), Some("secret"));
    }

    #[test]
    fn restore_unlock_policy_is_optional_and_validated_for_both_key_types() {
        use crate::native::UnlockPolicy;

        for key in [
            json!({ "mnemonic": crate::keys::REGTEST_FAUCET_MNEMONIC }),
            json!({ "ufvk": "uview1fixture" }),
        ] {
            for (name, expected) in [
                ("session", UnlockPolicy::Session),
                ("each-spend", UnlockPolicy::EachSpend),
                ("always", UnlockPolicy::Always),
            ] {
                let mut body = key.clone();
                body["network"] = json!("regtest");
                body["unlockPolicy"] = json!(name);
                let (_, _, _, _, _, _, _, policy) =
                    parse_restore(body.to_string().as_bytes()).unwrap();
                assert_eq!(policy, Some(expected));
                body["unlockPolicy"] = json!("typo");
                assert!(parse_restore(body.to_string().as_bytes()).is_err());
                body.as_object_mut().unwrap().remove("unlockPolicy");
                let (_, _, _, _, _, _, _, policy) =
                    parse_restore(body.to_string().as_bytes()).unwrap();
                assert_eq!(policy, None);
            }
        }
    }

    #[test]
    fn cors_defaults() {
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("http://127.0.0.1:5173")));
        assert!(origin_allowed(Some("http://localhost:5173")));
        assert!(!origin_allowed(Some("https://evil.example")));
    }

    #[test]
    fn sendraw_rejection_is_structured() {
        let (_, _, _, body) = lwd_send_err(EngineError::BroadcastRejected {
            code: -26,
            message: "bad-txns-nullifier-conflict".into(),
        });
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["rejected"]["code"], -26);
        assert_eq!(v["rejected"]["message"], "bad-txns-nullifier-conflict");
        // An unknown outcome carries no `rejected`: the client keeps the send.
        let (_, _, _, body) = lwd_send_err(EngineError::Transport("zebra rpc read: reset".into()));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("rejected").is_none());
    }

    #[test]
    fn sendraw_hex() {
        assert_eq!(
            parse_sendraw_hex(br#"{"hex":"deadbeef"}"#).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(parse_sendraw_hex(b"0xab").as_deref(), Some("ab"));
        assert!(parse_sendraw_hex(b"nope").is_none());
    }

    #[test]
    fn send_body() {
        let (to, zat, memo) = parse_send(br#"{"to":"uregtest1abc","amountZec":"0.0005"}"#).unwrap();
        assert_eq!(to, "uregtest1abc");
        assert_eq!(zat, 50_000);
        assert!(memo.is_none());
        assert_eq!(parse_limit("/history?limit=12"), 12);
        assert_eq!(
            query_param("/lwd/blocks?start=1&end=9", "end").as_deref(),
            Some("9")
        );
        assert_eq!(parse_mine_n("/lwd/mine?n=12", b""), 12);
        assert_eq!(parse_mine_n("/lwd/mine", br#"{"blocks":3}"#), 3);
        assert_eq!(parse_mine_n("/lwd/mine", b""), 1);
        assert!(parse_send(br#"{"to":"u","amountZec":"0"}"#).is_err());
        let (uri, zat0, _) = parse_send(
            br#"{"to":"zcash:uregtest1abc?amount=0.0005&address.1=uregtest1def&amount.1=0.1"}"#,
        )
        .unwrap();
        assert!(uri.starts_with("zcash:"));
        assert_eq!(zat0, 0);
        let (net, bday, srv, rpc, pass) = parse_create(br#"{"network":"regtest"}"#).unwrap();
        assert!(pass.is_none());
        assert_eq!(net, Network::Regtest);
        assert_eq!(bday, None);
        assert_eq!(srv.as_url(), LightServer::LOCAL_REGTEST_GRPC);
        assert!(rpc.is_none());
        let (net2, _, srv2, _, _) =
            parse_create(br#"{"network":"testnet","server":"local"}"#).unwrap();
        let (_, _, _, _, p2) =
            parse_create(br#"{"network":"regtest","passphrase":"secret"}"#).unwrap();
        assert_eq!(p2.as_deref(), Some("secret"));
        assert_eq!(net2, Network::Testnet);
        assert_eq!(srv2.as_url(), LightServer::LOCAL_ZAINO_GRPC);
        let words = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let body = format!(r#"{{"mnemonic":"{words}","network":"regtest"}}"#);
        let (m, ufvk, h, n, _, _, _, policy) = parse_restore(body.as_bytes()).unwrap();
        assert!(policy.is_none());
        assert_eq!(m, words);
        assert!(ufvk.is_none());
        assert_eq!(h, BridgeBirthday::Height(1));
        assert_eq!(n, Network::Regtest);
        assert!(parse_restore(br#"{"mnemonic":"too short"}"#).is_err());
        let (_, u, _, _, _, _, _, _) =
            parse_restore(br#"{"ufvk":"uview1abc","network":"regtest","birthday":"1"}"#).unwrap();
        assert_eq!(u.as_deref(), Some("uview1abc"));
        let (_, _, date_h, _, _, _, _, _) =
            parse_restore(br#"{"ufvk":"uview1xyz","network":"testnet","birthday":"2022-05-31"}"#)
                .unwrap();
        assert_eq!(date_h, BridgeBirthday::Estimate("2022-05-31".into()));
    }
}
