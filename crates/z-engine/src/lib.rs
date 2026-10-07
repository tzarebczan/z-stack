//! Shared wallet engine for z-stack.
//!
//! Built on Zakura Common + wallet-libraries (`zakura-*` packages).
//! Web (`z-wasm`) and desktop (`z-desktop`) both call into this crate.

#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;
use zcash_protocol::consensus::{
    BlockHeight, Network as ZcashNetwork, NetworkType, NetworkUpgrade, Parameters,
};

pub mod birthday;
pub mod error;
pub use error::{display_anyhow, EngineError};
pub mod keys;
#[cfg(feature = "hardware")]
pub mod ledger;
pub(crate) mod offload;
pub mod scan;
pub mod web;
mod zip321;

pub use birthday::{
    catch_up_percent, catch_up_percent_from, date_from_height, display_catch_up_percent, fmt_secs,
    height_from_date, historic_overlay_checks, historic_overlay_stage, historic_overlay_visible,
    light_stall_warning, live_scan_eta_secs, parse_birthday_input, scan_rate_range, typical_tip,
    ymd_days_ago, OverlayCheck, SyncEta, BLOCK_SECONDS, NEAR_TIP_BLOCKS, QUIET_BEHIND_BLOCKS,
    STALL_QUIET_REMAINING, SYNC_STALL_SECS,
};
pub use scan::{compact_has_shielded, sync_tuning, SyncTuning, BATCH_LOCAL, MAX_SYNC_BLOCKS};
pub use zip321::{
    parse_zip321, zip321_uri, zip321_uri_full, zip321_uri_many, Zip321Payment, Zip321Request,
};

#[cfg(feature = "native")]
pub mod native;

#[cfg(feature = "native")]
pub mod params;

/// Network the engine is configured for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    /// Local Zebra/Zaino compose. NU6.2 at height 2; NU6.3 at [`regtest_nu6_3_height`].
    Regtest,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Regtest => "regtest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mainnet" => Some(Self::Mainnet),
            "testnet" => Some(Self::Testnet),
            "regtest" => Some(Self::Regtest),
            _ => None,
        }
    }
}

/// Spend / shield confirmation policy.
///
/// Regtest matches the old WebZjs lab: 1 confirmation, 0-conf shielding.
/// Mainnet/testnet use **3 confirmations** for both trusted and untrusted
/// notes (ZIP-315's trusted rule, not the 10-block untrusted wait).
/// Zero-conf shielding of transparent UTXOs stays on.
///
/// Consensus coinbase maturity is still 100 blocks (Zebra has no config to
/// lower it). Immature faucet coinbase is rejected by the node, not this policy.
pub fn confirmations_policy(network: Network) -> ConfirmationsPolicy {
    match network {
        Network::Regtest => ConfirmationsPolicy::MIN,
        Network::Mainnet | Network::Testnet => {
            // 3 == NonZeroU32::MIN + 2
            let three = NonZeroU32::MIN.saturating_add(2);
            #[cfg(feature = "transparent-inputs")]
            {
                ConfirmationsPolicy::new_symmetrical(three, true)
            }
            #[cfg(not(feature = "transparent-inputs"))]
            {
                ConfirmationsPolicy::new_symmetrical(three)
            }
        }
    }
}

impl Parameters for Network {
    fn network_type(&self) -> NetworkType {
        match self {
            Self::Mainnet => NetworkType::Main,
            Self::Testnet => NetworkType::Test,
            Self::Regtest => NetworkType::Regtest,
        }
    }

    fn activation_height(&self, nu: NetworkUpgrade) -> Option<BlockHeight> {
        match self {
            Self::Mainnet => ZcashNetwork::MainNetwork.activation_height(nu),
            Self::Testnet => ZcashNetwork::TestNetwork.activation_height(nu),
            // Must match `infra/compose/regtest/zebra.toml`.
            Self::Regtest => match nu {
                NetworkUpgrade::Overwinter
                | NetworkUpgrade::Sapling
                | NetworkUpgrade::Blossom
                | NetworkUpgrade::Heartwood
                | NetworkUpgrade::Canopy => Some(BlockHeight::from_u32(1)),
                NetworkUpgrade::Nu5
                | NetworkUpgrade::Nu6
                | NetworkUpgrade::Nu6_1
                | NetworkUpgrade::Nu6_2 => Some(BlockHeight::from_u32(2)),
                NetworkUpgrade::Nu6_3 => Some(BlockHeight::from_u32(regtest_nu6_3_height())),
            },
        }
    }
}

