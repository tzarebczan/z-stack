fn empty_scan_db() -> super::super::wallet::SyncDb {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    db
}

#[test]
fn pipeline_truncation_distinguishes_missing_row_from_sqlite_failure() {
    let mut db = empty_scan_db();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value INTEGER)")
        .unwrap();
    let error = truncate_existing(&mut db, &path, 100).unwrap_err();
    assert!(error.to_string().contains("read rewind source"));

    conn.execute_batch("CREATE TABLE blocks(height INTEGER PRIMARY KEY)")
        .unwrap();
    assert_eq!(truncate_existing(&mut db, &path, 100).unwrap(), None);
}

#[test]
fn streamed_note_free_chunks_bound_each_historic_write_to_eight_thousand_heights() {
    use selective_scan::tests::no_note_batch;

    let mut pending = None;
    let mut pending_range = None;
    let mut writes = Vec::new();
    for start in (1..=24_000).step_by(1_000) {
        let range = ScanRange::from_parts(
            BlockHeight::from_u32(start)..BlockHeight::from_u32(start + 1_000),
            ScanPriority::Historic,
        );
        absorb_pending(
            &mut pending,
            &mut pending_range,
            no_note_batch(start, 1_000),
            range,
        );
        if historic_pending_ready(&pending) {
            let batch = pending.take().unwrap();
            let range = pending_range.take().unwrap();
            writes.push((
                u32::from(range.block_range().start),
                u32::from(range.block_range().end),
                batch.height_count(),
            ));
        }
    }
    assert_eq!(
        writes,
        [
            (1, 8_001, 8_000),
            (8_001, 16_001, 8_000),
            (16_001, 24_001, 8_000)
        ]
    );
    assert!(pending.is_none());
}

#[test]
fn fetched_ranges_require_complete_hashes_and_parent_links() {
    use selective_scan::tests::compact_at;
    let valid: Vec<_> = (10..=12).map(compact_at).collect();
    expect_contiguous_range(&valid, 10, 12).unwrap();
    for index in 0..valid.len() {
        for len in [0, 1, 31, 33] {
            let mut malformed = valid.clone();
            malformed[index].prev_hash.resize(len, 0);
            assert!(expect_contiguous_range(&malformed, 10, 12).is_err());
            malformed = valid.clone();
            malformed[index].hash.resize(len, 0);
            assert!(expect_contiguous_range(&malformed, 10, 12).is_err());
        }
    }
    let mut fork = valid;
    fork[1].prev_hash[0] ^= 1;
    assert!(expect_contiguous_range(&fork, 10, 12).is_err());
}

#[test]
fn rebuilt_offload_keeps_only_roots_sqlite_still_holds() {
    use incrementalmerkletree::Hashable;
    use zcash_client_backend::data_api::chain::CommitmentTreeRoot;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roots.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    let roots: Vec<_> = (0..3u32)
        .map(|i| {
            CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(100 + i),
                // Distinct valid hashes; the table keeps roots unique.
                MerkleHashOrchard::empty_root((i as u8).into()),
            )
        })
        .collect();
    db.put_orchard_subtree_roots(3, &roots).unwrap();

    // A restart keeps the learned range: every root is still stored.
    let kept = rebuilt_offload(&mut db, true, [(0, 0), (3, 6), (0, 0)]).unwrap();
    assert_eq!(kept.root_ranges(), [(0, 0), (3, 6), (0, 0)]);

    // A rewind into shard 5 deletes that row; only shards 3 and 4 stay covered.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM orchard_tree_shards WHERE shard_index >= 5", [])
        .unwrap();
    let clamped = rebuilt_offload(&mut db, true, [(0, 0), (3, 6), (0, 0)]).unwrap();
    assert_eq!(clamped.root_ranges(), [(0, 0), (3, 5), (0, 0)]);
}

