//! The small-update budget must preserve decryption, ordering and failure behavior.

use super::*;
use crate::keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC};
use crate::Network;
use orchard::{
    note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
    note_encryption::{
        IronwoodDomain, IronwoodNoteEncryption, OrchardDomain, OrchardNoteEncryption,
    },
    value::NoteValue,
    Note,
};
use zcash_note_encryption::Domain;

const BIRTHDAY: u32 = 1_000_001;

fn wallet(mnemonic: &str) -> WebWallet {
    WebWallet::from_account(
        account_from_mnemonic(mnemonic, Network::Regtest, 0).unwrap(),
        BIRTHDAY,
    )
    .unwrap()
}

/// Real note encryption; compact scanning does not require a transaction proof.
fn action(
    recipient: &WebWallet,
    value: u64,
    nullifier: [u8; 32],
    ironwood: bool,
) -> (CompactOrchardAction, [u8; 32]) {
    let ufvk = recipient.decode_ufvk().unwrap();
    let fvk = ufvk.orchard().unwrap();
    let rho = Rho::from_bytes(&nullifier).unwrap();
    let rseed = RandomSeed::from_bytes([7; 32], &rho).unwrap();
    let note = Note::from_parts(
        fvk.address_at(0u32, Scope::External),
        NoteValue::from_raw(value),
        rho,
        rseed,
        if ironwood {
            NoteVersion::V3
        } else {
            NoteVersion::V2
        },
    )
    .unwrap();
    let cmx = ExtractedNoteCommitment::from(note.commitment());
    let (ephemeral_key, ciphertext) = if ironwood {
        let enc = IronwoodNoteEncryption::new(None, note, [0; 512]);
        (
            IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
            enc.encrypt_note_plaintext()[..52].to_vec(),
        )
    } else {
        let enc = OrchardNoteEncryption::new(None, note, [0; 512]);
        (
            OrchardDomain::epk_bytes(enc.epk()).0.to_vec(),
            enc.encrypt_note_plaintext()[..52].to_vec(),
        )
    };
    (
        CompactOrchardAction {
            nullifier: nullifier.to_vec(),
            cmx: cmx.to_bytes().to_vec(),
            ephemeral_key,
            ciphertext,
        },
        note.nullifier(fvk).to_bytes(),
    )
}

fn chain(ironwood: bool) -> Vec<CompactBlock> {
    let owner = wallet(REGTEST_FAUCET_MNEMONIC);
    let stranger =
        wallet("legal winner thank year wave sausage worth useful legal winner thank yellow");
    let mut previous = vec![0; 32];
    let mut spent = [0; 32];
    (0..64u32)
        .map(|i| {
            let height = BIRTHDAY + i;
            let mut hash = vec![0; 32];
            hash[..4].copy_from_slice(&height.to_le_bytes());
            let mut block = CompactBlock {
                height: height.into(),
                hash: hash.clone(),
                prev_hash: previous.clone(),
                time: height,
                // Exercise the ordinary metadata-fill path too.
                chain_metadata: None,
                ..Default::default()
            };
            previous = hash.clone();
            let recipient_value = match i {
                0 => Some((&owner, 50_000)),
                15 => Some((&stranger, 7_000)),
                31 => Some((&owner, 30_000)),
                40 => Some((&stranger, 50_000)),
                63 => Some((&owner, 20_000)),
                _ => None,
            };
            if let Some((recipient, value)) = recipient_value {
                let mut nullifier = [0; 32];
                nullifier[0] = (i + 1) as u8;
                let (action, nf) = action(
                    recipient,
                    value,
                    if i == 40 { spent } else { nullifier },
                    ironwood,
                );
                if i == 0 {
                    spent = nf;
                }
                let mut tx = CompactTx {
                    index: 1,
                    txid: hash,
                    ..Default::default()
                };
                if ironwood {
                    tx.ironwood_actions.push(action);
                } else {
                    tx.actions.push(action);
                }
                block.vtx.push(tx);
            }
            block
        })
        .collect()
}

