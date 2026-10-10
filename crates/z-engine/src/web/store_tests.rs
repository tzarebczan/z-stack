//! Regression and opt-in cost measurements for snapshot/leaf persistence.

use super::*;
use crate::keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC};
use crate::offload::SHARD_SIZE;
use incrementalmerkletree::Hashable;
use prost::Message;
use zcash_client_backend::proto::compact_formats::{ChainMetadata, CompactBlock};

fn wallet() -> WebWallet {
    WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
        1,
    )
    .unwrap()
}

/// Snapshot JSON without the saved hashed trees, which differ with the set of
/// pools a wallet happens to have built.
fn leaves_and_metadata(bytes: Vec<u8>) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v.as_object_mut().unwrap().remove("liveTrees");
    v
}

fn leaf(position: u64) -> TreeLeaf {
    TreeLeaf {
        hash: to_hex(&orchard::tree::MerkleHashOrchard::empty_leaf().to_bytes()),
        kind: 0,
        height: 1,
        position,
    }
}

fn empty_block(height: u64, prev_hash: Vec<u8>) -> CompactBlock {
    let mut hash = vec![0; 32];
    hash[..8].copy_from_slice(&height.to_le_bytes());
    CompactBlock {
        height,
        hash,
        prev_hash,
        time: 1,
        header: vec![],
        vtx: vec![],
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        }),
    }
}

fn append_pool(w: &mut WebWallet, pool: &str, height: u32, count: usize, marked: bool) {
    let retention = if marked {
        Retention::Marked
    } else {
        Retention::Ephemeral
    };
    // Nonempty, distinct sibling hashes make a lost-prefix witness disagree
    // with the independently built reference instead of resembling padding.
    let sapling = if marked {
        sapling::Node::empty_leaf()
    } else {
        sapling::Node::from_bytes([height as u8; 32]).unwrap()
    };
    let orchard = if marked {
        MerkleHashOrchard::empty_leaf()
    } else {
        MerkleHashOrchard::from_bytes(&[height as u8; 32]).unwrap()
    };
    match pool {
        "sapling" => w.append_sapling(height, &vec![(sapling, retention); count]),
        "orchard" => w.append_orchard(height, &vec![(orchard, retention); count]),
        "ironwood" => w.append_ironwood(height, &vec![(orchard, retention); count]),
        _ => unreachable!(),
    }
    .unwrap();
}

fn record_test_height(w: &mut WebWallet, height: u32) {
    w.set_scanned(
        height,
        PriorMeta {
            height,
            hash: to_hex(&empty_block(height.into(), vec![]).hash),
            sapling_tree_size: w.sapling_next as u32,
            orchard_tree_size: w.orchard_next as u32,
            ironwood_tree_size: w.ironwood_next as u32,
        },
    )
    .unwrap();
}

fn assert_pool_witness(
    w: &mut WebWallet,
    reference: &mut WebWallet,
    pool: &str,
    position: u64,
    height: u32,
) {
    fn check<
        H: Hashable + Clone + PartialEq + std::fmt::Debug,
        const DEPTH: u8,
        const SHARD: u8,
    >(
        tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
        reference: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
        leaf: H,
        position: u64,
        height: u32,
    ) {
        let anchor = BlockHeight::from_u32(height);
        let root = reference.root_at_checkpoint_id(&anchor).unwrap().unwrap();
        assert_eq!(
            tree.root_at_checkpoint_id(&anchor).unwrap(),
            Some(root.clone())
        );
        let witness = tree
            .witness_at_checkpoint_id(Position::from(position), &anchor)
            .unwrap()
            .unwrap();
        assert_eq!(witness.root(leaf), root);
    }
    match pool {
        "sapling" => check(
            &mut w.sapling_tree,
            &mut reference.sapling_tree,
            sapling::Node::empty_leaf(),
            position,
            height,
        ),
        "orchard" => check(
            &mut w.orchard_tree,
            &mut reference.orchard_tree,
            MerkleHashOrchard::empty_leaf(),
            position,
            height,
        ),
        "ironwood" => check(
            &mut w.ironwood_tree,
            &mut reference.ironwood_tree,
            MerkleHashOrchard::empty_leaf(),
            position,
            height,
        ),
        _ => unreachable!(),
    }
}

#[test]
fn deferred_pools_rebuild_on_first_receipt_and_preserve_live_witnesses() {
    for (active, bit) in [
        ("sapling", SAPLING_LIVE),
        ("orchard", ORCHARD_LIVE),
        ("ironwood", IRONWOOD_LIVE),
    ] {
        let mut w = wallet();
        w.finalize_scan_trees().unwrap();
        let mut reference = wallet();
        reference.finalize_scan_trees().unwrap();
        // Model the previous all-pool live hashing policy as an independent
        // Merkle reference. Leaves and metadata do not depend on which pools
        // are built; only the saved hashed trees do.
        reference.live_pools = SAPLING_LIVE | ORCHARD_LIVE | IRONWOOD_LIVE;
        for height in 1..=2 {
            for pool in ["sapling", "orchard", "ironwood"] {
                append_pool(&mut w, pool, height, 4, false);
                append_pool(&mut reference, pool, height, 4, false);
            }
            record_test_height(&mut w, height);
            record_test_height(&mut reference, height);
            w.finalize_scan_trees().unwrap();
            assert_eq!(w.live_pools, 0);
            assert_eq!(
                leaves_and_metadata(w.to_snapshot().unwrap()),
                leaves_and_metadata(reference.to_snapshot().unwrap())
            );
        }
        assert!(w
            .sapling_tree
            .store()
            .get_shard(SaplingTree::subtree_addr(0.into()))
            .unwrap()
            .is_none());
        assert!(w
            .orchard_tree
            .store()
            .get_shard(OrchardTree::subtree_addr(0.into()))
            .unwrap()
            .is_none());
        assert!(w
            .ironwood_tree
            .store()
            .get_shard(OrchardTree::subtree_addr(0.into()))
            .unwrap()
            .is_none());
        w.prepare_received_pool(active);
        assert!(!w.sinsemilla_live);
        for current in [&mut w, &mut reference] {
            append_pool(current, active, 3, 1, true);
            record_test_height(current, 3);
            current.finalize_scan_trees().unwrap();
        }
        assert_eq!(w.live_pools, bit);
        assert_pool_witness(&mut w, &mut reference, active, 8, 3);
        for current in [&mut w, &mut reference] {
            for pool in ["sapling", "orchard", "ironwood"] {
                append_pool(current, pool, 4, 3, false);
            }
            record_test_height(current, 4);
            current.finalize_scan_trees().unwrap();
        }
        assert_eq!(w.live_pools, bit);
        assert_pool_witness(&mut w, &mut reference, active, 8, 3);
        assert_pool_witness(&mut w, &mut reference, active, 8, 4);
        assert_eq!(
            leaves_and_metadata(w.to_snapshot().unwrap()),
            leaves_and_metadata(reference.to_snapshot().unwrap())
        );
        // The saved hashed trees load: witnesses work without a finalize.
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.sinsemilla_live);
        assert_eq!(restored.live_pools, bit);
        assert_pool_witness(&mut restored, &mut reference, active, 8, 4);
        restored.finalize_scan_trees().unwrap();
        assert_eq!(restored.live_pools, bit);
        assert_pool_witness(&mut restored, &mut reference, active, 8, 3);
        restored.rewind_to_height(2).unwrap();
        assert_eq!(
            restored.live_pools, 0,
            "rewinding away the only note defers its pool again"
        );
        restored.reset_scan();
        assert_eq!(restored.live_pools, 0);
        assert!(!restored.sinsemilla_live);
    }
}

#[test]
fn sparse_deferred_pools_flush_before_first_receipt_rebuild() {
    for (active, bit) in [
        ("sapling", SAPLING_LIVE),
        ("orchard", ORCHARD_LIVE),
        ("ironwood", IRONWOOD_LIVE),
    ] {
        let mut w = wallet();
        for pool in ["sapling", "orchard", "ironwood"] {
            let root = if pool == "sapling" {
                sapling::Node::empty_root(SAPLING_SHARD_HEIGHT.into()).to_bytes()
            } else {
                MerkleHashOrchard::empty_root(ORCHARD_SHARD_HEIGHT.into()).to_bytes()
            };
            w.apply_subtree_roots_json(
                pool,
                &serde_json::json!({"roots": [{"completingHeight": 1, "rootHash": to_hex(&root)}]})
                    .to_string(),
            )
            .unwrap();
        }
        // A completed unmarked historic shard is represented only by its root.
        w.sapling_next = SHARD_SIZE;
        w.orchard_next = SHARD_SIZE;
        w.ironwood_next = SHARD_SIZE;
        record_test_height(&mut w, 1);
        w.finalize_scan_trees().unwrap();
        assert_eq!(w.live_pools, 0);
        for pool in ["sapling", "orchard", "ironwood"] {
            append_pool(&mut w, pool, 2, 4, false);
        }
        record_test_height(&mut w, 2);
        assert!(w.offload.is_some());
        // Persist while deferred leaves are still in the bounded open buffer.
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        w.finalize_scan_trees().unwrap();
        restored.finalize_scan_trees().unwrap();
        for current in [&mut w, &mut restored] {
            current.prepare_received_pool(active);
            append_pool(current, active, 3, 1, true);
            record_test_height(current, 3);
            current.finalize_scan_trees().unwrap();
            assert_eq!(current.live_pools, bit);
        }
        assert_pool_witness(&mut w, &mut restored, active, SHARD_SIZE + 4, 3);
        for current in [&mut w, &mut restored] {
            for pool in ["sapling", "orchard", "ironwood"] {
                append_pool(current, pool, 4, 2, false);
            }
            record_test_height(current, 4);
            current.finalize_scan_trees().unwrap();
            assert_eq!(current.live_pools, bit);
        }
        assert_pool_witness(&mut w, &mut restored, active, SHARD_SIZE + 4, 3);
        assert_pool_witness(&mut w, &mut restored, active, SHARD_SIZE + 4, 4);
        assert_eq!(w.to_snapshot().unwrap(), restored.to_snapshot().unwrap());
    }
}

#[test]
fn failed_rebuild_never_publishes_live_pool_mask_and_rewind_recovers() {
    let mut w = wallet();
    for pool in ["sapling", "orchard", "ironwood"] {
        append_pool(&mut w, pool, 1, 1, true);
    }
    record_test_height(&mut w, 1);
    w.finalize_scan_trees().unwrap();
    assert_eq!(w.live_pools, SAPLING_LIVE | ORCHARD_LIVE | IRONWOOD_LIVE);
    let saved = w.to_snapshot().unwrap();
    let mut conflict = w.orchard_leaves[0].clone();
    conflict.hash = "11".repeat(32);
    conflict.height = 2;
    w.orchard_leaves.push(conflict);
    assert!(w.rebuild_trees().is_err());
    assert_eq!(w.live_pools, 0);
    assert!(!w.sinsemilla_live);
    assert!(w.to_snapshot().is_err());
    assert!(w.finalize_scan_trees().is_err());
    assert_eq!(w.rewind_to_height(1).unwrap(), 1);
    assert_eq!(w.live_pools, SAPLING_LIVE | ORCHARD_LIVE | IRONWOOD_LIVE);
    assert_eq!(w.to_snapshot().unwrap(), saved);

    // An invalid later pool fails after earlier pools have been reconstructed.
    // Neither a partial tree nor the previous mask may become publishable.
    w.invalidate_live_trees();
    w.ironwood_leaves.push(TreeLeaf {
        position: 1,
        hash: "invalid hex".into(),
        kind: 1,
        height: 2,
    });
    w.ironwood_next = 2;
    record_test_height(&mut w, 2);
    assert!(w.rebuild_trees().is_err());
    assert_eq!(w.live_pools, 0);
    assert!(!w.sinsemilla_live);
    assert!(w.to_snapshot().is_err());
    w.reset_scan();
    w.finalize_scan_trees().unwrap();
    assert_eq!(w.live_pools, 0);
    assert!(w.to_snapshot().is_ok());
}

#[test]
fn incremental_validation_preserves_conflicts_and_marked_duplicates() {
    let mut leaves = vec![leaf(0), leaf(1)];
    let mut validation = LeafValidation::default();
    validation.validate(&mut leaves, "orchard").unwrap();
    let mut marked = leaf(0);
    marked.kind = 1;
    leaves.push(marked);
    validation.validate(&mut leaves, "orchard").unwrap();
    assert_eq!(leaves.len(), 2);
    assert_eq!(leaves[0].kind, 1);

    let mut conflict = leaf(1);
    conflict.hash = "11".repeat(32);
    leaves.push(conflict);
    let before = serde_json::to_vec(&leaves).unwrap();
    assert!(validation.validate(&mut leaves, "orchard").is_err());
    assert_eq!(serde_json::to_vec(&leaves).unwrap(), before);
    assert_eq!(validation.len, 2, "failure cannot bless the invalid suffix");
}

#[test]
fn apply_conflict_cannot_replace_saved_snapshot_and_reset_recovers() {
    let mut w = wallet();
    w.orchard_leaves = vec![leaf(0)];
    w.assert_consistent_leaf_hashes().unwrap();
    let saved = w.to_snapshot().unwrap();
    let mut bad = leaf(0);
    bad.hash = "11".repeat(32);
    w.orchard_leaves.push(bad);

    let block = empty_block(1, vec![0; 32]);
    assert!(w
        .apply_compact_blocks_blob(&super::super::encode_one(&block))
        .is_err());
    assert_eq!(
        w.orchard_leaves
            .iter()
            .filter(|l| !l.hash.is_empty())
            .count(),
        2
    );
    assert!(w.to_snapshot().is_err());
    assert!(w.apply_compact_block(&block.encode_to_vec()).is_err());
    assert!(WebWallet::from_snapshot(&saved).is_ok());
    w.reset_scan();
    w.apply_compact_block(&block.encode_to_vec()).unwrap();
    assert!(w.to_snapshot().is_ok());
}

