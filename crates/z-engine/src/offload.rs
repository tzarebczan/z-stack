//! selective shard scanning: trial-decrypt as usual, but only *hash* shards that contain a wallet note.
//!
//! Completed shards without a marked leaf are dropped; their `GetSubtreeRoots` hash
//! already sits in the cap. Birthday (first scanned) and tip shards are always built.
//! See `docs/WEB.md` / `docs/SYNC.md` and the WebZjs selective shard scanning spec (reference only — not vendored).

use crate::error::{EngineError, Result};
use incrementalmerkletree::{Hashable, Level, Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use rayon::prelude::*;
use shardtree::store::ShardStore;
use shardtree::{LocatedPrunableTree, ShardTree};
use std::collections::BTreeMap;
use zcash_client_backend::data_api::{ORCHARD_SHARD_HEIGHT, SAPLING_SHARD_HEIGHT};
use zcash_protocol::consensus::BlockHeight;

/// Find the existing terminal node ending at the checkpoint boundary. A pruned
/// node can represent several leaves; retaining its root is enough, but adding
/// a flagged leaf above an existing parent would discard the new retention flag
/// during ShardTree's merge. Descend to an actual leaf before adding that flag.
pub(crate) fn stored_checkpoint_boundary<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<S, DEPTH, SHARD>,
    position: Position,
) -> std::result::Result<
    Option<(incrementalmerkletree::Address, H, shardtree::RetentionFlags)>,
    shardtree::error::ShardTreeError<S::Error>,
>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
{
    let mut address = ShardTree::<S, DEPTH, SHARD>::subtree_addr(position);
    let Some(shard) = tree
        .store()
        .get_shard(address)
        .map_err(shardtree::error::ShardTreeError::Storage)?
    else {
        return Ok(None);
    };
    let mut node = shard.root();
    loop {
        match &**node {
            shardtree::Node::Parent { left, right, .. } => {
                let (l_addr, r_addr) = address.children().expect("parent has children");
                if position < l_addr.position_range_end() {
                    address = l_addr;
                    node = left;
                } else {
                    address = r_addr;
                    node = right;
                }
            }
            shardtree::Node::Leaf {
                value: (hash, flags),
            } => {
                return Ok(
                    (address.max_position() == position).then(|| (address, hash.clone(), *flags))
                );
            }
            shardtree::Node::Nil => return Ok(None),
        }
    }
}

pub(crate) const SHARD_HEIGHT: u8 = SAPLING_SHARD_HEIGHT;
pub(crate) const SHARD_SIZE: u64 = 1 << SHARD_HEIGHT;
/// Native historic batches smaller than this *used* to stay on stock `scan_cached_blocks`.
/// Gap-fill now uses selective shard scanning for any non-empty batch when subtree roots exist (sqlite's
/// 10-block Verify lookahead was hashing every orchard action). Kept as the near-tip
/// heuristic size in docs/comments.
#[allow(dead_code)]
pub(crate) const SELECTIVE_SCAN_MIN_BLOCKS: usize = 200;
const CHUNK_SIZE: u64 = 1024;
pub(crate) const TIP_CHECKPOINT_BLOCKS: u32 = 100;

#[derive(Clone)]
pub(crate) struct KeptLeaf {
    pub position: u64,
    pub hash: [u8; 32],
    pub kind: u8,
    pub height: u32,
}

/// Contiguous leaves to insert into a shardtree (native sqlite or WASM replay).
#[derive(Clone)]
pub(crate) struct KeptRun<H> {
    pub start: u64,
    pub historic: bool,
    pub leaves: Vec<(H, Retention<BlockHeight>)>,
}

pub(crate) trait NoteLeaf: Hashable + Clone + PartialEq {
    fn encode(&self) -> [u8; 32];
}

impl NoteLeaf for sapling::Node {
    fn encode(&self) -> [u8; 32] {
        self.to_bytes()
    }
}

impl NoteLeaf for MerkleHashOrchard {
    fn encode(&self) -> [u8; 32] {
        self.to_bytes()
    }
}

/// The consumer owns the selected representation after draining it. Never retain
/// the other representation across batches: native clones this accumulator for
/// transactional writes, and web keeps it alive until scan finalization.
#[derive(Clone, Copy)]
pub(crate) enum OffloadOutput {
    EncodedLeaves,
    #[cfg(any(feature = "native", test))]
    Runs,
    /// Reproduce the original dual output for parity tests and measurements.
    #[cfg(test)]
    Both,
}

