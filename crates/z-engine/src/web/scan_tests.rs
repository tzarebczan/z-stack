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