#[test]
fn current_snapshot_duplicate_zero_is_not_migrated_as_legacy() {
    let mut w = wallet();
    w.orchard_leaves = vec![leaf(0), leaf(0)];
    w.orchard_leaves[1].hash = "11".repeat(32);
    let bytes = w.to_snapshot().unwrap();
    let err = WebWallet::from_snapshot(&bytes).err().unwrap().to_string();
    assert!(err.contains("two hashes at leaf 0"), "{err}");

    let mut legacy: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    legacy.as_object_mut().unwrap().remove("version");
    let migrated = WebWallet::from_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(migrated.orchard_leaves[1].position, 1);
}

#[test]
fn borrowed_snapshot_preserves_open_offload_buffer_and_json_layout() {
    let mut w = wallet();
    w.orchard_leaves = vec![leaf(0)];
    w.orchard_next = 3;
    let mut off = Offload::new(1, 1, 1, OffloadOutput::EncodedLeaves);
    off.orchard.feed(
        1,
        &[(MerkleHashOrchard::empty_leaf(), Retention::Marked); 2],
        1,
    );
    w.offload = Some(off);
    let bytes = w.to_snapshot().unwrap();
    let owned: Snapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(owned.orchard_leaves.len(), 3);
    assert_eq!(owned.orchard_leaves[2].position, 2);
    assert_eq!(
        serde_json::to_vec(&owned).unwrap(),
        bytes,
        "old JSON field layout stays byte-compatible"
    );
    let restored = WebWallet::from_snapshot(&bytes).unwrap();
    assert_eq!(restored.orchard_leaves.len(), 3);
    assert_eq!(restored.orchard_next, 3);
    assert!(
        w.offload.is_some(),
        "saving does not flush/mutate scan state"
    );
}

#[test]
fn memo_completion_survives_reload_and_filters_before_limit() {
    let mut w = wallet();
    for i in 0..100 {
        w.txs.insert(
            format!("{i:064x}"),
            TxAgg {
                mined_height: Some(i),
                received_notes: 1,
                enhancement_complete: i >= 20,
                history_metadata_complete: i >= 20,
                ..Default::default()
            },
        );
    }
    assert_eq!(
        w.memo_enhancement_txids(2),
        [19, 18].map(|id| WebWallet::display_txid(&format!("{id:064x}")))
    );
    let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert_eq!(restored.memo_enhancement_txids(500).len(), 20);

    let mut legacy: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
    for agg in legacy["txs"].as_object_mut().unwrap().values_mut() {
        agg.as_object_mut()
            .unwrap()
            .remove("historyMetadataComplete");
    }
    let legacy = WebWallet::from_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(
        legacy.memo_enhancement_txids(500).len(),
        100,
        "previously enhanced snapshots need a one-time history metadata upgrade"
    );

    let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
    for agg in old["txs"].as_object_mut().unwrap().values_mut() {
        agg.as_object_mut().unwrap().remove("enhancementComplete");
    }
    let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(restored.memo_enhancement_txids(500).len(), 100);
    assert!(restored.enhance_raw_tx(b"invalid transaction").is_err());
    assert_eq!(restored.memo_enhancement_txids(500).len(), 100);
}

#[test]
fn empty_blobs_do_not_construct_scanning_keys() {
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    assert_eq!(w.scanned_height(), 2);
    assert!(w.cached_scan_keys.is_none());
}

#[test]
fn abandon_resolves_canonical_ids_once_without_reverse_alias_fallback() {
    let id = TxId::from_bytes(std::array::from_fn(|i| i as u8));
    let wire = to_hex(id.as_ref());
    let canonical = id.to_string();
    assert_ne!(wire, canonical);
    let mut w = wallet();
    // The opposite ID's wire key happens to spell the requested public ID.
    // It must not become a fallback when the requested transaction is absent.
    w.txs.insert(canonical.clone(), TxAgg::default());
    let before = w.to_snapshot().unwrap();
    assert!(!w.abandon_unmined(&canonical).unwrap());
    assert_eq!(w.to_snapshot().unwrap(), before);
    w.txs.insert(wire.clone(), TxAgg::default());
    assert!(w.abandon_unmined(&canonical.to_uppercase()).unwrap());
    assert!(!w.txs.contains_key(&wire));
    assert!(w.txs.contains_key(&canonical));
    assert!(w.abandon_unmined(&wire.to_uppercase()).unwrap());
    assert!(w.txs.is_empty());
    assert!(w.abandon_unmined("invalid transaction id").is_err());
}

/// Real Sapling note encryption, with inert proof/signature bytes: enhancement
/// parses/decrypts transactions and deliberately does not validate consensus.
fn legacy_sapling_memo(w: &WebWallet, mined: u32, memo: &str) -> Vec<u8> {
    use sapling::{
        bundle::{Authorized as SaplingAuthorized, Bundle, OutputDescription},
        note_encryption::{sapling_note_encryption, SaplingDomain},
        value::{NoteValue, ValueCommitTrapdoor, ValueCommitment},
        Rseed,
    };
    use zcash_note_encryption::Domain;
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    use zcash_protocol::{consensus::BranchId, value::ZatBalance};

    let mut rng = rand::rng();
    let ufvk = w.decode_ufvk().unwrap();
    let to = ufvk.sapling().unwrap().default_address().1;
    let value = NoteValue::from_raw(50_000);
    let note = to.create_note(value, Rseed::BeforeZip212(jubjub::Fr::from(7u64)));
    let cmu = note.cmu();
    let cv = ValueCommitment::derive(value, ValueCommitTrapdoor::random(&mut rng));
    let mut memo_bytes = [0; 512];
    memo_bytes[..memo.len()].copy_from_slice(memo.as_bytes());
    let enc = sapling_note_encryption(None, note, memo_bytes, &mut rng);
    let output = OutputDescription::from_parts(
        cv,
        cmu,
        SaplingDomain::epk_bytes(enc.epk()),
        enc.encrypt_note_plaintext(),
        [0; 80],
        [0; 192],
    );
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V4,
        BranchId::for_height(&w.network(), BlockHeight::from_u32(mined)),
        0,
        BlockHeight::from_u32(mined + 20),
        None,
        None,
        Bundle::from_parts(
            vec![],
            vec![output],
            ZatBalance::zero(),
            SaplingAuthorized {
                binding_sig: [0; 64].into(),
            },
        ),
        None,
    )
    .freeze()
    .unwrap();
    let mut raw = Vec::new();
    tx.write(&mut raw).unwrap();
    raw
}

#[test]
fn historical_sapling_memos_use_mined_height_across_zip212_boundary() {
    use zcash_protocol::consensus::{BranchId, NetworkUpgrade, Parameters, ZIP212_GRACE_PERIOD};
    let mut w = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
        1,
    )
    .unwrap();
    let canopy = u32::from(
        w.network()
            .activation_height(NetworkUpgrade::Canopy)
            .unwrap(),
    );
    w.scanned_height = canopy + ZIP212_GRACE_PERIOD + 100;
    for mined in [canopy - 1, canopy + ZIP212_GRACE_PERIOD - 1] {
        let memo = format!("historical Sapling at {mined}");
        let raw = legacy_sapling_memo(&w, mined, &memo);
        let parsed = Transaction::read(
            raw.as_slice(),
            BranchId::for_height(&w.network(), BlockHeight::from_u32(mined)),
        )
        .unwrap();
        let txid = to_hex(parsed.txid().as_ref());
        let keys = std::collections::HashMap::from([(0u32, w.decode_ufvk().unwrap())]);
        assert!(
            zcash_client_backend::decrypt_transaction(
                &w.network(),
                None,
                Some(BlockHeight::from_u32(w.scanned_height())),
                &parsed,
                &keys,
            )
            .sapling_outputs()
            .is_empty(),
            "the former tip-based context cannot decrypt this fixture"
        );
        w.txs.insert(
            txid.clone(),
            TxAgg {
                mined_height: Some(mined),
                received_notes: 1,
                sapling_received: 50_000,
                ..Default::default()
            },
        );
        assert_eq!(w.enhance_raw_tx(&raw).unwrap(), 1);
        assert_eq!(w.txs[&txid].memos, vec![memo]);
        assert!(w.txs[&txid].enhancement_complete);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(!restored
            .memo_enhancement_txids(40)
            .contains(&parsed.txid().to_string()));
    }
}

#[test]
fn unmined_sapling_memo_uses_next_block_height_at_zip212_boundary() {
    use zcash_protocol::consensus::{NetworkUpgrade, Parameters, ZIP212_GRACE_PERIOD};
    let mut w = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
        1,
    )
    .unwrap();
    let end = u32::from(
        w.network()
            .activation_height(NetworkUpgrade::Canopy)
            .unwrap(),
    ) + ZIP212_GRACE_PERIOD;
    let raw = legacy_sapling_memo(&w, end - 1, "pending before ZIP212 enforcement");
    // No mined record: tip+1 still accepts legacy plaintext on the final grace block.
    w.scanned_height = end - 2;
    assert_eq!(w.enhance_raw_tx(&raw).unwrap(), 1);
    assert!(w.txs.values().all(|agg| agg.mined_height.is_none()));
    w.txs.clear();
    // At the very next tip the same legacy plaintext is no longer accepted.
    w.scanned_height = end - 1;
    assert_eq!(w.enhance_raw_tx(&raw).unwrap(), 0);
    assert!(w.txs.is_empty());
}

#[test]
fn review_zero_output_enhancement_remains_retryable_then_blank_memo_completes() {
    use zcash_protocol::consensus::{BranchId, NetworkUpgrade, Parameters, ZIP212_GRACE_PERIOD};
    let mut w = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
        1,
    )
    .unwrap();
    let canopy = u32::from(
        w.network()
            .activation_height(NetworkUpgrade::Canopy)
            .unwrap(),
    );
    let mined = canopy - 1;
    let raw = legacy_sapling_memo(&w, mined, "");
    let tx = Transaction::read(
        raw.as_slice(),
        BranchId::for_height(&w.network(), mined.into()),
    )
    .unwrap();
    let key = to_hex(tx.txid().as_ref());
    w.scanned_height = canopy + ZIP212_GRACE_PERIOD + 100;
    // A tracked row does not prove the full transaction actually decrypted.
    // Force the real ZIP212 zero-output case, then correct the context and retry.
    w.txs.insert(
        key.clone(),
        TxAgg {
            mined_height: Some(w.scanned_height),
            received_notes: 1,
            ..Default::default()
        },
    );
    assert_eq!(w.enhance_raw_tx(&raw).unwrap(), 0);
    assert!(!w.txs[&key].enhancement_complete);
    assert_eq!(w.txs[&key].memo_recovered, None);
    assert_eq!(w.memo_enhancement_txids(40), vec![tx.txid().to_string()]);
    w.txs.get_mut(&key).unwrap().mined_height = Some(mined);
    assert_eq!(w.enhance_raw_tx(&raw).unwrap(), 0);
    assert!(
        w.txs[&key].enhancement_complete,
        "a successfully decrypted blank memo is complete"
    );
    assert!(w.memo_enhancement_txids(40).is_empty());
    assert_eq!(w.txs[&key].memo_recovered, Some(true));
    let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert_eq!(restored.txs[&key].memo_recovered, Some(true));
}

#[test]
fn mined_spend_without_recoverable_outgoing_memo_finishes_enhancement() {
    use zcash_protocol::consensus::{BranchId, NetworkUpgrade, Parameters};
    let mut sender = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
        1,
    )
    .unwrap();
    let recipient = WebWallet::from_account(
        account_from_mnemonic(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            Network::Mainnet,
            0,
        )
        .unwrap(),
        1,
    )
    .unwrap();
    let mined = u32::from(
        sender
            .network()
            .activation_height(NetworkUpgrade::Canopy)
            .unwrap(),
    ) - 1;
    // This helper encrypts to the recipient with no outgoing viewing key.
    let raw = legacy_sapling_memo(&recipient, mined, "recipient-only memo");
    let tx = Transaction::read(
        raw.as_slice(),
        BranchId::for_height(&sender.network(), mined.into()),
    )
    .unwrap();
    let key = to_hex(tx.txid().as_ref());
    sender.scanned_height = mined;
    assert_eq!(sender.enhance_raw_tx(&raw).unwrap(), 0);
    assert!(
        sender.txs.is_empty(),
        "unrelated raw data must not create history"
    );
    sender.txs.insert(
        key.clone(),
        TxAgg {
            mined_height: None,
            spent_notes: 1,
            spent_zat: 60_000,
            sapling_spent: 60_000,
            ..Default::default()
        },
    );
    assert_eq!(sender.memo_enhancement_txids(500).len(), 1);
    assert_eq!(sender.enhance_raw_tx(&raw).unwrap(), 0);
    assert_eq!(
        sender.memo_enhancement_txids(500).len(),
        1,
        "unmined zero-output results stay retryable"
    );
    sender.txs.get_mut(&key).unwrap().mined_height = Some(mined);
    // Constructed sends can suppress aggregate receipt counts. A compact-
    // observed zero-value change note must still require full decryption.
    let mut incomplete = WebWallet::from_snapshot(&sender.to_snapshot().unwrap()).unwrap();
    incomplete.notes_mut().push(TrackedNote {
        pool: "sapling".into(),
        txid: key.clone(),
        output_index: 0,
        value_zat: 0,
        nf: "00".repeat(32),
        is_change: true,
        spent: false,
        spent_in: None,
        spent_height: None,
        mined_height: mined,
    });
    assert_eq!(incomplete.enhance_raw_tx(&raw).unwrap(), 0);
    assert_eq!(incomplete.memo_enhancement_txids(500).len(), 1);
    assert_eq!(sender.enhance_raw_tx(&raw).unwrap(), 0);
    assert!(sender.txs[&key].memos.is_empty());
    assert!(sender.memo_enhancement_txids(500).is_empty());
    let restored = WebWallet::from_snapshot(&sender.to_snapshot().unwrap()).unwrap();
    assert!(restored.memo_enhancement_txids(500).is_empty());
    assert_eq!(restored.txs[&key].memo_recovered, Some(false));
}

