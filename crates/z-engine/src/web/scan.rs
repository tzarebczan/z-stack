//! Apply compact blocks with `scan_block` + local nullifier matching.
//!
//! Large length-delimited blobs trial-decrypt **shielded** blocks on Rayon (the
//! same pool `wasm-bindgen-rayon` / native BatchRunner uses). Small follow-on
//! batches stay on the calling worker. Empty compact blocks skip the pool and
//! are ingested as a run. Sequential hash/height
//! checks and note ingest stay ordered. Without a wasm thread pool Rayon is
//! single-threaded.

use super::framed::SERIAL_SCAN_MAX_BLOCKS;
use super::store::{TrackedNote, WebWallet};
use super::{decode_delimited, to_hex};
use crate::error::{EngineError, Result};
use crate::scan::compact_has_shielded;
use prost::Message;
use rayon::prelude::*;
use zcash_client_backend::{
    proto::compact_formats::{CompactBlock, CompactOrchardAction, CompactTx},
    scanning::{scan_block, Nullifiers, ScanningKeys},
    wallet::{WalletOrchardOutput, WalletSaplingOutput},
};
use zcash_primitives::block::BlockHash;
use zip32::Scope;

pub(crate) type ScanKeys = ScanningKeys<u32, (u32, Scope)>;

/// First step back when a block does not extend our tip. Consensus rollbacks
/// are at most 100 blocks, so a few doublings reach any real fork point.
const REORG_FIRST_STEP: u32 = 10;

struct Head {
    height: u32,
    time: u32,
    hash: Vec<u8>,
    sap: u32,
    orch: u32,
    iron: u32,
    shielded: bool,
    txids: Vec<String>,
    spends: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDelta {
    pub height: u32,
    pub notes_found: u32,
    pub spends_found: u32,
}

impl WebWallet {
    fn scan_keys(&mut self) -> Result<std::sync::Arc<ScanKeys>> {
        if let Some(keys) = &self.cached_scan_keys {
            return Ok(keys.clone());
        }
        let keys = std::sync::Arc::new(ScanningKeys::from_account_ufvks([(
            0u32,
            self.decode_ufvk()?,
        )]));
        self.cached_scan_keys = Some(keys.clone());
        Ok(keys)
    }

    pub fn apply_compact_blocks_summary(&mut self, blob: &[u8]) -> Result<ScanDelta> {
        let deltas = self.apply_compact_blocks_blob(blob)?;
        Ok(ScanDelta {
            height: deltas
                .last()
                .map(|d| d.height)
                .unwrap_or_else(|| self.scanned_height()),
            notes_found: deltas.iter().map(|d| d.notes_found).sum(),
            spends_found: deltas.iter().map(|d| d.spends_found).sum(),
        })
    }

    pub fn apply_compact_blocks_blob(&mut self, blob: &[u8]) -> Result<Vec<ScanDelta>> {
        self.ensure_scan_healthy()?;
        let mut blocks = decode_delimited(blob)?;
        if blocks.is_empty() {
            return Ok(Vec::new());
        }
        // Pool aggregates are rebuilt once after a full sync (notes are the source of truth).
        // Recomputing after every blob clones the note list and dominates catch-up time.
        // Fill missing chain_metadata so we never fall back to sequential scan_block
        // (Zaino sometimes omits it; that was a 1000-block single-thread crawl).
        self.prepare_scan_window(
            u32::try_from(blocks.last().expect("nonempty").height)
                .map_err(|_| EngineError::Message("block height overflow".into()))?,
        );
        let (sap0, orch0, iron0) = self.frontier_tree_sizes();
        crate::scan::ensure_compact_chain_metadata(&mut blocks, sap0, orch0, iron0);
        let result = self.apply_decoded_parallel(blocks).and_then(|out| {
            self.prune_settled_shards();
            self.assert_consistent_leaf_hashes()?;
            Ok(out)
        });
        self.finish_scan_update(result)
    }

    pub fn apply_compact_block(&mut self, bytes: &[u8]) -> Result<ScanDelta> {
        self.ensure_scan_healthy()?;
        let block = CompactBlock::decode(bytes)
            .map_err(|e| EngineError::Message(format!("compact block: {e}")))?;
        self.prepare_scan_window(
            u32::try_from(block.height)
                .map_err(|_| EngineError::Message("block height overflow".into()))?,
        );
        let result = self.apply_decoded(block).and_then(|out| {
            self.assert_consistent_leaf_hashes()?;
            Ok(out)
        });
        self.finish_scan_update(result)
    }

    /// Treat a proved raw tx as if it just landed in the next compact block.
    /// Used by unit tests (and WASM `applyMinedTx`) to exercise pending → mined.
    /// Display-order txid (as explorers show it) of a raw transaction built
    /// for this wallet's next block.
    pub fn raw_txid(&self, raw: &[u8]) -> Result<String> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};

