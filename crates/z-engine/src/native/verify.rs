//! Read-only check of the wallet's commitment trees against the light server.
//!
//! selective shard scanning builds sparse trees from a birthday frontier, kept shards and
//! server subtree roots. This compares each pool's root at recent checkpoints
//! with the chain's tree state, then checks every spendable note's witness at
//! the spend anchor. Reports carry heights and counts only: no positions,
//! keys or transaction IDs.

use super::pipeline::fetch_tree_state;
use super::wallet::NativeWallet;
use crate::error::{EngineError, Result};
use incrementalmerkletree::Position;
use orchard::tree::MerkleHashOrchard;
use serde::Serialize;
use shardtree::error::ShardTreeError;
use zcash_client_backend::data_api::{
    wallet::input_selection::{LockFilter, LockedInputPolicy},
    InputSource, MaxSpendMode, TargetValue, WalletCommitmentTrees, WalletRead,
};
use zcash_client_sqlite::wallet::commitment_tree;
use zcash_protocol::{consensus::BlockHeight, ShieldedPool};

/// One pool's root at one checkpoint height.
#[derive(Debug, Clone, Serialize)]
pub struct TreeRootCheck {
    pub pool: &'static str,
    pub height: u32,
    /// `None` when the wallet has no checkpoint at this height.
    pub matches: Option<bool>,
}

