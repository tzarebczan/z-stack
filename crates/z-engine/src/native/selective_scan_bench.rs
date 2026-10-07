// Manual component comparisons. Candidate calls the production native paths.

fn benchmark_empty_db() -> super::super::wallet::SyncDb {
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

fn benchmark_compacts(
    shielded_every: Option<usize>,
    bundles_per_block: usize,
) -> Vec<CompactBlock> {
    let mut blocks: Vec<_> = (1..=1_000).map(compact_at).collect();
    if let Some(every) = shielded_every {
        // Reuse genuine encrypted foreign output payloads. This is a decrypt/
        // hashing benchmark, not a claim that this synthetic chain is valid.
        let mut foreign = compact_at(1);
        pay_orchard(&mut foreign, &test_ufvk(8));
        for block in blocks.iter_mut().step_by(every) {
            for index in 0..bundles_per_block {
                let mut tx = foreign.vtx[0].clone();
                tx.index = index as u64;
                tx.txid[..8].copy_from_slice(&block.height.to_le_bytes());
                tx.txid[8..16].copy_from_slice(&(index as u64).to_le_bytes());
                block.vtx.push(tx);
            }
        }
    }
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    blocks
}

fn median_micros(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "manual native setup plus real compact trial-decrypt comparison"]
fn benchmark_native_trial_with_setup() {
    use std::hint::black_box;
    let db = benchmark_empty_db();
    let ufvk = test_ufvk(7);
    let account = AccountUuid::default();
    let context = TrialContext::new(
        AccountUfvks::from([(account, ufvk.clone())]),
        load_nullifiers(&db).unwrap(),
    );
    let iterations = 20;
    for (label, spacing) in [
        ("empty", None),
        ("sparse", Some(100)),
        ("shielded", Some(4)),
    ] {
        let blocks = benchmark_compacts(spacing, 1);
        let mut rebuilt = Vec::new();
        let mut reused = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            for _ in 0..iterations {
                let keys = ScanningKeys::from_account_ufvks([(account, ufvk.clone())]);
                let nfs = load_nullifiers(&db).unwrap();
                let mut owned = blocks.clone();
                ensure_compact_chain_metadata(&mut owned, 0, 0, 0);
                black_box(compact_span(&owned).unwrap());
                let trial = scan_batch_parallel(ZNetwork::Regtest, &keys, &nfs, &owned, None)
                    .unwrap()
                    .unwrap();
                assert!(trial.1.is_none());
                black_box(trial);
            }
            rebuilt.push(started.elapsed().as_micros());
            let started = Instant::now();
            for _ in 0..iterations {
                let trial = trial_decrypt(
                    ZNetwork::Regtest,
                    context.snapshot(&db, &blocks).unwrap(),
                    0,
                    0,
                    0,
                    1.into(),
                    blocks.clone(),
                    None,
                )
                .unwrap()
                .unwrap();
                assert!(!trial.had_notes);
                black_box(trial);
            }
            reused.push(started.elapsed().as_micros());
        }
        eprintln!("native trial {label}:1000 heights, {} shielded blocks; median5x{iterations} current setup+scan={}us, shared snapshots+scan={}us; profile={}", blocks.iter().filter(|b| compact_has_shielded(b)).count(), median_micros(rebuilt), median_micros(reused), if cfg!(debug_assertions) { "debug" } else { "release" });
    }
    let blocks = benchmark_compacts(Some(1), 1)[..4].to_vec();
    let mut rebuilt = Vec::new();
    let mut reused = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..iterations {
            let keys = ScanningKeys::from_account_ufvks([(account, ufvk.clone())]);
            let nfs = load_nullifiers(&db).unwrap();
            let owned = blocks.clone();
            black_box(compact_span(&owned).unwrap());
            black_box(
                scan_batch_sequential(ZNetwork::Regtest, &keys, nfs, 1.into(), &owned, None)
                    .unwrap()
                    .unwrap(),
            );
        }
        rebuilt.push(started.elapsed().as_micros());
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(
                trial_decrypt(
                    ZNetwork::Regtest,
                    context.snapshot(&db, &blocks).unwrap(),
                    0,
                    0,
                    0,
                    1.into(),
                    blocks.clone(),
                    None,
                )
                .unwrap()
                .unwrap(),
            );
        }
        reused.push(started.elapsed().as_micros());
    }
    eprintln!("native tiny sequential4 heights: median5x{iterations} original setup={}us, shared keys+owned nullifier reload={}us", median_micros(rebuilt), median_micros(reused));
}