#[test]
fn review_coalescing_unions_checkpoint_and_marked_retention() {
    for (first, second) in [(3, 1), (1, 2), (2, 1), (3, 2)] {
        let mut values = vec![leaf(0), leaf(0)];
        values[0].kind = first;
        values[1].kind = second;
        coalesce_hashed_leaves(&mut values, "orchard").unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].kind, 3, "retention {first} plus {second}");
    }
}

#[test]
fn review_mainnet_three_confirmation_anchors_survive_note_free_catchup() {
    use zcash_client_backend::data_api::WalletRead;
    use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
    for pool in ["sapling", "orchard", "ironwood"] {
        let start = u32::from(
            Network::Mainnet
                .activation_height(NetworkUpgrade::Nu5)
                .unwrap(),
        );
        let mut w = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
            start,
        )
        .unwrap();
        // Exercise the deferred output path, including empty-run calls that
        // retain metadata without producing commitment leaves.
        w.offload = Some(Offload::new(0, 0, 0, OffloadOutput::EncodedLeaves));
        append_pool(&mut w, pool, start, 1, true);
        record_test_height(&mut w, start);
        let blocks = (start + 1..=start + 200).map(|height| {
            empty_block(height.into(), empty_block((height - 1).into(), vec![]).hash)
        });
        w.apply_compact_blocks_blob(&super::super::encode_delimited(blocks))
            .unwrap();
        w.finalize_scan_trees().unwrap();
        let (_, anchor) = w
            .get_target_and_anchor_heights(crate::confirmations_policy(Network::Mainnet).trusted())
            .unwrap()
            .unwrap();
        assert_eq!(
            u32::from(anchor),
            start + 198,
            "{pool} must use the retained confirmation window"
        );
        let check = |tree: &OrchardTree| {
            assert!(tree.store().get_checkpoint(&anchor).unwrap().is_some());
            assert!(tree
                .witness_at_checkpoint_id(Position::from(0), &anchor)
                .unwrap()
                .is_some());
        };
        match pool {
            "sapling" => {
                assert!(w
                    .sapling_tree
                    .store()
                    .get_checkpoint(&anchor)
                    .unwrap()
                    .is_some());
                assert!(w
                    .sapling_tree
                    .witness_at_checkpoint_id(Position::from(0), &anchor)
                    .unwrap()
                    .is_some());
            }
            "orchard" => check(&w.orchard_tree),
            _ => check(&w.ironwood_tree),
        }
    }
}

#[cfg(feature = "native")]
fn ironwood_memo_transaction(sender: &WebWallet, recipient: &WebWallet, memo: &str) -> Vec<u8> {
    shielded_memo_transaction(sender, recipient, memo, 50_000, true)
}

#[cfg(feature = "native")]
fn shielded_memo_transaction(
    sender: &WebWallet,
    recipient: &WebWallet,
    memo: &str,
    value: u64,
    ironwood: bool,
) -> Vec<u8> {
    use orchard::{
        builder::{Builder, BundleType},
        bundle::{Authorized as OrchardAuthorized, BundleVersion},
        Anchor, Proof,
    };
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    use zcash_protocol::{consensus::BranchId, value::ZatBalance};

    let version = if ironwood {
        BundleVersion::ironwood_v3()
    } else {
        BundleVersion::orchard_v2()
    };
    let mut builder = Builder::new(
        BundleType::Transactional {
            bundle_required: false,
            pad_to_minimum: None,
        },
        version,
        version.default_flags(),
        Anchor::empty_tree(),
    )
    .unwrap();
    let mut memo_bytes = [0; 512];
    memo_bytes[..memo.len()].copy_from_slice(memo.as_bytes());
    builder
        .add_output(
            Some(
                sender
                    .decode_ufvk()
                    .unwrap()
                    .orchard()
                    .unwrap()
                    .to_ovk(Scope::External),
            ),
            recipient
                .decode_ufvk()
                .unwrap()
                .orchard()
                .unwrap()
                .address_at(0u32, Scope::External),
            orchard::value::NoteValue::from_raw(value),
            memo_bytes,
        )
        .unwrap();
    let (bundle, _) = builder.build::<ZatBalance>(rand::rng()).unwrap().unwrap();
    let proof_size = Proof::expected_proof_size(bundle.actions().len());
    // Decryption fixture: real action encryption/OVK recovery, inert signatures
    // and canonical-size proof bytes; no consensus verification is performed.
    let bundle = bundle.map_authorization(
        &mut (),
        |_, _, _| [0; 64].into(),
        |_, _| OrchardAuthorized::from_parts(Proof::new(vec![0; proof_size]), [0; 64].into()),
    );
    let tx = if ironwood {
        TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            BlockHeight::from_u32(1_000_020),
            None,
            None,
            None,
            Some(bundle),
        )
    } else {
        TransactionData::<Authorized>::from_parts(
            TxVersion::V5,
            BranchId::for_height(&recipient.network(), recipient.next_height().into()),
            0,
            BlockHeight::from_u32(recipient.next_height() + 20),
            None,
            None,
            None,
            Some(bundle),
        )
    }
    .freeze()
    .unwrap();
    let mut raw = Vec::new();
    tx.write(&mut raw).unwrap();
    raw
}

#[cfg(feature = "native")]
#[test]
fn review_max_send_uses_confirmed_notes_despite_a_large_recent_receipt() {
    use zcash_client_backend::data_api::WalletRead;
    use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
    let start = u32::from(
        Network::Mainnet
            .activation_height(NetworkUpgrade::Nu5)
            .unwrap(),
    );
    let mut w = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
        start,
    )
    .unwrap();
    w.apply_compact_block(&empty_block(start.into(), vec![0; 32]).encode_to_vec())
        .unwrap();
    let raw = shielded_memo_transaction(&w, &w, "mature", 50_000, false);
    w.apply_mined_raw_tx(&raw, 1).unwrap();
    w.enhance_raw_tx(&raw).unwrap();
    let original_txid = w.spend_notes[0].txid.clone();
    for _ in 0..3 {
        let block = empty_block(
            w.next_height().into(),
            from_hex(&w.prior.as_ref().unwrap().hash).unwrap(),
        );
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
    }
    let raw = shielded_memo_transaction(&w, &w, "recent", 5_000_000, false);
    w.apply_mined_raw_tx(&raw, 1).unwrap();
    let (amount, fee) = w.max_send(None).unwrap();
    assert!(
        amount > 0,
        "mature funds remain spendable while the larger receipt confirms"
    );
    assert_eq!(amount + fee, 50_000);
    let policy = crate::confirmations_policy(Network::Mainnet);
    assert_eq!(u32::from(policy.trusted()), 3);
    let (_, anchor) = w
        .get_target_and_anchor_heights(policy.trusted())
        .unwrap()
        .unwrap();
    assert_eq!(u32::from(anchor), w.scanned_height - 2);
    let note = &w.spend_notes[0];
    let path = w
        .orchard_tree
        .witness_at_checkpoint_id(Position::from(note.position), &anchor)
        .unwrap()
        .unwrap();
    let hash = MerkleHashOrchard::from_cmx(
        &WebWallet::decode_spend_note(note)
            .unwrap()
            .commitment()
            .into(),
    );
    assert_eq!(
        Some(path.root(hash)),
        w.orchard_tree.root_at_checkpoint_id(&anchor).unwrap()
    );
    let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert_eq!(restored.max_send(None).unwrap(), (amount, fee));
    restored.rewind_to_height(start + 1).unwrap();
    assert!(restored.txs[&original_txid].enhancement_complete,
        "a retained transaction has the same memo ciphertext; rewind must not undo verified completion");
    assert_eq!(
        restored.txs.len(),
        1,
        "the later receipt is removed by rewind"
    );
}

#[test]
fn review_zero_expiry_history_remains_pending() {
    let mut w = wallet();
    w.scanned_height = 100;
    w.txs.insert(
        "01".repeat(32),
        TxAgg {
            expiry_height: Some(0),
            ..Default::default()
        },
    );
    let row = w.history(1).remove(0);
    assert!(!row.expired_unmined, "zero expiry disables expiration");
}

#[test]
fn review_empty_height_boundary_survives_a_later_full_shard_drop() {
    let mut w = wallet();
    let root = MerkleHashOrchard::empty_root(ORCHARD_SHARD_HEIGHT.into()).to_bytes();
    w.apply_subtree_roots_json("orchard", &serde_json::json!({"roots": (0..3).map(|i|
        serde_json::json!({"completingHeight": 300 + i, "rootHash": to_hex(&root)})).collect::<Vec<_>>()}).to_string()).unwrap();
    w.prepare_scan_window(200);
    // The first shard is retained by the birthday policy; the next complete
    // unmarked shard is eligible for dropping unless its recent boundary is
    // protected. All commitments and seeded roots agree with the empty tree.
    let first =
        vec![(MerkleHashOrchard::empty_leaf(), Retention::Ephemeral); SHARD_SIZE as usize + 10];
    w.append_orchard(1, &first).unwrap();
    record_test_height(&mut w, 1);
    for height in 2..=198 {
        record_test_height(&mut w, height);
    }
    w.append_orchard(
        199,
        &vec![
            (MerkleHashOrchard::empty_leaf(), Retention::Ephemeral);
            SHARD_SIZE as usize - 10 + 1
        ],
    )
    .unwrap();
    record_test_height(&mut w, 199);
    assert!(
        w.orchard_leaves
            .iter()
            .any(|leaf| leaf.position == SHARD_SIZE + 9 && leaf.height == 1),
        "recent empty heights must protect authentic leaves without changing their mined height"
    );
    // Retention does not turn an unused pool into a live hashed tree.
    assert_eq!(w.live_pools, 0);
    let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert!(restored
        .orchard_leaves
        .iter()
        .any(|leaf| leaf.position == SHARD_SIZE + 9 && leaf.height == 1));
}

#[test]
fn review_sparse_recent_anchor_survives_later_append_reload_and_rewind() {
    use zcash_client_backend::data_api::WalletRead;
    use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
    let start = u32::from(
        Network::Mainnet
            .activation_height(NetworkUpgrade::Nu5)
            .unwrap(),
    );
    for (pool, bit) in [
        ("sapling", SAPLING_LIVE),
        ("orchard", ORCHARD_LIVE),
        ("ironwood", IRONWOOD_LIVE),
    ] {
        let mut w = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Mainnet, 0).unwrap(),
            start,
        )
        .unwrap();
        let root = if pool == "sapling" {
            sapling::Node::empty_root(SAPLING_SHARD_HEIGHT.into()).to_bytes()
        } else {
            MerkleHashOrchard::empty_root(ORCHARD_SHARD_HEIGHT.into()).to_bytes()
        };
        w.apply_subtree_roots_json(pool, &serde_json::json!({"roots": (0..3).map(|i|
            serde_json::json!({"completingHeight": start + 300 + i, "rootHash": to_hex(&root)})).collect::<Vec<_>>()}).to_string()).unwrap();
        match pool {
            "sapling" => w.sapling_next = SHARD_SIZE,
            "orchard" => w.orchard_next = SHARD_SIZE,
            _ => w.ironwood_next = SHARD_SIZE,
        }
        record_test_height(&mut w, start);
        w.prepare_scan_window(start + 200);
        append_pool(&mut w, pool, start + 1, 10, true);
        record_test_height(&mut w, start + 1);
        for height in start + 2..=start + 198 {
            record_test_height(&mut w, height);
        }
        let extra = vec![(MerkleHashOrchard::empty_leaf(), Retention::Ephemeral); 6];
        match pool {
            "sapling" => w.append_sapling(
                start + 199,
                &vec![(sapling::Node::empty_leaf(), Retention::Ephemeral); 6],
            ),
            "orchard" => w.append_orchard(start + 199, &extra),
            _ => w.append_ironwood(start + 199, &extra),
        }
        .unwrap();
        record_test_height(&mut w, start + 199);
        append_pool(&mut w, pool, start + 200, 1, true);
        record_test_height(&mut w, start + 200);
        w.finalize_scan_trees().unwrap();
        assert_eq!(w.live_pools, bit);
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        restored.finalize_scan_trees().unwrap();
        for current in [&w, &restored] {
            let (_, anchor) = current
                .get_target_and_anchor_heights(
                    crate::confirmations_policy(Network::Mainnet).trusted(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(u32::from(anchor), start + 198);
            let pos = if pool == "sapling" {
                current
                    .sapling_tree
                    .store()
                    .get_checkpoint(&anchor)
                    .unwrap()
                    .unwrap()
                    .position()
            } else {
                let tree = if pool == "orchard" {
                    &current.orchard_tree
                } else {
                    &current.ironwood_tree
                };
                assert!(tree
                    .witness_at_checkpoint_id(Position::from(SHARD_SIZE), &anchor)
                    .unwrap()
                    .is_some());
                tree.store()
                    .get_checkpoint(&anchor)
                    .unwrap()
                    .unwrap()
                    .position()
            };
            assert_eq!(pos, Some(Position::from(SHARD_SIZE + 9)));
        }
        assert_pool_witness(&mut w, &mut restored, pool, SHARD_SIZE, start + 198);
        restored.rewind_to_height(start + 198).unwrap();
        assert_eq!(restored.live_pools, bit);
        assert_pool_witness(&mut w, &mut restored, pool, SHARD_SIZE, start + 198);
    }
}

#[test]
fn review_anchor_requires_a_real_common_checkpoint_and_can_recover_after_legacy_reload() {
    use zcash_client_backend::data_api::WalletRead;
    let mut w = wallet();
    append_pool(&mut w, "orchard", 1, 1, true);
    record_test_height(&mut w, 1);
    record_test_height(&mut w, 200);
    w.tree_sizes.clear(); // A legacy snapshot cannot supply the missing empty-height metadata.
    w.finalize_scan_trees().unwrap();
    let three = std::num::NonZeroU32::new(3).unwrap();
    assert!(
        w.get_target_and_anchor_heights(three).unwrap().is_none(),
        "never fabricate a birthday anchor or use an obsolete checkpoint"
    );
    for height in 201..=202 {
        record_test_height(&mut w, height);
    }
    assert_eq!(
        w.get_target_and_anchor_heights(three).unwrap().unwrap().1,
        200.into()
    );
    w.prepare_received_pool("ironwood");
    append_pool(&mut w, "ironwood", 203, 1, true);
    record_test_height(&mut w, 203);
    w.finalize_scan_trees().unwrap();
    // Deliberately remove the Ironwood side: an Orchard checkpoint alone may
    // never advertise an anchor for a wallet whose other used pool lacks it.
    for height in 103..=203 {
        w.ironwood_tree
            .store_mut()
            .remove_checkpoint(&height.into())
            .unwrap();
    }
    assert!(w.get_target_and_anchor_heights(three).unwrap().is_none());
}