impl OffloadOutput {
    fn leaves(self) -> bool {
        match self {
            Self::EncodedLeaves => true,
            #[cfg(any(feature = "native", test))]
            Self::Runs => false,
            #[cfg(test)]
            Self::Both => true,
        }
    }

    fn runs(self) -> bool {
        match self {
            Self::EncodedLeaves => false,
            #[cfg(any(feature = "native", test))]
            Self::Runs => true,
            #[cfg(test)]
            Self::Both => true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Offload {
    pub sapling: PoolAcc<sapling::Node>,
    pub orchard: PoolAcc<MerkleHashOrchard>,
    pub ironwood: PoolAcc<MerkleHashOrchard>,
}

impl Offload {
    pub(crate) fn new(
        sapling_completed: u64,
        orchard_completed: u64,
        ironwood_completed: u64,
        output: OffloadOutput,
    ) -> Self {
        Self {
            sapling: PoolAcc::new(sapling_completed, output),
            orchard: PoolAcc::new(orchard_completed, output),
            ironwood: PoolAcc::new(ironwood_completed, output),
        }
    }

    pub(crate) fn flush(&mut self) {
        self.sapling.flush();
        self.orchard.flush();
        self.ironwood.flush();
    }

    /// Native sync inserts the birthday frontier itself and SQLite stores
    /// server roots as shard caps. Once its root is known, the first shard
    /// needs only marked or recent leaves fed after the frontier.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn drop_first_shards_with_roots(&mut self) {
        self.sapling.drop_first = true;
        self.orchard.drop_first = true;
        self.ironwood.drop_first = true;
    }

    /// Sync must retain the recent commitment boundaries needed for
    /// common-pool spending anchors even in otherwise unmarked interior shards.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn retain_checkpoints_from(&mut self, height: BlockHeight) {
        self.sapling.retain_checkpoints_from = Some(height);
        self.orchard.retain_checkpoints_from = Some(height);
        self.ironwood.retain_checkpoints_from = Some(height);
    }

    /// Persist note-bearing / birthday / tip shards that are still open.
    /// Unmarked interior shards stay in memory until they complete (then drop).
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn flush_durable(&mut self) {
        self.sapling.flush_durable();
        self.orchard.flush_durable();
        self.ironwood.flush_durable();
    }

    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn has_pending(&self) -> bool {
        self.sapling.has_pending() || self.orchard.has_pending() || self.ironwood.has_pending()
    }

    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    #[cfg(test)]
    pub(crate) fn dropped_shards(&self) -> u64 {
        self.sapling.dropped + self.orchard.dropped + self.ironwood.dropped
    }
}

#[derive(Clone)]
pub(crate) struct PoolAcc<H> {
    output: OffloadOutput,
    /// Shards in `roots_from..completed` have server subtree roots in the
    /// wallet store. Only those shards may be dropped or demoted as historic.
    roots_from: u64,
    completed: u64,
    first_shard: Option<u64>,
    /// First fed position; the frontier covers the first shard before it.
    first_start: u64,
    drop_first: bool,
    cur: Option<u64>,
    start: u64,
    buf: Vec<(H, Retention<BlockHeight>, u32)>,
    marked: bool,
    kept: Vec<KeptLeaf>,
    kept_runs: Vec<KeptRun<H>>,
    dropped: u64,
    retain_checkpoints_from: Option<BlockHeight>,
    // Web snapshots store a leaf's original height for rewind. Track later
    // empty-height use separately instead of rewriting its retention id.
    scanned_boundary_height: Option<BlockHeight>,
}

impl<H: NoteLeaf> PoolAcc<H> {
    fn new(completed: u64, output: OffloadOutput) -> Self {
        Self {
            output,
            roots_from: 0,
            completed,
            first_shard: None,
            first_start: 0,
            drop_first: false,
            cur: None,
            start: 0,
            buf: Vec::new(),
            marked: false,
            kept: Vec::new(),
            kept_runs: Vec::new(),
            dropped: 0,
            retain_checkpoints_from: None,
            scanned_boundary_height: None,
        }
    }

    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn has_pending(&self) -> bool {
        self.cur.is_some() || !self.kept_runs.is_empty()
    }

    fn has_root(&self, shard: u64) -> bool {
        (self.roots_from..self.completed).contains(&shard)
    }

    /// Root-covered, so its unmarked leaves need not be kept once complete.
    fn interior(&self, shard: u64) -> bool {
        let first = self.first_shard.unwrap_or(shard);
        self.has_root(shard) && (shard != first || self.drop_first)
    }

    /// Shards `start..end` treated as having durable subtree roots.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn root_range(&self) -> (u64, u64) {
        (self.roots_from, self.completed)
    }