fn apply(wallet: &mut WebWallet, blocks: &[CompactBlock], chunk: usize) -> Vec<ScanDelta> {
    blocks
        .chunks(chunk)
        .flat_map(|blocks| {
            wallet
                .apply_compact_blocks_blob(&super::super::encode_delimited(blocks.iter().cloned()))
                .unwrap()
        })
        .collect()
}

#[test]
fn small_batches_stay_serial_inside_a_multicore_pool() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    pool.install(|| {
        for n in [0, 1, SERIAL_SCAN_MAX_BLOCKS] {
            let decrypts = Decrypts::new(Network::Regtest, n);
            assert!(!decrypts.parallel);
            assert!(decrypts.stream.is_none());
        }
        let catchup = Decrypts::new(Network::Regtest, SERIAL_SCAN_MAX_BLOCKS + 1);
        assert!(catchup.parallel, "catch-up retains multicore decryption");
        assert!(
            catchup.stream.is_none(),
            "a pool thread cannot wait on its own stream"
        );
    });
}

#[test]
fn serial_and_parallel_batches_preserve_notes_spends_and_snapshot() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    for ironwood in [false, true] {
        let blocks = chain(ironwood);
        let (serial, parallel, serial_deltas, parallel_deltas) = pool.install(|| {
            let mut serial = wallet(REGTEST_FAUCET_MNEMONIC);
            let mut parallel = wallet(REGTEST_FAUCET_MNEMONIC);
            let serial_deltas = apply(&mut serial, &blocks, SERIAL_SCAN_MAX_BLOCKS);
            let parallel_deltas = apply(&mut parallel, &blocks, blocks.len());
            (serial, parallel, serial_deltas, parallel_deltas)
        });
        assert_eq!(
            serde_json::to_value(&serial_deltas).unwrap(),
            serde_json::to_value(&parallel_deltas).unwrap()
        );
        assert_eq!(serial_deltas.iter().map(|d| d.notes_found).sum::<u32>(), 3);
        assert_eq!(serial_deltas.iter().map(|d| d.spends_found).sum::<u32>(), 1);
        assert_eq!(
            serial
                .notes()
                .iter()
                .filter(|note| !note.spent)
                .map(|note| note.value_zat)
                .sum::<u64>(),
            50_000
        );
        assert_eq!(serial.scanned_height(), BIRTHDAY + 63);
        let snapshot = |w: &WebWallet| {
            serde_json::from_slice::<serde_json::Value>(&w.to_snapshot().unwrap()).unwrap()
        };
        assert_eq!(snapshot(&serial), snapshot(&parallel));
        assert_eq!(serial.history_json(10), parallel.history_json(10));

        // The usual browser/native call is outside the pool: also compare its
        // streaming catch-up path to the bounded serial path.
        let mut streaming = wallet(REGTEST_FAUCET_MNEMONIC);
        apply(&mut streaming, &blocks, blocks.len());
        assert_eq!(snapshot(&serial), snapshot(&streaming));
    }
}

#[test]
fn serial_and_parallel_batches_reject_bad_shielded_data_and_chain_order() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let valid = chain(false);
    for defect in ["ciphertext", "height", "previous hash"] {
        let mut errors = Vec::new();
        for n in [SERIAL_SCAN_MAX_BLOCKS, SERIAL_SCAN_MAX_BLOCKS + 1] {
            let mut blocks = valid[..n].to_vec();
            match defect {
                "ciphertext" => {
                    blocks[0].vtx[0].actions[0].ciphertext.pop();
                }
                "height" => blocks[5].height += 1,
                _ => blocks[5].prev_hash[0] ^= 1,
            }
            pool.install(|| {
                let mut w = wallet(REGTEST_FAUCET_MNEMONIC);
                let error = w
                    .apply_compact_blocks_blob(&super::super::encode_delimited(blocks))
                    .unwrap_err();
                errors.push(error.to_string());
                assert_eq!(w.scanned_height(), BIRTHDAY - 1);
                assert!(w.notes().is_empty());
                assert!(
                    w.ensure_scan_healthy().is_err(),
                    "invalid scans cannot publish a usable wallet"
                );
            });
        }
        assert_eq!(errors[0], errors[1], "{defect}");
    }
}