#[test]
#[ignore]
fn benchmark_recent_empty_height_checkpoints() {
    use std::time::Instant;
    for count in [1_000, 10_000] {
        for (label, checkpoint) in [
            ("prior metadata only", false),
            ("used-pool checkpoints", true),
        ] {
            let mut samples = Vec::new();
            for _ in 0..3 {
                let mut w = wallet();
                append_pool(&mut w, "orchard", 1, 1, true);
                record_test_height(&mut w, 1);
                w.finalize_scan_trees().unwrap();
                if !checkpoint {
                    w.live_pools = 0;
                }
                let started = Instant::now();
                for height in 2..=count + 1 {
                    record_test_height(&mut w, height);
                }
                samples.push(started.elapsed());
                if checkpoint {
                    assert!(w
                        .orchard_tree
                        .witness_at_checkpoint_id(Position::from(0), &(count - 1).into())
                        .unwrap()
                        .is_some());
                    assert_eq!(w.live_pools, ORCHARD_LIVE);
                    assert!(w.orchard_tree.store().checkpoint_count().unwrap() <= TREE_CHECKPOINTS);
                }
            }
            samples.sort();
            eprintln!(
                "recent empty heights {label}: {count} blocks {:.3}ms median",
                samples[1].as_secs_f64() * 1000.0
            );
        }
    }
}

#[cfg(feature = "native")]
#[test]
fn ironwood_memo_completion_preserves_incoming_outgoing_and_pending_pool() {
    use zcash_protocol::consensus::BranchId;
    let mut sender = wallet();
    let mut recipient = WebWallet::from_account(
        account_from_mnemonic(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            Network::Regtest,
            0,
        )
        .unwrap(),
        1,
    )
    .unwrap();
    sender.scanned_height = 1_000_001;
    recipient.scanned_height = 1_000_001;
    let raw = ironwood_memo_transaction(&sender, &recipient, "Ironwood pool memo");
    let parsed = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
    let txid = to_hex(parsed.txid().as_ref());
    let keys = std::collections::HashMap::from([(0u32, recipient.decode_ufvk().unwrap())]);
    let decrypted = zcash_client_backend::decrypt_transaction(
        &recipient.network(),
        None,
        Some(BlockHeight::from_u32(1_000_001)),
        &parsed,
        &keys,
    );
    assert!(decrypted.orchard_outputs().is_empty());
    assert_eq!(decrypted.ironwood_outputs().len(), 1);
    assert_eq!(recipient.apply_raw_tx(&raw).unwrap(), 1);
    assert_eq!(recipient.enhance_raw_tx(&raw).unwrap(), 1);
    assert!(recipient.memo_enhancement_txids(40).is_empty());
    let balance = recipient.balance();
    assert_eq!(balance.ironwood_pending, 50_000);
    assert_eq!(balance.orchard_pending, 0);
    assert_eq!(balance.sapling_pending, 0);
    assert_eq!(recipient.history(1)[0].ironwood_received, 50_000);
    assert_eq!(recipient.history(1)[0].memos, vec!["Ironwood pool memo"]);
    // Repeated enhancement and hydration must neither duplicate value nor move pools.
    recipient.apply_raw_tx(&raw).unwrap();
    let restored = WebWallet::from_snapshot(&recipient.to_snapshot().unwrap()).unwrap();
    assert_eq!(restored.balance().ironwood_pending, 50_000);
    assert_eq!(restored.balance().orchard_pending, 0);
    assert!(restored.memo_enhancement_txids(40).is_empty());

    sender.txs.insert(
        txid.clone(),
        TxAgg {
            mined_height: Some(1_000_001),
            spent_notes: 1,
            spent_zat: 60_000,
            ironwood_spent: 60_000,
            ..Default::default()
        },
    );
    assert_eq!(
        sender.enhance_raw_tx(&raw).unwrap(),
        1,
        "outgoing OVK recovery"
    );
    assert_eq!(sender.txs[&txid].memos, vec!["Ironwood pool memo"]);
    assert_eq!(sender.txs[&txid].ironwood_spent, 60_000);
    assert_eq!(sender.txs[&txid].orchard_received, 0);
    assert!(sender.txs[&txid].enhancement_complete);

    let blank = ironwood_memo_transaction(&sender, &recipient, "");
    assert_eq!(recipient.apply_raw_tx(&blank).unwrap(), 1);
    assert_eq!(recipient.balance().ironwood_pending, 100_000);
    assert!(
        recipient.memo_enhancement_txids(40).is_empty(),
        "valid blank Ironwood memo is complete"
    );
    let blank_id = to_hex(
        Transaction::read(blank.as_slice(), BranchId::Nu6_3)
            .unwrap()
            .txid()
            .as_ref(),
    );
    sender.txs.insert(
        blank_id.clone(),
        TxAgg {
            mined_height: Some(1_000_001),
            spent_notes: 1,
            ..Default::default()
        },
    );
    assert_eq!(sender.enhance_raw_tx(&blank).unwrap(), 0);
    assert!(
        sender.txs[&blank_id].enhancement_complete,
        "successful outgoing blank recovery counts too"
    );
}

#[cfg(feature = "transparent-inputs")]
#[test]
fn first_receipt_after_note_free_finalize_can_spend() {
    let recipient_seed =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let mut source = wallet();
    let mut recipient = WebWallet::from_account(
        account_from_mnemonic(recipient_seed, Network::Regtest, 0).unwrap(),
        1,
    )
    .unwrap();
    for w in [&mut source, &mut recipient] {
        let b1 = empty_block(1, vec![0; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
            .unwrap();
    }
    source.apply_utxos_json(
        r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    ).unwrap();
    let unrelated = source.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    let parse = |raw: &[u8]| {
        Transaction::read(
            raw,
            zcash_protocol::consensus::BranchId::for_height(&Network::Regtest, 3.into()),
        )
        .unwrap()
    };
    // Bridge UTXOs remain wire-ordered; public display changes must never
    // reverse a transparent outpoint used for signing.
    let shield = parse(&unrelated);
    assert_eq!(
        to_hex(shield.transparent_bundle().unwrap().vin[0].prevout().hash()),
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    );
    source.apply_mined_raw_tx(&unrelated, 1).unwrap();
    recipient.apply_mined_raw_tx(&unrelated, 1).unwrap();
    assert_eq!(recipient.balance().orchard_available, 0);
    assert!(recipient.orchard_leaves.len() >= 2);
    recipient.finalize_scan_trees().unwrap();
    assert!(recipient.sinsemilla_live());
    assert_eq!(recipient.live_pools, 0);

    // The first sync omitted this unused pool's hashes. The next sync must
    // rebuild its retained siblings once a real receipt needs a witness.
    let payment = source
        .prove_send(
            REGTEST_FAUCET_MNEMONIC,
            recipient.unified_address(),
            "0.0005",
            Some("canonical memo"),
        )
        .unwrap();
    let payment_tx = parse(&payment);
    let canonical = payment_tx.txid().to_string();
    let wire = to_hex(payment_tx.txid().as_ref());
    assert_ne!(
        canonical, wire,
        "fixture must expose the byte-order difference"
    );
    assert!(source.memo_enhancement_txids(40).contains(&canonical));
    let pending = source.history_matching(1, None, Some(&canonical.to_uppercase()));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].txid, canonical);
    assert_eq!(pending[0].mined_height, None);

    let mut abandoned = WebWallet::from_snapshot(&source.to_snapshot().unwrap()).unwrap();
    abandoned.pending_txs = source.pending_txs.clone();
    assert!(abandoned
        .abandon_unmined(&canonical.to_uppercase())
        .unwrap());
    assert!(!abandoned.txs.contains_key(&wire));
    assert!(!abandoned.pending_txs.contains_key(&payment_tx.txid()));
    assert!(abandoned
        .notes
        .iter()
        .all(|n| n.spent_in.as_deref() != Some(&wire)));
    assert!(abandoned
        .spend_notes
        .iter()
        .all(|n| n.spent_in.as_deref() != Some(&wire)));

    let mut compact_only = WebWallet::from_snapshot(&recipient.to_snapshot().unwrap()).unwrap();
    compact_only.apply_mined_raw_tx(&payment, 2).unwrap();
    assert_eq!(
        compact_only.memo_enhancement_txids(40),
        vec![canonical.clone()]
    );
    compact_only.enhance_raw_tx(&payment).unwrap();
    assert!(compact_only.memo_enhancement_txids(40).is_empty());
    assert_eq!(compact_only.history(1)[0].txid, canonical);
    assert_eq!(compact_only.history(1)[0].memos, vec!["canonical memo"]);

    recipient.apply_raw_tx(&payment).unwrap();
    assert_eq!(recipient.history(1)[0].txid, canonical);
    assert_eq!(recipient.history(1)[0].mined_height, None);
    source.apply_mined_raw_tx(&payment, 2).unwrap();
    recipient.apply_mined_raw_tx(&payment, 2).unwrap();
    let mined = recipient.history_matching(5, None, Some(&canonical.to_uppercase()));
    assert_eq!(
        mined.len(),
        1,
        "mempool and mined receipt must share one internal key"
    );
    assert_eq!(mined[0].mined_height, Some(4));
    assert!(recipient.abandon_unmined(&canonical).is_err());
    assert_eq!(recipient.balance().orchard_available, 50_000);
    recipient.finalize_scan_trees().unwrap();
    assert_eq!(recipient.live_pools, ORCHARD_LIVE);
    let saved = recipient.to_snapshot().unwrap();
    let legacy: Snapshot = serde_json::from_slice(&saved).unwrap();
    assert!(legacy.txs.contains_key(&wire));
    assert!(!legacy.txs.contains_key(&canonical));
    assert!(legacy.notes.iter().all(|n| n.txid == wire));
    assert!(legacy.spend_notes.iter().all(|n| n.txid == wire));
    let restored = WebWallet::from_snapshot(&saved).unwrap();
    assert_eq!(restored.history(1)[0].txid, canonical);
    assert_eq!(
        restored.to_snapshot().unwrap(),
        saved,
        "legacy snapshot keys and references stay byte-identical"
    );
    let returned = recipient
        .prove_send(recipient_seed, source.unified_address(), "0.0002", None)
        .unwrap();
    let tx = Transaction::read(
        returned.as_slice(),
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(recipient.scanned_height + 1),
        ),
    )
    .unwrap();
    let vk = orchard::circuit::VerifyingKey::build(
        orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2,
    );
    tx.orchard_bundle().unwrap().verify_proof(&vk).unwrap();
    assert!(recipient
        .history(10)
        .iter()
        .any(|row| row.txid == tx.txid().to_string() && row.mined_height.is_none()));
    recipient.apply_mined_raw_tx(&returned, 3).unwrap();
    source.apply_mined_raw_tx(&returned, 3).unwrap();
    assert!(recipient
        .history(10)
        .iter()
        .any(|row| row.txid == tx.txid().to_string()
            && row.spent_zat >= 50_000
            && row.mined_height == Some(5)));

    // Reload must retain the same witness inputs, independently of live trees.
    let mut reloaded = WebWallet::from_snapshot(&saved).unwrap();
    assert!(reloaded
        .prove_send(recipient_seed, source.unified_address(), "0.0002", None,)
        .is_ok());
}

/// A near-tip sync has no finalize after scanning. Mining a self-send through
/// the ordinary compact-block path must count its payment output as received,
/// the same as a reload does, rather than reporting the whole payment as sent.
#[cfg(feature = "transparent-inputs")]
#[test]
fn mined_self_send_history_matches_reload_without_finalize() {
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(
        r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    ).unwrap();
    let shield = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    w.apply_mined_raw_tx(&shield, 1).unwrap();
    w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
    w.finalize_scan_trees().unwrap();
    assert!(w.sinsemilla_live());

    let own = w.unified_address().to_string();
    let raw = w
        .prove_send(REGTEST_FAUCET_MNEMONIC, &own, "0.002", Some("self"))
        .unwrap();
    let block = w.mined_raw_tx_block(&raw, 2).unwrap();
    let txid = Transaction::read(
        raw.as_slice(),
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(u32::try_from(block.height).unwrap()),
        ),
    )
    .unwrap()
    .txid()
    .to_string();
    w.apply_compact_block(&block.encode_to_vec()).unwrap();

    let live = w.history_matching(1, None, Some(&txid)).remove(0);
    let fee = live.fee_zat.expect("constructed send records its fee") as i64;
    assert!(live.mined_height.is_some());
    assert_eq!(live.account_delta_zat, -fee, "self-send costs only its fee");
    let reloaded = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    let saved = reloaded.history_matching(1, None, Some(&txid)).remove(0);
    assert_eq!(live.account_delta_zat, saved.account_delta_zat);
    assert_eq!(
        live.orchard_received + live.ironwood_received,
        saved.orchard_received + saved.ironwood_received
    );
}

/// A shield's history keeps exactly the transparent input it spent: pending
/// recomputes must not add it again, and a UTXO refresh after mining (which
/// no longer lists the spent output) must not erase it.
#[cfg(feature = "transparent-inputs")]
#[test]
fn shield_history_keeps_its_transparent_input_through_refresh_and_mining() {
    const UTXOS: &str = r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#;
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(UTXOS).unwrap();
    let shield = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    let txid = Transaction::read(
        shield.as_slice(),
        zcash_protocol::consensus::BranchId::for_height(&Network::Regtest, 3.into()),
    )
    .unwrap()
    .txid()
    .to_string();
    let spent = |w: &WebWallet| {
        let row = w.history_matching(1, None, Some(&txid));
        (row[0].transparent_spent, row[0].is_shielding)
    };
    for _ in 0..3 {
        // The server still lists the output until the shield is mined.
        w.apply_utxos_json(UTXOS).unwrap();
        w.recompute_pool_fields();
        assert_eq!(spent(&w), (625_000_000, true), "pending shield");
    }
    w.apply_mined_raw_tx(&shield, 1).unwrap();
    w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
    assert_eq!(spent(&w), (625_000_000, true), "mined shield");
    assert_eq!(w.balance().transparent_available, 0);
}