    /// Roots for shards `start..end` are durable in the wallet store. Extend an
    /// overlapping or adjacent range; otherwise replace it. Dropping the old
    /// range only makes shards ineligible to drop, so either choice is safe.
    pub(crate) fn roots_available(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        if start <= self.completed && end >= self.roots_from {
            self.roots_from = self.roots_from.min(start);
            self.completed = self.completed.max(end);
        } else {
            self.roots_from = start;
            self.completed = end;
        }
    }

    pub(crate) fn retain_scanned_boundary(&mut self, height: BlockHeight) {
        if !self.buf.is_empty() {
            self.scanned_boundary_height = Some(height);
        }
    }

    /// Preserve a compact-derived boundary before a later append can compact
    /// this buffered prefix. Native sync also checkpoints note-free heights.
    #[cfg(feature = "native")]
    pub(crate) fn retain_checkpoint(&mut self, position: u64, height: BlockHeight) -> bool {
        if let Some(offset) = position.checked_sub(self.start) {
            if let Some((_, retention, _)) = self.buf.get_mut(offset as usize) {
                *retention = checkpoint_retention(retention, height);
                return true;
            }
        }
        for run in &mut self.kept_runs {
            if let Some(offset) = position.checked_sub(run.start) {
                if let Some((_, retention)) = run.leaves.get_mut(offset as usize) {
                    *retention = checkpoint_retention(retention, height);
                    run.historic = false;
                    return true;
                }
            }
        }
        false
    }

    pub(crate) fn feed(
        &mut self,
        pos: u64,
        commitments: &[(H, Retention<BlockHeight>)],
        height: u32,
    ) {
        for (i, (hash, ret)) in commitments.iter().enumerate() {
            let p = pos + i as u64;
            let shard = p >> SHARD_HEIGHT;
            if self.first_shard.is_none() {
                self.first_shard = Some(shard);
                self.first_start = p;
            }
            if self.cur.is_some_and(|c| c != shard) {
                self.finalize_cur();
            }
            if self.cur.is_none() {
                self.cur = Some(shard);
                self.start = p;
                self.marked = false;
            }
            self.marked |= is_marked(ret);
            self.buf.push((hash.clone(), ret.clone(), height));
        }
    }

    pub(crate) fn flush(&mut self) {
        self.finalize_cur();
    }

    /// Insert the current open shard if a crash would lose a wallet note or a
    /// birthday/tip shard. Interior unmarked shards must stay buffered so a later
    /// full unmarked shard can still be dropped.
    pub(crate) fn flush_durable(&mut self) {
        let Some(shard) = self.cur else {
            return;
        };
        if self.marked || !self.interior(shard) {
            self.finalize_cur();
        }
    }

    pub(crate) fn drain_kept(&mut self) -> Vec<KeptLeaf> {
        std::mem::take(&mut self.kept)
    }

    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn drain_runs(&mut self) -> Vec<KeptRun<H>> {
        std::mem::take(&mut self.kept_runs)
    }

    pub(crate) fn buffer_kept(&self) -> Vec<KeptLeaf> {
        self.buf
            .iter()
            .enumerate()
            .map(|(i, (h, r, height))| kept_from(self.start + i as u64, h, r, *height))
            .collect()
    }

    fn finalize_cur(&mut self) {
        let Some(shard) = self.cur.take() else {
            return;
        };
        let n = self.buf.len() as u64;
        // The buffer must hold every leaf this scan fed to the shard: from
        // its start, or from the first fed position in the first shard.
        let fed_from = if Some(shard) == self.first_shard {
            self.first_start.max(shard << SHARD_HEIGHT)
        } else {
            shard << SHARD_HEIGHT
        };
        let full = self.start == fed_from && self.start + n == (shard + 1) << SHARD_HEIGHT;
        let interior = self.interior(shard);
        let scanned_boundary_height = self.scanned_boundary_height.take();
        let keeps_recent_boundary = self.retain_checkpoints_from.is_some_and(|floor| {
            scanned_boundary_height.is_some_and(|height| height >= floor)
                || self.buf.iter().any(|(_, retention, _)| {
                    matches!(retention, Retention::Checkpoint { id, .. } if *id >= floor)
                })
        });
        let drop = interior && !self.marked && full && !keeps_recent_boundary;
        if drop {
            tracing::debug!("selective-scan drop unmarked completed shard");
            self.dropped = self.dropped.saturating_add(1);
            self.buf.clear();
            self.marked = false;
            return;
        }
        let historic = interior && !keeps_recent_boundary;
        if self.output.leaves() {
            self.kept.extend(
                self.buf
                    .iter()
                    .enumerate()
                    .map(|(i, (h, r, height))| kept_from(self.start + i as u64, h, r, *height)),
            );
        }
        if self.output.runs() {
            self.kept_runs.push(KeptRun {
                start: self.start,
                historic,
                leaves: self.buf.drain(..).map(|(h, r, _)| (h, r)).collect(),
            });
        } else {
            self.buf.clear();
        }
        self.marked = false;
    }
}

