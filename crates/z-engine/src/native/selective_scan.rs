//! Native selective shard scanning: `put_blocks_rows` + hash only note-bearing / birthday / tip shards.
//!
//! Stock `scan_cached_blocks` still runs for near-tip reorgs (`ScanPriority::Verify`
//! within `NEAR_TIP_BLOCKS`). Far-from-tip Verify / birthday wipe uses selective shard scanning.

use crate::error::{EngineError, Result};
use crate::offload::{
    checkpoint_retention, insert_leaf_run, is_full_shard_run, is_marked, keep_from_height,
    shard_of, stored_checkpoint_boundary, KeptRun, NoteLeaf, Offload, OffloadOutput, PoolAcc,
};
use crate::scan::{
    compact_commitment_counts, compact_has_shielded, ensure_compact_chain_metadata,
    HISTORIC_PERSIST_BLOCKS,
};
use crate::Network as ZNetwork;
use incrementalmerkletree::{Address, Level, Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use rayon::prelude::*;
use shardtree::error::ShardTreeError;
use shardtree::store::ShardStore;
use shardtree::ShardTree;
use std::collections::{BTreeMap, HashMap};
use std::result::Result as StdResult;
use std::sync::Arc;
#[cfg(test)]
use std::time::Instant;
use tracing::info;
use zcash_client_backend::{
    data_api::{
        chain::ChainState,
        ll::{wallet::put_blocks_rows, LowLevelWalletWrite},
        ScannedBlock, WalletCommitmentTrees, WalletRead,
    },
    proto::compact_formats::CompactBlock,
    scanning::{scan_block, Nullifiers, ScanningKeys},
};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_client_sqlite::wallet::commitment_tree;
use zcash_client_sqlite::AccountUuid;
use zcash_keys::keys::transparent::gap_limits::GapLimits;
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::BlockHeight;

pub(super) struct NativeOffload {
    acc: Offload,
    frontiers: bool,
    sapling_completed: u64,
    orchard_completed: u64,
    ironwood_completed: u64,
    /// Actual compact metadata for the last checkpoint retention window. These
    /// positions are independent of any future subtree roots already in SQLite.
    checkpoint_span: Vec<BlockWatermark>,
    persisted_identity: Option<Arc<ScanIdentity>>,
}

impl NativeOffload {
    pub(super) fn new(sapling: u64, orchard: u64, ironwood: u64) -> Self {
        let mut acc = Offload::new(sapling, orchard, ironwood, OffloadOutput::Runs);
        acc.drop_first_shards_with_roots();
        Self {
            acc,
            frontiers: false,
            sapling_completed: sapling,
            orchard_completed: orchard,
            ironwood_completed: ironwood,
            checkpoint_span: Vec::new(),
            persisted_identity: None,
        }
    }

    pub(super) fn should_use(&self, n_blocks: usize) -> bool {
        // Historic / birthday must use selective shard scanning even with no subtree roots yet
        // and even for sqlite's 10-block Verify lookahead. Stock
        // `scan_cached_blocks` hashes every orchard action (~10 heights per
        // GetTreeState). Near-tip Verify still uses `allow_selective_scan = false`.
        n_blocks > 0
    }

    /// Forgets roots learned during this scan. A rewind can delete root rows at
    /// or above its shard, so callers rebuild from SQLite before relying on more.
    pub(super) fn reset(&mut self) {
        self.acc = Offload::new(
            self.sapling_completed,
            self.orchard_completed,
            self.ironwood_completed,
            OffloadOutput::Runs,
        );
        self.acc.drop_first_shards_with_roots();
        self.frontiers = false;
        self.checkpoint_span.clear();
        self.persisted_identity = None;
    }

    /// Sapling, Orchard and Ironwood shard ranges treated as root-covered.
    pub(super) fn root_ranges(&self) -> [(u64, u64); 3] {
        [
            self.acc.sapling.root_range(),
            self.acc.orchard.root_range(),
            self.acc.ironwood.root_range(),
        ]
    }

    /// Subtree roots for these shard ranges were just committed to SQLite, so
    /// completed unmarked shards inside them no longer need to be hashed.
    pub(super) fn roots_available(
        &mut self,
        sapling: Option<(u64, u64)>,
        orchard: Option<(u64, u64)>,
        ironwood: Option<(u64, u64)>,
    ) {
        if let Some((start, end)) = sapling {
            self.acc.sapling.roots_available(start, end);
        }
        if let Some((start, end)) = orchard {
            self.acc.orchard.roots_available(start, end);
        }
        if let Some((start, end)) = ironwood {
            self.acc.ironwood.roots_available(start, end);
        }
    }
}

#[derive(Debug)]
pub(super) enum Continuity {
    Rewind(BlockHeight),
}

pub(super) type AccountUfvks = HashMap<AccountUuid, UnifiedFullViewingKey>;

/// Order-independent identity of every input that can affect wallet relevance.
/// Encodings stay in memory and use a fixed network prefix only for equality.
#[derive(PartialEq, Eq)]
struct ScanIdentity {
    ufvks: BTreeMap<AccountUuid, String>,
    sapling: std::collections::BTreeSet<(AccountUuid, sapling::Nullifier)>,
    orchard: std::collections::BTreeSet<(AccountUuid, orchard::note::Nullifier)>,
    ironwood: std::collections::BTreeSet<(AccountUuid, orchard::note::Nullifier)>,
}

impl ScanIdentity {
    fn new(ufvks: &AccountUfvks, nfs: &Nullifiers<AccountUuid>) -> Self {
        Self::with_keys(
            ufvks
                .iter()
                .map(|(id, key)| (*id, key.encode(&ZNetwork::Mainnet)))
                .collect(),
            nfs,
        )
    }

    fn with_keys(ufvks: BTreeMap<AccountUuid, String>, nfs: &Nullifiers<AccountUuid>) -> Self {
        Self {
            ufvks,
            sapling: nfs.sapling().iter().copied().collect(),
            orchard: nfs.orchard().iter().copied().collect(),
            ironwood: nfs.ironwood().iter().copied().collect(),
        }
    }

    fn with_nullifiers(&self, nfs: &Nullifiers<AccountUuid>) -> Self {
        Self::with_keys(self.ufvks.clone(), nfs)
    }
}

const CHANGED_SCAN_INPUTS: &str = "wallet inputs changed during scanning";

fn changed_scan_inputs() -> SqliteClientError {
    SqliteClientError::CorruptedData(CHANGED_SCAN_INPUTS.into())
}

fn map_persist_error(stage: &str, error: SqliteClientError) -> EngineError {
    if matches!(&error, SqliteClientError::CorruptedData(message) if message == CHANGED_SCAN_INPUTS)
        || matches!(&error, SqliteClientError::DbError(rusqlite::Error::SqliteFailure(code, _))
            if code.extended_code == rusqlite::ffi::SQLITE_BUSY_SNAPSHOT)
    {
        EngineError::Message(
            "Wallet changed during sync. Sync again to load its latest state.".into(),
        )
    } else {
        EngineError::WalletDb(format!("selective-scan {stage}: {error}"))
    }
}

type NativeScanningKeys = ScanningKeys<AccountUuid, (AccountUuid, zip32::Scope)>;

/// One historical scan invocation owns this context. Workers share immutable
/// snapshots; only a successful wallet-active persist publishes new nullifiers.
#[derive(Clone)]
pub(super) struct TrialContext {
    ufvks: Arc<AccountUfvks>,
    keys: Arc<std::sync::OnceLock<Arc<NativeScanningKeys>>>,
    nullifiers: Arc<Nullifiers<AccountUuid>>,
    identity: Arc<ScanIdentity>,
}

impl TrialContext {
    pub(super) fn new(ufvks: AccountUfvks, nullifiers: Nullifiers<AccountUuid>) -> Self {
        Self {
            identity: Arc::new(ScanIdentity::new(&ufvks, &nullifiers)),
            ufvks: Arc::new(ufvks),
            keys: Arc::new(std::sync::OnceLock::new()),
            nullifiers: Arc::new(nullifiers),
        }
    }

    fn keys(&self) -> &NativeScanningKeys {
        self.keys.get_or_init(|| {
            Arc::new(ScanningKeys::from_account_ufvks(
                self.ufvks.iter().map(|(id, key)| (*id, key.clone())),
            ))
        })
    }

    pub(super) fn snapshot(
        &self,
        db: &super::wallet::SyncDb,
        blocks: &[CompactBlock],
    ) -> Result<TrialInput> {
        // Nullifiers is intentionally not Clone in pinned Zakura. Keep its
        // original mutable sequential algorithm for tiny shielded batches.
        let sequential = if blocks.len() < 8 && blocks.iter().any(compact_has_shielded) {
            Some(load_nullifiers(db)?)
        } else {
            None
        };
        let mut read_fences = vec![Arc::clone(&self.identity)];
        if let Some(actual) = &sequential {
            read_fences.push(Arc::new(self.identity.with_nullifiers(actual)));
        }
        Ok(TrialInput {
            context: self.clone(),
            sequential,
            read_fences,
        })
    }

    pub(super) fn refresh_after_persist(
        &mut self,
        db: &super::wallet::SyncDb,
        had_activity: bool,
    ) -> Result<()> {
        if had_activity {
            self.nullifiers = Arc::new(load_nullifiers(db)?);
            self.identity = Arc::new(self.identity.with_nullifiers(&self.nullifiers));
        }
        Ok(())
    }
}

pub(super) struct TrialInput {
    context: TrialContext,
    sequential: Option<Nullifiers<AccountUuid>>,
    read_fences: Vec<Arc<ScanIdentity>>,
}

type NativeScanned = ScannedBlock<AccountUuid>;

/// Last compact block of a decrypt batch. Historic persist writes this row
/// (plus note-bearing heights) instead of one sqlite row per empty block.
#[derive(Clone)]
pub(super) struct BlockWatermark {
    height: BlockHeight,
    hash: BlockHash,
    prev_hash: BlockHash,
    time: u32,
    sapling_tree_size: u32,
    sapling_output_count: u32,
    orchard_tree_size: u32,
    orchard_action_count: u32,
    ironwood_tree_size: u32,
    ironwood_action_count: u32,
}

/// Trial-decrypt only (no frontier append). Two 1000-block ranges can run
/// this at once; [`finalize_trial`] applies `from_state` in height order.
pub(super) struct TrialBatch {
    read_fences: Vec<Arc<ScanIdentity>>,
    scanned: Vec<NativeScanned>,
    /// Compact metadata for every height in the original contiguous fetch.
    /// Rayon still skips `scan_block` on empty; persist uses this span so
    /// `put_blocks` never sees a gappy shielded-only list.
    span: Vec<BlockWatermark>,
    watermark: BlockWatermark,
    /// Wallet receipts or spends; either changes the next batch's nullifier set.
    pub had_notes: bool,
    tail: Option<DecryptTail>,
}

/// Trial-decrypt result. Persist is a separate sqlite step so the next 1000
/// can decrypt on Rayon while this batch writes.
pub(super) struct DecryptedBatch {
    read_fences: Vec<Arc<ScanIdentity>>,
    pub from_state: ChainState,
    /// Authentic prior frontiers and fixed run bounds captured during hashing.
    row_runs: Vec<RowRun>,
    pub scanned: Vec<NativeScanned>,
    pub last_height: BlockHeight,
    pub next_state: ChainState,
    /// Wallet receipts or spends; either changes the next batch's nullifier set.
    pub had_notes: bool,
    /// Commitment-tree sizes at the first height of this batch. Feed uses these,
    /// not `from_state`, so a note-free batch can skip the serial frontier append
    /// and still place leaves at the right position.
    pub start_sizes: (u32, u32, u32),
    /// Sizes after the batch, from compact `chain_metadata` (not from hashing).
    pub end_sizes: (u32, u32, u32),
    span: Vec<BlockWatermark>,
    watermark: BlockWatermark,
    tail: Option<DecryptTail>,
}

struct RowRun {
    first: BlockHeight,
    last: BlockHeight,
    prior: ChainState,
}

impl DecryptedBatch {
    pub(super) fn absorb(&mut self, other: Self) {
        self.read_fences.extend(other.read_fences);
        self.read_fences.dedup_by(|a, b| Arc::ptr_eq(a, b));
        self.row_runs.extend(other.row_runs);
        self.scanned.extend(other.scanned);
        self.span.extend(other.span);
        self.last_height = other.last_height;
        // A note-free suffix did not hash a frontier. Keep the earlier real one
        // so the next note batch can see it is stale and fetch GetTreeState.
        if other.had_notes {
            self.next_state = other.next_state;
        }
        self.had_notes |= other.had_notes;
        self.end_sizes = other.end_sizes;
        self.watermark = other.watermark;
    }

    pub(super) fn height_count(&self) -> u32 {
        u32::try_from(self.span.len()).unwrap_or(u32::MAX)
    }

    pub(super) fn persist_ready(&self) -> bool {
        self.height_count() >= HISTORIC_PERSIST_BLOCKS || self.had_notes
    }
}

struct DecryptTail {
    blocks: Vec<CompactBlock>,
    first_receive: usize,
    from_height: BlockHeight,
}

pub(super) fn load_ufvks(db: &super::wallet::SyncDb) -> Result<AccountUfvks> {
    db.get_unified_full_viewing_keys()
        .map_err(|e| EngineError::WalletDb(format!("ufvks: {e}")))
}

pub(super) fn load_nullifiers(db: &super::wallet::SyncDb) -> Result<Nullifiers<AccountUuid>> {
    Nullifiers::unspent(db).map_err(|e| EngineError::WalletDb(format!("nullifiers: {e}")))
}

/// Rayon trial-decrypt. No sqlite and no frontier append. Safe on a dedicated
/// thread so the 2-worker gRPC runtime is not blocked. Sequential note-tail
/// (rare) is finished later on the db thread via [`complete_decrypted`].
pub(super) fn trial_decrypt(
    network: ZNetwork,
    input: TrialInput,
    sap0: u32,
    orch0: u32,
    iron0: u32,
    from_height: BlockHeight,
    mut blocks: Vec<CompactBlock>,
    scanned_tick: Option<Arc<std::sync::atomic::AtomicU32>>,
) -> Result<std::result::Result<TrialBatch, Continuity>> {
    if blocks.is_empty() {
        return Err(EngineError::Message("selective-scan empty batch".into()));
    }
    ensure_scan_threads();

    ensure_compact_chain_metadata(&mut blocks, sap0, orch0, iron0);
    let span = compact_span(&blocks)?;
    if !heights_are_sequential(&span.iter().map(|w| u32::from(w.height)).collect::<Vec<_>>()) {
        return Err(EngineError::Message(
            "selective-scan compact span is not sequential".into(),
        ));
    }
    let watermark = span
        .last()
        .cloned()
        .ok_or_else(|| EngineError::Message("selective-scan empty batch".into()))?;

    let tick = scanned_tick.as_deref();
    let (scanned, first_receive) = if !blocks.iter().any(compact_has_shielded) {
        // No viewing-key construction or nullifier mutation is needed.
        (Vec::new(), None)
    } else {
        let keys = input.context.keys();
        let result = match input.sequential {
            Some(nullifiers) => {
                scan_batch_sequential(network, keys, nullifiers, from_height, &blocks, tick)?
            }
            None => scan_batch_parallel(network, keys, &input.context.nullifiers, &blocks, tick)?,
        };
        match result {
            Ok(value) => value,
            Err(c) => return Ok(Err(c)),
        }
    };

    let had_notes = scanned.iter().any(wallet_activity);
    if let Some(i) = first_receive {
        info!("selective-scan decrypt (notes; tail later)");
        return Ok(Ok(TrialBatch {
            read_fences: input.read_fences,
            scanned,
            span,
            watermark,
            had_notes: true,
            tail: Some(DecryptTail {
                blocks,
                first_receive: i,
                from_height,
            }),
        }));
    }
    info!("selective-scan decrypt");
    Ok(Ok(TrialBatch {
        read_fences: input.read_fences,
        scanned,
        span,
        watermark,
        had_notes,
        tail: None,
    }))
}

pub(super) fn tree_sizes(from_state: &ChainState) -> (u32, u32, u32) {
    (
        u32::try_from(from_state.final_sapling_tree().tree_size()).unwrap_or(u32::MAX),
        u32::try_from(from_state.final_orchard_tree().tree_size()).unwrap_or(u32::MAX),
        u32::try_from(from_state.final_ironwood_tree().tree_size()).unwrap_or(u32::MAX),
    )
}

pub(super) fn frontier_covers(state: &ChainState, sizes: (u32, u32, u32)) -> bool {
    tree_sizes(state) == sizes
}

fn watermark_sizes(w: &BlockWatermark) -> (u32, u32, u32) {
    (
        w.sapling_tree_size,
        w.orchard_tree_size,
        w.ironwood_tree_size,
    )
}

/// Rayon trial-decrypt + frontier finalize. Prefer [`trial_decrypt`] when two
/// ranges run at once so `chain_state_after` stays in height order.
pub(super) fn decrypt_owned(
    network: ZNetwork,
    input: TrialInput,
    from_state: ChainState,
    from_height: BlockHeight,
    blocks: Vec<CompactBlock>,
    scanned_tick: Option<Arc<std::sync::atomic::AtomicU32>>,
) -> Result<std::result::Result<DecryptedBatch, Continuity>> {
    let sizes = tree_sizes(&from_state);
    let (sap0, orch0, iron0) = sizes;
    let trial = trial_decrypt(
        network,
        input,
        sap0,
        orch0,
        iron0,
        from_height,
        blocks,
        scanned_tick.clone(),
    )?;
    match trial {
        Err(c) => Ok(Err(c)),
        Ok(t) => finalize_trial(from_state, t, scanned_tick.as_deref(), sizes),
    }
}

pub(super) fn finalize_trial(
    from_state: ChainState,
    trial: TrialBatch,
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
    start_sizes: (u32, u32, u32),
) -> Result<std::result::Result<DecryptedBatch, Continuity>> {
    if trial.tail.is_some() {
        let end_sizes = watermark_sizes(&trial.watermark);
        return Ok(Ok(DecryptedBatch {
            read_fences: trial.read_fences,
            from_state: from_state.clone(),
            row_runs: Vec::new(),
            scanned: trial.scanned,
            last_height: trial
                .tail
                .as_ref()
                .map(|t| t.from_height)
                .unwrap_or_else(|| from_state.block_height()),
            next_state: from_state,
            had_notes: true,
            start_sizes,
            end_sizes,
            span: trial.span,
            watermark: trial.watermark,
            tail: trial.tail,
        }));
    }
    let mut result = finish_decrypted(
        from_state,
        trial.scanned,
        trial.span,
        trial.watermark,
        trial.had_notes,
        start_sizes,
        scanned_tick,
    )?;
    if let Ok(batch) = &mut result {
        batch.read_fences = trial.read_fences;
    }
    Ok(result)
}

/// Finish a rare same-batch spend tail on the sqlite thread, then frontiers.
pub(super) fn complete_decrypted(
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
    context: &TrialContext,
    mut batch: DecryptedBatch,
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<DecryptedBatch, Continuity>> {
    let Some(tail) = batch.tail.take() else {
        return Ok(Ok(batch));
    };
    let keys = context.keys();
    let nullifiers = load_nullifiers(db)?;
    batch
        .read_fences
        .push(Arc::new(context.identity.with_nullifiers(&nullifiers)));
    let scanned = match finish_note_tail(
        network,
        nullifiers,
        keys,
        tail.from_height,
        &tail.blocks,
        batch.scanned,
        tail.first_receive,
        scanned_tick,
    )? {
        Ok(s) => s,
        Err(c) => return Ok(Err(c)),
    };
    let mut result = finish_decrypted(
        batch.from_state,
        scanned,
        batch.span,
        batch.watermark,
        true,
        batch.start_sizes,
        scanned_tick,
    )?;
    if let Ok(out) = &mut result {
        out.read_fences = batch.read_fences;
    }
    Ok(result)
}

pub(super) fn persist_batch(
    db: &mut super::wallet::SyncDb,
    batch: DecryptedBatch,
    off: &mut NativeOffload,
) -> Result<()> {
    persist(
        db,
        &batch.from_state,
        &batch.row_runs,
        &batch.read_fences,
        batch.scanned,
        off,
        &batch.span,
        &batch.watermark,
        batch.start_sizes,
    )
}

pub(super) fn scan_batch(
    network: ZNetwork,
    db: &mut super::wallet::SyncDb,
    from_state: &ChainState,
    from_height: BlockHeight,
    blocks: &[CompactBlock],
    off: &mut NativeOffload,
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<(u32, ChainState), Continuity>> {
    let ufvks = load_ufvks(db)?;
    let context = TrialContext::new(ufvks, load_nullifiers(db)?);
    let input = context.snapshot(db, blocks)?;
    let decrypted = decrypt_owned(
        network,
        input,
        from_state.clone(),
        from_height,
        blocks.to_vec(),
        None,
    )?;
    let decrypted = match decrypted {
        Err(c) => return Ok(Err(c)),
        Ok(d) => match complete_decrypted(network, db, &context, d, scanned_tick)? {
            Err(c) => return Ok(Err(c)),
            Ok(d) => d,
        },
    };
    if let Some(tick) = scanned_tick {
        tick.fetch_max(
            u32::from(decrypted.last_height),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    let last = u32::from(decrypted.last_height);
    let next_state = decrypted.next_state.clone();
    persist_batch(db, decrypted, off)?;
    Ok(Ok((last, next_state)))
}

fn finish_decrypted(
    from_state: ChainState,
    scanned: Vec<NativeScanned>,
    span: Vec<BlockWatermark>,
    watermark: BlockWatermark,
    had_notes: bool,
    start_sizes: (u32, u32, u32),
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<DecryptedBatch, Continuity>> {
    let last_height = watermark.height;
    if let Some(tick) = scanned_tick {
        tick.fetch_max(u32::from(last_height), std::sync::atomic::Ordering::Relaxed);
    }
    // Note-free batches do not hash every commitment into the frontier. The
    // next decrypt uses `end_sizes` from compact metadata. A later note batch
    // fetches GetTreeState when this frontier no longer covers `start_sizes`.
    let (next_state, row_runs) = if had_notes {
        if !frontier_covers(&from_state, start_sizes) {
            return Err(EngineError::Message(
                "selective-scan note frontier does not cover batch start".into(),
            ));
        }
        walk_with_row_frontiers(&from_state, &scanned, &span)?
    } else {
        (from_state.clone(), Vec::new())
    };
    if had_notes && !frontier_covers(&next_state, watermark_sizes(&watermark)) {
        return Err(EngineError::Message(
            "selective-scan note frontier does not cover batch end".into(),
        ));
    }
    Ok(Ok(DecryptedBatch {
        read_fences: Vec::new(),
        row_runs,
        from_state,
        scanned,
        last_height,
        next_state,
        had_notes,
        start_sizes,
        end_sizes: watermark_sizes(&watermark),
        span,
        watermark,
        tail: None,
    }))
}

fn tick_scanned(tick: Option<&std::sync::atomic::AtomicU32>, height: BlockHeight) {
    if let Some(p) = tick {
        p.fetch_max(u32::from(height), std::sync::atomic::Ordering::Relaxed);
    }
}

/// Hash each commitment once and capture the real prior frontier of every
/// wallet-active contiguous run. These boundaries survive batch coalescing.
fn walk_with_row_frontiers(
    from: &ChainState,
    scanned: &[NativeScanned],
    span: &[BlockWatermark],
) -> Result<(ChainState, Vec<RowRun>)> {
    let last = span
        .last()
        .ok_or_else(|| EngineError::Message("empty frontier span".into()))?;
    let mut sapling = from.final_sapling_tree().clone();
    let mut orchard = from.final_orchard_tree().clone();
    let mut ironwood = from.final_ironwood_tree().clone();
    let mut rows = Vec::new();
    for run in scanned.chunk_by(|a, b| b.height() == a.height() + 1) {
        if run.iter().any(wallet_activity) {
            let first = run[0].height();
            let index = u32::from(first)
                .checked_sub(u32::from(span[0].height))
                .ok_or_else(|| EngineError::Message("row frontier precedes compact span".into()))?
                as usize;
            let compact = span
                .get(index)
                .filter(|b| b.height == first)
                .ok_or_else(|| EngineError::Message("row frontier has no compact header".into()))?;
            rows.push(RowRun {
                first,
                last: run.last().expect("nonempty run").height(),
                prior: ChainState::new(
                    first - 1,
                    compact.prev_hash,
                    sapling.clone(),
                    orchard.clone(),
                    ironwood.clone(),
                ),
            });
        }
        for block in run {
            for (hash, _) in block.sapling().commitments() {
                if !sapling.append(hash.clone()) {
                    return Err(EngineError::Message("sapling frontier full".into()));
                }
            }
            for (hash, _) in block.orchard().commitments() {
                if !orchard.append(*hash) {
                    return Err(EngineError::Message("orchard frontier full".into()));
                }
            }
            for (hash, _) in block.ironwood().commitments() {
                if !ironwood.append(*hash) {
                    return Err(EngineError::Message("ironwood frontier full".into()));
                }
            }
        }
    }
    Ok((
        ChainState::new(last.height, last.hash, sapling, orchard, ironwood),
        rows,
    ))
}

#[cfg(test)]
pub(super) fn chain_state_after(
    from: &ChainState,
    scanned: &[NativeScanned],
    last_height: BlockHeight,
    last_hash: BlockHash,
) -> Option<ChainState> {
    // `scanned` is shielded-only (`compact_has_shielded`). Empty compact never
    // reaches a frontier append.
    let mut sapling = from.final_sapling_tree().clone();
    let mut orchard = from.final_orchard_tree().clone();
    let mut ironwood = from.final_ironwood_tree().clone();
    for block in scanned {
        for (c, _) in block.sapling().commitments() {
            if !sapling.append(c.clone()) {
                return None;
            }
        }
        for (c, _) in block.orchard().commitments() {
            if !orchard.append(*c) {
                return None;
            }
        }
        for (c, _) in block.ironwood().commitments() {
            if !ironwood.append(*c) {
                return None;
            }
        }
    }
    Some(ChainState::new(
        last_height,
        last_hash,
        sapling,
        orchard,
        ironwood,
    ))
}

fn compact_block_hash(block: &CompactBlock) -> Result<BlockHash> {
    let bytes: [u8; 32] = block
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| EngineError::Message("compact block hash".into()))?;
    Ok(BlockHash(bytes))
}

fn compact_span(blocks: &[CompactBlock]) -> Result<Vec<BlockWatermark>> {
    blocks.iter().map(watermark_of).collect()
}

fn heights_are_sequential(heights: &[u32]) -> bool {
    heights.windows(2).all(|w| w[1] == w[0].saturating_add(1))
}

/// `put_blocks` / `put_blocks_rows` reject a gappy slice. Skip-empty decrypt
/// leaves `scanned` as shielded-only heights — split those into contiguous
/// runs instead of handing `[birthday, birthday+7999]` with holes.
fn put_blocks_height_slices(scanned_heights: &[u32]) -> Vec<Vec<u32>> {
    let mut runs: Vec<Vec<u32>> = Vec::new();
    for &h in scanned_heights {
        match runs.last_mut() {
            Some(run) if run.last() == Some(&(h.saturating_sub(1))) => run.push(h),
            _ => runs.push(vec![h]),
        }
    }
    runs
}

fn watermark_of(block: &CompactBlock) -> Result<BlockWatermark> {
    let height =
        u32::try_from(block.height).map_err(|_| EngineError::Message("compact height".into()))?;
    let (sap_n, orch_n, iron_n) = compact_commitment_counts(block);
    let (sap, orch, iron) = match &block.chain_metadata {
        Some(m) => (
            m.sapling_commitment_tree_size,
            m.orchard_commitment_tree_size,
            m.ironwood_commitment_tree_size,
        ),
        None => (sap_n, orch_n, iron_n),
    };
    Ok(BlockWatermark {
        height: BlockHeight::from_u32(height),
        hash: compact_block_hash(block)?,
        prev_hash: BlockHash(
            block
                .prev_hash
                .as_slice()
                .try_into()
                .map_err(|_| EngineError::Message("compact previous block hash".into()))?,
        ),
        time: block.time,
        sapling_tree_size: sap,
        sapling_output_count: sap_n,
        orchard_tree_size: orch,
        orchard_action_count: orch_n,
        ironwood_tree_size: iron,
        ironwood_action_count: iron_n,
    })
}

fn ensure_scan_threads() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8)
            .max(1);
        match rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("z-stack-scan-{i}"))
            .build_global()
        {
            Ok(()) => info!("selective-scan rayon scan pool"),
            Err(_) => info!("selective-scan using existing rayon pool"),
        }
    });
}