/// A send that is never mined must not lock its inputs forever: once the
/// scanned chain passes its expiry height they return, as in SQLite wallets.
#[cfg(feature = "transparent-inputs")]
#[test]
fn inputs_of_an_expired_unmined_send_become_spendable_again() {
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(
        r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    )
    .unwrap();
    let shield = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    w.apply_mined_raw_tx(&shield, 1).unwrap();
    let funded = w.balance().orchard_available;
    assert!(funded > 0);

    let lost = w
        .prove_send(
            REGTEST_FAUCET_MNEMONIC,
            w.unified_address().to_string().as_str(),
            "0.1",
            None,
        )
        .unwrap();
    let tx = Transaction::read(
        lost.as_slice(),
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(w.scanned_height + 1),
        ),
    )
    .unwrap();
    let expiry = u32::from(tx.expiry_height());
    assert!(expiry > w.scanned_height);
    assert_eq!(w.balance().orchard_available, 0, "the input is reserved");

    let extend = |w: &mut WebWallet, to: u32| {
        let mut prev =
            super::super::from_hex(w.block_hashes.values().next_back().unwrap()).unwrap();
        let blocks: Vec<_> = (w.next_height()..=to)
            .map(|h| {
                let block = empty_block(h.into(), prev.clone());
                prev = block.hash.clone();
                block
            })
            .collect();
        w.apply_compact_blocks_blob(&super::super::encode_delimited(blocks))
            .unwrap();
    };
    // Still mineable at its expiry height: keep the reservation.
    extend(&mut w, expiry - 1);
    assert_eq!(w.balance().orchard_available, 0);
    extend(&mut w, expiry);
    let released = w.balance();
    assert_eq!(released.orchard_available, funded);
    assert_eq!(released.total_pending, 0, "expired change is not pending");
    let canonical = tx.txid().to_string();
    assert_eq!(w.history_matching(1, None, Some(&canonical)).len(), 1);
    // The same funds can be spent again.
    assert!(w
        .prove_send(
            REGTEST_FAUCET_MNEMONIC,
            w.unified_address().to_string().as_str(),
            "0.1",
            None
        )
        .is_ok());
}

/// Regtest activates NU6.3 at 1,000,000, where payments to Orchard receivers
/// become Ironwood outputs. An Orchard-funded wallet must still send, and the
/// recipient must be able to spend the Ironwood note it receives.
#[cfg(feature = "transparent-inputs")]
#[test]
fn post_nu6_3_payments_use_ironwood_and_ironwood_notes_can_be_spent() {
    use zcash_protocol::consensus::{BranchId, NetworkUpgrade, Parameters};
    let nu6_3 = u32::from(
        Network::Regtest
            .activation_height(NetworkUpgrade::Nu6_3)
            .expect("regtest NU6.3 height"),
    );
    let start = nu6_3 - 8;
    let recipient_seed =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let mut source = WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
        start,
    )
    .unwrap();
    let mut recipient = WebWallet::from_account(
        account_from_mnemonic(recipient_seed, Network::Regtest, 0).unwrap(),
        start,
    )
    .unwrap();
    // Empty blocks from the wallet's next height, linked to its last hash.
    let extend = |w: &mut WebWallet, to: u32| {
        let mut prev = w
            .block_hashes
            .values()
            .next_back()
            .map(|hash| super::super::from_hex(hash).unwrap())
            .unwrap_or_else(|| vec![0; 32]);
        let blocks: Vec<_> = (w.next_height()..=to)
            .map(|h| {
                let block = empty_block(h.into(), prev.clone());
                prev = block.hash.clone();
                block
            })
            .collect();
        w.apply_compact_blocks_blob(&super::super::encode_delimited(blocks))
            .unwrap();
    };
    for w in [&mut source, &mut recipient] {
        extend(w, start + 1);
    }
    source
        .apply_utxos_json(&format!(
            r#"{{"utxos":[{{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":{start},"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}}]}}"#
        ))
        .unwrap();
    // Funded before activation: an Orchard note and no Ironwood activity.
    let shield = source.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    source.apply_mined_raw_tx(&shield, 1).unwrap();
    recipient.apply_mined_raw_tx(&shield, 1).unwrap();
    assert!(source.balance().orchard_available > 0);
    assert_eq!(source.balance().ironwood_available, 0);
    for w in [&mut source, &mut recipient] {
        extend(w, nu6_3 + 1);
    }
    // Someone else's Ironwood activity: the pool the payment will use is now
    // nonempty but holds no note of either wallet, as on mainnet.
    for w in [&mut source, &mut recipient] {
        let height = w.next_height();
        let mut block = empty_block(
            height.into(),
            super::super::from_hex(w.block_hashes.values().next_back().unwrap()).unwrap(),
        );
        block.chain_metadata = None;
        block
            .vtx
            .push(zcash_client_backend::proto::compact_formats::CompactTx {
                index: 1,
                txid: vec![0xf0; 32],
                ironwood_actions: (1..=3u8)
                    .map(
                        |i| zcash_client_backend::proto::compact_formats::CompactOrchardAction {
                            nullifier: vec![i; 32],
                            cmx: vec![i; 32],
                            ephemeral_key: vec![0; 32],
                            ciphertext: vec![0; 52],
                        },
                    )
                    .collect(),
                ..Default::default()
            });
        w.apply_compact_blocks_blob(&super::super::encode_delimited([block]))
            .unwrap();
        extend(w, height + 2);
        assert!(w.ironwood_next > 0 && w.balance().ironwood_available == 0);
    }

    let verify = |raw: &[u8], height: u32| {
        let tx = Transaction::read(
            raw,
            BranchId::for_height(&Network::Regtest, BlockHeight::from_u32(height)),
        )
        .unwrap();
        for bundle in [tx.orchard_bundle(), tx.ironwood_bundle()]
            .into_iter()
            .flatten()
        {
            let vk =
                orchard::circuit::VerifyingKey::build(bundle.bundle_version().circuit_version());
            bundle.verify_proof(&vk).unwrap();
        }
        tx
    };
    // The payment is an Ironwood output even though every input is Orchard.
    let payment = source
        .prove_send(
            REGTEST_FAUCET_MNEMONIC,
            recipient.unified_address(),
            "0.0005",
            Some("after nu6.3"),
        )
        .expect("an Orchard-funded wallet can pay after NU6.3");
    let paid = verify(&payment, source.scanned_height + 1);
    assert!(paid.ironwood_bundle().is_some());
    source.apply_mined_raw_tx(&payment, 1).unwrap();
    recipient.apply_mined_raw_tx(&payment, 1).unwrap();
    assert_eq!(recipient.balance().ironwood_available, 50_000);

    // The received Ironwood note funds a send of its own.
    let returned = recipient
        .prove_send(recipient_seed, source.unified_address(), "0.0002", None)
        .expect("an Ironwood note is spendable");
    let spent = verify(&returned, recipient.scanned_height + 1);
    assert!(spent.ironwood_bundle().is_some());
    recipient.apply_mined_raw_tx(&returned, 1).unwrap();
    // One Ironwood bundle of two actions: a 10,000-zatoshi ZIP-317 fee, and
    // change that stays in Ironwood.
    let after = recipient.balance();
    assert_eq!(after.orchard_available + after.orchard_pending, 0);
    assert_eq!(after.ironwood_available + after.ironwood_pending, 20_000);
}

#[cfg(feature = "transparent-inputs")]
#[test]
fn future_subtree_roots_cannot_move_scanned_anchor() {
    use zcash_client_backend::data_api::WalletRead;
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(
        r#"{"utxos":[{"txid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    ).unwrap();
    let shielded = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    w.apply_mined_raw_tx(&shielded, 1).unwrap();
    w.finalize_scan_trees().unwrap();
    let actual_size = w.orchard_next;
    let expected_root = w
        .orchard_tree
        .root(OrchardTree::root_addr(), Position::from(actual_size))
        .unwrap();
    let first_root = w
        .orchard_tree
        .root(
            MerkleAddress::from_parts(ORCHARD_SHARD_HEIGHT.into(), 0),
            Position::from(actual_size),
        )
        .unwrap();
    w.apply_subtree_roots_json(
        "orchard",
        &serde_json::json!({"roots": [
            {"completingHeight": 100_000, "rootHash": to_hex(&first_root.to_bytes())},
            {"completingHeight": 200_000, "rootHash": to_hex(&MerkleHashOrchard::empty_root(ORCHARD_SHARD_HEIGHT.into()).to_bytes())},
        ]}).to_string(),
    ).unwrap();
    let mut empty = empty_block(
        u64::from(w.scanned_height + 1),
        from_hex(&w.prior.as_ref().unwrap().hash).unwrap(),
    );
    empty
        .chain_metadata
        .as_mut()
        .unwrap()
        .orchard_commitment_tree_size = actual_size as u32;
    w.apply_compact_blocks_blob(&super::super::encode_one(&empty))
        .unwrap();
    // Force the snapshot reload path, where all downloaded roots are seeded
    // before retained note-bearing leaves are replayed.
    let mut w = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    w.finalize_scan_trees().unwrap();
    let (_, anchor) = w
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .unwrap()
        .unwrap();
    assert_eq!(u32::from(anchor), w.scanned_height);
    assert_eq!(
        w.orchard_tree
            .store()
            .get_checkpoint(&anchor)
            .unwrap()
            .unwrap()
            .position(),
        Some(Position::from(actual_size - 1))
    );
    assert_eq!(
        w.orchard_tree.root_at_checkpoint_id(&anchor).unwrap(),
        Some(expected_root)
    );
    let note = w.spend_notes.iter().find(|n| !n.spent).unwrap().clone();
    let witness = w
        .orchard_tree
        .witness_at_checkpoint_id(Position::from(note.position), &anchor)
        .unwrap()
        .unwrap();
    let leaf = MerkleHashOrchard::from_cmx(
        &WebWallet::decode_spend_note(&note)
            .unwrap()
            .commitment()
            .into(),
    );
    assert_eq!(witness.root(leaf), expected_root);

    let address = w.unified_address().to_string();
    let sent = w
        .prove_send(REGTEST_FAUCET_MNEMONIC, &address, "0.0002", None)
        .unwrap();
    let tx = Transaction::read(
        sent.as_slice(),
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(w.scanned_height + 1),
        ),
    )
    .unwrap();
    let vk = orchard::circuit::VerifyingKey::build(
        orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2,
    );
    tx.orchard_bundle().unwrap().verify_proof(&vk).unwrap();
    w.apply_mined_raw_tx(&sent, 2).unwrap();
    w.finalize_scan_trees().unwrap();
    assert_eq!(
        w.orchard_tree.root_at_checkpoint_id(&anchor).unwrap(),
        Some(expected_root)
    );
    assert!(w
        .orchard_tree
        .witness_at_checkpoint_id(Position::from(note.position), &anchor)
        .unwrap()
        .is_some());
}

#[test]
fn exact_checkpoint_retains_boundary_through_mark_removal_and_pruning() {
    type SmallTree = ShardTree<MemoryShardStore<MerkleHashOrchard, BlockHeight>, 8, 4>;
    let mut tree = SmallTree::new(MemoryShardStore::empty(), 1);
    let leaf = MerkleHashOrchard::empty_leaf();
    for position in 0..6 {
        tree.append(
            leaf,
            if position == 0 || position == 4 {
                Retention::Marked
            } else {
                Retention::Ephemeral
            },
        )
        .unwrap();
    }
    let anchor = BlockHeight::from_u32(20);
    checkpoint_complete_prefix(&mut tree, anchor, 6).unwrap();
    tree.ensure_retained(anchor).unwrap();
    let expected = tree.root_at_checkpoint_id(&anchor).unwrap().unwrap();
    tree.append(leaf, Retention::Ephemeral).unwrap();
    tree.append(
        leaf,
        Retention::Checkpoint {
            id: 21.into(),
            marking: Marking::None,
        },
    )
    .unwrap();
    tree.remove_mark(Position::from(4), Some(&21.into()))
        .unwrap();
    tree.append(
        leaf,
        Retention::Checkpoint {
            id: 22.into(),
            marking: Marking::None,
        },
    )
    .unwrap();
    assert_eq!(tree.root_at_checkpoint_id(&anchor).unwrap(), Some(expected));
    assert_eq!(
        tree.witness_at_checkpoint_id(Position::from(0), &anchor)
            .unwrap()
            .unwrap()
            .root(leaf),
        expected
    );
}

#[test]
fn empty_checkpoint_excludes_future_seeded_shards_in_every_pool() {
    let mut w = wallet();
    w.sapling_tree
        .insert(
            MerkleAddress::from_parts(SAPLING_SHARD_HEIGHT.into(), 1),
            sapling::Node::empty_root(SAPLING_SHARD_HEIGHT.into()),
        )
        .unwrap();
    for tree in [&mut w.orchard_tree, &mut w.ironwood_tree] {
        tree.insert(
            MerkleAddress::from_parts(ORCHARD_SHARD_HEIGHT.into(), 1),
            MerkleHashOrchard::empty_root(ORCHARD_SHARD_HEIGHT.into()),
        )
        .unwrap();
    }
    w.scanned_height = 10;
    w.checkpoint_live_trees().unwrap();
    assert_eq!(
        w.sapling_tree
            .store()
            .get_checkpoint(&10.into())
            .unwrap()
            .unwrap()
            .position(),
        None
    );
    for tree in [&w.orchard_tree, &w.ironwood_tree] {
        assert_eq!(
            tree.store()
                .get_checkpoint(&10.into())
                .unwrap()
                .unwrap()
                .position(),
            None
        );
        assert_eq!(
            tree.root_at_checkpoint_id(&10.into()).unwrap(),
            Some(MerkleHashOrchard::empty_root(32.into()))
        );
    }
}

