//! Bounded offline regtest scanner for already authenticated shared publications.
use super::*;
use prost::Message;
use zcash_client_backend::{
    data_api::{
        chain::{error::Error as ChainError, scan_cached_blocks, BlockSource, ChainState},
        WalletCommitmentTrees,
    },
    proto::compact_formats::CompactBlock,
};
use zcash_primitives::block::BlockHash;

struct Blocks(Vec<CompactBlock>);
impl BlockSource for Blocks {
    type Error = Infallible;
    fn with_blocks<F, E>(
        &self,
        from: Option<BlockHeight>,
        limit: Option<usize>,
        mut visit: F,
    ) -> std::result::Result<(), ChainError<E, Self::Error>>
    where
        F: FnMut(CompactBlock) -> std::result::Result<(), ChainError<E, Self::Error>>,
    {
        let height = u64::from(u32::from(from.unwrap_or(0.into())));
        for block in self
            .0
            .iter()
            .filter(|b| b.height >= height)
            .take(limit.unwrap_or(usize::MAX))
        {
            visit(block.clone())?;
        }
        Ok(())
    }
}
fn invalid() -> EngineError {
    EngineError::Message("public_scan_invalid".into())
}

// Pinned shardtree can prune unmarked rightmost leaves while keeping persisted
// roots. Recover only commitment frontiers from the already authenticated prefix;
// the caller must compare every recovered size/root with native boundary state.
fn prefix_frontiers(blocks: &[CompactBlock], genesis: BlockHash) -> anyhow::Result<ChainState> {
    let empty = ChainState::empty(0.into(), genesis);
    let mut sapling = empty.final_sapling_tree().clone();
    let mut orchard = empty.final_orchard_tree().clone();
    let mut ironwood = empty.final_ironwood_tree().clone();
    for block in blocks {
        for tx in &block.vtx {
            for output in &tx.outputs {
                if output.cmu.len() != 32 {
                    anyhow::bail!("public_scan_invalid");
                }
                let cmu = output
                    .cmu()
                    .map_err(|_| anyhow::anyhow!("public_scan_invalid"))?;
                if !sapling.append(sapling::Node::from_cmu(&cmu)) {
                    anyhow::bail!("public_scan_invalid");
                }
            }
            for (frontier, actions) in [
                (&mut orchard, &tx.actions),
                (&mut ironwood, &tx.ironwood_actions),
            ] {
                for action in actions {
                    let cmx = action
                        .cmx()
                        .map_err(|_| anyhow::anyhow!("public_scan_invalid"))?;
                    if !frontier.append(orchard::tree::MerkleHashOrchard::from_cmx(&cmx)) {
                        anyhow::bail!("public_scan_invalid");
                    }
                }
            }
        }
    }
    let last = blocks
        .last()
        .ok_or_else(|| anyhow::anyhow!("public_scan_invalid"))?;
    Ok(ChainState::new(
        (last.height as u32).into(),
        BlockHash::from_slice(&last.hash),
        sapling,
        orchard,
        ironwood,
    ))
}

/// Activation heights from the caller's authenticated regtest chain profile.
#[derive(Clone, Copy, Debug)]
pub struct RegtestScanSchedule {
    pub nu6_3_height: u32,
    pub nu7_height: Option<u32>,
}

impl NativeWallet {
    /// Scan a complete, contiguous genesis-based regtest publication, using the
    /// standard native scanner and one wallet transaction. No network requests,
    /// memo enhancement, transparent-address lookup, rebroadcast or fallback.
    ///
    /// `bytes` uses protobuf varint-delimited CompactBlock messages. The caller
    /// verifies publication signatures, freshness, digests and anti-rollback first.
    /// This is shielded-wallet integration research, not production chain trust,
    /// transparent history, or a scalable incremental scanning API.
    pub fn scan_public_regtest(&self, bytes: &[u8], schedule: RegtestScanSchedule) -> Result<u32> {
        self.scan_public_regtest_inner(bytes, schedule, false)
    }

