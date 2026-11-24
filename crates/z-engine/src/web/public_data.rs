//! Wallet-independent block-range data. Coverage is separate from shielded scan
//! progress so an old snapshot never claims it has scanned transparent activity.

use super::store::{CoinbaseStatus, StoredUtxo};
use super::{decode_delimited, to_hex, WebWallet};
use crate::error::{EngineError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use zcash_client_backend::proto::compact_formats::CompactBlock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScanCoverage {
    pub height: u32,
    pub hash: String,
}

#[cfg(all(test, feature = "transparent-inputs"))]
mod tests {
    use super::super::encode_delimited;
    use super::*;
    use crate::{
        keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC},
        Network,
    };
    use zcash_client_backend::proto::compact_formats::{
        ChainMetadata, CompactTx, CompactTxIn, TxOut,
    };
    use zcash_keys::encoding::AddressCodec;

    fn wallet() -> WebWallet {
        WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            1,
        )
        .unwrap()
    }

    fn block(height: u32) -> CompactBlock {
        CompactBlock {
            height: height as u64,
            hash: vec![height as u8; 32],
            prev_hash: vec![(height - 1) as u8; 32],
            time: height,
            header: vec![],
            chain_metadata: Some(ChainMetadata::default()),
            vtx: vec![CompactTx {
                index: 0,
                txid: vec![(height + 100) as u8; 32],
                ..Default::default()
            }],
        }
    }

    fn output(address: &str, value: u64) -> TxOut {
        let address =
            transparent::address::TransparentAddress::decode(&Network::Regtest, address).unwrap();
        let script: transparent::address::Script = address.script().into();
        TxOut {
            value,
            script_pub_key: script.0 .0,
        }
    }

    fn receive(w: &WebWallet, height: u32) -> CompactBlock {
        let mut b = block(height);
        b.vtx.push(CompactTx {
            index: 1,
            txid: vec![height as u8; 32],
            vout: vec![output(w.transparent_address().unwrap(), 70_000)],
            ..Default::default()
        });
        b
    }

    fn scan(w: &mut WebWallet, blocks: Vec<CompactBlock>) {
        let blob = encode_delimited(blocks);
        w.apply_compact_blocks_blob(&blob).unwrap();
        w.apply_transparent_blocks(&blob).unwrap();
    }

    #[test]
    fn detects_spends_and_reorgs_without_address_queries() {
        let mut w = wallet();
        let first = receive(&w, 1);
        scan(&mut w, vec![first]);
        assert_eq!(w.balance().transparent_available, 70_000);
        let mut spend = block(2);
        spend.vtx.push(CompactTx {
            index: 1,
            txid: vec![8; 32],
            vin: vec![CompactTxIn {
                prevout_txid: vec![1; 32],
                prevout_index: 0,
            }],
            ..Default::default()
        });
        scan(&mut w, vec![spend]);
        assert_eq!(w.balance().transparent_available, 0);
        assert!(w.utxos[0].spent);
        w.rewind_to_height(1).unwrap();
        assert!(!w.utxos[0].spent);
        assert_eq!(w.transparent_next_height(), 2);
        assert_eq!(w.balance().transparent_available, 70_000);
        scan(&mut w, vec![block(2)]);
        assert_eq!(w.balance().transparent_available, 70_000);
    }

    #[test]
    fn legacy_snapshot_backfills_and_persists_separate_coverage() {
        let mut w = wallet();
        let b = receive(&w, 1);
        let all = encode_delimited(vec![b.clone(), block(2)]);
        w.apply_compact_blocks_blob(&all).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("transparentScan");
        json.as_object_mut().unwrap().remove("memoScan");
        let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(restored.transparent_next_height(), 1);
        restored
            .apply_transparent_blocks(&encode_delimited(vec![b]))
            .unwrap();
        assert_eq!(
            restored.balance().transparent_available,
            0,
            "incomplete coverage cannot shield stale outputs"
        );
        assert_eq!(restored.balance().transparent_pending, 70_000);
        let mut restored = WebWallet::from_snapshot(&restored.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.transparent_next_height(), 2);
        restored
            .apply_transparent_blocks(&encode_delimited(vec![block(2)]))
            .unwrap();
        assert_eq!(restored.balance().transparent_available, 70_000);
        restored.reset_scan();
        assert_eq!(restored.transparent_next_height(), 1);
    }

    #[test]
    fn rejects_incomplete_wrong_chain_and_malformed_ranges_before_mutation() {
        let mut w = wallet();
        let b = receive(&w, 1);
        w.apply_compact_blocks_blob(&encode_delimited(vec![b.clone(), block(2)]))
            .unwrap();
        let before = w.to_snapshot().unwrap();
        let mut absent = b.clone();
        absent.vtx.remove(0);
        let mut fork = b.clone();
        fork.hash = vec![9; 32];
        let mut invalid = b.clone();
        invalid.vtx[1].vout[0].value = u64::MAX;
        let mut trailing_invalid = block(2);
        trailing_invalid.vtx[0].txid.clear();
        for blocks in [
            vec![block(2)],
            vec![absent],
            vec![fork],
            vec![invalid],
            vec![b, trailing_invalid],
        ] {
            assert!(w
                .apply_transparent_blocks(&encode_delimited(blocks))
                .is_err());
            assert_eq!(w.to_snapshot().unwrap(), before);
        }
    }

    #[test]
    fn coinbase_maturity_and_old_receivers_survive_address_rotation() {
        let mut w = wallet();
        let original = w.transparent_address().unwrap().to_owned();
        w.next_unified_address().unwrap();
        let mut b = block(1);
        b.vtx[0].vout.push(output(&original, 80_000));
        scan(&mut w, vec![b]);
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 80_000);
        scan(&mut w, (2..100).map(block).collect());
        assert_eq!(w.balance().transparent_available, 0);
        scan(&mut w, vec![block(100)]);
        assert_eq!(w.balance().transparent_available, 80_000);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.utxos[0].coinbase, CoinbaseStatus::Coinbase);
        assert_eq!(restored.balance().transparent_available, 80_000);
    }

    #[test]
    fn transparent_and_shielded_address_rotation_are_independent() {
        let mut w = wallet();
        let initial_ua = w.unified_address().to_owned();
        let initial_t = w.transparent_address().unwrap().to_owned();
        // Default UA is shielded-only (no embedded transparent receiver).
        assert_eq!(
            crate::keys::transparent_from_unified(&initial_ua, Network::Regtest).unwrap(),
            None
        );

        let next_ua = w.next_unified_address().unwrap();
        assert_ne!(next_ua, initial_ua);
        assert_eq!(
            w.transparent_address(),
            Some(initial_t.as_str()),
            "rotating shielded UA must not rotate transparent address"
        );

        let next_t = w.next_transparent_address().unwrap();
        assert_ne!(next_t, initial_t);
        assert_eq!(
            w.unified_address(),
            next_ua,
            "rotating transparent address must not rotate shielded UA"
        );
        // Both original and rotated t1 addresses are tracked by public_transparent_scripts().
        let scripts = w.public_transparent_scripts().unwrap();
        assert!(scripts.iter().any(|(_, a)| a == &initial_t));
        assert!(scripts.iter().any(|(_, a)| a == &next_t));
    }

    #[cfg(feature = "transparent-pir")]
    #[test]
    fn verify_transparent_compact_bucket_promotes_pre_birthday_pir_utxo_and_rejects_forgery() {
        let mut w = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            5,
        )
        .unwrap();
        // Scan blocks 5..=6 (post-birthday).
        let mut b5 = block(5);
        b5.prev_hash = vec![4; 32];
        let b6 = block(6);
        w.apply_compact_blocks_blob(&encode_delimited(vec![b5.clone(), b6.clone()]))
            .unwrap();
        w.apply_transparent_blocks(&encode_delimited(vec![b5, b6]))
            .unwrap();

        // Inject an unverified pre-birthday PIR UTXO at height 2.
        let taddr = w.transparent_address().unwrap().to_owned();
        let tx_out = output(&taddr, 95_000);
        let txid_bytes = vec![0x42u8; 32];
        let txid_hex = to_hex(&txid_bytes);
        w.utxos.push(StoredUtxo {
            coinbase: CoinbaseStatus::Unknown,
            txid: txid_hex.clone(),
            index: 0,
            script: to_hex(&tx_out.script_pub_key),
            value_zat: 95_000,
            height: 2,
            address: taddr,
            spent: false,
            spent_in: None,
        });
        w.transparent_pir
            .unverified_outputs
            .get_or_insert_default()
            .insert(format!("r:{txid_hex}:0"));

        assert_eq!(w.unverified_transparent_heights(), vec![2]);
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 95_000);

        // Tampered compact bucket with mismatched value MUST fail atomically.
        let mut forged_b2 = block(2);
        forged_b2.vtx.push(CompactTx {
            index: 1,
            txid: txid_bytes.clone(),
            vout: vec![TxOut {
                value: 10_000,
                script_pub_key: tx_out.script_pub_key.clone(),
            }],
            ..Default::default()
        });
        let before = w.to_snapshot().unwrap();
        assert!(w
            .verify_transparent_compact_bucket(&encode_delimited(vec![block(1), forged_b2]))
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);

        // Honest aligned compact bucket 1..=6 (anchored to scanned blocks 5..=6)
        // verifies value, script, and non-coinbase provenance.
        let mut honest_b2 = block(2);
        honest_b2.vtx.push(CompactTx {
            index: 1,
            txid: txid_bytes,
            vout: vec![tx_out],
            ..Default::default()
        });
        let mut honest_b5 = block(5);
        honest_b5.prev_hash = vec![4; 32];
        let honest_b6 = block(6);
        let verified = w
            .verify_transparent_compact_bucket(&encode_delimited(vec![
                block(1),
                honest_b2,
                block(3),
                block(4),
                honest_b5,
                honest_b6,
            ]))
            .unwrap();
        assert_eq!(verified, 1);
        assert!(w.unverified_transparent_heights().is_empty());
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::NonCoinbase);
        assert_eq!(w.balance().transparent_available, 95_000);
    }
}

