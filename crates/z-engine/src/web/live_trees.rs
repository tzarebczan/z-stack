//! Hashed shard trees saved in the snapshot.
//!
//! `from_snapshot` used to start with no hashed trees, so the first proposal
//! on a loaded wallet re-hashed every retained leaf (8 s of single-threaded
//! Sinsemilla on a 24 MB mainnet wallet). The prove worker loads a snapshot
//! for every send, and each page reload paid it again. Saving the trees as
//! `zcash_client_sqlite` stores its shards (the same `write_shard` encoding)
//! lets a loaded wallet stay ready to spend.

use super::{from_hex, to_hex};
use crate::error::{EngineError, Result};
use incrementalmerkletree::{Address, Level, Position};
use serde::{Deserialize, Serialize};
use shardtree::store::memory::MemoryShardStore;
use shardtree::store::{Checkpoint, ShardStore, TreeState};
use shardtree::{LocatedTree, ShardTree};
use std::collections::BTreeSet;
use std::convert::Infallible;
use zcash_client_backend::serialization::shardtree::{read_shard, write_shard};
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::BlockHeight;

/// The three pools' trees and the scan position they belong to. A loaded
/// wallet uses them only if its cursors still match.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredLiveTrees {
    /// `live_pools` when saved.
    pub pools: u8,
    pub scanned_height: u32,
    pub sapling_next: u64,
    pub orchard_next: u64,
    pub ironwood_next: u64,
    pub sapling: StoredTree,
    pub orchard: StoredTree,
    pub ironwood: StoredTree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredTree {
    cap: String,
    shards: Vec<StoredShard>,
    checkpoints: Vec<StoredCheckpoint>,
    #[serde(default)]
    retained: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredShard {
    level: u8,
    index: u64,
    tree: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCheckpoint {
    height: u32,
    /// `None` for a checkpoint of the empty tree.
    #[serde(default)]
    position: Option<u64>,
    #[serde(default)]
    marks_removed: Vec<u64>,
}

fn infallible<T>(r: std::result::Result<T, Infallible>) -> T {
    match r {
        Ok(t) => t,
        Err(e) => match e {},
    }
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::Message(format!("saved tree: {e}"))
}

pub(crate) fn save_tree<H, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
) -> Result<StoredTree>
where
    H: HashSer + incrementalmerkletree::Hashable + Clone + PartialEq,
{
    let store = tree.store();
    let mut cap = Vec::new();
    write_shard(&mut cap, &infallible(store.get_cap())).map_err(io_err)?;
    let mut shards = Vec::new();
    for addr in infallible(store.get_shard_roots()) {
        let Some(shard) = infallible(store.get_shard(addr)) else {
            continue;
        };
        let mut bytes = Vec::new();
        write_shard(&mut bytes, shard.root()).map_err(io_err)?;
        shards.push(StoredShard {
            level: u8::from(addr.level()),
            index: addr.index(),
            tree: to_hex(&bytes),
        });
    }
    let mut checkpoints = Vec::new();
    infallible(store.for_each_checkpoint(usize::MAX, |id, c| {
        checkpoints.push(StoredCheckpoint {
            height: u32::from(*id),
            position: match c.tree_state() {
                TreeState::Empty => None,
                TreeState::AtPosition(p) => Some(u64::from(p)),
            },
            marks_removed: c.marks_removed().iter().map(|p| u64::from(*p)).collect(),
        });
        Ok(())
    }));
    let retained = infallible(store.retained_checkpoints())
        .into_iter()
        .map(u32::from)
        .collect();
    Ok(StoredTree {
        cap: to_hex(&cap),
        shards,
        checkpoints,
        retained,
    })
}

pub(crate) fn load_tree<H, const DEPTH: u8, const SHARD: u8>(
    stored: &StoredTree,
    max_checkpoints: usize,
) -> Result<ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>>
where
    H: HashSer + incrementalmerkletree::Hashable + Clone + PartialEq,
{
    let bytes =
        |hex: &str| from_hex(hex).map_err(|e| EngineError::Message(format!("saved tree: {e}")));
    let mut store = MemoryShardStore::empty();
    for s in &stored.shards {
        let addr = Address::from_parts(Level::from(s.level), s.index);
        let root = read_shard(&bytes(&s.tree)?[..]).map_err(io_err)?;
        let shard = LocatedTree::from_parts(addr, root).map_err(|at| {
            EngineError::Message(format!("saved tree: shard {addr:?} overflows at {at:?}"))
        })?;
        infallible(store.put_shard(shard));
    }
    infallible(store.put_cap(read_shard(&bytes(&stored.cap)?[..]).map_err(io_err)?));
    for c in &stored.checkpoints {
        let state = match c.position {
            None => TreeState::Empty,
            Some(p) => TreeState::AtPosition(Position::from(p)),
        };
        let removed: BTreeSet<Position> =
            c.marks_removed.iter().map(|p| Position::from(*p)).collect();
        infallible(store.add_checkpoint(
            BlockHeight::from_u32(c.height),
            Checkpoint::from_parts(state, removed),
        ));
    }
    for h in &stored.retained {
        infallible(store.add_retained_checkpoint(BlockHeight::from_u32(*h)));
    }
    Ok(ShardTree::new(store, max_checkpoints))
}