#[test]
fn fetched_ranges_must_extend_the_stored_tip_or_grafted_state() {
    use selective_scan::tests::{compact_at, empty_state};
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    let mut db = empty_scan_db();
    db.create_account(
        "reorg",
        &secrecy::SecretVec::new(vec![7; 32]),
        &AccountBirthday::from_parts(empty_state(0), None),
        None,
    )
    .unwrap();
    db.update_chain_tip(100.into()).unwrap();
    let blocks: Vec<_> = (1..=100).map(compact_at).collect();
    let mut off = NativeOffload::new(0, 0, 0);
    selective_scan::scan_batch(
        ZNetwork::Regtest,
        &mut db,
        &empty_state(0),
        1.into(),
        &blocks,
        &mut off,
        None,
    )
    .unwrap()
    .unwrap();
    selective_scan::flush_remaining(&mut db, &mut off).unwrap();

    // The stored row wins over an unrelated tree state.
    let tip = known_parent(&db, 100.into(), &empty_state(0)).unwrap();
    assert_eq!(tip, Some(BlockHash([100; 32])));
    assert!(extends_parent(&[compact_at(101)], tip.as_ref()));
    // After the scanned tip is orphaned, the server's next block names its
    // replacement. selective shard scanning must not append it onto the stale tip.
    let mut after_reorg = compact_at(101);
    after_reorg.prev_hash = vec![0xaa; 32];
    assert!(!extends_parent(&[after_reorg], tip.as_ref()));

    // No stored row: only a tree state at exactly the parent height counts.
    let graft = ChainState::new(
        150.into(),
        BlockHash([150; 32]),
        incrementalmerkletree::frontier::Frontier::empty(),
        incrementalmerkletree::frontier::Frontier::empty(),
        incrementalmerkletree::frontier::Frontier::empty(),
    );
    assert_eq!(
        known_parent(&db, 150.into(), &graft).unwrap(),
        Some(BlockHash([150; 32]))
    );
    assert_eq!(known_parent(&db, 149.into(), &graft).unwrap(), None);
    assert!(extends_parent(&[compact_at(150)], None));
}

#[tokio::test]
async fn continuity_rewind_requires_real_truncation_and_uses_actual_height() {
    use selective_scan::tests::{compact_at, empty_state};
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rewind.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    db.create_account(
        "rewind",
        &secrecy::SecretVec::new(vec![7; 32]),
        &AccountBirthday::from_parts(empty_state(0), None),
        None,
    )
    .unwrap();
    db.update_chain_tip(100.into()).unwrap();
    let blocks: Vec<_> = (1..=100).map(compact_at).collect();
    let mut off = NativeOffload::new(0, 0, 0);
    selective_scan::scan_batch(
        ZNetwork::Regtest,
        &mut db,
        &empty_state(0),
        1.into(),
        &blocks,
        &mut off,
        None,
    )
    .unwrap()
    .unwrap();
    selective_scan::flush_remaining(&mut db, &mut off).unwrap();
    let cache = FsBlockCache::open(dir.path().join("blocks")).unwrap();
    cache.insert(blocks).await.unwrap();
    let scanned = Arc::new(AtomicU32::new(1_000));
    let mut cached = Some(empty_state(100));
    // Sparse/no-row truncation remains a harmless no-op for ordinary callers.
    assert_eq!(truncate_existing(&mut db, &path, 0).unwrap(), None);
    let error = apply_continuity_rewind(&cache, &mut db, &path, &scanned, &mut cached, 5.into())
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("no usable persisted rewind point"));
    assert_eq!(
        super::super::wallet::last_filled_island_end(&path, 1, 100),
        100
    );
    assert!(db.block_metadata(100.into()).unwrap().is_some());

    // Zakura may choose a lower checkpoint than the requested blocks row.
    let control = rusqlite::Connection::open(&path).unwrap();
    for pool in ["sapling", "orchard", "ironwood"] {
        control
            .execute(
                &format!(
                    "DELETE FROM {pool}_tree_checkpoints WHERE checkpoint_id BETWEEN 45 AND 50"
                ),
                [],
            )
            .unwrap();
    }
    apply_continuity_rewind(&cache, &mut db, &path, &scanned, &mut cached, 60.into())
        .await
        .unwrap();
    assert_eq!(scanned.load(Ordering::Relaxed), 44);
    assert_eq!(cache.get_tip_height(None).unwrap(), Some(44.into()));
    assert!(db.block_metadata(45.into()).unwrap().is_none());
    assert!(cached.is_none());
}

#[tokio::test]
async fn dropped_queued_or_awaited_fetch_jobs_cancel_and_release_live_buffers() {
    for awaited in [false, true] {
        let live: LiveBlocks = Arc::new(Mutex::new(Vec::new()));
        let weak = Arc::downgrade(&live);
        let task_live = Arc::clone(&live);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (alive, cancelled) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            started.send(()).unwrap();
            let result = std::future::pending::<Result<()>>().await;
            drop((alive, task_live));
            result
        });
        ready.await.unwrap();
        let job = FetchJob {
            range: ScanRange::from_parts(1.into()..2.into(), ScanPriority::Historic),
            live,
            handle,
        };
        if awaited {
            let waiter = tokio::spawn(take_fetched(job));
            tokio::task::yield_now().await;
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
        } else {
            // The pool's queue has the same ownership when an error drops it.
            drop(VecDeque::from([job]));
        }
        assert!(tokio::time::timeout(Duration::from_secs(2), cancelled)
            .await
            .expect("fetch future must be cancelled")
            .is_err());
        assert!(
            weak.upgrade().is_none(),
            "aborted fetch kept its live block buffer"
        );
    }
}