fn continuity_or_err(err: zcash_client_backend::scanning::ScanError) -> Result<Continuity> {
    if err.is_continuity_error() {
        Ok(Continuity::Rewind(err.at_height()))
    } else {
        Err(EngineError::WalletDb(format!("scan_block: {err}")))
    }
}

fn wallet_activity(sb: &NativeScanned) -> bool {
    !sb.transactions().is_empty()
}

fn received_wallet_notes(sb: &NativeScanned) -> bool {
    sb.transactions().iter().any(|tx| {
        !tx.sapling_outputs().is_empty()
            || !tx.orchard_outputs().is_empty()
            || !tx.ironwood_outputs().is_empty()
    })
}

fn scan_batch_sequential<IvkTag>(
    network: ZNetwork,
    keys: &ScanningKeys<AccountUuid, IvkTag>,
    mut nullifiers: Nullifiers<AccountUuid>,
    from_height: BlockHeight,
    blocks: &[CompactBlock],
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<(Vec<NativeScanned>, Option<usize>), Continuity>>
where
    IvkTag: Copy + std::hash::Hash + Eq + Send + Sync + 'static,
{
    let _ = from_height;
    let mut scanned = Vec::with_capacity(blocks.len());
    let mut first_receive = None;
    for (i, block) in blocks.iter().enumerate() {
        if !compact_has_shielded(block) {
            continue;
        }
        // `prior` is None: skipped empty heights are not the previous compact,
        // so a hash-continuity check against the last shielded block is a
        // false rewind (birthday+4404 toast after wipe).
        match scan_block(&network, block.clone(), keys, &nullifiers, None) {
            Err(err) => return Ok(Err(continuity_or_err(err)?)),
            Ok(sb) => {
                nullifiers.update_with(&sb);
                if first_receive.is_none() && received_wallet_notes(&sb) {
                    first_receive = Some(i);
                }
                if i % 8 == 7 {
                    tick_scanned(scanned_tick, sb.height());
                }
                scanned.push(sb);
            }
        }
    }
    Ok(Ok((scanned, first_receive)))
}

/// Trial-decrypt on Rayon with the pre-batch nullifier set. Empty compact
/// blocks (`!compact_has_shielded`) never enter `scan_block` — that skip is
/// the hot-loop filter, not a comment. Same-batch spends of notes received
/// earlier in this batch are finished later on the db thread.
fn scan_batch_parallel<IvkTag>(
    network: ZNetwork,
    keys: &ScanningKeys<AccountUuid, IvkTag>,
    nullifiers: &Nullifiers<AccountUuid>,
    blocks: &[CompactBlock],
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<(Vec<NativeScanned>, Option<usize>), Continuity>>
where
    IvkTag: Copy + std::hash::Hash + Eq + Send + Sync + 'static,
{
    let results: Vec<(usize, _)> = blocks
        .par_iter()
        .enumerate()
        .with_min_len(4)
        .filter(|(_, block)| compact_has_shielded(block))
        .map(|(i, block)| {
            (
                i,
                scan_block(&network, block.clone(), keys, nullifiers, None),
            )
        })
        .collect();

    let mut scanned = Vec::with_capacity(results.len());
    let mut first_receive = None;
    for (i, r) in results {
        let sb = match r {
            Err(err) => return Ok(Err(continuity_or_err(err)?)),
            Ok(sb) => sb,
        };
        if first_receive.is_none() && received_wallet_notes(&sb) {
            first_receive = Some(i);
        }
        if i % 8 == 7 {
            tick_scanned(scanned_tick, sb.height());
        }
        scanned.push(sb);
    }
    Ok(Ok((scanned, first_receive)))
}

fn finish_note_tail<IvkTag>(
    network: ZNetwork,
    mut nfs: Nullifiers<AccountUuid>,
    keys: &ScanningKeys<AccountUuid, IvkTag>,
    from_height: BlockHeight,
    blocks: &[CompactBlock],
    mut scanned: Vec<NativeScanned>,
    first_receive: usize,
    scanned_tick: Option<&std::sync::atomic::AtomicU32>,
) -> Result<std::result::Result<Vec<NativeScanned>, Continuity>>
where
    IvkTag: Copy + std::hash::Hash + Eq + Send + Sync + 'static,
{
    // Parallel results after the first receipt used the pre-batch nullifiers.
    // Replace that suffix, and seed only from the retained prefix: later receipts
    // must not make earlier spends appear to spend a note from the future.
    let receive_height = blocks[first_receive].height;
    scanned.retain(|sb| u64::from(u32::from(sb.height())) <= receive_height);
    if first_receive + 1 >= blocks.len() {
        return Ok(Ok(scanned));
    }
    for sb in &scanned {
        nfs.update_with(sb);
    }
    let _ = from_height;
    for block in &blocks[first_receive + 1..] {
        if !compact_has_shielded(block) {
            continue;
        }
        match scan_block(&network, block.clone(), keys, &nfs, None) {
            Err(err) => return Ok(Err(continuity_or_err(err)?)),
            Ok(sb) => {
                nfs.update_with(&sb);
                tick_scanned(scanned_tick, sb.height());
                scanned.push(sb);
            }
        }
    }
    Ok(Ok(scanned))
}

pub(super) fn flush_remaining(
    db: &mut super::wallet::SyncDb,
    off: &mut NativeOffload,
) -> Result<()> {
    if !off.frontiers && !off.acc.has_pending() && off.checkpoint_span.is_empty() {
        return Ok(());
    }
    let mut acc = off.acc.clone();
    acc.flush();
    let height = off.checkpoint_span.last().map(|b| b.height).unwrap_or(
        db.chain_height()
            .map_err(|e| EngineError::WalletDb(format!("chain_height: {e}")))?
            .unwrap_or(BlockHeight::from_u32(0)),
    );
    let keep_from = keep_from_height(u32::from(height));
    let sap = acc.sapling.drain_runs();
    let orch = acc.orchard.drain_runs();
    let iron = acc.ironwood.drain_runs();
    if sap.is_empty() && orch.is_empty() && iron.is_empty() && off.checkpoint_span.is_empty() {
        off.acc = acc;
        info!("selective-scan historic complete");
        return Ok(());
    }
    db.transactionally(|wdb| {
        if let Some(expected) = &off.persisted_identity {
            let current = ScanIdentity::new(
                &wdb.get_unified_full_viewing_keys()?,
                &Nullifiers::unspent(wdb)?,
            );
            if expected.as_ref() != &current {
                return Err(changed_scan_inputs());
            }
        }
        if let Some(last) = off.checkpoint_span.last() {
            let actual = wdb
                .block_metadata(last.height)?
                .ok_or_else(changed_scan_inputs)?;
            if actual.block_hash() != last.hash
                || actual.sapling_tree_size() != Some(last.sapling_tree_size)
                || actual.orchard_tree_size() != Some(last.orchard_tree_size)
                || actual.ironwood_tree_size() != Some(last.ironwood_tree_size)
            {
                return Err(changed_scan_inputs());
            }
        }

        for run in &sap {
            insert_sapling_run(wdb, run, keep_from)?;
        }
        for run in &orch {
            insert_orchard_run(wdb, run, keep_from)?;
        }
        for run in &iron {
            insert_ironwood_run(wdb, run, keep_from)?;
        }
        checkpoint_compact_span(wdb, &off.checkpoint_span)?;
        Ok::<_, SqliteClientError>(())
    })
    .map_err(|e| map_persist_error("flush", e))?;
    off.acc = acc;
    off.checkpoint_span.clear();
    info!("selective-scan historic complete");
    Ok(())
}

fn persist(
    db: &mut super::wallet::SyncDb,
    from_state: &ChainState,
    row_runs: &[RowRun],
    read_fences: &[Arc<ScanIdentity>],
    scanned: Vec<zcash_client_backend::data_api::ScannedBlock<zcash_client_sqlite::AccountUuid>>,
    off: &mut NativeOffload,
    span: &[BlockWatermark],
    watermark: &BlockWatermark,
    start_sizes: (u32, u32, u32),
) -> Result<()> {
    if read_fences.is_empty() {
        return Err(EngineError::Message(
            "selective-scan persist has no scan-input snapshot".into(),
        ));
    }
    let last_height = watermark.height;
    let span_heights: Vec<u32> = span.iter().map(|w| u32::from(w.height)).collect();
    if !heights_are_sequential(&span_heights) {
        return Err(EngineError::Message(
            "selective-scan persist span is not sequential".into(),
        ));
    }
    let mut acc = off.acc.clone();
    let mut frontiers = off.frontiers;
    acc.retain_checkpoints_from(keep_from_height(u32::from(last_height)));
    let keep_from = keep_from_height(u32::from(last_height));
    let sap_pos = u64::from(start_sizes.0);
    let orch_pos = u64::from(start_sizes.1);
    let iron_pos = u64::from(start_sizes.2);
    let height_u32 = u32::from(last_height);

    let n_notes = scanned.iter().filter(|b| wallet_activity(b)).count();
    let scanned_heights: Vec<u32> = scanned.iter().map(|b| u32::from(b.height())).collect();
    let put_slices = put_blocks_height_slices(&scanned_heights);
    if put_slices.iter().any(|s| !heights_are_sequential(s)) {
        return Err(EngineError::Message(
            "selective-scan put_blocks slice is not sequential".into(),
        ));
    }

    let persisted_identity = db
        .transactionally(|wdb| {
            // These reads establish the same SQLite snapshot that will be upgraded
            // for writes. A later external commit causes an upgrade failure rather
            // than publishing stale relevance decisions.
            let current_keys = wdb.get_unified_full_viewing_keys()?;
            let current_nfs = Nullifiers::unspent(wdb)?;
            let identity = ScanIdentity::new(&current_keys, &current_nfs);
            if read_fences
                .iter()
                .any(|expected| expected.as_ref() != &identity)
            {
                return Err(changed_scan_inputs());
            }
            info!("wallet write input validation");

            let (mut sap, mut orch, mut iron) = collect_commitments(&scanned);
            if !frontiers {
                insert_frontiers(wdb, from_state)?;
                frontiers = true;
            }
            // A recent empty height can still use a boundary whose last
            // commitment predates the retention window. Preserve that exact
            // prefix before any new leaves can compact it into a larger node.
            let recent = off
                .checkpoint_span
                .iter()
                .chain(span)
                .filter(|b| b.height >= keep_from);
            let sap_boundaries = retain_recent_boundaries(
                &mut acc.sapling,
                sap_pos,
                &mut sap,
                recent.clone().map(|b| (b.height, b.sapling_tree_size)),
            );
            let orch_boundaries = retain_recent_boundaries(
                &mut acc.orchard,
                orch_pos,
                &mut orch,
                recent.clone().map(|b| (b.height, b.orchard_tree_size)),
            );
            let iron_boundaries = retain_recent_boundaries(
                &mut acc.ironwood,
                iron_pos,
                &mut iron,
                recent.map(|b| (b.height, b.ironwood_tree_size)),
            );
            if !sap_boundaries.is_empty() {
                wdb.with_sapling_tree_mut(|tree| {
                    checkpoint_exact_positions(tree, sap_boundaries.iter().copied())
                })?;
            }
            if !orch_boundaries.is_empty() {
                wdb.with_orchard_tree_mut(|tree| {
                    checkpoint_exact_positions(tree, orch_boundaries.iter().copied())
                })?;
            }
            if !iron_boundaries.is_empty() {
                wdb.with_ironwood_tree_mut(|tree| {
                    checkpoint_exact_positions(tree, iron_boundaries.iter().copied())
                })?;
            }
            info!("wallet write retained boundaries");

            acc.sapling.feed(sap_pos, &sap, height_u32);
            acc.orchard.feed(orch_pos, &orch, height_u32);
            acc.ironwood.feed(iron_pos, &iron, height_u32);
            acc.flush_durable();
            for run in acc.sapling.drain_runs() {
                insert_sapling_run(wdb, &run, keep_from)?;
            }
            for run in acc.orchard.drain_runs() {
                insert_orchard_run(wdb, &run, keep_from)?;
            }
            for run in acc.ironwood.drain_runs() {
                insert_ironwood_run(wdb, &run, keep_from)?;
            }
            info!("wallet write commitment rows");

            let mut note_positions = Vec::new();
            // Empty persist: watermark + scan_queue only. Do not invent a
            // `[birthday, birthday+7999]` put_blocks list with holes.
            if n_notes > 0 {
                let mut blocks = scanned.into_iter().peekable();
                for row_run in row_runs {
                    while blocks.peek().is_some_and(|b| b.height() < row_run.first) {
                        if wallet_activity(&blocks.next().expect("peeked block")) {
                            return Err(SqliteClientError::CorruptedData(
                                "wallet row outside captured frontier run".into(),
                            ));
                        }
                    }
                    let mut run = Vec::new();
                    while blocks.peek().is_some_and(|b| b.height() <= row_run.last) {
                        run.push(blocks.next().expect("peeked block"));
                    }
                    if run.first().map(|b| b.height()) != Some(row_run.first)
                        || run.last().map(|b| b.height()) != Some(row_run.last)
                        || run.windows(2).any(|b| b[1].height() != b[0].height() + 1)
                        || !run.iter().any(wallet_activity)
                    {
                        return Err(SqliteClientError::CorruptedData(
                            "captured wallet row run does not match scanned blocks".into(),
                        ));
                    }
                    let rows = put_blocks_rows::<_, SqliteClientError, commitment_tree::Error>(
                        wdb,
                        GapLimits::default(),
                        &row_run.prior,
                        run,
                    )
                    .map_err(SqliteClientError::from)?;
                    note_positions.extend(rows.note_positions);
                    // Commitments were already hashed during the captured walk and
                    // fed to Offload above. Dropping these avoids a second walk.
                }
                if blocks.any(|block| wallet_activity(&block)) {
                    return Err(SqliteClientError::CorruptedData(
                        "wallet rows remain after captured frontier runs".into(),
                    ));
                }
            }
            wdb.put_block_meta(
                watermark.height,
                watermark.hash,
                watermark.time,
                watermark.sapling_tree_size,
                watermark.sapling_output_count,
                watermark.orchard_tree_size,
                watermark.orchard_action_count,
                watermark.ironwood_tree_size,
                watermark.ironwood_action_count,
            )?;
            wdb.notify_scan_complete(
                span.first().expect("nonempty persist span").height..(last_height + 1),
                &note_positions,
            )?;
            info!("wallet write scan rows and watermark");
            // Historical replay can revisit a receipt whose later spend is
            // already known. Preserve SQLite's reconciled truth, rather than
            // inferring post-write unspent notes from this compact span alone.
            Ok::<_, SqliteClientError>(Arc::new(
                identity.with_nullifiers(&Nullifiers::unspent(wdb)?),
            ))
        })
        .map_err(|e| map_persist_error("put", e))?;
    off.acc = acc;
    off.frontiers = frontiers;
    off.persisted_identity = Some(persisted_identity);
    off.checkpoint_span.extend_from_slice(span);
    off.checkpoint_span
        .retain(|block| block.height >= keep_from);
    info!("selective-scan persist");
    Ok(())
}

fn collect_commitments(
    scanned: &[NativeScanned],
) -> (
    Vec<(sapling::Node, Retention<BlockHeight>)>,
    Vec<(MerkleHashOrchard, Retention<BlockHeight>)>,
    Vec<(MerkleHashOrchard, Retention<BlockHeight>)>,
) {
    let mut sap = Vec::new();
    let mut orch = Vec::new();
    let mut iron = Vec::new();
    for sb in scanned {
        sap.extend(sb.sapling().commitments().iter().cloned());
        orch.extend(sb.orchard().commitments().iter().cloned());
        iron.extend(sb.ironwood().commitments().iter().cloned());
    }
    (sap, orch, iron)
}

/// Tag not-yet-inserted leaves using actual compact heights. Return only the
/// boundaries already stored in SQLite, which must be retained before append.
fn retain_recent_boundaries<H: NoteLeaf>(
    acc: &mut PoolAcc<H>,
    start: u64,
    leaves: &mut [(H, Retention<BlockHeight>)],
    positions: impl Iterator<Item = (BlockHeight, u32)>,
) -> Vec<(BlockHeight, u32)> {
    let mut latest = BTreeMap::new();
    for (height, size) in positions {
        if size != 0 {
            latest.insert(size, height);
        }
    }
    let mut stored = Vec::new();
    for (size, height) in latest {
        let position = u64::from(size - 1);
        if let Some((_, retention)) = position
            .checked_sub(start)
            .and_then(|offset| leaves.get_mut(offset as usize))
        {
            *retention = checkpoint_retention(retention, height);
        } else if !acc.retain_checkpoint(position, height) {
            stored.push((height, size));
        }
    }
    stored
}

/// Add exact checkpoints only after all buffered leaves have been flushed. An
/// empty pool must advance too: Zakura uses a common Sapling/Orchard anchor.
fn checkpoint_compact_span<W>(
    wdb: &mut W,
    span: &[BlockWatermark],
) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees + LowLevelWalletWrite<Error = SqliteClientError>,
    SqliteClientError: From<ShardTreeError<<W as WalletCommitmentTrees>::Error>>,
{
    wdb.with_sapling_tree_mut(|tree| {
        checkpoint_exact_positions(tree, span.iter().map(|b| (b.height, b.sapling_tree_size)))
    })?;
    wdb.with_orchard_tree_mut(|tree| {
        checkpoint_exact_positions(tree, span.iter().map(|b| (b.height, b.orchard_tree_size)))
    })?;
    wdb.with_ironwood_tree_mut(|tree| {
        checkpoint_exact_positions(tree, span.iter().map(|b| (b.height, b.ironwood_tree_size)))
    })?;
    // Keep the exact compact evidence for recovery and legacy-checkpoint repair.
    for block in span {
        wdb.put_block_meta(
            block.height,
            block.hash,
            block.time,
            block.sapling_tree_size,
            block.sapling_output_count,
            block.orchard_tree_size,
            block.orchard_action_count,
            block.ironwood_tree_size,
            block.ironwood_action_count,
        )?;
    }
    Ok(())
}

fn checkpoint_exact_positions<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<S, DEPTH, SHARD>,
    positions: impl Iterator<Item = (BlockHeight, u32)>,
) -> StdResult<(), SqliteClientError>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    SqliteClientError: From<ShardTreeError<S::Error>>,
{
    use shardtree::{store::Checkpoint, LocatedPrunableTree, RetentionFlags, Tree};
    let mut empty = Vec::new();
    let mut groups: BTreeMap<
        u32,
        (
            Option<LocatedPrunableTree<H>>,
            BTreeMap<BlockHeight, Position>,
        ),
    > = BTreeMap::new();
    for (height, size) in positions {
        let existing = tree
            .store()
            .get_checkpoint(&height)
            .map_err(ShardTreeError::Storage)?;
        let expected = size.checked_sub(1).map(|p| Position::from(u64::from(p)));
        if existing.as_ref().is_some_and(|c| c.position() != expected) {
            return Err(SqliteClientError::CorruptedData(format!(
                "checkpoint at {height} disagrees with verified compact tree size {size}"
            )));
        }
        let Some(position) = expected else {
            if existing.is_none() {
                empty.push(height);
            }
            continue;
        };
        if !groups.contains_key(&size) {
            let boundary = stored_checkpoint_boundary(tree, position)?;
            let flagged = boundary
                .as_ref()
                .is_some_and(|(_, _, flags)| flags.is_checkpoint());
            if !flagged || existing.is_none() {
                // A future root is not evidence for an earlier prefix. Validate
                // the exact compact-derived boundary before changing any pool.
                tree.root(
                    ShardTree::<S, DEPTH, SHARD>::root_addr(),
                    Position::from(u64::from(size)),
                )
                .map_err(|error| {
                    SqliteClientError::CorruptedData(format!(
                        "checkpoint at {height} tree size {size}: {error}"
                    ))
                })?;
            }
            let retained_boundary = if flagged {
                None
            } else {
                let (address, hash, flags) = match boundary {
                    Some(boundary) => boundary,
                    None => {
                        // A complete shard may be represented only by a cached
                        // root. Materialize that actual root, never an unknown
                        // interior leaf, before retaining its end position.
                        let address = ShardTree::<S, DEPTH, SHARD>::subtree_addr(position);
                        if address.max_position() != position {
                            return Err(SqliteClientError::CorruptedData(format!(
                                "checkpoint boundary {position:?} is not available"
                            )));
                        }
                        (
                            address,
                            tree.root(address, position + 1)?,
                            RetentionFlags::EPHEMERAL,
                        )
                    }
                };
                Some(
                    LocatedPrunableTree::from_parts(
                        address,
                        Tree::leaf((hash, flags | RetentionFlags::CHECKPOINT)),
                    )
                    .expect("a leaf fits its existing address"),
                )
            };
            groups.insert(size, (retained_boundary, BTreeMap::new()));
        }
        if existing.is_none() {
            groups.get_mut(&size).unwrap().1.insert(height, position);
        }
    }
    for height in &empty {
        tree.store_mut()
            .add_checkpoint(*height, Checkpoint::tree_empty())
            .map_err(ShardTreeError::Storage)?;
    }
    let mut changed = false;
    for (_, (boundary, checkpoints)) in groups {
        if boundary.is_none() && checkpoints.is_empty() {
            continue;
        }
        tree.insert_tree(
            boundary.unwrap_or_else(|| {
                LocatedPrunableTree::empty(Address::from_parts(Level::from(SHARD), 0))
            }),
            checkpoints,
        )?;
        changed = true;
    }
    if !empty.is_empty() && !changed {
        // Empty trees still require normal upstream checkpoint pruning.
        tree.insert_tree(
            LocatedPrunableTree::empty(Address::from_parts(Level::from(SHARD), 0)),
            BTreeMap::new(),
        )?;
    }
    Ok(())
}

/// Repair the tip and policy-selected spending anchor of an already scanned
/// wallet. False requests verified compact metadata or authentic frontiers;
/// storage failures and contradictory persisted positions remain errors.
pub(super) fn repair_persisted_checkpoints(
    db: &mut super::wallet::SyncDb,
    _db_path: &std::path::Path,
    scanned_tip: u32,
    required_anchor: u32,
) -> Result<bool> {
    if required_anchor > scanned_tip || scanned_tip - required_anchor > 100 {
        return Err(EngineError::Message(
            "checkpoint recovery anchor is outside the recent window".into(),
        ));
    }
    db.transactionally(|db| {
        if db
            .block_fully_scanned()?
            .map(|b| u32::from(b.block_height()))
            != Some(scanned_tip)
        {
            return Err(SqliteClientError::CorruptedData(
                "checkpoint recovery requires a fully scanned tip".into(),
            ));
        }
        let mut positions = Vec::new();
        for height in [required_anchor, scanned_tip] {
            if positions
                .last()
                .is_some_and(|(h, _, _, _)| *h == BlockHeight::from(height))
            {
                continue;
            }
            let Some(block) = db.block_metadata(height.into())? else {
                return Ok(false);
            };
            let (Some(s), Some(o), Some(i)) = (
                block.sapling_tree_size(),
                block.orchard_tree_size(),
                block.ironwood_tree_size(),
            ) else {
                return Ok(false);
            };
            positions.push((block.block_height(), s, o, i));
        }
        let mut complete = true;
        for &(height, s, o, i) in &positions {
            let sap = db.with_sapling_tree_mut(|tree| checkpoint_matches(tree, height, s))?;
            let orch = db.with_orchard_tree_mut(|tree| checkpoint_matches(tree, height, o))?;
            let iron = db.with_ironwood_tree_mut(|tree| checkpoint_matches(tree, height, i))?;
            complete &= sap && orch && iron.unwrap_or(true);
        }
        if complete {
            return Ok(true);
        }
        // Preflight every required position before any pool is modified. A
        // pruned legacy prefix needs authenticated network data, not a reset.
        for &(height, s, o, i) in &positions {
            let sap = db.with_sapling_tree_mut(|tree| checkpoint_coverage(tree, height, s))?;
            let orch = db.with_orchard_tree_mut(|tree| checkpoint_coverage(tree, height, o))?;
            let iron = db.with_ironwood_tree_mut(|tree| checkpoint_coverage(tree, height, i))?;
            if !(sap && orch && iron.unwrap_or(true)) {
                return Ok(false);
            }
        }
        db.with_sapling_tree_mut(|tree| {
            checkpoint_exact_positions(tree, positions.iter().map(|&(h, s, _, _)| (h, s)))
        })?;
        db.with_orchard_tree_mut(|tree| {
            checkpoint_exact_positions(tree, positions.iter().map(|&(h, _, o, _)| (h, o)))
        })?;
        db.with_ironwood_tree_mut(|tree| {
            checkpoint_exact_positions(tree, positions.iter().map(|&(h, _, _, i)| (h, i)))
        })?;
        Ok::<_, SqliteClientError>(true)
    })
    .map_err(|e| EngineError::WalletDb(format!("checkpoint recovery: {e}")))
}

fn checkpoint_coverage<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<S, DEPTH, SHARD>,
    height: BlockHeight,
    size: u32,
) -> StdResult<bool, SqliteClientError>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    SqliteClientError: From<ShardTreeError<S::Error>>,
{
    let expected = size.checked_sub(1).map(|p| Position::from(u64::from(p)));
    if tree
        .store()
        .get_checkpoint(&height)
        .map_err(ShardTreeError::Storage)?
        .is_some_and(|c| c.position() != expected)
    {
        return Err(SqliteClientError::CorruptedData(format!(
            "checkpoint {height} disagrees with verified tree size {size}"
        )));
    }
    let Some(position) = expected else {
        return Ok(true);
    };
    match tree.root(ShardTree::<S, DEPTH, SHARD>::root_addr(), position + 1) {
        Ok(_) => {}
        Err(ShardTreeError::Query(shardtree::error::QueryError::TreeIncomplete(_))) => {
            return Ok(false)
        }
        Err(e) => return Err(e.into()),
    }
    Ok(stored_checkpoint_boundary(tree, position)?.is_some()
        || ShardTree::<S, DEPTH, SHARD>::subtree_addr(position).max_position() == position)
}

