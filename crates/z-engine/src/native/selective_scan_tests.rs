// Encrypted compact fixtures exercise the real Zakura scanner without a daemon.
include!("selective_scan_bench.rs");
include!("selective_scan_checkpoint_tests.rs");

#[test]
fn native_offload_constructor_and_reset_keep_only_typed_runs() {
    use incrementalmerkletree::Hashable;
    let mut off = NativeOffload::new(3, 4, 5);
    for reset in [false, true] {
        if reset {
            off.reset();
        }
        let retention = Retention::Checkpoint {
            id: 10.into(),
            marking: Marking::Marked,
        };
        off.acc
            .sapling
            .feed(0, &[(sapling::Node::empty_leaf(), retention)], 10);
        off.acc
            .orchard
            .feed(0, &[(MerkleHashOrchard::empty_leaf(), retention)], 10);
        off.acc
            .ironwood
            .feed(0, &[(MerkleHashOrchard::empty_leaf(), retention)], 10);
        off.acc.flush();
        assert!(off.acc.sapling.drain_kept().is_empty());
        assert!(off.acc.orchard.drain_kept().is_empty());
        assert!(off.acc.ironwood.drain_kept().is_empty());
        let sapling = off.acc.sapling.drain_runs();
        let orchard = off.acc.orchard.drain_runs();
        let ironwood = off.acc.ironwood.drain_runs();
        assert_eq!(sapling.len(), 1);
        assert_eq!(orchard.len(), 1);
        assert_eq!(ironwood.len(), 1);
        assert_eq!(sapling[0].leaves[0].1, retention);
        assert_eq!(orchard[0].leaves[0].1, retention);
        assert_eq!(ironwood[0].leaves[0].1, retention);
    }
}

pub(crate) fn empty_state(height: u32) -> ChainState {
    ChainState::new(
        height.into(),
        BlockHash([0; 32]),
        incrementalmerkletree::frontier::Frontier::empty(),
        incrementalmerkletree::frontier::Frontier::empty(),
        incrementalmerkletree::frontier::Frontier::empty(),
    )
}

pub(crate) fn compact_at(height: u32) -> CompactBlock {
    CompactBlock {
        height: u64::from(height),
        hash: vec![height as u8; 32],
        prev_hash: vec![height.wrapping_sub(1) as u8; 32],
        ..Default::default()
    }
}

pub(crate) fn test_ufvk(seed: u8) -> UnifiedFullViewingKey {
    zcash_keys::keys::UnifiedSpendingKey::from_seed(
        &ZNetwork::Regtest,
        &[seed; 32],
        zip32::AccountId::ZERO,
    )
    .unwrap()
    .to_unified_full_viewing_key()
}

pub(crate) fn pay_orchard(block: &mut CompactBlock, ufvk: &UnifiedFullViewingKey) {
    use orchard::{
        builder::{Builder, BundleType},
        bundle::BundleVersion,
    };
    let version = BundleVersion::orchard_v2();
    let mut builder = Builder::new(
        BundleType::DEFAULT,
        version,
        version.default_flags(),
        orchard::Anchor::empty_tree(),
    )
    .unwrap();
    builder
        .add_output(
            None,
            ufvk.orchard()
                .unwrap()
                .address_at(0u32, zip32::Scope::External),
            orchard::value::NoteValue::from_raw(50_000),
            [0; 512],
        )
        .unwrap();
    let (bundle, _) = builder
        .build::<i64>(rand::rand_core::UnwrapErr(rand::rngs::SysRng))
        .unwrap()
        .unwrap();
    block
        .vtx
        .push(zcash_client_backend::proto::compact_formats::CompactTx {
            txid: vec![block.height as u8; 32],
            actions: bundle.actions().iter().map(Into::into).collect(),
            ..Default::default()
        });
}