fn benchmark_walk_with_row_frontiers(
    from: &ChainState,
    scanned: &[NativeScanned],
    span: &[BlockWatermark],
) -> (ChainState, Vec<(BlockHeight, BlockHeight, ChainState)>) {
    let (end, rows) = walk_with_row_frontiers(from, scanned, span).unwrap();
    (
        end,
        rows.into_iter()
            .map(|r| (r.first, r.last, r.prior))
            .collect(),
    )
}

fn assert_same_benchmark_state(actual: &ChainState, expected: &ChainState) {
    assert_eq!(actual.block_height(), expected.block_height());
    assert_eq!(actual.block_hash(), expected.block_hash());
    assert_eq!(actual.final_sapling_tree(), expected.final_sapling_tree());
    assert_eq!(actual.final_orchard_tree(), expected.final_orchard_tree());
    assert_eq!(actual.final_ironwood_tree(), expected.final_ironwood_tree());
}

#[test]
fn row_frontiers_preserve_real_anchors_and_adjacent_batch_boundaries() {
    let ufvk = test_ufvk(7);
    let foreign = test_ufvk(8);
    let keys = ScanningKeys::from_account_ufvks([(AccountUuid::default(), ufvk.clone())]);
    let mut blocks: Vec<_> = (1..=12).map(compact_at).collect();
    for height in [2, 6, 7, 11] {
        pay_orchard(&mut blocks[height - 1], &ufvk);
    }
    for height in [4, 5, 8, 10] {
        pay_orchard(&mut blocks[height - 1], &foreign);
    }
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    let span = compact_span(&blocks).unwrap();
    let (scanned, _) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &blocks,
        None,
    )
    .unwrap()
    .unwrap();
    let from = empty_state(0);
    let split = scanned.partition_point(|block| block.height() <= BlockHeight::from_u32(6));
    let (middle, mut rows) =
        benchmark_walk_with_row_frontiers(&from, &scanned[..split], &span[..6]);
    let (actual, suffix_rows) =
        benchmark_walk_with_row_frontiers(&middle, &scanned[split..], &span[6..]);
    rows.extend(suffix_rows);
    assert_eq!(
        rows.iter()
            .map(|(first, last, _)| (u32::from(*first), u32::from(*last)))
            .collect::<Vec<_>>(),
        [(2, 2), (4, 6), (7, 8), (10, 11)],
        "adjacent batches keep their separately captured run anchors",
    );
    for (first, _, prior) in rows {
        let before = scanned.partition_point(|block| block.height() < first);
        let prior_hash = if u32::from(first) == 1 {
            from.block_hash()
        } else {
            span[(u32::from(first) - 2) as usize].hash
        };
        let expected = chain_state_after(&from, &scanned[..before], first - 1, prior_hash).unwrap();
        assert_same_benchmark_state(&prior, &expected);
    }
    let last = span.last().unwrap();
    let expected = chain_state_after(&from, &scanned, last.height, last.hash).unwrap();
    assert_same_benchmark_state(&actual, &expected);
}