fn checkpoint_matches<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<S, DEPTH, SHARD>,
    height: BlockHeight,
    size: u32,
) -> StdResult<bool, SqliteClientError>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    SqliteClientError: From<ShardTreeError<S::Error>>,
{
    let matches = tree
        .store()
        .get_checkpoint(&height)
        .map_err(ShardTreeError::Storage)?
        .is_some_and(|c| c.position() == size.checked_sub(1).map(|p| Position::from(u64::from(p))));
    if !matches || size == 0 {
        return Ok(matches);
    }
    Ok(
        stored_checkpoint_boundary(tree, Position::from(u64::from(size - 1)))?
            .is_some_and(|(_, _, flags)| flags.is_checkpoint()),
    )
}

/// Replay only verified recent compact metadata to recover missing anchors in
/// an otherwise fully scanned wallet. Receipts, spends and nullifiers are not
/// scanned again; existing tree coverage is required for every exact position.
pub(super) fn repair_checkpoints_from_compacts(
    db: &mut super::wallet::SyncDb,
    scanned_tip: u32,
    blocks: &[CompactBlock],
    frontiers: &[ChainState],
) -> Result<Vec<u32>> {
    if blocks.is_empty()
        || blocks.len() > 101
        || blocks
            .last()
            .is_none_or(|b| b.height != u64::from(scanned_tip))
        || blocks
            .first()
            .is_some_and(|b| b.height < u64::from(u32::from(keep_from_height(scanned_tip))))
    {
        return Err(EngineError::Message(
            "checkpoint recovery requires a bounded recent range ending at tip".into(),
        ));
    }
    if blocks.iter().any(|b| b.chain_metadata.is_none())
        || blocks
            .windows(2)
            .any(|pair| pair[1].height != pair[0].height + 1 || pair[1].prev_hash != pair[0].hash)
    {
        return Err(EngineError::Message(
            "checkpoint recovery compact range is incomplete or disconnected".into(),
        ));
    }
    let span = compact_span(blocks)?;
    for pair in span.windows(2) {
        let previous = watermark_sizes(&pair[0]);
        let actual = watermark_sizes(&pair[1]);
        let counts = (
            pair[1].sapling_output_count,
            pair[1].orchard_action_count,
            pair[1].ironwood_action_count,
        );
        if previous.0.checked_add(counts.0) != Some(actual.0)
            || previous.1.checked_add(counts.1) != Some(actual.1)
            || previous.2.checked_add(counts.2) != Some(actual.2)
        {
            return Err(EngineError::Message(
                "checkpoint recovery compact tree sizes are inconsistent".into(),
            ));
        }
    }
    let mut supplied = BTreeMap::new();
    for frontier in frontiers {
        let height = frontier.block_height();
        let Some(block) = span.iter().find(|b| b.height == height) else {
            return Err(EngineError::Message(
                "checkpoint recovery frontier is outside the verified compact range".into(),
            ));
        };
        if frontier.block_hash() != block.hash
            || frontier.final_sapling_tree().tree_size() != u64::from(block.sapling_tree_size)
            || frontier.final_orchard_tree().tree_size() != u64::from(block.orchard_tree_size)
            || frontier.final_ironwood_tree().tree_size() != u64::from(block.ironwood_tree_size)
            || supplied.insert(height, frontier).is_some()
        {
            return Err(EngineError::Message(
                "checkpoint recovery frontier does not match its verified compact block".into(),
            ));
        }
    }
    db.transactionally(|db| {
        let tip = db
            .block_fully_scanned()?
            .filter(|b| u32::from(b.block_height()) == scanned_tip)
            .ok_or_else(|| {
                SqliteClientError::CorruptedData(
                    "checkpoint recovery requires the fully scanned tip".into(),
                )
            })?;
        let final_block = span.last().expect("nonempty recovery span");
        if tip.block_hash() != final_block.hash
            || tip.sapling_tree_size() != Some(final_block.sapling_tree_size)
            || tip.orchard_tree_size() != Some(final_block.orchard_tree_size)
            || tip.ironwood_tree_size() != Some(final_block.ironwood_tree_size)
        {
            return Err(SqliteClientError::CorruptedData(
                "checkpoint recovery compact tip does not match persisted tip".into(),
            ));
        }
        if let Some(prior_height) = u32::from(span[0].height).checked_sub(1) {
            if let Some(prior) = db.block_metadata(prior_height.into())? {
                if prior.block_hash().0.as_slice() != blocks[0].prev_hash.as_slice() {
                    return Err(SqliteClientError::CorruptedData(
                        "checkpoint recovery compact start does not match persisted predecessor"
                            .into(),
                    ));
                }
            }
        }
        let mut needs_graft = false;
        for block in &span {
            let sap = db.with_sapling_tree_mut(|tree| {
                checkpoint_coverage(tree, block.height, block.sapling_tree_size)
            })?;
            let orch = db.with_orchard_tree_mut(|tree| {
                checkpoint_coverage(tree, block.height, block.orchard_tree_size)
            })?;
            let iron = db.with_ironwood_tree_mut(|tree| {
                checkpoint_coverage(tree, block.height, block.ironwood_tree_size)
            })?;
            needs_graft |= !(sap && orch && iron.unwrap_or(true));
        }
        let mut missing = Vec::new();
        let mut needed_sizes = Vec::new();
        if needs_graft {
            // Expanding a cached subtree at an earlier prefix can leave its
            // later boundary represented by an annotation without a terminal
            // node. Supply all distinct required frontiers together so both
            // boundaries can be retained atomically. Native policy requests
            // only its one- or three-block confirmation window.
            for block in &span {
                let sizes = watermark_sizes(block);
                if sizes != (0, 0, 0)
                    && !supplied.values().any(|f| {
                        (
                            f.final_sapling_tree().tree_size(),
                            f.final_orchard_tree().tree_size(),
                            f.final_ironwood_tree().tree_size(),
                        ) == (u64::from(sizes.0), u64::from(sizes.1), u64::from(sizes.2))
                    })
                    && !needed_sizes.contains(&sizes)
                {
                    missing.push(u32::from(block.height));
                    needed_sizes.push(sizes);
                }
            }
        }
        if !missing.is_empty() {
            // No graft, metadata row or checkpoint is published until all
            // unavailable prefixes have been supplied and validated.
            return Ok(missing);
        }
        for frontier in supplied.values() {
            graft_recovery_frontier(db, frontier)?;
        }
        checkpoint_compact_span(db, &span)?;
        Ok(Vec::new())
    })
    .map_err(|e| EngineError::WalletDb(format!("compact checkpoint recovery: {e}")))
}