#[tokio::test]
async fn outstanding_trials_are_joined_before_restart_or_error_returns() {
    use selective_scan::tests::empty_state;
    let finished = Arc::new(AtomicU32::new(0));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                finished.fetch_add(1, Ordering::Relaxed);
                Err(EngineError::Message("discarded speculative failure".into()))
            })
        })
        .collect();
    let mut handles = handles.into_iter();
    let prefix = StreamedTrial {
        source: vec![],
        handle: handles.next().unwrap(),
    };
    let inflight = InflightTrial {
        range: ScanRange::from_parts(1.into()..2.into(), ScanPriority::Historic),
        from_state: empty_state(0),
        start_sizes: (0, 0, 0),
        handle: handles.next().unwrap(),
    };
    join_outstanding_trials(Some(prefix), Some(inflight)).await;
    assert_eq!(finished.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn streamed_first_note_refreshes_before_and_after_prefix_boundary() {
    use selective_scan::tests::{compact_at, empty_state, pay_orchard, test_ufvk};
    use zcash_client_backend::scanning::{scan_block, Nullifiers, ScanningKeys};
    let ufvk = test_ufvk(7);
    let stranger = test_ufvk(8);
    let account = zcash_client_sqlite::AccountUuid::default();
    let keys = ScanningKeys::from_account_ufvks([(account, ufvk.clone())]);
    let mut prior_block = compact_at(8_000);
    pay_orchard(&mut prior_block, &stranger);
    crate::scan::ensure_compact_chain_metadata(std::slice::from_mut(&mut prior_block), 0, 0, 0);
    let prior_scan = scan_block(
        &ZNetwork::Regtest,
        prior_block,
        &keys,
        &Nullifiers::empty(),
        None,
    )
    .unwrap();
    let prior = selective_scan::chain_state_after(
        &empty_state(0),
        &[prior_scan],
        8_000.into(),
        zcash_primitives::block::BlockHash([0; 32]),
    )
    .unwrap();
    for receive_index in [100, 400] {
        let mut db = empty_scan_db();
        let ufvks = selective_scan::AccountUfvks::from([(account, ufvk.clone())]);
        let context = selective_scan::TrialContext::new(ufvks.clone(), Nullifiers::empty());
        let scanned = Arc::new(AtomicU32::new(0));
        let mut blocks: Vec<_> = (8_001..=9_000).map(compact_at).collect();
        pay_orchard(&mut blocks[receive_index], &ufvk);
        // The separate shielded suffix used to be duplicated by the note tail.
        pay_orchard(&mut blocks[receive_index + 1], &stranger);
        crate::scan::ensure_compact_chain_metadata(&mut blocks, 0, 2, 0);
        let prefix_job = spawn_streamed_trial(
            ZNetwork::Regtest,
            context
                .snapshot(&db, &blocks[..STREAM_DECRYPT_BLOCKS as usize])
                .unwrap(),
            (0, 2, 0),
            8_001.into(),
            blocks[..STREAM_DECRYPT_BLOCKS as usize].to_vec(),
        )
        .unwrap();
        let expected = if receive_index < STREAM_DECRYPT_BLOCKS as usize {
            8_000
        } else {
            8_256
        };
        let mut fetches = Vec::new();
        let result = finish_streamed_with_fetch(
            prefix_job,
            blocks,
            empty_state(0),
            (0, 2, 0),
            8_001.into(),
            ZNetwork::Regtest,
            &mut db,
            &context,
            &scanned,
            |height| {
                fetches.push(height);
                std::future::ready(Ok(ChainState::new(
                    height.into(),
                    prior.block_hash(),
                    prior.final_sapling_tree().clone(),
                    prior.final_orchard_tree().clone(),
                    prior.final_ironwood_tree().clone(),
                )))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fetches, [expected]);
        assert_eq!(result.height_count(), 1_000);
        assert_eq!(result.scanned.len(), 2);
        assert_eq!(selective_scan::tree_sizes(&result.next_state), (0, 6, 0));
        assert_eq!(result.last_height, 9_000.into());
    }
}

#[test]
#[ignore = "manual native decrypt setup microbenchmark"]
fn benchmark_trial_setup() {
    use std::{hint::black_box, time::Instant};
    use zcash_client_backend::scanning::ScanningKeys;
    let db = empty_scan_db();
    let ufvk = selective_scan::tests::test_ufvk(7);
    let account = zcash_client_sqlite::AccountUuid::default();
    let iterations = 1_000;
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(selective_scan::load_nullifiers(&db).unwrap());
    }
    let nullifier_us = started.elapsed().as_micros();
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(ScanningKeys::from_account_ufvks([(account, ufvk.clone())]));
    }
    let keys_us = started.elapsed().as_micros();
    let keys = Arc::new(ScanningKeys::from_account_ufvks([(account, ufvk)]));
    let nfs = Arc::new(selective_scan::load_nullifiers(&db).unwrap());
    let started = Instant::now();
    for _ in 0..iterations {
        black_box((Arc::clone(&keys), Arc::clone(&nfs)));
    }
    eprintln!("trial setup {iterations} iterations: empty sqlite nullifiers {nullifier_us}us, one-account scanning keys {keys_us}us, shared snapshots {}us; profile={}", started.elapsed().as_micros(), if cfg!(debug_assertions) { "debug" } else { "release" });
}

