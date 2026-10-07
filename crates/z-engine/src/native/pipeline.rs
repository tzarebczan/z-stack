//! Native download/scan pipeline.
//!
//! Local Zaino: **6** overlapping `GetBlockRange` RPCs of **2000** heights on two
//! reused HTTP/2 channels — same fan-out as `z-wallet pipe`, not the HTTP pipe.
//! Prefetch, Rayon trial-decrypt of batches *k* and *k+1*, and sqlite persist
//! of *k-1* overlap. Decrypt never runs on the gRPC worker; persist uses
//! `block_in_place`. Desktop sync is a 2-worker multi-thread runtime so HTTP/2
//! stays polled. Wipe / sparse persist never calls `truncate_to_height` on a
//! height that is not a real `blocks` row.

use crate::error::{EngineError, Result};
use crate::native::block_cache::FsBlockCache;
use crate::native::block_cache::FsCacheError;
use crate::native::lwd::LwdClient;
use crate::native::selective_scan::{self, NativeOffload};
#[cfg(test)]
use crate::scan::HISTORIC_PERSIST_BLOCKS;
use crate::scan::{sync_tuning, PREFETCH_NATIVE_LOCAL, PREFETCH_PUBLIC, STREAM_DECRYPT_BLOCKS};
use crate::{Network as ZNetwork, SyncProgress, SyncStage};
use orchard::tree::MerkleHashOrchard;
use sapling::Node as SaplingNode;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};
use zcash_client_backend::{
    data_api::{
        chain::{
            error::Error as ChainError, scan_cached_blocks, BlockCache, BlockSource, ChainState,
            CommitmentTreeRoot,
        },
        scanning::{ScanPriority, ScanRange},
        WalletCommitmentTrees, WalletRead, WalletWrite,
    },
    proto::{
        compact_formats::CompactBlock,
        service::{BlockId, BlockRange as ProtoBlockRange, GetSubtreeRootsArg, ShieldedProtocol},
    },
};
use zcash_primitives::block::BlockHash;
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::BlockHeight;

pub(super) async fn run(
    client: &mut LwdClient,
    network: ZNetwork,
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    birthday: u32,
    light_url: &str,
    downloaded: &Arc<AtomicU32>,
    scanned: &Arc<AtomicU32>,
    progress: Option<Arc<Mutex<SyncProgress>>>,
    // The caller's tip, which it also checks completion against. Reading it
    // again here let a load-balanced server answer one block lower and fail
    // the finished sync as "incomplete".
    tip: u32,
) -> Result<()> {
    let tuning = sync_tuning(crate::uses_fast_sync(light_url));
    let skip_ironwood = super::wallet::NativeWallet::skip_ironwood_subtrees(network, light_url);
    super::wallet::validate_empty_walk_scan(db_path)?;
    let island = super::wallet::last_filled_island_end(db_path, birthday, tip);
    scanned.store(island, Ordering::Relaxed);
    if island < tip {
        // After `reset_scan` the blocks table is empty and island is birthday-1.
        // `truncate_to_height` on that frontier makes sqlite's 10-block Verify
        // lookahead the first range — stock `scan_cached_blocks` + GetTreeState
        // every 10 heights (live Sinsemilla). Cold birthday scan must skip that.
        // Sparse persist: island may be a scan_queue watermark (e.g. 3339870)
        // that was never a `blocks` row — never pass that to rewind.
        if island < birthday {
            info!("cold scan from birthday; skip truncate_to_height (avoids Verify lookahead)");
        } else if let Some(h) = truncate_existing(db, db_path, island)? {
            info!("re-queue from last filled island so holes below the chain-tip watermark are scanned");
            scanned.store(h, Ordering::Relaxed);
        } else {
            info!("sparse/empty blocks; skip truncate_to_height (no row at watermark)");
        }
    }
    db.update_chain_tip(BlockHeight::from_u32(tip))
        .map_err(|e| EngineError::WalletDb(format!("update_chain_tip: {e}")))?;
    // Compact-block download starts during GetSubtreeRoots. Do not pin the
    // overlay on Connecting through roots / GetAddressUtxos — after a birthday
    // wipe that looked like 4% / trial-decrypt at the birthday height.
    note(
        &progress,
        SyncStage::Downloading,
        format!("downloading compact blocks from {light_url}"),
    );
    let local = crate::uses_fast_sync(light_url);
    let chunk = apply_chunk_width(local);
    let prefetch_n = initial_prefetch(local).max(tuning.prefetch as usize);
    // First fetch chunk must start on the client that already did GetLatestBlock.
    // Two extra HTTP/2 handshakes used to run *before* any GetBlockRange, and
    // OS-thread `.join()` on this current-thread runtime starved the tonic
    // driver — overlay stayed at birthday with 3 live sockets and 0 heights.
    let warmup = if island < birthday {
        let chunks = historic_fetch_chunks(Vec::new(), island, tip, chunk);
        if let Some((_start, _end)) = first_apply_range(Vec::new(), island, tip) {
            info!("first GetBlockRange on live channel (parallel with roots)");
        }
        Some(chunks)
    } else {
        None
    };
    let mut pool = FetchPool::start(
        light_url,
        skip_ironwood,
        client,
        Arc::clone(downloaded),
        prefetch_n,
        warmup,
    )
    .await;
    let have = subtree_have(db, skip_ironwood)?;
    let first_prior = if island < birthday {
        birthday.saturating_sub(1)
    } else {
        island
    };
    let (tree_state_job, roots_job) = spawn_state_and_roots(
        pool.control.clone(),
        light_url.to_string(),
        skip_ironwood,
        progress.clone(),
        have,
        first_prior,
    );
    let mut tree_state_job = Some(tree_state_job);
    let mut roots_job = Some(roots_job);
    let mut selective_scan = native_offload(have);
    let mut cached_state = None;

    let mut gap_retries = 0u8;
    let mut link_rewinds = 0u32;
    loop {
        let ranges = db
            .suggest_scan_ranges()
            .map_err(|e| EngineError::WalletDb(format!("suggest_scan_ranges: {e}")))?;
        let scanned_h = scanned.load(Ordering::Relaxed);
        let island = super::wallet::last_filled_island_end(db_path, birthday, tip);
        // `update_chain_tip` after GetSubtreeRoots often leaves suggest empty or
        // only the incomplete tip shard. Inject island+1→tip so historic holes
        // below the displayed watermark are still trial-decrypted.
        // sqlite's 10-block Verify lookahead 54k below tip must not go through
        // stock `scan_cached_blocks` (live Sinsemilla every orchard action).
        let ranges = coalesce_from_low_height(
            cover_gap_from(ranges, island, tip)
                .into_iter()
                .map(|r| demote_far_verify(r, tip))
                .collect(),
        );
        if ranges.is_empty() {
            break;
        }
        info!("first scan range");
        // Near-tip reorg window only. Far-from-tip Verify (birthday wipe /
        // sqlite's 10-block lookahead) is demoted above; never stock-scan it.
        if ranges[0].priority() == ScanPriority::Verify && !far_from_tip_range(&ranges[0], tip) {
            note(
                &progress,
                SyncStage::Downloading,
                format!("verifying recent blocks from {light_url}"),
            );
            let _ = apply_range(
                client,
                network,
                cache,
                db,
                db_path,
                &ranges[0],
                downloaded,
                scanned,
                &mut selective_scan,
            )
            .await?;
            continue;
        }
        let batches: Vec<_> = flatten_ranges(ranges, chunk)
            .into_iter()
            .filter_map(|r| clip_already_scanned(r, island))
            .collect();
        if batches.is_empty() {
            break;
        }
        if scanned_h <= birthday {
            note(
                &progress,
                SyncStage::Downloading,
                format!("downloading compact blocks from {light_url}"),
            );
        }
        try_ingest_roots(db, &mut roots_job, &mut selective_scan);
        let mut control = pool.control.clone();
        let invalidated = historic(
            &mut control,
            network,
            cache,
            db,
            db_path,
            batches,
            &mut pool,
            downloaded,
            scanned,
            &mut selective_scan,
            &mut cached_state,
            &mut tree_state_job,
            &mut roots_job,
            &progress,
            &mut link_rewinds,
        )
        .await?;
        if invalidated {
            pool.drain();
            pool.ranges.clear();
            pool.next = 0;
            cached_state = None;
            // A rewind deletes shard rows, including roots, at or above its
            // position; a restart keeps them. Only stored roots may drop shards.
            selective_scan = rebuilt_offload(db, skip_ironwood, selective_scan.root_ranges())?;
            continue;
        }
        let leftover = db
            .suggest_scan_ranges()
            .map_err(|e| EngineError::WalletDb(format!("suggest_scan_ranges: {e}")))?;
        let island = super::wallet::last_filled_island_end(db_path, birthday, tip);
        if leftover.is_empty() {
            if island >= tip {
                break;
            }
            gap_retries = gap_retries.saturating_add(1);
            if gap_retries >= 2 {
                break;
            }
            continue;
        }
        gap_retries = 0;
        if leftover[0].priority() == ScanPriority::Verify && !far_from_tip_range(&leftover[0], tip)
        {
            continue;
        }
        let leftover = cover_gap_from(leftover, island, tip);
        let leftover = coalesce_from_low_height(
            leftover
                .into_iter()
                .map(|r| demote_far_verify(r, tip))
                .collect(),
        );
        if leftover
            .into_iter()
            .filter_map(|r| clip_already_scanned(r, island))
            .next()
            .is_none()
        {
            break;
        }
        continue;
    }
    finish_roots(db, &mut roots_job, &mut selective_scan).await;
    if crate::is_loopback_light_url(light_url) {
        // After compact scan. Blocking GetAddressUtxos before the first
        // GetBlockRange left the birthday overlay at 4% with no heights.
        if let Err(_e) = refresh_utxos(client, network, db).await {
            warn!("GetAddressUtxos after scan failed");
        }
    }
    let filled = super::wallet::last_filled_island_end(db_path, birthday, tip);
    if filled < tip {
        return Err(EngineError::Message(format!(
            "sync incomplete: filled island {filled} < tip {tip}"
        )));
    }
    Ok(())
}

/// Drop the contiguous prefix sqlite already has. `update_chain_tip` after a
/// full `GetSubtreeRoots` dump can re-queue the incomplete tip shard from its
/// last complete-shard end — tens of thousands of blocks already in `blocks`.
///
/// Do **not** drop Historic/ChainTip ranges that sit entirely *below* the
/// high-watermark. selective shard scanning + a mid-range island leaves a birthday hole
/// (e.g. 3_335_466..3_390_009 while checkpoints sit at 3_418_128). Treating
/// that hole as "already scanned" is how desktop finishes at 0 ZEC while WASM
/// found the notes.
fn clip_already_scanned(range: ScanRange, scanned: u32) -> Option<ScanRange> {
    let start = u32::from(range.block_range().start);
    let end = u32::from(range.block_range().end);
    if end <= scanned.saturating_add(1) {
        if range.priority() > ScanPriority::Scanned {
            return Some(range);
        }
        return None;
    }
    if start > scanned {
        return Some(range);
    }
    let cut = BlockHeight::from_u32(scanned.saturating_add(1));
    range.split_at(cut).map(|(_, rest)| rest)
}

/// `update_chain_tip` after GetSubtreeRoots often queues only the incomplete tip
/// shard. That skips the hole between the last filled sqlite island and that
/// shard (notes at 3_424_719 while the island ends at 3_418_128).
fn cover_gap_from(ranges: Vec<ScanRange>, island_end: u32, tip: u32) -> Vec<ScanRange> {
    let from = island_end.saturating_add(1);
    if from > tip {
        return ranges;
    }
    let gap_end_excl = tip.saturating_add(1);
    let covered = ranges.iter().any(|r| {
        let start = u32::from(r.block_range().start);
        let end = u32::from(r.block_range().end);
        start <= from && end > from
    });
    if covered {
        return ranges;
    }
    let cut = ranges
        .iter()
        .map(|r| u32::from(r.block_range().start))
        .filter(|start| *start > from)
        .min()
        .unwrap_or(gap_end_excl)
        .min(gap_end_excl);
    if cut <= from {
        return ranges;
    }
    let gap = ScanRange::from_parts(from.into()..cut.into(), ScanPriority::ChainTip);
    let mut out = vec![gap];
    out.extend(ranges);
    out
}

/// sqlite marks the next 10 heights after a rewind as `Verify`. Far from tip that
/// is not a reorg check — it forces stock `scan_cached_blocks` (live Sinsemilla)
/// at ~10 heights per GetTreeState. Demote it so selective shard scanning gap-fill / birthday
/// wipe can batch 4000.
fn demote_far_verify(range: ScanRange, tip: u32) -> ScanRange {
    if range.priority() != ScanPriority::Verify {
        return range;
    }
    if far_from_tip_range(&range, tip) {
        ScanRange::from_parts(range.block_range().clone(), ScanPriority::ChainTip)
    } else {
        range
    }
}

fn far_from_tip_range(range: &ScanRange, tip: u32) -> bool {
    let end_incl = u32::from(range.block_range().end).saturating_sub(1);
    tip.saturating_sub(end_incl) > crate::NEAR_TIP_BLOCKS
}

fn coalesce_from_low_height(ranges: Vec<ScanRange>) -> Vec<ScanRange> {
    let mut ranges = ranges;
    ranges.sort_by_key(|r| {
        (
            u32::from(r.block_range().start),
            u32::from(r.block_range().end),
        )
    });
    coalesce_scan_ranges(ranges)
}

