//! SQLite-backed native wallet: create, sync, balance, shield, send.

mod database_lease;
#[cfg(feature = "native-pir")]
mod recovery_blocks;
#[cfg(feature = "native-pir")]
mod recovery_cancellation;
mod recovery_guard;
#[cfg(feature = "native-pir")]
pub use recovery_blocks::{verify_regtest_recovery_blocks, VerifiedRegtestRecoveryBlocks};
#[cfg(feature = "native-pir")]
pub use recovery_cancellation::RecoveryCancellation;
mod payments;
#[cfg(feature = "native-pir")]
mod pir;
#[cfg(feature = "native-pir")]
pub use pir::{
    PirConfirmedTransaction, PirDiscoveryReport, PirRecoveryReport, RegtestAcceptedChain,
};
mod public_scan;
pub use payments::PaymentReceipt;
pub use public_scan::RegtestScanSchedule;

use crate::error::{EngineError, Result};
use crate::native::block_cache::FsBlockCache;
use crate::native::lwd::{self, LwdClient};
use crate::native::seed::{SeedStore, UnlockPolicy};
use crate::{
    parse_zec_to_zatoshis, Balance, HistoryEntry, LightServer, Network as ZNetwork, SyncProgress,
    SyncStage,
};
use bip39::Mnemonic;
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use rand::TryRng;
use secrecy::{ExposeSecret, SecretString, SecretVec};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use zcash_address::ZcashAddress;
use zcash_client_backend::{
    data_api::{
        enhance_pir::{EnhancePirRead, EnhancementMode, TransactionEnhancementWork},
        status::{TransactionStatusMode, TransactionStatusRead, TransactionStatusWork},
        wallet::decrypt_and_store_transaction,
        wallet::{
            create_proposed_transactions,
            input_selection::{GreedyInputSelector, SpendPolicy},
            propose_shielding, propose_transfer, ConfirmationsPolicy, SpendingKeys,
        },
        Account, AccountBirthday, AccountPurpose, CoinbaseFilter, TransactionStatus, WalletRead,
        WalletWrite,
    },
    fees::{standard::SingleOutputChangeStrategy, DustOutputPolicy, StandardFeeRule},
    proto::service::{
        BlockId, ChainSpec, Empty, GetAddressUtxosArg, GetSubtreeRootsArg, RawTransaction,
        ShieldedProtocol, TxFilter,
    },
    wallet::OvkPolicy,
    zip321::{Payment, TransactionRequest},
};
use zcash_client_sqlite::{util::SystemClock, wallet::init::init_wallet_db, AccountUuid, WalletDb};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    memo::{Memo, MemoBytes},
    value::Zatoshis,
    ShieldedPool, TxId,
};
use zeroize::Zeroize;
use zip32::AccountId as Zip32AccountId;

/// Wall-clock budget for memo enhancement at the end of a sync.
const ENHANCE_BUDGET: Duration = Duration::from_secs(45);

/// Wall-clock budget for resubmitting unmined sends at the end of a sync.
const REBROADCAST_BUDGET: Duration = Duration::from_secs(30);

/// Compact-block batch on local Zaino. Public LWD uses [`crate::scan::sync_tuning`].
pub const BATCH_SIZE: u32 = crate::scan::BATCH_LOCAL;

/// Birthday safety margin when creating a new wallet near tip (blocks).
pub const BIRTHDAY_SAFETY_BUFFER: u32 = 100;

/// Max *unscanned* birthday→tip gap. Same as [`crate::scan::MAX_SYNC_BLOCKS`].
pub const MAX_MEM_SYNC_BLOCKS: u32 = crate::scan::MAX_SYNC_BLOCKS;

fn is_retryable_sync_outage(err: &EngineError) -> bool {
    if super::pipeline::is_light_connection_outage(err) {
        return true;
    }
    matches!(err, EngineError::Transport(message)
        if message.to_ascii_lowercase().contains("timed out")
            || message.to_ascii_lowercase().contains("deadline exceeded"))
}

/// Highest scanned height the live progress reports. Downloaded heights do
/// not count: a range that times out is fetched again from the saved island.
fn sync_mark(live: &Option<Arc<Mutex<SyncProgress>>>) -> u64 {
    live.as_ref()
        .and_then(|lock| lock.lock().ok().and_then(|g| g.scanned_height))
        .unwrap_or(0)
}

/// 75–100% of `delay`, so wallets that lost the same server do not all
/// reconnect in the same instant when it comes back.
fn jittered(delay: Duration) -> Duration {
    use std::hash::BuildHasher;
    let noise = std::collections::hash_map::RandomState::new().hash_one(Instant::now()) % 1024;
    delay.mul_f64(0.75 + 0.25 * noise as f64 / 1023.0)
}

async fn light_port_open(url: &str) -> bool {
    let Some((host, port)) = crate::light_url_host_port(url) else {
        return true;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect((host.as_str(), port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Sleep for `delay`, but wake as soon as a light-server port that was closed
/// when the wait began accepts connections again (a local Zaino restart). An
/// open port that answered with errors waits the full delay.
async fn outage_backoff(delay: Duration, reopen: Option<&str>) {
    let deadline = tokio::time::Instant::now() + delay;
    let Some(url) = reopen else {
        tokio::time::sleep_until(deadline).await;
        return;
    };
    if light_port_open(url).await {
        tokio::time::sleep_until(deadline).await;
        return;
    }
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + OUTAGE_PROBE_EVERY))
            .await;
        if light_port_open(url).await {
            return;
        }
    }
}

const OUTAGE_PROBE_EVERY: Duration = Duration::from_millis(500);

/// Retry a sync through light-server outages. `grace` bounds one continuous
/// outage: an attempt that moved the scan before failing proves the server
/// came back, so the next failure starts a fresh outage with fresh backoff.
async fn retry_transient_sync<F, Fut>(
    live: Option<Arc<Mutex<SyncProgress>>>,
    reopen: Option<&str>,
    grace: Duration,
    first_delay: Duration,
    cancel: &AtomicBool,
    mut run: F,
) -> Result<(u32, SyncProgress)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(u32, SyncProgress)>>,
{
    let mut first_outage = None;
    let mut retries = 0u32;
    let mut reached = sync_mark(&live);
    loop {
        let result = tokio::select! {
            result = run() => result,
            () = wait_for_sync_cancellation(cancel) => return Err(sync_cancelled()),
        };
        if cancel.load(Ordering::Acquire) {
            return Err(sync_cancelled());
        }
        match result {
            Ok(done) => return Ok(done),
            Err(err) if is_retryable_sync_outage(&err) => {
                let mark = sync_mark(&live);
                if mark > reached {
                    reached = mark;
                    first_outage = None;
                    retries = 0;
                }
                let began = *first_outage.get_or_insert_with(Instant::now);
                let remaining = grace.saturating_sub(began.elapsed());
                if remaining.is_zero() {
                    return Err(err);
                }
                retries += 1;
                let backoff = first_delay.saturating_mul(1 << retries.saturating_sub(1).min(3));
                let delay = jittered(backoff).min(remaining);
                if let Some(lock) = &live {
                    if let Ok(mut progress) = lock.lock() {
                        progress.stage = SyncStage::Connecting;
                        progress.message = format!(
                            "Zaino disconnected; reconnecting within {}s. Saved scan retained.",
                            delay.as_secs_f32().ceil()
                        );
                        progress.eta_secs = None;
                        progress.blocks_per_sec = None;
                        progress.download_blocks_per_sec = None;
                        progress.download_active = false;
                        progress.decrypt_active = false;
                        progress.persist_active = false;
                    }
                }
                warn!("light server disconnected during sync; waiting to resume");
                tokio::select! {
                    () = outage_backoff(delay, reopen) => {},
                    () = wait_for_sync_cancellation(cancel) => return Err(sync_cancelled()),
                }
            }
            Err(err) => return Err(err),
        }
    }
}

fn sync_cancelled() -> EngineError {
    EngineError::Message("sync cancelled; saved scan retained".into())
}

async fn wait_for_sync_cancellation(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct SyncTickerGuard {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SyncTickerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) type SyncDb = WalletDb<rusqlite::Connection, ZNetwork, SystemClock, UnwrapErr<SysRng>>;
type Db = SyncDb;

fn new_rng() -> UnwrapErr<SysRng> {
    UnwrapErr(SysRng)
}

fn shield_confirmations(network: ZNetwork) -> ConfirmationsPolicy {
    crate::confirmations_policy(network)
}

fn shielded_spend_policy() -> SpendPolicy {
    SpendPolicy::shielded_pools([
        ShieldedPool::Sapling,
        ShieldedPool::Orchard,
        ShieldedPool::Ironwood,
    ])
}

/// How to unlock the seed for create/restore/spend.
#[derive(Clone, Debug)]
pub struct SeedAuth {
    pub passphrase: Option<String>,
    pub windows_credential: bool,
    /// One-shot BIP-39 paste for view-only wallets (not written unless save is requested).
    pub mnemonic: Option<String>,
    pub unlock_policy: UnlockPolicy,
}

impl SeedAuth {
    pub fn passphrase(p: impl Into<String>) -> Self {
        Self {
            passphrase: Some(p.into()),
            windows_credential: false,
            mnemonic: None,
            unlock_policy: UnlockPolicy::Session,
        }
    }

    pub fn windows_credential() -> Self {
        Self {
            passphrase: None,
            windows_credential: true,
            mnemonic: None,
            unlock_policy: UnlockPolicy::Always,
        }
    }

    pub fn both(passphrase: impl Into<String>) -> Self {
        Self {
            passphrase: Some(passphrase.into()),
            windows_credential: true,
            mnemonic: None,
            unlock_policy: UnlockPolicy::Session,
        }
    }

    pub fn mnemonic_once(words: impl Into<String>) -> Self {
        Self {
            passphrase: None,
            windows_credential: false,
            mnemonic: Some(words.into()),
            unlock_policy: UnlockPolicy::EachSpend,
        }
    }

    fn validate_for_save(&self) -> Result<()> {
        if self.passphrase.is_none() && !self.windows_credential {
            return Err(EngineError::SeedLocked);
        }
        Ok(())
    }
}

/// `wallet.json` is written last by create/restore, and a torn copy makes the
/// folder look empty, so it is always replaced whole.
fn write_meta(path: &Path, meta: &WalletMeta) -> Result<()> {
    let json = serde_json::to_string_pretty(meta).expect("meta");
    super::replace_file(path, json.as_bytes(), |_| Ok(()))
        .map_err(|e| EngineError::Message(format!("wallet.json: {e}")))
}

#[derive(Debug, Clone)]
pub struct WalletPaths {
    pub root: PathBuf,
    pub data_db: PathBuf,
    pub block_cache: PathBuf,
    pub meta_path: PathBuf,
}

impl WalletPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            data_db: root.join("data.sqlite"),
            block_cache: root.join("blocks"),
            meta_path: root.join("wallet.json"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.block_cache)?;
        Ok(())
    }

    fn reset_backup(&self) -> PathBuf {
        self.data_db.with_extension("sqlite.bak")
    }

    /// Refuse to create or restore over a finished wallet. `wallet.json` is
    /// written last, so it marks one even when `data.sqlite` is missing (a
    /// rescan interrupted mid-swap); writing a new seed there would destroy the
    /// only copy of the old one. A database without `wallet.json` is a create
    /// or restore that never returned: nothing was scanned and no address was
    /// shown, so it is set aside rather than wedging every later attempt.
    fn prepare_new_wallet(&self) -> Result<()> {
        if self.meta_path.exists() || self.reset_backup().exists() {
            return Err(EngineError::AlreadyExists(self.root.display().to_string()));
        }
        if self.data_db.exists() {
            let _lease = database_lease::exclusive(&self.data_db)?;
            if self.meta_path.exists() || self.reset_backup().exists() {
                return Err(EngineError::AlreadyExists(self.root.display().to_string()));
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let aside = self
                .data_db
                .with_file_name(format!("data.sqlite.incomplete-{stamp}"));
            for suffix in ["", "-wal", "-shm"] {
                let from = PathBuf::from(format!("{}{suffix}", self.data_db.display()));
                if from.exists() {
                    std::fs::rename(&from, format!("{}{suffix}", aside.display()))?;
                }
            }
            warn!("set aside an unfinished wallet database");
        }
        Ok(())
    }

    /// Put back a database that an older rescan moved aside and never replaced.
    fn recover_interrupted_reset(&self) {
        let bak = self.reset_backup();
        if !self.data_db.exists() && bak.exists() {
            let Ok(_lease) = database_lease::exclusive(&self.data_db) else {
                warn!("wallet database recovery is busy");
                return;
            };
            if self.data_db.exists() || !bak.exists() {
                return;
            }
            match std::fs::rename(&bak, &self.data_db) {
                Ok(()) => warn!("restored data.sqlite from an interrupted rescan"),
                Err(_e) => warn!("could not restore data.sqlite.bak"),
            }
        }
    }
}

/// WAL persists on the file. `synchronous` does **not** — it is per connection.
/// Setting NORMAL on a throwaway handle left the wallet connection at FULL and
/// fsync'd every persist. Apply these on the same `Connection` `WalletDb` owns.
fn apply_wallet_pragmas(conn: &rusqlite::Connection) {
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    let _ = conn.busy_timeout(Duration::from_secs(5));
    match conn.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0)) {
        Ok(1) => {}
        _other => warn!("wallet sqlite synchronous is not NORMAL (1)"),
    }
}

fn open_wallet_db(path: &Path, network: ZNetwork) -> Result<Db> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| EngineError::WalletDb(format!("open: {e}")))?;
    rusqlite::vtab::array::load_module(&conn)
        .map_err(|e| EngineError::WalletDb(format!("array module: {e}")))?;
    apply_wallet_pragmas(&conn);
    Ok(
        WalletDb::from_connection(conn, network, SystemClock, new_rng())
            .with_enhancement_mode(EnhancementMode::Standard)
            .with_status_mode(TransactionStatusMode::Public),
    )
}

/// Highest height of the last *filled* contiguous `blocks` island.
///
/// `WalletRead::chain_height` is `MAX(scan_queue.end)-1` — the displayed chain tip —
/// not the last trial-decrypted block. Using that as "already scanned" drops
/// 3_418_129→tip while notes sit at 3_424_719. Near-tip Verify crumbs (a handful
/// of blocks at tip) are ignored when a real island sits below a gap.
pub(super) fn last_filled_island_end(db_path: &Path, birthday: u32, tip: u32) -> u32 {
    let fallback = birthday.saturating_sub(1);
    let Ok(conn) = rusqlite::Connection::open(db_path) else {
        return fallback;
    };
    let _ = conn.pragma_update(None, "query_only", true);
    let queue_scanned = (|| {
        let mut stmt = conn
            .prepare(
                "SELECT block_range_start, block_range_end FROM scan_queue
             WHERE priority = 10 AND block_range_end > ?1
             ORDER BY block_range_start",
            )
            .ok()?;
        let rows = stmt
            .query_map([birthday], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?))
            })
            .ok()?;
        let mut cursor = birthday;
        let mut found = false;
        for row in rows {
            let (start, end) = row.ok()?;
            found = true;
            if start > cursor {
                break;
            }
            cursor = cursor.max(end);
        }
        found.then_some(cursor.saturating_sub(1))
    })();
    let Ok(mut stmt) = conn.prepare(
        "SELECT b.height FROM blocks b
         LEFT JOIN blocks n ON n.height = b.height + 1
         WHERE n.height IS NULL
         ORDER BY b.height",
    ) else {
        return fallback;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, u32>(0)) else {
        return fallback;
    };
    let mut ends: Vec<u32> = rows.filter_map(|r| r.ok()).collect();
    if ends.is_empty() {
        return fallback;
    }
    while ends.len() >= 2 {
        let last = *ends.last().unwrap_or(&fallback);
        let prev = ends[ends.len() - 2];
        let start = conn
            .query_row(
                "SELECT MIN(height) FROM blocks WHERE height > ?1",
                [prev],
                |row| row.get::<_, Option<u32>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or(last);
        let last_len = last.saturating_sub(start).saturating_add(1);
        if last_len <= crate::NEAR_TIP_BLOCKS
            && last.saturating_add(1) >= tip.saturating_sub(crate::NEAR_TIP_BLOCKS)
        {
            ends.pop();
        } else {
            break;
        }
    }
    let blocks_island = ends.last().copied().unwrap_or(fallback);
    match queue_scanned {
        // A later blocks island (or Scanned queue island) cannot prove the
        // intervening heights were scanned. Queue coverage must be contiguous
        // from the birthday; sparse block rows are only a legacy fallback.
        Some(q) => q.min(tip),
        None => blocks_island,
    }
}

/// Highest `blocks.height` at or below `requested`.
///
/// Sparse persist writes one watermark row per 8000 (plus note heights), not
/// a row per empty compact. sqlite `truncate_to_height` only accepts a height
/// that exists in `blocks`. Queue watermarks are not enough.
pub(super) fn max_blocks_row_at_or_below(db_path: &Path, requested: u32) -> Result<Option<u32>> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| EngineError::WalletDb(format!("open rewind source: {e}")))?;
    conn.query_row(
        "SELECT MAX(height) FROM blocks WHERE height <= ?1",
        [requested],
        |row| row.get::<_, Option<u32>>(0),
    )
    .map_err(|e| EngineError::WalletDb(format!("read rewind source: {e}")))
}

/// Reject contradictory checkpoint/compact metadata without changing wallet
/// state. Clearing only scan coverage would leave old notes and tree shards in
/// place, and fabricating a new checkpoint position would be unverified. An
/// explicit full rescan can rebuild from an authenticated birthday frontier.
pub(super) fn validate_empty_walk_scan(db_path: &Path) -> Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| EngineError::WalletDb(format!("open checkpoint source: {e}")))?;
    let stale = stale_checkpoint_rows(&conn)
        .map_err(|e| EngineError::WalletDb(format!("validate checkpoints: {e}")))?;
    if stale.is_empty() {
        Ok(())
    } else {
        Err(EngineError::WalletDb(
            "wallet checkpoint disagrees with persisted compact metadata; wallet data was retained. Restore into a new wallet or perform an explicit full rescan from a trusted birthday".into(),
        ))
    }
}