#[test]
#[ignore = "synthetic cost benchmark; run explicitly with --release --ignored --nocapture"]
fn benchmark_snapshot_and_validation() {
    use std::{hint::black_box, time::Instant};

    for count in [16_384usize, 131_072, 524_288] {
        let mut w = wallet();
        w.orchard_leaves = (0..count as u64).map(leaf).collect();
        w.orchard_next = count as u64;
        w.assert_consistent_leaf_hashes().unwrap();
        let bytes = w.to_snapshot().unwrap();
        let owned: Snapshot = serde_json::from_slice(&bytes).unwrap();
        let mut cloned_ms = Vec::new();
        let mut borrowed_ms = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let cloned = owned.clone();
            black_box(serde_json::to_vec(&cloned).unwrap());
            drop(cloned);
            cloned_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(w.to_snapshot().unwrap());
            borrowed_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        cloned_ms.sort_by(f64::total_cmp);
        borrowed_ms.sort_by(f64::total_cmp);

        // Original behavior on every blob: allocate/map/sort all retained leaves.
        let mut old = w.orchard_leaves.clone();
        let start = Instant::now();
        for _ in 0..5 {
            let mut best = BTreeMap::new();
            for leaf in old.drain(..) {
                best.insert(leaf.position, leaf);
            }
            old.extend(best.into_values());
            black_box(&old);
        }
        let old_check_ms = start.elapsed().as_secs_f64() * 1000.0 / 5.0;
        let start = Instant::now();
        for _ in 0..10_000 {
            black_box(&mut w).assert_consistent_leaf_hashes().unwrap();
        }
        let empty_check_ns = start.elapsed().as_nanos() as f64 / 10_000.0;
        let new_leaves: Vec<_> = (count as u64..count as u64 + 1000).map(leaf).collect();
        w.orchard_leaves.extend(new_leaves);
        let start = Instant::now();
        w.assert_consistent_leaf_hashes().unwrap();
        let append_check_us = start.elapsed().as_secs_f64() * 1_000_000.0;

        // Isolate a real no-note blob from historical validation and JSON costs.
        let blocks = (1..=1000).map(|h| {
            let previous = if h == 1 {
                vec![0; 32]
            } else {
                empty_block(h - 1, vec![]).hash
            };
            empty_block(h, previous)
        });
        let blob = super::super::encode_delimited(blocks);
        let start = Instant::now();
        w.apply_compact_blocks_blob(&blob).unwrap();
        let empty_blob_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("leaves={count} bytes={} clone_json_ms={:.3} borrowed_json_ms={:.3} old_validation_ms={old_check_ms:.3} incremental_empty_ns={empty_check_ns:.1} incremental_1000_us={append_check_us:.2} empty_1000_block_blob_ms={empty_blob_ms:.3}", bytes.len(), cloned_ms[2], borrowed_ms[2]);
    }
}

#[test]
#[ignore = "isolated before/after post-finalize pool hashing benchmark"]
fn benchmark_unused_pool_catchup_hashing() {
    use std::{hint::black_box, time::Instant};
    for pool in ["sapling", "orchard", "ironwood"] {
        let mut live_ms = Vec::new();
        let mut deferred_ms = Vec::new();
        for _ in 0..3 {
            let mut live = wallet();
            live.finalize_scan_trees().unwrap();
            // Force the previous all-pool hashing policy for comparison.
            live.live_pools = SAPLING_LIVE | ORCHARD_LIVE | IRONWOOD_LIVE;
            let mut deferred = wallet();
            deferred.finalize_scan_trees().unwrap();
            let sapling = vec![(sapling::Node::empty_leaf(), Retention::Ephemeral); 1000];
            let orchard = vec![(MerkleHashOrchard::empty_leaf(), Retention::Ephemeral); 1000];
            let apply = |w: &mut WebWallet| match pool {
                "sapling" => w.append_sapling(1, &sapling),
                "orchard" => w.append_orchard(1, &orchard),
                _ => w.append_ironwood(1, &orchard),
            };
            let start = Instant::now();
            apply(&mut live).unwrap();
            black_box(&live);
            live_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            apply(&mut deferred).unwrap();
            black_box(&deferred);
            deferred_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(
                live.to_snapshot().unwrap(),
                deferred.to_snapshot().unwrap(),
                "deferral must preserve every retained leaf/cursor needed by a later receipt"
            );
        }
        live_ms.sort_by(f64::total_cmp);
        deferred_ms.sort_by(f64::total_cmp);
        println!(
            "pool={pool} unused_leaves=1000 live_hash_ms={:.3} deferred_store_ms={:.3}",
            live_ms[1], deferred_ms[1]
        );
    }
}

/// Blocks 1..=tip of a chain that shares blocks below `fork_at` with the
/// `empty_block` chain and has its own hashes from `fork_at` on.
fn chain_blocks(tip: u32, fork_at: u32, tag: u8) -> Vec<CompactBlock> {
    let mut prev = vec![0; 32];
    (1..=tip)
        .map(|h| {
            let mut block = empty_block(h.into(), prev.clone());
            if h >= fork_at {
                block.hash[31] = tag;
            }
            prev = block.hash.clone();
            block
        })
        .collect()
}