        let height = self.next_height();
        let branch = BranchId::for_height(&self.network(), BlockHeight::from_u32(height.max(1)));
        let tx = Transaction::read(raw, branch)
            .map_err(|e| EngineError::Message(format!("read tx: {e}")))?;
        Ok(tx.txid().to_string())
    }

    pub fn apply_mined_raw_tx(&mut self, raw: &[u8], time: u32) -> Result<ScanDelta> {
        self.ensure_scan_healthy()?;
        let result = self.apply_mined_raw_tx_inner(raw, time);
        self.finish_scan_update(result)
    }

    fn apply_mined_raw_tx_inner(&mut self, raw: &[u8], time: u32) -> Result<ScanDelta> {
        let block = self.mined_raw_tx_block(raw, time)?;
        let d = self.apply_decoded(block)?;
        self.assert_consistent_leaf_hashes()?;
        self.recompute_pool_fields();
        Ok(d)
    }

    /// The compact block `apply_mined_raw_tx` would scan at the next height.
    pub(crate) fn mined_raw_tx_block(&self, raw: &[u8], time: u32) -> Result<CompactBlock> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};

        let height = self.next_height();
        let branch = BranchId::for_height(&self.network(), BlockHeight::from_u32(height.max(1)));
        let tx = Transaction::read(&raw[..], branch)
            .map_err(|e| EngineError::Message(format!("mine tx: {e}")))?;
        let mut vtx = CompactTx {
            index: 1,
            txid: tx.txid().as_ref().to_vec(),
            fee: 0,
            spends: vec![],
            outputs: vec![],
            actions: vec![],
            ironwood_actions: vec![],
            vin: vec![],
            vout: vec![],
        };
        if let Some(bundle) = tx.orchard_bundle() {
            for action in bundle.actions() {
                vtx.actions.push(CompactOrchardAction::from(action));
            }
        }
        // After NU6.3 payments to Orchard receivers are Ironwood actions.
        if let Some(bundle) = tx.ironwood_bundle() {
            for action in bundle.actions() {
                vtx.ironwood_actions
                    .push(CompactOrchardAction::from(action));
            }
        }
        let prev_hash = self
            .prior
            .as_ref()
            .and_then(|p| super::from_hex(&p.hash).ok())
            .filter(|h| h.len() == 32)
            .unwrap_or_else(|| vec![0u8; 32]);
        let mut hash = vec![0u8; 32];
        hash[0] = (height & 0xff) as u8;
        hash[1] = 0x4d;
        hash[2] = ((height >> 8) & 0xff) as u8;
        Ok(CompactBlock {
            height: u64::from(height),
            hash,
            prev_hash,
            time: if time == 0 { 1 } else { time },
            header: vec![],
            vtx: vec![vtx],
            chain_metadata: None,
        })
    }

    fn ensure_chain(&mut self, block: &CompactBlock) -> Result<Option<u32>> {
        let height = u32::try_from(block.height)
            .map_err(|_| EngineError::Message("block height overflow".into()))?;
        let incoming = to_hex(&block.hash);
        if let Some(have) = self.hash_at(height) {
            if have == incoming {
                return Ok(None);
            }
            self.rewind_to_height(height.saturating_sub(1))?;
        }
        if !block.prev_hash.is_empty() {
            if let Some(prev_h) = height.checked_sub(1) {
                if let Some(have_prev) = self.hash_at(prev_h) {
                    let have_bytes = super::from_hex(have_prev).unwrap_or_default();
                    if have_bytes != block.prev_hash {
                        return Err(self.step_back_from_fork(height)?);
                    }
                }
            }
        }
        let expect = self.next_height();
        if height != expect {
            return Err(EngineError::Message(format!(
                "expected compact block {expect}, got {height}"
            )));
        }
        if self
            .reorg_probe
            .is_some_and(|(kept, _)| height > kept.saturating_add(1))
        {
            self.reorg_probe = None;
        }
        Ok(Some(height))
    }

    /// `height` does not extend our block at `height - 1`, so the fork is at
    /// or below that block. The new chain's blocks are not among our saved
    /// hashes, so its parent cannot be looked up: step back, have the caller
    /// refetch from the returned height, and double the step when the first
    /// refetched block still does not extend. Searching straight back to the
    /// birthday turned a one-block tip reorg into a full rescan that the sync
    /// refused to save. A fork below the oldest saved hash restarts at the
    /// birthday; when older hashes were pruned that keeps the session invalid
    /// (as before), so a long history is never replaced by a rescan unasked.
    fn step_back_from_fork(&mut self, height: u32) -> Result<EngineError> {
        let repeat = self
            .reorg_probe
            .filter(|&(kept, _)| kept.saturating_add(1) == height);
        let step = repeat.map_or(REORG_FIRST_STEP, |(_, step)| step.saturating_mul(2));
        let oldest = self.block_hashes.keys().next().copied();
        let exhausted = match (oldest, repeat) {
            (None, _) => true,
            (Some(oldest), Some((kept, _))) => kept <= oldest,
            _ => false,
        };
        let target = match oldest {
            Some(oldest) if !exhausted => height.saturating_sub(1).saturating_sub(step).max(oldest),
            _ => self.birthday().saturating_sub(1),
        };
        let pruned = oldest.is_none_or(|oldest| oldest > self.birthday());
        self.rewind_to_height(target)?;
        let next = self.next_height();
        self.reorg_probe = Some((next.saturating_sub(1), step));
        if exhausted && pruned {
            self.fail_close_deep_fork(height);
        }
        Ok(EngineError::Reorg { height, next })
    }

    fn apply_decoded(&mut self, block: CompactBlock) -> Result<ScanDelta> {
        let keys = compact_has_shielded(&block)
            .then(|| self.scan_keys())
            .transpose()?;
        self.apply_decoded_with_keys(block, keys.as_deref())
    }

    fn apply_decoded_with_keys(
        &mut self,
        block: CompactBlock,
        keys: Option<&ScanKeys>,
    ) -> Result<ScanDelta> {
        let Some(height) = self.ensure_chain(&block)? else {
            return Ok(ScanDelta {
                height: block.height as u32,
                notes_found: 0,
                spends_found: 0,
            });
        };

        let mut spends_found = 0u32;
        for tx in &block.vtx {
            let txid = to_hex(&tx.txid);
            for s in &tx.spends {
                spends_found += self.mark_spent(&s.nf, &txid, height);
            }
            for a in &tx.actions {
                spends_found += self.mark_spent(&a.nullifier, &txid, height);
            }
            for a in &tx.ironwood_actions {
                spends_found += self.mark_spent(&a.nullifier, &txid, height);
            }
        }

        let block_time = block.time;
        let block_txids: Vec<String> = block.vtx.iter().map(|t| to_hex(&t.txid)).collect();
        if !compact_has_shielded(&block) {
            let (sap, orch, iron) = empty_tree_sizes(&block, self.prior.as_ref());
            self.append_sapling(height, &[])?;
            self.append_orchard(height, &[])?;
            self.append_ironwood(height, &[])?;
            self.set_scanned(
                height,
                Self::make_prior(height, hash32(&block.hash), sap, orch, iron),
            )?;
            self.stamp_block_time(block_time, &block_txids);
            return Ok(ScanDelta {
                height,
                notes_found: 0,
                spends_found,
            });
        }

        let network = self.network();
        let nfs = Nullifiers::empty();
        let prior = self.prior_metadata()?;
        let keys = keys.ok_or_else(|| EngineError::Message("missing shielded scan keys".into()))?;
        let scanned = scan_block(&network, block, keys, &nfs, prior.as_ref())
            .map_err(|e| EngineError::Message(format!("scan_block: {e}")))?;

        let mut notes_found = 0u32;
        for wtx in scanned.transactions() {
            let txid = to_hex(wtx.txid().as_ref());
            notes_found += self.ingest_sapling(&txid, height, wtx.sapling_outputs())?;
            notes_found += self.ingest_orchard(&txid, height, "orchard", wtx.orchard_outputs())?;
            notes_found +=
                self.ingest_orchard(&txid, height, "ironwood", wtx.ironwood_outputs())?;
        }

        self.append_sapling(height, scanned.sapling().commitments())?;
        self.append_orchard(height, scanned.orchard().commitments())?;
        self.append_ironwood(height, scanned.ironwood().commitments())?;

        let sapling_sz = scanned.sapling().final_tree_size();
        let orchard_sz = scanned.orchard().final_tree_size();
        let ironwood_sz = scanned.ironwood().final_tree_size();
        let hash = scanned.block_hash();
        self.set_scanned(
            height,
            Self::make_prior(height, hash, sapling_sz, orchard_sz, ironwood_sz),
        )?;
        self.stamp_block_time(block_time, &block_txids);

        Ok(ScanDelta {
            height,
            notes_found,
            spends_found,
        })
    }

    fn stamp_block_time(&mut self, time: u32, txids: &[String]) {
        if time == 0 {
            return;
        }
        for txid in txids {
            if let Some(agg) = self.txs_mut().get_mut(txid) {
                if agg.block_time.is_none() {
                    agg.block_time = Some(time);
                }
            }
        }
    }

    /// Trial-decrypt shielded compact blocks on the Rayon pool, then ingest in height order.
    /// Empty (no sapling/orchard/ironwood) blocks never enter the pool — scheduling
    /// 4000 no-op tasks was slower than single-thread on sparse testnet.
    /// Each block must carry `chain_metadata` so `scan_block` does not need prior tree state.
    ///
    /// Live Sinsemilla `batch_insert` is deferred until `finalize_scan_trees` when the
    /// in-memory trees are not yet built (cold / rescan). selective shard scanning drops unmarked
    /// completed shards (seeded `GetSubtreeRoots` stand in). Incremental catch-up hashes
    /// live. Decrypt still runs here so download can overlap apply.
    fn apply_decoded_parallel(&mut self, blocks: Vec<CompactBlock>) -> Result<Vec<ScanDelta>> {
        self.apply_decoded_parallel_inner(blocks)
    }

    fn apply_decoded_parallel_inner(
        &mut self,
        mut blocks: Vec<CompactBlock>,
    ) -> Result<Vec<ScanDelta>> {
        while !blocks.is_empty() {
            match self.ensure_chain(&blocks[0])? {
                None => {
                    blocks.remove(0);
                }
                Some(_) => break,
            }
        }
        if blocks.is_empty() {
            return Ok(Vec::new());
        }
        let expect = self.next_height();
        for (i, block) in blocks.iter().enumerate() {
            let height = u32::try_from(block.height)
                .map_err(|_| EngineError::Message("block height overflow".into()))?;
            let want = expect.saturating_add(i as u32);
            if height != want {
                return Err(EngineError::Message(format!(
                    "expected compact block {want}, got {height}"
                )));
            }
            if i > 0 && block.prev_hash != blocks[i - 1].hash {
                return Err(EngineError::Message(format!(
                    "compact block {height} prev_hash mismatch"
                )));
            }
        }

        let network = self.network();
        let n = blocks.len();
        let mut decrypts = Decrypts::new(network, n);

        let mut heads = Vec::with_capacity(n);
        for (i, block) in blocks.into_iter().enumerate() {
            let height = block.height as u32;
            let shielded = compact_has_shielded(&block);
            let (sap, orch, iron) = match &block.chain_metadata {
                Some(m) => (
                    m.sapling_commitment_tree_size,
                    m.orchard_commitment_tree_size,
                    m.ironwood_commitment_tree_size,
                ),
                None => (0, 0, 0),
            };
            if shielded {
                let mut txids = Vec::with_capacity(block.vtx.len());
                let mut spends = Vec::new();
                for tx in &block.vtx {
                    let txid = to_hex(&tx.txid);
                    txids.push(txid.clone());
                    for s in &tx.spends {
                        spends.push((txid.clone(), s.nf.clone()));
                    }
                    for a in &tx.actions {
                        spends.push((txid.clone(), a.nullifier.clone()));
                    }
                    for a in &tx.ironwood_actions {
                        spends.push((txid.clone(), a.nullifier.clone()));
                    }
                }
                heads.push(Head {
                    height,
                    time: block.time,
                    hash: Vec::new(),
                    sap,
                    orch,
                    iron,
                    shielded: true,
                    txids,
                    spends,
                });
                decrypts.push(self, i, block)?;
            } else {
                heads.push(Head {
                    height,
                    time: block.time,
                    hash: block.hash,
                    sap,
                    orch,
                    iron,
                    shielded: false,
                    txids: Vec::new(),
                    spends: Vec::new(),
                });
            }
        }

        decrypts.finish_queue(self)?;

        let mut out = Vec::with_capacity(heads.len());
        let mut i = 0;
        while i < heads.len() {
            if !heads[i].shielded {
                let start = i;
                while i < heads.len() && !heads[i].shielded {
                    i += 1;
                }
                self.ingest_empty_run(&heads[start..i], &mut out)?;
                continue;
            }
            let head = &heads[i];
            let sc = decrypts.take(i)?;
            let mut spends_found = 0u32;
            for (txid, nf) in &head.spends {
                spends_found += self.mark_spent(nf, txid, head.height);
            }
            match sc {
                DecryptOut::Empty {
                    hash,
                    sapling_sz,
                    orchard_sz,
                    ironwood_sz,
                } => {
                    self.finish_empty(
                        head.height,
                        &hash,
                        sapling_sz,
                        orchard_sz,
                        ironwood_sz,
                        true,
                    )?;
                    self.stamp_block_time(head.time, &head.txids);
                    out.push(ScanDelta {
                        height: head.height,
                        notes_found: 0,
                        spends_found,
                    });
                }
                DecryptOut::Scanned(sc) => {
                    let mut notes_found = 0u32;
                    for wtx in sc.transactions() {
                        let txid = to_hex(wtx.txid().as_ref());
                        notes_found +=
                            self.ingest_sapling(&txid, head.height, wtx.sapling_outputs())?;
                        notes_found += self.ingest_orchard(
                            &txid,
                            head.height,
                            "orchard",
                            wtx.orchard_outputs(),
                        )?;
                        notes_found += self.ingest_orchard(
                            &txid,
                            head.height,
                            "ironwood",
                            wtx.ironwood_outputs(),
                        )?;
                    }
                    self.append_sapling(head.height, sc.sapling().commitments())?;
                    self.append_orchard(head.height, sc.orchard().commitments())?;
                    self.append_ironwood(head.height, sc.ironwood().commitments())?;
                    self.set_scanned(
                        head.height,
                        Self::make_prior(
                            head.height,
                            sc.block_hash(),
                            sc.sapling().final_tree_size(),
                            sc.orchard().final_tree_size(),
                            sc.ironwood().final_tree_size(),
                        ),
                    )?;
                    self.stamp_block_time(head.time, &head.txids);
                    out.push(ScanDelta {
                        height: head.height,
                        notes_found,
                        spends_found,
                    });
                }
            }
            i += 1;
        }
        Ok(out)
    }

    fn ingest_empty_run(&mut self, run: &[Head], out: &mut Vec<ScanDelta>) -> Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        let mut last_cp = self
            .sapling_leaves
            .last()
            .map(|l| l.height)
            .unwrap_or(self.scanned_height());
        for (i, h) in run.iter().enumerate() {
            let is_last = i + 1 == run.len();
            let checkpoint =
                is_last || h.height.saturating_sub(last_cp) >= crate::scan::CHECKPOINT_EVERY;
            self.finish_empty(h.height, &h.hash, h.sap, h.orch, h.iron, checkpoint)?;
            if checkpoint {
                last_cp = h.height;
            }
            out.push(ScanDelta {
                height: h.height,
                notes_found: 0,
                spends_found: 0,
            });
        }
        Ok(())
    }

    fn finish_empty(
        &mut self,
        height: u32,
        hash: &[u8],
        sapling_sz: u32,
        orchard_sz: u32,
        ironwood_sz: u32,
        checkpoint: bool,
    ) -> Result<()> {
        let (sap, orch, iron) = if sapling_sz == 0 && orchard_sz == 0 && ironwood_sz == 0 {
            if let Some(p) = &self.prior {
                (
                    p.sapling_tree_size,
                    p.orchard_tree_size,
                    p.ironwood_tree_size,
                )
            } else {
                (0, 0, 0)
            }
        } else {
            (sapling_sz, orchard_sz, ironwood_sz)
        };
        if checkpoint {
            self.append_sapling(height, &[])?;
            self.append_orchard(height, &[])?;
            self.append_ironwood(height, &[])?;
        }
        self.set_scanned(
            height,
            Self::make_prior(height, hash32(hash), sap, orch, iron),
        )?;
        Ok(())
    }

    fn mark_spent(&mut self, nf: &[u8], spent_in: &str, height: u32) -> u32 {
        if nf.is_empty() {
            return 0;
        }
        let nf_hex = to_hex(nf);
        let mut hit = 0u32;
        let mut newly = false;
        let mut value = 0u64;
        for n in self.notes_mut() {
            if n.nf != nf_hex {
                continue;
            }
            if !n.spent {
                n.spent = true;
                n.spent_in = Some(spent_in.to_string());
                n.spent_height = Some(height);
                value = n.value_zat;
                newly = true;
            } else if n.spent_height.is_none() {
                n.spent_in = Some(spent_in.to_string());
                n.spent_height = Some(height);
            }
            hit = 1;
            break;
        }
        if hit == 1 {
            self.mark_spend_nf(&nf_hex, Some(spent_in));
            let agg = self.txs_mut().entry(spent_in.to_string()).or_default();
            agg.mined_height = Some(height);
            if newly {
                agg.spent_zat = agg.spent_zat.saturating_add(value);
                agg.spent_notes = agg.spent_notes.saturating_add(1);
            }
        }
        hit
    }

    fn ingest_sapling(
        &mut self,
        txid: &str,
        height: u32,
        outs: &[WalletSaplingOutput<u32>],
    ) -> Result<u32> {
        if !outs.is_empty() {
            self.prepare_received_pool("sapling");
        }
        let mut n = 0u32;
        for o in outs {
            let value = u64::from(o.note().value().inner());
            let nf = o.nf().map(|nf| to_hex(nf.as_ref())).unwrap_or_default();
            self.push_note(TrackedNote {
                pool: "sapling".into(),
                txid: txid.into(),
                output_index: o.index() as u32,
                value_zat: value,
                nf,
                is_change: o.is_change(),
                spent: false,
                spent_in: None,
                spent_height: None,
                mined_height: height,
            });
            n += 1;
        }
        Ok(n)
    }

    fn ingest_orchard(
        &mut self,
        txid: &str,
        height: u32,
        pool: &str,
        outs: &[WalletOrchardOutput<u32>],
    ) -> Result<u32> {
        if !outs.is_empty() {
            self.prepare_received_pool(pool);
        }
        let mut n = 0u32;
        let shielded = if pool == "ironwood" {
            zcash_protocol::ShieldedPool::Ironwood
        } else {
            zcash_protocol::ShieldedPool::Orchard
        };
        for o in outs {
            let value = o.note().0.value().inner();
            let nf = o
                .nf()
                .map(|nf| to_hex(nf.to_bytes().as_ref()))
                .unwrap_or_default();
            self.push_spend_note(
                shielded,
                txid,
                o.index() as u16,
                o.note_commitment_tree_position(),
                height,
                o.recipient_key_scope(),
                &o.note().0,
                o.nf(),
                o.is_change(),
            )?;
            self.push_note(TrackedNote {
                pool: pool.into(),
                txid: txid.into(),
                output_index: o.index() as u32,
                value_zat: value,
                nf,
                is_change: o.is_change(),
                spent: false,
                spent_in: None,
                spent_height: None,
                mined_height: height,
            });
            n += 1;
        }
        Ok(n)
    }

    fn push_note(&mut self, note: TrackedNote) {
        if self.notes().iter().any(|n| {
            n.txid == note.txid && n.output_index == note.output_index && n.pool == note.pool
        }) {
            let agg = self.txs_mut().entry(note.txid.clone()).or_default();
            agg.mined_height = Some(note.mined_height);
            return;
        }
        let agg = self.txs_mut().entry(note.txid.clone()).or_default();
        // A send/shield we constructed already filled spent/received; only stamp height.
        let from_wallet_construct = agg.spent_notes > 0
            || agg.is_shielding
            || (agg.mined_height.is_none() && agg.received_notes > 0);
        agg.mined_height = Some(note.mined_height);
        if !from_wallet_construct {
            agg.received_zat = agg.received_zat.saturating_add(note.value_zat);
            agg.received_notes = agg.received_notes.saturating_add(1);
            agg.has_change = agg.has_change || note.is_change;
        }
        self.notes_mut().push(note);
    }

    /// Trial-decrypt a mempool (unmined) raw transaction. Transparent outputs to our
    /// t-addr become height-0 UTXOs; Orchard notes become pending history (not spendable
    /// until the compact-block scan gives a tree position).
    pub fn apply_raw_tx(&mut self, raw: &[u8]) -> Result<u32> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};

        let height = BlockHeight::from_u32(self.scanned_height().saturating_add(1));
        let branch = BranchId::for_height(&self.network(), height);
        let tx = Transaction::read(&raw[..], branch)
            .map_err(|e| EngineError::Message(format!("mempool tx: {e}")))?;
        let txid = to_hex(tx.txid().as_ref());
        let mut found = 0u32;

        #[cfg(feature = "transparent-inputs")]
        if let Some(tb) = tx.transparent_bundle() {
            use zcash_keys::encoding::AddressCodec;
            let ours = self.transparent_address().map(str::to_string);
            for (i, out) in tb.vout.iter().enumerate() {
                let Some(addr) = out.recipient_address() else {
                    continue;
                };
                let enc = addr.encode(&self.network);
                let ours_hit = ours.as_deref() == Some(enc.as_str())
                    || self.utxos.iter().any(|u| u.address == enc);
                if !ours_hit {
                    continue;
                }
                let idx = i as u32;
                if self.utxos.iter().any(|u| u.txid == txid && u.index == idx) {
                    found += 1;
                    continue;
                }
                self.utxos.push(super::store::StoredUtxo {
                    coinbase: false,
                    txid: txid.clone(),
                    index: idx,
                    script: super::to_hex(&out.script_pubkey().0 .0),
                    value_zat: u64::from(out.value()),
                    height: 0,
                    address: enc,
                    spent: false,
                    spent_in: None,
                });
                found += 1;
            }
        }

        let memos = self.enhance_transaction(&tx)?;
        let inbound_n = self.txs().get(&txid).map(|a| a.received_notes).unwrap_or(0);
        if inbound_n > 0 {
            found = found.saturating_add(inbound_n);
        }
        let _ = memos;
        self.recompute_pool_fields();
        Ok(found)
    }

    /// Decrypt memos from a full raw transaction (`GetTransaction` / `getrawtransaction`).
    pub fn enhance_raw_tx(&mut self, raw: &[u8]) -> Result<u32> {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::{BlockHeight, BranchId};

        let height = BlockHeight::from_u32(self.scanned_height().saturating_add(1));
        let read = |branch| {
            let mut remaining = raw;
            let tx = Transaction::read(&mut remaining, branch)
                .map_err(|e| EngineError::Message(format!("enhance tx: {e}")))?;
            if !remaining.is_empty() {
                return Err(EngineError::Message("enhance tx: trailing bytes".into()));
            }
            Ok(tx)
        };
        let mut tx = read(BranchId::for_height(&self.network(), height))?;
        if let Some(mined) = self
            .txs()
            .get(&to_hex(tx.txid().as_ref()))
            .and_then(|agg| agg.mined_height)
        {
            let branch = BranchId::for_height(&self.network(), BlockHeight::from_u32(mined));
            // V4 IDs hash the serialized bytes and do not depend on this parse
            // context. Once identified, give historical V4 transactions their
            // mined branch; V5/V6 already carry it in their serialized header.
            if tx.consensus_branch_id() != branch {
                tx = read(branch)?;
            }
        }
        self.enhance_transaction(&tx)
    }

    fn enhance_transaction(
        &mut self,
        tx: &zcash_primitives::transaction::Transaction,
    ) -> Result<u32> {
        use std::collections::HashMap;
        use zcash_client_backend::decrypt_transaction;
        use zcash_client_backend::TransferType;
        use zcash_protocol::consensus::BlockHeight;
        use zcash_protocol::memo::Memo;

        let txid = to_hex(tx.txid().as_ref());
        let ufvk = self.decode_ufvk()?;
        let mut keys = HashMap::new();
        keys.insert(0u32, ufvk);
        let tip = BlockHeight::from_u32(self.scanned_height());
        // Sapling plaintext versions are height-dependent around ZIP 212.
        // Treating every historical transaction as pending at today's tip can
        // silently decrypt zero memos, which must not become a completed result.
        let mined = self
            .txs()
            .get(&txid)
            .and_then(|agg| agg.mined_height)
            .map(BlockHeight::from_u32);
        let decrypted = decrypt_transaction(&self.network(), mined, Some(tip), tx, &keys);
        let mut memos = Vec::new();
        let mut inbound_zat = 0u64;
        let mut inbound_n = 0u32;
        let mut sapling_received = 0u64;
        let mut orchard_received = 0u64;
        let mut ironwood_received = 0u64;
        let mut outgoing_shielded_zat = 0u64;
        for o in decrypted
            .orchard_outputs()
            .iter()
            .chain(decrypted.ironwood_outputs())
        {
            if matches!(
                o.transfer_type(),
                TransferType::Incoming | TransferType::Outgoing | TransferType::AccountInternal
            ) {
                if let Ok(Memo::Text(t)) = Memo::try_from(o.memo()) {
                    let s = t.to_string();
                    if !s.is_empty() && !memos.contains(&s) {
                        memos.push(s);
                    }
                }
            }
            if matches!(
                o.transfer_type(),
                TransferType::Incoming | TransferType::AccountInternal
            ) {
                let value = o.note().0.value().inner();
                inbound_zat = inbound_zat.saturating_add(value);
                let pool_received = match o.note().1 {
                    orchard::ValuePool::Orchard => &mut orchard_received,
                    orchard::ValuePool::Ironwood => &mut ironwood_received,
                };
                *pool_received = pool_received.saturating_add(value);
                inbound_n += 1;
            } else if matches!(o.transfer_type(), TransferType::Outgoing) {
                outgoing_shielded_zat =
                    outgoing_shielded_zat.saturating_add(o.note().0.value().inner());
            }
        }
        for o in decrypted.sapling_outputs() {
            if let Ok(Memo::Text(t)) = Memo::try_from(o.memo()) {
                let s = t.to_string();
                if !s.is_empty() && !memos.contains(&s) {
                    memos.push(s);
                }
            }
            if matches!(
                o.transfer_type(),
                TransferType::Incoming | TransferType::AccountInternal
            ) {
                let value = o.note().value().inner();
                sapling_received = sapling_received.saturating_add(value);
                inbound_zat = inbound_zat.saturating_add(value);
                inbound_n += 1;
            } else if matches!(o.transfer_type(), TransferType::Outgoing) {
                outgoing_shielded_zat =
                    outgoing_shielded_zat.saturating_add(o.note().value().inner());
            }
        }
        let already = self.txs().contains_key(&txid);
        let observed_output = !decrypted.sapling_outputs().is_empty()
            || !decrypted.orchard_outputs().is_empty()
            || !decrypted.ironwood_outputs().is_empty();
        // Being known from compact scan is not evidence of successful full
        // decryption. Blank incoming AND recovered outgoing memos do count.
        if !already && inbound_n == 0 && memos.is_empty() {
            return Ok(0);
        }
        let expiry = Some(u32::from(tx.expiry_height()));
        let (transparent_inputs, transparent_outputs) = tx
            .transparent_bundle()
            .map(|bundle| {
                let inputs: Vec<_> = bundle
                    .vin
                    .iter()
                    .map(|input| {
                        let prev = input.prevout();
                        super::store::HistoryOutpoint {
                            txid: WebWallet::display_txid(&to_hex(prev.hash())),
                            index: prev.n(),
                        }
                    })
                    .collect();
                let outputs: Vec<_> = bundle
                    .vout
                    .iter()
                    .enumerate()
                    .map(|(index, output)| super::store::HistoryOutput {
                        index: index as u32,
                        value_zat: u64::from(output.value()),
                    })
                    .collect();
                (inputs, outputs)
            })
            .unwrap_or_default();
        // With no transparent inputs, shielded value balances and public
        // outputs completely determine the chain fee. A transparent input
        // requires its previous output value, resolved by the history layer
        // only when that previous transaction is known to this wallet.
        let chain_fee = if transparent_inputs.is_empty() {
            let sapling = tx
                .sapling_bundle()
                .map(|bundle| i64::from(*bundle.value_balance()) as i128)
                .unwrap_or(0);
            let orchard = tx
                .orchard_bundle()
                .map(|bundle| i64::from(*bundle.value_balance()) as i128)
                .unwrap_or(0);
            let ironwood = tx
                .ironwood_bundle()
                .map(|bundle| i64::from(*bundle.value_balance()) as i128)
                .unwrap_or(0);
            let public_outputs: i128 = transparent_outputs
                .iter()
                .map(|output: &super::store::HistoryOutput| output.value_zat as i128)
                .sum();
            u64::try_from(sapling + orchard + ironwood - public_outputs).ok()
        } else {
            None
        };
        let expected_incoming = self.notes().iter().any(|note| note.txid == txid);
        let agg = self.txs_mut().entry(txid).or_default();
        if agg.mined_height.is_none() && inbound_n > 0 && agg.received_notes == 0 {
            agg.received_zat = inbound_zat;
            agg.received_notes = inbound_n;
            agg.sapling_received = sapling_received;
            agg.orchard_received = orchard_received;
            agg.ironwood_received = ironwood_received;
            agg.expiry_height = expiry;
        }
        if !memos.is_empty() {
            for m in memos {
                if !agg.memos.contains(&m) {
                    agg.memos.push(m);
                }
            }
        }
        // A mined spend with no incoming outputs may deliberately omit outgoing
        // recovery (no OVK), or have no shielded outputs at all. The same full
        // transaction cannot yield more on retry. Still require decryption when
        // compact scanning found an incoming note: historical branch/ZIP-212
        // context errors must not silently turn that missing memo into success.
        let spent_only = agg.mined_height.is_some()
            && !expected_incoming
            && agg.spent_notes > 0
            && agg.received_notes == 0
            && agg.sapling_received == 0
            && agg.orchard_received == 0
            && agg.ironwood_received == 0;
        agg.enhancement_complete |= observed_output || spent_only;
        if observed_output || spent_only {
            agg.memo_recovered = Some(agg.memo_recovered == Some(true) || observed_output);
        }
        agg.history_metadata_complete = true;
        agg.outgoing_shielded_zat = outgoing_shielded_zat;
        agg.transparent_inputs = transparent_inputs;
        agg.transparent_outputs = transparent_outputs;
        if chain_fee.is_some() {
            agg.fee_zat = chain_fee;
        }
        Ok(agg.memos.len() as u32)
    }

    pub fn apply_mempool_json(&mut self, json: &str) -> Result<u32> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("mempool json: {e}")))?;
        let arr = v
            .get("txs")
            .and_then(|x| x.as_array())
            .or_else(|| v.get("hex").and_then(|x| x.as_array()))
            .ok_or_else(|| EngineError::Message("mempool txs array required".into()))?;
        let mut n = 0u32;
        for item in arr {
            let hex = item
                .as_str()
                .or_else(|| item.get("hex").and_then(|x| x.as_str()))
                .ok_or_else(|| EngineError::Message("mempool tx hex".into()))?;
            let raw = super::from_hex(hex).map_err(EngineError::Message)?;
            n = n.saturating_add(self.apply_raw_tx(&raw)?);
        }
        self.recompute_pool_fields();
        Ok(n)
    }
}