    /// Verify a complete authenticated publication against native overlap hashes,
    /// then scan only its suffix in one transaction using persisted native state.
    /// If native leaves were pruned, reconstruct prefix commitments and verify
    /// their sizes/roots against native state; prefix notes are not decrypted again.
    /// Equal prefixes are no-ops. Gaps, missing native roots, rollback and divergent
    /// history refuse advancement; explicitly rewind/reset before accepting a fork.
    /// The publication remains bounded to 320 blocks and 128 MiB. This reduces
    /// local rescanning, not publication download size or production chain-trust work.
    pub fn scan_public_regtest_incremental(
        &self,
        bytes: &[u8],
        schedule: RegtestScanSchedule,
    ) -> Result<u32> {
        self.scan_public_regtest_inner(bytes, schedule, true)
    }

    fn scan_public_regtest_inner(
        &self,
        mut bytes: &[u8],
        schedule: RegtestScanSchedule,
        incremental: bool,
    ) -> Result<u32> {
        if self.network != ZNetwork::Regtest
            || self.birthday_height() != 1
            || schedule.nu6_3_height < 2
            || schedule
                .nu7_height
                .is_some_and(|height| height <= schedule.nu6_3_height)
            || schedule.nu6_3_height != crate::regtest_nu6_3_height()
            || schedule.nu7_height != crate::regtest_nu7_height()
            || bytes.is_empty()
            || bytes.len() > 128 * 1024 * 1024
        {
            return Err(invalid());
        }
        let mut genesis_bytes = crate::web::from_hex(
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        )
        .map_err(|_| invalid())?;
        genesis_bytes.reverse();
        let genesis = BlockHash::from_slice(&genesis_bytes);
        let mut previous = genesis.0.to_vec();
        let mut blocks = Vec::new();
        while !bytes.is_empty() {
            if blocks.len() >= 320 {
                return Err(invalid());
            }
            let block = CompactBlock::decode_length_delimited(&mut bytes).map_err(|_| invalid())?;
            if block.height != (blocks.len() + 1) as u64
                || block.hash.len() != 32
                || block.prev_hash != previous
            {
                return Err(invalid());
            }
            previous = block.hash.clone();
            blocks.push(block);
        }
        let tip = blocks.len() as u32;
        let mut db = self.open_db()?;
        db.transactionally(|wdb| -> anyhow::Result<()> {
            scan_in_transaction(wdb, self.network, blocks, incremental, genesis)
        })
        .map_err(EngineError::from)?;
        Ok(tip)
    }
}

pub(super) type TransactionDb<'a, 'b, 'c, 'd> = WalletDb<
    zcash_client_sqlite::SqlTransaction<'a>,
    &'b ZNetwork,
    &'c SystemClock,
    &'d mut UnwrapErr<SysRng>,
>;