/// Graft only authenticated nodes, retaining each exact boundary immediately
/// so a later frontier in the same transaction cannot prune an earlier anchor.
fn graft_recovery_frontier<W>(wdb: &mut W, state: &ChainState) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees,
    SqliteClientError: From<ShardTreeError<W::Error>>,
{
    wdb.with_sapling_tree_mut(|tree| {
        tree.insert_frontier(state.final_sapling_tree().clone(), Retention::Ephemeral)?;
        checkpoint_exact_positions(
            tree,
            [(
                state.block_height(),
                state.final_sapling_tree().tree_size() as u32,
            )]
            .into_iter(),
        )
    })?;
    wdb.with_orchard_tree_mut(|tree| {
        tree.insert_frontier(state.final_orchard_tree().clone(), Retention::Ephemeral)?;
        checkpoint_exact_positions(
            tree,
            [(
                state.block_height(),
                state.final_orchard_tree().tree_size() as u32,
            )]
            .into_iter(),
        )
    })?;
    wdb.with_ironwood_tree_mut(|tree| {
        tree.insert_frontier(state.final_ironwood_tree().clone(), Retention::Ephemeral)?;
        checkpoint_exact_positions(
            tree,
            [(
                state.block_height(),
                state.final_ironwood_tree().tree_size() as u32,
            )]
            .into_iter(),
        )
    })?;
    Ok(())
}