/// Default regtest NU6.3 (Ironwood) height: held off on the compose Zebra chain.
pub const REGTEST_NU6_3_DEFAULT: u32 = 1_000_000;

/// 0 until first read or set.
static REGTEST_NU6_3: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Regtest NU6.3 (Ironwood) activation height. [`REGTEST_NU6_3_DEFAULT`] unless
/// `Z_STACK_REGTEST_NU6_3` (native) or [`set_regtest_nu6_3_height`] (WASM) says
/// otherwise; a regtest validator that activates Ironwood early (`"NU6.3" = 150`
/// in its config) needs the same height here.
pub fn regtest_nu6_3_height() -> u32 {
    use std::sync::atomic::Ordering::Relaxed;
    match REGTEST_NU6_3.load(Relaxed) {
        0 => {
            let height = std::env::var("Z_STACK_REGTEST_NU6_3")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|h| *h >= 2)
                .unwrap_or(REGTEST_NU6_3_DEFAULT);
            REGTEST_NU6_3.store(height, Relaxed);
            height
        }
        height => height,
    }
}

/// Set the regtest NU6.3 height before opening a regtest wallet (NU6.2 is at 2).
pub fn set_regtest_nu6_3_height(height: u32) {
    REGTEST_NU6_3.store(height.max(2), std::sync::atomic::Ordering::Relaxed);
}

/// Where compact blocks / tips come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LightServer {
    /// Public or custom lightwalletd / Zaino endpoint.
    ///
    /// Native clients need **native gRPC** (not browser gRPC-Web).
    Url(String),
    /// Local Zaino native gRPC (compose without Traefik).
    LocalZaino,
    /// Local regtest Zaino gRPC (`infra/compose/docker-compose.regtest.yml`).
    LocalRegtest,
}

impl LightServer {
    /// Native gRPC endpoint used by `z-engine` / desktop.
    /// Default testnet endpoint. Mainnet loopback is [`Self::LOCAL_ZAINO_GRPC_MAINNET`].
    pub const LOCAL_ZAINO_GRPC: &'static str = "http://127.0.0.1:8137";
    /// Default mainnet Zaino endpoint. Supply an explicit URL for other node layouts.
    pub const LOCAL_ZAINO_GRPC_MAINNET: &'static str = "http://127.0.0.1:8138";
    /// Browser gRPC-Web endpoint (Traefik); used by `z-wasm`, not tonic.
    pub const LOCAL_ZAINO_GRPC_WEB: &'static str = "http://127.0.0.1:1234/zaino";
    /// Native gRPC for the local regtest indexer (plaintext h2c).
    pub const LOCAL_REGTEST_GRPC: &'static str = "http://127.0.0.1:28137";
    /// Zebra JSON-RPC on the local regtest compose (no cookie auth).
    pub const LOCAL_REGTEST_ZEBRA_RPC: &'static str = "http://127.0.0.1:29232";
    /// Conventional Zakura/zcashd mainnet JSON-RPC.
    pub const LOCAL_ZAKURA_RPC_MAINNET: &'static str = "http://127.0.0.1:8232";
    /// Local testnet Zakura JSON-RPC. zcashd-style `:18232` is also probed.
    pub const LOCAL_ZAKURA_RPC_TESTNET: &'static str = "http://127.0.0.1:28232";
    /// Public testnet lightwalletd (compact blocks only — no t-scan).
    pub const TESTNET_PUBLIC_LWD: &'static str = "https://testnet.zec.rocks:443";

    /// Validator JSON-RPC for `sendrawtransaction` when Zaino submit is unusable.
    /// Independent of the light server: Zaino and Zakura are configured separately.
    pub fn zebra_rpc_url(&self) -> Option<String> {
        self.implied_validator_rpc().or_else(|| {
            std::env::var("Z_STACK_VALIDATOR_RPC")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    std::env::var("Z_STACK_ZEBRA_RPC")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
        })
    }

