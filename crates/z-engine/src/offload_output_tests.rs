use super::*;
use std::hint::black_box;
use std::mem::size_of;
use std::time::{Duration, Instant};

fn encoded(leaves: &[KeptLeaf]) -> Vec<(u64, [u8; 32], u8, u32)> {
    leaves
        .iter()
        .map(|leaf| (leaf.position, leaf.hash, leaf.kind, leaf.height))
        .collect()
}

fn runs<H: NoteLeaf>(
    runs: &[KeptRun<H>],
) -> Vec<(u64, bool, Vec<([u8; 32], Retention<BlockHeight>)>)> {
    runs.iter()
        .map(|run| {
            (
                run.start,
                run.historic,
                run.leaves
                    .iter()
                    .map(|(h, r)| (h.encode(), r.clone()))
                    .collect(),
            )
        })
        .collect()
}

fn assert_selected<H: NoteLeaf>(
    both: &mut PoolAcc<H>,
    leaves: &mut PoolAcc<H>,
    typed: &mut PoolAcc<H>,
) {
    assert_eq!(encoded(&both.drain_kept()), encoded(&leaves.drain_kept()));
    assert_eq!(runs(&both.drain_runs()), runs(&typed.drain_runs()));
    assert_eq!(
        leaves.kept_runs.capacity(),
        0,
        "web never allocates typed mirror runs"
    );
    assert_eq!(
        typed.kept.capacity(),
        0,
        "native never allocates encoded mirror leaves"
    );
    assert_eq!(both.dropped, leaves.dropped);
    assert_eq!(both.dropped, typed.dropped);
    assert_eq!(encoded(&both.buffer_kept()), encoded(&leaves.buffer_kept()));
    assert_eq!(encoded(&both.buffer_kept()), encoded(&typed.buffer_kept()));
}

fn output_parity<H: NoteLeaf>() {
    let mut both = PoolAcc::<H>::new(6, OffloadOutput::Both);
    let mut leaves = PoolAcc::<H>::new(6, OffloadOutput::EncodedLeaves);
    let mut typed = PoolAcc::<H>::new(6, OffloadOutput::Runs);
    for acc in [&mut both, &mut leaves, &mut typed] {
        acc.retain_checkpoints_from = Some(100.into());
    }
    let empty = H::empty_leaf();
    let other = H::combine(Level::from(0), &empty, &empty);
    for shard in 0..7 {
        let n = if shard < 4 { SHARD_SIZE as usize } else { 31 };
        let mut values = vec![(empty.clone(), Retention::Ephemeral); n];
        values[1].0 = other.clone();
        if shard == 2 || shard == 4 {
            values[3].1 = Retention::Marked;
            values[4].1 = Retention::Reference;
            values[5].1 = Retention::Checkpoint {
                id: 99.into(),
                marking: Marking::Marked,
            };
        }
        if shard == 3 || shard == 6 {
            values[n - 1].1 = Retention::Checkpoint {
                id: 100.into(),
                marking: Marking::Reference,
            };
        }
        for acc in [&mut both, &mut leaves, &mut typed] {
            acc.feed(shard * SHARD_SIZE, &values, 100);
            acc.flush_durable();
        }
        assert_selected(&mut both, &mut leaves, &mut typed);
        // The selected mode and open buffered commitments survive the same
        // clone-before-transaction boundary used by native persistence.
        both = both.clone();
        leaves = leaves.clone();
        typed = typed.clone();
    }
    for acc in [&mut both, &mut leaves, &mut typed] {
        acc.flush();
        acc.flush(); // Repeated finalization does not duplicate output.
    }
    assert_selected(&mut both, &mut leaves, &mut typed);
    assert_eq!(
        both.dropped, 1,
        "only the unmarked full interior shard is dropped"
    );
}

#[test]
fn selected_outputs_preserve_hashes_positions_retention_and_drop_policy() {
    output_parity::<sapling::Node>();
    output_parity::<MerkleHashOrchard>();
}

fn feed_all(acc: &mut Offload, start: u64, count: usize, height: u32) {
    let mut sapling = vec![(sapling::Node::empty_leaf(), Retention::Ephemeral); count];
    let mut orchard = vec![(MerkleHashOrchard::empty_leaf(), Retention::Ephemeral); count];
    sapling[count - 1].1 = Retention::Checkpoint {
        id: height.into(),
        marking: Marking::Marked,
    };
    orchard[count - 1].1 = Retention::Checkpoint {
        id: height.into(),
        marking: Marking::Reference,
    };
    acc.sapling.feed(start, &sapling, height);
    acc.orchard.feed(start, &orchard, height);
    acc.ironwood.feed(start, &orchard, height);
}

