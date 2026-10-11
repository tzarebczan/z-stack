use super::*;
use transparent::{
    address::Script,
    bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut as NativeTxOut},
};
use zcash_primitives::{
    block::{BlockHash, BlockHeaderData},
    transaction::{Authorized, TransactionData, TxVersion},
};
use zcash_protocol::{consensus::BranchId, value::Zatoshis};

const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn transaction(height: u32, coinbase: bool, shielded: bool) -> Transaction {
    let branch = BranchId::for_height(&Network::Mainnet, height.into());
    let version = match branch {
        BranchId::Sprout => TxVersion::Sprout(1),
        BranchId::Overwinter => TxVersion::V3,
        BranchId::Sapling | BranchId::Blossom | BranchId::Heartwood | BranchId::Canopy => {
            TxVersion::V4
        }
        BranchId::Nu5 | BranchId::Nu6 | BranchId::Nu6_1 | BranchId::Nu6_2 => TxVersion::V5,
        BranchId::Nu6_3 | BranchId::Nu7 => TxVersion::V6,
    };
    let mut number = height.to_le_bytes().to_vec();
    while number.last() == Some(&0) {
        number.pop();
    }
    if number.last().is_some_and(|b| b & 0x80 != 0) {
        number.push(0);
    }
    let mut sig = vec![number.len() as u8];
    sig.extend(number);
    let transparent = Some(Bundle {
        vin: vec![TxIn::from_parts(
            OutPoint::new(
                if coinbase {
                    [0; 32]
                } else {
                    std::array::from_fn(|index| index as u8)
                },
                if coinbase { u32::MAX } else { 7 },
            ),
            Script(zcash_script::script::Code(sig)),
            u32::MAX,
        )],
        vout: vec![NativeTxOut::new(
            Zatoshis::from_u64(77).unwrap(),
            Script(zcash_script::script::Code(vec![0x51])),
        )],
        authorization: TransparentAuthorized,
    });
    let sapling = if shielded && version.has_sapling() {
        Some(sapling_bundle())
    } else {
        None
    };
    let family = if shielded && version.has_orchard() {
        Some(orchard_family())
    } else {
        None
    };
    let orchard = family.as_ref().and_then(|tx| tx.orchard_bundle().cloned());
    let ironwood = if version == TxVersion::V6 {
        family.as_ref().and_then(|tx| tx.ironwood_bundle().cloned())
    } else {
        None
    };
    if version == TxVersion::V6 {
        TransactionData::<Authorized>::from_parts_v6(
            branch,
            0,
            0.into(),
            transparent,
            sapling,
            orchard,
            ironwood,
        )
        .freeze()
        .unwrap()
    } else {
        TransactionData::<Authorized>::from_parts(
            version,
            branch,
            0,
            0.into(),
            transparent,
            None,
            sapling,
            orchard,
        )
        .freeze()
        .unwrap()
    }
}
fn sapling_bundle(
) -> sapling::Bundle<sapling::bundle::Authorized, zcash_protocol::value::ZatBalance> {
    use sapling::bundle::{OutputDescription, SpendDescription};
    let sample = Transaction::read(
        &include_bytes!("../wallet/fixtures/zakura-public-sapling/transaction.bin")[..],
        BranchId::Canopy,
    )
    .unwrap();
    let bundle = sample.sapling_bundle().unwrap();
    let s = &bundle.shielded_spends()[0];
    let o = &bundle.shielded_outputs()[0];
    let spend = SpendDescription::from_parts(
        s.cv().clone(),
        *s.anchor(),
        sapling::Nullifier([0x55; 32]),
        s.rk().clone(),
        *s.zkproof(),
        *s.spend_auth_sig(),
    );
    let output = OutputDescription::from_parts(
        o.cv().clone(),
        sapling::note::ExtractedNoteCommitment::from_bytes(&[6; 32]).unwrap(),
        zcash_note_encryption::EphemeralKeyBytes([7; 32]),
        [0x33; 580],
        [0; 80],
        *o.zkproof(),
    );
    sapling::Bundle::from_parts(
        vec![spend],
        vec![output],
        zcash_protocol::value::ZatBalance::zero(),
        bundle.authorization().clone(),
    )
    .unwrap()
}
fn orchard_epk() -> [u8; 32] {
    use orchard::primitives::redpallas::{SigningKey, SpendAuth, VerificationKey};
    let key = SigningKey::<SpendAuth>::try_from([7; 32]).unwrap();
    VerificationKey::from(&key).into()
}
fn orchard_family() -> Transaction {
    let empty = TransactionData::<Authorized>::from_parts_v6(
        BranchId::Nu6_3,
        0,
        0.into(),
        None,
        None,
        None,
        None,
    )
    .freeze()
    .unwrap();
    let mut raw = vec![];
    empty.write(&mut raw).unwrap();
    raw.truncate(raw.len() - 2);
    for pool in [1u8, 2] {
        raw.push(1);
        raw.extend([0; 32]);
        raw.extend([pool; 32]);
        raw.extend(orchard_epk());
        raw.extend([pool + 2; 32]);
        raw.extend(orchard_epk());
        raw.extend([0x40 + pool; 580]);
        raw.extend([0; 80]);
        raw.push(3);
        raw.extend(0i64.to_le_bytes());
        raw.extend([0; 32]);
        let n = orchard::Proof::expected_proof_size(1);
        raw.push(253);
        raw.extend((n as u16).to_le_bytes());
        raw.extend(vec![0; n]);
        raw.extend([0; 128]);
    }
    // Dummy proofs/signatures isolate effect mapping. No wallet, valid PoW,
    // consensus-valid proof/signature, or actual Mainnet fixture is claimed.
    Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap()
}
fn display_to_protocol(hash: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    for (b, c) in bytes.iter_mut().zip(hash.as_bytes().chunks_exact(2)) {
        *b = u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap();
    }
    bytes.reverse();
    bytes
}
struct Fixture {
    frame: Vec<u8>,
    accepted: AcceptedMainnetWindow,
    hashes: Vec<[u8; 32]>,
}
fn fixture(predecessor: &MainnetCompactCheckpoint, shielded: bool) -> Fixture {
    let start = predecessor.height + 1;
    let mut frame = super::super::MAINNET_BLOCK_WINDOW_MAGIC.to_vec();
    frame.extend(start.to_le_bytes());
    frame.extend(16u32.to_le_bytes());
    let mut parent = display_to_protocol(&predecessor.hash);
    let mut headers = vec![(predecessor.height, predecessor.hash.clone())];
    let mut hashes = vec![];
    for height in start..start + 16 {
        let cb = transaction(height, true, false);
        let pay = transaction(height, false, shielded);
        let mut m = Sha256::new();
        m.update(cb.txid().as_ref());
        m.update(pay.txid().as_ref());
        let merkle = Sha256::digest(m.finalize()).into();
        let h = BlockHeaderData {
            version: 4,
            prev_block: BlockHash(parent),
            merkle_root: merkle,
            final_sapling_root: [0; 32],
            time: height,
            bits: 0,
            nonce: [0; 32],
            solution: vec![],
        }
        .freeze()
        .unwrap();
        let mut raw = vec![];
        h.write(&mut raw).unwrap();
        raw.push(2);
        cb.write(&mut raw).unwrap();
        pay.write(&mut raw).unwrap();
        frame.extend((raw.len() as u32).to_le_bytes());
        frame.extend(raw);
        parent = h.hash().0;
        hashes.push(parent);
        headers.push((height, h.hash().to_string()));
    }
    Fixture {
        frame,
        accepted: AcceptedMainnetWindow::from_local_node(
            chain_identity::genesis(Network::Mainnet),
            SOURCE,
            headers,
        )
        .unwrap(),
        hashes,
    }
}
fn derive_fixture(f: &Fixture, p: &MainnetCompactCheckpoint) -> VerifiedMainnetCompactWindow {
    derive_mainnet_compact_window(&f.frame, &f.accepted, p, &AtomicBool::new(false)).unwrap()
}
fn blocks(w: &VerifiedMainnetCompactWindow) -> Vec<CompactBlock> {
    let mut b = w.encoded_blocks();
    let mut out = vec![];
    while !b.is_empty() {
        out.push(CompactBlock::decode_length_delimited(&mut b).unwrap());
    }
    out
}
fn seeded(height: u32, sizes: [u32; 3]) -> MainnetCompactCheckpoint {
    // Private synthetic prefix used only to exercise later pool boundaries and
    // nonzero counters. There is no production checkpoint import constructor.
    let mut p = MainnetCompactCheckpoint::canonical_genesis();
    p.height = height;
    p.hash = "11".repeat(32);
    for _ in 0..sizes[0] {
        checked_append(
            &mut p.sapling,
            sapling::Node::from_cmu(
                &sapling::note::ExtractedNoteCommitment::from_bytes(&[6; 32]).unwrap(),
            ),
        )
        .unwrap();
    }
    for (tree, n, byte) in [
        (&mut p.orchard, sizes[1], 3),
        (&mut p.ironwood, sizes[2], 4),
    ] {
        for _ in 0..n {
            checked_append(
                tree,
                orchard::tree::MerkleHashOrchard::from_cmx(
                    &orchard::note::ExtractedNoteCommitment::from_bytes(&[byte; 32]).unwrap(),
                ),
            )
            .unwrap();
        }
    }
    p
}