/// Witness results for one pool's spendable notes at the anchor.
#[derive(Debug, Clone, Serialize)]
pub struct WitnessCheck {
    pub pool: &'static str,
    pub checked: usize,
    pub ok: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeReport {
    pub anchor_height: Option<u32>,
    pub roots: Vec<TreeRootCheck>,
    pub witnesses: Vec<WitnessCheck>,
}

impl TreeReport {
    /// Every compared root and witness matched the chain.
    pub fn all_match(&self) -> bool {
        self.roots.iter().all(|r| r.matches != Some(false))
            && self.witnesses.iter().all(|w| w.checked == w.ok)
    }
}

type TreeResult<T> = std::result::Result<T, ShardTreeError<commitment_tree::Error>>;

fn wallet_db(e: impl std::fmt::Display) -> EngineError {
    EngineError::WalletDb(format!("verify trees: {e}"))
}

impl NativeWallet {
    /// Compare roots at the anchor and the two newest checkpoints, then every
    /// spendable note's witness at the anchor. The chain side comes from
    /// GetTreeState, so this needs no spend key and writes nothing.
    pub async fn verify_commitment_trees(&self) -> Result<TreeReport> {
        let mut db = self.open_db()?;
        let mut client = self.connect().await?;
        let policy = crate::confirmations_policy(self.network());
        let anchor = db
            .get_target_and_anchor_heights(policy.trusted())
            .map_err(wallet_db)?;
        let tip = db.chain_height().map_err(wallet_db)?;
        let mut heights: Vec<u32> = [anchor.map(|(_, a)| u32::from(a)), tip.map(u32::from)]
            .into_iter()
            .flatten()
            .collect();
        if let Some(tip) = tip {
            heights.push(u32::from(tip).saturating_sub(1));
        }
        heights.sort_unstable();
        heights.dedup();

        let mut roots = Vec::new();
        let mut chain_at_anchor = None;
        for height in heights {
            let chain = fetch_tree_state(&mut client, height).await?;
            let id = BlockHeight::from_u32(height);
            let sapling = db
                .with_sapling_tree_mut(|t| t.root_at_checkpoint_id(&id))
                .map_err(wallet_db)?;
            let orchard = db
                .with_orchard_tree_mut(|t| t.root_at_checkpoint_id(&id))
                .map_err(wallet_db)?;
            // `None` outer: this backend keeps no Ironwood tree.
            let ironwood = db
                .with_ironwood_tree_mut(|t| t.root_at_checkpoint_id(&id))
                .map_err(wallet_db)?
                .flatten();
            roots.push(TreeRootCheck {
                pool: "sapling",
                height,
                matches: sapling.map(|r| r == chain.final_sapling_tree().root()),
            });
            roots.push(TreeRootCheck {
                pool: "orchard",
                height,
                matches: orchard.map(|r| r == chain.final_orchard_tree().root()),
            });
            roots.push(TreeRootCheck {
                pool: "ironwood",
                height,
                matches: ironwood.map(|r| r == chain.final_ironwood_tree().root()),
            });
            if anchor.is_some_and(|(_, a)| u32::from(a) == height) {
                chain_at_anchor = Some(chain);
            }
        }

        let mut witnesses = Vec::new();
        if let (Some((target, anchor)), Some(chain)) = (anchor, chain_at_anchor) {
            for account in db.get_account_ids().map_err(wallet_db)? {
                let notes = db
                    .select_spendable_notes(
                        account,
                        TargetValue::AllFunds(MaxSpendMode::MaxSpendable),
                        &[
                            ShieldedPool::Sapling,
                            ShieldedPool::Orchard,
                            ShieldedPool::Ironwood,
                        ],
                        target,
                        policy,
                        &[],
                        LockFilter::Policy(&LockedInputPolicy::Exclude),
                    )
                    .map_err(wallet_db)?;
                let sapling_root = chain.final_sapling_tree().root();
                let sapling = db
                    .with_sapling_tree_mut(|t| {
                        count_witnesses(notes.sapling().iter().map(|n| {
                            let leaf = sapling::Node::from_cmu(&n.note().cmu());
                            let witness = t.witness_at_checkpoint_id(
                                n.note_commitment_tree_position(),
                                &anchor,
                            )?;
                            Ok(witness.is_some_and(|w| w.root(leaf) == sapling_root))
                        }))
                    })
                    .map_err(wallet_db)?;
                let orchard = db
                    .with_orchard_tree_mut(|t| {
                        orchard_witnesses(
                            t,
                            notes.orchard(),
                            &anchor,
                            chain.final_orchard_tree().root(),
                        )
                    })
                    .map_err(wallet_db)?;
                let ironwood = db
                    .with_ironwood_tree_mut(|t| {
                        orchard_witnesses(
                            t,
                            notes.ironwood(),
                            &anchor,
                            chain.final_ironwood_tree().root(),
                        )
                    })
                    .map_err(wallet_db)?
                    .unwrap_or((0, 0));
                for (pool, (checked, ok)) in [
                    ("sapling", sapling),
                    ("orchard", orchard),
                    ("ironwood", ironwood),
                ] {
                    witnesses.push(WitnessCheck { pool, checked, ok });
                }
            }
        }
        Ok(TreeReport {
            anchor_height: anchor.map(|(_, a)| u32::from(a)),
            roots,
            witnesses,
        })
    }
}

fn count_witnesses(results: impl Iterator<Item = TreeResult<bool>>) -> TreeResult<(usize, usize)> {
    let (mut checked, mut ok) = (0, 0);
    for result in results {
        checked += 1;
        ok += usize::from(result?);
    }
    Ok((checked, ok))
}

fn orchard_witnesses<S, NoteRef>(
    tree: &mut shardtree::ShardTree<S, 32, 16>,
    notes: &[zcash_client_backend::wallet::ReceivedNote<NoteRef, orchard::note::Note>],
    anchor: &BlockHeight,
    root: MerkleHashOrchard,
) -> TreeResult<(usize, usize)>
where
    S: shardtree::store::ShardStore<
        H = MerkleHashOrchard,
        CheckpointId = BlockHeight,
        Error = commitment_tree::Error,
    >,
{
    count_witnesses(notes.iter().map(|n| {
        let leaf = MerkleHashOrchard::from_cmx(&n.note().commitment().into());
        let position: Position = n.note_commitment_tree_position();
        let witness = tree.witness_at_checkpoint_id(position, anchor)?;
        Ok(witness.is_some_and(|w| w.root(leaf) == root))
    }))
}