#[test]
fn repeated_persists_and_discarded_candidates_keep_only_selected_outputs() {
    for output in [OffloadOutput::EncodedLeaves, OffloadOutput::Runs] {
        let mut selected = Offload::new(0, 0, 0, output);
        let mut baseline = Offload::new(0, 0, 0, OffloadOutput::Both);
        for batch in 0..64 {
            let start = batch * 37;
            let height = batch as u32 + 1;
            // Failed transactions discard a fully finalized candidate without
            // changing pending leaves, mode, or outputs in the original.
            let mut failed = selected.clone();
            feed_all(&mut failed, start, 37, height);
            failed.flush_durable();
            drop(failed);
            assert!(!selected.has_pending());
            assert_eq!(duplicate_bytes(&selected, output), 0);

            let mut next = selected.clone();
            feed_all(&mut next, start, 37, height);
            next.flush_durable();
            feed_all(&mut baseline, start, 37, height);
            baseline.flush_durable();
            if output.leaves() {
                assert_eq!(
                    encoded(&next.sapling.drain_kept()),
                    encoded(&baseline.sapling.drain_kept())
                );
                assert_eq!(
                    encoded(&next.orchard.drain_kept()),
                    encoded(&baseline.orchard.drain_kept())
                );
                assert_eq!(
                    encoded(&next.ironwood.drain_kept()),
                    encoded(&baseline.ironwood.drain_kept())
                );
            } else {
                assert_eq!(
                    runs(&next.sapling.drain_runs()),
                    runs(&baseline.sapling.drain_runs())
                );
                assert_eq!(
                    runs(&next.orchard.drain_runs()),
                    runs(&baseline.orchard.drain_runs())
                );
                assert_eq!(
                    runs(&next.ironwood.drain_runs()),
                    runs(&baseline.ironwood.drain_runs())
                );
            }
            selected = next;
            assert_eq!(duplicate_bytes(&selected, output), 0);
            assert!(
                duplicate_bytes(&baseline, output) > 0,
                "former mirror grows even after consumer drains"
            );
        }
    }
}

fn pool_duplicate_bytes<H>(pool: &PoolAcc<H>, consumer: OffloadOutput) -> usize {
    if consumer.leaves() {
        pool.kept_runs.capacity() * size_of::<KeptRun<H>>()
            + pool
                .kept_runs
                .iter()
                .map(|run| run.leaves.capacity() * size_of::<(H, Retention<BlockHeight>)>())
                .sum::<usize>()
    } else {
        pool.kept.capacity() * size_of::<KeptLeaf>()
    }
}

fn duplicate_bytes(acc: &Offload, consumer: OffloadOutput) -> usize {
    pool_duplicate_bytes(&acc.sapling, consumer)
        + pool_duplicate_bytes(&acc.orchard, consumer)
        + pool_duplicate_bytes(&acc.ironwood, consumer)
}

fn drain_consumer(acc: &mut Offload, consumer: OffloadOutput) {
    if consumer.leaves() {
        black_box(acc.sapling.drain_kept());
        black_box(acc.orchard.drain_kept());
        black_box(acc.ironwood.drain_kept());
    } else {
        black_box(acc.sapling.drain_runs());
        black_box(acc.orchard.drain_runs());
        black_box(acc.ironwood.drain_runs());
    }
}

fn measure(
    batches: usize,
    consumer: OffloadOutput,
    emitted: OffloadOutput,
) -> (Duration, usize, Duration) {
    let mut acc = Offload::new(0, 0, 0, emitted);
    let count = 1024;
    let start = Instant::now();
    for batch in 0..batches {
        if !consumer.leaves() {
            acc = black_box(acc.clone()); // Native transactional candidate.
        }
        feed_all(&mut acc, (batch * count) as u64, count, batch as u32 + 1);
        if !consumer.leaves() {
            acc.flush_durable();
        }
        drain_consumer(&mut acc, consumer);
    }
    acc.flush();
    drain_consumer(&mut acc, consumer);
    let elapsed = start.elapsed();
    let bytes = duplicate_bytes(&acc, consumer);
    let clone_start = Instant::now();
    for _ in 0..16 {
        black_box(acc.clone());
    }
    (elapsed, bytes, clone_start.elapsed() / 16)
}

/// Accumulator work only; excludes decryption, tree hashing, SQLite and snapshots.
/// The dual-output baseline preserves the former unused-mirror accumulation but
/// benefits from the same buffer-to-run move as selected outputs, conservatively
/// understating the old native per-batch clone work.
/// Run: cargo test -p z-engine --features native --release --lib benchmark_consumer_outputs -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_consumer_outputs() {
    for (name, consumer) in [
        ("native", OffloadOutput::Runs),
        ("web", OffloadOutput::EncodedLeaves),
    ] {
        for batches in [32, 128, 512] {
            for (label, output) in [("dual", OffloadOutput::Both), ("selected", consumer)] {
                let mut samples: Vec<_> =
                    (0..3).map(|_| measure(batches, consumer, output)).collect();
                samples.sort_by_key(|sample| sample.0);
                let (elapsed, bytes, clone_time) = samples[1];
                if label == "selected" {
                    assert_eq!(bytes, 0);
                }
                eprintln!("output-mode {name} {label}: batches={batches} leaves_per_pool={} accumulator_ms={:.3} unused_capacity_bytes={bytes} final_clone_us={:.3}", batches * 1024, elapsed.as_secs_f64() * 1000.0, clone_time.as_secs_f64() * 1_000_000.0);
            }
        }
    }
}