fn insert_frontiers<W>(wdb: &mut W, from_state: &ChainState) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees,
    SqliteClientError: From<ShardTreeError<W::Error>>,
{
    let ret = Retention::Checkpoint {
        id: from_state.block_height(),
        marking: Marking::Reference,
    };
    wdb.with_sapling_tree_mut(|tree| {
        if prepare_frontier_insert(
            tree,
            from_state.block_height(),
            from_state.final_sapling_tree().tree_size(),
            from_state.final_sapling_tree().root(),
        ) {
            tree.insert_frontier(from_state.final_sapling_tree().clone(), ret.clone())?;
        }
        Ok::<(), ShardTreeError<W::Error>>(())
    })?;
    wdb.with_orchard_tree_mut(|tree| {
        if prepare_frontier_insert(
            tree,
            from_state.block_height(),
            from_state.final_orchard_tree().tree_size(),
            from_state.final_orchard_tree().root(),
        ) {
            tree.insert_frontier(from_state.final_orchard_tree().clone(), ret.clone())?;
        }
        Ok::<(), ShardTreeError<W::Error>>(())
    })?;
    wdb.with_ironwood_tree_mut(|tree| {
        if prepare_frontier_insert(
            tree,
            from_state.block_height(),
            from_state.final_ironwood_tree().tree_size(),
            from_state.final_ironwood_tree().root(),
        ) {
            tree.insert_frontier(from_state.final_ironwood_tree().clone(), ret)?;
        }
        Ok::<(), ShardTreeError<W::Error>>(())
    })?;
    Ok(())
}