pub(crate) fn no_note_batch(start: u32, width: u32) -> DecryptedBatch {
    let blocks: Vec<_> = (start..start + width).map(compact_at).collect();
    let span = compact_span(&blocks).unwrap();
    finish_decrypted(
        empty_state(0),
        Vec::new(),
        span.clone(),
        span.last().unwrap().clone(),
        false,
        (0, 0, 0),
        None,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn persist_width_counts_only_new_span_after_stale_frontier() {
    let mut transactions = 0;
    let mut pending: Option<DecryptedBatch> = None;
    for start in (1..=24_000).step_by(1_000) {
        let batch = no_note_batch(start, 1_000);
        if let Some(pending) = pending.as_mut() {
            pending.absorb(batch);
        } else {
            pending = Some(batch);
        }
        if pending.as_ref().unwrap().persist_ready() {
            assert_eq!(pending.as_ref().unwrap().height_count(), 8_000);
            transactions += 1;
            pending = None;
        }
    }
    assert_eq!(transactions, 3, "later RPCs must not each trigger a write");
    assert!(pending.is_none());
}

#[test]
fn encrypted_note_tail_replaces_parallel_suffix_and_tracks_spend() {
    let ufvk = test_ufvk(7);
    let stranger = test_ufvk(8);
    let account = AccountUuid::default();
    let keys = ScanningKeys::from_account_ufvks([(account, ufvk.clone())]);
    let mut blocks: Vec<_> = (1..=8).map(compact_at).collect();
    pay_orchard(&mut blocks[1], &ufvk);
    pay_orchard(&mut blocks[3], &stranger);
    pay_orchard(&mut blocks[5], &ufvk);
    pay_orchard(&mut blocks[6], &stranger);
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    let receive = scan_block(
        &ZNetwork::Regtest,
        blocks[1].clone(),
        &keys,
        &Nullifiers::empty(),
        None,
    )
    .unwrap();
    let nf = receive.transactions()[0].orchard_outputs()[0].nf().unwrap();
    blocks[6].vtx[0].actions[0].nullifier = nf.to_bytes().to_vec();
    let (parallel, first) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &blocks,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(parallel.last().unwrap().transactions().len(), 0);
    let result = finish_note_tail(
        ZNetwork::Regtest,
        Nullifiers::empty(),
        &keys,
        1.into(),
        &blocks,
        parallel,
        first.unwrap(),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        result
            .iter()
            .map(|b| u32::from(b.height()))
            .collect::<Vec<_>>(),
        [2, 4, 6, 7]
    );
    assert_eq!(result[3].transactions()[0].orchard_spends().len(), 1);
    assert!(result[3].transactions()[0].orchard_outputs().is_empty());
    assert!(
        wallet_activity(&result[3]),
        "spend-only rows must be persisted"
    );
    let mut nfs = Nullifiers::empty();
    for block in &result {
        nfs.update_with(block);
    }
    assert_eq!(
        nfs.orchard().len(),
        1,
        "only the second receipt remains unspent"
    );
    let span = compact_span(&blocks).unwrap();
    let complete = finish_decrypted(
        empty_state(0),
        result,
        span.clone(),
        span.last().unwrap().clone(),
        true,
        (0, 0, 0),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(tree_sizes(&complete.next_state), (0, 8, 0));
}

#[test]
fn coalesced_note_batch_keeps_fresh_row_frontier() {
    let mut pending = no_note_batch(8_001, 1_000);
    let mut note = no_note_batch(9_001, 1_000);
    let fresh = empty_state(9_000);
    note.from_state = fresh.clone();
    note.row_runs.push(RowRun {
        first: 9_001.into(),
        last: 9_002.into(),
        prior: fresh.clone(),
    });
    note.next_state = empty_state(10_000);
    note.had_notes = true;
    pending.absorb(note);
    assert_eq!(
        pending.from_state.block_height(),
        0.into(),
        "graft frontier stays at its actual height"
    );
    assert_eq!(pending.row_runs[0].prior.block_height(), 9_000.into());
    assert_eq!(pending.next_state.block_height(), 10_000.into());
    assert_eq!(pending.height_count(), 2_000);
}

#[test]
fn context_reuses_keys_and_nullifiers_and_skips_empty_key_setup() {
    let db = benchmark_empty_db();
    let mut context = TrialContext::new(
        AccountUfvks::from([(AccountUuid::default(), test_ufvk(7))]),
        Nullifiers::empty(),
    );
    let empty: Vec<_> = (1..=8).map(compact_at).collect();
    let old = context.clone();
    trial_decrypt(
        ZNetwork::Regtest,
        context.snapshot(&db, &empty).unwrap(),
        0,
        0,
        0,
        1.into(),
        empty,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(
        context.keys.get().is_none(),
        "note-free work never derives viewing keys"
    );
    context.refresh_after_persist(&db, false).unwrap();
    assert!(Arc::ptr_eq(&old.nullifiers, &context.nullifiers));
    let mut blocks: Vec<_> = (9..=16).map(compact_at).collect();
    pay_orchard(&mut blocks[0], &test_ufvk(8));
    trial_decrypt(
        ZNetwork::Regtest,
        context.snapshot(&db, &blocks).unwrap(),
        0,
        0,
        0,
        9.into(),
        blocks.clone(),
        None,
    )
    .unwrap()
    .unwrap();
    let keys = context.keys.get().unwrap().clone();
    let tiny = context.snapshot(&db, &blocks[..4]).unwrap();
    assert!(
        tiny.sequential.is_some(),
        "tiny shielded work retains owned sequential nullifiers"
    );
    trial_decrypt(
        ZNetwork::Regtest,
        tiny,
        0,
        0,
        0,
        9.into(),
        blocks[..4].to_vec(),
        None,
    )
    .unwrap()
    .unwrap();
    assert!(Arc::ptr_eq(&keys, context.keys.get().unwrap()));
    assert!(Arc::ptr_eq(&old.nullifiers, &context.nullifiers));
}

#[test]
fn row_frontier_uses_compact_previous_hash_when_empty_frontier_height_is_stale() {
    let ufvk = test_ufvk(7);
    let keys = ScanningKeys::from_account_ufvks([(AccountUuid::default(), ufvk.clone())]);
    let mut block = compact_at(8001);
    pay_orchard(&mut block, &ufvk);
    ensure_compact_chain_metadata(std::slice::from_mut(&mut block), 0, 0, 0);
    let (scanned, _) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &[block.clone()],
        None,
    )
    .unwrap()
    .unwrap();
    let span = compact_span(&[block]).unwrap();
    let (_, runs) = walk_with_row_frontiers(&empty_state(0), &scanned, &span).unwrap();
    assert_eq!(runs[0].prior.block_height(), 8000.into());
    assert_eq!(runs[0].prior.block_hash(), span[0].prev_hash);
    assert_ne!(runs[0].prior.block_hash(), empty_state(0).block_hash());
}

#[test]
fn coalesced_adjacent_active_runs_persist_real_notes_and_witnesses() {
    use zcash_client_backend::data_api::{
        wallet::input_selection::{LockFilter, LockedInputPolicy},
        AccountBirthday, InputSource, TargetValue, WalletWrite,
    };
    use zcash_protocol::{value::Zatoshis, ShieldedPool};
    let mut db = benchmark_empty_db();
    let (account, _) = db
        .create_account(
            "coalesced rows",
            &secrecy::SecretVec::new(vec![7; 32]),
            &AccountBirthday::from_parts(empty_state(0), None),
            None,
        )
        .unwrap();
    db.update_chain_tip(12.into()).unwrap();
    let context = TrialContext::new(load_ufvks(&db).unwrap(), load_nullifiers(&db).unwrap());
    let mut blocks: Vec<_> = (1..=12).map(compact_at).collect();
    for height in [2, 6, 7, 11] {
        pay_orchard(&mut blocks[height - 1], &test_ufvk(7));
    }
    for height in [4, 5, 8, 10] {
        pay_orchard(&mut blocks[height - 1], &test_ufvk(8));
    }
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    let finish =
        |db: &mut super::super::wallet::SyncDb, from: &ChainState, blocks: &[CompactBlock]| {
            let d = decrypt_owned(
                ZNetwork::Regtest,
                context.snapshot(db, blocks).unwrap(),
                from.clone(),
                (blocks[0].height as u32).into(),
                blocks.to_vec(),
                None,
            )
            .unwrap()
            .unwrap();
            complete_decrypted(ZNetwork::Regtest, db, &context, d, None)
                .unwrap()
                .unwrap()
        };
    let mut first = finish(&mut db, &empty_state(0), &blocks[..6]);
    let second = finish(&mut db, &first.next_state, &blocks[6..]);
    let expected = second.next_state.final_orchard_tree().root();
    first.absorb(second);
    assert_eq!(
        first
            .row_runs
            .iter()
            .map(|r| (u32::from(r.first), u32::from(r.last)))
            .collect::<Vec<_>>(),
        [(2, 2), (4, 6), (7, 8), (10, 11)]
    );
    for run in &first.row_runs {
        assert_eq!(
            run.prior.block_hash(),
            compact_block_hash(&blocks[(u32::from(run.first) - 2) as usize]).unwrap()
        );
    }
    let mut offload = NativeOffload::new(0, 0, 0);
    persist_batch(&mut db, first, &mut offload).unwrap();
    flush_remaining(&mut db, &mut offload).unwrap();
    let (target, anchor) = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .unwrap()
        .unwrap();
    assert_eq!(u32::from(anchor), 12);
    let notes = db
        .select_spendable_notes(
            account,
            TargetValue::AtLeast(Zatoshis::from_u64(1_000_000).unwrap()),
            &[ShieldedPool::Orchard],
            target,
            crate::confirmations_policy(ZNetwork::Regtest),
            &[],
            LockFilter::Policy(&LockedInputPolicy::Exclude),
        )
        .unwrap();
    assert_eq!(notes.orchard().len(), 4);
    db.with_orchard_tree_mut(|tree| {
        assert_eq!(tree.root_at_checkpoint_id(&anchor)?.unwrap(), expected);
        for note in notes.orchard() {
            let witness = tree
                .witness_at_checkpoint_id(note.note_commitment_tree_position(), &anchor)?
                .unwrap();
            assert_eq!(
                witness.root(MerkleHashOrchard::from_cmx(
                    &note.note().commitment().into()
                )),
                expected
            );
        }
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
}

/// A native birthday shard whose root is known keeps only the grafted frontier
/// and the stored root once its fed leaves are dropped. The checkpoint root and
/// a later note's witness must match a fully hashed tree, whichever of the
/// frontier and the roots reaches SQLite first.
#[test]
#[ignore = "hashes a full Orchard shard; run with --release"]
fn dropped_birthday_shard_with_root_matches_the_full_tree() {
    use incrementalmerkletree::frontier::Frontier;
    use zcash_client_backend::data_api::chain::CommitmentTreeRoot;
    const SHARD: u64 = 1 << 16;
    let leaf = |i: u64| {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(i + 1).to_le_bytes());
        Option::<MerkleHashOrchard>::from(MerkleHashOrchard::from_bytes(&bytes)).unwrap()
    };
    let grafted = 100u64;
    let note = SHARD + 5;
    let total = SHARD + 70;
    let mut full = Frontier::<MerkleHashOrchard, 32>::empty();
    let mut first_shard = Frontier::<MerkleHashOrchard, 16>::empty();
    let mut graft = Frontier::empty();
    for i in 0..total {
        if i == grafted {
            graft = full.clone();
        }
        assert!(full.append(leaf(i)));
        if i < SHARD {
            assert!(first_shard.append(leaf(i)));
        }
    }
    let expected = full.root();
    let root = CommitmentTreeRoot::from_parts(1_500.into(), first_shard.root());
    let tip = BlockHeight::from_u32(2_100);

    for roots_first in [true, false] {
        let mut db = benchmark_empty_db();
        let state = ChainState::new(
            999.into(),
            BlockHash([9; 32]),
            Frontier::empty(),
            graft.clone(),
            Frontier::empty(),
        );
        let mut acc = Offload::new(0, 0, 0, OffloadOutput::Runs);
        acc.drop_first_shards_with_roots();
        acc.orchard.roots_available(0, 1);
        let mut leaves: Vec<_> = (grafted..total)
            .map(|i| (leaf(i), Retention::Ephemeral))
            .collect();
        leaves[(note - grafted) as usize].1 = Retention::Marked;
        leaves.last_mut().unwrap().1 = Retention::Checkpoint {
            id: tip,
            marking: Marking::None,
        };
        acc.orchard.feed(grafted, &leaves, 1_000);
        acc.orchard.flush();
        let runs = acc.orchard.drain_runs();
        assert_eq!(
            runs.iter().map(|r| r.start).collect::<Vec<_>>(),
            [SHARD],
            "the birthday shard's fed leaves are dropped"
        );

        if roots_first {
            db.put_orchard_subtree_roots(0, std::slice::from_ref(&root))
                .unwrap();
        }
        db.transactionally(|wdb| {
            insert_frontiers(wdb, &state)?;
            for run in &runs {
                insert_orchard_run(wdb, run, 2_000.into())?;
            }
            Ok::<_, SqliteClientError>(())
        })
        .unwrap();
        if !roots_first {
            // Without the stored root, the dropped leaves leave a hole.
            let missing = db.with_orchard_tree_mut(|tree| {
                Ok::<_, ShardTreeError<commitment_tree::Error>>(
                    tree.root_at_checkpoint_id(&tip).ok().flatten(),
                )
            });
            assert_ne!(missing.unwrap(), Some(expected));
            db.put_orchard_subtree_roots(0, std::slice::from_ref(&root))
                .unwrap();
        }
        db.with_orchard_tree_mut(|tree| {
            assert_eq!(tree.root_at_checkpoint_id(&tip)?, Some(expected));
            let witness = tree
                .witness_at_checkpoint_id(Position::from(note), &tip)?
                .expect("note witness");
            assert_eq!(witness.root(leaf(note)), expected);
            Ok::<_, ShardTreeError<commitment_tree::Error>>(())
        })
        .unwrap();
    }
}