#[test]
#[ignore = "manual note-batch duplicate frontier hashing comparison"]
fn benchmark_native_note_frontier_walk() {
    use std::hint::black_box;
    let ufvk = test_ufvk(7);
    let keys = ScanningKeys::from_account_ufvks([(AccountUuid::default(), ufvk.clone())]);
    let mut blocks = benchmark_compacts(Some(4), 4);
    for index in [101, 501, 901] {
        pay_orchard(&mut blocks[index], &ufvk);
    }
    // Recompute sizes after inserting owned outputs into the synthetic fixture.
    for block in &mut blocks {
        block.chain_metadata = None;
    }
    ensure_compact_chain_metadata(&mut blocks, 0, 0, 0);
    let (scanned, first) = scan_batch_parallel(
        ZNetwork::Regtest,
        &keys,
        &Nullifiers::empty(),
        &blocks,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(first.is_some());
    let from = empty_state(0);
    let last = 1_000.into();
    let hash = compact_block_hash(blocks.last().unwrap()).unwrap();
    let expected = chain_state_after(&from, &scanned, last, hash).unwrap();
    let span = compact_span(&blocks).unwrap();
    let (candidate, frontiers) = benchmark_walk_with_row_frontiers(&from, &scanned, &span);
    assert_eq!(
        candidate.final_sapling_tree(),
        expected.final_sapling_tree()
    );
    assert_eq!(
        candidate.final_orchard_tree(),
        expected.final_orchard_tree()
    );
    assert_eq!(
        candidate.final_ironwood_tree(),
        expected.final_ironwood_tree()
    );
    assert_eq!(frontiers.len(), 3);
    let iterations = 3;
    let mut twice = Vec::new();
    let mut once = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(chain_state_after(&from, &scanned, last, hash).unwrap());
            black_box(chain_state_after(&from, &scanned, last, hash).unwrap());
        }
        twice.push(started.elapsed().as_micros());
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(benchmark_walk_with_row_frontiers(&from, &scanned, &span));
        }
        once.push(started.elapsed().as_micros());
    }
    eprintln!("native note frontier:1000 heights,{} commitments,{} wallet row runs; median5x{iterations} two append walks={}us, one walk+real frontier snapshots={}us; excludes SQLite/Offload/decrypt; profile={}", scanned.iter().map(|b| b.orchard().commitments().len()).sum::<usize>(), frontiers.len(), median_micros(twice), median_micros(once), if cfg!(debug_assertions) { "debug" } else { "release" });
}

#[test]
#[ignore = "manual cost of transactional scan-input validation; excludes scan/tree/row writes"]
fn benchmark_native_persist_input_validation() {
    use std::hint::black_box;
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    let mut empty = benchmark_empty_db();
    empty
        .create_account(
            "validation cost",
            &secrecy::SecretVec::new(vec![7; 32]),
            &AccountBirthday::from_parts(empty_state(0), None),
            None,
        )
        .unwrap();
    let (_dir, mut funded, _, _, _) = old_note_checkpoint_fixture();
    let iterations = 1_000;
    for (label, db) in [
        ("one-account/no-notes", &mut empty),
        ("one-account/one-unspent-note", &mut funded),
    ] {
        let expected = ScanIdentity::new(&load_ufvks(db).unwrap(), &load_nullifiers(db).unwrap());
        let mut samples = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            for _ in 0..iterations {
                db.transactionally(|wdb| {
                    let identity = ScanIdentity::new(
                        &wdb.get_unified_full_viewing_keys()?,
                        &Nullifiers::unspent(wdb)?,
                    );
                    assert!(identity == expected);
                    black_box(Arc::new(
                        identity.with_nullifiers(&Nullifiers::unspent(wdb)?),
                    ));
                    Ok::<_, SqliteClientError>(())
                })
                .unwrap();
            }
            samples.push(started.elapsed().as_micros());
        }
        eprintln!("native persist input validation {label}: median5x{iterations} transactional pre-read+comparison+post-read={}us; excludes tree/row writes; profile={}",
            median_micros(samples), if cfg!(debug_assertions) { "debug" } else { "release" });
    }
}
