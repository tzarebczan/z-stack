#[test]
fn late_first_note_preserves_recent_empty_height_prefix() {
    use zcash_client_backend::data_api::{
        wallet::input_selection::{LockFilter, LockedInputPolicy},
        AccountBirthday, InputSource, TargetValue, WalletWrite,
    };
    use zcash_protocol::{value::Zatoshis, ShieldedPool};
    for split in [false, true] {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        rusqlite::vtab::array::load_module(&conn).unwrap();
        let mut db = zcash_client_sqlite::WalletDb::from_connection(
            conn,
            ZNetwork::Regtest,
            zcash_client_sqlite::util::SystemClock,
            rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        );
        zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
        let seed = secrecy::SecretVec::new(vec![7; 32]);
        let (account, _) = db
            .create_account(
                "late recipient",
                &seed,
                &AccountBirthday::from_parts(empty_state(0), None),
                None,
            )
            .unwrap();
        db.update_chain_tip(200.into()).unwrap();
        let ufvk = test_ufvk(7);
        let mut blocks: Vec<_> = (1..=220).map(compact_at).collect();
        // Prefix10 is needed by recent empty heights, despite its most recent
        // commitment being far older than the final100-height retention floor.
        for index in 0..5 {
            pay_orchard(&mut blocks[index], &test_ufvk(8));
        }
        pay_orchard(&mut blocks[189], &test_ufvk(8));
        pay_orchard(&mut blocks[199], &ufvk);
        ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
        let keys = ScanningKeys::from_account_ufvks([(account, ufvk)]);
        let (all, _) = scan_batch_parallel(
            ZNetwork::Regtest,
            &keys,
            &Nullifiers::empty(),
            &blocks,
            None,
        )
        .unwrap()
        .unwrap();
        let state_at = |height: u32| {
            chain_state_after(
                &empty_state(0),
                &all[..all.partition_point(|b| u32::from(b.height()) <= height)],
                height.into(),
                compact_block_hash(&blocks[height as usize - 1]).unwrap(),
            )
            .unwrap()
        };
        let mut offload = NativeOffload::new(0, 0, 0);
        let (start, state) = if split {
            scan_batch(
                ZNetwork::Regtest,
                &mut db,
                &empty_state(0),
                1.into(),
                &blocks[..100],
                &mut offload,
                None,
            )
            .unwrap()
            .unwrap();
            (100, state_at(100))
        } else {
            (0, empty_state(0))
        };
        scan_batch(
            ZNetwork::Regtest,
            &mut db,
            &state,
            (start as u32 + 1).into(),
            &blocks[start..200],
            &mut offload,
            None,
        )
        .unwrap()
        .unwrap();
        flush_remaining(&mut db, &mut offload).unwrap();
        let notes = db
            .select_spendable_notes(
                account,
                TargetValue::AtLeast(Zatoshis::from_u64(10_000).unwrap()),
                &[ShieldedPool::Orchard],
                201.into(),
                crate::confirmations_policy(ZNetwork::Regtest),
                &[],
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .unwrap();
        assert_eq!(notes.orchard().len(), 1);
        let note = &notes.orchard()[0];
        db.with_orchard_tree_mut(|tree| {
            for height in [101, 189, 190, 200] {
                assert_eq!(
                    tree.root_at_checkpoint_id(&height.into())?.unwrap(),
                    state_at(height).final_orchard_tree().root()
                );
            }
            let witness = tree
                .witness_at_checkpoint_id_caching(
                    note.note_commitment_tree_position(),
                    &200.into(),
                )?
                .unwrap();
            let leaf = MerkleHashOrchard::from_cmx(&note.note().commitment().into());
            assert_eq!(
                witness.root(leaf),
                state_at(200).final_orchard_tree().root()
            );
            Ok::<_, ShardTreeError<commitment_tree::Error>>(())
        })
        .unwrap();
        // A later pure-empty persist must retain the same real prefix at the
        // newer compact height even though no new leaf run is available.
        db.update_chain_tip(220.into()).unwrap();
        scan_batch(
            ZNetwork::Regtest,
            &mut db,
            &state_at(200),
            201.into(),
            &blocks[200..],
            &mut offload,
            None,
        )
        .unwrap()
        .unwrap();
        flush_remaining(&mut db, &mut offload).unwrap();
        db.with_orchard_tree_mut(|tree| {
            assert_eq!(
                tree.root_at_checkpoint_id(&220.into())?.unwrap(),
                state_at(220).final_orchard_tree().root()
            );
            Ok::<_, ShardTreeError<commitment_tree::Error>>(())
        })
        .unwrap();
    }
}

fn old_note_checkpoint_fixture_with_blocks() -> (
    tempfile::TempDir,
    super::super::wallet::SyncDb,
    AccountUuid,
    NativeOffload,
    MerkleHashOrchard,
    Vec<CompactBlock>,
) {
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("wallet.sqlite")).unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    let seed = secrecy::SecretVec::new(vec![7; 32]);
    let birthday = AccountBirthday::from_parts(empty_state(0), None);
    let (account, _) = db
        .create_account("anchor regression", &seed, &birthday, None)
        .unwrap();
    db.update_chain_tip(10_000.into()).unwrap();
    let ufvk = test_ufvk(7);
    let mut blocks: Vec<_> = (1..=10_000).map(compact_at).collect();
    pay_orchard(&mut blocks[1], &ufvk);
    pay_orchard(&mut blocks[9_997], &test_ufvk(8));
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    let mut offload = NativeOffload::new(0, 0, 0);
    let (_, state) = scan_batch(
        ZNetwork::Regtest,
        &mut db,
        &empty_state(0),
        1.into(),
        &blocks[..1_000],
        &mut offload,
        None,
    )
    .unwrap()
    .unwrap();
    scan_batch(
        ZNetwork::Regtest,
        &mut db,
        &state,
        1_001.into(),
        &blocks[1_000..],
        &mut offload,
        None,
    )
    .unwrap()
    .unwrap();
    let keys = ScanningKeys::from_account_ufvks([(account, ufvk)]);
    let (all, _) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &blocks,
        None,
    )
    .unwrap()
    .unwrap();
    let expected = chain_state_after(
        &empty_state(0),
        &all,
        10_000.into(),
        compact_block_hash(blocks.last().unwrap()).unwrap(),
    )
    .unwrap()
    .final_orchard_tree()
    .root();
    (dir, db, account, offload, expected, blocks)
}