pub(crate) fn is_marked<C>(retention: &Retention<C>) -> bool {
    matches!(retention, Retention::Marked)
        || matches!(
            retention,
            Retention::Checkpoint {
                marking: Marking::Marked,
                ..
            }
        )
}

pub(crate) fn checkpoint_retention(
    retention: &Retention<BlockHeight>,
    height: BlockHeight,
) -> Retention<BlockHeight> {
    let marking = match retention {
        Retention::Checkpoint { marking, .. } => *marking,
        Retention::Marked => Marking::Marked,
        Retention::Reference => Marking::Reference,
        Retention::Ephemeral => Marking::None,
    };
    Retention::Checkpoint {
        id: height,
        marking,
    }
}

fn kept_from<H: NoteLeaf>(
    position: u64,
    hash: &H,
    ret: &Retention<BlockHeight>,
    height: u32,
) -> KeptLeaf {
    let (kind, height) = match ret {
        Retention::Ephemeral | Retention::Reference => (0, height),
        Retention::Marked => (1, height),
        Retention::Checkpoint {
            id,
            marking: Marking::None,
        } => (2, u32::from(*id)),
        Retention::Checkpoint { id, marking: _ } => (3, u32::from(*id)),
    };
    KeptLeaf {
        position,
        hash: hash.encode(),
        kind,
        height,
    }
}

fn demote_historic(ret: &Retention<BlockHeight>) -> Retention<BlockHeight> {
    if is_marked(ret) {
        Retention::Marked
    } else {
        Retention::Ephemeral
    }
}

fn demote_tip(ret: &Retention<BlockHeight>, keep_from: BlockHeight) -> Retention<BlockHeight> {
    match ret {
        Retention::Checkpoint { id, .. } if *id >= keep_from => ret.clone(),
        _ if is_marked(ret) => Retention::Marked,
        _ => Retention::Ephemeral,
    }
}

/// Chunk so `from_iter` stays aligned to 1024-leaf subtrees (unaligned is ~60× slower).
pub(crate) fn aligned_chunks(range_start: u64, len: usize) -> Vec<(u64, usize, usize)> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < len {
        let pos = range_start + idx as u64;
        let to_boundary = (CHUNK_SIZE - (pos % CHUNK_SIZE)) as usize;
        let take = to_boundary.min(len - idx);
        out.push((pos, idx, take));
        idx += take;
    }
    out
}

/// Consecutive aligned chunks grouped so one `insert_leaf_run` call has
/// enough chunks to keep every Rayon worker busy between progress ticks.
pub(crate) fn aligned_batches(range_start: u64, len: usize) -> Vec<(u64, usize, usize)> {
    let per_batch = rayon::current_num_threads().max(1) * 4;
    aligned_chunks(range_start, len)
        .chunks(per_batch)
        .map(|group| {
            let (pos, offset, _) = group[0];
            (pos, offset, group.iter().map(|c| c.2).sum())
        })
        .collect()
}

/// Hash each aligned chunk into its own subtree on the Rayon pool, then insert
/// them in order. Building a subtree touches no shared state; the insert is the
/// only step that needs the tree. The WASM multicore build spreads Sinsemilla
/// over every worker; the single-threaded build runs the same code inline.
pub(crate) fn insert_leaf_run<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<S, DEPTH, SHARD>,
    start: u64,
    leaves: &[(H, Retention<BlockHeight>)],
    historic: bool,
    keep_from: BlockHeight,
) -> Result<()>
where
    H: Hashable + Clone + PartialEq + Send + Sync,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    S::Error: core::fmt::Debug,
{
    if leaves.is_empty() {
        return Ok(());
    }
    let built: Vec<_> = aligned_chunks(start, leaves.len())
        .into_par_iter()
        .map(|(pos, offset, len)| {
            let chunk = &leaves[offset..offset + len];
            let values = chunk.iter().map(|(h, r)| {
                let ret = if historic {
                    demote_historic(r)
                } else {
                    demote_tip(r, keep_from)
                };
                (h.clone(), ret)
            });
            LocatedPrunableTree::from_iter(
                Position::from(pos)..Position::from(pos + len as u64),
                Level::from(SHARD),
                values,
            )
            .map(|res| (pos, res.subtree, res.checkpoints))
        })
        .collect();
    for (pos, subtree, checkpoints) in built.into_iter().flatten() {
        let checkpoints = if historic {
            BTreeMap::new()
        } else {
            checkpoints
        };
        drop_stale_checkpoints(tree, &checkpoints);
        tree.insert_tree(subtree, checkpoints)
            .map_err(|e| {
                EngineError::Message(format!(
                    "selective-scan insert_tree: {e} (run start {pos}). Wipe scan & resync (or rewind to last good mark)."
                ))
            })?;
    }
    Ok(())
}

