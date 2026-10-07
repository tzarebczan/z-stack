//! Wallet-independent block-range data. Coverage is separate from shielded scan
//! progress so an old snapshot never claims it has scanned transparent activity.

use super::store::StoredUtxo;
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
        assert!(restored.utxos[0].coinbase);
        assert_eq!(restored.balance().transparent_available, 80_000);
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
                        u.coinbase = tx.index == 0;
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
                            coinbase: tx.index == 0,
                            spent: false,
                            spent_in: None,
                        });
                        found += 1;
                    }
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
        self.recompute_pool_fields();
        Ok(found)
    }

    #[cfg(feature = "transparent-inputs")]
    fn public_transparent_scripts(&mut self) -> Result<Vec<(Vec<u8>, String)>> {
        use transparent::{
            address::{Script, TransparentAddress},
            keys::{NonHardenedChildIndex, TransparentKeyScope},
        };
        use zcash_keys::encoding::AddressCodec;
        if let Some((index, scripts)) = &self.transparent_scripts {
            if *index == self.next_diversifier {
                return Ok(scripts.clone());
            }
        }
        // Include previously issued receivers and a small gap, not just the
        // currently displayed address. Reject unreasonable imported state.
        let limit = self.next_diversifier.saturating_add(32);
        if limit > 10_000 {
            return Err(EngineError::Message(
                "transparent derivation range exceeds scan limit".into(),
            ));
        }
        let mut scripts = Vec::new();
        if let Some(key) = self.decode_ufvk()?.transparent() {
            for i in 0..=limit {
                let child =
                    NonHardenedChildIndex::from_index(i).expect("bounded non-hardened child");
                for scope in [TransparentKeyScope::EXTERNAL, TransparentKeyScope::INTERNAL] {
                    let public = key
                        .derive_address_pubkey(scope, child)
                        .map_err(|e| EngineError::Message(format!("transparent receiver: {e}")))?;
                    let address = TransparentAddress::from_pubkey(&public);
                    let script: Script = address.script().into();
                    scripts.push((script.0 .0, address.encode(&self.network())));
                }
            }
        }
        self.transparent_scripts = Some((self.next_diversifier, scripts.clone()));
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