/// Follow `blocks` to their tip as the SDK sync does: apply from the next
/// height and, on a reorg, restart where the engine says. Returns restarts.
fn follow(w: &mut WebWallet, blocks: &[CompactBlock]) -> Result<u32> {
    let tip = blocks.len() as u32;
    let mut restarts = 0;
    while w.next_height() <= tip {
        let from = w.next_height();
        let page = blocks[(from - 1) as usize..].iter().cloned();
        match w.apply_compact_blocks_blob(&super::super::encode_delimited(page)) {
            Ok(_) => {}
            Err(EngineError::Reorg { next, .. }) => {
                assert!(next <= from, "a reorg restart never skips ahead");
                assert_eq!(next, w.next_height());
                restarts += 1;
                assert!(restarts <= 16, "reorg search did not converge");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(restarts)
}

#[test]
fn a_short_tip_reorg_steps_back_a_few_blocks_and_keeps_the_session_healthy() {
    let mut w = wallet();
    follow(&mut w, &chain_blocks(60, 60, 0)).unwrap();
    let replaced = chain_blocks(61, 59, 0xb);
    let tip = replaced.last().unwrap().clone();
    let err = w
        .apply_compact_blocks_blob(&super::super::encode_delimited([tip]))
        .unwrap_err();
    // Used to rewind to the birthday and poison every later apply and save.
    assert!(
        matches!(
            err,
            EngineError::Reorg {
                height: 61,
                next: 51
            }
        ),
        "{err}"
    );
    assert_eq!(w.scanned_height, 50);
    w.to_snapshot()
        .expect("a reorg restart must not poison the session");
    assert_eq!(follow(&mut w, &replaced).unwrap(), 0);
    assert_eq!(w.scanned_height, 61);
    assert_eq!(w.hash_at(60), Some(to_hex(&replaced[59].hash).as_str()));
    assert_eq!(w.hash_at(58), Some(to_hex(&replaced[57].hash).as_str()));
    w.to_snapshot().unwrap();
}

#[test]
fn a_deeper_reorg_doubles_the_step_until_the_fork_is_below_it() {
    let mut w = wallet();
    follow(&mut w, &chain_blocks(300, 300, 0)).unwrap();
    // Fork after 250: steps of 10, 20 and 40 blocks reach it.
    let replaced = chain_blocks(301, 251, 0xb);
    let restarts = follow(&mut w, &replaced).unwrap();
    assert_eq!(restarts, 3);
    assert_eq!(w.scanned_height, 301);
    assert_eq!(w.hash_at(251), Some(to_hex(&replaced[250].hash).as_str()));
    assert_eq!(w.hash_at(250), Some(to_hex(&replaced[249].hash).as_str()));
    w.to_snapshot().unwrap();
}

#[test]
fn a_young_wallet_whose_first_block_reorged_rescans_its_short_history() {
    let mut w = wallet();
    follow(&mut w, &chain_blocks(5, 5, 0)).unwrap();
    // Every block from the birthday on was replaced; all hashes are saved.
    let replaced = chain_blocks(6, 1, 0xb);
    follow(&mut w, &replaced).unwrap();
    assert_eq!(w.scanned_height, 6);
    assert_eq!(w.hash_at(1), Some(to_hex(&replaced[0].hash).as_str()));
    w.to_snapshot().unwrap();
}

#[test]
fn a_fork_below_every_saved_hash_still_fails_closed() {
    let mut w = wallet();
    let tip = HASH_KEEP as u32 + 100;
    follow(&mut w, &chain_blocks(tip, tip, 0)).unwrap();
    let oldest = *w.block_hashes.keys().next().unwrap();
    assert!(oldest > 1, "old hashes were pruned");
    let replaced = chain_blocks(tip + 1, oldest - 10, 0xb);
    let err = follow(&mut w, &replaced).unwrap_err();
    let message = err.to_string();
    // The saved snapshot must not be replaced by a birthday rescan unasked,
    // and the SDK must not read the poison as another restart request.
    assert!(
        message.contains("below every saved block hash"),
        "{message}"
    );
    assert!(!message.to_ascii_lowercase().contains("reorg"), "{message}");
    assert!(w.to_snapshot().is_err());
}

/// Faucet wallet with one spendable Orchard note from a mined shield.
#[cfg(feature = "transparent-inputs")]
fn funded_wallet() -> (WebWallet, u64) {
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(
        r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    )
    .unwrap();
    let shield = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    w.apply_mined_raw_tx(&shield, 1).unwrap();
    let funded = w.balance().orchard_available;
    assert!(funded > 0);
    (w, funded)
}

/// Empty blocks from the wallet's next height through `to`.
#[cfg(feature = "transparent-inputs")]
fn extend_empty(w: &mut WebWallet, to: u32) {
    let mut prev = super::super::from_hex(w.block_hashes.values().next_back().unwrap()).unwrap();
    let blocks: Vec<_> = (w.next_height()..=to)
        .map(|h| {
            let block = empty_block(h.into(), prev.clone());
            prev = block.hash.clone();
            block
        })
        .collect();
    w.apply_compact_blocks_blob(&super::super::encode_delimited(blocks))
        .unwrap();
}

#[cfg(feature = "transparent-inputs")]
fn expiry_of(w: &WebWallet, raw: &[u8]) -> (String, u32) {
    let tx = Transaction::read(
        raw,
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(w.scanned_height + 1),
        ),
    )
    .unwrap();
    (to_hex(tx.txid().as_ref()), u32::from(tx.expiry_height()))
}

/// Our send A reserves a note; another transaction B (the same seed on
/// another device, or another tab) spends it and is mined. When A expires
/// unmined, releasing its inputs must not make B's spent note spendable.
#[cfg(feature = "transparent-inputs")]
#[test]
fn an_expired_reservation_never_releases_a_note_the_chain_spent() {
    let (mut w, _) = funded_wallet();
    let to = w.unified_address().to_string();
    let lost = w
        .prove_send(REGTEST_FAUCET_MNEMONIC, to.as_str(), "0.1", None)
        .unwrap();
    let (a, expiry) = expiry_of(&w, &lost);
    let reserved = w.spend_notes.iter().find(|n| n.spent).unwrap().nf.clone();
    let mut block = empty_block(
        w.next_height().into(),
        super::super::from_hex(w.block_hashes.values().next_back().unwrap()).unwrap(),
    );
    block.chain_metadata = None;
    let b = vec![0xbb; 32];
    block
        .vtx
        .push(zcash_client_backend::proto::compact_formats::CompactTx {
            index: 1,
            txid: b.clone(),
            actions: vec![
                zcash_client_backend::proto::compact_formats::CompactOrchardAction {
                    nullifier: super::super::from_hex(&reserved).unwrap(),
                    cmx: vec![7; 32],
                    ephemeral_key: vec![0; 32],
                    ciphertext: vec![0; 52],
                },
            ],
            ..Default::default()
        });
    w.apply_compact_blocks_blob(&super::super::encode_delimited([block]))
        .unwrap();
    let spender = |w: &WebWallet| {
        w.spend_notes
            .iter()
            .find(|n| n.nf == reserved)
            .map(|n| (n.spent, n.spent_in.clone()))
            .unwrap()
    };
    assert_eq!(
        spender(&w),
        (true, Some(to_hex(&b))),
        "the mined spender wins"
    );
    extend_empty(&mut w, expiry);
    assert!(w.expired_unmined(w.txs.get(&a).unwrap()));
    assert_eq!(spender(&w).0, true, "A's expiry must not revive B's spend");
    assert_eq!(w.balance().orchard_available, 0);
    assert!(
        w.prove_send(REGTEST_FAUCET_MNEMONIC, to.as_str(), "0.1", None)
            .is_err(),
        "a spent note must never be selected again"
    );

    // Snapshots saved before the fix hold A as the reserving spender of a
    // note B spent, then released it. Loading repairs that.
    let mut stale = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    for n in &mut stale.spend_notes {
        if n.nf == reserved {
            n.spent = false;
            n.spent_in = None;
        }
    }
    let reloaded = WebWallet::from_snapshot(&stale.to_snapshot().unwrap()).unwrap();
    let repaired = reloaded
        .spend_notes
        .iter()
        .find(|n| n.nf == reserved)
        .unwrap();
    assert!(repaired.spent);
    assert_eq!(repaired.spent_in.as_deref(), Some(to_hex(&b).as_str()));
}

/// A reorg that removes the block with our mined send leaves the send in the
/// mempool: it stays pending with its inputs reserved, and only its expiry
/// returns them, as in SQLite wallets. It used to be deleted with its inputs
/// released, so the same notes could be offered to a second send at once.
#[cfg(feature = "transparent-inputs")]
#[test]
fn a_reorged_out_send_stays_pending_until_it_expires() {
    let (mut w, funded) = funded_wallet();
    let to = w.unified_address().to_string();
    let sent = w
        .prove_send(REGTEST_FAUCET_MNEMONIC, to.as_str(), "0.1", None)
        .unwrap();
    let (txid, expiry) = expiry_of(&w, &sent);
    let before = w.scanned_height;
    w.apply_mined_raw_tx(&sent, 1).unwrap();
    assert_eq!(w.txs.get(&txid).unwrap().mined_height, Some(before + 1));
    let change = w.balance().total_pending + w.balance().total_available;
    assert!(change > 0 && change < funded);

    w.rewind_to_height(before).unwrap();
    let agg = w.txs.get(&txid).expect("the reorged-out send is kept");
    assert_eq!(agg.mined_height, None);
    let balance = w.balance();
    assert_eq!(balance.orchard_available, 0, "its input stays reserved");
    assert_eq!(balance.total_pending, change, "its change is pending again");
    assert!(w
        .prove_send(REGTEST_FAUCET_MNEMONIC, to.as_str(), "0.1", None)
        .is_err());

    extend_empty(&mut w, expiry - 1);
    assert_eq!(w.balance().orchard_available, 0);
    extend_empty(&mut w, expiry);
    let released = w.balance();
    assert_eq!(released.orchard_available, funded);
    assert_eq!(released.total_pending, 0);
}

#[test]
fn unmined_send_bytes_survive_a_reload_for_rebroadcast() {
    let mut w = wallet();
    let pending = TxId::from_bytes([1; 32]);
    let mined = TxId::from_bytes([2; 32]);
    let expired = TxId::from_bytes([3; 32]);
    for (id, mined_height, expiry) in [
        (pending, None, 140),
        (mined, Some(99), 140),
        (expired, None, 90),
    ] {
        w.pending_txs.insert(id, vec![id.as_ref()[0]; 3]);
        w.txs.insert(
            to_hex(id.as_ref()),
            TxAgg {
                mined_height,
                expiry_height: Some(expiry),
                spent_notes: 1,
                ..Default::default()
            },
        );
    }
    record_test_height(&mut w, 100);
    assert_eq!(w.pending_raw_txs(), vec![to_hex(&[1u8; 3])]);
    // A reload (page refresh) keeps only what can still be rebroadcast.
    let reloaded = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert_eq!(reloaded.pending_raw_txs(), vec![to_hex(&[1u8; 3])]);
    assert_eq!(reloaded.pending_txs.len(), 1);
}

/// Orchard spends from NU6.3 on leave zero-value Orchard notes in the wallet.
/// Offered to the greedy selector ahead of a covering note, they came back as
/// dust; excluding them added no value, so every proposal reported
/// insufficient funds (seen on regtest after an Orchard-to-Ironwood send).
#[cfg(feature = "transparent-inputs")]
#[test]
fn zero_value_notes_do_not_block_spending() {
    let mut w = wallet();
    let b1 = empty_block(1, vec![0; 32]);
    let b2 = empty_block(2, b1.hash.clone());
    w.apply_compact_blocks_blob(&super::super::encode_delimited([b1, b2]))
        .unwrap();
    w.apply_utxos_json(
        r#"{"utxos":[{"txid":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
    ).unwrap();
    let shield = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
    w.apply_mined_raw_tx(&shield, 1).unwrap();
    let own = w.unified_address().to_string();
    w.estimate_fee(&own, "0.0001", None).unwrap();

    let funded = w
        .spend_notes
        .iter()
        .find(|n| !n.spent && n.value_zat > 0)
        .unwrap()
        .clone();
    // Same block and position, so the selector meets the dust notes first.
    // Two of them: one would ride in a grace action for free.
    let next = w.spend_notes.iter().map(|n| n.id).max().unwrap() + 1;
    for id in [next, next + 1] {
        let dust = StoredNote {
            id,
            value_zat: 0,
            ..funded.clone()
        };
        w.spend_notes.insert(0, dust);
    }
    let fee = w.estimate_fee(&own, "0.0001", None);
    assert!(fee.is_ok(), "dust notes blocked the proposal: {fee:?}");
}

fn live_twins() -> (WebWallet, WebWallet) {
    let mut w = wallet();
    let mut reference = wallet();
    for current in [&mut w, &mut reference] {
        current.finalize_scan_trees().unwrap();
        for pool in ["sapling", "orchard", "ironwood"] {
            append_pool(current, pool, 1, 4, false);
        }
        append_pool(current, "orchard", 1, 1, true);
        record_test_height(current, 1);
        current.finalize_scan_trees().unwrap();
        assert!(current.sinsemilla_live);
        assert_eq!(current.live_pools, ORCHARD_LIVE);
    }
    (w, reference)
}

/// A reload keeps the hashed trees: witnesses work at once, and scanning on
/// leaves it in exactly the state of a wallet that never reloaded.
#[test]
fn saved_trees_reload_live_and_scan_on_identically() {
    let (mut w, mut reference) = live_twins();
    let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert!(restored.sinsemilla_live);
    assert_eq!(restored.live_pools, ORCHARD_LIVE);
    assert_pool_witness(&mut restored, &mut reference, "orchard", 4, 1);
    for height in 2..=3 {
        for current in [&mut w, &mut restored, &mut reference] {
            for pool in ["sapling", "orchard", "ironwood"] {
                append_pool(current, pool, height, 3, false);
            }
            record_test_height(current, height);
        }
    }
    assert_pool_witness(&mut restored, &mut reference, "orchard", 4, 3);
    assert_pool_witness(&mut restored, &mut reference, "orchard", 4, 2);
    assert_eq!(restored.to_snapshot().unwrap(), w.to_snapshot().unwrap());
    restored.rewind_to_height(2).unwrap();
    assert_pool_witness(&mut restored, &mut reference, "orchard", 4, 2);
}

/// Saved trees that do not match the scan position, or do not decode, are
/// ignored: the wallet re-hashes at its next finalize as before.
#[test]
fn mismatched_or_corrupt_saved_trees_are_ignored() {
    let (w, mut reference) = live_twins();
    let saved: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
    let mut stale = saved.clone();
    stale["liveTrees"]["orchardNext"] = serde_json::json!(4);
    let mut corrupt = saved.clone();
    corrupt["liveTrees"]["orchard"]["cap"] = serde_json::json!("ff");
    for bad in [stale, corrupt] {
        let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(!restored.sinsemilla_live);
        restored.finalize_scan_trees().unwrap();
        assert!(restored.sinsemilla_live);
        assert_pool_witness(&mut restored, &mut reference, "orchard", 4, 1);
    }
}

// ── Hardware signers through PCZTs ───────────────────────────────────────────

/// The funded faucet wallet, as a hardware account for the same seed: the
/// "device" signs with the faucet mnemonic, which is what a real device holds.
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
fn hardware_wallet() -> WebWallet {
    let (mut w, _) = funded_wallet();
    w.hardware = Some(HardwareAccount {
        device: "keystone".into(),
        seed_fingerprint: crate::keys::seed_fingerprint(REGTEST_FAUCET_MNEMONIC).unwrap(),
        account_index: 0,
    });
    w
}

#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
fn parse_tx(w: &WebWallet, raw: &[u8]) -> Transaction {
    Transaction::read(
        raw,
        zcash_protocol::consensus::BranchId::for_height(
            &Network::Regtest,
            BlockHeight::from_u32(w.scanned_height + 1),
        ),
    )
    .unwrap()
}

/// Keystone's full-PCZT flow: the device signs a redacted copy and returns a
/// PCZT, which is combined with the proved one.
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
#[test]
fn hardware_send_round_trip_full_pczt() {
    use super::super::hardware::{sign_pczt_with_mnemonic, SignerCopy};
    let mut w = hardware_wallet();
    let to = w.unified_address().to_string();
    let pczt = w
        .hardware_create_send(&to, "0.1", Some("hardware memo"))
        .unwrap();
    // The note is locked while the device signs: a second PCZT cannot spend it.
    assert!(w.hardware_create_send(&to, "0.1", None).is_err());
    // A reload drops the reservation with the PCZT that held it, so the note
    // is not stranded; cancelling releases it in place.
    let mut reloaded = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    assert!(reloaded.hardware_create_send(&to, "0.1", None).is_ok());
    let mut cancelled = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
    cancelled.hardware_create_send(&to, "0.1", None).unwrap();
    assert!(cancelled.hardware_release_locks() > 0);
    assert!(cancelled.hardware_create_send(&to, "0.1", None).is_ok());
    let proved = w.hardware_prove(&pczt).unwrap();
    let copy = w.hardware_signer_copy(&pczt, SignerCopy::Full).unwrap();
    let signed =
        sign_pczt_with_mnemonic(&copy, REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
    let combined = w.hardware_combine(&proved, &signed).unwrap();
    let done = w.hardware_finalize(&combined).unwrap();
    let raw = super::super::from_hex(&done.hex).unwrap();
    let tx = parse_tx(&w, &raw);
    assert_eq!(tx.txid().to_string(), done.txid);
    let actions = tx.orchard_bundle().map(|b| b.actions().len()).unwrap_or(0)
        + tx.ironwood_bundle().map(|b| b.actions().len()).unwrap_or(0);
    assert!(actions >= 2, "a shielded spend and outputs");
    // Recorded as our pending send, and it mines like any other.
    assert!(w.history_matching(5, None, Some(&done.txid)).len() == 1);
    w.apply_mined_raw_tx(&raw, 2).unwrap();
    assert!(w.balance().orchard_available > 0, "change came back");
}

/// Ledger and Keystone batch signing: the device returns spend authorization
/// signatures only; they are applied (and verified) on the proved PCZT.
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
#[test]
fn hardware_send_round_trip_signatures_only() {
    use super::super::hardware::{sign_pczt_with_mnemonic, DeviceSignature, SignerCopy};
    use pczt::roles::signer::extract_orchard_spend_auth_signatures;
    let mut w = hardware_wallet();
    let to = w.unified_address().to_string();
    let pczt = w.hardware_create_send(&to, "0.05", None).unwrap();
    let proved = w.hardware_prove(&pczt).unwrap();
    // The batch copy carries no FVK (Keystone derives its own); the simulated
    // device signs the full copy and returns only the signatures.
    let batch = w.hardware_signer_copy(&pczt, SignerCopy::Batch).unwrap();
    assert!(pczt::Pczt::parse(&batch).is_ok());
    let full = w.hardware_signer_copy(&pczt, SignerCopy::Full).unwrap();
    let signed =
        sign_pczt_with_mnemonic(&full, REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
    let sigs: Vec<DeviceSignature> =
        extract_orchard_spend_auth_signatures(&pczt::Pczt::parse(&signed).unwrap())
            .into_iter()
            .map(|s| DeviceSignature {
                pool: match s.value_pool() {
                    orchard::ValuePool::Ironwood => "ironwood".into(),
                    _ => "orchard".into(),
                },
                action_index: s.action_index(),
                signature: to_hex(s.signature()),
            })
            .collect();
    assert!(!sigs.is_empty());
    let applied = w.hardware_apply_signatures(&proved, &sigs).unwrap();
    let done = w.hardware_finalize(&applied).unwrap();
    assert_eq!(
        parse_tx(&w, &super::super::from_hex(&done.hex).unwrap())
            .txid()
            .to_string(),
        done.txid
    );
}

/// A signature from another seed is refused before anything is broadcast,
/// and an unsigned PCZT cannot be finalized.
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
#[test]
fn hardware_rejects_foreign_or_missing_signatures() {
    use super::super::hardware::{sign_pczt_with_mnemonic, DeviceSignature, SignerCopy};
    use pczt::roles::signer::extract_orchard_spend_auth_signatures;
    let mut w = hardware_wallet();
    let to = w.unified_address().to_string();
    let pczt = w.hardware_create_send(&to, "0.05", None).unwrap();
    let proved = w.hardware_prove(&pczt).unwrap();
    assert!(
        w.hardware_finalize(&proved).is_err(),
        "unsigned spends must not finalize"
    );
    // Garbage, and a valid signature over some other transaction, must both be refused.
    let garbage = DeviceSignature {
        pool: "orchard".into(),
        action_index: 0,
        signature: "11".repeat(64),
    };
    assert!(w.hardware_apply_signatures(&proved, &[garbage]).is_err());
    let mut other = hardware_wallet();
    let other_pczt = other.hardware_create_send(&to, "0.02", None).unwrap();
    let other_signed = sign_pczt_with_mnemonic(
        &other
            .hardware_signer_copy(&other_pczt, SignerCopy::Full)
            .unwrap(),
        REGTEST_FAUCET_MNEMONIC,
        Network::Regtest,
        0,
    )
    .unwrap();
    let sigs: Vec<DeviceSignature> =
        extract_orchard_spend_auth_signatures(&pczt::Pczt::parse(&other_signed).unwrap())
            .into_iter()
            .map(|s| DeviceSignature {
                pool: match s.value_pool() {
                    orchard::ValuePool::Ironwood => "ironwood".into(),
                    _ => "orchard".into(),
                },
                action_index: s.action_index(),
                signature: to_hex(s.signature()),
            })
            .collect();
    assert!(!sigs.is_empty());
    assert!(
        w.hardware_apply_signatures(&proved, &sigs).is_err(),
        "signatures over another transaction must not verify"
    );
}

/// Seed wallets cannot use the hardware path, and hardware metadata survives a
/// snapshot round trip (the derivation goes into every PCZT).
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
#[test]
fn hardware_account_persists_and_seed_wallets_are_refused() {
    let (mut seed_wallet, _) = funded_wallet();
    let to = seed_wallet.unified_address().to_string();
    assert!(seed_wallet.hardware_create_send(&to, "0.01", None).is_err());
    let w = hardware_wallet();
    let bytes = w.to_snapshot().unwrap();
    let back = WebWallet::from_snapshot(&bytes).unwrap();
    assert_eq!(back.hardware(), w.hardware());
    // Near the front, so the SDK's 512 KB header peek of a large snapshot
    // (hashed trees come later) still knows it is a hardware account.
    let text = String::from_utf8_lossy(&bytes);
    let at = text.find("\"hardware\"").expect("hardware is saved");
    assert!(at < 1024, "hardware at byte {at}");
    // Its seed turning up elsewhere on the origin must not bypass the device.
    let mut hw = hardware_wallet();
    assert!(hw.attach_seed(REGTEST_FAUCET_MNEMONIC).is_err());
    assert!(hw
        .prove_send(REGTEST_FAUCET_MNEMONIC, &to, "0.01", None)
        .is_err());
    assert!(hw.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());
}

/// Ledger: the engine plans every APDU and checks every response. The
/// simulated device acks the PCZT packets and answers each signature request
/// with the signature the account's key makes (what the Zcash app returns).
#[cfg(all(feature = "transparent-inputs", feature = "hardware"))]
#[test]
fn ledger_send_round_trip_with_a_simulated_device() {
    use super::super::hardware::{sign_pczt_with_mnemonic, SignerCopy};
    use pczt::roles::signer::extract_orchard_spend_auth_signatures;
    let (funded, _) = funded_wallet();
    let ufvk = funded.ufvk().to_string();
    let fp = to_hex(&crate::ledger::account_fingerprint(&ufvk, 0));
    // A Ledger account must use the fingerprint its export produced.
    let wrong = HardwareAccount {
        device: "ledger".into(),
        seed_fingerprint: "ab".repeat(32),
        account_index: 0,
    };
    assert!(WebWallet::from_hardware(Network::Regtest, &ufvk, 1, wrong).is_err());
    assert!(WebWallet::from_hardware(
        Network::Regtest,
        &ufvk,
        1,
        HardwareAccount {
            device: "ledger".into(),
            seed_fingerprint: fp.clone(),
            account_index: 0
        },
    )
    .is_ok());
    let mut w = funded;
    w.hardware = Some(HardwareAccount {
        device: "ledger".into(),
        seed_fingerprint: fp,
        account_index: 0,
    });
    let to = w.unified_address().to_string();

    // A memo the app shows as a hash needs 3.9.4; 3.9.3 would reset the device.
    let accented = w.hardware_create_send(&to, "0.01", Some("café")).unwrap();
    assert!(w
        .ledger_signing_plan(&accented, "3.9.3")
        .unwrap_err()
        .to_string()
        .contains("ledger_memo_hash_unsupported"));
    assert!(w.ledger_signing_plan(&accented, "3.9.4").is_ok());
    w.hardware_release_locks();

    let pczt = w.hardware_create_send(&to, "0.05", Some("thanks")).unwrap();
    assert!(w
        .ledger_signing_plan(&pczt, "3.9.2")
        .unwrap_err()
        .to_string()
        .contains("ledger_app_outdated"));
    let plan = w.ledger_signing_plan(&pczt, "3.9.3").unwrap();
    assert_eq!(plan.commands[0].ins, 0x52);
    assert!(plan.commands.iter().all(|c| c.data.len() <= 255));
    assert!(!plan.signatures.is_empty());
    let packets = plan.commands.len() - plan.signatures.len();
    assert!(plan.review_index < packets);

    let ok = |mut v: Vec<u8>| {
        v.extend_from_slice(&[0x90, 0x00]);
        v
    };
    let device = |pczt: &[u8]| -> Vec<Vec<u8>> {
        let signed = sign_pczt_with_mnemonic(
            &w.hardware_signer_copy(pczt, SignerCopy::Full).unwrap(),
            REGTEST_FAUCET_MNEMONIC,
            Network::Regtest,
            0,
        )
        .unwrap();
        let sigs = extract_orchard_spend_auth_signatures(&pczt::Pczt::parse(&signed).unwrap());
        let mut responses: Vec<Vec<u8>> = (0..packets).map(|_| ok(vec![])).collect();
        for cmd in &plan.commands[packets..] {
            let pool = if cmd.ins == 0x59 {
                orchard::ValuePool::Ironwood
            } else {
                orchard::ValuePool::Orchard
            };
            let sig = sigs
                .iter()
                .find(|s| s.value_pool() == pool && s.action_index() == cmd.p2 as usize)
                .unwrap();
            responses.push(ok(sig.signature().to_vec()));
        }
        responses
    };
    let responses = device(&pczt);
    // The user reviews on the device while the wallet proves.
    let proved = w.hardware_prove(&pczt).unwrap();

    let mut rejected = responses.clone();
    rejected[plan.review_index] = vec![0x69, 0x85];
    assert!(w
        .ledger_apply_responses(&proved, &rejected)
        .unwrap_err()
        .to_string()
        .starts_with("ledger_status_6985"));
    assert!(w
        .ledger_apply_responses(&proved, &responses[..responses.len() - 1])
        .is_err());
    let mut zero = responses.clone();
    *zero.last_mut().unwrap() = ok(vec![0; 64]);
    assert!(w.ledger_apply_responses(&proved, &zero).is_err());
    // A signature over another transaction does not verify.
    let mut other = hardware_wallet();
    other.hardware = w.hardware.clone();
    let other_pczt = other.hardware_create_send(&to, "0.02", None).unwrap();
    let foreign = device(&other_pczt);
    let mut mixed = responses.clone();
    *mixed.last_mut().unwrap() = foreign.last().unwrap().clone();
    assert!(w
        .ledger_apply_responses(&proved, &mixed)
        .unwrap_err()
        .to_string()
        .contains("ledger_signature_mismatch"));

    let applied = w.ledger_apply_responses(&proved, &responses).unwrap();
    let done = w.hardware_finalize(&applied).unwrap();
    assert_eq!(
        parse_tx(&w, &super::super::from_hex(&done.hex).unwrap())
            .txid()
            .to_string(),
        done.txid
    );
}

#[cfg(feature = "native")]
#[test]
fn shared_memo_ranges_verify_all_transactions_and_decrypt_only_local_matches() {
    use zcash_client_backend::proto::compact_formats::{CompactOrchardAction, CompactTx};
    use zcash_protocol::consensus::BranchId;
    for ironwood in [false, true] {
        let birthday = 1_000_001;
        let mut recipient = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            birthday,
        )
        .unwrap();
        let mut stranger = WebWallet::from_account(
            account_from_mnemonic(
                "legal winner thank year wave sausage worth useful legal winner thank yellow",
                Network::Regtest,
                0,
            )
            .unwrap(),
            birthday,
        )
        .unwrap();
        stranger.scanned_height = birthday;
        let raws = [
            shielded_memo_transaction(
                &stranger,
                &recipient,
                "shared private memo",
                50_000,
                ironwood,
            ),
            shielded_memo_transaction(&stranger, &stranger, "unrelated memo", 80_000, ironwood),
        ];
        let mut block = empty_block(u64::from(birthday), vec![0; 32]);
        block.chain_metadata = None;
        for (i, raw) in raws.iter().enumerate() {
            let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
            let bundle = if ironwood {
                tx.ironwood_bundle()
            } else {
                tx.orchard_bundle()
            }
            .unwrap();
            let actions: Vec<CompactOrchardAction> = bundle
                .actions()
                .iter()
                .map(CompactOrchardAction::from)
                .collect();
            block.vtx.push(CompactTx {
                index: i as u64 + 1,
                txid: tx.txid().as_ref().to_vec(),
                actions: if ironwood { vec![] } else { actions.clone() },
                ironwood_actions: if ironwood { actions } else { vec![] },
                ..Default::default()
            });
        }
        let blob = super::super::encode_one(&block);
        recipient.apply_compact_blocks_blob(&blob).unwrap();
        assert_eq!(recipient.history(10).len(), 1);
        assert!(recipient.history(1)[0].memos.is_empty());
        let bundle = serde_json::json!({ "start": birthday, "end": birthday, "blocks": to_hex(&blob),
            "transactions": raws.iter().map(|raw| to_hex(raw)).collect::<Vec<_>>() });
        let before = recipient.to_snapshot().unwrap();
        let mut missing = bundle.clone();
        missing["transactions"].as_array_mut().unwrap().pop();
        let mut swapped = bundle.clone();
        swapped["transactions"].as_array_mut().unwrap().swap(0, 1);
        let mut trailing = bundle.clone();
        trailing["transactions"][1] = format!("{}00", to_hex(&raws[1])).into();
        let mut wrong_range = bundle.clone();
        wrong_range["end"] = (birthday + 1).into();
        for invalid in [missing, swapped, trailing, wrong_range] {
            assert!(recipient.apply_shared_memos(&invalid.to_string()).is_err());
            assert_eq!(recipient.to_snapshot().unwrap(), before);
        }
        recipient.apply_shared_memos(&bundle.to_string()).unwrap();
        assert_eq!(
            recipient.history(10).len(),
            1,
            "unrelated transactions are never inserted into wallet history"
        );
        assert_eq!(recipient.history(1)[0].memos, vec!["shared private memo"]);
        let mut restored = WebWallet::from_snapshot(&recipient.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.memo_next_height(), birthday + 1);
        assert!(
            restored.apply_shared_memos(&bundle.to_string()).is_err(),
            "replayed coverage rejected"
        );
        restored.reset_scan();
        assert_eq!(restored.memo_next_height(), birthday);
    }
}

#[test]
fn explicit_transparent_send_rejects_ambiguous_destinations_before_mutation() {
    use zcash_address::{ToAddress, ZcashAddress};
    use zcash_protocol::consensus::NetworkType;
    let mut w = wallet();
    let before = w.to_snapshot().unwrap();
    let own = w.unified_address().to_string();
    let t = crate::keys::REGTEST_FAUCET_TRANSPARENT;
    let mainnet = ZcashAddress::from_transparent_p2pkh(NetworkType::Main, [3; 20]).to_string();
    let tex = ZcashAddress::from_tex(NetworkType::Test, [3; 20]).to_string();
    for (to, amount) in [
        (own, "0.1"),
        (format!("zcash:{t}?amount=0.1"), "0.1"),
        (
            format!("zcash:{t}?address.1={t}&amount=0.1&amount.1=0.1"),
            "0.1",
        ),
        (mainnet, "0.1"),
        (tex, "0.1"),
        (t.to_string(), "0"),
        (t.to_string(), "-1"),
        (t.to_string(), "0.000000001"),
    ] {
        assert!(w.estimate_transparent_fee(&to, amount).is_err(), "{to}");
        assert!(
            w.prove_transparent_send(REGTEST_FAUCET_MNEMONIC, &to, amount, None)
                .is_err(),
            "{to}"
        );
        assert_eq!(w.to_snapshot().unwrap(), before);
    }
    // New capability never changes ordinary send's destination policy.
    assert!(w
        .estimate_fee(t, "0.1", None)
        .unwrap_err()
        .to_string()
        .contains("transparent send"));
    assert!(w
        .prove_send(REGTEST_FAUCET_MNEMONIC, t, "0.1", None)
        .unwrap_err()
        .to_string()
        .contains("transparent send"));
}

#[cfg(feature = "transparent-inputs")]
#[test]
fn explicit_transparent_send_proves_exact_output_with_only_shielded_inputs() {
    use transparent::address::TransparentAddress;
    use zcash_keys::encoding::encode_transparent_address_p;
    for recipient in [
        TransparentAddress::PublicKeyHash([42; 20]),
        TransparentAddress::ScriptHash([43; 20]),
    ] {
        let (mut w, funded) = funded_wallet();
        let destination = encode_transparent_address_p(&Network::Regtest, &recipient);
        let fee = w.estimate_transparent_fee(&destination, "0.1").unwrap();
        assert!(fee > 0);
        let before_cap = w.to_snapshot().unwrap();
        assert!(w
            .prove_transparent_send(REGTEST_FAUCET_MNEMONIC, &destination, "0.1", Some(fee - 1))
            .unwrap_err()
            .to_string()
            .contains("approved maximum"));
        assert_eq!(
            w.to_snapshot().unwrap(),
            before_cap,
            "over-cap proposal must not reserve notes or create a pending transaction"
        );
        let raw = w
            .prove_transparent_send(REGTEST_FAUCET_MNEMONIC, &destination, "0.1", Some(fee))
            .unwrap();
        let tx = Transaction::read(
            raw.as_slice(),
            zcash_protocol::consensus::BranchId::for_height(
                &Network::Regtest,
                (w.scanned_height + 1).into(),
            ),
        )
        .unwrap();
        let transparent = tx
            .transparent_bundle()
            .expect("reviewed transparent output");
        assert!(
            transparent.vin.is_empty(),
            "transparent inputs must remain disabled"
        );
        assert_eq!(transparent.vout.len(), 1, "one reviewed output only");
        assert_eq!(transparent.vout[0].recipient_address(), Some(recipient));
        assert_eq!(u64::from(transparent.vout[0].value()), 10_000_000);
        let orchard = tx.orchard_bundle().expect("shielded spend and change");
        let vk = orchard::circuit::VerifyingKey::build(
            orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2,
        );
        orchard.verify_proof(&vk).unwrap();
        assert_eq!(w.balance().orchard_available, 0, "spent notes are reserved");
        let expected_change = funded - 10_000_000 - fee;
        assert_eq!(w.balance().total_pending, expected_change);
        let reloaded = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(
            reloaded.pending_raw_txs().contains(&to_hex(&raw)),
            "pending bytes survive reload"
        );
        assert_eq!(reloaded.balance().total_pending, expected_change);
        let txid = tx.txid().to_string();
        assert_eq!(w.history_matching(1, None, Some(&txid)).len(), 1);
        w.apply_mined_raw_tx(&raw, 2).unwrap();
        assert_eq!(
            w.balance().orchard_available,
            expected_change,
            "recognized shielded change becomes spendable"
        );
        assert_eq!(w.balance().total_pending, 0);
    }
}

#[cfg(feature = "hardware")]
#[test]
fn explicit_transparent_send_never_uses_hardware_seed_path() {
    let mut hw = hardware_wallet();
    let t = crate::keys::REGTEST_FAUCET_TRANSPARENT;
    let before = hw.to_snapshot().unwrap();
    assert!(hw
        .estimate_transparent_fee(t, "0.1")
        .unwrap_err()
        .to_string()
        .contains("hardware"));
    assert!(hw
        .prove_transparent_send(REGTEST_FAUCET_MNEMONIC, t, "0.1", None)
        .unwrap_err()
        .to_string()
        .contains("hardware"));
    assert_eq!(hw.to_snapshot().unwrap(), before);
}