fn apply_chunk_width(local: bool) -> u32 {
    if local {
        crate::scan::FETCH_CHUNK_LOCAL
    } else {
        crate::scan::BATCH_PUBLIC
    }
}

fn initial_prefetch(local: bool) -> usize {
    if local {
        PREFETCH_NATIVE_LOCAL as usize
    } else {
        PREFETCH_PUBLIC as usize
    }
}

fn historic_fetch_chunks(
    suggest: Vec<ScanRange>,
    island: u32,
    tip: u32,
    chunk: u32,
) -> Vec<ScanRange> {
    flatten_ranges(
        coalesce_from_low_height(
            cover_gap_from(suggest, island, tip)
                .into_iter()
                .map(|r| demote_far_verify(r, tip))
                .collect(),
        ),
        chunk,
    )
    .into_iter()
    .filter_map(|r| clip_already_scanned(r, island))
    .collect()
}

/// First apply unit after sqlite `suggest_scan_ranges`.
/// Empty birthday reset must start at birthday, in bounded fetch chunks — apply
/// starts when that chunk is on disk, not after a 4000-block selective shard scanning batch.
fn first_apply_range(suggest: Vec<ScanRange>, island: u32, tip: u32) -> Option<(u32, u32)> {
    let batches = historic_fetch_chunks(suggest, island, tip, crate::scan::FETCH_CHUNK_LOCAL);
    let first = batches.first()?;
    Some((
        u32::from(first.block_range().start),
        u32::from(first.block_range().end).saturating_sub(1),
    ))
}

/// First native GetBlockRange after sqlite `suggest_scan_ranges`.
/// Empty birthday reset must start at birthday, in bounded fetch chunks, even when
/// the queue is ChainTip-first or a far-from-tip Verify-10 lookahead.
#[cfg(test)]
fn first_get_block_range(suggest: Vec<ScanRange>, island: u32, tip: u32) -> Option<(u32, u32)> {
    first_apply_range(suggest, island, tip)
}

fn coalesce_scan_ranges(ranges: Vec<ScanRange>) -> Vec<ScanRange> {
    let mut out: Vec<ScanRange> = Vec::new();
    for r in ranges {
        if r.is_empty() {
            continue;
        }
        if let Some(prev) = out.last() {
            // Adjacent *or* overlapping (sqlite Verify-10 sits inside the
            // Historic birthday hole). Only touching at end==start used to
            // leave a 10-block first GetBlockRange after a wipe.
            if prev.block_range().end >= r.block_range().start {
                let pri = prev.priority().max(r.priority());
                let start = prev.block_range().start.min(r.block_range().start);
                let end = prev.block_range().end.max(r.block_range().end);
                out.pop();
                out.push(ScanRange::from_parts(start..end, pri));
                continue;
            }
        }
        out.push(r);
    }
    out
}

fn flatten_ranges(ranges: Vec<ScanRange>, batch: u32) -> Vec<ScanRange> {
    let mut out = Vec::new();
    for r in ranges {
        let mut acc = r;
        loop {
            if acc.is_empty() {
                break;
            }
            let cut = acc.block_range().start + batch;
            if let Some((cur, next)) = acc.split_at(cut) {
                out.push(cur);
                acc = next;
            } else {
                out.push(acc);
                break;
            }
        }
    }
    out
}

type LiveBlocks = Arc<Mutex<Vec<CompactBlock>>>;

struct FetchJob {
    range: ScanRange,
    live: LiveBlocks,
    handle: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for FetchJob {
    fn drop(&mut self) {
        // JoinHandle normally detaches. A failed/cancelled scan must release
        // both queued downloads and a popped job that never reached its await.
        self.handle.abort();
    }
}

struct FetchPool {
    fetch_a: LwdClient,
    fetch_b: Option<LwdClient>,
    /// Tree states and subtree roots: small replies on the scan's critical
    /// path. On a slow link a reply sharing a connection with compact blocks
    /// queues behind megabytes of them (a scan-start GetTreeState timed out
    /// three times at 100 KB/s); its own connection gets a fair share.
    control: LwdClient,
    fetch_n: usize,
    fetch_rr: std::sync::atomic::AtomicUsize,
    downloaded: Arc<AtomicU32>,
    ranges: Vec<ScanRange>,
    next: usize,
    prefetch: usize,
    q: VecDeque<FetchJob>,
}

impl FetchPool {
    /// Start compact-block tasks on the already-live client, then add a second
    /// hot channel. Extra connects must not precede the first GetBlockRange.
    async fn start(
        light_url: &str,
        skip_ironwood: bool,
        fallback: &LwdClient,
        downloaded: Arc<AtomicU32>,
        prefetch: usize,
        warmup: Option<Vec<ScanRange>>,
    ) -> Self {
        let mut pool = Self {
            fetch_a: fallback.clone(),
            fetch_b: None,
            control: fallback.clone(),
            fetch_n: 1,
            fetch_rr: std::sync::atomic::AtomicUsize::new(0),
            downloaded,
            ranges: Vec::new(),
            next: 0,
            prefetch: prefetch.max(1),
            q: VecDeque::new(),
        };
        if let Some(chunks) = warmup {
            pool.set_ranges(chunks);
            if let Err(_e) = pool.fill() {
                warn!("warmup prefetch failed");
            }
        }
        let connect = || super::wallet::NativeWallet::connect_url_hot(light_url, skip_ironwood);
        let (second, control) = tokio::join!(
            async {
                if prefetch > 1 {
                    Some(connect().await)
                } else {
                    None
                }
            },
            connect()
        );
        match second {
            Some(Ok(c)) => {
                pool.fetch_b = Some(c);
                pool.fetch_n = 2;
            }
            Some(Err(_e)) => warn!("second fetch channel failed; staying on one"),
            None => {}
        }
        match control {
            Ok(c) => pool.control = c,
            Err(_e) => warn!("control channel failed; tree states share block fetches"),
        }
        pool
    }

    fn set_ranges(&mut self, ranges: Vec<ScanRange>) {
        self.ranges = ranges;
        self.next = 0;
    }

    fn adopt_ranges(&mut self, ranges: Vec<ScanRange>) {
        if self.q.is_empty() && self.next == 0 {
            self.ranges = ranges;
            return;
        }
        let have0 = self
            .ranges
            .first()
            .map(|r| u32::from(r.block_range().start));
        let want0 = ranges.first().map(|r| u32::from(r.block_range().start));
        if have0.is_some() && have0 == want0 {
            self.ranges = ranges;
            if self.next > self.ranges.len() {
                self.next = self.ranges.len();
            }
            return;
        }
        self.drain();
        self.next = 0;
        self.ranges = ranges;
    }

    fn enqueue(&self, range: ScanRange) -> Result<FetchJob> {
        let slot = self
            .fetch_rr
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.fetch_n;
        let mut c = if slot == 1 {
            self.fetch_b.as_ref().unwrap_or(&self.fetch_a).clone()
        } else {
            self.fetch_a.clone()
        };
        let r = range.clone();
        let dl = Arc::clone(&self.downloaded);
        let live: LiveBlocks = Arc::new(Mutex::new(Vec::new()));
        let live_job = Arc::clone(&live);
        // Never `std::thread::join` a tonic clone — that starved the driver.
        Ok(FetchJob {
            range,
            live,
            handle: tokio::spawn(
                async move { fetch_range_into(&mut c, &r, Some(dl), live_job).await },
            ),
        })
    }