#[test]
fn observed_spend_before_same_block_receipt_is_not_wallet_construction() {
    for ironwood in [false, true] {
        let mut w = wallet(REGTEST_FAUCET_MNEMONIC);
        let (first, nf) = action(
            &w,
            700,
            {
                let mut n = [0; 32];
                n[0] = 1;
                n
            },
            ironwood,
        );
        let (second, _) = action(&w, 500, nf, ironwood);
        let mut blocks = Vec::new();
        for (offset, a) in [first, second].into_iter().enumerate() {
            let mut tx = CompactTx {
                txid: vec![offset as u8 + 1; 32],
                ..Default::default()
            };
            if ironwood {
                tx.ironwood_actions.push(a);
            } else {
                tx.actions.push(a);
            }
            blocks.push(CompactBlock {
                height: u64::from(BIRTHDAY) + offset as u64,
                hash: vec![offset as u8 + 1; 32],
                prev_hash: vec![offset as u8; 32],
                vtx: vec![tx],
                ..Default::default()
            });
        }
        apply(&mut w, &blocks, 1);
        w.recompute_pool_fields();
        let row = w
            .history(20)
            .into_iter()
            .find(|r| r.txid == WebWallet::display_txid(&"02".repeat(32)))
            .unwrap();
        assert_eq!((row.spent_zat, row.sent_note_count), (700, 1));
        assert_eq!(
            (row.received_zat, row.received_note_count),
            (500, 1),
            "passive scan spend is not a constructed transaction"
        );
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        let row = cold
            .history(20)
            .into_iter()
            .find(|r| r.txid == WebWallet::display_txid(&"02".repeat(32)))
            .unwrap();
        assert_eq!((row.received_zat, row.received_note_count), (500, 1));
    }
}

#[cfg(feature = "native")]
#[test]
fn old_wallet_write_pending_change_keeps_accounting_through_cold_confirmation() {
    use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
    use zcash_client_backend::wallet::{Note as WalletNote, Recipient};
    use zcash_primitives::transaction::Transaction;
    use zcash_protocol::{
        consensus::{BlockHeight, BranchId},
        value::Zatoshis,
    };
    let mut w = wallet(REGTEST_FAUCET_MNEMONIC);
    let raw = crate::web::shielded_memo_transaction(&w, &w, "saved change", 30000, true);
    let tx = Transaction::read(&raw[..], BranchId::Nu6_3).unwrap();
    let keys = std::collections::HashMap::from([(0u32, w.decode_ufvk().unwrap())]);
    let decrypted = zcash_client_backend::decrypt_transaction(
        &Network::Regtest,
        None,
        Some(BlockHeight::from_u32(BIRTHDAY)),
        &tx,
        &keys,
    );
    let o = &decrypted.ironwood_outputs()[0];
    let outputs = [SentTransactionOutput::from_parts(
        o.index(),
        Recipient::InternalShielded {
            receiving_account: 0,
            external_address: None,
            note: Box::new(WalletNote::Orchard {
                note: o.note().0.clone(),
                pool: o.note().1,
            }),
        },
        Zatoshis::from_u64(30000).unwrap(),
        None,
    )];
    let sent = SentTransaction::new(
        &tx,
        std::time::UNIX_EPOCH.into(),
        BIRTHDAY.into(),
        0,
        &outputs,
        Zatoshis::from_u64(10000).unwrap(),
        &[],
    );
    w.store_transactions_to_be_sent(&[sent]).unwrap();
    let pending = w.pending_txs.clone();
    let id = to_hex(tx.txid().as_ref());
    let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
    old["txs"][&id]
        .as_object_mut()
        .unwrap()
        .remove("receiptTotalsRecorded");
    let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(cold.pending_txs, pending);
    assert_eq!(
        cold.balance().ironwood_pending,
        30000,
        "saved signed construction preserves its pool before confirmation"
    );
    assert_eq!(cold.balance().orchard_pending, 0);
    let block = cold.mined_raw_tx_block(&raw, 1).unwrap();
    cold.apply_compact_blocks_blob(&super::super::encode_delimited([block]))
        .unwrap();
    cold.recompute_pool_fields();
    let row = cold
        .history_matching(1, None, Some(&tx.txid().to_string()))
        .remove(0);
    assert_eq!((row.received_zat, row.received_note_count), (30000, 1));
    let reopened = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
    let row = reopened
        .history_matching(1, None, Some(&tx.txid().to_string()))
        .remove(0);
    assert_eq!((row.received_zat, row.received_note_count), (30000, 1));
    assert_eq!(reopened.pending_txs, pending);
}