fn prepare_frontier_insert<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<S, DEPTH, SHARD>,
    id: BlockHeight,
    tree_size: u64,
    expected_root: H,
) -> bool
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    S::Error: core::fmt::Debug,
{
    let covered = tree
        .root(
            ShardTree::<S, DEPTH, SHARD>::root_addr(),
            Position::from(tree_size),
        )
        .is_ok_and(|root| root == expected_root);
    let want = tree_size.saturating_sub(1);
    if let Ok(Some(existing)) = tree.store().get_checkpoint(&id) {
        let have = existing.position().map(u64::from).unwrap_or(0);
        if have >= want && covered {
            return false;
        }
        let _ = tree.store_mut().remove_checkpoint(&id);
    }
    // Empty persist can walk `from_state` height forward while trees stay at
    // the birthday frontier. Do not add a later checkpoint at that same pos
    // (that is the 3340376 / insert_tree conflict).
    let mut same_pos = false;
    let _ = tree.store().for_each_checkpoint(4096, |_, ckpt| {
        if ckpt.position().map(u64::from) == Some(want) {
            same_pos = true;
        }
        Ok(())
    });
    !same_pos || !covered
}

fn insert_sapling_run<W>(
    wdb: &mut W,
    run: &KeptRun<sapling::Node>,
    keep_from: BlockHeight,
) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees,
    SqliteClientError: From<ShardTreeError<W::Error>>,
{
    wdb.with_sapling_tree_mut(|tree| {
        insert_leaf_run(tree, run.start, &run.leaves, run.historic, keep_from)
            .map_err(|e| SqliteClientError::CorruptedData(e.to_string()))
    })?;
    if run.historic && is_full_shard_run(run) && run.leaves.iter().any(|(_, r)| is_marked(r)) {
        let shard = shard_of(run.start);
        if let Some(want) = wdb.get_sapling_subtree_root(shard).map_err(Into::into)? {
            wdb.with_sapling_tree_mut(|tree| verify_root(tree, shard, &want, "sapling"))?;
        }
    }
    Ok(())
}