pub(super) fn scan_in_transaction(
    wdb: &mut TransactionDb<'_, '_, '_, '_>,
    network: ZNetwork,
    blocks: Vec<CompactBlock>,
    incremental: bool,
    genesis: BlockHash,
) -> anyhow::Result<()> {
    let tip = blocks.len() as u32;

    // Tip, overlap and commitment state must come from this same write transaction.
    if wdb
        .chain_height()?
        .is_some_and(|height| u32::from(height) > tip)
    {
        anyhow::bail!("public_scan_invalid");
    }
    for b in &blocks {
        if let Some(hash) = wdb.get_block_hash((b.height as u32).into())? {
            if hash.0.as_slice() != b.hash {
                anyhow::bail!("public_scan_invalid");
            }
        }
    }
    let max = wdb.block_max_scanned()?;
    if max
        .as_ref()
        .is_some_and(|b| u32::from(b.block_height()) > tip)
    {
        anyhow::bail!("public_scan_invalid");
    }
    let from_state = if incremental {
        let fully = wdb.block_fully_scanned()?;
        if fully.as_ref().map(|b| b.block_height()) != max.as_ref().map(|b| b.block_height()) {
            anyhow::bail!("public_scan_resume_required");
        }
        if let Some(boundary) = fully {
            let height = u32::from(boundary.block_height());
            // Every committed overlap hash is required, not just the boundary.
            for b in blocks.iter().take(height as usize) {
                if wdb
                    .get_block_hash((b.height as u32).into())?
                    .as_ref()
                    .map(|h| h.0.as_slice())
                    != Some(b.hash.as_slice())
                {
                    anyhow::bail!("public_scan_resume_required");
                }
            }
            if height == tip {
                return Ok(());
            }
            macro_rules! boundary_tree {
                ($tree:expr,$expected:expr) => {{
                    let size = $expected
                        .map(u64::from)
                        .ok_or_else(|| anyhow::anyhow!("public_scan_resume_required"))?;
                    let maximum = $tree.max_leaf_position(None)?;
                    let maximum_size = maximum.map(|p| u64::from(p) + 1).unwrap_or(0);
                    if maximum_size < size {
                        anyhow::bail!("public_scan_resume_required");
                    }
                    // Block metadata fixes the scanned position even when no checkpoint
                    // was retained for an empty block or prefetched roots extend past it.
                    let root = if size == 0 {
                        incrementalmerkletree::Hashable::empty_root(32.into())
                    } else {
                        $tree.root(
                            incrementalmerkletree::Address::from_parts(32.into(), 0),
                            size.into(),
                        )?
                    };
                    let frontier = if maximum_size != size {
                        None
                    } else {
                        match $tree.frontier() {
                            Ok(frontier) => Some(frontier),
                            Err(shardtree::error::ShardTreeError::Query(
                                shardtree::error::QueryError::TreeIncomplete(_),
                            )) => None,
                            Err(error) => return Err(anyhow::Error::from(error)),
                        }
                    };
                    Ok::<_, anyhow::Error>((root, frontier))
                }};
            }
            let (sapling_root, sapling) =
                wdb.with_sapling_tree_mut::<_, _, anyhow::Error>(|tree| {
                    boundary_tree!(tree, boundary.sapling_tree_size())
                })?;
            let (orchard_root, orchard) =
                wdb.with_orchard_tree_mut::<_, _, anyhow::Error>(|tree| {
                    boundary_tree!(tree, boundary.orchard_tree_size())
                })?;
            let (ironwood_root, ironwood) = wdb
                .with_ironwood_tree_mut::<_, _, anyhow::Error>(|tree| {
                    boundary_tree!(tree, boundary.ironwood_tree_size())
                })?
                .ok_or_else(|| anyhow::anyhow!("public_scan_resume_required"))?;
            let resumed = match (sapling, orchard, ironwood) {
                (Some(s), Some(o), Some(i))
                    if boundary.sapling_tree_size().map(u64::from) == Some(s.tree_size())
                        && boundary.orchard_tree_size().map(u64::from) == Some(o.tree_size())
                        && boundary.ironwood_tree_size().map(u64::from) == Some(i.tree_size()) =>
                {
                    ChainState::new(boundary.block_height(), boundary.block_hash(), s, o, i)
                }
                _ => prefix_frontiers(&blocks[..height as usize], genesis)?,
            };
            if boundary.sapling_tree_size().map(u64::from)
                != Some(resumed.final_sapling_tree().tree_size())
                || boundary.orchard_tree_size().map(u64::from)
                    != Some(resumed.final_orchard_tree().tree_size())
                || boundary.ironwood_tree_size().map(u64::from)
                    != Some(resumed.final_ironwood_tree().tree_size())
                || resumed.final_sapling_tree().root() != sapling_root
                || resumed.final_orchard_tree().root() != orchard_root
                || resumed.final_ironwood_tree().root() != ironwood_root
            {
                anyhow::bail!("public_scan_resume_required");
            }
            resumed
        } else {
            ChainState::empty(0.into(), genesis)
        }
    } else {
        ChainState::empty(0.into(), genesis)
    };
    let start = u32::from(from_state.block_height()) + 1;
    wdb.update_chain_tip(tip.into())?;
    scan_cached_blocks(
        &network,
        &Blocks(blocks),
        wdb,
        start.into(),
        &from_state,
        (tip - start + 1) as usize,
    )
    .map_err(|_| anyhow::anyhow!("public_scan_failed"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_account, fixture_wallet};
    use super::*;
    use shardtree::store::ShardStore;

    fn schedule() -> RegtestScanSchedule {
        RegtestScanSchedule {
            nu6_3_height: crate::regtest_nu6_3_height(),
            nu7_height: crate::regtest_nu7_height(),
        }
    }

    fn first_block() -> Vec<u8> {
        let mut genesis = crate::web::from_hex(
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        )
        .unwrap();
        genesis.reverse();
        CompactBlock {
            height: 1,
            hash: vec![1; 32],
            prev_hash: genesis,
            chain_metadata: Some(
                zcash_client_backend::proto::compact_formats::ChainMetadata {
                    sapling_commitment_tree_size: 0,
                    orchard_commitment_tree_size: 0,
                    ironwood_commitment_tree_size: 0,
                },
            ),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    fn encrypted_publication() -> Vec<CompactBlock> {
        let (key, _) = fixture_account();
        let mut previous = CompactBlock::decode_length_delimited(first_block().as_slice())
            .unwrap()
            .prev_hash;
        let mut size = 0;
        (1..=8)
            .map(|height| {
                let mut block = crate::native::selective_scan::tests::compact_at(height);
                block.prev_hash = previous.clone();
                if height == 3 || height == 6 {
                    crate::native::selective_scan::tests::pay_orchard(&mut block, &key);
                }
                size += block
                    .vtx
                    .iter()
                    .map(|tx| tx.actions.len() as u32)
                    .sum::<u32>();
                block.chain_metadata = Some(
                    zcash_client_backend::proto::compact_formats::ChainMetadata {
                        sapling_commitment_tree_size: 0,
                        orchard_commitment_tree_size: size,
                        ironwood_commitment_tree_size: 0,
                    },
                );
                previous = block.hash.clone();
                block
            })
            .collect()
    }
    fn encode(blocks: &[CompactBlock]) -> Vec<u8> {
        blocks
            .iter()
            .flat_map(Message::encode_length_delimited_to_vec)
            .collect()
    }
    fn initialized(dir: &std::path::Path) -> NativeWallet {
        let wallet = fixture_wallet(dir);
        let (key, _) = fixture_account();
        let genesis = CompactBlock::decode_length_delimited(first_block().as_slice())
            .unwrap()
            .prev_hash;
        wallet
            .replace_scan_db(
                &key,
                &AccountBirthday::from_parts(
                    ChainState::empty(0.into(), BlockHash::from_slice(&genesis)),
                    None,
                ),
            )
            .unwrap();
        wallet
    }
    fn snapshot(wallet: &NativeWallet) -> String {
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let blocks = conn
            .prepare("SELECT height,hash FROM blocks ORDER BY height")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let notes=conn.prepare("SELECT t.txid,n.value,n.commitment_tree_position,n.nf FROM orchard_received_notes n JOIN transactions t ON n.transaction_id=t.id_tx ORDER BY t.txid,n.action_index").unwrap().query_map([],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,u64>(1)?,r.get::<_,u64>(2)?,r.get::<_,Vec<u8>>(3)?))).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
        let spends=conn.prepare("SELECT t.txid,s.orchard_received_note_id FROM orchard_received_note_spends s JOIN transactions t ON s.transaction_id=t.id_tx ORDER BY t.txid,s.orchard_received_note_id").unwrap().query_map([],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,u64>(1)?))).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
        let mut db = wallet.open_db().unwrap();
        let frontier = db
            .with_orchard_tree_mut::<_, _, anyhow::Error>(|t| {
                Ok(t.root_at_checkpoint_id(&BlockHeight::from(wallet.scanned_height().unwrap()))?)
            })
            .map_err(|e| e.to_string());
        format!(
            "{:?}",
            (
                db.chain_height().unwrap(),
                blocks,
                notes,
                spends,
                frontier,
                wallet
                    .balance()
                    .map(|b| b.orchard_available)
                    .map_err(|e| e.to_string())
            )
        )
    }
    fn assert_witnesses(wallet: &NativeWallet) {
        use zcash_client_backend::data_api::{
            wallet::input_selection::{LockFilter, LockedInputPolicy},
            InputSource, TargetValue,
        };
        use zcash_protocol::{value::Zatoshis, ShieldedPool};
        let mut db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let (target, anchor) = db
            .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
            .unwrap()
            .unwrap();
        let notes = db
            .select_spendable_notes(
                account,
                TargetValue::AtLeast(Zatoshis::from_u64(100_000).unwrap()),
                &[ShieldedPool::Orchard],
                target,
                crate::confirmations_policy(ZNetwork::Regtest),
                &[],
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .unwrap();
        assert_eq!(notes.orchard().len(), 2);
        db.with_orchard_tree_mut::<_, _, anyhow::Error>(|tree| {
            let root = tree.root_at_checkpoint_id(&anchor)?.unwrap();
            for note in notes.orchard() {
                let witness = tree
                    .witness_at_checkpoint_id_caching(
                        note.note_commitment_tree_position(),
                        &anchor,
                    )?
                    .unwrap();
                let leaf =
                    orchard::tree::MerkleHashOrchard::from_cmx(&note.note().commitment().into());
                assert_eq!(witness.root(leaf), root);
            }
            Ok(())
        })
        .unwrap();
    }
    #[test]
    fn incremental_reopen_matches_full_scan_and_equal_prefix_is_noop() {
        let full_dir = tempfile::tempdir().unwrap();
        let full = initialized(full_dir.path());
        let inc_dir = tempfile::tempdir().unwrap();
        let incremental = initialized(inc_dir.path());
        let blocks = encrypted_publication();
        let bytes = encode(&blocks);
        full.scan_public_regtest(&bytes, schedule()).unwrap();
        incremental
            .scan_public_regtest_incremental(&encode(&blocks[..4]), schedule())
            .unwrap();
        drop(incremental);
        let incremental = NativeWallet::open(inc_dir.path()).unwrap();
        incremental
            .scan_public_regtest_incremental(&bytes, schedule())
            .unwrap();
        assert_eq!(snapshot(&full), snapshot(&incremental));
        assert_witnesses(&full);
        assert_witnesses(&incremental);
        let before = snapshot(&incremental);
        let conn = rusqlite::Connection::open(&incremental.paths.data_db).unwrap();
        conn.execute_batch("CREATE TRIGGER ext_public_no_insert BEFORE INSERT ON blocks BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert_eq!(
            incremental
                .scan_public_regtest_incremental(&bytes, schedule())
                .unwrap(),
            8
        );
        assert_eq!(before, snapshot(&incremental));
    }
    #[test]
    fn incremental_resume_uses_boundary_position_despite_prefetched_subtree_roots() {
        use incrementalmerkletree::Hashable;
        use orchard::tree::MerkleHashOrchard;
        use zcash_client_backend::data_api::chain::CommitmentTreeRoot;

        for empty in [false, true] {
            let mut blocks = encrypted_publication();
            if empty {
                for block in &mut blocks {
                    block.vtx.clear();
                    block
                        .chain_metadata
                        .as_mut()
                        .unwrap()
                        .orchard_commitment_tree_size = 0;
                }
            }
            let full_dir = tempfile::tempdir().unwrap();
            let full = initialized(full_dir.path());
            full.scan_public_regtest(&encode(&blocks), schedule())
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let wallet = initialized(dir.path());
            wallet
                .scan_public_regtest_incremental(&encode(&blocks[..4]), schedule())
                .unwrap();
            let mut db = wallet.open_db().unwrap();
            db.put_orchard_subtree_roots(
                1,
                &[CommitmentTreeRoot::from_parts(
                    100.into(),
                    MerkleHashOrchard::empty_root(16.into()),
                )],
            )
            .unwrap();
            db.with_orchard_tree_mut::<_, _, anyhow::Error>(|tree| {
                let position = tree
                    .store()
                    .get_checkpoint(&4.into())?
                    .and_then(|c| c.position());
                assert_ne!(tree.max_leaf_position(None)?, position);
                Ok(())
            })
            .unwrap();
            drop(db);
            drop(wallet);
            let wallet = NativeWallet::open(dir.path()).unwrap();
            wallet
                .scan_public_regtest_incremental(&encode(&blocks), schedule())
                .unwrap();
            assert_eq!(snapshot(&full), snapshot(&wallet));
            if !empty {
                assert_witnesses(&wallet);
            }
        }
    }
    #[test]
    fn incremental_fork_rollback_and_failed_commit_preserve_state() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = initialized(dir.path());
        let blocks = encrypted_publication();
        wallet
            .scan_public_regtest_incremental(&encode(&blocks[..4]), schedule())
            .unwrap();
        let before = snapshot(&wallet);
        assert!(wallet
            .scan_public_regtest_incremental(&encode(&blocks[..3]), schedule())
            .is_err());
        let mut fork = blocks[..4].to_vec();
        fork[3].hash = vec![44; 32];
        assert!(wallet
            .scan_public_regtest_incremental(&encode(&fork), schedule())
            .is_err());
        assert_eq!(before, snapshot(&wallet));
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute_batch("CREATE TRIGGER ext_public_fail BEFORE INSERT ON blocks WHEN NEW.height=6 BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(wallet
            .scan_public_regtest_incremental(&encode(&blocks), schedule())
            .is_err());
        assert_eq!(before, snapshot(&wallet));
        drop(wallet);
        let wallet = NativeWallet::open(dir.path()).unwrap();
        assert_eq!(before, snapshot(&wallet));
        conn.execute_batch("DROP TRIGGER ext_public_fail").unwrap();
        wallet
            .scan_public_regtest_incremental(&encode(&blocks), schedule())
            .unwrap();
        assert_witnesses(&wallet);
        // A deliberate native rewind permits the replacement fork; publication alone cannot rewind.
        wallet
            .open_db()
            .unwrap()
            .truncate_to_height(4.into())
            .unwrap();
        let mut replacement = blocks.clone();
        for i in 4..replacement.len() {
            replacement[i].hash = vec![50 + i as u8; 32];
            replacement[i].prev_hash = replacement[i - 1].hash.clone();
        }
        wallet
            .scan_public_regtest_incremental(&encode(&replacement), schedule())
            .unwrap();
        let oracle_dir = tempfile::tempdir().unwrap();
        let oracle = initialized(oracle_dir.path());
        oracle
            .scan_public_regtest(&encode(&replacement), schedule())
            .unwrap();
        assert_eq!(snapshot(&oracle), snapshot(&wallet));
        assert_witnesses(&wallet);
    }
    #[test]
    fn incremental_suffix_detects_spend_of_prefix_note() {
        let inc_dir = tempfile::tempdir().unwrap();
        let wallet = initialized(inc_dir.path());
        let mut blocks = encrypted_publication();
        wallet
            .scan_public_regtest_incremental(&encode(&blocks[..4]), schedule())
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let nf: Vec<u8> = conn
            .query_row("SELECT nf FROM orchard_received_notes LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        crate::native::selective_scan::tests::pay_orchard(
            &mut blocks[6],
            &crate::native::selective_scan::tests::test_ufvk(8),
        );
        blocks[6].vtx[0].actions[0].nullifier = nf;
        let added = blocks[6].vtx[0].actions.len() as u32;
        for block in blocks.iter_mut().skip(6) {
            block
                .chain_metadata
                .as_mut()
                .unwrap()
                .orchard_commitment_tree_size += added;
        }
        drop(wallet);
        let wallet = NativeWallet::open(inc_dir.path()).unwrap();
        wallet
            .scan_public_regtest_incremental(&encode(&blocks), schedule())
            .unwrap();
        let mut next = crate::native::selective_scan::tests::compact_at(9);
        next.prev_hash = blocks.last().unwrap().hash.clone();
        crate::native::selective_scan::tests::pay_orchard(&mut next, &fixture_account().0);
        next.chain_metadata = blocks.last().unwrap().chain_metadata;
        next.chain_metadata
            .as_mut()
            .unwrap()
            .orchard_commitment_tree_size += next.vtx[0].actions.len() as u32;
        blocks.push(next);
        let before = snapshot(&wallet);
        let mut contradiction = blocks.clone();
        contradiction[2].vtx[0].actions[0].cmx = contradiction[6].vtx[0].actions[0].cmx.clone();
        assert!(wallet
            .scan_public_regtest_incremental(&encode(&contradiction), schedule())
            .is_err());
        assert_eq!(snapshot(&wallet), before);
        wallet
            .scan_public_regtest_incremental(&encode(&blocks), schedule())
            .unwrap();
        let full_dir = tempfile::tempdir().unwrap();
        let full = initialized(full_dir.path());
        full.scan_public_regtest(&encode(&blocks), schedule())
            .unwrap();
        assert_eq!(snapshot(&full), snapshot(&wallet));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM orchard_received_note_spends",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(wallet.balance().unwrap().orchard_available, 100_000);
        assert_witnesses(&wallet);
    }
    #[test]
    fn incremental_refuses_missing_overlap_or_native_frontier() {
        let blocks = encrypted_publication();
        for sql in [
            "DELETE FROM blocks WHERE height=2",
            "DELETE FROM orchard_tree_shards; DELETE FROM orchard_tree_cap",
            "UPDATE blocks SET orchard_commitment_tree_size=99 WHERE height=4",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let wallet = initialized(dir.path());
            wallet
                .scan_public_regtest_incremental(&encode(&blocks[..4]), schedule())
                .unwrap();
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            conn.execute_batch(sql).unwrap();
            let before = snapshot(&wallet);
            assert!(wallet
                .scan_public_regtest_incremental(&encode(&blocks), schedule())
                .is_err());
            assert_eq!(before, snapshot(&wallet));
            assert_eq!(
                wallet.open_db().unwrap().chain_height().unwrap(),
                Some(4.into())
            );
        }
    }
    #[test]
    fn activation_at_height_two_is_a_supported_offline_profile() {
        const CASE: &str = "Z_STACK_OFFLINE_SCAN_TEST";
        if std::env::var(CASE).as_deref() != Ok("height-two") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native::wallet::public_scan::tests::activation_at_height_two_is_a_supported_offline_profile"])
                .env(CASE, "height-two")
                .env("Z_STACK_REGTEST_NU6_3", "2")
                .env_remove("Z_STACK_REGTEST_NU7")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, _) = fixture_account();
        let publication = first_block();
        let block = CompactBlock::decode_length_delimited(publication.as_slice()).unwrap();
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(0.into(), BlockHash::from_slice(&block.prev_hash)),
            None,
        );
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        assert_eq!(crate::regtest_nu6_3_height(), 2);
        assert_eq!(
            wallet
                .scan_public_regtest(&publication, schedule())
                .unwrap(),
            1
        );
        assert_eq!(wallet.scanned_height().unwrap(), 1);
    }

    #[test]
    fn wrong_activation_schedule_leaves_wallet_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let scanned = wallet.scanned_height().unwrap();
        let tip = wallet.open_db().unwrap().chain_height().unwrap();
        let current = schedule();
        for mismatch in [
            RegtestScanSchedule {
                nu6_3_height: current.nu6_3_height + 1,
                ..current
            },
            RegtestScanSchedule {
                nu7_height: Some(current.nu6_3_height),
                ..current
            },
            RegtestScanSchedule {
                nu7_height: Some(current.nu6_3_height + 1),
                ..current
            },
        ] {
            assert_eq!(
                wallet
                    .scan_public_regtest(&first_block(), mismatch)
                    .unwrap_err()
                    .to_string(),
                "public_scan_invalid"
            );
            assert_eq!(wallet.scanned_height().unwrap(), scanned);
            assert_eq!(wallet.open_db().unwrap().chain_height().unwrap(), tip);
        }
    }

    #[test]
    fn publication_cannot_roll_back_an_unscanned_chain_tip() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        wallet
            .open_db()
            .unwrap()
            .update_chain_tip(300.into())
            .unwrap();
        let scanned = wallet.scanned_height().unwrap();
        assert!(scanned < 1);
        let mut genesis = crate::web::from_hex(
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        )
        .unwrap();
        genesis.reverse();
        let publication = CompactBlock {
            height: 1,
            hash: vec![1; 32],
            prev_hash: genesis,
            ..Default::default()
        }
        .encode_length_delimited_to_vec();

        assert_eq!(
            wallet
                .scan_public_regtest(&publication, schedule())
                .unwrap_err()
                .to_string(),
            "public_scan_invalid"
        );
        assert_eq!(wallet.scanned_height().unwrap(), scanned);
        assert_eq!(
            wallet.open_db().unwrap().chain_height().unwrap(),
            Some(300.into())
        );
    }

    #[test]
    fn invalid_publication_framing_and_chain_leave_wallet_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let before = wallet.scanned_height().unwrap();
        for bytes in [
            vec![],
            vec![0xff],
            vec![0xff; 10],
            CompactBlock {
                height: 1,
                hash: vec![1; 32],
                prev_hash: vec![0; 32],
                ..Default::default()
            }
            .encode_length_delimited_to_vec(),
            CompactBlock {
                height: 2,
                hash: vec![1; 32],
                prev_hash: vec![0; 32],
                ..Default::default()
            }
            .encode_length_delimited_to_vec(),
        ] {
            assert_eq!(
                wallet
                    .scan_public_regtest(&bytes, schedule())
                    .unwrap_err()
                    .to_string(),
                "public_scan_invalid"
            );
            assert_eq!(wallet.scanned_height().unwrap(), before);
        }
        let mut wrong_profile = wallet;
        wrong_profile.meta.birthday_height = 2;
        assert!(wrong_profile.scan_public_regtest(&[0], schedule()).is_err());
        wrong_profile.meta.birthday_height = 1;
        wrong_profile.network = ZNetwork::Mainnet;
        assert!(wrong_profile.scan_public_regtest(&[0], schedule()).is_err());
    }
}