/// Drop checkpoints we are about to rewrite at a later position so `insert_tree`
/// does not toast `Conflict at checkpoint id`.
pub(crate) fn drop_stale_checkpoints<H, S, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<S, DEPTH, SHARD>,
    checkpoints: &BTreeMap<BlockHeight, Position>,
) where
    H: Hashable + Clone + PartialEq,
    S: ShardStore<H = H, CheckpointId = BlockHeight>,
    S::Error: core::fmt::Debug,
{
    for (id, new_pos) in checkpoints {
        let Ok(Some(existing)) = tree.store().get_checkpoint(id) else {
            continue;
        };
        let old = existing.position().map(u64::from).unwrap_or(0);
        if old < u64::from(*new_pos) {
            let _ = tree.store_mut().remove_checkpoint(id);
        }
    }
}

pub(crate) fn keep_from_height(scanned: u32) -> BlockHeight {
    BlockHeight::from_u32(scanned.saturating_sub(TIP_CHECKPOINT_BLOCKS))
}

pub(crate) fn is_sparse_leaves(hashed_leaves: u64, base: u64, next: u64) -> bool {
    next.saturating_sub(base) != hashed_leaves
}

pub(crate) fn shard_of(pos: u64) -> u64 {
    pos >> SHARD_HEIGHT
}

/// True when `run` covers a complete 2^16 shard from its index base.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) fn is_full_shard_run<H>(run: &KeptRun<H>) -> bool {
    run.start == shard_of(run.start) << SHARD_HEIGHT && run.leaves.len() as u64 == SHARD_SIZE
}

/// Historic = completed shard that is not the birthday/resume shard.
pub(crate) fn is_historic_shard(shard: u64, first_shard: u64, completed: u64) -> bool {
    shard < completed && shard != first_shard
}

const _: () = assert!(SAPLING_SHARD_HEIGHT == ORCHARD_SHARD_HEIGHT);