fn insert_orchard_run<W>(
    wdb: &mut W,
    run: &KeptRun<MerkleHashOrchard>,
    keep_from: BlockHeight,
) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees,
    SqliteClientError: From<ShardTreeError<W::Error>>,
{
    wdb.with_orchard_tree_mut(|tree| {
        insert_leaf_run(tree, run.start, &run.leaves, run.historic, keep_from)
            .map_err(|e| SqliteClientError::CorruptedData(e.to_string()))
    })?;
    if run.historic && is_full_shard_run(run) && run.leaves.iter().any(|(_, r)| is_marked(r)) {
        let shard = shard_of(run.start);
        if let Some(want) = wdb.get_orchard_subtree_root(shard).map_err(Into::into)? {
            wdb.with_orchard_tree_mut(|tree| verify_root(tree, shard, &want, "orchard"))?;
        }
    }
    Ok(())
}

fn insert_ironwood_run<W>(
    wdb: &mut W,
    run: &KeptRun<MerkleHashOrchard>,
    keep_from: BlockHeight,
) -> StdResult<(), SqliteClientError>
where
    W: WalletCommitmentTrees,
    SqliteClientError: From<ShardTreeError<W::Error>>,
{
    let _ = wdb.with_ironwood_tree_mut(|tree| {
        insert_leaf_run(tree, run.start, &run.leaves, run.historic, keep_from)
            .map_err(|e| SqliteClientError::CorruptedData(e.to_string()))
    })?;
    if run.historic && is_full_shard_run(run) && run.leaves.iter().any(|(_, r)| is_marked(r)) {
        let shard = shard_of(run.start);
        if let Some(want) = wdb.get_ironwood_subtree_root(shard).map_err(Into::into)? {
            let _ =
                wdb.with_ironwood_tree_mut(|tree| verify_root(tree, shard, &want, "ironwood"))?;
        }
    }
    Ok(())
}