/// Trial decryption of one blob's shielded blocks. Small batches stay serial
/// so routine new-block sync does not wake the whole pool. For larger batches
/// with a thread pool and a caller outside it (the scan worker, or a native
/// thread), blocks go to the pool as they are read. This thread ingests a
/// block once it and all earlier blocks are done, overlapping decryption instead
/// of leaving the pool idle. Otherwise the whole blob decrypts first.
struct Decrypts {
    network: crate::Network,
    /// Built on the first shielded block: empty blobs never need them.
    keys: Option<std::sync::Arc<ScanKeys>>,
    parallel: bool,
    stream: Option<Stream>,
    jobs: Vec<(usize, CompactBlock)>,
    done: Vec<Option<DecryptOut>>,
}

struct Stream {
    tx: Option<std::sync::mpsc::Sender<(usize, Result<DecryptOut>)>>,
    rx: std::sync::mpsc::Receiver<(usize, Result<DecryptOut>)>,
}

impl Decrypts {
    fn new(network: crate::Network, n: usize) -> Self {
        let parallel = n > SERIAL_SCAN_MAX_BLOCKS && rayon::current_num_threads() > 1;
        // A pool thread must not block on its own pool's jobs.
        let stream = (parallel && rayon::current_thread_index().is_none()).then(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            Stream { tx: Some(tx), rx }
        });
        Self {
            network,
            keys: None,
            parallel,
            stream,
            jobs: Vec::new(),
            done: (0..n).map(|_| None).collect(),
        }
    }

    fn keys(&mut self, wallet: &mut WebWallet) -> Result<std::sync::Arc<ScanKeys>> {
        if let Some(keys) = &self.keys {
            return Ok(keys.clone());
        }
        let keys = wallet.scan_keys()?;
        self.keys = Some(keys.clone());
        Ok(keys)
    }

    fn push(&mut self, wallet: &mut WebWallet, i: usize, block: CompactBlock) -> Result<()> {
        let Some(tx) = self.stream.as_ref().and_then(|s| s.tx.clone()) else {
            self.jobs.push((i, block));
            return Ok(());
        };
        let (network, keys) = (self.network, self.keys(wallet)?);
        rayon::spawn(move || {
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decrypt_one(network, &keys, block)
            }))
            .unwrap_or_else(|_| Err(EngineError::Message("scan_block panicked".into())));
            // The receiver is gone only after an earlier error.
            let _ = tx.send((i, out));
        });
        Ok(())
    }

    /// Every block is queued: decrypt the batch now, or let the stream end.
    fn finish_queue(&mut self, wallet: &mut WebWallet) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            stream.tx.take();
            return Ok(());
        }
        if self.jobs.is_empty() {
            return Ok(());
        }
        let (network, keys) = (self.network, self.keys(wallet)?);
        let jobs = std::mem::take(&mut self.jobs);
        let decrypt = |(i, block)| Ok((i, decrypt_one(network, &keys, block)?));
        let decrypted: Vec<(usize, DecryptOut)> = if self.parallel {
            jobs.into_par_iter()
                .with_min_len(1)
                .map(decrypt)
                .collect::<Result<Vec<_>>>()?
        } else {
            jobs.into_iter().map(decrypt).collect::<Result<Vec<_>>>()?
        };
        for (i, d) in decrypted {
            self.done[i] = Some(d);
        }
        Ok(())
    }

    fn take(&mut self, i: usize) -> Result<DecryptOut> {
        let missing = || EngineError::Message("missing shielded decrypt".into());
        loop {
            if let Some(d) = self.done[i].take() {
                return Ok(d);
            }
            let stream = self.stream.as_ref().ok_or_else(missing)?;
            let (j, d) = stream.rx.recv().map_err(|_| missing())?;
            self.done[j] = Some(d?);
        }
    }
}