    fn implied_validator_rpc(&self) -> Option<String> {
        match self {
            Self::LocalRegtest => Some(Self::LOCAL_REGTEST_ZEBRA_RPC.into()),
            Self::Url(u) if u.contains(":28137") => Some(Self::LOCAL_REGTEST_ZEBRA_RPC.into()),
            _ => None,
        }
    }

    /// Loopback light server for this network (Zaino). Public LWD is not this.
    pub fn local_for_network(network: Network) -> Self {
        match network {
            Network::Regtest => Self::LocalRegtest,
            Network::Testnet => Self::LocalZaino,
            Network::Mainnet => Self::Url(Self::LOCAL_ZAINO_GRPC_MAINNET.into()),
        }
    }

    /// Default JSON-RPC URL to try first for a local validator.
    pub fn local_validator_rpc(network: Network) -> &'static str {
        match network {
            Network::Regtest => Self::LOCAL_REGTEST_ZEBRA_RPC,
            Network::Testnet => Self::LOCAL_ZAKURA_RPC_TESTNET,
            Network::Mainnet => Self::LOCAL_ZAKURA_RPC_MAINNET,
        }
    }

    /// Loopback RPC ports to probe. Testnet tries Zakura `:28232` then zcashd-style `:18232`.
    pub fn validator_rpc_candidates(network: Network) -> &'static [&'static str] {
        match network {
            Network::Regtest => &[Self::LOCAL_REGTEST_ZEBRA_RPC],
            Network::Testnet => &[Self::LOCAL_ZAKURA_RPC_TESTNET, "http://127.0.0.1:18232"],
            Network::Mainnet => &[Self::LOCAL_ZAKURA_RPC_MAINNET],
        }
    }

    pub fn mainnet_default() -> Self {
        // Native gRPC (zec.rocks). ChainSafe URLs are typically gRPC-Web for browsers.
        Self::Url("https://zec.rocks:443".into())
    }

    pub fn testnet_default() -> Self {
        // TLS on 443 (9067 is historically plaintext gRPC).
        Self::Url("https://testnet.zec.rocks:443".into())
    }

    pub fn for_network(network: Network) -> Self {
        match network {
            Network::Mainnet => Self::mainnet_default(),
            Network::Testnet => Self::testnet_default(),
            Network::Regtest => Self::LocalRegtest,
        }
    }

    pub fn as_url(&self) -> String {
        match self {
            Self::Url(u) => u.trim_end_matches('/').to_string(),
            Self::LocalZaino => Self::LOCAL_ZAINO_GRPC.to_string(),
            Self::LocalRegtest => Self::LOCAL_REGTEST_GRPC.to_string(),
        }
    }

    /// `local` / `local-zaino` / a URL. Empty string uses [`Self::for_network`].
    pub fn parse(s: &str, network: Network) -> Self {
        match s.trim() {
            "" => Self::for_network(network),
            "local" | "local-zaino" => Self::local_for_network(network),
            "local-regtest" => Self::LocalRegtest,
            u => Self::Url(normalize_grpc_url(u)),
        }
    }

    /// `GetAddressUtxos` / `GetTaddressTxids` leak t-addrs to the light server.
    /// Only a loopback indexer (your Zaino) is allowed.
    pub fn allows_transparent_query(&self) -> bool {
        match self {
            Self::LocalZaino | Self::LocalRegtest => true,
            Self::Url(u) => is_loopback_light_url(u),
        }
    }
}

fn light_url_host(url: &str) -> String {
    light_url_host_port(url)
        .map(|(host, _)| host)
        .unwrap_or_default()
}