fn verify_root<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<S, DEPTH, SHARD>,
    shard: u64,
    want: &H,
    pool: &str,
) -> StdResult<(), SqliteClientError>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    S::Error: core::fmt::Debug,
{
    let addr = Address::from_parts(Level::from(SHARD), shard);
    let got = tree.root(addr, Position::from(u64::MAX)).map_err(|e| {
        SqliteClientError::CorruptedData(format!("{pool} selective-scan shard {shard}: {e}"))
    })?;
    if &got != want {
        return Err(SqliteClientError::CorruptedData(format!(
            "{pool} selective-scan shard {shard} root mismatch; rescan"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    include!("selective_scan_tests.rs");

    #[test]
    fn tiny_historic_uses_selective_scan_when_roots_exist() {
        let off = NativeOffload::new(10, 20, 0);
        assert!(
            off.should_use(10),
            "10-block Verify lookahead must not fall back to scan_cached_blocks"
        );
        assert!(off.should_use(4_000));
        assert!(
            NativeOffload::new(0, 0, 0).should_use(4_000),
            "birthday wipe before GetSubtreeRoots lands must not stock-scan 4000"
        );
        assert!(!off.should_use(0));
    }

    #[test]
    fn empty_compact_is_filtered_from_trial_decrypt() {
        use zcash_client_backend::proto::compact_formats::{CompactOrchardAction, CompactTx};
        let empty = CompactBlock::default();
        assert!(
            !compact_has_shielded(&empty),
            "hot loop must skip scan_block on empty compact"
        );
        let mut tx = CompactTx::default();
        tx.actions.push(CompactOrchardAction::default());
        let mut shielded = CompactBlock::default();
        shielded.vtx.push(tx);
        let kept: Vec<_> = [&empty, &shielded]
            .into_iter()
            .filter(|b| compact_has_shielded(b))
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(compact_has_shielded(kept[0]));
    }

    #[test]
    fn historic_persist_coalesces_8000() {
        assert_eq!(HISTORIC_PERSIST_BLOCKS, 8_000);
        assert!(HISTORIC_PERSIST_BLOCKS >= crate::scan::BATCH_LOCAL);
    }

    #[test]
    fn persist_put_blocks_slices_stay_sequential_across_empty_heights() {
        // Contiguous compact span birthday..=birthday+7. Skip-empty decrypt
        // keeps only shielded heights — holes at +1, +3..=+4, +6.
        let birthday = 3_335_466u32;
        let span: Vec<u32> = (birthday..=birthday + 7).collect();
        let scanned = vec![birthday, birthday + 2, birthday + 5, birthday + 7];
        assert!(
            heights_are_sequential(&span),
            "original compact list must stay sequential"
        );
        assert!(
            !heights_are_sequential(&scanned),
            "shielded-only persist batch is gappy"
        );
        let slices = put_blocks_height_slices(&scanned);
        assert_eq!(
            slices,
            vec![
                vec![birthday],
                vec![birthday + 2],
                vec![birthday + 5],
                vec![birthday + 7],
            ]
        );
        for slice in &slices {
            assert!(
                heights_are_sequential(slice),
                "put_blocks slice must be sequential, got {slice:?}"
            );
        }
        let consecutive = vec![birthday + 2, birthday + 3, birthday + 4];
        assert_eq!(
            put_blocks_height_slices(&consecutive),
            vec![consecutive.clone()]
        );
        assert!(heights_are_sequential(&consecutive));
        let gappy_8000 = vec![birthday, birthday + 7_999];
        let watermark_slices = put_blocks_height_slices(&gappy_8000);
        assert_eq!(watermark_slices.len(), 2);
        assert!(
            watermark_slices
                .iter()
                .all(|s| heights_are_sequential(s) && s.len() == 1),
            "8000 watermark must not hand put_blocks [birthday, birthday+7999] with a hole"
        );
    }

    #[test]
    fn checkpoint_position_alone_does_not_prove_frontier_coverage() {
        use incrementalmerkletree::Hashable;
        use incrementalmerkletree::Position;
        use shardtree::store::memory::MemoryShardStore;
        let mut tree: ShardTree<
            MemoryShardStore<sapling::Node, BlockHeight>,
            { crate::offload::SHARD_HEIGHT * 2 },
            { crate::offload::SHARD_HEIGHT },
        > = ShardTree::new(MemoryShardStore::empty(), 32);
        let birthday = BlockHeight::from_u32(3_335_465);
        let later = BlockHeight::from_u32(3_340_376);
        let pos = 49_991_547u64;
        tree.store_mut()
            .add_checkpoint(
                birthday,
                shardtree::store::Checkpoint::at_position(Position::from(pos)),
            )
            .unwrap();
        assert!(
            prepare_frontier_insert(
                &mut tree,
                later,
                pos + 1,
                sapling::Node::empty_root(Level::from(32))
            ),
            "a checkpoint row without tree coverage must not suppress the actual frontier graft"
        );
        assert!(tree.store().get_checkpoint(&later).unwrap().is_none());
    }
}