fn old_note_checkpoint_fixture() -> (
    tempfile::TempDir,
    super::super::wallet::SyncDb,
    AccountUuid,
    NativeOffload,
    MerkleHashOrchard,
) {
    let (dir, db, account, offload, expected, _) = old_note_checkpoint_fixture_with_blocks();
    (dir, db, account, offload, expected)
}

fn assert_old_note_selectable_with_witness(
    db: &mut super::super::wallet::SyncDb,
    account: AccountUuid,
    expected_root: MerkleHashOrchard,
) {
    use zcash_client_backend::data_api::{
        wallet::input_selection::{LockFilter, LockedInputPolicy},
        InputSource, TargetValue,
    };
    use zcash_protocol::{value::Zatoshis, ShieldedPool};
    let (target, anchor) = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .unwrap()
        .unwrap();
    assert_eq!(u32::from(anchor), 10_000);
    // Also support the three-confirmation policy without inventing a height
    // whose compact metadata was not actually persisted.
    assert_eq!(
        u32::from(
            db.get_target_and_anchor_heights(std::num::NonZeroU32::new(3).unwrap())
                .unwrap()
                .unwrap()
                .1
        ),
        9_998
    );
    let notes = db
        .select_spendable_notes(
            account,
            TargetValue::AtLeast(Zatoshis::from_u64(10_000).unwrap()),
            &[ShieldedPool::Orchard],
            target,
            crate::confirmations_policy(ZNetwork::Regtest),
            &[],
            LockFilter::Policy(&LockedInputPolicy::Exclude),
        )
        .unwrap();
    assert_eq!(notes.orchard().len(), 1);
    let note = &notes.orchard()[0];
    db.with_orchard_tree_mut(|tree| {
        let root = tree.root_at_checkpoint_id(&anchor)?.unwrap();
        let witness = tree
            .witness_at_checkpoint_id_caching(note.note_commitment_tree_position(), &anchor)?
            .unwrap();
        let leaf = MerkleHashOrchard::from_cmx(&note.note().commitment().into());
        assert_eq!(witness.root(leaf), root);
        assert_eq!(root, expected_root);
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
}

#[test]
fn old_note_empty_suffix_has_current_shared_anchor_and_witness() {
    let (_dir, mut db, account, mut offload, expected) = old_note_checkpoint_fixture();
    assert_eq!(
        u32::from(
            db.get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
                .unwrap()
                .unwrap()
                .1
        ),
        0
    );
    flush_remaining(&mut db, &mut offload).unwrap();
    assert_old_note_selectable_with_witness(&mut db, account, expected);
    db.with_sapling_tree_mut(|tree| {
        assert_eq!(
            tree.store()
                .get_checkpoint(&10_000.into())
                .map_err(ShardTreeError::Storage)?
                .unwrap()
                .position(),
            None
        );
        assert!(
            tree.store()
                .checkpoint_count()
                .map_err(ShardTreeError::Storage)?
                <= tree.max_checkpoints()
        );
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
}

#[test]
fn checkpoint_positions_ignore_preseeded_future_roots() {
    use incrementalmerkletree::Hashable;
    let (_dir, mut db, account, mut offload, expected) = old_note_checkpoint_fixture();
    db.with_orchard_tree_mut(|tree| {
        tree.insert(
            Address::from_parts(Level::from(crate::offload::SHARD_HEIGHT), 1),
            MerkleHashOrchard::empty_root(Level::from(crate::offload::SHARD_HEIGHT)),
        )?;
        assert!(u64::from(tree.max_leaf_position(None)?.unwrap()) > 4);
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
    flush_remaining(&mut db, &mut offload).unwrap();
    db.with_orchard_tree_mut(|tree| {
        assert_eq!(
            tree.store()
                .get_checkpoint(&10_000.into())
                .map_err(ShardTreeError::Storage)?
                .unwrap()
                .position()
                .map(u64::from),
            Some(3)
        );
        assert_eq!(
            tree.store()
                .get_checkpoint(&9_997.into())
                .map_err(ShardTreeError::Storage)?
                .unwrap()
                .position()
                .map(u64::from),
            Some(1)
        );
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
    assert_old_note_selectable_with_witness(&mut db, account, expected);
}

#[test]
fn failed_checkpoint_coverage_rolls_back_all_pools_and_keeps_retry_state() {
    let (_dir, mut db, account, mut offload, expected) = old_note_checkpoint_fixture();
    let put_metadata = |db: &mut super::super::wallet::SyncDb, block: &BlockWatermark| {
        db.transactionally(|wdb| {
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
            Ok::<_, SqliteClientError>(())
        })
        .unwrap();
    };
    offload
        .checkpoint_span
        .last_mut()
        .unwrap()
        .orchard_tree_size += 1;
    // Match the persisted watermark so the external-change fence succeeds.
    // The actual tree failure must happen after Sapling checkpoint writes,
    // exercising cross-pool transaction rollback rather than a pre-write exit.
    put_metadata(&mut db, offload.checkpoint_span.last().unwrap());
    let failure = flush_remaining(&mut db, &mut offload).unwrap_err();
    assert!(
        failure.to_string().contains("checkpoint at 10000"),
        "{failure}"
    );
    assert!(!offload.checkpoint_span.is_empty());
    db.with_sapling_tree_mut(|tree| {
        assert!(tree
            .store()
            .get_checkpoint(&10_000.into())
            .map_err(ShardTreeError::Storage)?
            .is_none());
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
    offload
        .checkpoint_span
        .last_mut()
        .unwrap()
        .orchard_tree_size -= 1;
    put_metadata(&mut db, offload.checkpoint_span.last().unwrap());
    flush_remaining(&mut db, &mut offload).unwrap();
    assert_old_note_selectable_with_witness(&mut db, account, expected);
}

#[test]
fn persisted_old_wallet_recovers_checkpoints_without_rescanning() {
    let (dir, mut db, account, mut offload, expected) = old_note_checkpoint_fixture();
    flush_remaining(&mut db, &mut offload).unwrap();
    let path = dir.path().join("wallet.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    for pool in ["sapling", "orchard", "ironwood"] {
        conn.execute(&format!("DELETE FROM {pool}_tree_checkpoints"), [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {pool}_tree_checkpoints (checkpoint_id,position) VALUES (0,NULL)"
            ),
            [],
        )
        .unwrap();
    }
    assert_eq!(
        u32::from(
            db.get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
                .unwrap()
                .unwrap()
                .1
        ),
        0
    );
    repair_persisted_checkpoints(&mut db, &path, 10_000, 9_998).unwrap();
    assert_old_note_selectable_with_witness(&mut db, account, expected);
    let before: u32 = conn
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    repair_persisted_checkpoints(&mut db, &path, 10_000, 9_998).unwrap();
    let after: u32 = conn
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    assert_eq!(
        before, after,
        "already repaired wallets perform no checkpoint writes"
    );
    assert_old_note_selectable_with_witness(&mut db, account, expected);
}

#[test]
fn matching_checkpoint_preserves_removed_mark_metadata() {
    use shardtree::store::Checkpoint;
    let (_dir, mut db, _account, mut offload, _expected) = old_note_checkpoint_fixture();
    flush_remaining(&mut db, &mut offload).unwrap();
    db.with_orchard_tree_mut(|tree| {
        tree.store_mut()
            .update_checkpoint_with(&10_000.into(), |checkpoint| {
                *checkpoint =
                    Checkpoint::from_parts(checkpoint.tree_state(), [Position::from(0)].into());
                Ok(())
            })
            .map_err(ShardTreeError::Storage)?;
        checkpoint_exact_positions(tree, [(BlockHeight::from_u32(10_000), 4)].into_iter())?;
        assert!(tree
            .store()
            .get_checkpoint(&10_000.into())
            .map_err(ShardTreeError::Storage)?
            .unwrap()
            .marks_removed()
            .contains(&Position::from(0)));
        Ok::<_, SqliteClientError>(())
    })
    .unwrap();
}

#[test]
fn exact_checkpoint_retains_old_root_and_witness_after_appends_and_mark_removal() {
    let mut db = benchmark_empty_db();
    db.with_orchard_tree_mut(|tree| {
        let leaf = |id: u8| {
            Option::<MerkleHashOrchard>::from(MerkleHashOrchard::from_bytes(&[id; 32])).unwrap()
        };
        for id in 1..=6 {
            tree.append(
                leaf(id),
                if id == 1 || id == 5 {
                    Retention::Marked
                } else {
                    Retention::Ephemeral
                },
            )?;
        }
        let anchor = BlockHeight::from_u32(20);
        checkpoint_exact_positions(tree, [(anchor, 6)].into_iter())?;
        tree.ensure_retained(anchor)?;
        let expected = tree.root_at_checkpoint_id(&anchor)?.unwrap();
        tree.append(leaf(7), Retention::Ephemeral)?;
        tree.append(
            leaf(8),
            Retention::Checkpoint {
                id: 21.into(),
                marking: Marking::None,
            },
        )?;
        tree.remove_mark(Position::from(4), Some(&21.into()))?;
        // Prune the checkpoint that carried the later mark removal while the
        // old exact anchor remains explicitly retained.
        for height in 22..=u32::try_from(tree.max_checkpoints()).unwrap() + 23 {
            tree.append(
                leaf(9),
                Retention::Checkpoint {
                    id: height.into(),
                    marking: Marking::None,
                },
            )?;
        }
        assert_eq!(tree.root_at_checkpoint_id(&anchor)?.unwrap(), expected);
        let witness = tree
            .witness_at_checkpoint_id(Position::from(0), &anchor)?
            .unwrap();
        assert_eq!(witness.root(leaf(1)), expected);
        Ok::<_, SqliteClientError>(())
    })
    .unwrap();
}

#[test]
fn persisted_recovery_ignores_ancient_pruned_positions() {
    let (dir, mut db, _account, mut offload, _expected) = old_note_checkpoint_fixture();
    flush_remaining(&mut db, &mut offload).unwrap();
    // The unmarked output pair at positions2..3 has legitimately compacted to
    // a subtree root: the earlier interior prefix at position2 is unavailable.
    db.with_orchard_tree_mut(|tree| {
        assert!(tree
            .root(Address::from_parts(Level::from(32), 0), Position::from(3))
            .is_err());
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
    let path = dir.path().join("wallet.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("INSERT INTO blocks (height,hash,time,sapling_tree,sapling_commitment_tree_size,orchard_commitment_tree_size,ironwood_commitment_tree_size) VALUES(5000,zeroblob(32),0,X'00',0,3,0)", []).unwrap();
    // Model the old sparse layout, retaining only historical watermarks and tip.
    conn.execute(
        "DELETE FROM blocks WHERE height >= 9900 AND height < 10000",
        [],
    )
    .unwrap();
    for pool in ["sapling", "orchard", "ironwood"] {
        conn.execute(&format!("DELETE FROM {pool}_tree_checkpoints"), [])
            .unwrap();
    }
    repair_persisted_checkpoints(&mut db, &path, 10_000, 10_000).unwrap();
    let anchor = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(u32::from(anchor), 10_000);
    db.with_orchard_tree_mut(|tree| {
        assert!(tree
            .store()
            .get_checkpoint(&5000.into())
            .map_err(ShardTreeError::Storage)?
            .is_none());
        assert_eq!(
            tree.store()
                .get_checkpoint(&anchor)
                .map_err(ShardTreeError::Storage)?
                .unwrap()
                .position(),
            Some(Position::from(3))
        );
        Ok::<_, ShardTreeError<commitment_tree::Error>>(())
    })
    .unwrap();
}

#[test]
fn compact_checkpoint_recovery_rejects_disconnected_or_wrong_tip_metadata() {
    let (dir, mut db, account, mut offload, expected) = old_note_checkpoint_fixture();
    flush_remaining(&mut db, &mut offload).unwrap();
    let path = dir.path().join("wallet.sqlite");
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "DELETE FROM blocks WHERE height >= 9900 AND height < 10000",
        [],
    )
    .unwrap();
    for pool in ["sapling", "orchard", "ironwood"] {
        conn.execute(&format!("DELETE FROM {pool}_tree_checkpoints"), [])
            .unwrap();
    }
    // Actual final three compacts from the fixture: one two-action bundle at
    // 9998 followed by two empty blocks. Payload commitments are needed only
    // for count continuity; database tree bytes remain the authoritative tree.
    let mut compacts: Vec<_> = (9_998..=10_000).map(compact_at).collect();
    pay_orchard(&mut compacts[0], &test_ufvk(8));
    ensure_compact_chain_metadata(&mut compacts, 0, 2, 0);
    let mut broken = compacts.clone();
    broken[1].prev_hash[0] ^= 1;
    assert!(repair_checkpoints_from_compacts(&mut db, 10_000, &broken, &[]).is_err());
    let mut wrong_tip = compacts.clone();
    wrong_tip[2].hash[0] ^= 1;
    assert!(repair_checkpoints_from_compacts(&mut db, 10_000, &wrong_tip, &[]).is_err());
    assert!(db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .unwrap()
        .is_none());
    repair_checkpoints_from_compacts(&mut db, 10_000, &compacts, &[]).unwrap();
    assert_old_note_selectable_with_witness(&mut db, account, expected);
}

#[test]
fn healthy_tip_does_not_hide_required_anchor_retention_or_position_failure() {
    let (dir, mut db, _, mut offload, _) = old_note_checkpoint_fixture();
    flush_remaining(&mut db, &mut offload).unwrap();
    let path = dir.path().join("wallet.sqlite");
    // Remove only the earlier checkpoint's boundary flag; the later tip and
    // every checkpoint row still look healthy to get_target_and_anchor_heights.
    db.with_orchard_tree_mut(|tree| {
        let (_, boundary_hash, _) = stored_checkpoint_boundary(tree, Position::from(1))?.unwrap();
        let address = Address::from_parts(Level::from(crate::offload::SHARD_HEIGHT), 0);
        let shard = tree
            .store()
            .get_shard(address)
            .map_err(ShardTreeError::Storage)?
            .unwrap();
        let shard = shard.map(&|(hash, flags)| {
            (
                *hash,
                if *hash == boundary_hash {
                    *flags & !shardtree::RetentionFlags::CHECKPOINT
                } else {
                    *flags
                },
            )
        });
        tree.store_mut()
            .put_shard(shard)
            .map_err(ShardTreeError::Storage)?;
        assert!(checkpoint_matches(tree, 10_000.into(), 4)?);
        assert!(!checkpoint_matches(tree, 9_997.into(), 2)?);
        Ok::<_, SqliteClientError>(())
    })
    .unwrap();
    assert!(repair_persisted_checkpoints(&mut db, &path, 10_000, 9_997).unwrap());
    db.with_orchard_tree_mut(|tree| {
        assert!(checkpoint_matches(tree, 9_997.into(), 2)?);
        Ok::<_, SqliteClientError>(())
    })
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE orchard_tree_checkpoints SET position=2 WHERE checkpoint_id=9997",
        [],
    )
    .unwrap();
    assert!(repair_persisted_checkpoints(&mut db, &path, 10_000, 9_997).is_err());
}

#[test]
fn missing_legacy_prefix_requests_authentic_frontier_without_partial_writes() {
    let (dir, mut db, account, mut offload, expected, blocks) =
        old_note_checkpoint_fixture_with_blocks();
    flush_remaining(&mut db, &mut offload).unwrap();
    let path = dir.path().join("wallet.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    // Keep the same ordered commitment sequence, but split the two unrelated
    // actions across adjacent synthetic compact blocks to expose a prefix
    // inside the pruned two-leaf subtree. These are unit-fixture headers.
    let mut compacts = blocks[9_997..].to_vec();
    let mut second = compacts[0].vtx[0].clone();
    second.actions = vec![compacts[0].vtx[0].actions.pop().unwrap()];
    compacts[1].vtx.push(second);
    for block in &mut compacts {
        block.chain_metadata = None;
    }
    ensure_compact_chain_metadata(&mut compacts, 0, 2, 0);
    conn.execute("DELETE FROM blocks WHERE height=9998 OR height=9999", [])
        .unwrap();
    for pool in ["sapling", "orchard", "ironwood"] {
        conn.execute(
            &format!("DELETE FROM {pool}_tree_checkpoints WHERE checkpoint_id>=9998"),
            [],
        )
        .unwrap();
    }
    assert!(!repair_persisted_checkpoints(&mut db, &path, 10_000, 9_998).unwrap());
    let before: u32 = conn
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    assert_eq!(
        repair_checkpoints_from_compacts(&mut db, 10_000, &compacts, &[]).unwrap(),
        [9_998, 9_999]
    );
    let after: u32 = conn
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    assert_eq!(
        before, after,
        "missing coverage must not publish any checkpoint or metadata"
    );
    let mut prefix = blocks[..9_997].to_vec();
    prefix.push(compacts[0].clone());
    let keys = ScanningKeys::from_account_ufvks([(account, test_ufvk(7))]);
    let (scanned, _) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &prefix,
        None,
    )
    .unwrap()
    .unwrap();
    let frontier = chain_state_after(
        &empty_state(0),
        &scanned,
        9_998.into(),
        compact_block_hash(&compacts[0]).unwrap(),
    )
    .unwrap();
    assert_eq!(frontier.final_orchard_tree().tree_size(), 3);
    let wrong = ChainState::new(
        frontier.block_height(),
        BlockHash([9; 32]),
        frontier.final_sapling_tree().clone(),
        frontier.final_orchard_tree().clone(),
        frontier.final_ironwood_tree().clone(),
    );
    assert!(repair_checkpoints_from_compacts(&mut db, 10_000, &compacts, &[wrong]).is_err());
    assert_eq!(
        before,
        conn.pragma_query_value::<u32, _>(None, "data_version", |r| r.get(0))
            .unwrap()
    );
    assert_eq!(
        repair_checkpoints_from_compacts(&mut db, 10_000, &compacts, &[frontier.clone()]).unwrap(),
        [9_999]
    );
    assert_eq!(
        before,
        conn.pragma_query_value::<u32, _>(None, "data_version", |r| r.get(0))
            .unwrap()
    );
    let (suffix, _) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &compacts[1..],
        None,
    )
    .unwrap()
    .unwrap();
    let end_frontier = chain_state_after(
        &frontier,
        &suffix,
        9_999.into(),
        compact_block_hash(&compacts[1]).unwrap(),
    )
    .unwrap();
    let mut altered = incrementalmerkletree::frontier::Frontier::<MerkleHashOrchard, 32>::empty();
    for id in 1..=3 {
        altered.append(
            Option::<MerkleHashOrchard>::from(MerkleHashOrchard::from_bytes(&[id; 32])).unwrap(),
        );
    }
    let conflicting = ChainState::new(
        frontier.block_height(),
        frontier.block_hash(),
        frontier.final_sapling_tree().clone(),
        altered,
        frontier.final_ironwood_tree().clone(),
    );
    assert!(repair_checkpoints_from_compacts(
        &mut db,
        10_000,
        &compacts,
        &[conflicting, end_frontier.clone()]
    )
    .is_err());
    assert_eq!(
        before,
        conn.pragma_query_value::<u32, _>(None, "data_version", |r| r.get(0))
            .unwrap(),
        "conflicting authentic frontier data must roll back earlier empty-pool writes"
    );
    assert!(repair_checkpoints_from_compacts(
        &mut db,
        10_000,
        &compacts,
        &[frontier, end_frontier]
    )
    .unwrap()
    .is_empty());
    assert!(repair_persisted_checkpoints(&mut db, &path, 10_000, 9_998).unwrap());
    assert_old_note_selectable_with_witness(&mut db, account, expected);
}

#[test]
fn external_receipt_spend_or_account_change_blocks_stale_persist_and_final_flush() {
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    for change in ["receipt", "spend", "account"] {
        let (dir, mut db, _, mut offload, _, original_blocks) =
            old_note_checkpoint_fixture_with_blocks();
        let path = dir.path().join("wallet.sqlite");
        let observer = rusqlite::Connection::open(&path).unwrap();
        let context = TrialContext::new(load_ufvks(&db).unwrap(), load_nullifiers(&db).unwrap());
        let mut snapshot_blocks: Vec<_> = (10_001..=10_008).map(compact_at).collect();
        ensure_compact_chain_metadata(&mut snapshot_blocks, 0, 4, 0);
        let input = context.snapshot(&db, &snapshot_blocks).unwrap();
        let mut stale = decrypt_owned(
            ZNetwork::Regtest,
            input,
            empty_state(0),
            10_001.into(),
            snapshot_blocks,
            None,
        )
        .unwrap()
        .unwrap();
        // Simulate coalescing another trial from the same original inputs.
        let mut suffix: Vec<_> = (10_009..=10_016).map(compact_at).collect();
        ensure_compact_chain_metadata(&mut suffix, 0, 4, 0);
        stale.absorb(
            decrypt_owned(
                ZNetwork::Regtest,
                context.snapshot(&db, &suffix).unwrap(),
                empty_state(0),
                10_009.into(),
                suffix,
                None,
            )
            .unwrap()
            .unwrap(),
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
        rusqlite::vtab::array::load_module(&conn).unwrap();
        let mut other = zcash_client_sqlite::WalletDb::from_connection(
            conn,
            ZNetwork::Regtest,
            zcash_client_sqlite::util::SystemClock,
            rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        );
        if change == "account" {
            other
                .create_account(
                    "external account",
                    &secrecy::SecretVec::new(vec![9; 32]),
                    &AccountBirthday::from_parts(empty_state(0), None),
                    None,
                )
                .unwrap();
        } else {
            let (scanned, _) = scan_batch_parallel(
                ZNetwork::Regtest,
                context.keys(),
                &Nullifiers::empty(),
                &original_blocks,
                None,
            )
            .unwrap()
            .unwrap();
            let state = chain_state_after(
                &empty_state(0),
                &scanned,
                10_000.into(),
                compact_block_hash(original_blocks.last().unwrap()).unwrap(),
            )
            .unwrap();
            let mut actual: Vec<_> = (10_001..=10_008).map(compact_at).collect();
            pay_orchard(
                &mut actual[0],
                &test_ufvk(if change == "receipt" { 7 } else { 8 }),
            );
            if change == "spend" {
                actual[0].vtx[0].actions[0].nullifier =
                    context.nullifiers.orchard()[0].1.to_bytes().to_vec();
            }
            other.update_chain_tip(10_008.into()).unwrap();
            scan_batch(
                ZNetwork::Regtest,
                &mut other,
                &state,
                10_001.into(),
                &actual,
                &mut NativeOffload::new(0, 0, 0),
                None,
            )
            .unwrap()
            .unwrap();
        }
        let before: u32 = observer
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let retained = offload.checkpoint_span.len();
        let error = persist_batch(&mut db, stale, &mut offload).unwrap_err();
        assert!(
            error.to_string().contains("Wallet changed during sync"),
            "{change}: {error}"
        );
        assert_eq!(
            observer
                .pragma_query_value::<u32, _>(None, "data_version", |r| r.get(0))
                .unwrap(),
            before,
            "{change} stale persist wrote data"
        );
        assert_eq!(offload.checkpoint_span.len(), retained);
        assert!(
            flush_remaining(&mut db, &mut offload).is_err(),
            "{change} final flush"
        );
        assert_eq!(
            observer
                .pragma_query_value::<u32, _>(None, "data_version", |r| r.get(0))
                .unwrap(),
            before,
            "{change} final flush wrote data"
        );
        assert_eq!(offload.checkpoint_span.len(), retained);
        if change == "spend" {
            // Historic replay must not resurrect the earlier receipt's NF when
            // SQLite already knows its later spend outside this replay span.
            assert!(load_nullifiers(&db).unwrap().orchard().is_empty());
            let mut replay = NativeOffload::new(0, 0, 0);
            scan_batch(
                ZNetwork::Regtest,
                &mut db,
                &empty_state(0),
                1.into(),
                &original_blocks[..8],
                &mut replay,
                None,
            )
            .unwrap()
            .unwrap();
            flush_remaining(&mut db, &mut replay).unwrap();
            assert!(load_nullifiers(&db).unwrap().orchard().is_empty());
        }
    }
}