/// Host + port from a light URL (`http://127.0.0.1:8138`). Used for TCP probes.
pub fn light_url_host_port(url: &str) -> Option<(String, u16)> {
    let s = url.trim();
    let (rest, default_port) = if let Some(r) = s.strip_prefix("https://") {
        (r, 443u16)
    } else if let Some(r) = s.strip_prefix("http://") {
        (r, 80u16)
    } else {
        (s, 80u16)
    };
    let hostport = rest.split(['/', '?']).next().unwrap_or(rest);
    if hostport.is_empty() {
        return None;
    }
    if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_ascii_lowercase(), port));
    }
    if let Some((host, port)) = hostport.rsplit_once(':') {
        if let Ok(p) = port.parse::<u16>() {
            if !host.is_empty() {
                return Some((host.to_ascii_lowercase(), p));
            }
        }
    }
    Some((hostport.to_ascii_lowercase(), default_port))
}

/// Host is loopback (127.0.0.1 / localhost / ::1). Public LWD hosts are false.
pub fn is_loopback_light_url(url: &str) -> bool {
    matches!(
        light_url_host(url).as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Compact-block URL plus a short where-is-this label for overlays.
pub fn describe_light_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return "(no light server)".into();
    }
    if is_public_lwd_url(url) {
        return format!("{url}  ·  public LWD");
    }
    if url.contains(":8138") {
        return format!("{url}  ·  local mainnet Zaino");
    }
    if url.contains(":8137") {
        return format!("{url}  ·  local Zaino");
    }
    if url.contains(":28137") {
        return format!("{url}  ·  local regtest Zaino");
    }
    if is_loopback_light_url(url) {
        return format!("{url}  ·  loopback");
    }
    url.to_string()
}

/// Shared public compact-block hosts (batch-capped). A user-run Zaino on a
/// public IP is **not** this — that still uses local 4000/prefetch-4 batches.
pub fn is_public_lwd_url(url: &str) -> bool {
    let host = light_url_host(url);
    host == "zec.rocks" || host.ends_with(".zec.rocks")
}

/// Local / dedicated Zaino: 4000-block batches. `zec.rocks`: 1000.
/// Independent of t-scan (GetAddressUtxos stays loopback-only).
pub fn uses_fast_sync(url: &str) -> bool {
    !is_public_lwd_url(url)
}

/// tonic `Endpoint` needs a scheme. Plaintext Zaino is `http://` (h2c).
pub fn normalize_grpc_url(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    if s.contains("://") {
        return s.to_string();
    }
    if is_public_lwd_url(s) {
        format!("https://{s}")
    } else {
        format!("http://{s}")
    }
}

/// Coarse sync progress for UIs (web + GPUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    pub stage: SyncStage,
    pub percent: f32,
    pub message: String,
    pub tip_height: Option<u64>,
    pub scanned_height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks_per_sec: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_blocks_per_sec: Option<f32>,
    #[serde(default)]
    pub download_active: bool,
    #[serde(default)]
    pub decrypt_active: bool,
    #[serde(default)]
    pub persist_active: bool,
}