#[cfg(test)]
#[path = "offload_output_tests.rs"]
mod output_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use incrementalmerkletree::Hashable;
    use shardtree::store::memory::MemoryShardStore;
    use std::time::Instant;

    fn eph(n: usize) -> Vec<(sapling::Node, Retention<BlockHeight>)> {
        vec![(sapling::Node::empty_leaf(), Retention::Ephemeral); n]
    }

    #[test]
    fn drops_unmarked_interior_full_shard() {
        let mut acc = PoolAcc::<sapling::Node>::new(2, OffloadOutput::Both);
        acc.feed(0, &eph(SHARD_SIZE as usize), 1);
        acc.feed(SHARD_SIZE, &eph(SHARD_SIZE as usize), 2);
        acc.flush();
        let kept = acc.drain_kept();
        assert_eq!(kept.len() as u64, SHARD_SIZE, "birthday shard kept");
        assert!(kept.iter().all(|l| l.position < SHARD_SIZE));
        assert_eq!(acc.dropped, 1);
        let runs = acc.drain_runs();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].historic);
        assert_eq!(runs[0].start, 0);
    }

    #[test]
    fn keeps_marked_interior_shard() {
        let mut acc = PoolAcc::<sapling::Node>::new(2, OffloadOutput::Both);
        acc.feed(0, &eph(SHARD_SIZE as usize), 1);
        let mut shard1 = eph(SHARD_SIZE as usize);
        shard1[3] = (sapling::Node::empty_leaf(), Retention::Marked);
        acc.feed(SHARD_SIZE, &shard1, 2);
        acc.flush();
        assert_eq!(acc.drain_kept().len() as u64, SHARD_SIZE * 2);
        assert_eq!(acc.dropped, 0);
        let runs = acc.drain_runs();
        assert_eq!(runs.len(), 2);
        assert!(runs[1].historic);
        assert!(runs[1].leaves.iter().any(|(_, r)| is_marked(r)));
    }

    #[test]
    fn root_ranges_extend_when_touching_and_replace_when_disjoint() {
        let mut acc = PoolAcc::<sapling::Node>::new(4, OffloadOutput::Runs);
        acc.roots_available(4, 6);
        assert_eq!((acc.roots_from, acc.completed), (0, 6));
        acc.roots_available(9, 12);
        assert_eq!((acc.roots_from, acc.completed), (9, 12));
        acc.roots_available(7, 9);
        assert_eq!((acc.roots_from, acc.completed), (7, 12));
        acc.roots_available(8, 8);
        assert_eq!((acc.roots_from, acc.completed), (7, 12));
    }

    #[test]
    fn roots_learned_mid_scan_drop_only_the_full_unmarked_shards_they_cover() {
        let mut acc = PoolAcc::<sapling::Node>::new(0, OffloadOutput::Runs);
        let birthday = 2 * SHARD_SIZE + 10;
        acc.feed(birthday, &eph((SHARD_SIZE - 10) as usize), 1);
        acc.roots_available(2, 5);
        acc.feed(3 * SHARD_SIZE, &eph(SHARD_SIZE as usize), 2);
        let mut noted = eph(SHARD_SIZE as usize);
        noted[7].1 = Retention::Marked;
        acc.feed(4 * SHARD_SIZE, &noted, 3);
        acc.feed(5 * SHARD_SIZE, &eph(SHARD_SIZE as usize), 4);
        acc.feed(6 * SHARD_SIZE, &eph(3), 5);
        acc.flush();
        assert_eq!(
            acc.dropped, 1,
            "only shard 3 is full, unmarked and root-covered"
        );
        let runs: Vec<_> = acc
            .drain_runs()
            .iter()
            .map(|r| (r.start, r.historic))
            .collect();
        assert_eq!(
            runs,
            [
                (birthday, false),
                (4 * SHARD_SIZE, true),
                (5 * SHARD_SIZE, false),
                (6 * SHARD_SIZE, false),
            ],
            "the first shard, a marked shard, a shard without a root and the tip stay"
        );
    }

    #[test]
    fn a_root_covered_first_shard_drops_when_every_fed_leaf_is_buffered() {
        // (native opt-in, first fed position, roots known before first persist, dropped)
        for (native, start, early_roots, dropped) in [
            (true, 0, true, 1),
            (true, 10, true, 1),
            (false, 10, true, 0),
            (true, 10, false, 0),
        ] {
            let mut acc = PoolAcc::<sapling::Node>::new(0, OffloadOutput::Runs);
            acc.drop_first = native;
            if early_roots {
                acc.roots_available(0, 2);
            }
            acc.feed(start, &eph(100), 1);
            acc.flush_durable();
            assert_eq!(
                acc.drain_runs().is_empty(),
                native && early_roots,
                "a droppable first shard stays buffered until it completes"
            );
            acc.roots_available(0, 2);
            acc.feed(start + 100, &eph((SHARD_SIZE - start - 100) as usize), 2);
            acc.feed(SHARD_SIZE, &eph(3), 3);
            acc.flush();
            assert_eq!(acc.dropped, dropped, "native={native} start={start}");
            let starts: Vec<_> = acc.drain_runs().iter().map(|r| r.start).collect();
            let tip = [SHARD_SIZE];
            if dropped == 1 {
                assert_eq!(starts, tip, "the frontier covers the rest of shard 0");
            } else {
                assert_eq!(starts.last(), tip.last());
            }
        }
    }

    #[test]
    fn roots_arriving_mid_shard_keep_the_rest_of_an_already_persisted_shard() {
        let mut acc = PoolAcc::<sapling::Node>::new(0, OffloadOutput::Runs);
        acc.feed(0, &eph(5), 1);
        acc.feed(SHARD_SIZE, &eph(100), 2);
        acc.flush_durable();
        assert_eq!(
            acc.drain_runs().len(),
            2,
            "no roots yet: open shard persists"
        );
        acc.roots_available(0, 3);
        acc.feed(SHARD_SIZE + 100, &eph((SHARD_SIZE - 100) as usize), 3);
        acc.feed(2 * SHARD_SIZE, &eph(SHARD_SIZE as usize), 4);
        acc.feed(3 * SHARD_SIZE, &eph(1), 5);
        acc.flush();
        assert_eq!(acc.dropped, 1);
        let starts: Vec<_> = acc.drain_runs().iter().map(|r| r.start).collect();
        assert_eq!(
            starts,
            [SHARD_SIZE + 100, 3 * SHARD_SIZE],
            "shard 1 already has stored leaves, so its remainder cannot be dropped"
        );
    }

    #[test]
    fn flush_durable_keeps_unmarked_interior_in_buffer() {
        let mut acc = PoolAcc::<sapling::Node>::new(2, OffloadOutput::Both);
        acc.feed(0, &eph(SHARD_SIZE as usize), 1);
        acc.feed(SHARD_SIZE, &eph(100), 2);
        acc.flush_durable();
        let runs = acc.drain_runs();
        assert_eq!(
            runs.len(),
            1,
            "only the completed birthday shard is durable"
        );
        assert_eq!(runs[0].start, 0);
        assert!(
            acc.has_pending(),
            "unmarked interior partial stays in memory"
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn recent_empty_height_retains_buffered_interior_boundary_and_marking() {
        for marking in [Marking::None, Marking::Marked, Marking::Reference] {
            let mut acc = PoolAcc::<sapling::Node>::new(3, OffloadOutput::Both);
            acc.feed(0, &eph(1), 1);
            acc.flush_durable();
            acc.drain_runs();
            let mut leaves = eph(10);
            leaves[9].1 = Retention::Checkpoint {
                id: 5.into(),
                marking,
            };
            acc.feed(SHARD_SIZE, &leaves, 5);
            // Leave this interior run buffered, as native Offload does before
            // a full shard's drop/keep decision is known.
            assert!(acc.retain_checkpoint(SHARD_SIZE + 9, 189.into()));
            acc.retain_checkpoints_from = Some(100.into());
            acc.feed(SHARD_SIZE + 10, &eph(2), 190);
            acc.flush();
            let runs = acc.drain_runs();
            assert_eq!(runs.len(), 1);
            assert!(!runs[0].historic);
            assert_eq!(
                runs[0].leaves[9].1,
                Retention::Checkpoint {
                    id: 189.into(),
                    marking
                }
            );
        }
    }

    #[test]
    fn scanned_empty_boundary_preserves_original_height_without_leaking_to_next_shard() {
        for recent_height in [99u32, 100] {
            let mut acc = PoolAcc::<MerkleHashOrchard>::new(4, OffloadOutput::Both);
            acc.retain_checkpoints_from = Some(100.into());
            let leaf = MerkleHashOrchard::empty_leaf();
            acc.feed(0, &[(leaf, Retention::Ephemeral)], 1);
            acc.feed(SHARD_SIZE, &vec![(leaf, Retention::Ephemeral); 10], 5);
            acc.retain_scanned_boundary(recent_height.into());
            let mut candidate = acc.clone();
            candidate.feed(
                SHARD_SIZE + 10,
                &vec![(leaf, Retention::Ephemeral); SHARD_SIZE as usize * 2 - 10 + 1],
                200,
            );
            candidate.flush();
            let kept = candidate.drain_kept();
            let boundary = kept.iter().find(|l| l.position == SHARD_SIZE + 9);
            assert_eq!(boundary.is_some(), recent_height == 100);
            if let Some(boundary) = boundary {
                assert_eq!(boundary.height, 5);
                assert_eq!(boundary.kind, 0);
            }
            assert!(
                !kept
                    .iter()
                    .any(|l| (SHARD_SIZE * 2..SHARD_SIZE * 3).contains(&l.position)),
                "a boundary in the preceding shard must not keep the next interior shard"
            );
            assert_eq!(
                acc.buffer_kept().len(),
                10,
                "discarded candidates do not consume original state"
            );
        }
    }

    #[test]
    fn recent_checkpoint_keeps_unmarked_interior_shard_and_its_retention() {
        for recent in [false, true] {
            let mut acc = PoolAcc::<MerkleHashOrchard>::new(3, OffloadOutput::Both);
            acc.retain_checkpoints_from = Some(100.into());
            let leaf = MerkleHashOrchard::empty_leaf();
            let mut values = vec![(leaf, Retention::Ephemeral); SHARD_SIZE as usize * 2];
            values.last_mut().unwrap().1 = Retention::Checkpoint {
                id: if recent { 100.into() } else { 99.into() },
                marking: Marking::None,
            };
            acc.feed(0, &values, 100);
            acc.flush();
            let interior = acc.drain_runs().into_iter().find(|r| r.start == SHARD_SIZE);
            assert_eq!(interior.is_some(), recent);
            if let Some(run) = interior {
                assert!(!run.historic, "recent boundaries must not be demoted");
            }
            assert_eq!(acc.dropped, u64::from(!recent));
        }
    }

    #[test]
    fn insert_tree_after_skipped_empty_heights_does_not_conflict() {
        let keep = BlockHeight::from_u32(3_340_276);
        let mut tree: ShardTree<
            MemoryShardStore<sapling::Node, BlockHeight>,
            { SAPLING_SHARD_HEIGHT * 2 },
            SAPLING_SHARD_HEIGHT,
        > = ShardTree::new(MemoryShardStore::empty(), 32);
        let frontier = 80u64;
        let note_height = BlockHeight::from_u32(3_340_376);
        tree.store_mut()
            .add_checkpoint(
                note_height,
                shardtree::store::Checkpoint::at_position(Position::from(frontier)),
            )
            .unwrap();
        // Skip empty heights: first real orchard leaves start at the birthday
        // frontier. The leftover checkpoint sat at that same position.
        let mut leaves = eph(40);
        leaves[39] = (
            sapling::Node::empty_leaf(),
            Retention::Checkpoint {
                id: note_height,
                marking: Marking::None,
            },
        );
        insert_leaf_run(&mut tree, frontier, &leaves, false, keep).unwrap();
        let got = tree
            .store()
            .get_checkpoint(&note_height)
            .unwrap()
            .expect("checkpoint rewritten at the real leaf");
        assert_eq!(got.position(), Some(Position::from(frontier + 39)));
    }

    #[test]
    fn flush_durable_persists_marked_partial_shard() {
        let mut acc = PoolAcc::<sapling::Node>::new(2, OffloadOutput::Both);
        acc.feed(0, &eph(SHARD_SIZE as usize), 1);
        let mut partial = eph(50);
        partial[0] = (sapling::Node::empty_leaf(), Retention::Marked);
        acc.feed(SHARD_SIZE, &partial, 2);
        acc.flush_durable();
        let runs = acc.drain_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].leaves.len(), 50);
        assert!(runs[1].leaves.iter().any(|(_, r)| is_marked(r)));
        assert!(!acc.has_pending());
        assert!(!is_full_shard_run(&runs[1]));
    }

    /// Hashing one full shard vs dropping an unmarked interior shard.
    /// Run: `cargo test -p z-engine --release --lib selective_scan_drop_vs_sinsemilla -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn selective_scan_drop_vs_sinsemilla_speed() {
        let n = SHARD_SIZE as usize;
        let leaves = eph(n);
        let mut tree: ShardTree<
            MemoryShardStore<sapling::Node, BlockHeight>,
            { SAPLING_SHARD_HEIGHT * 2 },
            SAPLING_SHARD_HEIGHT,
        > = ShardTree::new(MemoryShardStore::empty(), 8);
        let t0 = Instant::now();
        insert_leaf_run(&mut tree, 0, &leaves, true, BlockHeight::from_u32(1)).unwrap();
        let hashed = t0.elapsed();

        let mut acc = PoolAcc::<sapling::Node>::new(2, OffloadOutput::Both);
        let t1 = Instant::now();
        acc.feed(0, &eph(n), 1);
        acc.feed(SHARD_SIZE, &eph(n), 2);
        acc.flush();
        let dropped = t1.elapsed();
        assert_eq!(acc.drain_kept().len() as u64, SHARD_SIZE);
        eprintln!(
            "selective-scan speed: hash 1 sapling shard {hashed:?}; drop unmarked interior {dropped:?}"
        );
        if hashed.as_millis() >= 20 {
            assert!(
                dropped * 10 < hashed,
                "drop should be at least 10× faster than hashing ({dropped:?} vs {hashed:?})"
            );
        }
    }

    /// Isolate the ordered Orchard tree-hash portion of historic persistence.
    /// Compare debug and release builds with the same 4,096-leaf input; this
    /// excludes SQLite, RPC, trial decryption, and any wallet material.
    #[test]
    #[ignore = "manual debug-vs-release commitment insertion benchmark"]
    fn benchmark_orchard_commitment_insert() {
        use std::hint::black_box;
        let leaves = vec![(MerkleHashOrchard::empty_leaf(), Retention::Ephemeral); 4_096];
        let mut samples = Vec::new();
        for _ in 0..5 {
            let mut tree: ShardTree<
                MemoryShardStore<MerkleHashOrchard, BlockHeight>,
                { ORCHARD_SHARD_HEIGHT * 2 },
                ORCHARD_SHARD_HEIGHT,
            > = ShardTree::new(MemoryShardStore::empty(), 8);
            let started = Instant::now();
            insert_leaf_run(&mut tree, 0, &leaves, true, BlockHeight::from_u32(1)).unwrap();
            black_box(tree);
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        eprintln!(
            "Orchard 4096-leaf ordered tree insert: {:?} median ({} profile)",
            samples[2],
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
}