    fn fill(&mut self) -> Result<()> {
        while self.q.len() < self.prefetch && self.next < self.ranges.len() {
            let range = self.ranges[self.next].clone();
            self.next += 1;
            match self.enqueue(range) {
                Ok(job) => self.q.push_back(job),
                Err(e) => {
                    self.drain();
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<FetchJob> {
        self.q.pop_front()
    }

    fn drain(&mut self) {
        for job in self.q.drain(..) {
            job.handle.abort();
        }
    }
}

fn native_offload(have: SubtreeHave) -> NativeOffload {
    NativeOffload::new(
        u64::from(have.sap),
        u64::from(have.orch),
        u64::from(have.iron),
    )
}

/// Fresh offload after `historic` gave up its state. Keep each learned root
/// range only up to the first shard whose root SQLite no longer holds.
fn rebuilt_offload(
    db: &mut super::wallet::SyncDb,
    skip_ironwood: bool,
    learned: [(u64, u64); 3],
) -> Result<NativeOffload> {
    fn stored(
        (start, end): (u64, u64),
        mut present: impl FnMut(u64) -> Result<bool>,
    ) -> Result<Option<(u64, u64)>> {
        let mut last = start;
        while last < end && present(last)? {
            last += 1;
        }
        Ok((last > start).then_some((start, last)))
    }
    let mut offload = native_offload(subtree_have(db, skip_ironwood)?);
    let sapling = stored(learned[0], |i| {
        db.get_sapling_subtree_root(i)
            .map(|root| root.is_some())
            .map_err(|e| EngineError::WalletDb(format!("sapling shard {i}: {e}")))
    })?;
    let orchard = stored(learned[1], |i| {
        db.get_orchard_subtree_root(i)
            .map(|root| root.is_some())
            .map_err(|e| EngineError::WalletDb(format!("orchard shard {i}: {e}")))
    })?;
    let ironwood = if skip_ironwood {
        None
    } else {
        stored(learned[2], |i| {
            db.get_ironwood_subtree_root(i)
                .map(|root| root.is_some())
                .map_err(|e| EngineError::WalletDb(format!("ironwood shard {i}: {e}")))
        })?
    };
    offload.roots_available(sapling, orchard, ironwood);
    Ok(offload)
}

/// Fetch the scan-start tree state once and hand it to the scanner, then ask
/// only for roots this scan can use. Shards before that tree size are never fed
/// to selective shard scanning. Zaino 0.10 resolves a full block for every root under a 120 s
/// stream deadline, so asking mainnet for every root from shard 0 never finishes
/// and leaves selective shard scanning hashing every commitment.
fn spawn_state_and_roots(
    mut client: LwdClient,
    light_url: String,
    skip_ironwood: bool,
    progress: Option<Arc<Mutex<SyncProgress>>>,
    have: SubtreeHave,
    prior: u32,
) -> (tokio::task::JoinHandle<Result<ChainState>>, RootsJob) {
    let (state_tx, state_rx) = tokio::sync::oneshot::channel();
    let state = tokio::spawn(async move {
        state_rx.await.unwrap_or_else(|_| {
            Err(EngineError::Transport(
                "GetTreeState task ended early".into(),
            ))
        })
    });
    let (roots_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let fetched = fetch_tree_state(&mut client, prior).await;
        let start = match &fetched {
            Ok(state) => roots_start(have, selective_scan::tree_sizes(state)),
            Err(_) => have,
        };
        let _ = state_tx.send(fetched);
        fetch_subtree_root_vecs(
            &mut client,
            &light_url,
            skip_ironwood,
            &progress,
            start,
            roots_tx,
        )
        .await
    });
    let awaiting = if skip_ironwood { 2 } else { 3 };
    (state, RootsJob { rx, task, awaiting })
}

/// First root worth requesting per pool: the shard holding the next leaf,
/// unless SQLite already has a longer contiguous prefix.
fn roots_start(have: SubtreeHave, sizes: (u32, u32, u32)) -> SubtreeHave {
    let shard = |size: u32| size >> crate::offload::SHARD_HEIGHT;
    SubtreeHave {
        sap: have.sap.max(shard(sizes.0)),
        orch: have.orch.max(shard(sizes.1)),
        iron: have.iron.max(shard(sizes.2)),
    }
}

enum ChainStateSrc {
    Cached(ChainState),
    Walk(ChainState),
    Fetch,
}

/// Adopt GetTreeState only when we have no walk yet. After birthday / first
/// persist, empty-block skips must not fetch a later height (that checkpoints
/// the birthday orchard size at a note height).
fn resolve_chain_state(cached: Option<&ChainState>, prior: BlockHeight) -> ChainStateSrc {
    match cached {
        Some(cs) if cs.block_height() == prior => ChainStateSrc::Cached(cs.clone()),
        Some(cs) => ChainStateSrc::Walk(cs.clone()),
        None => ChainStateSrc::Fetch,
    }
}

#[cfg(test)]
fn should_fetch_tree_state(cached_height: Option<u32>, _prior: u32) -> bool {
    cached_height.is_none()
}

async fn take_tree_state(
    cached: &mut Option<ChainState>,
    job: &mut Option<tokio::task::JoinHandle<Result<ChainState>>>,
) {
    if cached.is_some() {
        return;
    }
    let Some(handle) = job.take() else {
        return;
    };
    match handle.await {
        Ok(Ok(cs)) => *cached = Some(cs),
        Ok(Err(_e)) => warn!("warmup GetTreeState failed"),
        Err(_) => warn!("GetTreeState task panicked"),
    }
}

/// A pool's roots arrive as contiguous runs in shard order, so early shards
/// can drop before later roots resolve; `Done` ends that pool, successful or not.
enum PoolRoots {
    Sapling(u32, Vec<CommitmentTreeRoot<SaplingNode>>),
    Orchard(u32, Vec<CommitmentTreeRoot<MerkleHashOrchard>>),
    Ironwood(u32, Vec<CommitmentTreeRoot<MerkleHashOrchard>>),
    Done,
}

/// Roots for one sync. Dropping it stops the remaining server lookups.
struct RootsJob {
    rx: tokio::sync::mpsc::UnboundedReceiver<PoolRoots>,
    task: tokio::task::JoinHandle<()>,
    /// Pools that have not reported yet.
    awaiting: usize,
}

impl Drop for RootsJob {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How long a finished scan waits for roots still in flight. Roots only let
/// selective shard scanning skip hashing, so after the scan they save nothing for this sync,
/// and the next sync asks from its own scan-start shard.
const ROOTS_FINAL_GRACE: Duration = Duration::from_secs(2);

fn try_ingest_roots(
    db: &mut super::wallet::SyncDb,
    job: &mut Option<RootsJob>,
    selective_scan: &mut NativeOffload,
) {
    let Some(roots) = job.as_mut() else {
        return;
    };
    while let Ok(pool) = roots.rx.try_recv() {
        match pool {
            PoolRoots::Done => roots.awaiting = roots.awaiting.saturating_sub(1),
            pool => ingest_roots(db, selective_scan, pool),
        }
    }
}

/// Stop Zaino's per-root block lookups once the scan no longer needs them.
async fn finish_roots(
    db: &mut super::wallet::SyncDb,
    job: &mut Option<RootsJob>,
    selective_scan: &mut NativeOffload,
) {
    let Some(mut roots) = job.take() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + ROOTS_FINAL_GRACE;
    loop {
        match tokio::time::timeout_at(deadline, roots.rx.recv()).await {
            Ok(Some(PoolRoots::Done)) => {
                roots.awaiting = roots.awaiting.saturating_sub(1);
                if roots.awaiting == 0 {
                    return;
                }
            }
            Ok(Some(pool)) => ingest_roots(db, selective_scan, pool),
            Ok(None) => return,
            Err(_) => {
                info!("subtree roots abandoned after scan");
                return;
            }
        }
    }
}

fn ingest_roots(
    db: &mut super::wallet::SyncDb,
    selective_scan: &mut NativeOffload,
    pool: PoolRoots,
) {
    let ranges = run_blocking_apply(|| put_pool_roots(db, pool));
    selective_scan.roots_available(ranges.sapling, ranges.orchard, ranges.ironwood);

    info!("subtree roots wallet write");
}

async fn historic(
    client: &mut LwdClient,
    network: ZNetwork,
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    batches: Vec<ScanRange>,
    pool: &mut FetchPool,
    downloaded: &Arc<AtomicU32>,
    scanned: &Arc<AtomicU32>,
    selective_scan: &mut NativeOffload,
    cached_state: &mut Option<ChainState>,
    tree_state_job: &mut Option<tokio::task::JoinHandle<Result<ChainState>>>,
    roots_job: &mut Option<RootsJob>,
    progress: &Option<Arc<Mutex<SyncProgress>>>,
    link_rewinds: &mut u32,
) -> Result<bool> {
    pool.adopt_ranges(batches);
    pool.fill()?;
    let mut context = selective_scan::TrialContext::new(
        selective_scan::load_ufvks(db)?,
        selective_scan::load_nullifiers(db)?,
    );
    let mut pending: Option<selective_scan::DecryptedBatch> = None;
    let mut pending_range: Option<ScanRange> = None;
    let mut inflight: Option<InflightTrial> = None;
    // Commitment-tree sizes for the next decrypt. Advanced from compact
    // metadata on note-free batches so those batches skip frontier hashing.
    let mut size_cursor: Option<(u32, u32, u32)> = None;
    let mut prefix_job: Option<StreamedTrial> = None;
    // Block each fetched range must extend. `scan_block` runs without a prior
    // block here, so this is the only reorg check on the selective shard scanning path.
    let mut parent: Option<(BlockHeight, BlockHash)> = None;
    let result = async {
        while let Some(job) = pool.pop() {
            if let Err(e) = pool.fill() {
                job.handle.abort();
                return Err(e);
            }
            try_ingest_roots(db, roots_job, selective_scan);
            let range = job.range.clone();
            let start = range.block_range().start;
            let end_incl = u32::from(range.block_range().end).saturating_sub(1);
            let prior = start - 1;
            take_tree_state(cached_state, tree_state_job).await;
            if cached_state
                .as_ref()
                .is_some_and(|cs| cs.block_height() != prior)
            {
                if let Some(prev) = inflight.take() {
                    match finish_inflight(
                        prev,
                        client,
                        network,
                        db,
                        &context,
                        scanned,
                        cache,
                        db_path,
                        cached_state,
                        selective_scan,
                        pool,
                    )
                    .await?
                    {
                        InflightOut::Rewind => return Ok(true),
                        InflightOut::Batch(decrypted, prev_range) => {
                            size_cursor = Some(decrypted.end_sizes);
                            absorb_pending(&mut pending, &mut pending_range, decrypted, prev_range);
                        }
                    }
                }
            }
            let from_state = match resolve_chain_state(cached_state.as_ref(), prior) {
                ChainStateSrc::Cached(cs) | ChainStateSrc::Walk(cs) => cs,
                ChainStateSrc::Fetch => {
                    info!("fetching GetTreeState once at scan start");
                    let cs = fetch_tree_state(client, u32::from(prior)).await?;
                    size_cursor = Some(selective_scan::tree_sizes(&cs));
                    cs
                }
            };
            let cursor = size_cursor.unwrap_or_else(|| selective_scan::tree_sizes(&from_state));
            size_cursor = Some(cursor);
            let parent_hash = match parent {
                Some((height, hash)) if height == prior => Some(hash),
                _ => known_parent(db, prior, &from_state)?,
            };

            if pending
                .as_ref()
                .is_some_and(|p| notes_force_serial_persist(p.had_notes))
            {
                match persist_pending(
                    db,
                    selective_scan,
                    scanned,
                    cached_state,
                    &mut pending,
                    &mut pending_range,
                    progress,
                    &mut context,
                )? {
                    Step::Rewind => {
                        pool.drain();
                        selective_scan.reset();
                        return Ok(true);
                    }
                    Step::Restart => {
                        pool.drain();
                        selective_scan::flush_remaining(db, selective_scan)?;
                        return Ok(true);
                    }
                    Step::Continue => {}
                }
            }

            note_pipeline(progress, true, pending.is_some());

            let prefix =
                wait_stream_prefix(&job.live, &job.handle, STREAM_DECRYPT_BLOCKS as usize).await;
            info!("compact prefix wait");
            prefix_job = match prefix {
                Some(blocks) if !blocks.is_empty() => Some(spawn_streamed_trial(
                    network,
                    context.snapshot(db, &blocks)?,
                    cursor,
                    start,
                    blocks,
                )?),
                _ => None,
            };

            let blocks = take_fetched(job).await?;
            info!("compact fetch consumed");
            mark_downloaded(downloaded, &blocks);
            expect_contiguous_range(&blocks, u32::from(start), end_incl)?;
            if !extends_parent(&blocks, parent_hash.as_ref()) {
                *link_rewinds += 1;
                if *link_rewinds > MAX_LINK_REWINDS {
                    return Err(EngineError::Message(format!(
                        "blocks from the light server at {start} still do not extend this \
                         wallet's chain after {MAX_LINK_REWINDS} rewinds"
                    )));
                }
                info!("fetched range does not extend its parent; rewinding");
                rewind_continuity(
                    cache,
                    db,
                    db_path,
                    scanned,
                    cached_state,
                    selective_scan::Continuity::Rewind(start),
                    selective_scan,
                    pool,
                )
                .await?;
                return Ok(true);
            }
            parent = blocks
                .last()
                .and_then(|b| BlockHash::try_from_slice(&b.hash))
                .map(|hash| (BlockHeight::from_u32(end_incl), hash));

            let mut stale_nfs = false;
            if let Some(prev) = inflight.take() {
                match finish_inflight(
                    prev,
                    client,
                    network,
                    db,
                    &context,
                    scanned,
                    cache,
                    db_path,
                    cached_state,
                    selective_scan,
                    pool,
                )
                .await?
                {
                    InflightOut::Rewind => return Ok(true),
                    InflightOut::Batch(decrypted, prev_range) => {
                        stale_nfs = decrypted.had_notes;
                        size_cursor = Some(decrypted.end_sizes);
                        absorb_pending(&mut pending, &mut pending_range, decrypted, prev_range);
                    }
                }
            }

            if stale_nfs {
                match persist_pending(
                    db,
                    selective_scan,
                    scanned,
                    cached_state,
                    &mut pending,
                    &mut pending_range,
                    progress,
                    &mut context,
                )? {
                    Step::Rewind => {
                        pool.drain();
                        selective_scan.reset();
                        return Ok(true);
                    }
                    Step::Restart => {
                        pool.drain();
                        selective_scan::flush_remaining(db, selective_scan)?;
                        return Ok(true);
                    }
                    Step::Continue => {}
                }
                if let Some(pj) = prefix_job.take() {
                    let _ = join_trial(pj.handle).await;
                }
            }

            let decrypted = if let Some(prefix_job) = prefix_job.take() {
                match finish_streamed(
                    prefix_job, blocks, from_state, cursor, start, client, network, db, &context,
                    scanned,
                )
                .await?
                {
                    Err(c) => {
                        return rewind_continuity(
                            cache,
                            db,
                            db_path,
                            scanned,
                            cached_state,
                            c,
                            selective_scan,
                            pool,
                        )
                        .await
                        .map(|_| true);
                    }
                    Ok(d) => {
                        info!("streamed decrypt and frontier finalize");
                        d
                    }
                }
            } else {
                let handle = spawn_trial(
                    network,
                    context.snapshot(db, &blocks)?,
                    cursor,
                    start,
                    blocks,
                )?;
                inflight = Some(InflightTrial {
                    range: range.clone(),
                    from_state,
                    start_sizes: cursor,
                    handle,
                });
                if historic_pending_ready(&pending) {
                    match persist_pending(
                        db,
                        selective_scan,
                        scanned,
                        cached_state,
                        &mut pending,
                        &mut pending_range,
                        progress,
                        &mut context,
                    )? {
                        Step::Rewind => {
                            pool.drain();
                            selective_scan.reset();
                            return Ok(true);
                        }
                        Step::Restart => {
                            pool.drain();
                            selective_scan::flush_remaining(db, selective_scan)?;
                            return Ok(true);
                        }
                        Step::Continue => {}
                    }
                }
                continue;
            };

            size_cursor = Some(decrypted.end_sizes);
            if decrypted.had_notes {
                *cached_state = Some(decrypted.next_state.clone());
            }
            absorb_pending(&mut pending, &mut pending_range, decrypted, range);
            // The streamed-prefix path does not create an `inflight` trial. It
            // must still honor the same historic persist bound as the
            // full-range path below; otherwise a series of streamed chunks
            // without wallet notes accumulates until the end of the scan and
            // turns one SQLite write into a long, memory-heavy pause.
            if historic_pending_ready(&pending) {
                match persist_pending(
                    db,
                    selective_scan,
                    scanned,
                    cached_state,
                    &mut pending,
                    &mut pending_range,
                    progress,
                    &mut context,
                )? {
                    Step::Rewind => {
                        pool.drain();
                        selective_scan.reset();
                        return Ok(true);
                    }
                    Step::Restart => {
                        pool.drain();
                        selective_scan::flush_remaining(db, selective_scan)?;
                        return Ok(true);
                    }
                    Step::Continue => {}
                }
            }
        }
        if let Some(prev) = inflight.take() {
            match finish_inflight(
                prev,
                client,
                network,
                db,
                &context,
                scanned,
                cache,
                db_path,
                cached_state,
                selective_scan,
                pool,
            )
            .await?
            {
                InflightOut::Rewind => return Ok(true),
                InflightOut::Batch(decrypted, prev_range) => {
                    absorb_pending(&mut pending, &mut pending_range, decrypted, prev_range);
                }
            }
        }
        if let Some(prev) = pending.take() {
            let prev_range = pending_range.take().expect("pending range");

            match persist_decrypted(db, &prev_range, prev, selective_scan, scanned, cached_state)? {
                Step::Rewind => {
                    pool.drain();
                    selective_scan.reset();
                    return Ok(true);
                }
                Step::Restart => {
                    pool.drain();
                    selective_scan::flush_remaining(db, selective_scan)?;
                    return Ok(true);
                }
                Step::Continue => {}
            }
            info!("final historic wallet write");
        }

        selective_scan::flush_remaining(db, selective_scan)?;
        info!("historic checkpoint flush");
        Ok(false)
    }
    .await;
    join_outstanding_trials(prefix_job.take(), inflight.take()).await;
    result
}

/// A chain that keeps failing to link means a broken server, not a reorg;
/// stop before rewinds walk far back through committed scan progress.
const MAX_LINK_REWINDS: u32 = 10;

/// Parent below a range whose predecessor was not fetched in this pass: the
/// stored block row, or the tree state grafted at exactly that height.
fn known_parent(
    db: &super::wallet::SyncDb,
    prior: BlockHeight,
    from_state: &ChainState,
) -> Result<Option<BlockHash>> {
    if let Some(meta) = db
        .block_metadata(prior)
        .map_err(|e| EngineError::WalletDb(format!("block metadata at {prior}: {e}")))?
    {
        return Ok(Some(meta.block_hash()));
    }
    Ok((from_state.block_height() == prior).then(|| from_state.block_hash()))
}

/// The first fetched block must name the known parent. Unknown parents pass;
/// links inside a range are checked by `expect_contiguous_range`.
fn extends_parent(blocks: &[CompactBlock], parent: Option<&BlockHash>) -> bool {
    match (parent, blocks.first()) {
        (Some(parent), Some(first)) => first.prev_hash.as_slice() == parent.0.as_slice(),
        _ => true,
    }
}

struct InflightTrial {
    range: ScanRange,
    from_state: ChainState,
    /// Tree sizes the decrypt actually used (metadata cursor, not a stale frontier).
    start_sizes: (u32, u32, u32),
    handle: std::thread::JoinHandle<
        Result<std::result::Result<selective_scan::TrialBatch, selective_scan::Continuity>>,
    >,
}

enum InflightOut {
    Rewind,
    Batch(selective_scan::DecryptedBatch, ScanRange),
}

/// Every normal exit, including error and restart, waits for speculative jobs.
/// Their worker-local work never writes the ordered progress atomic, so even
/// cancellation cannot raise a cursor after a continuity rewind.
async fn join_outstanding_trials(prefix: Option<StreamedTrial>, inflight: Option<InflightTrial>) {
    if let Some(prefix) = prefix {
        let _ = join_trial(prefix.handle).await;
    }
    if let Some(inflight) = inflight {
        let _ = join_trial(inflight.handle).await;
    }
}

fn spawn_trial(
    network: ZNetwork,
    input: selective_scan::TrialInput,
    sizes: (u32, u32, u32),
    from_height: BlockHeight,
    blocks: Vec<CompactBlock>,
) -> Result<
    std::thread::JoinHandle<
        Result<std::result::Result<selective_scan::TrialBatch, selective_scan::Continuity>>,
    >,
> {
    let (sap0, orch0, iron0) = sizes;
    std::thread::Builder::new()
        .name("z-stack-decrypt".into())
        .spawn(move || {
            selective_scan::trial_decrypt(
                network,
                input,
                sap0,
                orch0,
                iron0,
                from_height,
                blocks,
                None,
            )
        })
        .map_err(|e| EngineError::Message(format!("decrypt thread: {e}")))
}

/// The prefix is bounded to STREAM_DECRYPT_BLOCKS. Keep its exact input until
/// the final fetch succeeds: a retry can replace the live buffer with a new fork
/// or corrected compact payload even while this trial is still running.
struct StreamedTrial {
    source: Vec<CompactBlock>,
    handle: std::thread::JoinHandle<
        Result<std::result::Result<selective_scan::TrialBatch, selective_scan::Continuity>>,
    >,
}

fn spawn_streamed_trial(
    network: ZNetwork,
    input: selective_scan::TrialInput,
    sizes: (u32, u32, u32),
    from_height: BlockHeight,
    blocks: Vec<CompactBlock>,
) -> Result<StreamedTrial> {
    let handle = spawn_trial(network, input, sizes, from_height, blocks.clone())?;
    Ok(StreamedTrial {
        source: blocks,
        handle,
    })
}

async fn join_trial(
    job: std::thread::JoinHandle<
        Result<std::result::Result<selective_scan::TrialBatch, selective_scan::Continuity>>,
    >,
) -> Result<std::result::Result<selective_scan::TrialBatch, selective_scan::Continuity>> {
    match tokio::task::spawn_blocking(move || job.join()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(EngineError::Transport("decrypt thread panicked".into())),
        Err(_) => Err(EngineError::Transport("decrypt join failed".into())),
    }
}

fn absorb_pending(
    pending: &mut Option<selective_scan::DecryptedBatch>,
    pending_range: &mut Option<ScanRange>,
    decrypted: selective_scan::DecryptedBatch,
    range: ScanRange,
) {
    match pending.as_mut() {
        Some(prev) => {
            prev.absorb(decrypted);
            if let Some(prev_range) = pending_range.as_mut() {
                *prev_range = extend_persist_range(prev_range, &range);
            }
        }
        None => {
            *pending_range = Some(range);
            *pending = Some(decrypted);
        }
    }
}

fn historic_pending_ready(pending: &Option<selective_scan::DecryptedBatch>) -> bool {
    pending.as_ref().is_some_and(|batch| batch.persist_ready())
}

fn persist_pending(
    db: &mut super::wallet::SyncDb,
    selective_scan: &mut NativeOffload,
    scanned: &Arc<AtomicU32>,
    cached_state: &mut Option<ChainState>,
    pending: &mut Option<selective_scan::DecryptedBatch>,
    pending_range: &mut Option<ScanRange>,
    progress: &Option<Arc<Mutex<SyncProgress>>>,
    context: &mut selective_scan::TrialContext,
) -> Result<Step> {
    let Some(prev) = pending.take() else {
        return Ok(Step::Continue);
    };
    let prev_range = pending_range.take().expect("pending range");
    note_pipeline(progress, true, true);
    let had_activity = prev.had_notes;

    let step = persist_decrypted(db, &prev_range, prev, selective_scan, scanned, cached_state)?;
    info!("historic wallet write");
    if matches!(step, Step::Continue) {
        context.refresh_after_persist(db, had_activity)?;
    }
    Ok(step)
}

async fn finish_inflight(
    prev: InflightTrial,
    client: &mut LwdClient,
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
    context: &selective_scan::TrialContext,
    scanned: &Arc<AtomicU32>,
    cache: &FsBlockCache,
    db_path: &Path,
    cached_state: &mut Option<ChainState>,
    selective_scan: &mut NativeOffload,
    pool: &mut FetchPool,
) -> Result<InflightOut> {
    let trial = join_trial(prev.handle).await?;
    let trial = match trial {
        Err(c) => {
            rewind_continuity(
                cache,
                db,
                db_path,
                scanned,
                cached_state,
                c,
                selective_scan,
                pool,
            )
            .await?;
            return Ok(InflightOut::Rewind);
        }
        Ok(t) => t,
    };
    let mut from_state = prev.from_state;
    if trial.had_notes && !selective_scan::frontier_covers(&from_state, prev.start_sizes) {
        let prior = u32::from(prev.range.block_range().start).saturating_sub(1);
        info!("GetTreeState for note batch after note-free frontier skip");
        from_state = fetch_tree_state(client, prior).await?;
    }
    let finalized = match selective_scan::finalize_trial(
        from_state,
        trial,
        Some(scanned.as_ref()),
        prev.start_sizes,
    )? {
        Err(c) => {
            rewind_continuity(
                cache,
                db,
                db_path,
                scanned,
                cached_state,
                c,
                selective_scan,
                pool,
            )
            .await?;
            return Ok(InflightOut::Rewind);
        }
        Ok(d) => d,
    };
    let decrypted = match selective_scan::complete_decrypted(
        network,
        db,
        context,
        finalized,
        Some(scanned.as_ref()),
    )? {
        Err(c) => {
            rewind_continuity(
                cache,
                db,
                db_path,
                scanned,
                cached_state,
                c,
                selective_scan,
                pool,
            )
            .await?;
            return Ok(InflightOut::Rewind);
        }
        Ok(d) => d,
    };
    if decrypted.had_notes {
        *cached_state = Some(decrypted.next_state.clone());
    }
    info!("full-range decrypt and frontier finalize");
    Ok(InflightOut::Batch(decrypted, prev.range))
}

async fn finish_streamed(
    prefix_job: StreamedTrial,
    blocks: Vec<CompactBlock>,
    from_state: ChainState,
    start_sizes: (u32, u32, u32),
    start: BlockHeight,
    client: &mut LwdClient,
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
    context: &selective_scan::TrialContext,
    scanned: &Arc<AtomicU32>,
) -> Result<std::result::Result<selective_scan::DecryptedBatch, selective_scan::Continuity>> {
    finish_streamed_with_fetch(
        prefix_job,
        blocks,
        from_state,
        start_sizes,
        start,
        network,
        db,
        context,
        scanned,
        |height| {
            let mut client = client.clone();
            async move { fetch_tree_state(&mut client, height).await }
        },
    )
    .await
}

async fn finish_streamed_with_fetch<F, Fut>(
    prefix_job: StreamedTrial,
    blocks: Vec<CompactBlock>,
    from_state: ChainState,
    start_sizes: (u32, u32, u32),
    start: BlockHeight,
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
    context: &selective_scan::TrialContext,
    scanned: &Arc<AtomicU32>,
    mut fetch_state: F,
) -> Result<std::result::Result<selective_scan::DecryptedBatch, selective_scan::Continuity>>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<ChainState>>,
{
    let prefix_n = STREAM_DECRYPT_BLOCKS as usize;
    let prefix_matches =
        blocks.get(..prefix_job.source.len()) == Some(prefix_job.source.as_slice());
    let joined = join_trial(prefix_job.handle).await;
    let prefix_trial = if prefix_matches {
        match joined? {
            Err(c) => return Ok(Err(c)),
            Ok(t) => Some(t),
        }
    } else {
        // Discard even a failed old trial. Its input was not the successful
        // range, and neither its notes nor its continuity errors may escape.
        info!("streamed prefix changed during fetch retry; decrypting final range");
        None
    };
    if prefix_trial.as_ref().is_none_or(|trial| trial.had_notes) {
        let handle = spawn_trial(
            network,
            context.snapshot(db, &blocks)?,
            start_sizes,
            start,
            blocks,
        )?;
        let trial = join_trial(handle).await?;
        let mut from_state = from_state;
        if trial.as_ref().is_ok_and(|t| t.had_notes)
            && !selective_scan::frontier_covers(&from_state, start_sizes)
        {
            let prior = u32::from(start).saturating_sub(1);
            info!("GetTreeState for streamed note prefix after frontier skip");
            from_state = fetch_state(prior).await?;
        }
        return match trial {
            Err(c) => Ok(Err(c)),
            Ok(t) => {
                match selective_scan::finalize_trial(
                    from_state,
                    t,
                    Some(scanned.as_ref()),
                    start_sizes,
                )? {
                    Err(c) => Ok(Err(c)),
                    Ok(d) => selective_scan::complete_decrypted(
                        network,
                        db,
                        context,
                        d,
                        Some(scanned.as_ref()),
                    ),
                }
            }
        };
    }
    let prefix = match selective_scan::finalize_trial(
        from_state.clone(),
        prefix_trial.expect("matching note-free prefix"),
        Some(scanned.as_ref()),
        start_sizes,
    )? {
        Err(c) => return Ok(Err(c)),
        Ok(d) => {
            match selective_scan::complete_decrypted(
                network,
                db,
                context,
                d,
                Some(scanned.as_ref()),
            )? {
                Err(c) => return Ok(Err(c)),
                Ok(d) => d,
            }
        }
    };
    if blocks.len() <= prefix_n {
        return Ok(Ok(prefix));
    }
    let rest: Vec<CompactBlock> = blocks.into_iter().skip(prefix_n).collect();
    if rest.is_empty() {
        return Ok(Ok(prefix));
    }
    let rest_start = start + u32::try_from(prefix_n).unwrap_or(u32::MAX);
    let rest_sizes = prefix.end_sizes;
    let rest_handle = spawn_trial(
        network,
        context.snapshot(db, &rest)?,
        rest_sizes,
        rest_start,
        rest,
    )?;
    let rest_trial = join_trial(rest_handle).await?;
    let mut rest_state = prefix.next_state.clone();
    if let Ok(ref t) = rest_trial {
        if t.had_notes && !selective_scan::frontier_covers(&rest_state, rest_sizes) {
            let prior = u32::from(rest_start).saturating_sub(1);
            info!("GetTreeState for streamed note suffix after frontier skip");
            rest_state = fetch_state(prior).await?;
        }
    }
    let rest = match rest_trial {
        Err(c) => return Ok(Err(c)),
        Ok(t) => {
            match selective_scan::finalize_trial(rest_state, t, Some(scanned.as_ref()), rest_sizes)?
            {
                Err(c) => return Ok(Err(c)),
                Ok(d) => {
                    match selective_scan::complete_decrypted(
                        network,
                        db,
                        context,
                        d,
                        Some(scanned.as_ref()),
                    )? {
                        Err(c) => return Ok(Err(c)),
                        Ok(d) => d,
                    }
                }
            }
        }
    };
    let mut out = prefix;
    out.absorb(rest);
    Ok(Ok(out))
}

async fn wait_stream_prefix(
    live: &LiveBlocks,
    handle: &tokio::task::JoinHandle<Result<()>>,
    n: usize,
) -> Option<Vec<CompactBlock>> {
    if n == 0 {
        return None;
    }
    while !handle.is_finished() {
        {
            let g = live.lock().unwrap_or_else(|e| e.into_inner());
            if g.len() >= n {
                return Some(g[..n].to_vec());
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

async fn take_fetched(mut job: FetchJob) -> Result<Vec<CompactBlock>> {
    match (&mut job.handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(EngineError::Transport("fetch task panicked".into())),
    }
    let mut g = job.live.lock().unwrap_or_else(|e| e.into_inner());
    Ok(std::mem::take(&mut *g))
}

fn persist_decrypted(
    db: &mut super::wallet::SyncDb,
    range: &ScanRange,
    batch: selective_scan::DecryptedBatch,
    selective_scan: &mut NativeOffload,
    scanned: &Arc<AtomicU32>,
    cached_state: &mut Option<ChainState>,
) -> Result<Step> {
    let last = u32::from(batch.last_height);
    let want_last = u32::from(range.block_range().end).saturating_sub(1);
    if last < want_last {
        return Err(EngineError::Transport(format!(
            "scan covered through {last}, wanted {want_last}"
        )));
    }
    let next_state = batch.next_state.clone();
    run_blocking_apply(|| selective_scan::persist_batch(db, batch, selective_scan))?;
    scanned.fetch_max(last, Ordering::Relaxed);
    *cached_state = Some(next_state);
    let latest = db
        .suggest_scan_ranges()
        .map_err(|e| EngineError::WalletDb(format!("suggest_scan_ranges: {e}")))?;
    if latest
        .first()
        .is_some_and(|next| next.priority() > range.priority())
    {
        Ok(Step::Restart)
    } else {
        Ok(Step::Continue)
    }
}

async fn rewind_continuity(
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    scanned: &Arc<AtomicU32>,
    cached_state: &mut Option<ChainState>,
    at: selective_scan::Continuity,
    selective_scan: &mut NativeOffload,
    pool: &mut FetchPool,
) -> Result<()> {
    let selective_scan::Continuity::Rewind(at) = at;
    apply_continuity_rewind(cache, db, db_path, scanned, cached_state, at).await?;
    *cached_state = None;
    selective_scan.reset();
    pool.drain();
    Ok(())
}

fn notes_force_serial_persist(had_notes: bool) -> bool {
    had_notes
}

/// sqlite `truncate_to_height` only accepts a height that exists in `blocks`.
/// Sparse watermarks and wipe/empty DBs may have no row; an actual SQLite
/// truncation failure must stop sync without replaying into old tree state.
fn truncate_existing(
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    requested: u32,
) -> Result<Option<u32>> {
    let Some(h) = super::wallet::rewind_target(db_path, requested)? else {
        info!("skip truncate_to_height; no blocks row at or below request");
        return Ok(None);
    };
    match db.truncate_to_height(BlockHeight::from_u32(h)) {
        Ok(actual) => Ok(Some(u32::from(actual))),
        Err(e) => Err(EngineError::WalletDb(format!(
            "truncate_to_height at {h}: {e}"
        ))),
    }
}

fn extend_persist_range(prev: &ScanRange, next: &ScanRange) -> ScanRange {
    ScanRange::from_parts(
        prev.block_range().start..next.block_range().end,
        prev.priority(),
    )
}

#[cfg(test)]
fn historic_persist_ready(n_heights: u32, had_notes: bool) -> bool {
    n_heights >= HISTORIC_PERSIST_BLOCKS || had_notes
}

fn note_pipeline(progress: &Option<Arc<Mutex<SyncProgress>>>, decrypt: bool, persist: bool) {
    if let Some(lock) = progress {
        if let Ok(mut g) = lock.lock() {
            g.decrypt_active = decrypt;
            g.persist_active = persist;
            g.download_active = true;
            if !matches!(
                g.stage,
                SyncStage::Scanning | SyncStage::CatchingUp | SyncStage::Enhancing
            ) {
                g.stage = SyncStage::Scanning;
            }
            g.message = "downloading · trial-decrypt · persist".into();
        }
    }
}

fn run_blocking_apply<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

fn note(progress: &Option<Arc<Mutex<SyncProgress>>>, stage: SyncStage, msg: impl Into<String>) {
    if let Some(lock) = progress {
        if let Ok(mut g) = lock.lock() {
            let msg = msg.into();
            if stage == SyncStage::Connecting
                && matches!(
                    g.stage,
                    SyncStage::Downloading
                        | SyncStage::Scanning
                        | SyncStage::CatchingUp
                        | SyncStage::Enhancing
                )
            {
                g.message = msg;
                return;
            }
            // Prefetch / GetTreeState must not flip Scanning back to Downloading
            // every fetch chunk. Ticker owns Downloading vs Scanning.
            if stage == SyncStage::Downloading
                && matches!(
                    g.stage,
                    SyncStage::Scanning | SyncStage::CatchingUp | SyncStage::Enhancing
                )
            {
                g.message = msg;
                return;
            }
            g.stage = stage;
            g.message = msg;
        }
    }
}

async fn apply_range(
    client: &mut LwdClient,
    network: ZNetwork,
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    range: &ScanRange,
    downloaded: &Arc<AtomicU32>,
    scanned: &Arc<AtomicU32>,
    selective_scan: &mut NativeOffload,
) -> Result<bool> {
    let blocks = fetch_range(client, range, Some(Arc::clone(downloaded))).await?;
    mark_downloaded(downloaded, &blocks);
    // Near-tip Verify/reorg always uses stock scan_cached_blocks.
    selective_scan.reset();
    let mut unused = None;
    match scan_inserted(
        client,
        network,
        cache,
        db,
        db_path,
        range,
        &blocks,
        scanned,
        selective_scan,
        false,
        &mut unused,
        &None,
    )
    .await?
    {
        Step::Continue => Ok(false),
        Step::Restart | Step::Rewind => Ok(true),
    }
}

enum Step {
    Continue,
    Restart,
    Rewind,
}

async fn scan_inserted(
    client: &mut LwdClient,
    network: ZNetwork,
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    range: &ScanRange,
    blocks: &[CompactBlock],
    scanned: &Arc<AtomicU32>,
    selective_scan: &mut NativeOffload,
    allow_selective_scan: bool,
    cached_state: &mut Option<ChainState>,
    progress: &Option<Arc<Mutex<SyncProgress>>>,
) -> Result<Step> {
    expect_contiguous_range(
        blocks,
        u32::from(range.block_range().start),
        u32::from(range.block_range().end).saturating_sub(1),
    )?;
    let start = range.block_range().start;
    let prior = start - 1;
    let chain_state = if let ChainStateSrc::Cached(cs) | ChainStateSrc::Walk(cs) =
        resolve_chain_state(cached_state.as_ref(), prior)
    {
        *cached_state = None;
        cs
    } else {
        info!("fetching GetTreeState before applying compact blocks");
        if allow_selective_scan {
            note(
                progress,
                SyncStage::Downloading,
                format!(
                    "fetching tree state at {} before a {}-block apply",
                    u32::from(prior),
                    blocks.len()
                ),
            );
        }
        fetch_tree_state(client, u32::from(prior)).await?
    };
    let limit = blocks.len();

    if allow_selective_scan && selective_scan.should_use(blocks.len()) {
        info!("selective-scan scan (hash note/birthday/tip shards only)");
        note(
            progress,
            SyncStage::Scanning,
            format!("trial-decrypting {} compact blocks", blocks.len()),
        );
        match run_blocking_apply(|| {
            selective_scan::scan_batch(
                network,
                db,
                &chain_state,
                start,
                blocks,
                selective_scan,
                Some(scanned.as_ref()),
            )
        })? {
            Err(selective_scan::Continuity::Rewind(at)) => {
                apply_continuity_rewind(cache, db, db_path, scanned, cached_state, at).await?;
                return Ok(Step::Rewind);
            }
            Ok((last, next_state)) => {
                let want_last = u32::from(range.block_range().end).saturating_sub(1);
                if last < want_last {
                    return Err(EngineError::Transport(format!(
                        "scan covered through {last}, wanted {want_last}"
                    )));
                }
                scanned.fetch_max(last, Ordering::Relaxed);
                *cached_state = Some(next_state);
                let latest = db
                    .suggest_scan_ranges()
                    .map_err(|e| EngineError::WalletDb(format!("suggest_scan_ranges: {e}")))?;
                if latest
                    .first()
                    .is_some_and(|next| next.priority() > range.priority())
                {
                    return Ok(Step::Restart);
                }
                return Ok(Step::Continue);
            }
        }
    }

    match run_blocking_apply(|| {
        scan_cached_blocks(
            &network,
            &SliceSource(blocks),
            db,
            start,
            &chain_state,
            limit,
        )
    }) {
        Err(ChainError::Scan(err)) if err.is_continuity_error() => {
            apply_continuity_rewind(cache, db, db_path, scanned, cached_state, err.at_height())
                .await?;
            Ok(Step::Rewind)
        }
        Ok(summary) => {
            let last = u32::from(summary.scanned_range().end).saturating_sub(1);
            let want_last = u32::from(range.block_range().end).saturating_sub(1);
            if last < want_last {
                return Err(EngineError::Transport(format!(
                    "scan covered through {last}, wanted {want_last}"
                )));
            }
            scanned.fetch_max(last, Ordering::Relaxed);
            *cached_state = None;
            let latest = db
                .suggest_scan_ranges()
                .map_err(|e| EngineError::WalletDb(format!("suggest_scan_ranges: {e}")))?;
            if latest
                .first()
                .is_some_and(|next| next.priority() > range.priority())
            {
                Ok(Step::Restart)
            } else {
                Ok(Step::Continue)
            }
        }
        Err(e) => Err(EngineError::WalletDb(format!("scan: {e}"))),
    }
}

fn mark_downloaded(downloaded: &Arc<AtomicU32>, blocks: &[CompactBlock]) {
    if let Some(last) = blocks.last() {
        if let Ok(h) = u32::try_from(last.height) {
            downloaded.fetch_max(h, Ordering::Relaxed);
            info!("cached compact-block batch");
        }
    }
}

async fn apply_continuity_rewind(
    cache: &FsBlockCache,
    db: &mut super::wallet::SyncDb,
    db_path: &Path,
    scanned: &Arc<AtomicU32>,
    cached_state: &mut Option<ChainState>,
    at: BlockHeight,
) -> Result<()> {
    let want = u32::from(at.saturating_sub(10));
    info!("chain continuity error; rewinding to an existing blocks row");
    let h = truncate_existing(db, db_path, want)?.ok_or_else(|| {
        EngineError::WalletDb(format!(
            "chain continuity error at {at}: no usable persisted rewind point at or below {want}"
        ))
    })?;
    // This is committed wallet state, unlike trial-decrypt progress. Publish it
    // even if removing stale cache data subsequently fails.
    scanned.store(h, Ordering::Relaxed);
    *cached_state = None;
    cache
        .truncate(BlockHeight::from_u32(h))
        .await
        .map_err(|e| EngineError::WalletDb(format!("cache truncate: {e}")))?;
    Ok(())
}

async fn fetch_range(
    client: &mut LwdClient,
    range: &ScanRange,
    downloaded: Option<Arc<AtomicU32>>,
) -> Result<Vec<CompactBlock>> {
    let start = u32::from(range.block_range().start);
    let end_incl = u32::from(range.block_range().end) - 1;
    fetch_height_range_inner(client, start, end_incl, 3, downloaded, None).await
}

async fn fetch_range_into(
    client: &mut LwdClient,
    range: &ScanRange,
    downloaded: Option<Arc<AtomicU32>>,
    live: LiveBlocks,
) -> Result<()> {
    let start = u32::from(range.block_range().start);
    let end_incl = u32::from(range.block_range().end) - 1;
    fetch_height_range_inner(client, start, end_incl, 3, downloaded, Some(live)).await?;
    Ok(())
}

/// How long one GetBlockRange may go without delivering a block. Tiny near-tip
/// ranges used to sit on the tonic 120–180s channel timeout; 13 blocks behind
/// then looked like a dead URL. 6–64 blocks stay on 45s so a last-range hang
/// does not look like a 90s freeze. The limit is idle time, not total time: a
/// slow link that keeps delivering blocks is never cut off mid-range (a total
/// deadline failed every range below ~150 KB/s, and the retry fetched the same
/// ranges again, so the scan never advanced).
fn get_block_range_timeout(start: u32, end_incl: u32) -> Duration {
    let n = end_incl.saturating_sub(start).saturating_add(1);
    Duration::from_secs(if n <= 64 {
        45
    } else if n <= crate::NEAR_TIP_BLOCKS {
        90
    } else if n <= crate::scan::FETCH_CHUNK_LOCAL {
        // A 2,000-block response can slow down while Zaino restarts or reads
        // cold history. Give it the same budget as a smaller historic range.
        90
    } else {
        120
    })
}

pub(super) async fn fetch_height_range(
    client: &mut LwdClient,
    start: u32,
    end_incl: u32,
) -> Result<Vec<CompactBlock>> {
    fetch_height_range_inner(client, start, end_incl, 3, None, None).await
}

fn fetch_height_range_inner<'a>(
    client: &'a mut LwdClient,
    start: u32,
    end_incl: u32,
    attempts: u32,
    downloaded: Option<Arc<AtomicU32>>,
    live: Option<LiveBlocks>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<CompactBlock>>> + Send + 'a>> {
    Box::pin(async move {
        let n = end_incl.saturating_sub(start).saturating_add(1);
        if n > crate::scan::FETCH_CHUNK_LOCAL {
            let mut out = Vec::new();
            let mut a = start;
            while a <= end_incl {
                let b = a
                    .saturating_add(crate::scan::FETCH_CHUNK_LOCAL - 1)
                    .min(end_incl);
                let part = fetch_height_range_inner(
                    client,
                    a,
                    b,
                    attempts,
                    downloaded.clone(),
                    live.clone(),
                )
                .await?;
                if live.is_none() {
                    out.extend(part);
                }
                a = b.saturating_add(1);
            }
            if live.is_none() {
                expect_contiguous_range(out.as_slice(), start, end_incl)?;
            }
            return Ok(out);
        }
        let live_start = live.as_ref().map_or(0, |blocks| {
            blocks.lock().unwrap_or_else(|e| e.into_inner()).len()
        });
        let idle = get_block_range_timeout(start, end_incl);
        let attempted = fetch_range_attempts(start, end_incl, attempts, live.clone(), |live| {
            let mut client = client.clone();
            let downloaded = downloaded.clone();
            async move {
                fetch_height_range_once(
                    &mut client,
                    start,
                    end_incl,
                    idle,
                    downloaded.as_ref(),
                    live,
                )
                .await
            }
        })
        .await;
        let last_err = match attempted {
            Ok(blocks) => return Ok(blocks),
            Err(err) => err,
        };
        if let Some(ref live) = live {
            if let Some(resume_at) = resumable_prefix(live, live_start, start, end_incl, &last_err)
            {
                // Zaino 0.10 ends any GetBlockRange after about 180 s, and a
                // slow link can hit the idle limit, but the stream delivered a
                // prefix. Keep it and ask for the rest; every resume needs new
                // blocks, so this ends. Refetching whole ranges instead failed
                // every attempt on links too slow for one range per deadline.
                info!("GetBlockRange resuming after a cut stream");
                fetch_height_range_inner(
                    client,
                    resume_at,
                    end_incl,
                    attempts,
                    downloaded,
                    Some(Arc::clone(live)),
                )
                .await?;
                let blocks = live.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(err) = expect_contiguous_range(&blocks[live_start..], start, end_incl) {
                    drop(blocks);
                    live.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .truncate(live_start);
                    return Err(err);
                }
                return Ok(Vec::new());
            }
        }
        // Splitting a range cannot fix a disconnected light server. It turns
        // one outage into hundreds of immediate RPCs while Zaino restarts.
        // Let the caller reconnect and resume from the durable scan island.
        if is_light_connection_outage(&last_err) {
            return Err(last_err);
        }
        if n > 1 {
            let mid = start + (n / 2) - 1;
            warn!("GetBlockRange splitting after retries");
            if let Some(ref live) = live {
                live.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .truncate(live_start);
            }
            let mut a = fetch_height_range_inner(
                client,
                start,
                mid,
                attempts,
                downloaded.clone(),
                live.clone(),
            )
            .await?;
            let b = fetch_height_range_inner(
                client,
                mid + 1,
                end_incl,
                attempts,
                downloaded,
                live.clone(),
            )
            .await?;
            if live.is_none() {
                a.extend(b);
                expect_contiguous_range(a.as_slice(), start, end_incl)?;
                return Ok(a);
            }
            return Ok(Vec::new());
        }
        Err(last_err)
    })
}

pub(super) fn is_light_connection_outage(err: &EngineError) -> bool {
    let message = match err {
        EngineError::Transport(message) => message.to_ascii_lowercase(),
        EngineError::Io(error) => {
            return matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
            )
        }
        _ => return false,
    };
    [
        "unavailable",
        "transport error",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "http2 error",
        "timed out",
        "deadline exceeded",
        "deadlineexceeded",
        // tonic's channel timeout (`Status::cancelled("Timeout expired")`) and
        // streams a restarting server cancels.
        "timeout expired",
        "operation was cancelled",
    ]
    .iter()
    .any(|reason| message.contains(reason))
}

/// Where to resume a range whose stream was cut by a deadline after it
/// delivered a valid prefix. `None` when nothing new arrived.
fn resumable_prefix(
    live: &LiveBlocks,
    live_start: usize,
    start: u32,
    end_incl: u32,
    err: &EngineError,
) -> Option<u32> {
    if !is_light_deadline(err) {
        return None;
    }
    let blocks = live.lock().unwrap_or_else(|e| e.into_inner());
    let got = u32::try_from(blocks.len().checked_sub(live_start)?).ok()?;
    let resume_at = start.checked_add(got)?;
    if got == 0 || resume_at > end_incl {
        return None;
    }
    expect_contiguous_range(&blocks[live_start..], start, resume_at - 1).ok()?;
    Some(resume_at)
}

fn is_light_deadline(err: &EngineError) -> bool {
    match err {
        EngineError::Transport(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("timed out")
                || message.contains("deadline exceeded")
                || message.contains("deadlineexceeded")
        }
        EngineError::Io(error) => error.kind() == std::io::ErrorKind::TimedOut,
        _ => false,
    }
}

async fn fetch_range_attempts<F, Fut>(
    start: u32,
    end_incl: u32,
    attempts: u32,
    live: Option<LiveBlocks>,
    mut fetch: F,
) -> Result<Vec<CompactBlock>>
where
    F: FnMut(Option<LiveBlocks>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<CompactBlock>>>,
{
    let live_start = live.as_ref().map_or(0, |blocks| {
        blocks.lock().unwrap_or_else(|e| e.into_inner()).len()
    });
    // The fetch enforces the idle limit per block. This cap only bounds a
    // server that trickles blocks just inside it (about 5 KB/s for 2,000).
    let timeout = get_block_range_timeout(start, end_incl) * RANGE_TOTAL_CAP_FACTOR;
    let mut last_err = None;
    for attempt in 1..=attempts {
        if attempt > 1 {
            if last_err.as_ref().is_some_and(is_light_connection_outage) {
                // Give the service time to restart instead of hammering a
                // refused connection three times in the same millisecond.
                tokio::time::sleep(Duration::from_secs(1 << (attempt - 2).min(3))).await;
            }
            if let Some(ref live) = live {
                // Only this subrange belongs to this attempt. Earlier chunks
                // and successful split halves remain part of the final range.
                live.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .truncate(live_start);
            }
        }
        tracing::debug!("GetBlockRange");

        let result = match tokio::time::timeout(timeout, fetch(live.clone())).await {
            Ok(Ok(blocks)) => {
                let valid = if let Some(ref live) = live {
                    let blocks = live.lock().unwrap_or_else(|e| e.into_inner());
                    expect_contiguous_range(&blocks[live_start..], start, end_incl)
                } else {
                    expect_contiguous_range(&blocks, start, end_incl)
                };
                valid.map(|_| blocks)
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(EngineError::Transport(format!(
                "GetBlockRange {start}..={end_incl} timed out after {}s",
                timeout.as_secs()
            ))),
        };
        match result {
            Ok(blocks) => {
                info!("compact RPC completed");
                return Ok(blocks);
            }
            Err(err) => {
                info!("compact RPC retry");
                warn!("GetBlockRange failed");
                let deadline = is_light_deadline(&err);
                last_err = Some(err);
                // Retrying a hung response three times, then recursively
                // bisecting it, delays reconnect by minutes and floods a
                // recovering indexer. The outer sync resumes from SQLite.
                if deadline {
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| EngineError::Transport("GetBlockRange failed".into())))
}

pub(super) async fn fetch_tree_state(client: &mut LwdClient, height: u32) -> Result<ChainState> {
    let timeout = Duration::from_secs(45);
    let mut last_err = None;
    for _attempt in 1u32..=3 {
        match tokio::time::timeout(
            timeout,
            client.get_tree_state(BlockId {
                height: u64::from(height),
                hash: vec![],
            }),
        )
        .await
        {
            Ok(Ok(resp)) => {
                info!("tree state RPC completed");
                return resp
                    .into_inner()
                    .to_chain_state()
                    .map_err(|e| EngineError::Transport(format!("tree state: {e}")));
            }
            Ok(Err(e)) => {
                info!("tree state RPC retry");
                warn!("GetTreeState failed; retrying");
                last_err = Some(EngineError::Transport(format!("GetTreeState: {e}")));
            }
            Err(_) => {
                info!("tree state RPC timeout");
                warn!("GetTreeState timed out; retrying");
                last_err = Some(EngineError::Transport(format!(
                    "GetTreeState at {height} timed out after {}s",
                    timeout.as_secs()
                )));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| EngineError::Transport("GetTreeState failed".into())))
}

const RANGE_TOTAL_CAP_FACTOR: u32 = 10;

async fn fetch_height_range_once(
    client: &mut LwdClient,
    start: u32,
    end_incl: u32,
    idle: Duration,
    downloaded: Option<&Arc<AtomicU32>>,
    live: Option<LiveBlocks>,
) -> Result<Vec<CompactBlock>> {
    let idle_error = || {
        EngineError::Transport(format!(
            "GetBlockRange {start}..={end_incl} timed out after {}s without a block",
            idle.as_secs()
        ))
    };
    let request = client.get_block_range(ProtoBlockRange {
        start: Some(BlockId {
            height: u64::from(start),
            hash: vec![],
        }),
        end: Some(BlockId {
            height: u64::from(end_incl),
            hash: vec![],
        }),
        pool_types: vec![],
    });
    let mut stream = tokio::time::timeout(idle, request)
        .await
        .map_err(|_| idle_error())?
        .map_err(|e| EngineError::Transport(format!("GetBlockRange: {e}")))?
        .into_inner();
    let mut blocks = Vec::new();
    loop {
        let message = tokio::time::timeout(idle, stream.message())
            .await
            .map_err(|_| idle_error())?;
        match message {
            Ok(Some(b)) => {
                if let Some(d) = downloaded {
                    if let Ok(h) = u32::try_from(b.height) {
                        d.fetch_max(h, Ordering::Relaxed);
                    }
                }
                if let Some(ref live) = live {
                    live.lock().unwrap_or_else(|e| e.into_inner()).push(b);
                } else {
                    blocks.push(b);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(EngineError::Transport(format!("GetBlockRange stream: {e}"))),
        }
    }
    if live.is_none() {
        expect_contiguous_range(blocks.as_slice(), start, end_incl)?;
    }
    Ok(blocks)
}

pub(super) fn expect_contiguous_range(
    blocks: &[CompactBlock],
    start: u32,
    end_incl: u32,
) -> Result<()> {
    if end_incl < start {
        return Err(EngineError::Message("end < start".into()));
    }
    let want = (end_incl - start) as usize + 1;
    if blocks.len() != want {
        return Err(EngineError::Transport(format!(
            "GetBlockRange {start}..={end_incl}: got {} blocks, want {want}",
            blocks.len()
        )));
    }
    for (i, b) in blocks.iter().enumerate() {
        let h = u32::try_from(b.height)
            .map_err(|_| EngineError::Message("block height overflow".into()))?;
        let expect = start + i as u32;
        if h != expect {
            return Err(EngineError::Transport(format!(
                "GetBlockRange gap: expected height {expect}, got {h}"
            )));
        }
        if b.hash.len() != 32 || b.prev_hash.len() != 32 {
            return Err(EngineError::Transport(format!(
                "GetBlockRange malformed block hash at {h}"
            )));
        }
        if i > 0 && b.prev_hash != blocks[i - 1].hash {
            return Err(EngineError::Transport(format!(
                "GetBlockRange prev_hash mismatch at {h}"
            )));
        }
    }
    Ok(())
}

struct SliceSource<'a>(&'a [CompactBlock]);

impl BlockSource for SliceSource<'_> {
    type Error = FsCacheError;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> std::result::Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> std::result::Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let start = from_height.unwrap_or(BlockHeight::from(0));
        let take = limit.unwrap_or(usize::MAX);
        let mut n = 0usize;
        for b in self.0 {
            let h = u32::try_from(b.height)
                .map_err(|_| ChainError::BlockSource(FsCacheError::HeightOverflow(b.height)))?;
            let bh = BlockHeight::from_u32(h);
            if bh < start {
                continue;
            }
            if n >= take {
                break;
            }
            with_block(b.clone())?;
            n += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct SubtreeHave {
    sap: u32,
    orch: u32,
    iron: u32,
}

/// Shard ranges whose roots were committed, per pool.
#[derive(Clone, Copy, Default)]
struct RootRanges {
    sapling: Option<(u64, u64)>,
    orchard: Option<(u64, u64)>,
    ironwood: Option<(u64, u64)>,
}

fn subtree_have(db: &mut super::wallet::SyncDb, skip_ironwood: bool) -> Result<SubtreeHave> {
    let sap = count_complete_shards(|i| {
        db.get_sapling_subtree_root(i)
            .map(|o| o.is_some())
            .map_err(|e| EngineError::WalletDb(format!("sapling shard {i}: {e}")))
    })?;
    let orch = count_complete_shards(|i| {
        db.get_orchard_subtree_root(i)
            .map(|o| o.is_some())
            .map_err(|e| EngineError::WalletDb(format!("orchard shard {i}: {e}")))
    })?;
    let iron = if skip_ironwood {
        0
    } else {
        count_complete_shards(|i| {
            db.get_ironwood_subtree_root(i)
                .map(|o| o.is_some())
                .map_err(|e| EngineError::WalletDb(format!("ironwood shard {i}: {e}")))
        })?
    };
    Ok(SubtreeHave { sap, orch, iron })
}

async fn fetch_subtree_root_vecs(
    client: &mut LwdClient,
    light_url: &str,
    skip_ironwood: bool,
    progress: &Option<Arc<Mutex<SyncProgress>>>,
    start: SubtreeHave,
    tx: tokio::sync::mpsc::UnboundedSender<PoolRoots>,
) {
    /// Zaino 0.10 resolves a block for every root before replying, so one
    /// request for many roots is slow and all-or-nothing. Small pages run
    /// concurrently and are emitted in order; the first short page ends it.
    const ROOT_PAGE: u32 = 2;
    const ROOT_PAGES_IN_FLIGHT: u32 = 4;

    async fn roots<H: HashSer + Send + 'static>(
        client: &mut LwdClient,
        protocol: ShieldedProtocol,
        name: &str,
        start_index: u32,
        light_url: &str,
        progress: &Option<Arc<Mutex<SyncProgress>>>,
        mut emit: impl FnMut(u32, Vec<CommitmentTreeRoot<H>>),
    ) {
        note(
            progress,
            SyncStage::Connecting,
            format!("fetching {name} subtree roots from {light_url} (from shard {start_index})"),
        );

        let page_start = |page: u32| start_index.saturating_add(page.saturating_mul(ROOT_PAGE));
        // Dropping the set aborts pages still in flight.
        let mut in_flight = tokio::task::JoinSet::new();
        let mut ready = BTreeMap::new();
        let (mut spawned, mut next) = (0u32, 0u32);
        'pages: loop {
            while spawned < next.saturating_add(ROOT_PAGES_IN_FLIGHT) {
                let mut client = client.clone();
                let (page, from) = (spawned, page_start(spawned));
                in_flight.spawn(async move {
                    (
                        page,
                        roots_page::<H>(&mut client, protocol, from, ROOT_PAGE).await,
                    )
                });
                spawned += 1;
            }
            let Some(joined) = in_flight.join_next().await else {
                break;
            };
            let Ok((page, fetched)) = joined else {
                warn!("GetSubtreeRoots page task failed");
                break;
            };
            ready.insert(page, fetched);
            while let Some(fetched) = ready.remove(&next) {
                match fetched {
                    Ok(roots) => {
                        // Anything but a full page ends the pool; more than a
                        // page means the server ignored the limit.
                        let last = roots.len() != ROOT_PAGE as usize;
                        if !roots.is_empty() {
                            emit(page_start(next), roots);
                        }
                        next += 1;
                        if last {
                            break 'pages;
                        }
                    }
                    Err(_e) => {
                        warn!("GetSubtreeRoots failed");
                        break 'pages;
                    }
                }
            }
        }
        info!("subtree roots RPC completed");
    }

    async fn roots_page<H: HashSer>(
        client: &mut LwdClient,
        protocol: ShieldedProtocol,
        start_index: u32,
        max_entries: u32,
    ) -> Result<Vec<CommitmentTreeRoot<H>>> {
        let mut req = GetSubtreeRootsArg::default();
        req.start_index = start_index;
        req.max_entries = max_entries;
        req.set_shielded_protocol(protocol);
        let mut stream = client
            .get_subtree_roots(req)
            .await
            .map_err(|e| EngineError::Transport(format!("GetSubtreeRoots: {e}")))?
            .into_inner();
        let mut out = Vec::new();
        while let Some(root) = stream
            .message()
            .await
            .map_err(|e| EngineError::Transport(format!("GetSubtreeRoots stream: {e}")))?
        {
            let root_hash = H::read(&root.root_hash[..])
                .map_err(|e| EngineError::Transport(format!("subtree hash: {e}")))?;
            out.push(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(root.completing_block_height as u32),
                root_hash,
            ));
        }
        Ok(out)
    }

    note(
        progress,
        SyncStage::Connecting,
        format!(
            "fetching subtree roots from {light_url} (from shards {}/{}/{})",
            start.sap, start.orch, start.iron
        ),
    );
    // Separate streams: the server resolves each pool independently, and one
    // pool's failure must not discard roots the other pools already returned.
    let mut orchard_client = client.clone();
    let mut ironwood_client = client.clone();
    tokio::join!(
        async {
            roots::<SaplingNode>(
                client,
                ShieldedProtocol::Sapling,
                "Sapling",
                start.sap,
                light_url,
                progress,
                |at, roots| {
                    let _ = tx.send(PoolRoots::Sapling(at, roots));
                },
            )
            .await;
            let _ = tx.send(PoolRoots::Done);
        },
        async {
            roots::<MerkleHashOrchard>(
                &mut orchard_client,
                ShieldedProtocol::Orchard,
                "Orchard",
                start.orch,
                light_url,
                progress,
                |at, roots| {
                    let _ = tx.send(PoolRoots::Orchard(at, roots));
                },
            )
            .await;
            let _ = tx.send(PoolRoots::Done);
        },
        async {
            if skip_ironwood {
                return;
            }
            roots::<MerkleHashOrchard>(
                &mut ironwood_client,
                ShieldedProtocol::Ironwood,
                "Ironwood",
                start.iron,
                light_url,
                progress,
                |at, roots| {
                    let _ = tx.send(PoolRoots::Ironwood(at, roots));
                },
            )
            .await;
            let _ = tx.send(PoolRoots::Done);
        },
    );
}

fn put_pool_roots(db: &mut super::wallet::SyncDb, pool: PoolRoots) -> RootRanges {
    fn committed(
        _pool: &str,
        start: u32,
        len: usize,
        put: std::result::Result<(), impl std::fmt::Display>,
    ) -> Option<(u64, u64)> {
        match put {
            Ok(()) => Some((u64::from(start), u64::from(start) + len as u64)),
            Err(_e) => {
                warn!("put subtree roots");
                None
            }
        }
    }
    let mut ranges = RootRanges::default();
    match pool {
        PoolRoots::Sapling(start, roots) => {
            let put = db.put_sapling_subtree_roots(u64::from(start), &roots);
            ranges.sapling = committed("Sapling", start, roots.len(), put);
        }
        PoolRoots::Orchard(start, roots) => {
            let put = db.put_orchard_subtree_roots(u64::from(start), &roots);
            ranges.orchard = committed("Orchard", start, roots.len(), put);
        }
        PoolRoots::Ironwood(start, roots) => {
            let put = db.put_ironwood_subtree_roots(u64::from(start), &roots);
            ranges.ironwood = committed("Ironwood", start, roots.len(), put);
        }
        PoolRoots::Done => {}
    }
    ranges
}

fn count_complete_shards(mut present: impl FnMut(u64) -> Result<bool>) -> Result<u32> {
    if !present(0)? {
        return Ok(0);
    }
    let mut hi = 1u64;
    while present(hi)? {
        hi = hi.saturating_mul(2);
        if hi >= 1_048_576 {
            break;
        }
    }
    let mut lo = hi / 2;
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if present(mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    u32::try_from(lo + 1).map_err(|_| EngineError::Message("subtree root count overflow".into()))
}

async fn refresh_utxos(
    client: &mut LwdClient,
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
) -> Result<()> {
    use ::transparent::{
        address::Script,
        bundle::{OutPoint, TxOut},
    };
    use zcash_client_backend::{
        proto::service::GetAddressUtxosArg, wallet::WalletTransparentOutput,
    };
    use zcash_keys::encoding::AddressCodec;
    use zcash_protocol::value::Zatoshis;
    use zcash_script::script;

    for account_id in db
        .get_account_ids()
        .map_err(|e| EngineError::WalletDb(format!("accounts: {e}")))?
    {
        let start_height = db
            .utxo_query_height(account_id)
            .map_err(|e| EngineError::WalletDb(format!("utxo height: {e}")))?;
        let addresses: Vec<String> = db
            .get_transparent_receivers(account_id, true, true)
            .map_err(|e| EngineError::WalletDb(format!("t-receivers: {e}")))?
            .into_keys()
            .map(|addr| addr.encode(&network))
            .collect();
        if addresses.is_empty() {
            continue;
        }
        let reply = client
            .get_address_utxos(GetAddressUtxosArg {
                addresses,
                start_height: u64::from(start_height),
                max_entries: 0,
            })
            .await
            .map_err(|e| EngineError::Transport(format!("GetAddressUtxos: {e}")))?
            .into_inner();
        let mut outputs = Vec::with_capacity(super::transparent_refresh::UTXO_REFRESH_BATCH);
        for u in reply.address_utxos {
            let txid: [u8; 32] = u
                .txid
                .as_slice()
                .try_into()
                .map_err(|_| EngineError::Message("utxo txid".into()))?;
            let Some(out) = WalletTransparentOutput::from_parts(
                OutPoint::new(txid, u.index.try_into().unwrap_or(0)),
                TxOut::new(
                    Zatoshis::from_nonnegative_i64(u.value_zat)
                        .map_err(|_| EngineError::Message("utxo value".into()))?,
                    Script(script::Code(u.script)),
                ),
                Some(BlockHeight::from_u32(u.height as u32)),
                Some(account_id),
                None,
                None,
            ) else {
                continue;
            };
            outputs.push(out);
            if outputs.len() == super::transparent_refresh::UTXO_REFRESH_BATCH {
                super::transparent_refresh::refresh_transparent_outputs(db, &outputs)
                    .map_err(|e| EngineError::WalletDb(format!("put utxo: {e}")))?;
                outputs.clear();
            }
        }
        super::transparent_refresh::refresh_transparent_outputs(db, &outputs)
            .map_err(|e| EngineError::WalletDb(format!("put utxo: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("pipeline_stream_tests.rs");
    include!("pipeline_utxo_tests.rs");

    #[test]
    fn server_outage_does_not_trigger_recursive_range_splitting() {
        assert!(is_light_connection_outage(&EngineError::Transport(
            "GetBlockRange: status: Unavailable, message: transport error".into()
        )));
        assert!(is_light_connection_outage(&EngineError::Transport(
            "connect http://127.0.0.1:8138: transport error".into()
        )));
        assert!(is_light_connection_outage(&EngineError::Transport(
            "GetBlockRange 1..=2000 timed out after 90s".into()
        )));
        assert!(is_light_connection_outage(&EngineError::Transport(
            "status: DeadlineExceeded, message: deadline exceeded".into()
        )));
        assert!(!is_light_connection_outage(&EngineError::Transport(
            "GetBlockRange 1..=1000: got 999 blocks, want 1000".into()
        )));
        assert!(!is_light_connection_outage(&EngineError::Transport(
            "GetBlockRange prev_hash mismatch at 1000".into()
        )));
    }

    #[tokio::test]
    async fn timed_out_range_returns_to_sync_without_retry_tree() {
        let attempts = std::cell::Cell::new(0);
        let error = fetch_range_attempts(1, 2_000, 3, None, |_| {
            attempts.set(attempts.get() + 1);
            async { Err(EngineError::Transport("GetBlockRange timed out".into())) }
        })
        .await
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(is_light_connection_outage(&error));
        assert_eq!(get_block_range_timeout(1, 2_000), Duration::from_secs(90));
    }

    fn blk(height: u64, prev: Vec<u8>) -> CompactBlock {
        let mut hash = vec![0u8; 32];
        hash[0] = height as u8;
        CompactBlock {
            height,
            hash,
            prev_hash: prev,
            time: 1,
            header: vec![],
            vtx: vec![],
            chain_metadata: None,
        }
    }

    #[test]
    fn a_stream_cut_by_a_deadline_resumes_after_its_valid_prefix() {
        let chain = |from: u64, n: u64| {
            let mut out: Vec<CompactBlock> = Vec::new();
            for height in from..from + n {
                let prev = out.last().map_or(vec![0u8; 32], |b| b.hash.clone());
                out.push(blk(height, prev));
            }
            out
        };
        let cut = EngineError::Transport(
            "GetBlockRange stream: code: 'Deadline expired before operation could complete', \
             message: \"Error: get_block_range gRPC request timed out.\""
                .into(),
        );
        let live: LiveBlocks = Arc::new(Mutex::new(vec![blk(1, vec![0u8; 32])]));
        live.lock().unwrap().extend(chain(10, 3));
        assert_eq!(resumable_prefix(&live, 1, 10, 20, &cut), Some(13));
        // A refused connection goes back to the outer retry, which backs off.
        let refused = EngineError::Transport("transport error: connection refused".into());
        assert_eq!(resumable_prefix(&live, 1, 10, 20, &refused), None);
        // Nothing new, or nothing left: no resume.
        assert_eq!(resumable_prefix(&live, 4, 13, 20, &cut), None);
        assert_eq!(resumable_prefix(&live, 1, 10, 12, &cut), None);
        // A prefix that does not start at the range start is not trusted.
        assert_eq!(resumable_prefix(&live, 1, 9, 20, &cut), None);
    }

    #[test]
    fn contiguous_range_ok() {
        let a = blk(10, vec![0u8; 32]);
        let mut b = blk(11, a.hash.clone());
        b.prev_hash = a.hash.clone();
        expect_contiguous_range(&[a, b], 10, 11).unwrap();
    }

    #[test]
    fn empty_or_short_range_fails() {
        assert!(expect_contiguous_range(&[], 10, 12).is_err());
        let a = blk(10, vec![0u8; 32]);
        assert!(expect_contiguous_range(&[a], 10, 12).is_err());
    }

    #[test]
    fn gapped_heights_fail() {
        let a = blk(10, vec![0u8; 32]);
        let b = blk(12, a.hash.clone());
        assert!(expect_contiguous_range(&[a, b], 10, 11).is_err());
    }

    #[test]
    fn clip_drops_already_scanned_prefix() {
        let r = ScanRange::from_parts(3_418_018.into()..3_472_263.into(), ScanPriority::ChainTip);
        let rest = clip_already_scanned(r, 3_472_165).unwrap();
        assert_eq!(u32::from(rest.block_range().start), 3_472_166);
        assert_eq!(u32::from(rest.block_range().end), 3_472_263);
        let scanned = ScanRange::from_parts(10.into()..20.into(), ScanPriority::Scanned);
        assert!(clip_already_scanned(scanned, 30).is_none());
        let ignored = ScanRange::from_parts(10.into()..20.into(), ScanPriority::Ignored);
        assert!(clip_already_scanned(ignored, 30).is_none());
        let hole = ScanRange::from_parts(10.into()..20.into(), ScanPriority::Historic);
        let kept = clip_already_scanned(hole, 30).expect("historic hole below watermark");
        assert_eq!(u32::from(kept.block_range().start), 10);
        assert_eq!(u32::from(kept.block_range().end), 20);
        let tip_hole =
            ScanRange::from_parts(3_335_466.into()..3_390_009.into(), ScanPriority::Historic);
        assert!(clip_already_scanned(tip_hole, 3_418_128).is_some());
        let below_displayed_tip =
            ScanRange::from_parts(3_418_129.into()..3_472_882.into(), ScanPriority::ChainTip);
        assert!(
            clip_already_scanned(below_displayed_tip, 3_472_881).is_some(),
            "chain-tip hole below displayed tip must not be clipped away"
        );
    }

    #[test]
    fn cover_gap_injects_island_to_tip_when_queue_skips_middle() {
        let tip_shard =
            ScanRange::from_parts(3_470_000.into()..3_472_882.into(), ScanPriority::ChainTip);
        let out = cover_gap_from(vec![tip_shard], 3_418_128, 3_472_881);
        assert_eq!(u32::from(out[0].block_range().start), 3_418_129);
        assert_eq!(u32::from(out[0].block_range().end), 3_470_000);
        let empty = cover_gap_from(Vec::new(), 3_418_128, 3_472_881);
        assert_eq!(u32::from(empty[0].block_range().start), 3_418_129);
        assert_eq!(u32::from(empty[0].block_range().end), 3_472_882);
    }

    #[test]
    fn far_verify_lookahead_joins_the_4000_gap() {
        let verify =
            ScanRange::from_parts(3_418_129.into()..3_418_139.into(), ScanPriority::Verify);
        let rest =
            ScanRange::from_parts(3_418_139.into()..3_472_882.into(), ScanPriority::ChainTip);
        let demoted = demote_far_verify(verify, 3_472_881);
        assert_eq!(demoted.priority(), ScanPriority::ChainTip);
        let merged = coalesce_scan_ranges(vec![demoted, rest]);
        assert_eq!(merged.len(), 1);
        assert_eq!(u32::from(merged[0].block_range().start), 3_418_129);
        assert_eq!(u32::from(merged[0].block_range().end), 3_472_882);
        let near = ScanRange::from_parts(3_472_870.into()..3_472_880.into(), ScanPriority::Verify);
        assert_eq!(
            demote_far_verify(near, 3_472_881).priority(),
            ScanPriority::Verify
        );
    }

    #[test]
    fn birthday_wipe_verify_joins_4000_selective_scan() {
        let birthday = 3_335_466u32;
        let tip = 3_473_482u32;
        let verify = ScanRange::from_parts(
            birthday.into()..(birthday + 10).into(),
            ScanPriority::Verify,
        );
        let rest = ScanRange::from_parts(
            (birthday + 10).into()..(tip + 1).into(),
            ScanPriority::Historic,
        );
        assert!(far_from_tip_range(&verify, tip));
        let demoted = demote_far_verify(verify, tip);
        assert_eq!(demoted.priority(), ScanPriority::ChainTip);
        let merged = coalesce_scan_ranges(vec![demoted, rest]);
        assert_eq!(merged.len(), 1);
        let batches = super::flatten_ranges(merged, crate::scan::BATCH_LOCAL);
        assert!(
            batches.len() > 1,
            "birthday→tip must split into 4000-block selective shard scanning batches, got {}",
            batches.len()
        );
        assert_eq!(u32::from(batches[0].block_range().start), birthday);
        assert_eq!(
            u32::from(batches[0].block_range().end) - u32::from(batches[0].block_range().start),
            crate::scan::BATCH_LOCAL
        );
    }

    #[test]
    fn birthday_wipe_chaintip_priority_still_starts_at_birthday() {
        let birthday = 3_335_466u32;
        let hole_end = 3_390_009u32;
        let tip = 3_473_510u32;
        let historic =
            ScanRange::from_parts(birthday.into()..hole_end.into(), ScanPriority::Historic);
        let chain_tip =
            ScanRange::from_parts(hole_end.into()..(tip + 1).into(), ScanPriority::ChainTip);
        let merged = coalesce_from_low_height(vec![chain_tip, historic]);
        assert_eq!(merged.len(), 1);
        assert_eq!(u32::from(merged[0].block_range().start), birthday);
        let batches = super::flatten_ranges(merged, crate::scan::BATCH_LOCAL);
        assert_eq!(u32::from(batches[0].block_range().start), birthday);
        assert_eq!(
            u32::from(batches[0].block_range().end) - u32::from(batches[0].block_range().start),
            crate::scan::BATCH_LOCAL
        );
    }

    #[test]
    fn first_apply_starts_before_4000_downloaded() {
        let birthday = 3_335_466u32;
        let tip = 3_479_136u32;
        let island = birthday.saturating_sub(1);
        let (start, end) = first_apply_range(Vec::new(), island, tip).expect("first apply");
        assert_eq!(start, birthday);
        assert_eq!(end, birthday + crate::scan::FETCH_CHUNK_LOCAL - 1);
        assert!(
            end < birthday + crate::scan::BATCH_LOCAL - 1,
            "first apply must not wait for a 4000-block download, got {end}"
        );
        assert_eq!(apply_chunk_width(true), crate::scan::FETCH_CHUNK_LOCAL);
        assert_eq!(initial_prefetch(true), PREFETCH_NATIVE_LOCAL as usize);
        assert!(initial_prefetch(true) >= 6);
        assert!(
            initial_prefetch(true) as u32 * crate::scan::FETCH_CHUNK_LOCAL >= 12_000,
            "keep enough prefetched heights to overlap an 8000-block wallet write"
        );
        assert!(!notes_force_serial_persist(false));
        assert!(notes_force_serial_persist(true));
        assert_eq!(HISTORIC_PERSIST_BLOCKS, 8_000);
        assert!(!historic_persist_ready(1_000, false));
        assert!(historic_persist_ready(8_000, false));
        assert!(historic_persist_ready(1_000, true));
        let a = ScanRange::from_parts(10.into()..1_010.into(), ScanPriority::ChainTip);
        let b = ScanRange::from_parts(1_010.into()..2_010.into(), ScanPriority::ChainTip);
        let merged = extend_persist_range(&a, &b);
        assert_eq!(u32::from(merged.block_range().start), 10);
        assert_eq!(u32::from(merged.block_range().end), 2_010);
        let chunks = historic_fetch_chunks(Vec::new(), island, tip, crate::scan::FETCH_CHUNK_LOCAL);
        assert_eq!(
            u32::from(chunks[0].block_range().end) - u32::from(chunks[0].block_range().start),
            crate::scan::FETCH_CHUNK_LOCAL
        );
        assert!(chunks.len() > 4, "birthday→tip must be many fetch chunks");
        assert_eq!(STREAM_DECRYPT_BLOCKS, 256);
    }

    #[test]
    fn wipe_sparse_8000_does_not_rewind_missing_height() {
        let birthday = 3_335_466u32;
        let first_notes = birthday + 4_404;
        let watermark = birthday + HISTORIC_PERSIST_BLOCKS - 1;
        let tip = birthday + 140_000;
        assert_eq!(first_notes, 3_339_870);
        assert!(
            super::super::wallet::rewind_target_from_rows(first_notes, &[]).is_none(),
            "empty / wiped blocks must not request 3339870"
        );
        assert!(
            super::super::wallet::rewind_target_from_rows(first_notes, &[watermark]).is_none(),
            "8000 watermark is above 3339870; do not invent that height"
        );
        assert_eq!(
            super::super::wallet::rewind_target_from_rows(watermark, &[watermark]),
            Some(watermark)
        );
        let island = birthday.saturating_sub(1);
        let (start, end) = first_apply_range(Vec::new(), island, tip).expect("first apply");
        assert_eq!(start, birthday);
        assert_eq!(
            end,
            birthday + crate::scan::FETCH_CHUNK_LOCAL - 1,
            "after wipe, first range is birthday..=birthday+999"
        );
        let suggest = vec![
            ScanRange::from_parts(
                birthday.into()..(birthday + 10).into(),
                ScanPriority::Verify,
            ),
            ScanRange::from_parts(
                (birthday + 10).into()..(tip + 1).into(),
                ScanPriority::Historic,
            ),
        ];
        let (v_start, v_end) = first_get_block_range(suggest, island, tip).expect("verify");
        assert_eq!((v_start, v_end), (start, end));
    }

    #[test]
    fn mid_scan_does_not_adopt_get_tree_state() {
        assert!(should_fetch_tree_state(None, 3_335_465));
        assert!(
            !should_fetch_tree_state(Some(3_335_465), 3_340_375),
            "walk already started; do not GetTreeState at a later prior"
        );
        assert!(!should_fetch_tree_state(Some(3_335_465), 3_335_465));
        match resolve_chain_state(None, 3_335_465.into()) {
            ChainStateSrc::Fetch => {}
            _ => panic!("cold start fetches once"),
        }
    }

    #[test]
    fn overlapping_verify_inside_historic_still_merges() {
        let birthday = 3_335_466u32;
        let hole_end = 3_390_009u32;
        let tip = 3_479_136u32;
        let verify = ScanRange::from_parts(
            birthday.into()..(birthday + 10).into(),
            ScanPriority::Verify,
        );
        let historic =
            ScanRange::from_parts(birthday.into()..hole_end.into(), ScanPriority::Historic);
        let merged = coalesce_from_low_height(vec![historic, verify]);
        assert_eq!(merged.len(), 1);
        assert_eq!(u32::from(merged[0].block_range().start), birthday);
        assert_eq!(u32::from(merged[0].block_range().end), hole_end);
        let (start, end) = first_get_block_range(
            vec![
                ScanRange::from_parts(
                    birthday.into()..(birthday + 10).into(),
                    ScanPriority::Verify,
                ),
                ScanRange::from_parts(birthday.into()..hole_end.into(), ScanPriority::Historic),
            ],
            birthday.saturating_sub(1),
            tip,
        )
        .expect("first RPC");
        assert_eq!(start, birthday);
        assert_eq!(end, birthday + crate::scan::FETCH_CHUNK_LOCAL - 1);
        assert_ne!(
            end,
            birthday + 9,
            "must not stock-scan the Verify-10 window"
        );
    }

    #[test]
    fn empty_birthday_reset_first_rpc_is_one_chunk_from_birthday_not_chaintip() {
        let birthday = 3_335_466u32;
        let hole_end = 3_390_009u32;
        let tip = 3_479_009u32;
        let island = birthday.saturating_sub(1);
        // sqlite after reset_scan: Historic birthday hole + ChainTip first.
        let historic =
            ScanRange::from_parts(birthday.into()..hole_end.into(), ScanPriority::Historic);
        let chain_tip =
            ScanRange::from_parts(hole_end.into()..(tip + 1).into(), ScanPriority::ChainTip);
        let verify = ScanRange::from_parts(
            birthday.into()..(birthday + 10).into(),
            ScanPriority::Verify,
        );
        let (start, end) =
            first_get_block_range(vec![chain_tip.clone(), historic.clone()], island, tip)
                .expect("first RPC");
        assert_eq!(start, birthday);
        assert_eq!(end, birthday + crate::scan::FETCH_CHUNK_LOCAL - 1);
        assert!(start < hole_end, "must not start at ChainTip {hole_end}");
        let (v_start, v_end) =
            first_get_block_range(vec![verify, historic, chain_tip], island, tip).expect("verify");
        assert_eq!((v_start, v_end), (start, end));
        assert!(
            far_from_tip_range(
                &ScanRange::from_parts(
                    birthday.into()..(birthday + 10).into(),
                    ScanPriority::Verify
                ),
                tip
            ),
            "Verify-10 at birthday is far from tip and must not be stock-scanned"
        );
    }

    #[test]
    fn last_filled_island_ignores_near_tip_crumbs() {
        let dir = std::env::temp_dir().join(format!(
            "z-stack-island-{}-{}",
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
        assert_eq!(
            super::super::wallet::last_filled_island_end(&path, 10, 105),
            20
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_start_at_the_scan_start_shard() {
        // Mainnet tree sizes at height 3,255,310: Sapling shard 1,127, Orchard
        // shard 759; Ironwood was not active yet.
        let none = SubtreeHave {
            sap: 0,
            orch: 0,
            iron: 0,
        };
        let start = roots_start(none, (73_875_153, 49_785_198, 0));
        assert_eq!((start.sap, start.orch, start.iron), (1_127, 759, 0));
        // A longer stored prefix wins; a size on a shard boundary starts there.
        let stored = SubtreeHave {
            sap: 1_200,
            orch: 0,
            iron: 2,
        };
        let start = roots_start(stored, (73_875_153, 760 << 16, 0));
        assert_eq!((start.sap, start.orch, start.iron), (1_200, 760, 2));
    }

    #[test]
    fn small_get_block_range_times_out_before_tonic() {
        assert_eq!(
            get_block_range_timeout(3_472_815, 3_472_828),
            Duration::from_secs(45)
        );
        assert_eq!(get_block_range_timeout(1, 4000), Duration::from_secs(120));
        assert_eq!(get_block_range_timeout(1, 200), Duration::from_secs(90));
        assert_eq!(
            get_block_range_timeout(3_472_800, 3_472_805),
            Duration::from_secs(45)
        );
        assert_eq!(
            get_block_range_timeout(3_472_800, 3_472_843),
            Duration::from_secs(45)
        );
    }
}