enum DecryptOut {
    Scanned(zcash_client_backend::data_api::ScannedBlock<u32>),
    Empty {
        hash: Vec<u8>,
        sapling_sz: u32,
        orchard_sz: u32,
        ironwood_sz: u32,
    },
}

fn hash32(bytes: &[u8]) -> BlockHash {
    let mut a = [0u8; 32];
    let n = bytes.len().min(32);
    a[..n].copy_from_slice(&bytes[..n]);
    BlockHash(a)
}

fn empty_tree_sizes(
    block: &CompactBlock,
    prior: Option<&super::store::PriorMeta>,
) -> (u32, u32, u32) {
    if let Some(m) = &block.chain_metadata {
        return (
            m.sapling_commitment_tree_size,
            m.orchard_commitment_tree_size,
            m.ironwood_commitment_tree_size,
        );
    }
    if let Some(p) = prior {
        return (
            p.sapling_tree_size,
            p.orchard_tree_size,
            p.ironwood_tree_size,
        );
    }
    (0, 0, 0)
}

fn decrypt_one(
    network: crate::Network,
    keys: &ScanKeys,
    block: CompactBlock,
) -> Result<DecryptOut> {
    if !compact_has_shielded(&block) {
        let (sapling_sz, orchard_sz, ironwood_sz) = match &block.chain_metadata {
            Some(m) => (
                m.sapling_commitment_tree_size,
                m.orchard_commitment_tree_size,
                m.ironwood_commitment_tree_size,
            ),
            None => (0, 0, 0),
        };
        return Ok(DecryptOut::Empty {
            hash: block.hash,
            sapling_sz,
            orchard_sz,
            ironwood_sz,
        });
    }
    scan_block(&network, block, keys, &Nullifiers::empty(), None)
        .map(DecryptOut::Scanned)
        .map_err(|e| EngineError::Message(format!("scan_block: {e}")))
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