#[tokio::test]
async fn streamed_retry_replaces_a_stale_note_free_prefix() {
    use selective_scan::tests::{compact_at, empty_state, pay_orchard, test_ufvk};
    use zcash_client_backend::scanning::Nullifiers;
    let mut db = empty_scan_db();
    let ufvk = test_ufvk(7);
    let ufvks =
        selective_scan::AccountUfvks::from([(zcash_client_sqlite::AccountUuid::default(), ufvk.clone())]);
    let mut original: Vec<_> = (1..=1_000).map(compact_at).collect();
    crate::scan::ensure_compact_chain_metadata(&mut original, 0, 0, 0);
    let context = selective_scan::TrialContext::new(ufvks, Nullifiers::empty());
    let prefix_job = spawn_streamed_trial(
        ZNetwork::Regtest,
        context
            .snapshot(&db, &original[..STREAM_DECRYPT_BLOCKS as usize])
            .unwrap(),
        (0, 0, 0),
        1.into(),
        original[..STREAM_DECRYPT_BLOCKS as usize].to_vec(),
    )
    .unwrap();
    // Model a retry replacing the compact payload while the old trial is in
    // flight. Preserve header hashes too: compare exact contents, not just hashes.
    let mut retried = original;
    pay_orchard(&mut retried[100], &ufvk);
    pay_orchard(&mut retried[400], &test_ufvk(8));
    crate::scan::ensure_compact_chain_metadata(&mut retried, 0, 0, 0);
    let result = finish_streamed_with_fetch(
        prefix_job,
        retried,
        empty_state(0),
        (0, 0, 0),
        1.into(),
        ZNetwork::Regtest,
        &mut db,
        &context,
        &Arc::new(AtomicU32::new(0)),
        |_| {
            std::future::ready(Err(EngineError::Message(
                "unexpected frontier fetch".into(),
            )))
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.had_notes,
        "the successful retry's prefix receipt must be retained"
    );
    assert_eq!(result.scanned.len(), 2);
    assert_eq!(
        result.scanned[0].transactions()[0].orchard_outputs().len(),
        1
    );
    assert_eq!(selective_scan::tree_sizes(&result.next_state), (0, 4, 0));
    assert_eq!(result.height_count(), 1_000);
}

#[tokio::test]
async fn split_suffix_retry_retains_completed_prefix_and_removes_failed_suffix() {
    use selective_scan::tests::compact_at;
    // A completed left half already exists when the right half starts.
    let live = Arc::new(Mutex::new(vec![compact_at(1), compact_at(2)]));
    let mut calls = 0;
    fetch_range_attempts(3, 6, 3, Some(Arc::clone(&live)), |attempt_live| {
        calls += 1;
        let shared = attempt_live.unwrap();
        let mut blocks = shared.lock().unwrap();
        assert_eq!(blocks.iter().map(|b| b.height).collect::<Vec<_>>(), [1, 2]);
        let result = match calls {
            1 => {
                // Even a successful RPC must retry a malformed live range.
                blocks.extend([compact_at(3), compact_at(5)]);
                Ok(Vec::new())
            }
            2 => {
                blocks.extend([compact_at(3), compact_at(4)]);
                Err(EngineError::Transport("interrupted stream".into()))
            }
            3 => {
                blocks.extend((3..=6).map(compact_at));
                Ok(Vec::new())
            }
            _ => panic!("retry bound exceeded"),
        };
        std::future::ready(result)
    })
    .await
    .unwrap();
    assert_eq!(calls, 3);
    let final_blocks = live.lock().unwrap();
    expect_contiguous_range(&final_blocks, 1, 6).unwrap();
    assert_eq!(
        final_blocks.iter().map(|b| b.height).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn trial_cache_refreshes_only_after_committed_receipts_and_spends() {
    use selective_scan::tests::{compact_at, empty_state, pay_orchard, test_ufvk};
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("cache.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    db.create_account(
        "cache regression",
        &secrecy::SecretVec::new(vec![7; 32]),
        &AccountBirthday::from_parts(empty_state(0), None),
        None,
    )
    .unwrap();
    db.update_chain_tip(24.into()).unwrap();
    let mut context = selective_scan::TrialContext::new(
        selective_scan::load_ufvks(&db).unwrap(),
        selective_scan::load_nullifiers(&db).unwrap(),
    );
    let mut offload = NativeOffload::new(0, 0, 0);
    let scanned = Arc::new(AtomicU32::new(0));
    let mut cached = None;
    let mut receive: Vec<_> = (1..=8).map(compact_at).collect();
    pay_orchard(&mut receive[0], &test_ufvk(7));
    let decrypt = |db: &mut super::super::wallet::SyncDb,
                   context: &selective_scan::TrialContext,
                   state: &ChainState,
                   blocks: &[CompactBlock]| {
        let d = selective_scan::decrypt_owned(
            ZNetwork::Regtest,
            context.snapshot(db, blocks).unwrap(),
            state.clone(),
            (blocks[0].height as u32).into(),
            blocks.to_vec(),
            None,
        )
        .unwrap()
        .unwrap();
        selective_scan::complete_decrypted(ZNetwork::Regtest, db, context, d, None)
            .unwrap()
            .unwrap()
    };
    let batch = decrypt(&mut db, &context, &empty_state(0), &receive);
    let nf = batch.scanned[0].transactions()[0].orchard_outputs()[0]
        .nf()
        .unwrap();
    let after_receive = batch.next_state.clone();
    let mut spend: Vec<_> = (9..=16).map(compact_at).collect();
    pay_orchard(&mut spend[0], &test_ufvk(8));
    spend[0].vtx[0].actions[0].nullifier = nf.to_bytes().to_vec();
    let mut pending = Some(batch);
    let mut range = Some(ScanRange::from_parts(
        1.into()..9.into(),
        ScanPriority::Verify,
    ));
    let control = rusqlite::Connection::open(&path).unwrap();
    control.execute_batch("CREATE TRIGGER reject_cache_batch BEFORE INSERT ON blocks WHEN NEW.height=1 BEGIN SELECT RAISE(ABORT, 'injected cache persist failure'); END;").unwrap();
    assert!(persist_pending(
        &mut db,
        &mut offload,
        &scanned,
        &mut cached,
        &mut pending,
        &mut range,
        &None,
        &mut context
    )
    .is_err());
    assert!(
        !decrypt(&mut db, &context, &after_receive, &spend).had_notes,
        "failed receipt persist must not publish its nullifier"
    );
    control
        .execute_batch("DROP TRIGGER reject_cache_batch;")
        .unwrap();
    pending = Some(decrypt(&mut db, &context, &empty_state(0), &receive));
    range = Some(ScanRange::from_parts(
        1.into()..9.into(),
        ScanPriority::Verify,
    ));
    assert!(matches!(
        persist_pending(
            &mut db,
            &mut offload,
            &scanned,
            &mut cached,
            &mut pending,
            &mut range,
            &None,
            &mut context
        )
        .unwrap(),
        Step::Continue
    ));
    let old = context.clone();
    let spend_batch = decrypt(&mut db, &context, &after_receive, &spend);
    assert!(
        spend_batch.had_notes,
        "committed receipt refreshes the next job"
    );
    assert!(spend_batch.scanned[0].transactions()[0]
        .orchard_outputs()
        .is_empty());
    assert_eq!(
        spend_batch.scanned[0].transactions()[0]
            .orchard_spends()
            .len(),
        1
    );
    pending = Some(spend_batch);
    range = Some(ScanRange::from_parts(
        9.into()..17.into(),
        ScanPriority::Verify,
    ));
    assert!(matches!(
        persist_pending(
            &mut db,
            &mut offload,
            &scanned,
            &mut cached,
            &mut pending,
            &mut range,
            &None,
            &mut context
        )
        .unwrap(),
        Step::Continue
    ));
    assert!(
        !decrypt(&mut db, &context, &after_receive, &spend).had_notes,
        "spend-only commit removes the spent nullifier from the next snapshot"
    );
    assert!(
        decrypt(&mut db, &old, &after_receive, &spend).had_notes,
        "already-spawned jobs retain their immutable previous snapshot"
    );
}
