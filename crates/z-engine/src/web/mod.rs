//! WASM-safe compact-block wallet: trial-decrypt, snapshot, `HistoryEntry`.
//!
//! Not a `WalletWrite` backend. Spend/prove wait on `zakura-client-memory`.
//! Compact-block **fetch** is the app's job (gRPC-Web or `/lwd/*` proxy).

mod framed;
#[cfg(feature = "hardware")]
pub mod hardware;
#[cfg(feature = "hardware")]
pub use hardware::{
    hardware_apply_signatures, hardware_combine, hardware_prove, hardware_signer_copy,
    sign_pczt_with_mnemonic, DeviceSignature, FinalizedTransaction, SignerCopy,
};
mod history;
mod live_trees;
mod prove;
mod public_data;
mod scan;
mod store;
mod write;

pub use framed::{decode_delimited, encode_delimited, encode_one, SNAPSHOT_MAGIC};
pub use prove::{orchard_proving_key_ready, warm_orchard_proving_key, SHIELD_NOT_READY};
pub use scan::ScanDelta;
pub use store::{HardwareAccount, HistoryOutpoint, HistoryOutput, TreeFinalizeTick, WebWallet};

/// Kept for SDK compatibility; orchard send now uses `WebWallet::prove_send`.
pub const PROVE_NOT_READY: &str =
    "wasm snapshot wallet cannot prove yet (needs WalletWrite + orchard notes)";

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("odd hex length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let nibble = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err("invalid hex".into()),
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        out.push((nibble(bytes[i])? << 4) | nibble(bytes[i + 1])?);
        i += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC};
    use crate::Network;
    use prost::Message;
    use zcash_client_backend::proto::compact_formats::{ChainMetadata, CompactBlock};

    fn empty_block(height: u64, prev_hash: Vec<u8>) -> CompactBlock {
        let mut hash = vec![0u8; 32];
        hash[0] = height as u8;
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

    #[test]
    fn estimate_fee_and_max_send_empty_wallet() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct.clone(), 1).unwrap();
        let (max, fee) = w.max_send(None).unwrap();
        assert_eq!(max, 0);
        assert_eq!(fee, 0);
        let err = w
            .estimate_fee(&acct.unified_address, "0.0005", None)
            .unwrap_err();
        assert!(
            matches!(err, crate::error::EngineError::SyncRequired)
                || err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("insufficient")
                || err.to_string().to_ascii_lowercase().contains("sync")
        );
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        let err2 = w
            .estimate_fee(&acct.unified_address, "0.0005", None)
            .unwrap_err();
        assert!(matches!(err2, crate::error::EngineError::InsufficientFunds));
        let (max2, fee2) = w.max_send(None).unwrap();
        assert_eq!((max2, fee2), (0, 0));
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(from_hex(&to_hex(&[0xde, 0xad])).unwrap(), vec![0xde, 0xad]);
    }

    #[test]
    fn delimited_roundtrip() {
        let blob = encode_delimited([empty_block(1, vec![0u8; 32]), empty_block(2, vec![0u8; 32])]);
        let blocks = decode_delimited(&blob).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].height, 2);
        let encoded = blocks[0].encode_to_vec();
        let one = CompactBlock::decode(encoded.as_slice()).unwrap();
        assert_eq!(one.height, 1);
    }

    #[test]
    fn snapshot_and_scan_empty_blocks() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        assert_eq!(w.scanned_height(), 0);
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        assert_eq!(w.scanned_height(), 2);

        let mut w3 = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            1,
        )
        .unwrap();
        let blob = encode_delimited([b1.clone(), b2.clone()]);
        let deltas = w3.apply_compact_blocks_blob(&blob).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!(w3.scanned_height(), 2);
        assert!(w.history(10).is_empty());
        assert_eq!(w.balance().total_available, 0);

        let mut many = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            1,
        )
        .unwrap();
        let mut prev = vec![0u8; 32];
        let mut chain = Vec::new();
        for h in 1u64..=64 {
            let b = empty_block(h, prev);
            prev = b.hash.clone();
            chain.push(b);
        }
        let blob64 = encode_delimited(chain);
        let d64 = many.apply_compact_blocks_blob(&blob64).unwrap();
        assert_eq!(d64.len(), 64);
        assert_eq!(many.scanned_height(), 64);
        assert!(
            !many.sinsemilla_live,
            "cold blob apply should defer Sinsemilla until finalize"
        );
        many.finalize_scan_trees().unwrap();
        assert!(many.sinsemilla_live);
        assert_eq!(many.wallet_snapshot("wasm")["treesReady"], true);
        assert_eq!(many.wallet_snapshot("wasm")["spendReady"], false);

        let bytes = w.to_snapshot().unwrap();
        let w2 = WebWallet::from_snapshot(&bytes).unwrap();
        assert_eq!(w2.scanned_height(), 2);
        assert!(
            !w2.sinsemilla_live,
            "hydrate must not replay shardtrees; finalize/prove does"
        );
        assert_eq!(w2.unified_address(), w.unified_address());
        let hist = w2.history_json(5);
        assert!(hist.is_array());
        assert_eq!(hist.as_array().unwrap().len(), 0);
    }

    fn empty_block_no_meta(height: u64, prev_hash: Vec<u8>) -> CompactBlock {
        let mut hash = vec![0u8; 32];
        hash[0] = height as u8;
        CompactBlock {
            height,
            hash,
            prev_hash,
            time: 1,
            header: vec![],
            vtx: vec![],
            chain_metadata: None,
        }
    }

    #[test]
    fn blob_without_chain_metadata_skips_trial_decrypt() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block_no_meta(1, vec![0u8; 32]);
        let b2 = empty_block_no_meta(2, b1.hash.clone());
        let blob = encode_delimited([b1, b2]);
        let deltas = w.apply_compact_blocks_blob(&blob).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!(w.scanned_height(), 2);
        assert!(
            !w.sinsemilla_live,
            "empty compact blocks must not live-hash Sinsemilla"
        );
    }

    #[test]
    fn spend_ready_requires_birthday_frontier() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let late = WebWallet::from_account(acct.clone(), 100).unwrap();
        assert!(!late.trees_ready());
        assert_eq!(late.wallet_snapshot("wasm")["treesReady"], false);
        assert_eq!(late.wallet_snapshot("wasm")["spendReady"], false);
        let genesis = WebWallet::from_account(acct, 1).unwrap();
        assert!(genesis.trees_ready());
        assert_eq!(genesis.wallet_snapshot("wasm")["treesReady"], true);
        assert_eq!(genesis.wallet_snapshot("wasm")["spendReady"], false);
    }

    #[test]
    fn history_json_shape() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let w = WebWallet::from_account(acct, 1).unwrap();
        let snap = w.wallet_snapshot("wasm");
        assert_eq!(snap["network"], "regtest");
        assert_eq!(snap["birthdayHeight"], 1);
        assert!(snap["unifiedAddress"]
            .as_str()
            .unwrap()
            .starts_with("uregtest1"));
        assert_eq!(snap["balance"]["totalAvailable"], 0);
        assert_eq!(snap["balance"]["totalPending"], 0);
        assert_eq!(snap["confirmations"]["trusted"], 1);
        assert_eq!(snap["confirmations"]["untrusted"], 1);
        assert_eq!(snap["confirmations"]["zeroConfShield"], true);
    }

    #[test]
    fn prove_send_needs_funds_and_matching_seed() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let ua = acct.unified_address.clone();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        let err = w
            .prove_send(REGTEST_FAUCET_MNEMONIC, &ua, "0.0001", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.to_ascii_lowercase().contains("insufficient") || err.contains("SyncRequired"),
            "unexpected: {err}"
        );
        let other = "legal winner thank year wave sausage worth useful legal winner thank yellow";
        let wrong = w
            .prove_send(other, &ua, "0.0001", None)
            .unwrap_err()
            .to_string();
        assert!(
            wrong.contains("match") || wrong.contains("invalid mnemonic"),
            "unexpected: {wrong}"
        );
        assert!(w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());

        let n = w
            .apply_utxos_json(
                r#"{"utxos":[{"txid":"aa","index":0,"script":"","valueZat":100000,"height":1,"address":""}]}"#,
            )
            .unwrap();
        assert_eq!(n, 1);
        assert!(w.balance().transparent_available >= 100000);
        assert!(w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());
    }

    #[cfg(feature = "transparent-inputs")]
    #[test]
    fn prove_shield_faucet_p2pkh() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let mut prev = vec![0u8; 32];
        for h in 1..=2u64 {
            let b = empty_block(h, prev);
            prev = b.hash.clone();
            w.apply_compact_block(&b.encode_to_vec()).unwrap();
        }
        w.apply_utxos_json(
            r#"{"utxos":[{"txid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
        )
        .unwrap();
        let raw = w
            .prove_shield(REGTEST_FAUCET_MNEMONIC, 1)
            .unwrap_or_else(|e| panic!("prove_shield: {e}"));
        assert!(raw.len() > 100);
        assert!(!w.memo_enhancement_txids(40).is_empty());
        assert_eq!(
            w.enhance_raw_tx(&raw).unwrap(),
            0,
            "empty memo is a successful enhancement"
        );
        assert!(w.memo_enhancement_txids(40).is_empty());
        let reloaded = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(reloaded.memo_enhancement_txids(40).is_empty());
        let pending = w.balance();
        assert_eq!(pending.transparent_available, 0);
        assert!(
            pending.orchard_pending > 0,
            "shield change is pending until mined"
        );
        let hist = w.history(5);
        let shield = hist
            .iter()
            .find(|h| h.is_shielding)
            .expect("pending shield in history");
        assert!(hist.iter().any(|h| h.transparent_received > 0));
        assert!(shield.mined_height.is_none());
        assert!(shield.fee_zat.is_some());
        assert!(shield.expiry_height.is_some());
        assert_eq!(shield.status(), "pending");

        let n = w
            .apply_utxos_json(
                r#"{"utxos":[{"txid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","index":0,"script":"76a914d3c0870e8e13a9ec320f1280889127aa15a4c0a188ac","valueZat":625000000,"height":1,"address":"tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"}]}"#,
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            w.balance().transparent_available,
            0,
            "locally spent UTXO stays spent when the server still lists it"
        );

        let acct2 = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w2 = WebWallet::from_account(acct2, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w2.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w2.apply_compact_block(&b2.encode_to_vec()).unwrap();
        let hits = w2.apply_raw_tx(&raw).unwrap();
        assert!(hits > 0);
        assert!(
            w2.balance().orchard_pending > 0,
            "mempool inbound orchard shows as pending"
        );

        let pending_orchard = pending.orchard_pending;
        let mined = w
            .apply_mined_raw_tx(&raw, 1_700_000_000)
            .unwrap_or_else(|e| panic!("mine pending shield: {e}"));
        assert_eq!(mined.height, 3);
        assert!(w.cached_scan_keys.is_some());
        let after = w.balance();
        assert!(
            after.orchard_available >= pending_orchard,
            "mined shield change should be spendable under MIN policy (avail={} pending_before={})",
            after.orchard_available,
            pending_orchard
        );
        assert_eq!(
            after.orchard_pending, 0,
            "pending orchard should clear once the compact block is applied"
        );
        let hist = w.history(5);
        assert_eq!(hist[0].status(), "mined");
        assert_eq!(hist[0].mined_height, Some(3));
        assert_eq!(hist[0].block_time, Some(1_700_000_000));
        assert_eq!(hist[0].confirmations, Some(1));
    }

    #[test]
    fn pending_balance_from_unmined_send() {
        use super::store::TxAgg;
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        w.txs_mut().insert(
            "cafebabe".into(),
            TxAgg {
                mined_height: None,
                received_zat: 50_000,
                spent_zat: 60_000,
                received_notes: 1,
                spent_notes: 1,
                has_change: true,
                is_shielding: false,
                fee_zat: Some(10_000),
                expiry_height: None,
                memos: vec![],
                block_time: None,
                ..Default::default()
            },
        );
        let b = w.balance();
        assert_eq!(b.orchard_pending, 50_000);
        assert_eq!(b.orchard_available, 0);
        assert_eq!(b.total_pending, 50_000);
    }

    #[test]
    fn history_marks_expired_unmined() {
        use super::store::TxAgg;
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.txs_mut().insert(
            "deadbeef".into(),
            TxAgg {
                mined_height: None,
                received_zat: 1,
                spent_zat: 2,
                received_notes: 0,
                spent_notes: 1,
                has_change: false,
                is_shielding: false,
                fee_zat: Some(1),
                expiry_height: Some(1),
                memos: vec![],
                block_time: None,
                ..Default::default()
            },
        );
        let h = w.history(5);
        assert_eq!(h.len(), 1);
        assert!(h[0].expired_unmined);
        assert_eq!(h[0].status(), "expired");
        assert_eq!(h[0].fee_zat, Some(1));
    }

    #[test]
    fn history_includes_transparent_receive_and_pool_fields() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        w.apply_utxos_json(
            r#"{"utxos":[{"txid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","index":0,"script":"","valueZat":100000,"height":12,"address":""}]}"#,
        )
        .unwrap();
        let hist = w.history(5);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].transparent_received, 100_000);
        assert_eq!(hist[0].received_zat, 100_000);
        assert_eq!(hist[0].mined_height, Some(12));
        let snap = w.wallet_snapshot("wasm");
        assert!(snap["transactions"].as_array().unwrap().len() >= 1);
    }

    #[test]
    fn attach_seed_rejects_wrong_mnemonic() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let ufvk = acct.ufvk.clone();
        let mut w = WebWallet::from_ufvk(Network::Regtest, &ufvk, 1, 0).unwrap();
        assert!(w.view_only());
        w.attach_seed(REGTEST_FAUCET_MNEMONIC).unwrap();
        assert!(!w.view_only());
        let other = "legal winner thank year wave sausage worth useful legal winner thank yellow";
        let mut w2 = WebWallet::from_ufvk(Network::Regtest, &ufvk, 1, 0).unwrap();
        let err = w2.attach_seed(other).unwrap_err().to_string();
        assert!(err.contains("match"), "unexpected: {err}");
    }

    #[test]
    fn reject_transparent_send() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let t = acct.transparent_address.clone().unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        let err = w
            .prove_send(REGTEST_FAUCET_MNEMONIC, &t, "0.0001", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("transparent send is not supported"),
            "unexpected: {err}"
        );
        let ua = w.unified_address().to_string();
        let multi = crate::zip321_uri_many(&[
            crate::Zip321Payment {
                address: ua,
                amount_zec: Some("0.0001".into()),
                ..Default::default()
            },
            crate::Zip321Payment {
                address: t,
                amount_zec: Some("0.0001".into()),
                ..Default::default()
            },
        ])
        .unwrap();
        let err2 = w
            .prove_send(REGTEST_FAUCET_MNEMONIC, &multi, "", None)
            .unwrap_err()
            .to_string();
        assert!(
            err2.contains("transparent send is not supported"),
            "unexpected: {err2}"
        );
    }

    #[test]
    fn rewind_drops_later_scan() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        assert_eq!(w.scanned_height(), 2);
        w.rewind_to_height(1).unwrap();
        assert_eq!(w.scanned_height(), 1);
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        assert_eq!(w.scanned_height(), 2);
    }

    #[test]
    fn reset_scan_clears_history_keeps_keys() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let ua = acct.unified_address.clone();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.apply_utxos_json(
            r#"{"utxos":[{"txid":"aa","index":0,"script":"","valueZat":100000,"height":1,"address":""}]}"#,
        )
        .unwrap();
        assert_eq!(w.scanned_height(), 2);
        assert!(!w.history(5).is_empty());
        w.reset_scan();
        assert_eq!(w.scanned_height(), 0);
        assert!(w.history(5).is_empty());
        assert_eq!(w.unified_address(), ua);
        assert_eq!(w.balance().total_available, 0);
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        assert_eq!(w.scanned_height(), 2);
    }

    #[test]
    fn ufvk_wallet_is_view_only() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let w = WebWallet::from_ufvk(Network::Regtest, &acct.ufvk, 1, 0).unwrap();
        assert!(w.view_only());
        assert_eq!(w.unified_address(), acct.unified_address);
    }

    #[test]
    fn ufvk_matching_seed_is_not_rejected_as_view_only() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_ufvk(Network::Regtest, &acct.ufvk, 1, 0).unwrap();
        let err = w
            .prove_send(REGTEST_FAUCET_MNEMONIC, &acct.unified_address, "0.01", None)
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("view-only"),
            "matching seed must be allowed to prove; got {err}"
        );
    }

    #[test]
    fn ufvk_wrong_seed_is_rejected() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_ufvk(Network::Regtest, &acct.ufvk, 1, 0).unwrap();
        let other = "legal winner thank year wave sausage worth useful legal winner thank yellow";
        let err = w
            .prove_send(other, &acct.unified_address, "0.01", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.to_ascii_lowercase().contains("match"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn duplicate_block_is_skipped() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 1).unwrap();
        let b1 = empty_block(1, vec![0u8; 32]);
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        assert_eq!(w.scanned_height(), 1);
    }
}