impl WebWallet {
    pub fn transparent_next_height(&self) -> u32 {
        self.transparent_scan
            .as_ref()
            .map_or(self.birthday(), |s| s.height.saturating_add(1))
    }

    pub fn memo_next_height(&self) -> u32 {
        self.memo_scan
            .as_ref()
            .map_or(self.birthday(), |s| s.height.saturating_add(1))
    }

    /// Validate a complete public range before changing any wallet state.
    fn validate_public_range(
        &self,
        blocks: &[CompactBlock],
        coverage: Option<&ScanCoverage>,
    ) -> Result<()> {
        let mut next = coverage.map_or(self.birthday(), |s| s.height.saturating_add(1));
        let mut previous = coverage.map(|s| s.hash.clone());
        for block in blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| EngineError::Message("public range height overflow".into()))?;
            if height != next
                || height > self.scanned_height()
                || block.hash.len() != 32
                || block.prev_hash.len() != 32
            {
                return Err(EngineError::Message(
                    "public range is not contiguous with scanned history".into(),
                ));
            }
            if previous
                .as_deref()
                .or_else(|| self.hash_at(height.saturating_sub(1)))
                .is_some_and(|hash| hash != to_hex(&block.prev_hash))
                || self
                    .hash_at(height)
                    .is_some_and(|hash| hash != to_hex(&block.hash))
            {
                return Err(EngineError::Message(
                    "public range does not match scanned chain; sync again".into(),
                ));
            }
            let mut ids = BTreeSet::new();
            let mut index = None;
            for tx in &block.vtx {
                if tx.txid.len() != 32
                    || !ids.insert(&tx.txid)
                    || index.is_some_and(|i| tx.index <= i)
                    || tx.vin.iter().any(|v| v.prevout_txid.len() != 32)
                    || tx
                        .vout
                        .iter()
                        .any(|v| v.value > 2_100_000_000_000_000 || v.script_pub_key.len() > 10_000)
                {
                    return Err(EngineError::Message(
                        "invalid public transaction data".into(),
                    ));
                }
                index = Some(tx.index);
            }
            next = height.saturating_add(1);
            previous = Some(to_hex(&block.hash));
        }
        Ok(())
    }

    /// Verify the entire public range before decrypting only locally matched
    /// transactions. The gateway receives heights, never our matching txids.
    pub fn apply_shared_memos(&mut self, json: &str) -> Result<u32> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Bundle {
            start: u32,
            end: u32,
            blocks: String,
            transactions: Vec<String>,
        }
        self.ensure_scan_healthy()?;
        if json.len() > 64 * 1024 * 1024 {
            return Err(EngineError::Message("shared memo range too large".into()));
        }
        let bundle: Bundle = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("shared memo bundle: {e}")))?;
        if bundle.end < bundle.start || bundle.end - bundle.start >= 10 {
            return Err(EngineError::Message("invalid shared memo range".into()));
        }
        let bytes = super::from_hex(&bundle.blocks).map_err(EngineError::Message)?;
        let blocks = decode_delimited(&bytes)?;
        if blocks.len() != (bundle.end - bundle.start + 1) as usize
            || blocks
                .first()
                .is_none_or(|b| b.height != u64::from(bundle.start))
            || blocks
                .last()
                .is_none_or(|b| b.height != u64::from(bundle.end))
        {
            return Err(EngineError::Message("incomplete shared memo blocks".into()));
        }
        self.validate_public_range(&blocks, self.memo_scan.as_ref())?;
        let expected: Vec<_> = blocks
            .iter()
            .flat_map(|b| {
                b.vtx
                    .iter()
                    .filter(|tx| {
                        !tx.spends.is_empty()
                            || !tx.outputs.is_empty()
                            || !tx.actions.is_empty()
                            || !tx.ironwood_actions.is_empty()
                    })
                    .map(move |tx| (b.height as u32, &tx.txid))
            })
            .collect();
        if expected.len() != bundle.transactions.len() {
            return Err(EngineError::Message(
                "incomplete shared memo transactions".into(),
            ));
        }
        // The bundle cannot declare a known wallet payment absent just by
        // omitting it from its own compact list. Chain authentication otherwise
        // has the same light-server trust boundary as compact-block sync.
        for (id, agg) in self.txs() {
            if agg
                .mined_height
                .is_some_and(|h| h >= bundle.start && h <= bundle.end)
                && agg.has_shielded_activity()
                && !expected
                    .iter()
                    .any(|(height, txid)| Some(*height) == agg.mined_height && to_hex(txid) == *id)
            {
                return Err(EngineError::Message(
                    "shared memo range omitted a scanned transaction".into(),
                ));
            }
        }
        let mut matched = Vec::new();
        for ((height, id), raw) in expected.into_iter().zip(&bundle.transactions) {
            let raw = super::from_hex(raw).map_err(EngineError::Message)?;
            let mut remaining = raw.as_slice();
            let tx = Transaction::read(
                &mut remaining,
                BranchId::for_height(&self.network(), BlockHeight::from_u32(height)),
            )
            .map_err(|e| EngineError::Message(format!("shared memo transaction: {e}")))?;
            if !remaining.is_empty() || tx.txid().as_ref() != id.as_slice() {
                return Err(EngineError::Message(
                    "shared memo transaction ID mismatch".into(),
                ));
            }
            if self
                .txs()
                .get(&to_hex(id))
                .is_some_and(|agg| agg.mined_height == Some(height))
            {
                matched.push(raw);
            }
        }
        // This is the only fallible wallet prerequisite for enhancement; check
        // it before mutation, after validating every transaction in the range.
        self.decode_ufvk()?;
        let mut found = 0;
        for raw in matched {
            found += self.enhance_raw_tx(&raw)?;
        }
        let last = blocks.last().expect("nonempty validated range");
        self.memo_scan = Some(ScanCoverage {
            height: bundle.end,
            hash: to_hex(&last.hash),
        });
        Ok(found)
    }

    /// Apply a one-off aligned cover-bucket of shared memos (up to 10 blocks)
    /// for an explicit out-of-PIR transaction detail fetch without requiring or
    /// mutating sequential `memo_scan` coverage.
    pub fn apply_private_tx_bucket(&mut self, json: &str) -> Result<u32> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Bundle {
            start: u32,
            end: u32,
            blocks: String,
            transactions: Vec<String>,
        }
        self.ensure_scan_healthy()?;
        if json.len() > 64 * 1024 * 1024 {
            return Err(EngineError::Message("private tx bucket too large".into()));
        }
        let bundle: Bundle = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("private tx bucket: {e}")))?;
        let width = bundle.end.saturating_sub(bundle.start).saturating_add(1);
        let scanned = self.scanned_height();
        if bundle.start == 0 || bundle.end < bundle.start || width > 10 {
            return Err(EngineError::Message(
                "invalid private tx bucket range".into(),
            ));
        }
        // Reject single-block or partial cover buckets when chain depth allows full 10-block cover.
        if scanned >= 10 {
            let aligned = (bundle.start - 1) % 10 == 0;
            let trailing_tip = bundle.end == scanned;
            if width != 10 || (!aligned && !trailing_tip) {
                return Err(EngineError::Message(
                    "private tx bucket must be a full 10-block aligned or tip cover window".into(),
                ));
            }
        } else if bundle.start != 1 || bundle.end != scanned {
            return Err(EngineError::Message(
                "private tx bucket must cover the full scanned chain on short chains".into(),
            ));
        }
        let bytes = super::from_hex(&bundle.blocks).map_err(EngineError::Message)?;
        let blocks = decode_delimited(&bytes)?;
        if blocks.len() != (bundle.end - bundle.start + 1) as usize
            || blocks
                .first()
                .is_none_or(|b| b.height != u64::from(bundle.start))
            || blocks
                .last()
                .is_none_or(|b| b.height != u64::from(bundle.end))
        {
            return Err(EngineError::Message(
                "incomplete private tx bucket blocks".into(),
            ));
        }
        // Validate contiguity, hash linkage, and agreement with any known scanned block hashes.
        let mut expected_height = bundle.start;
        let mut prev_hash: Option<String> = None;
        for block in &blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| EngineError::Message("bucket height overflow".into()))?;
            if height != expected_height
                || height > self.scanned_height()
                || block.hash.len() != 32
                || block.prev_hash.len() != 32
            {
                return Err(EngineError::Message(
                    "private tx bucket is not contiguous with scanned chain".into(),
                ));
            }
            if prev_hash
                .as_deref()
                .or_else(|| self.hash_at(height.saturating_sub(1)))
                .is_some_and(|h| h != to_hex(&block.prev_hash))
                || self
                    .hash_at(height)
                    .is_some_and(|h| h != to_hex(&block.hash))
            {
                return Err(EngineError::Message(
                    "private tx bucket does not match scanned chain".into(),
                ));
            }
            let mut ids = BTreeSet::new();
            let mut index = None;
            for tx in &block.vtx {
                if tx.txid.len() != 32
                    || !ids.insert(&tx.txid)
                    || index.is_some_and(|i| tx.index <= i)
                {
                    return Err(EngineError::Message(
                        "invalid transaction metadata in private tx bucket".into(),
                    ));
                }
                index = Some(tx.index);
            }
            expected_height = height.saturating_add(1);
            prev_hash = Some(to_hex(&block.hash));
        }
        let expected: Vec<_> = blocks
            .iter()
            .flat_map(|b| {
                b.vtx
                    .iter()
                    .filter(|tx| {
                        !tx.spends.is_empty()
                            || !tx.outputs.is_empty()
                            || !tx.actions.is_empty()
                            || !tx.ironwood_actions.is_empty()
                    })
                    .map(move |tx| (b.height as u32, &tx.txid))
            })
            .collect();
        if expected.len() != bundle.transactions.len() {
            return Err(EngineError::Message(
                "incomplete private tx bucket transactions".into(),
            ));
        }
        for (id, agg) in self.txs() {
            if agg
                .mined_height
                .is_some_and(|h| h >= bundle.start && h <= bundle.end)
                && agg.has_shielded_activity()
                && !expected
                    .iter()
                    .any(|(height, txid)| Some(*height) == agg.mined_height && to_hex(txid) == *id)
            {
                return Err(EngineError::Message(
                    "private tx bucket omitted a scanned transaction".into(),
                ));
            }
        }
        let mut matched = Vec::new();
        for ((height, id), raw) in expected.into_iter().zip(&bundle.transactions) {
            let raw = super::from_hex(raw).map_err(EngineError::Message)?;
            let mut remaining = raw.as_slice();
            let tx = Transaction::read(
                &mut remaining,
                BranchId::for_height(&self.network(), BlockHeight::from_u32(height)),
            )
            .map_err(|e| EngineError::Message(format!("private tx bucket transaction: {e}")))?;
            if !remaining.is_empty() || tx.txid().as_ref() != id.as_slice() {
                return Err(EngineError::Message(
                    "private tx bucket transaction ID mismatch".into(),
                ));
            }
            if self
                .txs()
                .get(&to_hex(id))
                .is_some_and(|agg| agg.mined_height == Some(height))
            {
                matched.push(raw);
            }
        }
        self.decode_ufvk()?;
        let mut found = 0;
        for raw in matched {
            found += self.enhance_raw_tx(&raw)?;
        }
        Ok(found)
    }

    /// Distinct mined heights of unspent transparent UTXOs whose funding is still
    /// unverified (e.g. discovered pre-birthday via Transparent PIR). Callers fetch
    /// aligned 500-block all-pool compact buckets covering these heights and pass
    /// them to [`Self::verify_transparent_compact_bucket`] before shielding.
    #[cfg(feature = "transparent-inputs")]
    pub fn unverified_transparent_heights(&self) -> Vec<u32> {
        #[cfg(feature = "transparent-pir")]
        {
            let mut heights = BTreeSet::new();
            for u in &self.utxos {
                if !u.spent && u.height > 0 && self.pir_funding_unverified(&u.txid, u.index) {
                    heights.insert(u.height);
                }
            }
            heights.into_iter().collect()
        }
        #[cfg(not(feature = "transparent-pir"))]
        {
            Vec::new()
        }
    }

    /// Verify unverified transparent PIR UTXOs against an aligned 500-block all-pool
    /// compact block bucket. Requires cryptographic chain anchoring against local
    /// scanned hashes (or an explicit publisher-signed end-block hash via
    /// [`Self::verify_transparent_compact_bucket_anchored`]). Atomic: no state changes
    /// if any check fails.
    #[cfg(feature = "transparent-inputs")]
    pub fn verify_transparent_compact_bucket(&mut self, blob: &[u8]) -> Result<u32> {
        self.verify_transparent_compact_bucket_anchored(blob, None)
    }

    /// Same as [`Self::verify_transparent_compact_bucket`], accepting an optional
    /// 32-byte wire-order end-block hash attested by the caller for strictly
    /// pre-birthday buckets that do not overlap locally scanned block hashes.
    ///
    /// Security requirement: callers MUST derive `trusted_end_hash` exclusively from
    /// cryptographic verification of a pinned publisher signature over the complete
    /// bucket payload (never from an unauthenticated transport response or by echoing
    /// `blocks.last().hash`). Passing `None` enforces anchoring strictly against
    /// locally scanned wallet block hashes.
    #[cfg(feature = "transparent-inputs")]
    pub fn verify_transparent_compact_bucket_anchored(
        &mut self,
        blob: &[u8],
        trusted_end_hash: Option<&[u8]>,
    ) -> Result<u32> {
        const VERIFY_BUCKET_WIDTH: u32 = 500;
        self.ensure_scan_healthy()?;
        let blocks = decode_delimited(blob)?;
        if blocks.is_empty() {
            return Err(EngineError::Message(
                "transparent verification bucket is empty".into(),
            ));
        }
        let start = u32::try_from(blocks[0].height)
            .map_err(|_| EngineError::Message("verification bucket height overflow".into()))?;
        let scanned = self.scanned_height();
        let width = u32::try_from(blocks.len())
            .map_err(|_| EngineError::Message("verification bucket too large".into()))?;
        let end = start
            .checked_add(width)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| EngineError::Message("verification bucket height overflow".into()))?;
        if start == 0
            || width > VERIFY_BUCKET_WIDTH
            || (start - 1) % VERIFY_BUCKET_WIDTH != 0
            || (width < VERIFY_BUCKET_WIDTH && end != scanned)
        {
            return Err(EngineError::Message(
                "transparent verification bucket must be aligned to 500-block boundaries".into(),
            ));
        }
        let mut expected_height = start;
        let mut prev_hash: Option<String> = None;
        let mut anchored = false;
        for block in &blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| EngineError::Message("verification bucket height overflow".into()))?;
            if height != expected_height
                || height > scanned
                || block.hash.len() != 32
                || block.prev_hash.len() != 32
            {
                return Err(EngineError::Message(
                    "transparent verification bucket is not contiguous".into(),
                ));
            }
            let block_hash_hex = to_hex(&block.hash);
            let block_prev_hex = to_hex(&block.prev_hash);
            if prev_hash.as_deref().is_some_and(|h| h != block_prev_hex) {
                return Err(EngineError::Message(
                    "transparent verification bucket is not contiguous".into(),
                ));
            }
            if let Some(known_prev) = self.hash_at(height.saturating_sub(1)) {
                if known_prev != block_prev_hex {
                    return Err(EngineError::Message(
                        "transparent verification bucket does not match scanned chain".into(),
                    ));
                }
                anchored = true;
            }
            if let Some(known_here) = self.hash_at(height) {
                if known_here != block_hash_hex {
                    return Err(EngineError::Message(
                        "transparent verification bucket does not match scanned chain".into(),
                    ));
                }
                anchored = true;
            }
            if block.vtx.first().is_none_or(|t| t.index != 0) {
                return Err(EngineError::Message(
                    "verification bucket omitted transparent block coverage (coinbase missing)"
                        .into(),
                ));
            }
            let mut ids = BTreeSet::new();
            let mut index = None;
            for tx in &block.vtx {
                if tx.txid.len() != 32
                    || !ids.insert(&tx.txid)
                    || index.is_some_and(|i| tx.index <= i)
                    || tx.vin.iter().any(|v| v.prevout_txid.len() != 32)
                    || tx
                        .vout
                        .iter()
                        .any(|v| v.value > 2_100_000_000_000_000 || v.script_pub_key.len() > 10_000)
                {
                    return Err(EngineError::Message(
                        "invalid transparent transaction in verification bucket".into(),
                    ));
                }
                index = Some(tx.index);
            }
            expected_height = height.saturating_add(1);
            prev_hash = Some(block_hash_hex);
        }
        if self.network != crate::Network::Regtest && trusted_end_hash.is_none() {
            return Err(EngineError::Message(
                "transparent_pir_verification_unauthenticated".into(),
            ));
        }
        if let Some(expected_end) = trusted_end_hash {
            if expected_end.len() != 32
                || blocks
                    .last()
                    .is_none_or(|b| b.hash.as_slice() != expected_end)
            {
                return Err(EngineError::Message(
                    "transparent verification bucket anchor hash mismatch".into(),
                ));
            }
            anchored = true;
        }
        if !anchored {
            return Err(EngineError::Message(
                "transparent_pir_verification_unanchored".into(),
            ));
        }
        #[cfg(feature = "transparent-pir")]
        {
            // Require continuous spend coverage between the verified bucket and the
            // wallet's locally scanned compact range (either PIR checkpoint reaches
            // birthday - 1, or the bucket overlaps the scanned range).
            let pir_covered = self.transparent_pir.covered_through;
            let bday_floor = u64::from(self.birthday().saturating_sub(1));
            if u64::from(end) < bday_floor && pir_covered < bday_floor {
                return Err(EngineError::Message(
                    "transparent_pir_spend_coverage_gap".into(),
                ));
            }
        }

        // Build lookup map of outputs & spends in this anchored compact bucket.
        let mut bucket_outputs =
            std::collections::BTreeMap::<(u32, String, u32), (u64, String, bool)>::new();
        let mut bucket_spends = std::collections::BTreeMap::<(String, u32), (String, u32)>::new();
        for block in &blocks {
            let height = block.height as u32;
            for tx in &block.vtx {
                let txid = to_hex(&tx.txid);
                let is_coinbase = tx.index == 0;
                for (vout_idx, out) in tx.vout.iter().enumerate() {
                    bucket_outputs.insert(
                        (height, txid.clone(), vout_idx as u32),
                        (out.value, to_hex(&out.script_pub_key), is_coinbase),
                    );
                }
                for vin in &tx.vin {
                    bucket_spends.insert(
                        (to_hex(&vin.prevout_txid), vin.prevout_index),
                        (txid.clone(), height),
                    );
                }
            }
        }

        #[cfg(feature = "transparent-pir")]
        {
            // Preflight every unverified UTXO in `start..=end` before mutating state.
            let mut verified_indices: Vec<(usize, bool)> = Vec::new();
            for (i, u) in self.utxos.iter().enumerate() {
                if u.height < start
                    || u.height > end
                    || !self.pir_funding_unverified(&u.txid, u.index)
                {
                    continue;
                }
                let Some((value, script, is_coinbase)) =
                    bucket_outputs.get(&(u.height, u.txid.clone(), u.index))
                else {
                    return Err(EngineError::Message(
                        "transparent_pir_verification_missing".into(),
                    ));
                };
                if *value != u.value_zat || *script != u.script {
                    return Err(EngineError::Message(
                        "transparent_pir_verification_mismatch".into(),
                    ));
                }
                verified_indices.push((i, *is_coinbase));
            }

            // Apply verified coinbase provenance and clear PIR unverified flags.
            let mut confirmed = 0u32;
            for (i, is_coinbase) in verified_indices {
                let txid = self.utxos[i].txid.clone();
                let index = self.utxos[i].index;
                let value = self.utxos[i].value_zat;
                let height = self.utxos[i].height;
                self.utxos[i].coinbase = CoinbaseStatus::from_transaction(is_coinbase);
                self.credit_transparent_receipt(&txid, index, value, Some(height), false);
                self.confirm_public_transparent_output(&txid, index);
                confirmed += 1;
            }
            // Apply observed spends ONLY for outputs that are verified.
            for ((prev_txid, prev_idx), (spender, spend_height)) in bucket_spends {
                if self.pir_funding_unverified(&prev_txid, prev_idx) {
                    continue;
                }
                let mut matched = false;
                for u in &mut self.utxos {
                    if u.txid == prev_txid && u.index == prev_idx {
                        u.spent = true;
                        u.spent_in = Some(spender.clone());
                        matched = true;
                    }
                }
                if matched {
                    self.record_transparent_receipt_spend(
                        &prev_txid,
                        prev_idx,
                        &spender,
                        Some(spend_height),
                    );
                    self.record_public_transparent_spend(&spender, &prev_txid, prev_idx);
                }
            }
            self.recompute_pool_fields();
            Ok(confirmed)
        }
        #[cfg(not(feature = "transparent-pir"))]
        {
            let _ = (bucket_outputs, bucket_spends, end);
            Ok(0)
        }
    }

    /// Ingest all transparent inputs/outputs from public compact ranges. The
    /// caller must negotiate lightwallet-protocol >= 0.5 before requesting them.
    /// All-pool ranges include coinbase; its absence is not an empty balance.
    #[cfg(feature = "transparent-inputs")]
    pub fn apply_transparent_blocks(&mut self, blob: &[u8]) -> Result<u32> {
        self.ensure_scan_healthy()?;
        let blocks = decode_delimited(blob)?;
        self.validate_public_range(&blocks, self.transparent_scan.as_ref())?;
        if blocks
            .iter()
            .any(|b| b.vtx.first().is_none_or(|t| t.index != 0))
        {
            return Err(EngineError::Message(
                "server omitted transparent block coverage (coinbase missing)".into(),
            ));
        }
        let scripts = self.public_transparent_scripts()?;
        let mut receipts = Vec::new();
        let mut spends = Vec::new();
        for block in &blocks {
            for tx in &block.vtx {
                let id = to_hex(&tx.txid);
                for (index, output) in tx.vout.iter().enumerate() {
                    if scripts
                        .iter()
                        .any(|(script, _)| *script == output.script_pub_key)
                    {
                        receipts.push((id.clone(), index as u32, output.value));
                    }
                }
                for input in &tx.vin {
                    spends.push((
                        to_hex(&input.prevout_txid),
                        input.prevout_index,
                        id.clone(),
                        Some(block.height as u32),
                    ));
                }
            }
        }
        self.preflight_transparent_receipts(&receipts)?;
        self.preflight_transparent_spends(&spends)?;
        self.transparent_scan_required = true;
        let mut found = 0;
        for block in blocks {
            let height = block.height as u32;
            for tx in &block.vtx {
                let txid = to_hex(&tx.txid);
                let mut involved = false;
                for input in &tx.vin {
                    let prev = to_hex(&input.prevout_txid);
                    for u in &mut self.utxos {
                        if u.txid == prev && u.index == input.prevout_index {
                            u.spent = true;
                            u.spent_in = Some(txid.clone());
                            involved = true;
                        }
                    }
                    involved |= self
                        .record_transparent_receipt_spend(
                            &prev,
                            input.prevout_index,
                            &txid,
                            Some(height),
                        )
                        .is_some();
                    #[cfg(feature = "transparent-pir")]
                    self.record_public_transparent_spend(&txid, &prev, input.prevout_index);
                }
                for (index, output) in tx.vout.iter().enumerate() {
                    let Some((_, address)) = scripts
                        .iter()
                        .find(|(script, _)| *script == output.script_pub_key)
                    else {
                        continue;
                    };
                    involved = true;
                    if let Some(u) = self
                        .utxos
                        .iter_mut()
                        .find(|u| u.txid == txid && u.index == index as u32)
                    {
                        u.height = height;
                        u.coinbase = CoinbaseStatus::from_transaction(tx.index == 0);
                        u.value_zat = output.value;
                        u.script = to_hex(&output.script_pub_key);
                    } else {
                        self.utxos.push(StoredUtxo {
                            txid: txid.clone(),
                            index: index as u32,
                            script: to_hex(&output.script_pub_key),
                            value_zat: output.value,
                            height,
                            address: address.clone(),
                            coinbase: CoinbaseStatus::from_transaction(tx.index == 0),
                            spent: false,
                            spent_in: None,
                        });
                        found += 1;
                    }
                    self.credit_transparent_receipt(
                        &txid,
                        index as u32,
                        output.value,
                        Some(height),
                        false,
                    );
                    #[cfg(feature = "transparent-pir")]
                    self.record_public_transparent_output(&txid, index as u32);
                }
                if involved {
                    let agg = self.txs_mut().entry(txid).or_default();
                    agg.mined_height = Some(height);
                    agg.block_time = Some(block.time);
                    if tx.fee > 0 {
                        agg.fee_zat = Some(tx.fee as u64);
                    }
                }
            }
            self.transparent_scan = Some(ScanCoverage {
                height,
                hash: to_hex(&block.hash),
            });
        }
        #[cfg(feature = "transparent-pir")]
        self.confirm_complete_public_transparent_outputs();
        self.recompute_pool_fields();
        Ok(found)
    }

    #[cfg(feature = "transparent-inputs")]
    pub(crate) fn public_transparent_entries(
        &mut self,
    ) -> Result<Vec<(Vec<u8>, String, transparent::keys::TransparentKeyScope, u32)>> {
        use transparent::{
            address::{Script, TransparentAddress},
            keys::{NonHardenedChildIndex, TransparentKeyScope},
        };
        use zcash_keys::encoding::AddressCodec;
        let ext_limit = self.next_transparent_diversifier.saturating_add(32);
        let int_limit = self.next_internal_taddr_index.saturating_add(32);
        let eph_limit = self.next_ephemeral_index.saturating_add(16);
        if ext_limit > 10_000 || int_limit > 10_000 || eph_limit > 10_000 {
            return Err(EngineError::Message(
                "transparent derivation range exceeds scan limit".into(),
            ));
        }
        let mut entries = Vec::new();
        if let Some(key) = self.decode_ufvk()?.transparent() {
            for i in 0..=ext_limit {
                let child =
                    NonHardenedChildIndex::from_index(i).expect("bounded non-hardened child");
                let public = key
                    .derive_address_pubkey(TransparentKeyScope::EXTERNAL, child)
                    .map_err(|e| EngineError::Message(format!("transparent receiver: {e}")))?;
                let address = TransparentAddress::from_pubkey(&public);
                let script: Script = address.script().into();
                entries.push((
                    script.0 .0,
                    address.encode(&self.network()),
                    TransparentKeyScope::EXTERNAL,
                    i,
                ));
            }
            for i in 0..=int_limit {
                let child =
                    NonHardenedChildIndex::from_index(i).expect("bounded non-hardened child");
                let public = key
                    .derive_address_pubkey(TransparentKeyScope::INTERNAL, child)
                    .map_err(|e| EngineError::Message(format!("transparent receiver: {e}")))?;
                let address = TransparentAddress::from_pubkey(&public);
                let script: Script = address.script().into();
                entries.push((
                    script.0 .0,
                    address.encode(&self.network()),
                    TransparentKeyScope::INTERNAL,
                    i,
                ));
            }
            if let Ok(eph_ivk) = key.derive_ephemeral_ivk() {
                for i in 0..=eph_limit {
                    let child =
                        NonHardenedChildIndex::from_index(i).expect("bounded non-hardened child");
                    let address = eph_ivk
                        .derive_ephemeral_address(child)
                        .map_err(|e| EngineError::Message(format!("ephemeral receiver: {e}")))?;
                    let script: Script = address.script().into();
                    entries.push((
                        script.0 .0,
                        address.encode(&self.network()),
                        TransparentKeyScope::EPHEMERAL,
                        i,
                    ));
                }
            }
        }
        Ok(entries)
    }

    #[cfg(feature = "transparent-inputs")]
    pub(crate) fn public_transparent_scripts(&mut self) -> Result<Vec<(Vec<u8>, String)>> {
        let key = (
            self.next_transparent_diversifier,
            self.next_internal_taddr_index,
            self.next_ephemeral_index,
        );
        if let Some((cached, scripts)) = &self.transparent_scripts {
            if *cached == key {
                return Ok(scripts.clone());
            }
        }
        let scripts: Vec<_> = self
            .public_transparent_entries()?
            .into_iter()
            .map(|(script, address, _, _)| (script, address))
            .collect();
        self.transparent_scripts = Some((key, scripts.clone()));
        Ok(scripts)
    }

    pub(crate) fn rewind_public_coverage(&mut self) {
        let height = self.scanned_height();
        let hash = self.hash_at(height).map(str::to_owned);
        for coverage in [&mut self.transparent_scan, &mut self.memo_scan] {
            if coverage.as_ref().is_some_and(|s| s.height > height) {
                *coverage = hash.as_ref().map(|h| ScanCoverage {
                    height,
                    hash: h.clone(),
                });
            }
        }
    }
}