impl Default for SyncProgress {
    fn default() -> Self {
        Self {
            stage: SyncStage::Idle,
            percent: 0.0,
            message: String::new(),
            tip_height: None,
            scanned_height: None,
            downloaded_height: None,
            eta_secs: None,
            blocks_per_sec: None,
            download_blocks_per_sec: None,
            download_active: false,
            decrypt_active: false,
            persist_active: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStage {
    Idle,
    Connecting,
    Downloading,
    Scanning,
    Enhancing,
    CatchingUp,
    Synced,
    Error,
}

/// Product policy: ZODL-style.
#[derive(Debug, Clone, Copy)]
pub struct ProductPolicy;

impl ProductPolicy {
    /// Transparent funds may be received, then must be shielded before spend.
    pub const AUTO_SHIELD: bool = true;
    /// First-class transparent sends are out of v1 scope.
    pub const ALLOW_TRANSPARENT_SPEND: bool = false;
}

/// Payment-note recovery state. Empty means no text memo was recovered; binary
/// memos are not displayed. Unknown covers older snapshots and native providers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoStatus {
    Pending,
    Available,
    Empty,
    Unavailable,
    #[default]
    Unknown,
    NotApplicable,
}

/// One wallet transaction as shown in history (from `v_transactions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub txid: String,
    pub mined_height: Option<u32>,
    pub expiry_height: Option<u32>,
    /// Signed zatoshis: received − spent for this account (fee included in spent).
    pub account_delta_zat: i64,
    pub spent_zat: u64,
    pub received_zat: u64,
    pub fee_zat: Option<u64>,
    pub sent_note_count: u32,
    pub received_note_count: u32,
    pub memo_count: u32,
    pub has_change: bool,
    pub is_shielding: bool,
    pub expired_unmined: bool,
    /// Decrypted UTF-8 memos (empty until GetTransaction enhance).
    #[serde(default)]
    pub memos: Vec<String>,
    #[serde(default)]
    pub memo_status: MemoStatus,
    /// Unix seconds from the compact block (0/None if unknown).
    #[serde(default)]
    pub block_time: Option<u32>,
    /// Confirmations at scanned tip (`scanned - mined + 1`).
    #[serde(default)]
    pub confirmations: Option<u32>,
    /// Per-pool movement for the TS classifier. 0 when unknown (native sqlite view).
    #[serde(default)]
    pub transparent_received: u64,
    #[serde(default)]
    pub transparent_spent: u64,
    #[serde(default)]
    pub sapling_received: u64,
    #[serde(default)]
    pub sapling_spent: u64,
    #[serde(default)]
    pub orchard_received: u64,
    #[serde(default)]
    pub orchard_spent: u64,
    #[serde(default)]
    pub ironwood_received: u64,
    #[serde(default)]
    pub ironwood_spent: u64,
    /// Raw transaction facts used to distinguish external sends from
    /// transparent-to-shielded moves. Optional for native SQLite history.
    #[serde(default)]
    pub history_metadata_complete: bool,
    #[serde(default)]
    pub outgoing_shielded_zat: u64,
    #[serde(default)]
    pub transparent_inputs: Vec<web::HistoryOutpoint>,
    #[serde(default)]
    pub transparent_outputs: Vec<web::HistoryOutput>,
}

/// Filter for `history` / `GET /history?status=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryStatusFilter {
    Mined,
    Pending,
    Expired,
}

impl HistoryStatusFilter {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mined" => Some(Self::Mined),
            "pending" => Some(Self::Pending),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

impl HistoryEntry {
    pub fn status(&self) -> &'static str {
        if self.mined_height.is_some() {
            "mined"
        } else if self.expired_unmined {
            "expired"
        } else {
            "pending"
        }
    }

    pub fn matches_status(&self, filter: HistoryStatusFilter) -> bool {
        match filter {
            HistoryStatusFilter::Mined => self.mined_height.is_some(),
            HistoryStatusFilter::Expired => self.mined_height.is_none() && self.expired_unmined,
            HistoryStatusFilter::Pending => self.mined_height.is_none() && !self.expired_unmined,
        }
    }

    /// JSON object matching `@z-stack/core` `HistoryEntry` (camelCase + `status`).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "txid": self.txid,
            "status": self.status(),
            "minedHeight": self.mined_height,
            "expiryHeight": self.expiry_height,
            "accountDeltaZat": self.account_delta_zat,
            "spentZat": self.spent_zat,
            "receivedZat": self.received_zat,
            "feeZat": self.fee_zat,
            "sentNoteCount": self.sent_note_count,
            "receivedNoteCount": self.received_note_count,
            "memoCount": self.memo_count,
            "hasChange": self.has_change,
            "isShielding": self.is_shielding,
            "expiredUnmined": self.expired_unmined,
            "memos": self.memos,
            "memoStatus": self.memo_status,
            "blockTime": self.block_time,
            "confirmations": self.confirmations,
            "transparentReceived": self.transparent_received,
            "transparentSpent": self.transparent_spent,
            "saplingReceived": self.sapling_received,
            "saplingSpent": self.sapling_spent,
            "orchardReceived": self.orchard_received,
            "orchardSpent": self.orchard_spent,
            "ironwoodReceived": self.ironwood_received,
            "ironwoodSpent": self.ironwood_spent,
            "historyMetadataComplete": self.history_metadata_complete,
            "outgoingShieldedZat": self.outgoing_shielded_zat,
            "transparentInputs": self.transparent_inputs,
            "transparentOutputs": self.transparent_outputs,
        })
    }
}