fn stale_checkpoint_rows(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<Vec<(&'static str, u32)>> {
    let mut stale = Vec::new();
    for prefix in ["orchard", "sapling", "ironwood"] {
        let sql = format!(
            "SELECT c.checkpoint_id FROM {prefix}_tree_checkpoints c
             JOIN blocks b ON b.height = c.checkpoint_id
             WHERE b.{prefix}_commitment_tree_size IS NOT NULL
               AND b.{prefix}_commitment_tree_size != COALESCE(c.position + 1, 0)"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        for row in rows {
            stale.push((prefix, row?));
        }
    }
    Ok(stale)
}

/// Height we may pass to `truncate_to_height`. `None` = skip (wipe / empty /
/// sparse island — never invent a missing height such as birthday+4404).
#[cfg(test)]
pub(super) fn rewind_target_from_rows(requested: u32, rows: &[u32]) -> Option<u32> {
    rows.iter().copied().filter(|&h| h <= requested).max()
}

pub(super) fn rewind_target(db_path: &Path, requested: u32) -> Result<Option<u32>> {
    max_blocks_row_at_or_below(db_path, requested)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WalletMeta {
    network: String,
    server: String,
    birthday_height: u32,
    account_index: u32,
    /// Independent of `server` (Zaino). Zakura/Zebra JSON-RPC for sendraw / getrawtx.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validator_rpc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ufvk: Option<String>,
    #[serde(default)]
    view_only: bool,
    #[serde(default)]
    unlock_policy: UnlockPolicy,
    #[serde(default)]
    os_unlock: bool,
    #[serde(default)]
    allow_deep_sync: bool,
}

#[derive(Debug)]
pub struct CreatedWallet {
    pub mnemonic: String,
    pub birthday_height: u32,
    pub unified_address: String,
    pub ufvk: String,
}

/// Result of probing a compact-block light server (Zaino / public LWD).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LightProbe {
    pub ok: bool,
    pub url: String,
    pub chain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sapling_activation: Option<u32>,
    pub t_scan: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct NativeWallet {
    network_access: bool,
    paths: WalletPaths,
    network: ZNetwork,
    server: LightServer,
    meta: WalletMeta,
}

impl NativeWallet {
    pub fn paths(&self) -> &WalletPaths {
        &self.paths
    }

    pub fn server_url(&self) -> String {
        self.server.as_url()
    }

    /// Validator JSON-RPC if configured on this wallet, else implied by the light server.
    pub fn validator_rpc_url(&self) -> Option<String> {
        if !self.network_access {
            return None;
        }
        self.meta
            .validator_rpc
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.server.zebra_rpc_url())
    }

    pub fn set_light_server(&mut self, server: LightServer) -> Result<()> {
        self.server = server;
        self.meta.server = self.server.as_url();
        self.save_meta()
    }

    pub fn set_validator_rpc(&mut self, url: Option<String>) -> Result<()> {
        self.meta.validator_rpc = url
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        self.save_meta()
    }

    fn save_meta(&self) -> Result<()> {
        write_meta(&self.paths.meta_path, &self.meta)
    }

    pub fn network(&self) -> ZNetwork {
        self.network
    }

    pub fn birthday_height(&self) -> u32 {
        self.meta.birthday_height
    }

    fn account_index(&self) -> Zip32AccountId {
        Zip32AccountId::try_from(self.meta.account_index).unwrap_or(Zip32AccountId::ZERO)
    }

    fn seed_store(&self) -> SeedStore {
        SeedStore::new(&self.paths.root)
    }

    pub(super) fn open_db(&self) -> Result<Db> {
        let mut db = open_wallet_db(&self.paths.data_db, self.network)?;
        // Migrations: only when needed. Passing None avoids seed-required failures on open;
        // seed-bearing migrations run during create/restore with the seed present.
        if needs_migrate(&mut db)? {
            init_wallet_db(&mut db, None)
                .map_err(|e| EngineError::WalletDb(format!("migrate: {e}")))?;
        }
        Ok(db)
    }

    /// Native sync asks local Zaino for Ironwood roots too: 0.10 serves them,
    /// and `LwdChannel` answers an older server's InvalidArgument with an empty
    /// stream. Skipping them made every Ironwood commitment hash on restore.
    /// Regtest has no Ironwood tree. `Z_STACK_SKIP_IRONWOOD_SUBTREES` overrides.
    pub(super) fn skip_ironwood_subtrees(network: ZNetwork, _light_url: &str) -> bool {
        match std::env::var("Z_STACK_SKIP_IRONWOOD_SUBTREES")
            .ok()
            .as_deref()
        {
            Some("0") | Some("false") => false,
            Some("1") | Some("true") => true,
            _ => network == ZNetwork::Regtest,
        }
    }

    pub(super) async fn connect_url(url: &str, skip_ironwood_subtrees: bool) -> Result<LwdClient> {
        Self::connect_url_hot(url, skip_ironwood_subtrees).await
    }

    /// Native gRPC to Zaino / LWD. Same HTTP/2 windows as `z-wallet pipe`.
    /// Do **not** enable `http2_adaptive_window`: tonic applies it *after* these
    /// sizes and hyper then resets both to the spec 64 KiB. Localhost RTT is so
    /// small the adaptive BDP never grows, so GetBlockRange stays at ~16 blk/s.
    pub(super) async fn connect_url_hot(
        url: &str,
        skip_ironwood_subtrees: bool,
    ) -> Result<LwdClient> {
        use tonic::transport::{ClientTlsConfig, Endpoint};

        let url = crate::normalize_grpc_url(url);
        let mut endpoint = Endpoint::from_shared(url.clone())
            .map_err(|e| EngineError::Transport(format!("invalid url {url}: {e}")))?
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .tcp_nodelay(true)
            .concurrency_limit(256)
            .initial_stream_window_size(8 * 1024 * 1024)
            .initial_connection_window_size(64 * 1024 * 1024)
            .buffer_size(4096)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true);
        if url.starts_with("https://") {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .map_err(|e| EngineError::Transport(format!("tls: {e}")))?;
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| EngineError::Transport(format!("connect {url}: {e}")))?;
        Ok(lwd::client(channel, skip_ironwood_subtrees))
    }

    pub(super) async fn connect(&self) -> Result<LwdClient> {
        self.require_network_access()?;
        Self::connect_url(
            &self.server.as_url(),
            Self::skip_ironwood_subtrees(self.network, &self.server.as_url()),
        )
        .await
    }

    async fn assert_chain(client: &mut LwdClient, expected: ZNetwork) -> Result<()> {
        let info = client
            .get_lightd_info(Empty {})
            .await
            .map_err(|e| EngineError::Transport(format!("GetLightdInfo: {e}")))?
            .into_inner();
        let server = info.chain_name.to_lowercase();
        let ok = match expected {
            ZNetwork::Mainnet => server.contains("main"),
            // Zebra regtest getblockchaininfo.chain is "test"; Zaino may forward that.
            ZNetwork::Testnet => server.contains("test") && !server.contains("regtest"),
            ZNetwork::Regtest => server.contains("regtest") || server.contains("test"),
        };
        if !ok {
            return Err(EngineError::ChainMismatch {
                wallet: expected.as_str().into(),
                server: info.chain_name,
            });
        }
        Ok(())
    }

    fn check_sync_gap(birthday: u32, tip: u32, allow_deep: bool) -> Result<()> {
        let gap = tip.saturating_sub(birthday);
        let env = std::env::var("Z_STACK_ALLOW_DEEP_SYNC").ok().as_deref() == Some("1");
        if gap > MAX_MEM_SYNC_BLOCKS && !allow_deep && !env {
            return Err(EngineError::DeepSyncRejected {
                birthday,
                tip,
                max_gap: MAX_MEM_SYNC_BLOCKS,
            });
        }
        Ok(())
    }

    /// The gap guard stops a mistyped birthday from starting a huge first
    /// scan. A wallet that has scanned before is only catching up; refusing
    /// it stranded any wallet left closed for about 130 days, with no way in
    /// the app to continue.
    fn check_catch_up_gap(scanned: u32, birthday: u32, tip: u32, allow_deep: bool) -> Result<()> {
        if scanned >= birthday {
            return Ok(());
        }
        Self::check_sync_gap(birthday, tip, allow_deep)
    }

    /// Create a new wallet. Seed is written **before** account creation.
    pub async fn create(
        root: impl Into<PathBuf>,
        network: ZNetwork,
        server: Option<LightServer>,
        birthday_height: Option<u32>,
        auth: SeedAuth,
        account_index: u32,
    ) -> Result<(Self, CreatedWallet)> {
        auth.validate_for_save()?;
        let paths = WalletPaths::new(root);
        paths.prepare_new_wallet()?;
        paths.ensure_dirs()?;

        let server = server.unwrap_or_else(|| LightServer::for_network(network));

        let mut entropy = [0u8; 32];
        UnwrapErr(SysRng)
            .try_fill_bytes(&mut entropy)
            .map_err(|e| EngineError::Message(format!("rng: {e}")))?;
        let mnemonic = Mnemonic::from_entropy(&entropy)
            .map_err(|e| EngineError::Message(format!("mnemonic: {e}")))?;
        entropy.zeroize();
        let mnemonic_str = mnemonic.to_string();

        // Persist seed FIRST so a crash after this cannot orphan a seeded account.
        SeedStore::new(&paths.root).save(
            &mnemonic_str,
            auth.passphrase.as_deref(),
            auth.windows_credential,
        )?;

        let mut client = Self::connect_url(
            &server.as_url(),
            Self::skip_ironwood_subtrees(network, &server.as_url()),
        )
        .await?;
        Self::assert_chain(&mut client, network).await?;

        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip height out of range".into()))?;

        let birthday_h =
            birthday_height.unwrap_or_else(|| tip.saturating_sub(BIRTHDAY_SAFETY_BUFFER));
        if birthday_h > tip {
            return Err(EngineError::BirthdayAboveTip {
                birthday: birthday_h,
                tip,
            });
        }
        Self::check_sync_gap(birthday_h, tip, false)?;

        let prior_height = birthday_h.saturating_sub(1);
        let treestate = client
            .get_tree_state(BlockId {
                height: u64::from(prior_height),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetTreeState: {e}")))?
            .into_inner();

        let birthday = AccountBirthday::from_treestate(treestate, Some(BlockHeight::from(tip)))
            .map_err(|e| EngineError::WalletDb(format!("birthday: {e}")))?;

        let seed_bytes = SecretVec::new(mnemonic.to_seed("").to_vec());
        let zip_account = Zip32AccountId::try_from(account_index).unwrap_or(Zip32AccountId::ZERO);

        let mut db = open_wallet_db(&paths.data_db, network)?;
        init_wallet_db(
            &mut db,
            Some(SecretVec::new(seed_bytes.expose_secret().clone())),
        )
        .map_err(|e| EngineError::WalletDb(format!("init: {e}")))?;

        let (_id, usk) = if account_index == 0 {
            db.create_account("primary", &seed_bytes, &birthday, Some("z-stack"))
                .map_err(|e| EngineError::WalletDb(format!("create_account: {e}")))?
        } else {
            let (acct, usk) = db
                .import_account_hd(
                    "primary",
                    &seed_bytes,
                    zip_account,
                    &birthday,
                    Some("z-stack"),
                )
                .map_err(|e| EngineError::WalletDb(format!("import_account_hd: {e}")))?;
            (acct.id(), usk)
        };

        let ufvk = usk.to_unified_full_viewing_key();
        let ua = ufvk
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| EngineError::WalletDb(format!("address: {e:?}")))?
            .0
            .encode(&network);
        let ufvk_str = ufvk.encode(&network);

        let meta = WalletMeta {
            network: network.as_str().into(),
            server: server.as_url(),
            birthday_height: birthday_h,
            account_index,
            validator_rpc: None,
            ufvk: Some(ufvk_str.clone()),
            view_only: false,
            unlock_policy: auth.unlock_policy,
            os_unlock: auth.windows_credential,
            allow_deep_sync: false,
        };
        write_meta(&paths.meta_path, &meta)?;

        let wallet = Self {
            paths,
            network_access: true,
            network,
            server,
            meta,
        };
        Ok((
            wallet,
            CreatedWallet {
                mnemonic: mnemonic_str,
                birthday_height: birthday_h,
                unified_address: ua,
                ufvk: ufvk_str,
            },
        ))
    }

    pub async fn restore(
        root: impl Into<PathBuf>,
        mnemonic: &str,
        network: ZNetwork,
        server: Option<LightServer>,
        birthday_height: u32,
        auth: SeedAuth,
        account_index: u32,
    ) -> Result<(Self, String)> {
        auth.validate_for_save()?;
        let paths = WalletPaths::new(root);
        paths.prepare_new_wallet()?;
        paths.ensure_dirs()?;

        let mnemonic = Mnemonic::parse_normalized(mnemonic.trim())
            .map_err(|e| EngineError::Message(format!("invalid mnemonic: {e}")))?;
        let mnemonic_str = mnemonic.to_string();

        SeedStore::new(&paths.root).save(
            &mnemonic_str,
            auth.passphrase.as_deref(),
            auth.windows_credential,
        )?;

        let server = server.unwrap_or_else(|| LightServer::for_network(network));

        let mut client = Self::connect_url(
            &server.as_url(),
            Self::skip_ironwood_subtrees(network, &server.as_url()),
        )
        .await?;
        Self::assert_chain(&mut client, network).await?;

        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip height out of range".into()))?;

        if birthday_height > tip {
            return Err(EngineError::BirthdayAboveTip {
                birthday: birthday_height,
                tip,
            });
        }
        let deep = tip.saturating_sub(birthday_height) > MAX_MEM_SYNC_BLOCKS;
        Self::check_sync_gap(birthday_height, tip, true)?;

        let prior_height = birthday_height.saturating_sub(1);
        let treestate = client
            .get_tree_state(BlockId {
                height: u64::from(prior_height),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetTreeState: {e}")))?
            .into_inner();
        let birthday = AccountBirthday::from_treestate(treestate, Some(BlockHeight::from(tip)))
            .map_err(|e| EngineError::WalletDb(format!("birthday: {e}")))?;

        let seed_bytes = SecretVec::new(mnemonic.to_seed("").to_vec());
        let zip_account = Zip32AccountId::try_from(account_index).unwrap_or(Zip32AccountId::ZERO);

        let mut db = open_wallet_db(&paths.data_db, network)?;
        init_wallet_db(
            &mut db,
            Some(SecretVec::new(seed_bytes.expose_secret().clone())),
        )
        .map_err(|e| EngineError::WalletDb(format!("init: {e}")))?;

        let (_acct, usk) = db
            .import_account_hd(
                "primary",
                &seed_bytes,
                zip_account,
                &birthday,
                Some("z-stack"),
            )
            .map_err(|e| EngineError::WalletDb(format!("import_account_hd: {e}")))?;

        let ufvk = usk.to_unified_full_viewing_key();
        let ua = ufvk
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| EngineError::WalletDb(format!("address: {e:?}")))?
            .0
            .encode(&network);
        let ufvk_str = ufvk.encode(&network);

        let meta = WalletMeta {
            network: network.as_str().into(),
            server: server.as_url(),
            birthday_height,
            account_index,
            validator_rpc: None,
            ufvk: Some(ufvk_str),
            view_only: false,
            unlock_policy: auth.unlock_policy,
            os_unlock: auth.windows_credential,
            allow_deep_sync: deep,
        };
        write_meta(&paths.meta_path, &meta)?;

        Ok((
            Self {
                paths,
                network_access: true,
                network,
                server,
                meta,
            },
            ua,
        ))
    }

    /// View-only restore: UFVK + birthday. Scan and receive work; send needs a seed paste.
    pub async fn restore_ufvk(
        root: impl Into<PathBuf>,
        ufvk_str: &str,
        network: ZNetwork,
        server: Option<LightServer>,
        birthday_height: u32,
        account_index: u32,
    ) -> Result<(Self, String)> {
        let keys = crate::keys::account_from_ufvk(ufvk_str, network, account_index)?;
        let ufvk = UnifiedFullViewingKey::decode(&network, keys.ufvk.trim())
            .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;
        let paths = WalletPaths::new(root);
        paths.prepare_new_wallet()?;
        paths.ensure_dirs()?;
        let server = server.unwrap_or_else(|| LightServer::for_network(network));
        let mut client = Self::connect_url(
            &server.as_url(),
            Self::skip_ironwood_subtrees(network, &server.as_url()),
        )
        .await?;
        Self::assert_chain(&mut client, network).await?;
        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip height out of range".into()))?;
        if birthday_height > tip {
            return Err(EngineError::BirthdayAboveTip {
                birthday: birthday_height,
                tip,
            });
        }
        let deep = tip.saturating_sub(birthday_height) > MAX_MEM_SYNC_BLOCKS;
        Self::check_sync_gap(birthday_height, tip, true)?;
        let prior_height = birthday_height.saturating_sub(1);
        let treestate = client
            .get_tree_state(BlockId {
                height: u64::from(prior_height),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetTreeState: {e}")))?
            .into_inner();
        let birthday = AccountBirthday::from_treestate(treestate, Some(BlockHeight::from(tip)))
            .map_err(|e| EngineError::WalletDb(format!("birthday: {e}")))?;

        let mut db = open_wallet_db(&paths.data_db, network)?;
        init_wallet_db(&mut db, None).map_err(|e| EngineError::WalletDb(format!("init: {e}")))?;
        db.import_account_ufvk(
            "primary",
            &ufvk,
            &birthday,
            AccountPurpose::Spending { derivation: None },
            Some("z-stack-ufvk"),
        )
        .map_err(|e| EngineError::WalletDb(format!("import ufvk: {e}")))?;

        let meta = WalletMeta {
            network: network.as_str().into(),
            server: server.as_url(),
            birthday_height,
            account_index,
            validator_rpc: None,
            ufvk: Some(keys.ufvk.clone()),
            view_only: true,
            unlock_policy: UnlockPolicy::EachSpend,
            os_unlock: false,
            allow_deep_sync: deep,
        };
        write_meta(&paths.meta_path, &meta)?;
        Ok((
            Self {
                paths,
                network_access: true,
                network,
                server,
                meta,
            },
            keys.unified_address,
        ))
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let paths = WalletPaths::new(root);
        if paths.meta_path.exists() {
            paths.recover_interrupted_reset();
        }
        if !paths.data_db.exists() || !paths.meta_path.exists() {
            return Err(EngineError::NotFound(paths.root.display().to_string()));
        }
        let meta: WalletMeta = serde_json::from_str(&std::fs::read_to_string(&paths.meta_path)?)
            .map_err(|e| EngineError::Message(format!("wallet.json: {e}")))?;
        let network = ZNetwork::parse(&meta.network)
            .ok_or_else(|| EngineError::InvalidNetwork(meta.network.clone()))?;
        Ok(Self {
            paths,
            network_access: true,
            network,
            server: LightServer::parse(&meta.server, network),
            meta,
        })
    }

    /// Open local wallet state with transport capability disabled, regardless
    /// of stored endpoints. Signing and canonical verified recovery stay local.
    pub fn open_offline(root: impl Into<PathBuf>) -> Result<Self> {
        let mut wallet = Self::open(root)?;
        wallet.network_access = false;
        Ok(wallet)
    }

    fn require_network_access(&self) -> Result<()> {
        if self.network_access {
            Ok(())
        } else {
            Err(EngineError::Message("native_offline_wallet".into()))
        }
    }

    /// Open and persist a different compact-block URL when the UI field changed.
    pub fn open_with_light(root: impl Into<PathBuf>, server: Option<LightServer>) -> Result<Self> {
        let mut w = Self::open(root)?;
        if let Some(server) = server {
            if w.server.as_url() != server.as_url() {
                w.set_light_server(server)?;
            }
        }
        Ok(w)
    }

    fn load_seed(&self, auth: &SeedAuth) -> Result<SecretVec<u8>> {
        let words = if let Some(m) = auth
            .mnemonic
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            m.to_string()
        } else {
            let secret: SecretString = self.seed_store().load(
                auth.passphrase.as_deref(),
                auth.windows_credential || auth.passphrase.is_none(),
            )?;
            secret.expose_secret().clone()
        };
        let mnemonic = Mnemonic::parse_normalized(words.trim())
            .map_err(|e| EngineError::Message(format!("mnemonic: {e}")))?;
        if let Some(expect) = self.meta.ufvk.as_deref() {
            let got = crate::keys::account_from_mnemonic(
                mnemonic.to_string().as_str(),
                self.network,
                self.meta.account_index,
            )?;
            if let Err(e) = crate::keys::derived_ufvk_covers(self.network, &got.ufvk, expect) {
                return Err(EngineError::Message(format!(
                    "{e} (account index {})",
                    self.meta.account_index
                )));
            }
        }
        Ok(SecretVec::new(mnemonic.to_seed("").to_vec()))
    }

    pub fn has_seed(&self) -> bool {
        self.seed_store().exists()
    }

    pub fn is_view_only(&self) -> bool {
        self.meta.view_only || !self.has_seed()
    }

    pub fn unlock_policy(&self) -> UnlockPolicy {
        self.meta.unlock_policy
    }

    pub fn os_unlock(&self) -> bool {
        self.meta.os_unlock || self.seed_store().os_unlock_present()
    }

    pub fn viewing_key(&self) -> Result<String> {
        if let Some(u) = &self.meta.ufvk {
            return Ok(u.clone());
        }
        let db = self.open_db()?;
        let account_id = Self::primary_account_id(&db)?;
        let account = db
            .get_account(account_id)
            .map_err(|e| EngineError::WalletDb(format!("get_account: {e}")))?
            .ok_or(EngineError::NoAccount)?;
        let ufvk = account.ufvk().ok_or(EngineError::NoAccount)?;
        Ok(ufvk.encode(&self.network))
    }

    pub fn set_unlock_policy(&mut self, policy: UnlockPolicy) -> Result<()> {
        self.meta.unlock_policy = policy;
        self.save_meta()
    }

    /// Persist a matching mnemonic so a view-only wallet can spend later.
    /// Keep the wallet's policy; callers may hold auth from before a restore or
    /// policy change. Use `set_unlock_policy` to change how spending unlocks.
    pub fn attach_seed(&mut self, mnemonic: &str, auth: &SeedAuth) -> Result<()> {
        let words = mnemonic.trim();
        let check = SeedAuth {
            passphrase: auth.passphrase.clone(),
            windows_credential: auth.windows_credential,
            mnemonic: Some(words.to_string()),
            unlock_policy: self.meta.unlock_policy,
        };
        let _ = self.load_seed(&check)?;
        SeedStore::new(&self.paths.root).save(
            words,
            auth.passphrase.as_deref(),
            auth.windows_credential,
        )?;
        self.meta.view_only = false;
        self.meta.os_unlock = auth.windows_credential;
        self.save_meta()
    }

    /// Write UFVK into wallet.json if an older wallet is missing it.
    pub fn ensure_ufvk(&mut self) -> Result<()> {
        if self.meta.ufvk.is_some() {
            return Ok(());
        }
        let ufvk = self.viewing_key()?;
        self.meta.ufvk = Some(ufvk);
        self.save_meta()
    }

    fn primary_account_id(db: &Db) -> Result<AccountUuid> {
        db.get_account_ids()
            .map_err(|e| EngineError::WalletDb(format!("accounts: {e}")))?
            .into_iter()
            .next()
            .ok_or(EngineError::NoAccount)
    }

    pub async fn sync(&self) -> Result<(u32, SyncProgress)> {
        self.sync_reported_resilient(None).await
    }

    /// Resume a historical scan through a short light-server restart. Every
    /// attempt opens the wallet from its committed scan island, so a failed
    /// in-flight range is fetched again without discarding prior wallet data.
    /// Invalid chain data and database errors still fail immediately.
    pub async fn sync_reported_resilient(
        &self,
        live: Option<Arc<Mutex<SyncProgress>>>,
    ) -> Result<(u32, SyncProgress)> {
        self.sync_reported_resilient_cancellable(live, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// As above, but stop promptly when the owning desktop wallet session is
    /// closed or the user stops its scan. A dropped scan future leaves only
    /// fully committed SQLite batches; the next scan resumes at that island.
    pub async fn sync_reported_resilient_cancellable(
        &self,
        live: Option<Arc<Mutex<SyncProgress>>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<(u32, SyncProgress)> {
        // The CLI passes no live progress; keep one anyway so the retry loop
        // can tell an attempt that moved the scan from one that never connected.
        let live = live.or_else(|| Some(Arc::new(Mutex::new(SyncProgress::default()))));
        let progress = live.clone();
        let light_url = self.server.as_url();
        retry_transient_sync(
            live,
            Some(&light_url),
            Duration::from_secs(120),
            Duration::from_secs(2),
            &cancel,
            || self.sync_reported(progress.clone()),
        )
        .await
    }

    pub async fn sync_reported(
        &self,
        live: Option<Arc<Mutex<SyncProgress>>>,
    ) -> Result<(u32, SyncProgress)> {
        let mut progress = SyncProgress {
            stage: SyncStage::Connecting,
            percent: 0.0,
            message: "connecting".into(),
            ..Default::default()
        };
        Self::push_progress(&live, &progress);

        let mut db = self.open_db()?;
        let mut client = self.connect().await?;
        Self::assert_chain(&mut client, self.network).await?;

        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip out of range".into()))?;
        progress.tip_height = Some(u64::from(tip));
        let already_scanned =
            last_filled_island_end(&self.paths.data_db, self.meta.birthday_height, tip);
        Self::check_catch_up_gap(
            already_scanned,
            self.meta.birthday_height,
            tip,
            self.meta.allow_deep_sync,
        )?;
        let downloaded = Arc::new(AtomicU32::new(
            already_scanned.max(self.meta.birthday_height),
        ));
        let scanned = Arc::new(AtomicU32::new(already_scanned));
        let cache = FsBlockCache::open(&self.paths.block_cache)
            .map_err(|e| EngineError::WalletDb(format!("block cache: {e}")))?
            .with_progress(Arc::clone(&downloaded))
            .with_scan_progress(Arc::clone(&scanned));
        let light_url = self.server.as_url();

        progress.stage = SyncStage::Connecting;
        progress.eta_secs = None;
        progress.message = format!(
            "connected to {} — fetching merkle frontiers",
            crate::describe_light_url(&light_url)
        );
        progress.percent = 0.0;
        Self::push_progress(&live, &progress);
        info!("starting sync");

        let live_tick = live.clone();
        let birthday = self.meta.birthday_height;
        let already = already_scanned;
        let downloaded_tick = Arc::clone(&downloaded);
        let scanned_tick = Arc::clone(&scanned);
        let scan_started = std::time::Instant::now();
        let stop_tick = Arc::new(AtomicBool::new(false));
        let stop_tick_run = Arc::clone(&stop_tick);
        let light_tick = light_url.clone();
        let ticker = std::thread::Builder::new()
            .name("z-stack-sync-tick".into())
            .spawn(move || {
                let origin = already.max(birthday);
                let span = tip.saturating_sub(origin).max(1);
                let mut last_mark = origin;
                let mut last_move = scan_started;
                let mut last_probe = scan_started
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or(scan_started);
                let mut light_reachable = true;
                let mut rate_marks: VecDeque<(Instant, u32, u32)> = VecDeque::new();
                while !stop_tick_run.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(250));
                    let downloaded_h = downloaded_tick.load(Ordering::Relaxed).max(birthday);
                    let scanned_h = scanned_tick.load(Ordering::Relaxed).max(birthday);
                    let show_dl = downloaded_h.max(scanned_h);
                    let mark = show_dl.max(scanned_h);
                    if mark != last_mark {
                        last_mark = mark;
                        last_move = Instant::now();
                    }
                    let sc_done = scanned_h.saturating_sub(origin).min(span);
                    let dl_done = show_dl.saturating_sub(origin).min(span);
                    let pct =
                        crate::catch_up_percent_from(origin, scanned_h, show_dl, tip, birthday);
                    let now = Instant::now();
                    rate_marks.push_back((now, show_dl, scanned_h));
                    while rate_marks.len() > 1
                        && now.duration_since(rate_marks[0].0) > Duration::from_secs(2)
                    {
                        rate_marks.pop_front();
                    }
                    let (dl_bps, scan_bps) = match (rate_marks.front(), rate_marks.back()) {
                        (Some(a), Some(b)) if b.0 > a.0 => {
                            let dt = b.0.duration_since(a.0).as_secs_f32().max(0.25);
                            (
                                b.1.saturating_sub(a.1) as f32 / dt,
                                b.2.saturating_sub(a.2) as f32 / dt,
                            )
                        }
                        _ => {
                            let elapsed = scan_started.elapsed().as_secs_f32().max(0.4);
                            (dl_done as f32 / elapsed, sc_done as f32 / elapsed)
                        }
                    };
                    let left = tip.saturating_sub(scanned_h);
                    let scan_moving = sc_done > 1 && scan_bps > 1.0;
                    let eta_secs = crate::live_scan_eta_secs(
                        sc_done, left, origin, birthday, scan_bps,
                    );
                    let idle = show_dl <= origin && scanned_h <= origin;
                    let have_bytes = show_dl > origin;
                    let scan_moved = scanned_h > origin;
                    let downloading_ahead = show_dl > scanned_h;
                    let frozen = last_move.elapsed()
                        >= Duration::from_secs(u64::from(crate::SYNC_STALL_SECS))
                        && left > 0;
                    let quiet = span <= crate::NEAR_TIP_BLOCKS
                        || left <= crate::STALL_QUIET_REMAINING;
                    if frozen && !quiet && last_probe.elapsed() >= Duration::from_secs(5) {
                        last_probe = std::time::Instant::now();
                        light_reachable = light_tcp_reachable(&light_tick);
                        if !light_reachable {
                            warn!("light TCP probe failed during compact-block stall");
                        }
                    }
                    let scream =
                        crate::light_stall_warning(span, left, frozen, light_reachable);
                    if let Some(lock) = &live_tick {
                        if let Ok(mut g) = lock.lock() {
                            g.downloaded_height = Some(u64::from(show_dl));
                            g.scanned_height = Some(u64::from(scanned_h));
                            g.tip_height = Some(u64::from(tip));
                            g.eta_secs = eta_secs;
                            if matches!(g.stage, SyncStage::Enhancing | SyncStage::Synced) {
                                continue;
                            }
                            // Always write session percent — including Connecting / idle.
                            // Birthday catch-up is 0% until down or scan leaves origin.
                            g.percent = pct;
                            if g.stage == SyncStage::Connecting && !have_bytes {
                                // pipeline `note()` owns the frontiers copy until
                                // compact bytes exist. Once heights move, leave Connecting.
                                continue;
                            }
                            g.stage = crate::historic_overlay_stage(
                                scan_moved,
                                downloading_ahead,
                                left,
                                scream,
                            );
                            g.download_active = have_bytes && left > 0;
                            g.decrypt_active = (scan_moved || have_bytes) && left > 0;
                            g.persist_active = (scan_moved || have_bytes) && left > 0;
                            g.blocks_per_sec = if scan_bps >= 1.0 {
                                Some(scan_bps)
                            } else {
                                None
                            };
                            g.download_blocks_per_sec = if dl_bps >= 1.0 {
                                Some(dl_bps)
                            } else {
                                None
                            };
                            g.message = if scream {
                                format!(
                                    "compact-block fetch not moving at {show_dl}/{tip} — check Zaino / the light URL"
                                )
                            } else if left > 0 && left <= crate::QUIET_BEHIND_BLOCKS {
                                format!("{left} behind")
                            } else if frozen
                                && left > 0
                                && left <= crate::NEAR_TIP_BLOCKS
                            {
                                format!(
                                    "{left} behind · waiting {}s on this range",
                                    last_move.elapsed().as_secs().max(1)
                                )
                            } else if idle {
                                "starting compact-block download".into()
                            } else {
                                let mut msg = format!(
                                    "downloaded {show_dl} / {tip}  ·  scanned {scanned_h} / {tip}"
                                );
                                if span <= crate::NEAR_TIP_BLOCKS
                                    || left <= crate::NEAR_TIP_BLOCKS
                                {
                                    msg.push_str(&format!("  ·  {left} behind"));
                                }
                                if let Some(secs) = eta_secs {
                                    msg.push_str(&format!(
                                        "  ·  ~{} remaining",
                                        crate::fmt_secs(secs)
                                    ));
                                }
                                if dl_bps >= 1.0 {
                                    msg.push_str(&format!("  ·  {dl_bps:.0} down blk/s"));
                                }
                                if scan_moving {
                                    msg.push_str(&format!("  ·  {scan_bps:.0} scan blk/s"));
                                }
                                msg
                            };
                        }
                    }
                }
            })
            .map_err(|e| EngineError::Message(format!("sync ticker: {e}")))?;

        let ticker = SyncTickerGuard {
            stop: stop_tick,
            thread: Some(ticker),
        };
        let run = super::pipeline::run(
            &mut client,
            self.network,
            &cache,
            &mut db,
            &self.paths.data_db,
            self.meta.birthday_height,
            &light_url,
            &downloaded,
            &scanned,
            live.clone(),
            tip,
        )
        .await;
        drop(ticker);
        run?;

        let scanned_now =
            last_filled_island_end(&self.paths.data_db, self.meta.birthday_height, tip);
        if scanned_now < tip {
            return Err(EngineError::Message(format!(
                "sync incomplete: filled island {scanned_now} < tip {tip}"
            )));
        }

        progress.stage = SyncStage::Enhancing;
        progress.percent = 92.0;
        progress.message = "reading memos from mined transactions".into();
        Self::push_progress(&live, &progress);

        // Memos are best effort and retried next sync. A hung validator RPC
        // (120 s read timeout per lookup) must not hold the sync, and with it
        // Send, for many minutes.
        let enhanced = tokio::time::timeout(ENHANCE_BUDGET, self.enhance_memos(&mut db))
            .await
            .unwrap_or_else(|_| {
                warn!("memo enhancement ran out of time; the rest waits for the next sync");
                Ok(0)
            })
            .unwrap_or(0);
        info!("memo enhancement completed");
        if enhanced > 0 {
            info!("memo enhance stored decrypted outputs");
        }

        match tokio::time::timeout(REBROADCAST_BUDGET, self.rebroadcast_unmined(tip)).await {
            Ok(0) => {}
            Ok(_sent) => info!("saved unmined sends resubmitted"),
            Err(_) => warn!("rebroadcast ran out of time; the rest waits for the next sync"),
        }

        let scanned = last_filled_island_end(&self.paths.data_db, self.meta.birthday_height, tip);
        progress.stage = SyncStage::Synced;
        progress.percent = 100.0;
        progress.scanned_height = Some(u64::from(scanned));
        progress.tip_height = Some(u64::from(tip));
        progress.message = format!("synced to {scanned}");
        Self::push_progress(&live, &progress);
        info!("sync complete");
        Ok((scanned, progress))
    }

    fn push_progress(live: &Option<Arc<Mutex<SyncProgress>>>, progress: &SyncProgress) {
        if let Some(lock) = live {
            if let Ok(mut g) = lock.lock() {
                *g = progress.clone();
            }
        }
    }

    /// Answer the wallet's transaction data requests (bounded per sync).
    /// Enhancements store full data at the server-reported mined height;
    /// status requests and unknown transactions are reported back with
    /// `set_transaction_status`, so they leave the queue instead of blocking it.
    async fn enhance_memos(&self, db: &mut Db) -> Result<u32> {
        self.require_network_access()?;
        const MAX: usize = 24;
        let reqs = db
            .transaction_enhancement_work()
            .map_err(|e| EngineError::WalletDb(format!("tx data requests: {e}")))?;
        let height = db
            .chain_height()
            .map_err(|e| EngineError::WalletDb(format!("chain_height: {e}")))?
            .unwrap_or(BlockHeight::from_u32(1));
        // An Enhancement subsumes a GetStatus for the same transaction. Memo
        // work goes first so status polling cannot crowd it out.
        let mut wanted = std::collections::BTreeMap::<TxId, bool>::new();
        for req in reqs {
            if let TransactionEnhancementWork::Public(request) = req {
                wanted.insert(request.txid(), true);
            }
        }
        for req in db
            .transaction_status_work()
            .map_err(|e| EngineError::WalletDb(format!("tx status requests: {e}")))?
        {
            if let TransactionStatusWork::Public(request) = req {
                wanted.entry(request.txid()).or_insert(false);
            }
        }
        let mut wanted: Vec<(TxId, bool)> = wanted.into_iter().collect();
        wanted.sort_by_key(|(_, full)| !full);
        wanted.truncate(MAX);
        let mut n = 0u32;
        if wanted.is_empty() {
            return Ok(0);
        }
        // One connection for every lookup in this pass; clones share it.
        let skip = matches!(self.server, LightServer::LocalRegtest)
            || self.server.as_url().contains(":28137");
        let client = Self::connect_url(&self.server.as_url(), skip).await;
        for chunk in wanted.chunks(4) {
            let ids: Vec<TxId> = chunk.iter().map(|(id, _)| *id).collect();
            let lookups = self.lookup_transactions(client.as_ref().ok(), &ids).await;
            info!("memo transaction RPC batch completed");
            for (&(txid, full), lookup) in chunk.iter().zip(lookups) {
                let (raw, mined_at) = match lookup {
                    TxLookup::Found(raw, mined_at) => (raw, mined_at),
                    TxLookup::Unknown => {
                        if full {
                            // A completed payload lookup, independent of status scheduling.
                            if db.notify_transaction_enhancement_not_found(txid).is_err() {
                                tracing::debug!("transaction enhancement not recorded");
                            }
                        }
                        if let Err(_e) =
                            db.set_transaction_status(txid, TransactionStatus::TxidNotRecognized)
                        {
                            tracing::debug!("transaction status not recorded");
                        }
                        continue;
                    }
                    TxLookup::Unavailable(_e) => {
                        tracing::debug!("transaction lookup deferred");
                        continue;
                    }
                };
                if !full {
                    let status = mined_at
                        .map_or(TransactionStatus::NotInMainChain, TransactionStatus::Mined);
                    if let Err(_e) = db.set_transaction_status(txid, status) {
                        tracing::debug!("transaction status not recorded");
                    }
                    continue;
                }
                let mined = match mined_at {
                    Some(h) => Some(h),
                    None => db
                        .get_tx_height(txid)
                        .map_err(|e| EngineError::WalletDb(format!("memo tx height: {e}")))?,
                };
                let decode_height = mined
                    .unwrap_or_else(|| BlockHeight::from_u32(u32::from(height).saturating_add(1)));
                let branch = BranchId::for_height(&self.network, decode_height);
                let mut remaining = raw.as_slice();
                let tx = match Transaction::read(&mut remaining, branch) {
                    Ok(tx) if remaining.is_empty() && tx.txid() == txid => tx,
                    Ok(_) => continue,
                    Err(_) => continue,
                };
                match decrypt_and_store_transaction(&self.network, db, &tx, mined) {
                    Ok(()) => n = n.saturating_add(1),
                    Err(_e) => {
                        tracing::debug!("memo enhance skipped");
                    }
                }
            }
        }
        Ok(n)
    }

    /// Up to four lookups at once.
    async fn lookup_transactions(&self, client: Option<&LwdClient>, ids: &[TxId]) -> Vec<TxLookup> {
        let lookup = |i: usize| async move {
            match ids.get(i) {
                Some(id) => Some(self.lookup_transaction(client.cloned(), *id).await),
                None => None,
            }
        };
        let (a, b, c, d) = tokio::join!(lookup(0), lookup(1), lookup(2), lookup(3));
        [a, b, c, d].into_iter().flatten().collect()
    }

    /// A validator RPC answers with the mined height in one call; the light
    /// server's GetTransaction does too and covers wallets without one.
    async fn lookup_transaction(&self, client: Option<LwdClient>, txid: TxId) -> TxLookup {
        if !self.network_access {
            return TxLookup::Unavailable(EngineError::Message("native_offline_wallet".into()));
        }
        let mut failure = None;
        if let Some(rpc) = self
            .validator_rpc_url()
            .or_else(|| self.server.zebra_rpc_url())
        {
            let id = txid.to_string();
            let fetched = tokio::task::spawn_blocking(move || {
                super::rpc::get_raw_transaction_verbose(&rpc, &id)
            })
            .await;
            match fetched {
                Ok(Ok((hex, mined))) => match crate::web::from_hex(&hex) {
                    Ok(raw) => return TxLookup::Found(raw, mined.map(BlockHeight::from_u32)),
                    Err(e) => failure = Some(EngineError::Message(e)),
                },
                Ok(Err(e)) if super::rpc::is_unknown_transaction(&e) => return TxLookup::Unknown,
                Ok(Err(e)) => failure = Some(e),
                Err(_) => {}
            }
        }
        let Some(mut client) = client else {
            return TxLookup::Unavailable(
                failure
                    .unwrap_or_else(|| EngineError::Transport("light server unreachable".into())),
            );
        };
        // Wire order first; a legacy server may expect it reversed.
        let mut hash = txid.as_ref().to_vec();
        let mut unknown = 0;
        for _ in 0..2 {
            let filter = TxFilter {
                block: None,
                index: 0,
                hash: hash.clone(),
            };
            match client.get_transaction(filter).await {
                Ok(resp) => {
                    let raw = resp.into_inner();
                    if !raw.data.is_empty() {
                        // Zaino and lightwalletd report 0 for mempool.
                        let mined = u32::try_from(raw.height)
                            .ok()
                            .filter(|h| *h > 0)
                            .map(BlockHeight::from_u32);
                        return TxLookup::Found(raw.data, mined);
                    }
                }
                Err(status) if unknown_transaction(&status) => unknown += 1,
                Err(status) => {
                    failure = Some(EngineError::Transport(format!("GetTransaction: {status}")));
                }
            }
            hash.reverse();
        }
        if unknown == 2 {
            return TxLookup::Unknown;
        }
        TxLookup::Unavailable(
            failure.unwrap_or_else(|| EngineError::Message("GetTransaction empty".into())),
        )
    }

    /// Rotate to the next diversified unified address (preferred for privacy).
    pub fn next_unified_address(&self) -> Result<String> {
        let mut db = self.open_db()?;
        let account = Self::primary_account_id(&db)?;
        let (addr, _idx) = db
            .get_next_available_address(account, UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| EngineError::WalletDb(format!("next address: {e}")))?
            .ok_or(EngineError::NoAccount)?;
        Ok(addr.encode(&self.network))
    }

    /// Current (last generated) unified address, or UFVK default.
    pub fn unified_address(&self) -> Result<String> {
        let db = self.open_db()?;
        let account_id = Self::primary_account_id(&db)?;
        if let Some(addr) = db
            .get_last_generated_address_matching(
                account_id,
                UnifiedAddressRequest::AllAvailableKeys,
            )
            .map_err(|e| EngineError::WalletDb(format!("get address: {e}")))?
        {
            return Ok(addr.encode(&self.network));
        }
        let account = db
            .get_account(account_id)
            .map_err(|e| EngineError::WalletDb(format!("get_account: {e}")))?
            .ok_or(EngineError::NoAccount)?;
        let ufvk = account.ufvk().ok_or(EngineError::NoAccount)?;
        Ok(ufvk
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| EngineError::WalletDb(format!("default address: {e:?}")))?
            .0
            .encode(&self.network))
    }

    /// Transparent P2PKH receiver of the current unified address (for faucet/mining).
    pub fn transparent_address(&self) -> Result<Option<String>> {
        crate::keys::transparent_from_unified(&self.unified_address()?, self.network)
    }

    pub fn balance(&self) -> Result<Balance> {
        let db = self.open_db()?;
        let summary = db
            .get_wallet_summary(crate::confirmations_policy(self.network))
            .map_err(|e| EngineError::WalletDb(format!("summary: {e}")))?
            .ok_or(EngineError::SyncRequired)?;

        let mut sapling = 0u64;
        let mut orchard = 0u64;
        let mut ironwood = 0u64;
        let mut transparent = 0u64;
        let mut sapling_p = 0u64;
        let mut orchard_p = 0u64;
        let mut ironwood_p = 0u64;
        let mut transparent_p = 0u64;
        for (_id, account_bal) in summary.account_balances() {
            let s = account_bal.sapling_balance();
            sapling = sapling.saturating_add(u64::from(s.spendable_value()));
            sapling_p = sapling_p
                .saturating_add(u64::from(s.change_pending_confirmation()))
                .saturating_add(u64::from(s.value_pending_spendability()));
            let o = account_bal.orchard_balance();
            orchard = orchard.saturating_add(u64::from(o.spendable_value()));
            orchard_p = orchard_p
                .saturating_add(u64::from(o.change_pending_confirmation()))
                .saturating_add(u64::from(o.value_pending_spendability()));
            let i = account_bal.ironwood_balance();
            ironwood = ironwood.saturating_add(u64::from(i.spendable_value()));
            ironwood_p = ironwood_p
                .saturating_add(u64::from(i.change_pending_confirmation()))
                .saturating_add(u64::from(i.value_pending_spendability()));
            #[cfg(feature = "transparent-inputs")]
            {
                let t = account_bal.unshielded_balance();
                transparent = transparent.saturating_add(u64::from(t.spendable_value()));
                transparent_p = transparent_p
                    .saturating_add(u64::from(t.change_pending_confirmation()))
                    .saturating_add(u64::from(t.value_pending_spendability()));
            }
        }
        Ok(Balance {
            sapling_available: sapling,
            orchard_available: orchard,
            ironwood_available: ironwood,
            transparent_available: transparent,
            total_available: sapling
                .saturating_add(orchard)
                .saturating_add(ironwood)
                .saturating_add(transparent),
            sapling_pending: sapling_p,
            orchard_pending: orchard_p,
            ironwood_pending: ironwood_p,
            transparent_pending: transparent_p,
            total_pending: sapling_p
                .saturating_add(orchard_p)
                .saturating_add(ironwood_p)
                .saturating_add(transparent_p),
        })
    }

    /// Rewind scan_queue to the last filled island so the next `sync` trial-decrypts
    /// island_end+1→tip. Does not wipe keys or the already-scanned island.
    pub async fn rewind_scan_to_gap(&self) -> Result<u32> {
        self.require_network_access()?;
        let mut client = Self::connect_url(
            &self.server.as_url(),
            Self::skip_ironwood_subtrees(self.network, &self.server.as_url()),
        )
        .await?;
        Self::assert_chain(&mut client, self.network).await?;
        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip height out of range".into()))?;
        self.rewind_scan_to_gap_at_tip(tip)
    }

    fn rewind_scan_to_gap_at_tip(&self, tip: u32) -> Result<u32> {
        let island = last_filled_island_end(&self.paths.data_db, self.meta.birthday_height, tip);
        if island >= tip {
            return Err(EngineError::Message(
                "wallet scan already reaches the chain tip; no gap to rewind".into(),
            ));
        }
        let target = rewind_target(&self.paths.data_db, island)?.ok_or_else(|| {
            EngineError::WalletDb(format!(
                "scan gap begins after {island}, but no persisted block exists at or below it; existing wallet data was retained"
            ))
        })?;
        let mut db = self.open_db()?;
        db.truncate_to_height(BlockHeight::from_u32(target))
            .map(u32::from)
            .map_err(|e| EngineError::WalletDb(format!("rewind scan gap at {target}: {e}")))
    }

    /// Wipe sqlite + compact-block cache and re-import the UFVK at birthday.
    /// Seed / keyring / wallet.json stay. Next `sync` is a full rescan.
    pub async fn reset_scan(&self) -> Result<()> {
        self.require_network_access()?;
        let ufvk_str = match self
            .meta
            .ufvk
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(u) => u.to_string(),
            None => self.viewing_key()?,
        };
        let ufvk = UnifiedFullViewingKey::decode(&self.network, ufvk_str.trim())
            .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;

        let mut client = Self::connect_url(
            &self.server.as_url(),
            Self::skip_ironwood_subtrees(self.network, &self.server.as_url()),
        )
        .await?;
        Self::assert_chain(&mut client, self.network).await?;
        let tip: u32 = client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip height out of range".into()))?;
        let birthday_height = self.meta.birthday_height.max(1);
        if birthday_height > tip {
            return Err(EngineError::BirthdayAboveTip {
                birthday: birthday_height,
                tip,
            });
        }
        let prior_height = birthday_height.saturating_sub(1);
        let treestate = client
            .get_tree_state(BlockId {
                height: u64::from(prior_height),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetTreeState: {e}")))?
            .into_inner();
        let birthday = AccountBirthday::from_treestate(treestate, Some(BlockHeight::from(tip)))
            .map_err(|e| EngineError::WalletDb(format!("birthday: {e}")))?;

        self.replace_scan_db(&ufvk, &birthday)
    }

    fn replace_scan_db(
        &self,
        ufvk: &UnifiedFullViewingKey,
        birthday: &AccountBirthday,
    ) -> Result<()> {
        let db_path = &self.paths.data_db;
        let _lease = database_lease::exclusive(db_path)?;
        payments::require_rescan_safe(db_path)?;
        Self::checkpoint_sqlite(db_path)?;
        // Build the replacement beside the live database and swap it in with
        // one rename. A kill at any point leaves the old wallet or the new one
        // at `data.sqlite`. Moving the old file away first left a window where
        // the folder looked empty and the app offered Create over the seed.
        let fresh = db_path.with_file_name("data.sqlite.new");
        let _ = std::fs::remove_file(&fresh);
        Self::remove_sqlite_sidecars(&fresh);
        let built = (|| -> Result<()> {
            let mut db = open_wallet_db(&fresh, self.network)?;
            init_wallet_db(&mut db, None)
                .map_err(|e| EngineError::WalletDb(format!("init: {e}")))?;
            db.import_account_ufvk(
                "primary",
                ufvk,
                birthday,
                AccountPurpose::Spending { derivation: None },
                Some("z-stack-reset"),
            )
            .map_err(|e| EngineError::WalletDb(format!("import ufvk: {e}")))?;
            drop(db);
            // Everything in the main file, so the rename moves the whole DB.
            Self::checkpoint_sqlite(&fresh)?;
            Self::remove_sqlite_sidecars(&fresh);
            if let Err(e) = FsBlockCache::wipe(&self.paths.block_cache) {
                return Err(EngineError::WalletDb(format!("wipe block cache: {e}")));
            }
            Ok(())
        })();
        if let Err(e) = built {
            let _ = std::fs::remove_file(&fresh);
            Self::remove_sqlite_sidecars(&fresh);
            return Err(EngineError::WalletDb(format!(
                "could not build the rescan database; existing data was retained: {e}"
            )));
        }
        // The old WAL was checkpointed empty above; its sidecars must not be
        // read as the new database's.
        Self::remove_sqlite_sidecars(db_path);
        if let Err(e) = std::fs::rename(&fresh, db_path) {
            let _ = std::fs::remove_file(&fresh);
            // Includes Windows file locks held by another wallet instance.
            return Err(EngineError::WalletDb(format!(
                "could not replace wallet database for rescan; existing data was retained. Close other wallet instances and retry: {e}"
            )));
        }
        let _ = std::fs::remove_file(self.paths.reset_backup());
        Ok(())
    }

    fn checkpoint_sqlite(db_path: &Path) -> Result<()> {
        if !db_path.exists() {
            return Ok(());
        }
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| EngineError::WalletDb(format!("checkpoint open: {e}")))?;
        let (busy, frames, checkpointed): (i32, i32, i32) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| EngineError::WalletDb(format!("wal checkpoint before reset: {e}")))?;
        // SQLITE_BUSY is returned in the pragma's result row, not as a SQL
        // error. Replacing the DB and deleting its WAL here could discard the
        // only committed copy of records missing from the backup's main file.
        if busy != 0 || frames != checkpointed {
            return Err(EngineError::WalletDb(format!(
                "wallet database is in use; checkpoint before reset is incomplete \
                 ({checkpointed}/{frames} frames). Existing data was retained. \
                 Close other wallet instances and retry"
            )));
        }
        Ok(())
    }

    fn remove_sqlite_sidecars(db_path: &Path) {
        for ext in ["sqlite-wal", "sqlite-shm", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(db_path.with_extension(ext));
        }
        if let Some(name) = db_path.file_name() {
            let stem = name.to_string_lossy();
            let _ = std::fs::remove_file(db_path.with_file_name(format!("{stem}-wal")));
            let _ = std::fs::remove_file(db_path.with_file_name(format!("{stem}-shm")));
        }
    }

    /// Height of the last filled contiguous scan island (not the scan-queue tip).
    pub fn scanned_height(&self) -> Result<u32> {
        let tip = rusqlite::Connection::open(&self.paths.data_db)
            .ok()
            .and_then(|conn| {
                let _ = conn.pragma_update(None, "query_only", true);
                conn.query_row("SELECT COALESCE(MAX(height), 0) FROM blocks", [], |r| {
                    r.get::<_, u32>(0)
                })
                .ok()
            })
            .unwrap_or(0);
        Ok(last_filled_island_end(
            &self.paths.data_db,
            self.meta.birthday_height,
            tip,
        ))
    }

    /// Recent transactions from `v_transactions` (newest first).
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.query_history(limit, None, None)
    }

    /// One transaction by txid hex, or `None` when the wallet has no such row.
    pub fn transaction(&self, txid: &str) -> Result<Option<HistoryEntry>> {
        let txid = txid.trim();
        if txid.is_empty() {
            return Err(EngineError::Message("missing txid".into()));
        }
        Ok(self.query_history(1, None, Some(txid))?.into_iter().next())
    }

    /// `status` filters before `LIMIT`, so a pending row is not pushed out by mined history.
    pub fn query_history(
        &self,
        limit: usize,
        status: Option<crate::HistoryStatusFilter>,
        txid: Option<&str>,
    ) -> Result<Vec<HistoryEntry>> {
        // Public IDs use the canonical node/explorer order; SQLite stores the
        // wire bytes. Compare the BLOB directly so its index remains usable.
        let tx_filter = txid
            .map(|id| {
                TxId::from_hex(id.trim()).ok_or_else(|| {
                    EngineError::Message("txid must be 64 hexadecimal digits".into())
                })
            })
            .transpose()?;
        let scanned = self.scanned_height().unwrap_or(0);
        let conn = rusqlite::Connection::open(&self.paths.data_db)
            .map_err(|e| EngineError::WalletDb(format!("history open: {e}")))?;
        let _ = conn.pragma_update(None, "query_only", true);
        let mut sql = String::from(
            "SELECT txid, mined_height, expiry_height, account_balance_delta,
                    total_spent, total_received, fee_paid, spent_note_count,
                    has_change, sent_note_count, received_note_count, memo_count,
                    expired_unmined, is_shielding
             FROM v_transactions WHERE 1=1",
        );
        if txid.is_some() {
            sql.push_str(" AND txid = :txid");
        }
        match status {
            Some(crate::HistoryStatusFilter::Mined) => {
                sql.push_str(" AND mined_height IS NOT NULL");
            }
            Some(crate::HistoryStatusFilter::Expired) => {
                sql.push_str(" AND mined_height IS NULL AND expired_unmined != 0");
            }
            Some(crate::HistoryStatusFilter::Pending) => {
                sql.push_str(" AND mined_height IS NULL AND IFNULL(expired_unmined, 0) = 0");
            }
            None => {}
        }
        sql.push_str(" ORDER BY mined_height IS NULL, mined_height DESC, tx_index DESC LIMIT :lim");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| EngineError::WalletDb(format!("history prepare: {e}")))?;
        let lim = if txid.is_some() {
            1i64
        } else {
            limit.min(500) as i64
        };
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(Vec<u8>, HistoryEntry)> {
            let txid: Vec<u8> = row.get(0)?;
            let txid_bytes: [u8; 32] = txid.as_slice().try_into().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    Box::new(e),
                )
            })?;
            let mined_height: Option<u32> = row.get(1)?;
            Ok((
                txid.clone(),
                HistoryEntry {
                    txid: TxId::from_bytes(txid_bytes).to_string(),
                    mined_height,
                    expiry_height: row.get::<_, Option<u32>>(2)?,
                    account_delta_zat: row.get(3)?,
                    spent_zat: row.get::<_, i64>(4)?.max(0) as u64,
                    received_zat: row.get::<_, i64>(5)?.max(0) as u64,
                    fee_zat: row.get::<_, Option<i64>>(6)?.map(|v| v.max(0) as u64),
                    sent_note_count: row.get::<_, i64>(9)?.max(0) as u32,
                    received_note_count: row.get::<_, i64>(10)?.max(0) as u32,
                    memo_count: row.get::<_, i64>(11)?.max(0) as u32,
                    has_change: row.get(8)?,
                    is_shielding: row.get(13)?,
                    expired_unmined: row.get(12)?,
                    memos: Vec::new(),
                    block_time: None,
                    confirmations: mined_height
                        .map(|h| scanned.saturating_sub(h).saturating_add(1)),
                    ..Default::default()
                },
            ))
        };
        let rows = if let Some(txid) = tx_filter {
            stmt.query_map(
                rusqlite::named_params! { ":txid": txid.as_ref().as_slice(), ":lim": lim },
                map_row,
            )
        } else {
            stmt.query_map(rusqlite::named_params! { ":lim": lim }, map_row)
        }
        .map_err(|e| EngineError::WalletDb(format!("history query: {e}")))?;
        let mut pending = Vec::new();
        for r in rows {
            pending.push(r.map_err(|e| EngineError::WalletDb(format!("history row: {e}")))?);
        }
        drop(stmt);
        let mut out = Vec::with_capacity(pending.len());
        for (txid, mut e) in pending {
            e.memos = memos_for_txid(&conn, &txid);
            out.push(e);
        }
        Ok(out)
    }

    /// Sync if the wallet is behind the light server tip. Returns whether work ran.
    pub async fn catch_up(&self) -> Result<(u32, SyncProgress, bool)> {
        let scanned = self.scanned_height()?;
        let tip = Self::fetch_tip(&self.server).await?;
        if scanned >= tip {
            // Older sparse scans could persist notes/rows without a usable
            // checkpoint. A caught-up wallet must recover those anchors too.
            self.recover_spend_checkpoints(scanned).await?;
            // The web bridge syncs through here: resubmit unmined sends even
            // when no block arrived.
            let _ = tokio::time::timeout(REBROADCAST_BUDGET, self.rebroadcast_unmined(tip)).await;
            return Ok((
                scanned,
                SyncProgress {
                    stage: SyncStage::Synced,
                    percent: 100.0,
                    message: format!("already at {scanned}"),
                    tip_height: Some(u64::from(tip)),
                    scanned_height: Some(u64::from(scanned)),
                    ..Default::default()
                },
                false,
            ));
        }
        let (h, p) = self.sync().await?;
        Ok((h, p, true))
    }

    async fn recover_spend_checkpoints(&self, scanned: u32) -> Result<()> {
        let confirmations = crate::confirmations_policy(self.network).trusted();
        let expected = scanned.saturating_sub(confirmations.get() - 1);
        // A new wallet cannot have a spendable receipt before its birthday.
        if scanned < self.meta.birthday_height || expected < self.meta.birthday_height {
            return Ok(());
        }
        let mut db = self.open_db()?;
        if super::selective_scan::repair_persisted_checkpoints(
            &mut db,
            &self.paths.data_db,
            scanned,
            expected,
        )? {
            return Ok(());
        }
        // Legacy sparse scans may have only a tip watermark. Fetch only the
        // policy anchor through the scanned tip (three blocks on mainnet), and
        // validate linkage and the durable tip before publishing metadata.
        drop(db);
        let mut client = self.connect().await?;
        let blocks = super::pipeline::fetch_height_range(&mut client, expected, scanned).await?;
        let missing = {
            let mut db = self.open_db()?;
            super::selective_scan::repair_checkpoints_from_compacts(&mut db, scanned, &blocks, &[])?
        };
        if !missing.is_empty() {
            // Older builds may have discarded a required prefix's interior
            // nodes. Obtain authentic frontiers only for those missing sizes;
            // never hold a SQLite connection/transaction over network I/O.
            let mut frontiers = Vec::with_capacity(missing.len());
            for height in missing {
                let frontier = super::pipeline::fetch_tree_state(&mut client, height).await?;
                if u32::from(frontier.block_height()) != height {
                    return Err(EngineError::Transport(
                        "checkpoint recovery returned a different tree-state height".into(),
                    ));
                }
                frontiers.push(frontier);
            }
            let mut db = self.open_db()?;
            let remaining = super::selective_scan::repair_checkpoints_from_compacts(
                &mut db, scanned, &blocks, &frontiers,
            )?;
            if !remaining.is_empty() {
                return Err(EngineError::WalletDb(
                    "verified frontier recovery left incomplete anchor coverage".into(),
                ));
            }
        }
        let mut db = self.open_db()?;
        if !super::selective_scan::repair_persisted_checkpoints(
            &mut db,
            &self.paths.data_db,
            scanned,
            expected,
        )? {
            return Err(EngineError::WalletDb(
                "verified checkpoint recovery did not produce the required spend anchor".into(),
            ));
        }
        Ok(())
    }

    #[cfg(feature = "transparent-inputs")]
    pub async fn shield(&self, auth: &SeedAuth, threshold_zat: u64) -> Result<Vec<String>> {
        self.require_network_access()?;
        self.ensure_recovery_selection_ready()?;
        let mut db = self.open_db()?;
        let account = Self::primary_account_id(&db)?;
        let seed = self.load_seed(auth)?;
        let usk = UnifiedSpendingKey::from_seed(
            &self.network,
            seed.expose_secret(),
            self.account_index(),
        )
        .map_err(|e| EngineError::WalletDb(format!("USK: {e:?}")))?;

        let from_addrs: Vec<_> = db
            .get_transparent_receivers(account, true, true)
            .map_err(|e| EngineError::WalletDb(format!("receivers: {e}")))?
            .into_keys()
            .collect();
        if from_addrs.is_empty() {
            return Err(EngineError::Message("no transparent receivers".into()));
        }

        let rpc = self.validator_rpc_url();
        let classified = super::transparent_funding::classify_recent_funding(
            &mut db,
            &self.paths.data_db,
            self.network,
            &from_addrs,
            shield_confirmations(self.network),
            |txid| {
                let txid = txid.to_string();
                let rpc = rpc.as_deref();
                async move { Self::fetch_raw_transaction_with_rpc(&self.server, rpc, &txid).await }
            },
        )
        .await?;
        if classified > 0 {
            info!("classified recent transparent funding before shielding");
        }

        let input_selector = GreedyInputSelector::new();
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::default(),
        );
        let threshold = Zatoshis::from_u64(threshold_zat.max(1))
            .map_err(|_| EngineError::Message("bad threshold".into()))?;

        let proposal = propose_shielding::<_, _, _, _, Infallible>(
            &mut db,
            &self.network,
            &input_selector,
            &change_strategy,
            threshold,
            &from_addrs,
            account,
            shield_confirmations(self.network),
            CoinbaseFilter::AllTransparentOutputs,
            None,
        )
        .map_err(|e| map_funds_err(format!("propose_shielding: {e}")))?;

        let prover = crate::params::local_tx_prover()?;
        let txids = db
            .transactionally_with_extension(|wdb, ext| -> anyhow::Result<_> {
                recovery_guard::transaction_recovery_ready(wdb, ext)?;
                Ok(create_proposed_transactions::<
                    _,
                    _,
                    Infallible,
                    _,
                    Infallible,
                    _,
                >(
                    wdb,
                    &self.network,
                    &*prover,
                    &*prover,
                    &SpendingKeys::from_unified_spending_key(usk),
                    OvkPolicy::Sender,
                    &proposal,
                    None,
                )?)
            })
            .map_err(|e| map_funds_err(format!("create shielding tx: {e}")))?;

        let out = self.broadcast_all(&mut db, txids.iter().copied()).await;
        info!("shield timings");
        out
    }

    /// After sync: auto-shield t→Orchard, then migrate Sapling→Orchard when funded.
    pub async fn maintain(&self, auth: &SeedAuth) -> Result<Vec<String>> {
        self.require_network_access()?;
        let mut out = Vec::new();
        let bal = self.balance()?;
        if bal.transparent_available >= crate::SHIELD_THRESHOLD_ZAT {
            match self.shield(auth, crate::SHIELD_THRESHOLD_ZAT).await {
                Ok(t) => out.extend(t),
                Err(e) if skip_maintain_err(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let bal = self.balance()?;
        if bal.sapling_available >= crate::SHIELD_THRESHOLD_ZAT {
            let ua = self.unified_address()?;
            let sapling_only = SpendPolicy::shielded_pools([ShieldedPool::Sapling]);
            // A Sapling spend bundle pads to two outputs and the destination
            // bundle to two actions, so the fee is at least 20,000 zatoshis.
            // A flat fee pad made every migration fail as underfunded.
            let amt = match self.max_send_amount(&ua, bal.sapling_available, sapling_only.clone()) {
                Ok((amt, _fee)) => amt,
                Err(e) if skip_maintain_err(&e) => 0,
                Err(e) => return Err(e),
            };
            if amt > 0 {
                match self
                    .send_with_policy(auth, &ua, amt, None, sapling_only)
                    .await
                {
                    Ok(t) => out.extend(t),
                    Err(e) if skip_maintain_err(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(out)
    }

    /// ZIP-317 fee for a shielded send / ZIP-321 URI. Propose only — no prove or broadcast.
    pub fn estimate_fee(&self, to: &str, amount_zatoshis: u64, memo: Option<&str>) -> Result<u64> {
        let request = self.send_request(to, amount_zatoshis, memo)?;
        let mut db = self.open_db()?;
        self.propose_fee(&mut db, request, shielded_spend_policy())
    }

    /// Largest shielded amount that `propose_transfer` accepts for `to` (own UA if omitted).
    pub fn max_send(&self, to: Option<&str>) -> Result<(u64, u64)> {
        let dest = match to.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) if s.to_ascii_lowercase().starts_with("zcash:") => {
                let req = crate::parse_zip321(s).map_err(EngineError::Message)?;
                let addr = req.address().to_string();
                if addr.is_empty() {
                    return Err(EngineError::Message("ZIP-321 URI has no address".into()));
                }
                crate::keys::assert_shielded_send_dest(
                    &addr,
                    crate::keys::SendDestPolicy::Shielded,
                )?;
                addr
            }
            Some(s) => {
                crate::keys::assert_shielded_send_dest(s, crate::keys::SendDestPolicy::Shielded)?;
                s.to_string()
            }
            None => self.unified_address()?,
        };
        let bal = self.balance()?;
        let available = bal
            .sapling_available
            .saturating_add(bal.orchard_available)
            .saturating_add(bal.ironwood_available);
        self.max_send_amount(&dest, available, shielded_spend_policy())
    }

    fn send_request(
        &self,
        to: &str,
        amount_zatoshis: u64,
        memo: Option<&str>,
    ) -> Result<TransactionRequest> {
        if to.trim().to_ascii_lowercase().starts_with("zcash:") {
            let mut req = crate::parse_zip321(to).map_err(EngineError::Message)?;
            if req.payments.len() == 1 {
                if req.payments[0].amount_zec.is_none() && amount_zatoshis > 0 {
                    req.payments[0].amount_zec = Some(crate::format_zatoshis(amount_zatoshis));
                }
                if req.payments[0].memo.is_none() {
                    if let Some(m) = memo.map(str::trim).filter(|s| !s.is_empty()) {
                        req.payments[0].memo = Some(m.to_string());
                    }
                }
            }
            return self.zip321_request(&req.payments);
        }
        crate::keys::assert_shielded_send_dest(to, crate::keys::SendDestPolicy::Shielded)?;
        let payment = zip_payment(to, amount_zatoshis, memo)?;
        TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))
    }

    fn zip321_request(&self, payments: &[crate::Zip321Payment]) -> Result<TransactionRequest> {
        if payments.is_empty() {
            return Err(EngineError::Message("ZIP-321 URI has no payments".into()));
        }
        let mut built = Vec::with_capacity(payments.len());
        for p in payments {
            crate::keys::assert_shielded_send_dest(
                &p.address,
                crate::keys::SendDestPolicy::Shielded,
            )?;
            let zat = match p.amount_zec.as_deref() {
                Some(s) => parse_zec_to_zatoshis(s).map_err(EngineError::Message)?,
                None => 0,
            };
            if zat == 0
                && p.memo
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                return Err(EngineError::Message(
                    "zero-value send needs a memo (encrypted note)".into(),
                ));
            }
            built.push(zip_payment(&p.address, zat, p.memo.as_deref())?);
        }
        TransactionRequest::new(built).map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))
    }

    fn propose_fee(
        &self,
        db: &mut Db,
        request: TransactionRequest,
        policy: SpendPolicy,
    ) -> Result<u64> {
        let account = Self::primary_account_id(db)?;
        let input_selector = GreedyInputSelector::new();
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::default(),
        );
        let proposal = propose_transfer::<_, _, _, _, Infallible>(
            db,
            &self.network,
            account,
            &input_selector,
            &change_strategy,
            request,
            crate::confirmations_policy(self.network),
            &policy,
            None,
            None,
        )
        .map_err(|e| map_funds_err(format!("propose_transfer: {e}")))?;
        Ok(crate::proposal_fee_zat(&proposal))
    }

    fn propose_amount(
        &self,
        db: &mut Db,
        dest: &str,
        amount_zat: u64,
        policy: SpendPolicy,
    ) -> Result<u64> {
        let payment = zip_payment(dest, amount_zat, None)?;
        let request = TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?;
        self.propose_fee(db, request, policy)
    }

    fn max_send_amount(
        &self,
        dest: &str,
        available: u64,
        policy: SpendPolicy,
    ) -> Result<(u64, u64)> {
        if available == 0 {
            return Ok((0, 0));
        }
        let mut db = self.open_db()?;
        let mut pad = crate::FEE_PAD_ZAT;
        for _ in 0..8 {
            let amt = available.saturating_sub(pad);
            if amt == 0 {
                return Ok((0, 0));
            }
            match self.propose_amount(&mut db, dest, amt, policy.clone()) {
                Ok(fee) => {
                    let exact = available.saturating_sub(fee);
                    if exact > 0 && exact != amt {
                        if let Ok(fee2) = self.propose_amount(&mut db, dest, exact, policy.clone())
                        {
                            return Ok((exact, fee2));
                        }
                    }
                    return Ok((amt, fee));
                }
                Err(EngineError::InsufficientFunds) => {
                    pad = pad.saturating_mul(2).max(pad.saturating_add(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok((0, 0))
    }

    pub async fn send(
        &self,
        auth: &SeedAuth,
        to: &str,
        amount_zatoshis: u64,
        memo: Option<&str>,
    ) -> Result<Vec<String>> {
        if to.trim().to_ascii_lowercase().starts_with("zcash:") {
            let mut req = crate::parse_zip321(to).map_err(EngineError::Message)?;
            if req.payments.len() == 1 {
                if req.payments[0].amount_zec.is_none() && amount_zatoshis > 0 {
                    req.payments[0].amount_zec = Some(crate::format_zatoshis(amount_zatoshis));
                }
                if req.payments[0].memo.is_none() {
                    if let Some(m) = memo.map(str::trim).filter(|s| !s.is_empty()) {
                        req.payments[0].memo = Some(m.to_string());
                    }
                }
            }
            return self
                .send_payments(auth, &req.payments, shielded_spend_policy())
                .await;
        }
        self.send_with_policy(auth, to, amount_zatoshis, memo, shielded_spend_policy())
            .await
    }

    async fn send_with_policy(
        &self,
        auth: &SeedAuth,
        to: &str,
        amount_zatoshis: u64,
        memo: Option<&str>,
        policy: SpendPolicy,
    ) -> Result<Vec<String>> {
        self.require_network_access()?;
        self.ensure_recovery_selection_ready()?;
        crate::keys::assert_shielded_send_dest(to, crate::keys::SendDestPolicy::Shielded)?;
        let db = self.open_db()?;
        let account = Self::primary_account_id(&db)?;
        let seed = self.load_seed(auth)?;
        let usk = UnifiedSpendingKey::from_seed(
            &self.network,
            seed.expose_secret(),
            self.account_index(),
        )
        .map_err(|e| EngineError::WalletDb(format!("USK: {e:?}")))?;

        let address = ZcashAddress::try_from_encoded(to)
            .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
        let value =
            Zatoshis::from_u64(amount_zatoshis).map_err(|_| EngineError::InsufficientFunds)?;
        if amount_zatoshis == 0 && memo.map(str::trim).filter(|s| !s.is_empty()).is_none() {
            return Err(EngineError::Message(
                "zero-value send needs a memo (encrypted note)".into(),
            ));
        }
        let memo_bytes = match memo.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => {
                let mut buf = [0u8; 512];
                let raw = s.as_bytes();
                if raw.len() > 512 {
                    return Err(EngineError::Message("memo longer than 512 bytes".into()));
                }
                buf[..raw.len()].copy_from_slice(raw);
                Some(
                    MemoBytes::from_bytes(&buf)
                        .map_err(|e| EngineError::Message(format!("memo: {e:?}")))?,
                )
            }
        };
        let payment = match memo_bytes {
            None => Payment::without_memo(address, value),
            Some(m) => Payment::new(address, Some(value), Some(m), None, None, vec![])
                .map_err(|e| EngineError::Message(format!("payment: {e}")))?,
        };
        let request = TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?;
        self.finish_send(db, account, usk, request, policy).await
    }

    async fn send_payments(
        &self,
        auth: &SeedAuth,
        payments: &[crate::Zip321Payment],
        policy: SpendPolicy,
    ) -> Result<Vec<String>> {
        self.require_network_access()?;
        self.ensure_recovery_selection_ready()?;
        if payments.is_empty() {
            return Err(EngineError::Message("ZIP-321 URI has no payments".into()));
        }
        for p in payments {
            crate::keys::assert_shielded_send_dest(
                &p.address,
                crate::keys::SendDestPolicy::Shielded,
            )?;
        }
        let db = self.open_db()?;
        let account = Self::primary_account_id(&db)?;
        let seed = self.load_seed(auth)?;
        let usk = UnifiedSpendingKey::from_seed(
            &self.network,
            seed.expose_secret(),
            self.account_index(),
        )
        .map_err(|e| EngineError::WalletDb(format!("USK: {e:?}")))?;
        let mut built = Vec::with_capacity(payments.len());
        for p in payments {
            let zat = match p.amount_zec.as_deref() {
                Some(s) => parse_zec_to_zatoshis(s).map_err(EngineError::Message)?,
                None => 0,
            };
            if zat == 0
                && p.memo
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                return Err(EngineError::Message(
                    "zero-value send needs a memo (encrypted note)".into(),
                ));
            }
            built.push(zip_payment(&p.address, zat, p.memo.as_deref())?);
        }
        let request = TransactionRequest::new(built)
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?;
        self.finish_send(db, account, usk, request, policy).await
    }

    async fn finish_send(
        &self,
        mut db: Db,
        account: AccountUuid,
        usk: UnifiedSpendingKey,
        request: TransactionRequest,
        policy: SpendPolicy,
    ) -> Result<Vec<String>> {
        let input_selector = GreedyInputSelector::new();
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::default(),
        );

        let proposal = propose_transfer::<_, _, _, _, Infallible>(
            &mut db,
            &self.network,
            account,
            &input_selector,
            &change_strategy,
            request,
            crate::confirmations_policy(self.network),
            &policy,
            None,
            None,
        )
        .map_err(|e| map_funds_err(format!("propose_transfer: {e}")))?;

        let prover = crate::params::local_tx_prover()?;
        let txids = db
            .transactionally_with_extension(|wdb, ext| -> anyhow::Result<_> {
                recovery_guard::transaction_recovery_ready(wdb, ext)?;
                Ok(create_proposed_transactions::<
                    _,
                    _,
                    Infallible,
                    _,
                    Infallible,
                    _,
                >(
                    wdb,
                    &self.network,
                    &*prover,
                    &*prover,
                    &SpendingKeys::from_unified_spending_key(usk),
                    OvkPolicy::Sender,
                    &proposal,
                    None,
                )?)
            })
            .map_err(|e| map_funds_err(format!("create send tx: {e}")))?;

        let out = self.broadcast_all(&mut db, txids.iter().copied()).await;
        info!("send timings");
        out
    }

    async fn broadcast_all(
        &self,
        db: &mut Db,
        txids: impl IntoIterator<Item = TxId>,
    ) -> Result<Vec<String>> {
        let txids: Vec<_> = txids.into_iter().collect();
        let saved_ids = txids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let failure = |e: EngineError| {
            let outcome = if matches!(e, EngineError::BroadcastRejected { .. }) {
                "submission rejected"
            } else {
                "submission outcome unknown"
            };
            EngineError::BroadcastFailed(format!(
                "{outcome}; saved transaction(s) {saved_ids}: {e}"
            ))
        };
        // An explicit validator endpoint does not require a second light-server
        // connection. Either transport may lose its response after accepting a
        // transaction; saved spends and unrelated proposal locks must survive.
        let mut client = if self.validator_rpc_url().is_some() {
            None
        } else {
            Some(self.connect().await.map_err(&failure)?)
        };
        let mut out = Vec::new();
        for txid in txids {
            match self.broadcast_one(&mut client, db, txid).await {
                Ok(s) => out.push(s),
                Err(e) => return Err(failure(e)),
            }
        }
        Ok(out)
    }

    async fn broadcast_one(
        &self,
        client: &mut Option<LwdClient>,
        db: &mut Db,
        txid: TxId,
    ) -> Result<String> {
        let raw = db
            .get_transaction(txid)
            .map_err(|e| EngineError::WalletDb(format!("get_transaction: {e}")))?
            .ok_or_else(|| EngineError::WalletDb(format!("tx {txid} missing from db")))?;

        let mut data = Vec::new();
        raw.write(&mut data)
            .map_err(|e| EngineError::WalletDb(format!("serialize tx: {e}")))?;
        self.submit_raw(client, data).await?;
        info!("broadcast ok");
        Ok(txid.to_string())
    }

    /// Submit raw transaction bytes through the validator RPC when one is set,
    /// else the light server. A validator RPC that could not be reached never
    /// received the bytes, so the light server takes them rather than the send
    /// ending as "outcome unknown" with its notes held until expiry.
    async fn submit_raw(&self, client: &mut Option<LwdClient>, data: Vec<u8>) -> Result<()> {
        self.require_network_access()?;
        if let Some(rpc) = self.validator_rpc_url() {
            let bytes = data.clone();
            let url = rpc.clone();
            // Blocking socket I/O with a 120 s read timeout: off the runtime.
            let sent =
                tokio::task::spawn_blocking(move || super::rpc::send_raw_transaction(&url, &bytes))
                    .await
                    .map_err(|e| EngineError::Transport(format!("broadcast task: {e}")))?;
            match sent {
                Ok(_hash) => {
                    info!("broadcast via validator sendrawtransaction");
                    return Ok(());
                }
                Err(e) if super::rpc::never_sent(&e) => {
                    warn!("validator RPC unreachable; broadcasting through the light server");
                }
                Err(e) => return Err(e),
            }
        }
        if client.is_none() {
            *client = Some(self.connect().await?);
        }
        let resp = client
            .as_mut()
            .expect("light client connected above")
            .send_transaction(RawTransaction { data, height: 0 })
            .await
            .map_err(|e| EngineError::Transport(format!("SendTransaction: {e}")))?
            .into_inner();
        if resp.error_code != 0 {
            return Err(EngineError::BroadcastRejected {
                code: resp.error_code,
                message: resp.error_message,
            });
        }
        Ok(())
    }

    /// Resubmit this wallet's own sends that are saved but neither mined nor
    /// expired. The same bytes are idempotent: a node that already has one
    /// answers "already …", which counts as delivered. This delivers a
    /// broadcast whose outcome was unknown (lost reply, outage, the app closed
    /// between saving and sending) without the user sending again, which picks
    /// other notes and could pay twice. Returns how many were resubmitted.
    async fn rebroadcast_unmined(&self, tip: u32) -> usize {
        let pending = match unmined_sends(&self.paths.data_db, tip) {
            Ok(pending) => pending,
            Err(_e) => {
                warn!("could not list unmined sends to rebroadcast");
                return 0;
            }
        };
        let mut client = None;
        let mut sent = 0;
        for (_txid, raw) in pending {
            match self.submit_raw(&mut client, raw).await {
                Ok(()) => {
                    info!("rebroadcast saved unmined send");
                    sent += 1;
                }
                Err(e) if already_known_transaction(&e) => sent += 1,
                // A definitive rejection (spent inputs, bad anchor) expires
                // on its own and releases its notes; keep going.
                Err(_e @ EngineError::BroadcastRejected { .. }) => {
                    warn!("node rejected a saved unmined send");
                }
                Err(_e) => {
                    warn!("rebroadcast stopped; server unavailable");
                    break;
                }
            }
        }
        sent
    }

    pub async fn fetch_tip(server: &LightServer) -> Result<u32> {
        let skip =
            matches!(server, LightServer::LocalRegtest) || server.as_url().contains(":28137");
        let mut client = Self::connect_url(&server.as_url(), skip).await?;
        Ok(client
            .get_latest_block(ChainSpec::default())
            .await
            .map_err(|e| EngineError::Transport(format!("GetLatestBlock: {e}")))?
            .into_inner()
            .height
            .try_into()
            .map_err(|_| EngineError::Message("tip out of range".into()))?)
    }

    /// Probe a compact-block light server (Zaino / LWD). Independent of Zakura RPC.
    pub async fn probe_light(server: &LightServer) -> LightProbe {
        let url = server.as_url();
        let secs = if crate::is_loopback_light_url(&url) {
            5
        } else {
            10
        };
        match tokio::time::timeout(Duration::from_secs(secs), Self::probe_light_inner(server)).await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => LightProbe {
                ok: false,
                url,
                chain: String::new(),
                tip: None,
                sapling_activation: None,
                t_scan: server.allows_transparent_query(),
                error: Some(e.to_string()),
            },
            Err(_) => {
                let error = format!("probe timeout ({secs}s) {url}");
                LightProbe {
                    ok: false,
                    url,
                    chain: String::new(),
                    tip: None,
                    sapling_activation: None,
                    t_scan: server.allows_transparent_query(),
                    error: Some(error),
                }
            }
        }
    }

    async fn probe_light_inner(server: &LightServer) -> Result<LightProbe> {
        let url = server.as_url();
        let skip = matches!(server, LightServer::LocalRegtest) || url.contains(":28137");
        let mut client = Self::connect_url(&url, skip).await?;
        let info = client
            .get_lightd_info(Empty {})
            .await
            .ok()
            .map(|r| r.into_inner());
        let chain = info
            .as_ref()
            .map(|i| i.chain_name.clone())
            .unwrap_or_default();
        let sapling_activation = info
            .as_ref()
            .and_then(|i| u32::try_from(i.sapling_activation_height).ok());
        let tip = client
            .get_latest_block(ChainSpec::default())
            .await
            .ok()
            .and_then(|r| u32::try_from(r.into_inner().height).ok());
        if tip.is_none() && chain.is_empty() {
            return Err(EngineError::Transport(format!("no tip from {url}")));
        }
        Ok(LightProbe {
            ok: tip.is_some(),
            url,
            chain,
            tip,
            sapling_activation,
            t_scan: server.allows_transparent_query(),
            error: None,
        })
    }

    /// Compact blocks as a length-delimited blob for the WASM scanner (not a wallet).
    pub async fn fetch_compact_block_blob(
        server: &LightServer,
        start: u32,
        end: u32,
    ) -> Result<Vec<u8>> {
        if end < start {
            return Err(EngineError::Message("end < start".into()));
        }
        if end.saturating_sub(start) > 8_000 {
            return Err(EngineError::Message("block range larger than 8000".into()));
        }
        let skip =
            matches!(server, LightServer::LocalRegtest) || server.as_url().contains(":28137");
        let mut client = Self::connect_url(&server.as_url(), skip).await?;
        let blocks = super::pipeline::fetch_height_range(&mut client, start, end).await?;
        Ok(crate::web::encode_delimited(blocks))
    }

    /// `GetTreeState` for the WASM scanner (birthday frontier).
    pub async fn fetch_tree_state(server: &LightServer, height: u32) -> Result<serde_json::Value> {
        let skip =
            matches!(server, LightServer::LocalRegtest) || server.as_url().contains(":28137");
        let mut client = Self::connect_url(&server.as_url(), skip).await?;
        let ts = client
            .get_tree_state(BlockId {
                height: u64::from(height),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetTreeState: {e}")))?
            .into_inner();
        Ok(serde_json::json!({
            "network": ts.network,
            "height": ts.height,
            "hash": ts.hash,
            "time": ts.time,
            "saplingTree": ts.sapling_tree,
            "orchardTree": ts.orchard_tree,
            "ironwoodTree": ts.ironwood_tree,
        }))
    }

    /// `GetSubtreeRoots` for the WASM scanner (complete shards before birthday).
    /// `start_index` is the number of shards already stored (0 = full dump).
    /// `max_entries == 0` streams every root from `start_index`. The light
    /// server spends up to a second per root, so callers page.
    pub async fn fetch_subtree_roots(
        server: &LightServer,
        network: ZNetwork,
        protocol: &str,
        start_index: u32,
        max_entries: u32,
    ) -> Result<Vec<serde_json::Value>> {
        let skip_iw = Self::skip_ironwood_subtrees(network, &server.as_url());
        if protocol.eq_ignore_ascii_case("ironwood") && skip_iw {
            return Ok(vec![]);
        }
        let proto = match protocol {
            "sapling" => ShieldedProtocol::Sapling,
            "orchard" => ShieldedProtocol::Orchard,
            "ironwood" => ShieldedProtocol::Ironwood,
            other => {
                return Err(EngineError::Message(format!(
                    "unknown shielded protocol {other}"
                )))
            }
        };
        let mut client = Self::connect_url(&server.as_url(), skip_iw).await?;
        let mut req = GetSubtreeRootsArg::default();
        req.start_index = start_index;
        req.max_entries = max_entries;
        req.set_shielded_protocol(proto);
        let mut stream = client
            .get_subtree_roots(req)
            .await
            .map_err(|e| EngineError::Transport(format!("GetSubtreeRoots: {e}")))?
            .into_inner();
        let mut out = Vec::new();
        loop {
            match stream.message().await {
                Ok(Some(root)) => {
                    out.push(serde_json::json!({
                        "completingHeight": root.completing_block_height,
                        "rootHash": to_hex(&root.root_hash),
                    }));
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(EngineError::Transport(format!(
                        "GetSubtreeRoots stream: {e}"
                    )))
                }
            }
        }
        Ok(out)
    }

    /// Loopback-only transparent UTXO fetch. Leaks addresses to the light server.
    pub async fn fetch_address_utxos(
        server: &LightServer,
        addresses: &[String],
        start_height: u32,
    ) -> Result<Vec<serde_json::Value>> {
        if addresses.is_empty() {
            return Ok(vec![]);
        }
        if !server.allows_transparent_query() {
            return Err(EngineError::Message(
                "GetAddressUtxos leaks t-addrs; only a local Zaino (loopback) is allowed".into(),
            ));
        }
        let skip =
            matches!(server, LightServer::LocalRegtest) || server.as_url().contains(":28137");
        let mut client = Self::connect_url(&server.as_url(), skip).await?;
        let reply = client
            .get_address_utxos(GetAddressUtxosArg {
                addresses: addresses.to_vec(),
                start_height: u64::from(start_height),
                max_entries: 0,
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetAddressUtxos: {e}")))?
            .into_inner();
        Ok(reply
            .address_utxos
            .into_iter()
            .map(|u| {
                serde_json::json!({
                    "txid": to_hex(&u.txid),
                    "index": u.index.max(0) as u32,
                    "script": to_hex(&u.script),
                    "valueZat": u.value_zat.max(0) as u64,
                    "height": u.height,
                    "address": u.address,
                })
            })
            .collect())
    }

    /// Full transaction bytes via Zebra RPC or LWD `GetTransaction`.
    pub async fn fetch_raw_transaction(server: &LightServer, txid_hex: &str) -> Result<Vec<u8>> {
        Self::fetch_raw_transaction_with_rpc(server, server.zebra_rpc_url().as_deref(), txid_hex)
            .await
    }

    pub async fn fetch_raw_transaction_with_rpc(
        server: &LightServer,
        validator_rpc: Option<&str>,
        txid_hex: &str,
    ) -> Result<Vec<u8>> {
        let rpc = validator_rpc
            .map(str::to_string)
            .or_else(|| server.zebra_rpc_url());
        let try_rpc = |id: &str| -> Result<Vec<u8>> {
            let Some(rpc) = rpc.as_deref() else {
                return Err(EngineError::Message("no validator rpc".into()));
            };
            let hex = super::rpc::get_raw_transaction_hex(rpc, id)?;
            crate::web::from_hex(&hex).map_err(EngineError::Message)
        };
        if let Ok(raw) = try_rpc(txid_hex) {
            return Ok(raw);
        }
        if let Ok(mut bytes) = crate::web::from_hex(txid_hex) {
            if bytes.len() == 32 {
                bytes.reverse();
                if let Ok(raw) = try_rpc(&to_hex(&bytes)) {
                    return Ok(raw);
                }
            }
        }
        let skip =
            matches!(server, LightServer::LocalRegtest) || server.as_url().contains(":28137");
        let mut client = Self::connect_url(&server.as_url(), skip).await?;
        let mut hash = TxId::from_hex(txid_hex)
            .ok_or_else(|| EngineError::Message("txid must be 64 hexadecimal digits".into()))?
            .as_ref()
            .to_vec();
        let mut last_err = None;
        for _ in 0..2 {
            match client
                .get_transaction(TxFilter {
                    block: None,
                    index: 0,
                    hash: hash.clone(),
                })
                .await
            {
                Ok(resp) => {
                    let data = resp.into_inner().data;
                    if !data.is_empty() {
                        return Ok(data);
                    }
                    last_err = Some(EngineError::Message("GetTransaction empty".into()));
                }
                Err(e) => {
                    last_err = Some(EngineError::Transport(format!("GetTransaction: {e}")));
                }
            }
            hash.reverse();
        }
        Err(last_err.unwrap_or_else(|| EngineError::Message("GetTransaction empty".into())))
    }
}

/// One light-server answer for a transaction data request.
enum TxLookup {
    /// Raw bytes and the mined height, when the server reported one.
    Found(Vec<u8>, Option<BlockHeight>),
    /// The node has no such mempool or main-chain transaction.
    Unknown,
    /// Transport or server trouble; a later sync asks again.
    Unavailable(EngineError),
}

/// lightwalletd answers NotFound; Zaino wraps the node's legacy RPC error.
fn unknown_transaction(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::NotFound
        || status
            .message()
            .contains("No such mempool or main chain transaction")
}

fn needs_migrate(db: &mut Db) -> Result<bool> {
    // Cheap heuristic: if accounts query works, schema is current enough for open.
    // Still run migrator when accounts query fails.
    match db.get_account_ids() {
        Ok(_) => Ok(false),
        Err(_) => Ok(true),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn zip_payment(to: &str, zat: u64, memo: Option<&str>) -> Result<Payment> {
    let address = ZcashAddress::try_from_encoded(to.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    let value = Zatoshis::from_u64(zat).map_err(|_| EngineError::InsufficientFunds)?;
    let memo_bytes = match memo.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let mut buf = [0u8; 512];
            let raw = s.as_bytes();
            if raw.len() > 512 {
                return Err(EngineError::Message("memo longer than 512 bytes".into()));
            }
            buf[..raw.len()].copy_from_slice(raw);
            Some(
                MemoBytes::from_bytes(&buf)
                    .map_err(|e| EngineError::Message(format!("memo: {e:?}")))?,
            )
        }
    };
    match memo_bytes {
        None => Ok(Payment::without_memo(address, value)),
        Some(m) => Payment::new(address, Some(value), Some(m), None, None, vec![])
            .map_err(|e| EngineError::Message(format!("payment: {e}"))),
    }
}

fn memos_for_txid(conn: &rusqlite::Connection, txid: &[u8]) -> Vec<String> {
    let sql = "SELECT memo FROM orchard_received_notes n
         JOIN transactions t ON t.id_tx = n.transaction_id WHERE t.txid = ?1 AND n.memo IS NOT NULL
         UNION ALL
         SELECT memo FROM sapling_received_notes n
         JOIN transactions t ON t.id_tx = n.transaction_id WHERE t.txid = ?1 AND n.memo IS NOT NULL
         UNION ALL
         SELECT memo FROM ironwood_received_notes n
         JOIN transactions t ON t.id_tx = n.transaction_id WHERE t.txid = ?1 AND n.memo IS NOT NULL
         UNION ALL
         SELECT memo FROM sent_notes n
         JOIN transactions t ON t.id_tx = n.transaction_id WHERE t.txid = ?1 AND n.memo IS NOT NULL";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([txid], |row| row.get::<_, Vec<u8>>(0)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in rows.flatten() {
        if let Ok(Memo::Text(t)) = Memo::from_bytes(&r) {
            let s = t.to_string();
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// Most unmined sends one sync resubmits.
const REBROADCAST_MAX: usize = 16;

/// This wallet's own sends (`created` is only set when the wallet built the
/// transaction) that are saved, unmined and still valid above `tip`.
fn unmined_sends(db_path: &Path, tip: u32) -> Result<Vec<(TxId, Vec<u8>)>> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| EngineError::WalletDb(format!("open: {e}")))?;
    let _ = conn.pragma_update(None, "query_only", true);
    let mut stmt = conn
        .prepare(
            "SELECT txid, raw FROM transactions
             WHERE created IS NOT NULL AND mined_height IS NULL AND raw IS NOT NULL
               AND expiry_height > ?1
             ORDER BY id_tx LIMIT ?2",
        )
        .map_err(|e| EngineError::WalletDb(format!("unmined sends: {e}")))?;
    let rows = stmt
        .query_map(rusqlite::params![tip, REBROADCAST_MAX as i64], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| EngineError::WalletDb(format!("unmined sends: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, raw) = row.map_err(|e| EngineError::WalletDb(format!("unmined sends: {e}")))?;
        let Ok(id) = <[u8; 32]>::try_from(id.as_slice()) else {
            continue;
        };
        out.push((TxId::from_bytes(id), raw));
    }
    Ok(out)
}

/// A node that already holds *this* transaction. The reason has to start with
/// one of these phrases, or be Zebra's full already-mined sentence. "nullifier
/// already known" and "conflicts with a transaction already in the mempool" are
/// real rejections.
fn already_known_transaction(error: &EngineError) -> bool {
    let EngineError::BroadcastRejected { message, .. } = error else {
        return false;
    };
    let message = message.trim().to_ascii_lowercase();
    // Zebra reports an already-mined transaction with this whole sentence.
    // A different suffix after the same prefix is still a real refusal.
    if message
        == "any transaction with the same effects will be rejected from the mempool until a chain reset: transaction was committed to the best chain"
    {
        return true;
    }
    const PREFIXES: &[&str] = &[
        "txn-already-in-mempool",
        "txn-already-known",
        "transaction is already in the mempool",
        "transaction already in mempool",
        // Zebra's replies for a transaction it holds unmined.
        "transaction already exists in mempool",
        "transaction dropped because it is already queued for download",
        "transaction already in block chain",
        "transaction already in the block chain",
        "already exists in the mempool",
        "already exists in mempool",
        "already in the mempool",
        "already in mempool",
        "already in the block chain",
        "already in block chain",
        "transaction was committed to the best chain",
    ];
    PREFIXES.iter().any(|prefix| {
        let Some(rest) = message.strip_prefix(prefix) else {
            return false;
        };
        rest.chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

fn light_tcp_reachable(url: &str) -> bool {
    let Some((host, port)) = crate::light_url_host_port(url) else {
        return true;
    };
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    match target.to_socket_addrs() {
        Ok(mut addrs) => addrs.next().is_some_and(|addr| {
            std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
        }),
        Err(_) => false,
    }
}

fn skip_maintain_err(e: &EngineError) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("insufficient")
        || s.contains("no spendable")
        || s.contains("no transparent")
        || s.contains("sync required")
}

fn map_funds_err(msg: String) -> EngineError {
    tracing::debug!("wallet proposal or transaction construction failed");
    let lower = msg.to_lowercase();
    if lower.contains("insufficient") || lower.contains("no spendable") {
        EngineError::InsufficientFunds
    } else {
        EngineError::WalletDb(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_definitive_miss_marks_a_transaction_unrecognized() {
        // Zaino 0.10 wraps the node's error in an Internal status.
        let zaino = tonic::Status::internal(
            "RPC error: LegacyRpcError { code: -5, message: \"No such mempool or main chain \
             transaction\" }",
        );
        assert!(unknown_transaction(&zaino));
        assert!(unknown_transaction(&tonic::Status::not_found("tx")));
        // Retry later: the server may simply be restarting.
        assert!(!unknown_transaction(&tonic::Status::unavailable("connect")));
        assert!(!unknown_transaction(&tonic::Status::internal(
            "Failed to fetch block data"
        )));
    }

    #[test]
    fn sync_retries_only_temporary_light_failures() {
        assert!(is_retryable_sync_outage(&EngineError::Transport(
            "GetBlockRange: status: Unavailable".into()
        )));
        assert!(is_retryable_sync_outage(&EngineError::Transport(
            "GetBlockRange 1..=1000 timed out after 45s".into()
        )));
        // The Display of tonic's channel timeout.
        let timeout = tonic::Status::cancelled("Timeout expired");
        assert!(is_retryable_sync_outage(&EngineError::Transport(format!(
            "GetLatestBlock: {timeout}"
        ))));
        assert!(!is_retryable_sync_outage(&EngineError::Transport(
            "GetBlockRange prev_hash mismatch at 999".into()
        )));
        assert!(!is_retryable_sync_outage(&EngineError::ChainMismatch {
            wallet: "main".into(),
            server: "test".into(),
        }));
        assert!(!is_retryable_sync_outage(&EngineError::WalletDb(
            "sqlite write failed".into()
        )));
    }

    #[tokio::test]
    async fn temporary_light_failure_resumes_without_resetting_progress() {
        let cancel = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(SyncProgress {
            scanned_height: Some(3_400_000),
            ..Default::default()
        }));
        let attempts = std::cell::Cell::new(0);
        let result = retry_transient_sync(
            Some(live.clone()),
            None,
            Duration::from_millis(100),
            Duration::from_millis(1),
            &cancel,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                async move {
                    if attempt < 3 {
                        Err(EngineError::Transport("status: Unavailable".into()))
                    } else {
                        Ok((3_400_001, SyncProgress::default()))
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts.get(), 3);
        assert_eq!(result.0, 3_400_001);
        assert_eq!(live.lock().unwrap().scanned_height, Some(3_400_000));
    }

    #[tokio::test]
    async fn an_outage_after_scan_progress_gets_a_fresh_grace() {
        let cancel = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(SyncProgress::default()));
        let attempts = std::cell::Cell::new(0);
        let result = retry_transient_sync(
            Some(live.clone()),
            None,
            Duration::from_millis(60),
            Duration::from_millis(1),
            &cancel,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                let live = live.clone();
                async move {
                    match attempt {
                        1 => Err(EngineError::Transport("status: Unavailable".into())),
                        // Reconnected, scanned for longer than the grace, then
                        // lost the server again.
                        2 => {
                            live.lock().unwrap().scanned_height = Some(3_400_000);
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            Err(EngineError::Transport("transport error".into()))
                        }
                        _ => Ok((3_400_100, SyncProgress::default())),
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts.get(), 3);
        assert_eq!(result.0, 3_400_100);
    }

    #[tokio::test]
    async fn attempts_that_never_move_the_scan_share_one_grace() {
        let cancel = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(SyncProgress {
            scanned_height: Some(3_400_000),
            ..Default::default()
        }));
        let attempts = std::cell::Cell::new(0);
        let started = Instant::now();
        let error = retry_transient_sync(
            Some(live),
            None,
            Duration::from_millis(60),
            Duration::from_millis(1),
            &cancel,
            || {
                attempts.set(attempts.get() + 1);
                async {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    Err(EngineError::Transport(
                        "GetBlockRange timed out after 90s".into(),
                    ))
                }
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, EngineError::Transport(_)));
        assert!(attempts.get() >= 2);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn backoff_ends_when_a_closed_light_port_reopens() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let reopen = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(listener);
        });
        let cancel = AtomicBool::new(false);
        let attempts = std::cell::Cell::new(0);
        let started = Instant::now();
        let result = retry_transient_sync(
            None,
            Some(&url),
            Duration::from_secs(120),
            Duration::from_secs(30),
            &cancel,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                async move {
                    if attempt == 1 {
                        Err(EngineError::Transport("connect: transport error".into()))
                    } else {
                        Ok((1, SyncProgress::default()))
                    }
                }
            },
        )
        .await
        .unwrap();
        reopen.abort();
        assert_eq!(result.0, 1);
        assert_eq!(attempts.get(), 2);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn backoff_jitter_stays_within_a_quarter_below_the_delay() {
        for _ in 0..200 {
            let delay = jittered(Duration::from_secs(8));
            assert!(delay >= Duration::from_secs(6) && delay <= Duration::from_secs(8));
        }
    }

    #[tokio::test]
    async fn permanent_sync_failure_is_not_retried() {
        let cancel = AtomicBool::new(false);
        let attempts = std::cell::Cell::new(0);
        let error = retry_transient_sync(
            None,
            None,
            Duration::from_secs(1),
            Duration::from_millis(1),
            &cancel,
            || {
                attempts.set(attempts.get() + 1);
                async { Err(EngineError::WalletDb("sqlite failure".into())) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(matches!(error, EngineError::WalletDb(_)));
    }

    #[tokio::test]
    async fn unavailable_light_server_exhausts_bounded_grace() {
        let cancel = AtomicBool::new(false);
        let attempts = std::cell::Cell::new(0);
        let error = retry_transient_sync(
            None,
            None,
            Duration::from_millis(5),
            Duration::from_millis(1),
            &cancel,
            || {
                attempts.set(attempts.get() + 1);
                async { Err(EngineError::Transport("status: Unavailable".into())) }
            },
        )
        .await
        .unwrap_err();
        assert!(attempts.get() >= 2);
        assert!(matches!(error, EngineError::Transport(_)));
    }

    #[tokio::test]
    async fn cancelling_active_attempt_drops_it_without_retry() {
        let cancel = Arc::new(AtomicBool::new(false));
        let attempts = std::cell::Cell::new(0);
        let changed = Arc::clone(&cancel);
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            changed.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let error = retry_transient_sync(
            None,
            None,
            Duration::from_secs(120),
            Duration::from_secs(2),
            &cancel,
            || {
                attempts.set(attempts.get() + 1);
                async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok((1, SyncProgress::default()))
                }
            },
        )
        .await
        .unwrap_err();
        stopper.await.unwrap();
        assert_eq!(attempts.get(), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(error, EngineError::Message(message) if message.contains("cancelled")));
    }

    #[tokio::test]
    async fn cancelling_retry_backoff_stops_before_next_attempt() {
        let cancel = Arc::new(AtomicBool::new(false));
        let changed = Arc::clone(&cancel);
        let attempts = std::cell::Cell::new(0);
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            changed.store(true, Ordering::Release);
        });
        let error = retry_transient_sync(
            None,
            None,
            Duration::from_secs(120),
            Duration::from_secs(2),
            &cancel,
            || {
                attempts.set(attempts.get() + 1);
                async { Err(EngineError::Transport("status: Unavailable".into())) }
            },
        )
        .await
        .unwrap_err();
        stopper.await.unwrap();
        assert_eq!(attempts.get(), 1);
        assert!(matches!(error, EngineError::Message(message) if message.contains("cancelled")));
    }

    fn fixture_meta() -> WalletMeta {
        WalletMeta {
            network: "regtest".into(),
            server: "http://127.0.0.1:1".into(),
            birthday_height: 1,
            account_index: 0,
            validator_rpc: None,
            ufvk: None,
            view_only: true,
            unlock_policy: UnlockPolicy::default(),
            os_unlock: false,
            allow_deep_sync: false,
        }
    }

    fn fixture_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE fixture(value); INSERT INTO fixture VALUES (1);",
        )
        .unwrap();
    }

    fn has_fixture(path: &Path) -> bool {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fixture'",
                [],
                |r| r.get::<_, u32>(0),
            )
            .unwrap()
            == 1
    }

    #[test]
    fn a_finished_wallet_folder_refuses_a_new_seed_even_without_its_database() {
        let dir = tempfile::tempdir().unwrap();
        let paths = WalletPaths::new(dir.path());
        paths.ensure_dirs().unwrap();
        write_meta(&paths.meta_path, &fixture_meta()).unwrap();
        let seed = dir.path().join("seed.enc");
        std::fs::write(&seed, b"existing seed").unwrap();
        assert!(matches!(
            paths.prepare_new_wallet(),
            Err(EngineError::AlreadyExists(_))
        ));
        std::fs::remove_file(&paths.meta_path).unwrap();
        std::fs::write(paths.reset_backup(), b"moved-aside wallet").unwrap();
        assert!(matches!(
            paths.prepare_new_wallet(),
            Err(EngineError::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read(&seed).unwrap(), b"existing seed");
    }

    #[test]
    fn an_unfinished_create_is_set_aside_instead_of_blocking_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let paths = WalletPaths::new(dir.path());
        paths.ensure_dirs().unwrap();
        fixture_db(&paths.data_db);
        paths.prepare_new_wallet().unwrap();
        assert!(!paths.data_db.exists());
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("data.sqlite.incomplete-"))
            .collect();
        assert!(aside
            .iter()
            .any(|name| !name.contains("-wal") && !name.contains("-shm")));
    }

    #[test]
    fn open_puts_back_a_database_an_interrupted_rescan_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let paths = WalletPaths::new(dir.path());
        paths.ensure_dirs().unwrap();
        write_meta(&paths.meta_path, &fixture_meta()).unwrap();
        fixture_db(&paths.reset_backup());
        NativeWallet::open(dir.path()).unwrap();
        assert!(has_fixture(&paths.data_db));
        assert!(!paths.reset_backup().exists());
    }

    pub(super) fn fixture_wallet(dir: &Path) -> NativeWallet {
        let paths = WalletPaths::new(dir);
        paths.ensure_dirs().unwrap();
        let meta = fixture_meta();
        write_meta(&paths.meta_path, &meta).unwrap();
        NativeWallet {
            network_access: true,
            paths,
            network: ZNetwork::Regtest,
            server: LightServer::parse(&meta.server, ZNetwork::Regtest),
            meta,
        }
    }

    #[test]
    fn attaching_seed_preserves_the_stored_policy_despite_stale_auth() {
        let words = crate::keys::REGTEST_FAUCET_MNEMONIC;
        let keys = crate::keys::account_from_mnemonic(words, ZNetwork::Regtest, 0).unwrap();
        for (stored, stale) in [
            (UnlockPolicy::EachSpend, UnlockPolicy::Session),
            (UnlockPolicy::Always, UnlockPolicy::Session),
            (UnlockPolicy::Session, UnlockPolicy::EachSpend),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut wallet = fixture_wallet(dir.path());
            fixture_db(&wallet.paths.data_db);
            wallet.meta.ufvk = Some(keys.ufvk.clone());
            wallet.set_unlock_policy(stored).unwrap();

            // Restore (or /unlock-policy) persisted a policy, but the running
            // bridge still holds its startup SeedAuth when /attach-seed opens.
            let mut auth = SeedAuth::passphrase("test-only seed encryption");
            auth.unlock_policy = stale;
            let mut wallet = NativeWallet::open(dir.path()).unwrap();
            wallet.attach_seed(words, &auth).unwrap();
            assert_eq!(wallet.unlock_policy(), stored);
            let reopened = NativeWallet::open(dir.path()).unwrap();
            assert_eq!(reopened.unlock_policy(), stored);
            assert!(!reopened.is_view_only());
            let expected = Mnemonic::parse_normalized(words).unwrap().to_seed("");
            assert_eq!(
                reopened.load_seed(&auth).unwrap().expose_secret(),
                &expected
            );

            // A mismatched paste must not replace the saved seed or metadata.
            let seed_path = dir.path().join("seed.enc");
            let before_seed = std::fs::read(&seed_path).unwrap();
            let before_meta = std::fs::read(&wallet.paths.meta_path).unwrap();
            let wrong = Mnemonic::from_entropy(&[3; 32]).unwrap().to_string();
            assert!(wallet.attach_seed(&wrong, &auth).is_err());
            assert_eq!(std::fs::read(&seed_path).unwrap(), before_seed);
            assert_eq!(std::fs::read(&wallet.paths.meta_path).unwrap(), before_meta);
        }
    }

    pub(super) fn fixture_account() -> (UnifiedFullViewingKey, AccountBirthday) {
        let ufvk =
            UnifiedSpendingKey::from_seed(&ZNetwork::Regtest, &[7u8; 32], Zip32AccountId::ZERO)
                .unwrap()
                .to_unified_full_viewing_key();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                BlockHeight::from_u32(0),
                zcash_primitives::block::BlockHash([0; 32]),
            ),
            None,
        );
        (ufvk, birthday)
    }

    #[test]
    fn rescan_swaps_in_a_fresh_database_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        fixture_db(&wallet.paths.data_db);
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        assert!(!has_fixture(&wallet.paths.data_db));
        let accounts: u32 = rusqlite::Connection::open(&wallet.paths.data_db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accounts, 1);
        assert!(!dir.path().join("data.sqlite.new").exists());
        assert!(!wallet.paths.reset_backup().exists());
        NativeWallet::open(dir.path()).unwrap();
    }

    #[test]
    fn a_rescan_that_cannot_build_its_database_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        fixture_db(&wallet.paths.data_db);
        // A directory where the replacement would go makes the build fail.
        std::fs::create_dir(dir.path().join("data.sqlite.new")).unwrap();
        let (ufvk, birthday) = fixture_account();
        let error = wallet.replace_scan_db(&ufvk, &birthday).unwrap_err();
        assert!(
            error.to_string().contains("existing data was retained"),
            "{error}"
        );
        assert!(has_fixture(&wallet.paths.data_db));
        NativeWallet::open(dir.path()).unwrap();
    }

    #[test]
    fn only_saved_unmined_unexpired_sends_are_rebroadcast() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let tip = 1_000u32;
        let insert = |id: u8, created: bool, mined: Option<u32>, expiry: u32| {
            conn.execute(
                "INSERT INTO transactions (txid, created, mined_height, expiry_height, raw, min_observed_height)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                rusqlite::params![
                    vec![id; 32],
                    created.then_some("2026-09-24T00:00:00Z"),
                    mined,
                    expiry,
                    vec![id; 4]
                ],
            )
            .unwrap();
        };
        insert(1, true, None, tip + 20); // lost reply: resend
        insert(2, true, Some(990), tip + 20); // mined
        insert(3, false, None, tip + 20); // someone else's
        insert(4, true, None, tip); // expires at the tip
        insert(5, true, None, tip + 1); // still valid in the next block
        drop(conn);
        let pending = unmined_sends(&wallet.paths.data_db, tip).unwrap();
        let ids: Vec<u8> = pending.iter().map(|(id, _)| id.as_ref()[0]).collect();
        assert_eq!(ids, vec![1, 5]);
        assert_eq!(pending[0].1, vec![1u8; 4]);
    }

    #[test]
    fn a_node_that_already_has_the_transaction_counts_as_delivered() {
        for message in [
            "transaction is already in the mempool",
            "txn-already-in-mempool",
            "transaction already in block chain",
            "transaction already exists in mempool",
            "transaction dropped because it is already queued for download",
            "any transaction with the same effects will be rejected from the mempool until a chain reset: transaction was committed to the best chain",
        ] {
            assert!(already_known_transaction(&EngineError::BroadcastRejected {
                code: -27,
                message: message.into(),
            }));
        }
        assert!(!already_known_transaction(
            &EngineError::BroadcastRejected {
                code: -26,
                message: "bad-txns-inputs-spent".into(),
            }
        ));
        assert!(!already_known_transaction(
            &EngineError::BroadcastRejected {
                code: -26,
                message: "nullifier already known".into(),
            }
        ));
        assert!(!already_known_transaction(
            &EngineError::BroadcastRejected {
                code: -26,
                message: "conflicts with a transaction already in the mempool".into(),
            }
        ));
        assert!(!already_known_transaction(
            &EngineError::BroadcastRejected {
                code: -1,
                message: "any transaction with the same effects will be rejected from the mempool until a chain reset: expired"
                    .into(),
            }
        ));
        assert!(!already_known_transaction(&EngineError::Transport(
            "already closed".into()
        )));
    }

    #[test]
    fn replacing_a_file_keeps_the_old_contents_until_the_new_copy_is_finished() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("seed.enc");
        std::fs::write(&target, b"old").unwrap();
        let failed = super::super::replace_file(&target, b"new", |_| {
            Err(EngineError::Message("permissions".into()))
        });
        assert!(failed.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!dir.path().join("seed.enc.tmp").exists());
        super::super::replace_file(&target, b"new", |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn reset_requires_a_complete_wal_checkpoint_before_replacing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.sqlite");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value); INSERT INTO fixture VALUES (1);").unwrap();
        NativeWallet::checkpoint_sqlite(&path).unwrap();
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT * FROM fixture;")
            .unwrap();
        writer
            .execute("INSERT INTO fixture VALUES (2)", [])
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let wal = path.with_file_name("checkpoint.sqlite-wal");
        let pending = std::fs::read(&wal).unwrap();
        let error = NativeWallet::checkpoint_sqlite(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains("checkpoint before reset is incomplete"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&wal).unwrap(), pending);
        assert_eq!(
            writer
                .query_row("SELECT COUNT(*) FROM fixture", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            2
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        NativeWallet::checkpoint_sqlite(&path).unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(
            writer
                .query_row("SELECT COUNT(*) FROM fixture", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn history_memos_include_ironwood_and_filter_by_transaction() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE transactions (id_tx INTEGER PRIMARY KEY, txid BLOB);
            CREATE TABLE orchard_received_notes (transaction_id INTEGER, memo BLOB);
            CREATE TABLE sapling_received_notes (transaction_id INTEGER, memo BLOB);
            CREATE TABLE ironwood_received_notes (transaction_id INTEGER, memo BLOB);
            CREATE TABLE sent_notes (transaction_id INTEGER, memo BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transactions VALUES (1, ?1), (2, ?2)",
            rusqlite::params![vec![1u8; 32], vec![2u8; 32]],
        )
        .unwrap();
        let mut memo = [0u8; 512];
        memo[..8].copy_from_slice(b"ironwood");
        conn.execute(
            "INSERT INTO ironwood_received_notes VALUES (1, ?1)",
            [memo.as_slice()],
        )
        .unwrap();
        assert_eq!(memos_for_txid(&conn, &[1; 32]), ["ironwood"]);
        assert!(memos_for_txid(&conn, &[2; 32]).is_empty());
    }

    #[cfg(feature = "transparent-inputs")]
    #[test]
    fn history_and_transaction_use_canonical_ids_with_real_sqlite_rows() {
        use zcash_client_backend::wallet::WalletTransparentOutput;
        let dir = tempfile::TempDir::new().unwrap();
        let paths = WalletPaths::new(dir.path());
        let mut db = open_wallet_db(&paths.data_db, ZNetwork::Regtest).unwrap();
        init_wallet_db(&mut db, Some(SecretVec::new(vec![7; 32]))).unwrap();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                0.into(),
                zcash_primitives::block::BlockHash([0; 32]),
            ),
            None,
        );
        let (account, _) = db
            .create_account(
                "history regression",
                &SecretVec::new(vec![7; 32]),
                &birthday,
                None,
            )
            .unwrap();
        db.update_chain_tip(110.into()).unwrap();
        let address = *db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .keys()
            .next()
            .unwrap();
        let raw_id = std::array::from_fn(|i| i as u8);
        let txid = TxId::from_bytes(raw_id);
        let canonical = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
        assert_eq!(txid.to_string(), canonical);
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(raw_id, 0),
            transparent::bundle::TxOut::new(
                Zatoshis::from_u64(150_000).unwrap(),
                address.script().into(),
            ),
            Some(101.into()),
            Some(account),
            None,
            None,
        )
        .unwrap();
        db.put_received_transparent_utxo(&output).unwrap();
        drop(db);
        let wallet = NativeWallet {
            network_access: true,
            paths,
            network: ZNetwork::Regtest,
            server: LightServer::LocalRegtest,
            meta: WalletMeta {
                network: "regtest".into(),
                server: LightServer::LocalRegtest.as_url(),
                birthday_height: 1,
                account_index: 0,
                validator_rpc: None,
                ufvk: None,
                view_only: true,
                unlock_policy: UnlockPolicy::Session,
                os_unlock: false,
                allow_deep_sync: false,
            },
        };
        let rows = wallet.history(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].txid, canonical);
        assert_eq!(rows[0].received_zat, 150_000);
        let single = wallet
            .transaction(&canonical.to_uppercase())
            .unwrap()
            .unwrap();
        assert_eq!(single.txid, canonical);
        assert_eq!(single.mined_height, Some(101));
        assert_eq!(
            wallet
                .query_history(0, Some(crate::HistoryStatusFilter::Mined), Some(canonical))
                .unwrap()
                .len(),
            1
        );
        assert!(wallet
            .query_history(
                10,
                Some(crate::HistoryStatusFilter::Pending),
                Some(canonical)
            )
            .unwrap()
            .is_empty());
        assert!(wallet.transaction(&to_hex(&raw_id)).unwrap().is_none());
        assert!(wallet.transaction(&"ff".repeat(32)).unwrap().is_none());
        assert!(wallet.transaction("invalid").is_err());
    }

    #[test]
    fn deep_sync_guard() {
        assert!(NativeWallet::check_sync_gap(0, MAX_MEM_SYNC_BLOCKS, false).is_ok());
        assert!(NativeWallet::check_sync_gap(0, MAX_MEM_SYNC_BLOCKS + 1, false).is_err());
        assert!(NativeWallet::check_sync_gap(0, MAX_MEM_SYNC_BLOCKS + 1, true).is_ok());
    }

    #[test]
    fn a_wallet_that_scanned_before_may_catch_up_any_distance() {
        let birthday = 3_000_000;
        let far = birthday + MAX_MEM_SYNC_BLOCKS * 2;
        assert!(matches!(
            NativeWallet::check_catch_up_gap(birthday - 1, birthday, far, false),
            Err(EngineError::DeepSyncRejected { .. })
        ));
        assert!(NativeWallet::check_catch_up_gap(birthday, birthday, far, false).is_ok());
        assert!(NativeWallet::check_catch_up_gap(birthday + 10, birthday, far, false).is_ok());
    }

    #[test]
    fn last_filled_island_end_skips_tip_crumbs() {
        let dir = std::env::temp_dir().join(format!(
            "z-stack-wallet-island-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE blocks (height INTEGER PRIMARY KEY)", [])
            .unwrap();
        for h in 10u32..=20 {
            conn.execute("INSERT INTO blocks (height) VALUES (?1)", [h])
                .unwrap();
        }
        for h in 100u32..=105 {
            conn.execute("INSERT INTO blocks (height) VALUES (?1)", [h])
                .unwrap();
        }
        drop(conn);
        assert_eq!(last_filled_island_end(&path, 10, 105), 20);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE scan_queue (
                block_range_start INTEGER NOT NULL,
                block_range_end INTEGER NOT NULL,
                priority INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_queue (block_range_start, block_range_end, priority)
             VALUES (10, 80, 10)",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            last_filled_island_end(&path, 10, 105),
            79,
            "scan_queue Scanned watermark must beat a sparse/stale blocks island"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sparse_8000_watermark_rewind_never_invents_missing_height() {
        let birthday = 3_335_466u32;
        let first_notes = birthday + 4_404; // 3339870 — first persist-on-notes
        let watermark = birthday + crate::scan::HISTORIC_PERSIST_BLOCKS - 1;
        assert_eq!(first_notes, 3_339_870);
        assert!(rewind_target_from_rows(first_notes, &[]).is_none());
        assert_eq!(
            rewind_target_from_rows(first_notes, &[watermark]),
            None,
            "must not rewind to birthday+4404 when only the 8000 watermark exists"
        );
        assert_eq!(
            rewind_target_from_rows(watermark, &[watermark]),
            Some(watermark)
        );
        assert_eq!(
            rewind_target_from_rows(watermark, &[birthday, first_notes, watermark]),
            Some(watermark)
        );
        assert_eq!(
            rewind_target_from_rows(first_notes, &[birthday, first_notes]),
            Some(first_notes)
        );

        let dir = std::env::temp_dir().join(format!(
            "z-stack-sparse-rewind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE blocks (height INTEGER PRIMARY KEY)", [])
            .unwrap();
        conn.execute("CREATE TABLE scan_queue (block_range_start INTEGER, block_range_end INTEGER, priority INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO blocks (height) VALUES (?1)", [watermark])
            .unwrap();
        conn.execute(
            "INSERT INTO scan_queue (block_range_start, block_range_end, priority) VALUES (?1, ?2, 10)",
            [birthday, watermark + 1],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            last_filled_island_end(&path, birthday, watermark + 40_000),
            watermark,
            "queue watermark still counts for clip"
        );
        assert_eq!(rewind_target(&path, first_notes).unwrap(), None);
        assert_eq!(rewind_target(&path, watermark).unwrap(), Some(watermark));
        // Empty blocks (wipe): ignore leftover queue; rewind target is missing.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM blocks", []).unwrap();
        drop(conn);
        assert_eq!(
            last_filled_island_end(&path, birthday, watermark + 40_000),
            birthday.saturating_sub(1),
            "wipe/empty blocks must not keep the stale queue island"
        );
        assert!(rewind_target(&path, first_notes).unwrap().is_none());
        assert!(rewind_target(&path, watermark).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filled_cursor_stops_at_queue_hole_despite_a_later_scanned_island() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("islands.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE blocks (height INTEGER PRIMARY KEY);
            CREATE TABLE scan_queue (block_range_start INTEGER, block_range_end INTEGER, priority INTEGER);
            INSERT INTO blocks VALUES (20),(80),(100);
            INSERT INTO scan_queue VALUES (10,21,10),(21,80,20),(80,101,10);").unwrap();
        assert_eq!(last_filled_island_end(&path, 10, 100), 20);
        conn.execute(
            "UPDATE scan_queue SET priority=10 WHERE block_range_start=21",
            [],
        )
        .unwrap();
        assert_eq!(last_filled_island_end(&path, 10, 100), 100);
        conn.execute("DELETE FROM scan_queue WHERE block_range_start=10", [])
            .unwrap();
        assert_eq!(
            last_filled_island_end(&path, 10, 100),
            9,
            "a tip island cannot establish birthday coverage"
        );
    }

    fn checkpoint_repair_fixture() -> (tempfile::TempDir, PathBuf, rusqlite::Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("data.sqlite");
        let mut db = open_wallet_db(&path, ZNetwork::Regtest).unwrap();
        init_wallet_db(&mut db, None).unwrap();
        drop(db);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        (dir, path, conn)
    }

    fn checkpoint_fixture_wallet(root: &Path, birthday_height: u32) -> NativeWallet {
        NativeWallet {
            network_access: true,
            paths: WalletPaths::new(root),
            network: ZNetwork::Regtest,
            server: LightServer::LocalRegtest,
            meta: WalletMeta {
                network: "regtest".into(),
                server: LightServer::LocalRegtest.as_url(),
                birthday_height,
                account_index: 0,
                validator_rpc: None,
                ufvk: None,
                view_only: true,
                unlock_policy: UnlockPolicy::Session,
                os_unlock: false,
                allow_deep_sync: false,
            },
        }
    }

    #[test]
    fn gap_rewind_rejects_missing_target_and_already_scanned_tip() {
        let (dir, path, conn) = checkpoint_repair_fixture();
        let wallet = checkpoint_fixture_wallet(dir.path(), 101);
        let error = wallet.rewind_scan_to_gap_at_tip(200).unwrap_err();
        assert!(error.to_string().contains("no persisted block"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );

        conn.execute_batch(
            "INSERT INTO blocks (height,hash,time,sapling_tree) VALUES
                (200,zeroblob(32),0,X'000000');
             INSERT INTO scan_queue (block_range_start,block_range_end,priority)
                VALUES (101,201,10);",
        )
        .unwrap();
        let before: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let error = wallet.rewind_scan_to_gap_at_tip(200).unwrap_err();
        assert!(error.to_string().contains("no gap to rewind"));
        let after: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(rewind_target(&path, 200).unwrap(), Some(200));
    }

    #[test]
    fn rewind_source_query_error_is_not_reported_as_no_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("broken.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE unrelated(value INTEGER)")
            .unwrap();
        let error = rewind_target(&path, 200).unwrap_err();
        assert!(error.to_string().contains("read rewind source"));
    }

    #[test]
    fn gap_rewind_reports_rejected_tree_truncation_without_mutating_wallet() {
        let (dir, path, conn) = checkpoint_repair_fixture();
        let mut db = open_wallet_db(&path, ZNetwork::Regtest).unwrap();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                100.into(),
                zcash_primitives::block::BlockHash([0; 32]),
            ),
            None,
        );
        db.create_account(
            "rewind fixture",
            &SecretVec::new(vec![7; 32]),
            &birthday,
            None,
        )
        .unwrap();
        db.update_chain_tip(300.into()).unwrap();
        drop(db);
        conn.execute_batch(
            "DELETE FROM scan_queue;
             INSERT INTO blocks (height,hash,time,sapling_tree,orchard_commitment_tree_size)
                VALUES (200,zeroblob(32),0,X'000000',12);
             INSERT OR REPLACE INTO orchard_tree_checkpoints VALUES (100,9),(300,19);
             INSERT INTO transactions (txid,block,mined_height,min_observed_height)
                VALUES (zeroblob(32),200,200,200);
             INSERT INTO orchard_received_notes
                (transaction_id,action_index,account_id,diversifier,value,rho,rseed,
                 is_change,commitment_tree_position)
                VALUES ((SELECT id_tx FROM transactions LIMIT 1),0,
                        (SELECT id FROM accounts LIMIT 1),zeroblob(11),123,
                        zeroblob(32),zeroblob(32),0,11);
             INSERT INTO scan_queue (block_range_start,block_range_end,priority)
                VALUES (101,201,10);",
        )
        .unwrap();
        let wallet = checkpoint_fixture_wallet(dir.path(), 101);
        let before: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let error = wallet.rewind_scan_to_gap_at_tip(300).unwrap_err();
        assert!(error.to_string().contains("rewind scan gap at 200"));
        let after: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(last_filled_island_end(&path, 101, 300), 200);
        assert_eq!(
            conn.query_row("SELECT block FROM transactions", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            200
        );
    }

    #[test]
    fn valid_empty_and_sparse_checkpoints_do_not_trigger_scan_wipe() {
        let (_dir, path, conn) = checkpoint_repair_fixture();
        conn.execute_batch("INSERT INTO orchard_tree_checkpoints VALUES (100, 9), (200, 9), (201, 9), (300, NULL), (400, 20);
            INSERT INTO blocks (height, hash, time, sapling_tree, orchard_commitment_tree_size) VALUES
                (200, zeroblob(32), 0, X'000000', 10), (300, zeroblob(32), 0, X'000000', 0),
                (400, zeroblob(32), 0, X'000000', NULL);
            INSERT INTO scan_queue (block_range_start,block_range_end,priority) VALUES (101,401,10);").unwrap();
        // Same-position empty block, sparse checkpoint without a row, genuinely
        // empty pool, and unknown legacy metadata all lack mismatch evidence.
        validate_empty_walk_scan(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM orchard_tree_checkpoints", [], |r| r
                .get::<_, u32>(
                0
            ))
            .unwrap(),
            5
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM scan_queue", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            1
        );
    }

    fn add_stale_checkpoint_fixture(conn: &rusqlite::Connection) {
        conn.execute_batch("INSERT INTO orchard_tree_checkpoints VALUES (100,9), (200,9);
            INSERT INTO orchard_tree_checkpoint_marks_removed VALUES (200,4);
            INSERT INTO orchard_tree_retained_checkpoints VALUES (200);
            INSERT INTO blocks (height,hash,time,sapling_tree,orchard_commitment_tree_size) VALUES
                (200,zeroblob(32),0,X'000000',12);
            INSERT INTO transactions (txid,block,mined_height,min_observed_height) VALUES
                (zeroblob(32),200,200,200);
            INSERT INTO scan_queue (block_range_start,block_range_end,priority) VALUES (101,201,10);").unwrap();
    }

    #[test]
    fn contradictory_checkpoint_preserves_scan_notes_and_tree_state() {
        let (_dir, path, conn) = checkpoint_repair_fixture();
        let mut db = open_wallet_db(&path, ZNetwork::Regtest).unwrap();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                0.into(),
                zcash_primitives::block::BlockHash([0; 32]),
            ),
            None,
        );
        db.create_account(
            "checkpoint fixture",
            &SecretVec::new(vec![7; 32]),
            &birthday,
            None,
        )
        .unwrap();
        drop(db);
        add_stale_checkpoint_fixture(&conn);
        conn.execute_batch(
            "INSERT INTO orchard_tree_shards
                (shard_index, subtree_end_height, root_hash, shard_data, contains_marked)
                VALUES (0, 200, zeroblob(32), X'00', 1);
             INSERT INTO orchard_received_notes
                (transaction_id, action_index, account_id, diversifier, value, rho, rseed,
                 is_change, memo, commitment_tree_position)
                VALUES ((SELECT id_tx FROM transactions LIMIT 1), 0,
                        (SELECT id FROM accounts LIMIT 1), zeroblob(11), 123,
                        zeroblob(32), zeroblob(32), 0, X'4d', 11);",
        )
        .unwrap();
        let before: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let error = validate_empty_walk_scan(&path).unwrap_err();
        assert!(error.to_string().contains("wallet data was retained"));
        let after: u32 = conn
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "validation must be read-only");
        for table in [
            "blocks",
            "scan_queue",
            "orchard_tree_checkpoints",
            "orchard_tree_checkpoint_marks_removed",
            "orchard_tree_retained_checkpoints",
            "orchard_tree_shards",
            "orchard_received_notes",
        ] {
            let expected = if table == "orchard_tree_checkpoints" {
                2
            } else {
                1
            };
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                    .get::<_, u32>(0))
                    .unwrap(),
                expected,
                "{table} changed after rejected checkpoint"
            );
        }
        assert_eq!(
            conn.query_row("SELECT block,mined_height FROM transactions", [], |r| Ok((
                r.get::<_, Option<u32>>(0)?,
                r.get::<_, u32>(1)?
            )))
            .unwrap(),
            (Some(200), 200)
        );
        assert!(conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
        assert!(validate_empty_walk_scan(&path).is_err());
    }

    #[test]
    fn checkpoint_validation_propagates_schema_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("invalid.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE unrelated(value INTEGER); INSERT INTO unrelated VALUES (7)",
        )
        .unwrap();
        assert!(validate_empty_walk_scan(&path).is_err());
        assert_eq!(
            conn.query_row("SELECT value FROM unrelated", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            7
        );
    }

    #[test]
    fn spend_policy_is_shielded_only() {
        let p = shielded_spend_policy();
        assert!(p.permits_shielded(ShieldedPool::Orchard));
    }

    #[test]
    fn public_lwd_cannot_query_transparent() {
        let pub_s = LightServer::Url("https://zec.rocks:443".into());
        assert!(!pub_s.allows_transparent_query());
        assert!(LightServer::LocalZaino.allows_transparent_query());
    }

    #[tokio::test]
    async fn public_lwd_utxos_rejected_without_network() {
        let err = NativeWallet::fetch_address_utxos(
            &LightServer::Url("https://zec.rocks:443".into()),
            &["tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6".into()],
            0,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("leaks t-addrs") || msg.contains("local Zaino"),
            "unexpected: {msg}"
        );
    }
}

#[cfg(all(test, feature = "transparent-inputs"))]
#[path = "wallet_broadcast_tests.rs"]
mod broadcast_tests;