#[cfg(all(feature = "native", feature = "transparent-pir"))]
#[test]
fn public_pir_spend_transfer_then_real_receipt_uses_independent_accounting() {
    use crate::transparent_pir::acquisition::Coverage;
    use crate::transparent_pir::{
        TransparentPirCheckpoint, TransparentPirRecord, TransparentPirScope,
    };
    use transparent_events::{ReceiveEvent, SpendEvent, TransparentEvent, Txid};
    use zcash_client_backend::proto::compact_formats::CompactTxIn;
    use zcash_primitives::transaction::{Authorized, Transaction, TransactionData};
    use zcash_protocol::consensus::{BlockHeight, BranchId};
    let mut decryptor = wallet(REGTEST_FAUCET_MNEMONIC);
    let raw = crate::web::shielded_memo_transaction(
        &decryptor,
        &decryptor,
        "independent receipt",
        30000,
        true,
    );
    let template = Transaction::read(&raw[..], BranchId::Nu6_3).unwrap();
    // One parsed transaction supplies both the real encrypted action and its
    // actual public input. Proof/signature bytes are inert; no consensus claim.
    let tx = TransactionData::<Authorized>::from_parts_v6(
        BranchId::Nu6_3,
        0,
        BlockHeight::from_u32(BIRTHDAY + 20),
        Some(transparent::bundle::Bundle {
            vin: vec![transparent::bundle::TxIn::from_parts(
                transparent::bundle::OutPoint::new([1; 32], 0),
                transparent::address::Script::default(),
                u32::MAX,
            )],
            vout: vec![],
            authorization: transparent::bundle::Authorized,
        }),
        None,
        None,
        template.ironwood_bundle().cloned(),
    )
    .freeze()
    .unwrap();
    let mut raw = Vec::new();
    tx.write(&mut raw).unwrap();
    let id = to_hex(tx.txid().as_ref());
    let mut block = decryptor.mined_raw_tx_block(&raw, 1).unwrap();
    // The scanner independently trial-decrypts the real note before its later
    // ingestion boundary is exercised in the target wallet.
    decryptor
        .apply_compact_blocks_blob(&super::super::encode_delimited([block.clone()]))
        .unwrap();
    decryptor.recompute_pool_fields();
    let note = decryptor
        .notes()
        .iter()
        .find(|n| n.txid == id)
        .unwrap()
        .clone();
    assert_eq!(note.value_zat, 30000);
    block.vtx[0].vin = vec![CompactTxIn {
        prevout_txid: vec![1; 32],
        prevout_index: 0,
    }];
    let mut w = wallet(REGTEST_FAUCET_MNEMONIC);
    w.set_scanned(
        BIRTHDAY + 2,
        WebWallet::make_prior(
            BIRTHDAY + 2,
            zcash_primitives::block::BlockHash([3; 32]),
            0,
            0,
            0,
        ),
    )
    .unwrap();
    for (offset, byte) in [(0, 1), (1, 2), (2, 3)] {
        w.block_hashes
            .insert(BIRTHDAY + offset, format!("{byte:02x}").repeat(32));
    }
    w.block_hashes.insert(BIRTHDAY - 1, "00".repeat(32));
    let scope: TransparentPirScope =
        serde_json::from_str(&w.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
    let script = to_hex(&w.public_transparent_scripts().unwrap()[0].0);
    let events = [
        TransparentEvent::Receive(ReceiveEvent {
            metadata: None,
            height: BIRTHDAY - 1,
            txid: Txid([1; 32]),
            transaction_index: 1,
            output_index: 0,
            value: 150000,
            coinbase: false,
        }),
        TransparentEvent::Spend(SpendEvent {
            metadata: None,
            height: BIRTHDAY,
            spending_txid: Txid(*tx.txid().as_ref()),
            transaction_index: 1,
            input_index: 0,
            spent_txid: Txid([1; 32]),
            spent_output_index: 0,
        }),
    ];
    let cp = TransparentPirCheckpoint {
        source_id: scope.source_id,
        generation: scope.generation,
        scope_id: scope.scope_id,
        anchor: scope.anchor,
        checkpoint_id: "cd".repeat(32),
        previous_checkpoint_id: None,
        covered_through: u64::from(BIRTHDAY + 2),
        complete: true,
        pending_pages: 0,
        unresolved_spends: 0,
        records: events
            .into_iter()
            .map(|e| TransparentPirRecord {
                script: script.clone(),
                event: to_hex(&e.to_bytes()),
                shard_id: 0,
                revision: "ef".repeat(32),
            })
            .collect(),
        coverage: scope
            .scripts
            .into_iter()
            .map(|s| Coverage {
                script: s.script,
                required_from: s.required_from,
                through: u64::from(BIRTHDAY + 2),
                revision: "ef".repeat(32),
            })
            .collect(),
        rewind_to: None,
        continuation: String::new(),
    };
    w.apply_transparent_pir_checkpoint(&serde_json::to_string(&cp).unwrap())
        .unwrap();
    w.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
        .unwrap();
    let mut blocks: Vec<_> = (0..3)
        .map(|i| CompactBlock {
            height: u64::from(BIRTHDAY + i),
            hash: vec![i as u8 + 1; 32],
            prev_hash: vec![i as u8; 32],
            vtx: vec![CompactTx {
                index: 0,
                txid: vec![100 + i as u8; 32],
                ..Default::default()
            }],
            ..Default::default()
        })
        .collect();
    blocks[0].vtx.push(block.vtx.remove(0));
    w.apply_transparent_blocks(&super::super::encode_delimited(blocks))
        .unwrap();
    assert_eq!(
        (w.txs()[&id].spent_zat, w.txs()[&id].spent_notes),
        (150000, 1)
    );
    assert_eq!(
        (w.txs()[&id].pir_spent_zat, w.txs()[&id].pir_spent_notes),
        (0, 0)
    );
    assert!(w.pending_txs.is_empty());
    // This is an ingestion-boundary test, not a claim that normal scheduling
    // scans transparent blocks before shielded blocks at a new height. The
    // public range above is checked against the target's accepted saved headers.
    w.push_note(note.clone());
    w.recompute_pool_fields();
    w.push_note(note);
    let assert_history = |wallet: &WebWallet| {
        let row = wallet
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!((row.received_zat, row.received_note_count), (30000, 1));
        assert_eq!((row.spent_zat, row.sent_note_count), (150000, 1));
        assert_eq!(row.account_delta_zat, -120000);
    };
    assert_history(&w);
    assert_history(&WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap());
}