#[test]
fn genesis_prefix_can_advance_twice_without_input_mutation() {
    let p = MainnetCompactCheckpoint::canonical_genesis();
    let old = p.identity();
    assert_eq!(p.height(), 0);
    assert_eq!(p.block_hash(), chain_identity::genesis(Network::Mainnet));
    assert_eq!(p.tree_sizes(), [0; 3]);
    let f = fixture(&p, false);
    let w = derive_fixture(&f, &p);
    let compact = blocks(&w);
    assert_eq!(compact.len(), 16);
    assert_eq!(p.identity(), old);
    assert_eq!(w.predecessor_identity(), old);
    assert_eq!(w.block_window().start_height(), 1);
    assert_eq!(w.block_window().end_height(), 16);
    assert_eq!(w.block_window().source_identity(), SOURCE);
    assert_eq!(w.derivation_policy(), MAINNET_COMPACT_WINDOW_POLICY);
    assert_eq!(w.payload_sha256(), hex(&Sha256::digest(w.encoded_blocks())));
    assert_eq!(w.successor.height(), 16);
    assert_eq!(w.successor.block_hash(), w.block_window().target_hash());
    assert_eq!(compact[0].hash, f.hashes[0]);
    assert_eq!(compact[0].prev_hash, display_to_protocol(p.block_hash()));
    assert_eq!(compact[0].height, 1);
    assert_eq!(compact[0].time, 1);
    assert!(compact[0].header.is_empty());
    assert!(compact[0].vtx[0].vin.is_empty());
    assert_eq!(compact[0].vtx[0].index, 0);
    assert_eq!(compact[0].vtx[1].index, 1);
    assert_eq!(compact[0].vtx[1].fee, 0);
    assert!(
        compact[0].vtx[1].vin
            == vec![CompactTxIn {
                prevout_txid: (0u8..32).collect(),
                prevout_index: 7
            }]
    );
    assert!(
        compact[0].vtx[1].vout
            == vec![TxOut {
                value: 77,
                script_pub_key: vec![0x51]
            }]
    );
    assert_eq!(
        compact[0].vtx[1].txid,
        transaction(1, false, false).txid().as_ref()
    );
    let f2 = fixture(&w.successor, false);
    let w2 = derive_fixture(&f2, &w.successor);
    assert_eq!(w2.successor.height(), 32);
    assert_eq!(w2.predecessor_identity(), w.successor.identity());
}
#[test]
fn all_pool_fields_match_literal_goldens_without_reversing_protocol_data() {
    let p = seeded(3_428_144, [1, 2, 3]);
    let old = p.identity();
    let f = fixture(&p, true);
    let w = derive_fixture(&f, &p);
    let c = blocks(&w);
    let t = &c[0].vtx[1];
    assert!(t.spends == vec![CompactSaplingSpend { nf: vec![0x55; 32] }]);
    assert!(
        t.outputs
            == vec![CompactSaplingOutput {
                cmu: vec![6; 32],
                ephemeral_key: vec![7; 32],
                ciphertext: vec![0x33; 52]
            }]
    );
    for (actions, pool) in [(&t.actions, 1u8), (&t.ironwood_actions, 2)] {
        assert!(
            actions
                == &vec![CompactOrchardAction {
                    nullifier: vec![pool; 32],
                    cmx: vec![pool + 2; 32],
                    ephemeral_key: orchard_epk().to_vec(),
                    ciphertext: vec![0x40 + pool; 52]
                }]
        );
    }
    assert!(
        t.vin
            == vec![CompactTxIn {
                prevout_txid: (0u8..32).collect(),
                prevout_index: 7
            }]
    );
    assert!(
        t.vout
            == vec![TxOut {
                value: 77,
                script_pub_key: vec![0x51]
            }]
    );
    assert_eq!(t.index, 1);
    assert_eq!(t.fee, 0);
    let mut output_golden = vec![0x0a, 32];
    output_golden.extend([6; 32]);
    output_golden.extend([0x12, 32]);
    output_golden.extend([7; 32]);
    output_golden.extend([0x1a, 52]);
    output_golden.extend([0x33; 52]);
    assert!(t.outputs[0].encode_to_vec() == output_golden);
    let mut input_golden = vec![0x0a, 32];
    input_golden.extend(0u8..32);
    input_golden.extend([0x10, 7]);
    assert!(t.vin[0].encode_to_vec() == input_golden);
    assert!(t.vout[0].encode_to_vec() == [0x08, 77, 0x12, 1, 0x51]);
    assert_eq!(t.txid, transaction(3_428_145, false, true).txid().as_ref());
    assert_eq!(p.identity(), old);
    assert_eq!(w.successor.tree_sizes(), [17, 18, 19]);
    assert!(
        c[0].chain_metadata
            == Some(ChainMetadata {
                sapling_commitment_tree_size: 2,
                orchard_commitment_tree_size: 3,
                ironwood_commitment_tree_size: 4
            })
    );
}
#[test]
fn nonzero_sequential_checkpoints_equal_an_ordered_prefix_fold() {
    let p = seeded(3_428_144, [1, 2, 3]);
    let f = fixture(&p, true);
    let w = derive_fixture(&f, &p);
    let f2 = fixture(&w.successor, true);
    let w2 = derive_fixture(&f2, &w.successor);
    let expected = seeded(3_428_176, [33, 34, 35]);
    assert_eq!(w2.successor.tree_sizes(), [33, 34, 35]);
    assert_eq!(w2.successor.tree_roots(), expected.tree_roots());
    assert_eq!(w2.predecessor_identity(), w.successor.identity());
    assert_eq!(p.tree_sizes(), [1, 2, 3]);
    assert_ne!(w.successor.tree_roots()[1], w.successor.tree_roots()[2]);
}
#[test]
fn upgrade_boundaries_follow_mainnet_branch_rules_and_distinct_pool_counts() {
    let boundaries: [(u32, [u32; 3], [u64; 3]); 3] = [
        (419_185, [0, 0, 0], [1, 0, 0]),
        (1_687_089, [15, 0, 0], [16, 1, 0]),
        (3_428_129, [14, 14, 0], [16, 16, 2]),
    ];
    for (start, before, after) in boundaries {
        let p = seeded(start - 1, [0; 3]);
        let f = fixture(&p, true);
        let w = derive_fixture(&f, &p);
        let c = blocks(&w);
        let b = c[13].chain_metadata.as_ref().unwrap();
        let actual_before = [
            b.sapling_commitment_tree_size,
            b.orchard_commitment_tree_size,
            b.ironwood_commitment_tree_size,
        ];
        if start == 419_185 {
            assert_eq!(actual_before, before);
        } else if start == 1_687_089 {
            assert_eq!(
                [
                    c[14].chain_metadata.unwrap().sapling_commitment_tree_size,
                    c[14].chain_metadata.unwrap().orchard_commitment_tree_size,
                    0
                ],
                before
            );
        } else {
            assert_eq!(actual_before, before);
        }
        assert_eq!(w.successor.tree_sizes(), after);
    }
}
#[test]
fn wrong_predecessor_gap_and_bad_final_block_return_no_output_or_checkpoint() {
    let p = MainnetCompactCheckpoint::canonical_genesis();
    let old = p.identity();
    let f = fixture(&p, false);
    let mut wrong = p.clone();
    wrong.hash = "11".repeat(32);
    assert!(
        derive_mainnet_compact_window(&f.frame, &f.accepted, &wrong, &AtomicBool::new(false))
            .is_err()
    );
    wrong = p.clone();
    wrong.height = 16;
    assert!(
        derive_mainnet_compact_window(&f.frame, &f.accepted, &wrong, &AtomicBool::new(false))
            .is_err()
    );
    let mut bad = f.frame.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert!(derive_mainnet_compact_window(&bad, &f.accepted, &p, &AtomicBool::new(false)).is_err());
    assert_eq!(p.identity(), old);
}
#[test]
fn overflowing_or_full_frontiers_are_refused_without_mutation() {
    use incrementalmerkletree::Position;
    let leaf = sapling::Node::from_cmu(
        &sapling::note::ExtractedNoteCommitment::from_bytes(&[6; 32]).unwrap(),
    );
    let mut near = SaplingFrontier::from_parts(
        Position::from(u64::from(u32::MAX) - 1),
        leaf,
        vec![leaf; 31],
    )
    .unwrap();
    let size = near.tree_size();
    assert!(checked_append(&mut near, leaf).is_err());
    assert_eq!(near.tree_size(), size);
    let mut full =
        SaplingFrontier::from_parts(Position::from(u64::from(u32::MAX)), leaf, vec![leaf; 32])
            .unwrap();
    assert!(checked_append(&mut full, leaf).is_err());
    assert_eq!(full.tree_size(), 1u64 << 32);
    let mut p = MainnetCompactCheckpoint::canonical_genesis();
    p.sapling = full;
    let f = fixture(&p, false);
    assert!(
        derive_mainnet_compact_window(&f.frame, &f.accepted, &p, &AtomicBool::new(false)).is_err()
    );
}
#[test]
fn encoding_budgets_discard_even_late_private_output() {
    let p = MainnetCompactCheckpoint::canonical_genesis();
    let old = p.identity();
    let f = fixture(&p, false);
    let successful = derive_fixture(&f, &p);
    for limits in [
        Limits {
            block: 1,
            window: MAINNET_COMPACT_WINDOW_MAX_BYTES,
        },
        Limits {
            block: MAINNET_COMPACT_BLOCK_MAX_BYTES,
            window: successful.encoded_blocks().len() - 1,
        },
    ] {
        assert!(derive(
            &f.frame,
            &f.accepted,
            &p,
            &AtomicBool::new(false),
            limits,
            &mut || Ok(())
        )
        .is_err());
        assert_eq!(p.identity(), old);
    }
}
#[test]
fn cancellation_during_actions_and_before_final_result_preserves_the_prefix() {
    let p = seeded(3_428_144, [1, 2, 3]);
    let f = fixture(&p, true);
    let old = p.identity();
    let cancel = AtomicBool::new(false);
    let tx = transaction(3_428_145, false, true);
    let mut scratch = p.clone();
    let mut calls = 0;
    let mut check = || {
        calls += 1;
        if calls == 7 {
            cancel.store(true, Ordering::Release);
        }
        current(&cancel)
    };
    let Err(error) = compact_transaction(&tx, 1, &mut scratch, &mut check) else {
        panic!("action cancellation must refuse output");
    };
    assert_eq!(error.to_string(), "native_compact_window_cancelled");
    assert_eq!(calls, 7);
    assert_eq!(p.identity(), old);
    let mut total = 0;
    derive(
        &f.frame,
        &f.accepted,
        &p,
        &AtomicBool::new(false),
        LIMITS,
        &mut || {
            total += 1;
            Ok(())
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    assert!(derive(&f.frame, &f.accepted, &p, &cancel, LIMITS, &mut || {
        calls += 1;
        if calls == total {
            cancel.store(true, Ordering::Release);
        }
        current(&cancel)
    })
    .is_err());
    assert_eq!(p.identity(), old);
    assert!(
        derive_mainnet_compact_window(&f.frame, &f.accepted, &p, &AtomicBool::new(true)).is_err()
    );
}