/// Pool balances in zatoshis.
///
/// `*_available` is spendable under [`confirmations_policy`].
/// `*_pending` is not yet spendable: mempool / unmined change, or mined notes
/// still waiting for trusted/untrusted confirmations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub sapling_available: u64,
    pub orchard_available: u64,
    pub ironwood_available: u64,
    pub transparent_available: u64,
    pub total_available: u64,
    #[serde(default)]
    pub sapling_pending: u64,
    #[serde(default)]
    pub orchard_pending: u64,
    #[serde(default)]
    pub ironwood_pending: u64,
    #[serde(default)]
    pub transparent_pending: u64,
    #[serde(default)]
    pub total_pending: u64,
}

/// 1 ZEC = 100_000_000 zatoshis.
pub const ZATOSHI_PER_ZEC: u64 = 100_000_000;

/// Conservative ZIP-317 pad so UIs can gate Send before propose.
pub const FEE_PAD_ZAT: u64 = 10_000;

/// Default auto-shield / migrate dust floor (zatoshis).
pub const SHIELD_THRESHOLD_ZAT: u64 = 100_000;

/// Spendable orchard minus a ZIP-317 pad. Never negative.
/// Prefer [`NativeWallet::max_send`] / [`web::WebWallet::max_send`] when notes exist.
pub fn max_send_zat(orchard_available: u64) -> u64 {
    orchard_available.saturating_sub(FEE_PAD_ZAT)
}

/// ZIP-317 fee paid by a `propose_transfer` / `propose_shielding` proposal.
pub fn proposal_fee_zat<F, N>(proposal: &zcash_client_backend::proposal::Proposal<F, N>) -> u64 {
    proposal
        .steps()
        .iter()
        .map(|s| u64::from(s.balance().fee_required()))
        .sum()
}

pub fn fee_estimate_json(fee_zat: u64) -> serde_json::Value {
    serde_json::json!({
        "feeZat": fee_zat,
        "feeZec": format_zatoshis(fee_zat),
    })
}

pub fn max_send_json(max_send_zat: u64, fee_zat: u64) -> serde_json::Value {
    serde_json::json!({
        "maxSendZat": max_send_zat,
        "maxSendZec": format_zatoshis(max_send_zat),
        "feeZat": fee_zat,
        "feeZec": format_zatoshis(fee_zat),
    })
}

/// Format zatoshis as a decimal ZEC string without trailing zeros (`687.5`, `0.0005`).
pub fn format_zatoshis(zats: u64) -> String {
    let whole = zats / ZATOSHI_PER_ZEC;
    let frac = zats % ZATOSHI_PER_ZEC;
    if frac == 0 {
        return whole.to_string();
    }
    let mut s = format!("{whole}.{frac:08}");
    while s.ends_with('0') {
        s.pop();
    }
    s
}

/// Parse a decimal ZEC amount (`1`, `0.0005`, `.5`) into zatoshis.
pub fn parse_zec_to_zatoshis(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty amount".into());
    }
    if s.contains('-') || s.contains('+') || s.contains('e') || s.contains('E') {
        return Err("amount must be a non-negative decimal ZEC value".into());
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return Err("empty amount".into());
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err("amount must be a non-negative decimal ZEC value".into());
    }
    if frac.len() > 8 {
        return Err("more than 8 decimal places".into());
    }
    let whole_n: u64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| "amount overflows u64".to_string())?
    };
    let mut frac_padded = frac.to_string();
    while frac_padded.len() < 8 {
        frac_padded.push('0');
    }
    let frac_n: u64 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded
            .parse()
            .map_err(|_| "amount overflows u64".to_string())?
    };
    whole_n
        .checked_mul(ZATOSHI_PER_ZEC)
        .and_then(|v| v.checked_add(frac_n))
        .ok_or_else(|| "amount overflows u64 zatoshis".into())
}

/// Smoke check that Zakura crypto crates link.
pub fn crypto_smoke() -> &'static str {
    let _ = std::any::type_name::<orchard::Note>();
    "zakura-orchard linked"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_zaino_url() {
        assert_eq!(LightServer::LocalZaino.as_url(), "http://127.0.0.1:8137");
        assert_eq!(LightServer::LocalRegtest.as_url(), "http://127.0.0.1:28137");
        assert!(LightServer::LocalZaino.allows_transparent_query());
        assert!(LightServer::LocalRegtest.allows_transparent_query());
        assert!(LightServer::Url("http://127.0.0.1:8137".into()).allows_transparent_query());
        assert!(LightServer::Url("http://localhost:28137".into()).allows_transparent_query());
        assert!(!LightServer::Url("https://zec.rocks:443".into()).allows_transparent_query());
        assert!(
            !LightServer::Url("https://testnet.zec.rocks:443".into()).allows_transparent_query()
        );
        assert!(!is_loopback_light_url("https://example.com:8137"));
        assert!(is_loopback_light_url("http://[::1]:8137"));
        assert!(is_public_lwd_url("https://zec.rocks:443"));
        assert!(is_public_lwd_url("https://testnet.zec.rocks:443"));
        assert!(!is_public_lwd_url("http://203.0.113.9:8137"));
        assert!(uses_fast_sync("http://203.0.113.9:8137"));
        assert!(!uses_fast_sync("https://zec.rocks:443"));
        assert_eq!(
            normalize_grpc_url("203.0.113.9:8137"),
            "http://203.0.113.9:8137"
        );
        assert_eq!(
            LightServer::parse("203.0.113.9:8137", Network::Mainnet).as_url(),
            "http://203.0.113.9:8137"
        );
        assert!(
            !LightServer::parse("203.0.113.9:8137", Network::Mainnet).allows_transparent_query()
        );
        assert_eq!(normalize_grpc_url("zec.rocks:443"), "https://zec.rocks:443");
        assert_eq!(
            LightServer::parse("local", Network::Mainnet).as_url(),
            LightServer::LOCAL_ZAINO_GRPC_MAINNET
        );
        assert_eq!(
            LightServer::local_for_network(Network::Mainnet).as_url(),
            LightServer::LOCAL_ZAINO_GRPC_MAINNET
        );
        assert!(LightServer::parse("local", Network::Mainnet).allows_transparent_query());
        assert_eq!(
            describe_light_url(LightServer::LOCAL_ZAINO_GRPC_MAINNET),
            "http://127.0.0.1:8138  ·  local mainnet Zaino"
        );
        assert_eq!(
            light_url_host_port(LightServer::LOCAL_ZAINO_GRPC_MAINNET),
            Some(("127.0.0.1".into(), 8138))
        );
        assert_eq!(
            light_url_host_port("http://[::1]:8137"),
            Some(("::1".into(), 8137))
        );
        assert!(describe_light_url("https://zec.rocks:443").contains("public LWD"));
        assert!(
            !LightServer::parse("https://zec.rocks:443", Network::Mainnet)
                .allows_transparent_query()
        );
        assert_eq!(
            LightServer::for_network(Network::Regtest).as_url(),
            LightServer::LOCAL_REGTEST_GRPC
        );
        assert_eq!(
            LightServer::LocalRegtest.zebra_rpc_url().as_deref(),
            Some(LightServer::LOCAL_REGTEST_ZEBRA_RPC)
        );
        assert_eq!(
            LightServer::local_for_network(Network::Testnet).as_url(),
            LightServer::LOCAL_ZAINO_GRPC
        );
        assert_eq!(
            LightServer::local_validator_rpc(Network::Testnet),
            LightServer::LOCAL_ZAKURA_RPC_TESTNET
        );
        assert!(LightServer::LocalZaino.zebra_rpc_url().is_none());
    }

    #[test]
    fn crypto_links() {
        assert_eq!(crypto_smoke(), "zakura-orchard linked");
    }

    #[test]
    fn confirmations_policy_matches_old_wallet_on_regtest() {
        let r = confirmations_policy(Network::Regtest);
        assert_eq!(u32::from(r.trusted()), 1);
        assert_eq!(u32::from(r.untrusted()), 1);
        #[cfg(feature = "transparent-inputs")]
        assert!(r.allow_zero_conf_shielding());
        let m = confirmations_policy(Network::Mainnet);
        assert_eq!(u32::from(m.trusted()), 3);
        assert_eq!(u32::from(m.untrusted()), 3);
        let t = confirmations_policy(Network::Testnet);
        assert_eq!(u32::from(t.trusted()), 3);
        assert_eq!(u32::from(t.untrusted()), 3);
    }

    #[test]
    fn parse_networks() {
        assert_eq!(Network::parse("regtest"), Some(Network::Regtest));
        assert_eq!(Network::Regtest.as_str(), "regtest");
        assert_eq!(
            Network::Regtest.activation_height(NetworkUpgrade::Nu6_2),
            Some(BlockHeight::from_u32(2))
        );
        assert_eq!(
            Network::Regtest.activation_height(NetworkUpgrade::Nu6_3),
            Some(BlockHeight::from_u32(regtest_nu6_3_height()))
        );
    }

    #[test]
    fn max_send_pad_and_fee_json() {
        assert_eq!(max_send_zat(0), 0);
        assert_eq!(max_send_zat(9_999), 0);
        assert_eq!(max_send_zat(50_000), 40_000);
        let fee = fee_estimate_json(10_000);
        assert_eq!(fee["feeZat"], 10_000);
        assert_eq!(fee["feeZec"], "0.0001");
        let max = max_send_json(40_000, 10_000);
        assert_eq!(max["maxSendZat"], 40_000);
        assert_eq!(max["feeZat"], 10_000);
    }

    #[test]
    fn zat_format_roundtrip() {
        assert_eq!(format_zatoshis(0), "0");
        assert_eq!(format_zatoshis(1), "0.00000001");
        assert_eq!(format_zatoshis(50_000), "0.0005");
        assert_eq!(format_zatoshis(100_000_000), "1");
        assert_eq!(format_zatoshis(68_750_000_000), "687.5");
        assert_eq!(parse_zec_to_zatoshis("1").unwrap(), 100_000_000);
        assert_eq!(parse_zec_to_zatoshis("0.0005").unwrap(), 50_000);
        assert_eq!(parse_zec_to_zatoshis(".5").unwrap(), 50_000_000);
        assert_eq!(parse_zec_to_zatoshis("687.5").unwrap(), 68_750_000_000);
        assert!(parse_zec_to_zatoshis("").is_err());
        assert!(parse_zec_to_zatoshis("abc").is_err());
        assert!(parse_zec_to_zatoshis("1.123456789").is_err());
        assert!(parse_zec_to_zatoshis("-1").is_err());
        assert_eq!(
            zip321_uri("uregtest1abc", None).unwrap(),
            "zcash:uregtest1abc"
        );
        assert_eq!(
            zip321_uri("uregtest1abc", Some("0.0005")).unwrap(),
            "zcash:uregtest1abc?amount=0.0005"
        );
        assert!(zip321_uri("", None).is_err());
        assert!(zip321_uri("ua with space", None).is_err());
        assert!(zip321_uri("uregtest1abc", Some("nope")).is_err());
    }

    #[test]
    fn history_status() {
        let mined = HistoryEntry {
            txid: "ab".into(),
            mined_height: Some(10),
            expiry_height: None,
            account_delta_zat: 1,
            spent_zat: 0,
            received_zat: 1,
            fee_zat: None,
            sent_note_count: 0,
            received_note_count: 1,
            memo_count: 0,
            has_change: false,
            is_shielding: false,
            expired_unmined: false,
            memos: vec![],
            block_time: None,
            confirmations: Some(1),
            ..Default::default()
        };
        assert_eq!(mined.status(), "mined");
        assert_eq!(mined.to_json()["status"], "mined");
        assert!(mined.matches_status(HistoryStatusFilter::Mined));
        assert!(!mined.matches_status(HistoryStatusFilter::Pending));
        let pending = HistoryEntry {
            mined_height: None,
            expired_unmined: false,
            ..mined.clone()
        };
        assert!(pending.matches_status(HistoryStatusFilter::Pending));
        let expired = HistoryEntry {
            expired_unmined: true,
            ..pending
        };
        assert!(expired.matches_status(HistoryStatusFilter::Expired));
        assert_eq!(
            HistoryStatusFilter::parse("Pending"),
            Some(HistoryStatusFilter::Pending)
        );
        assert!(HistoryStatusFilter::parse("nope").is_none());
    }
}
