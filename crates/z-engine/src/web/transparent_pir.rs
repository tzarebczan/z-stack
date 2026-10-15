use super::*;
use crate::transparent_pir::{
    parse, refused, TransparentPirAnchor, TransparentPirCheckpoint, TransparentPirScript,
};
use transparent_events::TransparentEvent;

impl WebWallet {
    pub(crate) fn reconcile_transparent_pir_history(&mut self) {
        let owned: std::collections::BTreeSet<_> = self
            .transparent_pir
            .projected_spends
            .iter()
            .filter_map(|key| {
                let fields: Vec<_> = key.split(':').collect();
                match fields.as_slice() {
                    ["s", spender, _, previous, index] => {
                        Some((*spender, *previous, index.parse::<u32>().ok()?))
                    }
                    _ => None,
                }
            })
            .collect();
        let mut totals = std::collections::BTreeMap::<&str, (u64, u32)>::new();
        for output in &self.utxos {
            let Some(spender) = output.spent_in.as_deref().filter(|_| output.spent) else {
                continue;
            };
            if owned.contains(&(spender, output.txid.as_str(), output.index)) {
                let total = totals.entry(spender).or_default();
                total.0 = total.0.saturating_add(output.value_zat);
                total.1 = total.1.saturating_add(1);
            }
        }
        let mut receipts = std::collections::BTreeMap::<&str, (u64, u32)>::new();
        for output in &self.utxos {
            if self.pir_receipt_unaccounted(&output.txid, output.index) {
                let total = receipts.entry(output.txid.as_str()).or_default();
                total.0 = total.0.saturating_add(output.value_zat);
                total.1 = total.1.saturating_add(1);
            }
        }
        for (txid, agg) in &mut self.txs {
            let (value, count) = totals.get(txid.as_str()).copied().unwrap_or_default();
            agg.pir_spent_zat = value;
            agg.pir_spent_notes = count;
            let (value, count) = receipts.get(txid.as_str()).copied().unwrap_or_default();
            if let Some(received) = &mut agg.pir_received_zat {
                *received = value;
                agg.pir_received_notes = count;
            }
        }
    }

    /// The caller has accepted this exact transparent publisher profile. All scripts
    /// and the target are obtained from this wallet; no host-provided amount is used.
    pub fn begin_transparent_pir(&mut self, source_id: &str) -> Result<String> {
        if self.transparent_pir_rescan_required() {
            return Err(refused("transparent_pir_rescan_required"));
        }
        let height = self.scanned_height;
        let mut hash = super::super::from_hex(
            self.hash_at(height)
                .ok_or_else(|| refused("transparent_pir_anchor_unavailable"))?,
        )
        .map_err(|_| refused("transparent_pir_anchor_unavailable"))?;
        hash.reverse();
        let anchor = TransparentPirAnchor {
            height: u64::from(height),
            hash: super::super::to_hex(&hash),
        };
        let scripts = self
            .public_transparent_scripts()?
            .into_iter()
            .map(|(script, _)| TransparentPirScript {
                script: super::super::to_hex(&script),
                required_from: 0,
            })
            .collect();
        let mut scope = self.transparent_pir.begin(source_id, anchor, scripts)?;
        scope.accepted_headers.retain(|h| h.height != 0);
        scope.accepted_headers.push(TransparentPirAnchor {
            height: 0,
            hash: self.network.genesis_hash().into(),
        });
        self.transparent_pir.scope = Some(scope.clone());
        self.transparent_pir
            .unverified_outputs
            .get_or_insert_with(Default::default);
        self.transparent_pir.wallet_cursor = Some(self.next_diversifier);
        serde_json::to_string(&scope).map_err(|_| refused("transparent_pir_scope_invalid"))
    }

    pub fn transparent_pir_checkpoint(&self) -> Option<String> {
        self.transparent_pir
            .continuation
            .as_ref()
            .and_then(|c| serde_json::to_string(c).ok())
    }

    pub(crate) fn transparent_pir_rescan_required(&self) -> bool {
        let legacy_receipts = self.transparent_pir.projected_outputs.iter().any(|key| {
            key.split(':')
                .nth(1)
                .and_then(|id| self.txs.get(id))
                .is_some_and(|agg| agg.pir_received_zat.is_none())
        });
        self.legacy_transparent_receipt_accounting()
            || legacy_receipts
            || self.transparent_pir.unverified_outputs.is_none()
                && (self.transparent_pir.bound_source.is_some()
                    || self.transparent_pir.scope.is_some()
                    || self.transparent_pir.checkpoint_id.is_some()
                    || !self.transparent_pir.records.is_empty()
                    || !self.transparent_pir.projected_outputs.is_empty())
    }

    pub fn transparent_pir_status(&self) -> serde_json::Value {
        let mut state = self.transparent_pir.clone();
        let rescan_required = self.transparent_pir_rescan_required();
        if rescan_required {
            state.complete = false;
        }
        if state
            .scope
            .as_ref()
            .is_some_and(|scope| scope.anchor.height != u64::from(self.scanned_height))
        {
            state.complete = false;
        }
        if state.wallet_cursor != Some(self.next_diversifier) {
            state.complete = false;
        }
        if let Some(scope) = &state.scope {
            let known = u32::try_from(scope.anchor.height)
                .ok()
                .and_then(|h| self.hash_at(h))
                .and_then(|h| super::super::from_hex(h).ok())
                .map(|mut h| {
                    h.reverse();
                    super::super::to_hex(&h)
                });
            if known.as_deref() != Some(&scope.anchor.hash) {
                state.complete = false;
            }
        }
        let mut status = state.status();
        status["rescanRequired"] = serde_json::json!(rescan_required);
        let total: u64 = self
            .utxos
            .iter()
            .filter(|u| {
                !u.spent
                    && self
                        .transparent_pir
                        .records
                        .contains_key(&format!("r:{}:{}", u.txid, u.index))
            })
            .map(|u| u.value_zat)
            .sum();
        status["confirmedZat"] = serde_json::json!(total.to_string());
        status
    }

    pub fn apply_transparent_pir_checkpoint(&mut self, json: &str) -> Result<String> {
        if self.transparent_pir_rescan_required() {
            return Err(refused("transparent_pir_rescan_required"));
        }
        let checkpoint: TransparentPirCheckpoint = parse(json)?;
        let mut state = self.transparent_pir.clone();
        let old_records = state.records.clone();
        let retirement_floor = state
            .rewind_floor
            .into_iter()
            .chain(checkpoint.rewind_to.as_ref().map(|a| a.height as u32))
            .min();
        if let Some(floor) = retirement_floor {
            state.prepare_rewind(floor);
        }
        if let Some(ancestor) = &checkpoint.rewind_to {
            let height = u32::try_from(ancestor.height)
                .map_err(|_| refused("transparent_pir_anchor_invalid"))?;
            let known = if height == 0 {
                Some(self.network.genesis_hash().to_string())
            } else {
                self.hash_at(height)
                    .and_then(|h| super::super::from_hex(h).ok())
                    .map(|mut h| {
                        h.reverse();
                        super::super::to_hex(&h)
                    })
            };
            if known.as_deref() != Some(&ancestor.hash) {
                return Err(refused("transparent_pir_rewind_anchor_unknown"));
            }
            state.prepare_rewind(height);
        }
        let records = state.validate(&checkpoint)?;
        let height = u32::try_from(checkpoint.anchor.height)
            .map_err(|_| refused("transparent_pir_anchor_invalid"))?;
        let mut hash = super::super::from_hex(&checkpoint.anchor.hash)
            .map_err(|_| refused("transparent_pir_anchor_invalid"))?;
        hash.reverse();
        if self.hash_at(height) != Some(super::super::to_hex(&hash).as_str())
            || self.scanned_height != height
        {
            return Err(refused("transparent_pir_chain_changed"));
        }
        if self.transparent_pir.checkpoint_id.as_ref() == Some(&checkpoint.checkpoint_id)
            && !self.transparent_pir.projection_invalidated
        {
            return serde_json::to_string(&self.transparent_pir_status())
                .map_err(|_| refused("transparent_pir_status_invalid"));
        }
        if retirement_floor.is_some() && !checkpoint.complete {
            state.records = old_records;
            state.stage(&checkpoint, &[])?;
            state.rewind_floor = retirement_floor;
            self.transparent_pir = state;
            return serde_json::to_string(&self.transparent_pir_status())
                .map_err(|_| refused("transparent_pir_status_invalid"));
        }
        let scripts = self.public_transparent_scripts()?;
        // Stage all canonical effects on clones. A malformed late record cannot
        // partially mutate outputs, pending reservations or transaction history.
        let mut utxos = self.utxos.clone();
        let mut txs = self.txs.clone();
        let mut next_diversifier = self.next_diversifier;
        if let Some(floor) = retirement_floor {
            let incoming: std::collections::BTreeMap<_, _> = records
                .iter()
                .map(|(_, e)| {
                    (
                        crate::transparent_pir::event_key(*e),
                        crate::transparent_pir::digest(&e.to_bytes()),
                    )
                })
                .collect();
            let retired: Vec<_> = old_records
                .iter()
                .filter(|(key, (h, d))| *h > floor && incoming.get(*key) != Some(d))
                .map(|(k, _)| k.clone())
                .collect();
            for key in &retired {
                let fields: Vec<_> = key.split(':').collect();
                if fields.first() == Some(&"s") && state.projected_spends.remove(key) {
                    let spending = Self::txid_from_hex(fields[1])?;
                    if self.pending_txs.contains_key(&spending) {
                        return Err(refused("transparent_pir_pending_reconciliation_required"));
                    }
                    let index: u32 = fields[4]
                        .parse()
                        .map_err(|_| refused("transparent_pir_state_invalid"))?;
                    if let Some(u) = utxos.iter_mut().find(|u| {
                        u.txid == fields[3]
                            && u.index == index
                            && u.spent_in.as_deref() == Some(fields[1])
                    }) {
                        u.spent = false;
                        u.spent_in = None;
                    }
                }
            }
            for key in &retired {
                let fields: Vec<_> = key.split(':').collect();
                if fields.first() == Some(&"r") && state.projected_outputs.remove(key) {
                    let index: u32 = fields[2]
                        .parse()
                        .map_err(|_| refused("transparent_pir_state_invalid"))?;
                    if self
                        .output_locks
                        .iter()
                        .any(|l| l.txid == fields[1] && l.index == index && l.pool == "transparent")
                        || utxos.iter().any(|u| {
                            u.txid == fields[1]
                                && u.index == index
                                && u.spent_in.as_ref().is_some_and(|t| {
                                    Self::txid_from_hex(t)
                                        .ok()
                                        .is_some_and(|id| self.pending_txs.contains_key(&id))
                                })
                        })
                    {
                        return Err(refused("transparent_pir_pending_reconciliation_required"));
                    }
                    utxos.retain(|u| u.txid != fields[1] || u.index != index);
                    if let Some(unverified) = &mut state.unverified_outputs {
                        unverified.remove(key);
                    }
                }
            }
            for id in &state.projected_transactions {
                if !self.pending_txs.contains_key(&Self::txid_from_hex(id)?) {
                    if let Some(agg) = txs.get_mut(id) {
                        // Only PIR owns this inclusion stamp. Independent compact
                        // observations transfer that ownership before retirement;
                        // raw enhancement retains facts, but proves no inclusion.
                        if agg.mined_height.is_some_and(|h| h > floor) {
                            agg.mined_height = None;
                        }
                    }
                }
            }
        }
        for (script, event) in &records {
            let (position, (_, address)) = scripts
                .iter()
                .enumerate()
                .find(|(_, (s, _))| s == script)
                .ok_or_else(|| refused("transparent_pir_scope_changed"))?;
            match event {
                TransparentEvent::Receive(e) => {
                    let txid = super::super::to_hex(&e.txid.0);
                    let mut mined = true;
                    if let Some(u) = utxos
                        .iter_mut()
                        .find(|u| u.txid == txid && u.index == e.output_index)
                    {
                        if u.value_zat != e.value
                            || u.script != super::super::to_hex(script)
                            || (u.height != 0 && u.height != e.height)
                        {
                            return Err(refused("transparent_pir_output_conflict"));
                        }
                        // Existing funding belongs to its original canonical source.
                        // PIR cannot mine an ordinary pending output or downgrade coinbase rules.
                        mined = u.height > 0;
                    } else {
                        state
                            .projected_outputs
                            .insert(crate::transparent_pir::event_key(*event));
                        state
                            .unverified_outputs
                            .get_or_insert_with(Default::default)
                            .insert(crate::transparent_pir::event_key(*event));
                        utxos.push(StoredUtxo {
                            txid: txid.clone(),
                            index: e.output_index,
                            script: super::super::to_hex(script),
                            value_zat: e.value,
                            height: e.height,
                            address: address.clone(),
                            coinbase: CoinbaseStatus::from_unverified(e.coinbase),
                            spent: false,
                            spent_in: None,
                        });
                    }
                    if !txs.contains_key(&txid) {
                        state.projected_transactions.insert(txid.clone());
                    }
                    let owned_receipt = state
                        .projected_outputs
                        .contains(&crate::transparent_pir::event_key(*event));
                    let agg = txs.entry(txid).or_default();
                    if owned_receipt {
                        agg.pir_received_zat.get_or_insert(0);
                    }
                    if agg.mined_height.is_some_and(|h| h != e.height) {
                        return Err(refused("transparent_pir_transaction_conflict"));
                    }
                    if mined {
                        agg.mined_height = Some(e.height);
                    }
                    next_diversifier = next_diversifier.max(
                        u32::try_from(position / 2 + 1)
                            .map_err(|_| refused("transparent_pir_scope_limit"))?,
                    );
                }
                TransparentEvent::Spend(e) => {
                    let txid = super::super::to_hex(&e.spending_txid.0);
                    let previous = super::super::to_hex(&e.spent_txid.0);
                    if let Some(u) = utxos
                        .iter_mut()
                        .find(|u| u.txid == previous && u.index == e.spent_output_index)
                    {
                        if u.script != super::super::to_hex(script)
                            || (u.spent && u.spent_in.as_ref().is_some_and(|known| known != &txid))
                        {
                            return Err(refused("transparent_pir_spend_conflict"));
                        }
                        if !u.spent {
                            state
                                .projected_spends
                                .insert(crate::transparent_pir::event_key(*event));
                        }
                        u.spent = true;
                        u.spent_in = Some(txid.clone());
                    } else if checkpoint.complete {
                        return Err(refused("transparent_pir_unresolved_spend"));
                    }
                    if !txs.contains_key(&txid) {
                        state.projected_transactions.insert(txid.clone());
                    }
                    let agg = txs.entry(txid).or_default();
                    if agg.mined_height.is_some_and(|h| h != e.height) {
                        return Err(refused("transparent_pir_transaction_conflict"));
                    }
                    agg.mined_height = Some(e.height);
                }
            }
        }
        let mut unspent = 0u64;
        let mut spends = std::collections::BTreeMap::<&str, u64>::new();
        for u in &utxos {
            let total = if let Some(spender) = u.spent_in.as_deref() {
                spends.entry(spender).or_default()
            } else if !u.spent {
                &mut unspent
            } else {
                continue;
            };
            *total = total
                .checked_add(u.value_zat)
                .ok_or_else(|| refused("transparent_pir_amount_invalid"))?;
            zcash_protocol::value::Zatoshis::from_u64(*total)
                .map_err(|_| refused("transparent_pir_amount_invalid"))?;
        }
        state.stage(&checkpoint, &records)?;
        state.projection_invalidated = false;
        // A received gap address expands inventory, so this checkpoint cannot
        // assert completeness for the new scripts until a further pass covers them.
        if next_diversifier != self.next_diversifier {
            state.complete = false;
        }
        self.utxos = utxos;
        self.txs = txs;
        self.next_diversifier = next_diversifier;
        self.transparent_pir = state;
        self.recompute_pool_fields();
        if retirement_floor.is_some() {
            self.txs.retain(|id, agg| {
                !self.transparent_pir.projected_transactions.contains(id)
                    || agg.mined_height.is_some()
                    || agg.received_zat > 0
                    || agg.spent_zat > 0
                    || agg.received_notes > 0
                    || agg.spent_notes > 0
                    || agg.transparent_received > 0
                    || agg.transparent_spent > 0
                    || agg.has_shielded_activity()
                    || agg.history_metadata_complete
                    || agg.receipt_totals_recorded
                    || Self::txid_from_hex(id)
                        .ok()
                        .is_some_and(|id| self.pending_txs.contains_key(&id))
            });
            self.transparent_pir
                .projected_transactions
                .retain(|id| self.txs.contains_key(id));
        }
        serde_json::to_string(&self.transparent_pir_status())
            .map_err(|_| refused("transparent_pir_status_invalid"))
    }

    pub fn acknowledge_transparent_pir_checkpoint(&mut self, id: &str) -> Result<()> {
        self.transparent_pir.acknowledge(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC};
    use crate::transparent_pir::{TransparentPirRecord, TransparentPirScope};
    use transparent_events::{ReceiveEvent, SpendEvent, Txid};
    use z_transparent_pir_v2::Coverage;

    pub(super) fn wallet() -> WebWallet {
        let mut w = WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            1,
        )
        .unwrap();
        w.scanned_height = 3;
        w.block_hashes.insert(3, "03".repeat(32));
        w
    }
    pub(super) fn checkpoint(
        w: &mut WebWallet,
        events: Vec<TransparentEvent>,
    ) -> TransparentPirCheckpoint {
        let scope: TransparentPirScope =
            serde_json::from_str(&w.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
        let script = super::super::super::to_hex(&w.public_transparent_scripts().unwrap()[0].0);
        let through = u64::from(w.scanned_height);
        TransparentPirCheckpoint {
            source_id: scope.source_id,
            generation: scope.generation,
            scope_id: scope.scope_id,
            anchor: scope.anchor,
            checkpoint_id: "cd".repeat(32),
            previous_checkpoint_id: None,
            covered_through: through,
            complete: true,
            pending_pages: 0,
            unresolved_spends: 0,
            records: events
                .into_iter()
                .map(|e| TransparentPirRecord {
                    script: script.clone(),
                    event: super::super::super::to_hex(&e.to_bytes()),
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
                    through,
                    revision: "ef".repeat(32),
                })
                .collect(),
            rewind_to: None,
            continuation: String::new(),
        }
    }
    pub(super) fn receive() -> TransparentEvent {
        TransparentEvent::Receive(ReceiveEvent {
            metadata: None,
            height: 1,
            txid: Txid([1; 32]),
            transaction_index: 1,
            output_index: 0,
            value: 150000,
            coinbase: false,
        })
    }
    pub(super) fn spend() -> TransparentEvent {
        TransparentEvent::Spend(SpendEvent {
            metadata: None,
            height: 2,
            spending_txid: Txid([2; 32]),
            transaction_index: 1,
            input_index: 0,
            spent_txid: Txid([1; 32]),
            spent_output_index: 0,
        })
    }
    fn public_range(
        w: &mut WebWallet,
        with_spend: bool,
    ) -> Vec<zcash_client_backend::proto::compact_formats::CompactBlock> {
        use zcash_client_backend::proto::compact_formats::{
            CompactBlock, CompactTx, CompactTxIn, TxOut,
        };
        let script = w.public_transparent_scripts().unwrap()[0].0.clone();
        let mut blocks: Vec<_> = (1..=3)
            .map(|h| CompactBlock {
                height: h,
                hash: vec![h as u8; 32],
                prev_hash: vec![h as u8 - 1; 32],
                vtx: vec![CompactTx {
                    index: 0,
                    txid: vec![h as u8 + 100; 32],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect();
        blocks[0].vtx.push(CompactTx {
            index: 1,
            txid: vec![1; 32],
            vout: vec![TxOut {
                value: 150000,
                script_pub_key: script,
            }],
            ..Default::default()
        });
        if with_spend {
            blocks[1].vtx.push(CompactTx {
                index: 1,
                txid: vec![2; 32],
                vin: vec![CompactTxIn {
                    prevout_txid: vec![1; 32],
                    prevout_index: 0,
                }],
                ..Default::default()
            });
        }
        blocks
    }
    fn apply_pir(w: &mut WebWallet, c: &TransparentPirCheckpoint) {
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(c).unwrap())
            .unwrap();
        w.acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
            .unwrap();
    }
    fn replace_pir(
        w: &mut WebWallet,
        previous: String,
        events: Vec<TransparentEvent>,
        ancestor: u64,
    ) {
        let mut c = checkpoint(w, events);
        c.checkpoint_id = "de".repeat(32);
        c.previous_checkpoint_id = Some(previous);
        c.rewind_to = Some(TransparentPirAnchor {
            height: ancestor,
            hash: if ancestor == 0 {
                Network::Regtest.genesis_hash().into()
            } else {
                format!("{ancestor:02x}").repeat(32)
            },
        });
        apply_pir(w, &c);
    }
    #[test]
    fn header_progress_checkpoint_preserves_projection_reservation_and_pending_marker() {
        use zcash_client_backend::data_api::OutputLockStore;
        use zcash_client_backend::wallet::{LockOwner, OutputRef};
        use zcash_primitives::transaction::TxId as WalletTxId;
        use zcash_protocol::PoolType;

        let mut w = wallet();
        let mut last_receive = receive();
        if let TransparentEvent::Receive(e) = &mut last_receive {
            e.txid = Txid([3; 32]);
            e.height = 3;
            e.value = 17000;
        }
        let c = checkpoint(&mut w, vec![receive(), spend(), last_receive]);
        apply_pir(&mut w, &c);
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "17000");
        assert_eq!(w.history(20).len(), 3);
        let owner = LockOwner::new([9; 32]);
        w.lock_outputs(
            &[OutputRef::new(
                WalletTxId::from_bytes([3; 32]),
                PoolType::TRANSPARENT,
                0,
            )],
            owner,
            100.into(),
        )
        .unwrap();
        // A receipt-persistence marker, not a signed transaction or send fixture.
        let pending_id = WalletTxId::from_bytes([9; 32]);
        w.txs.entry("09".repeat(32)).or_default();
        w.pending_txs.insert(pending_id, vec![0xaa]);
        let canonical = serde_json::to_value((&w.utxos, &w.txs)).unwrap();
        assert_eq!(w.output_locks.len(), 1);
        let pending = w.pending_txs.clone();
        let mut progress = c.clone();
        progress.previous_checkpoint_id = Some(c.checkpoint_id);
        progress.checkpoint_id = "de".repeat(32);
        progress.complete = false;
        progress.continuation = "opaque-verified-header-progress".into();
        apply_pir(&mut w, &progress);
        assert_eq!(serde_json::to_value((&w.utxos, &w.txs)).unwrap(), canonical);
        assert_eq!(w.output_locks.len(), 1);
        assert_eq!(w.output_locks[0].expiry, 100);
        assert_eq!(
            w.active_lock_owner(&"03".repeat(32), "transparent", 0),
            Some("09".repeat(32).as_str())
        );
        assert_eq!(w.pending_txs, pending);
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "17000");
        assert_eq!(w.transparent_pir_status()["completion"], "incomplete");
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value((&restored.utxos, &restored.txs)).unwrap(),
            canonical
        );
        assert_eq!(restored.pending_txs, pending);
        assert_eq!(restored.transparent_pir_status()["confirmedZat"], "17000");
        assert_eq!(
            restored.transparent_pir_status()["completion"],
            "incomplete"
        );
        let saved: TransparentPirCheckpoint =
            serde_json::from_str(&restored.transparent_pir_checkpoint().unwrap()).unwrap();
        assert_eq!(saved.continuation, progress.continuation);
    }
    #[test]
    fn independently_verified_inputs_can_shield_after_pir_enrollment() {
        for compact in [true, false] {
            let mut w = wallet();
            let mut unverified = receive();
            if let TransparentEvent::Receive(e) = &mut unverified {
                e.txid = Txid([3; 32]);
                e.height = 3;
                e.value = 70000;
            }
            let c = checkpoint(&mut w, vec![receive(), unverified]);
            apply_pir(&mut w, &c);
            assert!(w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());
            assert!(w.pending_txs.is_empty());
            if compact {
                let blocks = public_range(&mut w, false);
                w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
                    .unwrap();
            } else {
                let blocks = public_range(&mut w, false);
                w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
                    blocks[0].clone()
                ]))
                .unwrap();
                assert_eq!(w.balance().transparent_available, 0);
                let rows: Vec<_> = w.utxos.iter().map(|u| serde_json::json!({
                    "txid":u.txid,"index":u.index,"script":u.script,
                    "valueZat":u.value_zat,"height":if u.txid == "01".repeat(32) {1} else {0},
                    "address":u.address
                })).collect();
                w.apply_utxos_json(&serde_json::json!({"utxos":rows}).to_string())
                    .unwrap();
            }
            assert!(w.transparent_pir.scope.is_some());
            assert_eq!(w.balance().transparent_available, 150000);
            assert_eq!(w.balance().transparent_pending, 70000);
            let raw = w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).unwrap();
            assert!(raw.len() > 100);
            assert!(
                w.utxos
                    .iter()
                    .find(|u| u.txid == "01".repeat(32))
                    .unwrap()
                    .spent
            );
            assert!(
                !w.utxos
                    .iter()
                    .find(|u| u.txid == "03".repeat(32))
                    .unwrap()
                    .spent
            );
            assert!(!w.pending_txs.is_empty());
            assert_eq!(w.balance().transparent_pending, 70000);
        }
    }
    #[test]
    fn independent_compact_receive_remains_ready_after_pir_enrollment() {
        let mut w = wallet();
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        assert_eq!(w.balance().transparent_available, 150000);
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        assert_eq!(w.balance().transparent_available, 150000);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 150000);
    }
    #[test]
    fn pir_only_receive_waits_for_full_public_coverage_and_survives_pir_retirement() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 150000);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
            blocks[0].clone()
        ]))
        .unwrap();
        assert_eq!(w.balance().transparent_available, 0);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(
            blocks[1..].iter().cloned(),
        ))
        .unwrap();
        assert_eq!(w.balance().transparent_available, 150000);
        replace_pir(&mut w, c.checkpoint_id, vec![], 0);
        assert_eq!(w.balance().transparent_available, 150000);
        assert_eq!(w.history(20).len(), 1);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 150000);
    }
    #[test]
    fn public_compact_spend_cannot_be_undone_by_pir_retirement() {
        let mut w = wallet();
        w.block_hashes.insert(1, "01".repeat(32));
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        apply_pir(&mut w, &c);
        let blocks = public_range(&mut w, true);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
        assert_eq!(w.balance().transparent_available, 0);
        assert!(w.utxos[0].spent);
        assert_eq!(w.history(20).len(), 2);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        assert!(restored.utxos[0].spent);
    }
    #[test]
    fn legacy_false_or_absent_coinbase_is_unknown_through_refresh_and_wallet_write() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        for legacy in [None, Some(false), Some(true)] {
            let mut w = wallet();
            let c = checkpoint(&mut w, vec![receive()]);
            apply_pir(&mut w, &c);
            let mut saved: serde_json::Value =
                serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
            if let Some(value) = legacy {
                saved["utxos"][0]["coinbase"] = serde_json::json!(value);
            } else {
                saved["utxos"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("coinbase");
            }
            let mut restored =
                WebWallet::from_snapshot(&serde_json::to_vec(&saved).unwrap()).unwrap();
            let u = restored.utxos[0].clone();
            let expected = CoinbaseStatus::from_unverified(legacy.unwrap_or(false));
            assert_eq!(u.coinbase, expected);
            let row = serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,"valueZat":u.value_zat,"height":u.height,"address":u.address,"coinbase":false});
            restored
                .apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
                .unwrap();
            assert_eq!(restored.utxos[0].coinbase, expected);
            assert_eq!(restored.balance().transparent_available, 0);
            let output = WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new([1; 32], 0),
                transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(150000).unwrap(),
                    transparent::address::Script(zcash_script::script::Code(
                        super::super::super::from_hex(&u.script).unwrap(),
                    )),
                ),
                Some(1.into()),
                None,
                None,
                None,
            )
            .unwrap();
            restored.put_received_transparent_utxo(&output).unwrap();
            assert_eq!(restored.utxos[0].coinbase, expected);
            assert_eq!(restored.balance().transparent_available, 0);
            let cold = WebWallet::from_snapshot(&restored.to_snapshot().unwrap()).unwrap();
            assert_eq!(cold.utxos[0].coinbase, expected);
            assert_eq!(cold.balance().transparent_available, 0);
        }
    }

    #[test]
    fn public_non_coinbase_classification_survives_refresh_and_cold_reopen() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::NonCoinbase);
        let u = w.utxos[0].clone();
        let row = serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,"valueZat":u.value_zat,"height":u.height,"address":u.address});
        w.apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::NonCoinbase);
        assert_eq!(w.balance().transparent_available, 150000);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.utxos[0].coinbase, CoinbaseStatus::NonCoinbase);
        assert_eq!(cold.balance().transparent_available, 150000);
    }

    #[test]
    fn actual_full_transaction_classifies_unknown_zero_conf_without_pir() {
        let mut w = wallet();
        let script = w.public_transparent_scripts().unwrap()[0].0.clone();
        // Disposable structurally valid legacy non-coinbase bytes, not a consensus fixture.
        let mut raw = 1u32.to_le_bytes().to_vec();
        raw.push(1);
        raw.extend([7; 32]);
        raw.extend(0u32.to_le_bytes());
        raw.push(0);
        raw.extend(u32::MAX.to_le_bytes());
        raw.push(1);
        raw.extend(150000u64.to_le_bytes());
        raw.push(u8::try_from(script.len()).unwrap());
        raw.extend(script);
        raw.extend(0u32.to_le_bytes());
        let tx = zcash_primitives::transaction::Transaction::read(
            &raw[..],
            zcash_protocol::consensus::BranchId::Nu6_3,
        )
        .unwrap();
        let txid = super::super::super::to_hex(tx.txid().as_ref());
        let row = serde_json::json!({"txid":txid,"index":0,"script":super::super::super::to_hex(&w.public_transparent_scripts().unwrap()[0].0),"valueZat":150000,"height":0,"address":w.public_transparent_scripts().unwrap()[0].1});
        w.apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::Unknown);
        assert_eq!(w.balance().transparent_available, 0);
        w.apply_raw_tx(&raw).unwrap();
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::NonCoinbase);
        assert_eq!(w.balance().transparent_available, 150000);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_available, 150000);
    }

    #[test]
    fn address_refresh_preserves_immature_pir_coinbase_maturity() {
        let mut w = wallet();
        let mut reward = receive();
        if let TransparentEvent::Receive(event) = &mut reward {
            event.coinbase = true;
        }
        let c = checkpoint(&mut w, vec![reward]);
        apply_pir(&mut w, &c);
        assert_eq!(w.balance().transparent_available, 0);
        let u = w.utxos[0].clone();
        // Exact fields returned by GetAddressUtxos: coinbase is absent.
        let listed = serde_json::json!({"utxos":[{
            "txid":u.txid,"index":u.index,"script":u.script,
            "valueZat":u.value_zat,"height":u.height,"address":u.address
        }]});
        w.apply_utxos_json(&listed.to_string()).unwrap();
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 150000);
        assert!(w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());
        assert!(w.pending_txs.is_empty());
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        assert_eq!(restored.balance().transparent_pending, 150000);
        w.scanned_height = 99;
        assert_eq!(w.balance().transparent_available, 0);
        w.scanned_height = 100;
        assert_eq!(w.balance().transparent_available, 150000);
    }

    #[test]
    fn independent_address_refresh_promotes_pir_receive_without_releasing_pending_spend() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        // A public prefix establishes transaction structure, not complete unspent coverage.
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
            blocks[0].clone()
        ]))
        .unwrap();
        assert_eq!(w.balance().transparent_available, 0);

        let u = w.utxos[0].clone();
        let listed = serde_json::json!({"utxos":[{"txid":u.txid,"index":u.index,"script":u.script,"valueZat":u.value_zat,"height":u.height,"address":u.address}]});
        w.apply_utxos_json(&listed.to_string()).unwrap();
        assert_eq!(w.balance().transparent_available, 150000);
        w.utxos[0].spent = true;
        w.utxos[0].spent_in = Some("09".repeat(32));
        w.txs.entry("09".repeat(32)).or_default();
        w.pending_txs.insert(
            zcash_primitives::transaction::TxId::from_bytes([9; 32]),
            vec![0xaa],
        );
        let pending = w.pending_txs.clone();
        w.apply_utxos_json(&listed.to_string()).unwrap();
        assert_eq!(w.balance().transparent_available, 0);
        assert!(w.utxos[0].spent);
        assert_eq!(
            w.utxos[0].spent_in.as_deref(),
            Some("0909090909090909090909090909090909090909090909090909090909090909")
        );
        assert!(w.pending_txs == pending);
        replace_pir(&mut w, c.checkpoint_id, vec![], 0);
        assert!(w.utxos[0].spent);
        assert!(w.pending_txs == pending);
    }
    #[test]
    fn confirmed_wallet_write_waits_for_transaction_structure_evidence() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        let script = transparent::address::Script(zcash_script::script::Code(
            super::super::super::from_hex(&w.utxos[0].script).unwrap(),
        ));
        for mined in [None, Some(1.into())] {
            let output = WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new([1; 32], 0),
                transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(150000).unwrap(),
                    script.clone(),
                ),
                mined,
                None,
                None,
                None,
            )
            .unwrap();
            w.put_received_transparent_utxo(&output).unwrap();
            assert_eq!(w.balance().transparent_available, 0);
            assert_eq!(
                w.pir_funding_unverified(&"01".repeat(32), 0),
                mined.is_none()
            );
        }
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        assert_eq!(w.balance().transparent_available, 150000);
        replace_pir(&mut w, c.checkpoint_id, vec![], 0);
        assert_eq!(w.balance().transparent_available, 150000);
        assert_eq!(w.history(20).len(), 1);
    }

    #[test]
    fn pir_does_not_promote_preexisting_unmined_or_coinbase_funding() {
        for (height, coinbase) in [(0, false), (1, true)] {
            let mut w = wallet();
            let script = w.public_transparent_scripts().unwrap()[0].0.clone();
            w.apply_utxos_json(
                &serde_json::json!({"utxos":[{
                    "txid":"01".repeat(32),"index":0,"script":super::super::super::to_hex(&script),
                    "valueZat":150000,"height":height,"coinbase":coinbase
                }]})
                .to_string(),
            )
            .unwrap();
            let available = w.balance().transparent_available;
            let c = checkpoint(&mut w, vec![receive()]);
            apply_pir(&mut w, &c);
            assert_eq!(w.utxos[0].height, height);
            assert_eq!(
                w.utxos[0].coinbase,
                CoinbaseStatus::from_unverified(coinbase)
            );
            assert_eq!(w.balance().transparent_available, available);
            replace_pir(&mut w, c.checkpoint_id, vec![], 0);
            assert_eq!(w.utxos[0].height, height);
            assert_eq!(w.balance().transparent_available, available);
        }
    }

    #[test]
    fn legacy_pir_snapshot_stays_unspendable_and_requires_explicit_rescan() {
        let mut w = wallet();
        assert_eq!(w.transparent_pir_status()["rescanRequired"], false);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        assert_eq!(w.transparent_pir_status()["rescanRequired"], false);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        legacy["transparentPir"]
            .as_object_mut()
            .unwrap()
            .remove("unverifiedOutputs");
        legacy["transparentPir"]["publicFundingIsolated"] = serde_json::json!(true);
        let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        assert_eq!(restored.transparent_pir_status()["rescanRequired"], true);
        let before = restored.to_snapshot().unwrap();
        assert!(restored
            .begin_transparent_pir(&"ab".repeat(32))
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_rescan_required"));
        assert_eq!(restored.to_snapshot().unwrap(), before);
        assert!(restored
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_rescan_required"));
        assert_eq!(restored.to_snapshot().unwrap(), before);
        let continuation = restored.transparent_pir_checkpoint();
        restored.rescan_from(1).unwrap();
        assert_eq!(restored.transparent_pir_status()["rescanRequired"], false);
        assert!(restored.transparent_pir_checkpoint() == continuation);
        restored.scanned_height = 3;
        restored.block_hashes.insert(3, "03".repeat(32));
        apply_pir(&mut restored, &c);
        assert_eq!(restored.balance().transparent_available, 0);
        assert_eq!(restored.balance().transparent_pending, 150000);
    }

    #[test]
    fn explicit_rescan_preserves_pir_acquisition_and_issued_scripts() {
        for birthday in [1, 2] {
            let mut w = wallet();
            w.birthday = 2;
            w.next_unified_address().unwrap();
            w.next_unified_address().unwrap();
            let scripts = w.public_transparent_scripts().unwrap();
            let address = w.unified_address.clone();
            let cursor = w.next_diversifier;
            let mut c = checkpoint(&mut w, vec![receive()]);
            c.continuation = "opaque-logical-cover-debt-and-acquisition".into();
            apply_pir(&mut w, &c);
            let continuation = w.transparent_pir_checkpoint();
            let source = w.transparent_pir.bound_source.clone();
            w.rescan_from(birthday).unwrap();
            assert!(w.transparent_pir_checkpoint() == continuation);
            assert_eq!(w.transparent_pir.bound_source, source);
            assert_eq!(w.next_diversifier, cursor);
            assert_eq!(w.unified_address, address);
            assert_eq!(w.public_transparent_scripts().unwrap(), scripts);
            assert_eq!(w.transparent_pir_status()["rescanRequired"], false);
            let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            cold.scanned_height = 3;
            cold.block_hashes.insert(3, "03".repeat(32));
            apply_pir(&mut cold, &c);
            assert_eq!(cold.balance().transparent_available, 0);
            assert_eq!(cold.balance().transparent_pending, 150000);
            assert!(cold.transparent_pir_checkpoint() == continuation);
        }
    }

    #[test]
    fn explicit_rescan_refuses_unknown_payment_and_active_reservation_atomically() {
        let mut w = wallet();
        let mut c = checkpoint(&mut w, vec![receive()]);
        c.continuation = "opaque-acquisition-and-cover-debt".into();
        apply_pir(&mut w, &c);
        let continuation = w.transparent_pir_checkpoint();
        let pending = zcash_primitives::transaction::TxId::from_bytes([9; 32]);
        w.pending_txs.insert(pending, vec![0xaa]);
        w.txs.entry("09".repeat(32)).or_default().expiry_height = Some(100);
        let before = w.to_snapshot().unwrap();
        assert!(w
            .rescan_from(1)
            .unwrap_err()
            .to_string()
            .contains("rescan pending payment"));
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert_eq!(w.pending_txs.get(&pending), Some(&vec![0xaa]));
        w.pending_txs.clear();
        w.txs.remove(&"09".repeat(32));
        w.output_locks.push(StoredLock {
            txid: "01".repeat(32),
            pool: "transparent".into(),
            index: 0,
            owner: "ff".repeat(32),
            expiry: 100,
        });
        let before = w.to_snapshot().unwrap();
        assert!(w
            .rescan_from(1)
            .unwrap_err()
            .to_string()
            .contains("rescan active output reservation"));
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert_eq!(w.output_locks.len(), 1);
        assert_eq!(w.output_locks[0].owner, "ff".repeat(32));
        assert!(w.transparent_pir_checkpoint() == continuation);
    }

    #[test]
    fn mined_wallet_write_on_spent_pir_row_cannot_authorize_retired_funding() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let mut w = wallet();
        w.block_hashes.insert(1, "01".repeat(32));
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        apply_pir(&mut w, &c);
        let script = transparent::address::Script(zcash_script::script::Code(
            super::super::super::from_hex(&w.utxos[0].script).unwrap(),
        ));
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new([1; 32], 0),
            transparent::bundle::TxOut::new(
                zcash_protocol::value::Zatoshis::from_u64(40000).unwrap(),
                script,
            ),
            Some(3.into()),
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        assert_eq!(w.utxos[0].value_zat, 150000);
        assert_eq!(w.utxos[0].height, 1);
        replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
        assert_eq!(w.balance().transparent_available, 0);
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        let blocks = public_range(&mut restored, false);
        restored
            .apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        assert_eq!(restored.balance().transparent_available, 150000);
    }

    #[test]
    fn partial_public_receive_then_unmined_refresh_cannot_release_pir_funding() {
        for height in [Some(0), None] {
            let mut w = wallet();
            let c = checkpoint(&mut w, vec![receive()]);
            apply_pir(&mut w, &c);
            let blocks = public_range(&mut w, false);
            w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
                blocks[0].clone()
            ]))
            .unwrap();
            assert_eq!(w.balance().transparent_available, 0);
            let u = w.utxos[0].clone();
            let mut row = serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,"valueZat":u.value_zat,"address":u.address});
            if let Some(h) = height {
                row["height"] = serde_json::json!(h);
            }
            w.apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
                .unwrap();
            assert_eq!(w.balance().transparent_available, 0);
            replace_pir(&mut w, c.checkpoint_id, vec![], 0);
            assert_eq!(w.utxos.len(), 1);
            assert_eq!(w.balance().transparent_available, 0);
            let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            assert_eq!(restored.balance().transparent_available, 0);
            let blocks = public_range(&mut restored, false);
            restored
                .apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
                .unwrap();
            assert_eq!(restored.balance().transparent_available, 150000);
        }
    }

    #[test]
    fn partial_funding_block_survives_conflict_retirement_reset_and_cold_replay() {
        let mut w = wallet();
        let mut c = checkpoint(&mut w, vec![receive()]);
        c.continuation = "opaque-cover-debt-and-acquisition".into();
        apply_pir(&mut w, &c);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
            blocks[0].clone()
        ]))
        .unwrap();
        let mut conflict_receive = receive();
        if let TransparentEvent::Receive(ref mut receive) = conflict_receive {
            receive.value = 200000;
        }
        let mut conflict = checkpoint(&mut w, vec![conflict_receive]);
        conflict.checkpoint_id = "dc".repeat(32);
        conflict.previous_checkpoint_id = Some(c.checkpoint_id.clone());
        let before = w.to_snapshot().unwrap();
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&conflict).unwrap())
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert_eq!(w.balance().transparent_available, 0);
        let continuation = w.transparent_pir_checkpoint();
        w.rescan_from(1).unwrap();
        assert!(w.transparent_pir_checkpoint() == continuation);
        w.scanned_height = 3;
        w.block_hashes.insert(3, "03".repeat(32));
        apply_pir(&mut w, &c);
        assert_eq!(w.balance().transparent_available, 0);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
            blocks[0].clone()
        ]))
        .unwrap();
        replace_pir(&mut w, c.checkpoint_id, vec![], 0);
        assert_eq!(w.utxos.len(), 1);
        assert_eq!(w.balance().transparent_available, 0);
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        let u = restored.utxos[0].clone();
        let row = serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,"valueZat":40000,"height":1});
        let before = restored.to_snapshot().unwrap();
        assert!(restored
            .apply_utxos_json(&serde_json::json!({"utxos":[row.clone(),{}]}).to_string())
            .is_err());
        assert_eq!(restored.to_snapshot().unwrap(), before);
        assert!(restored
            .apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .is_err());
        assert!(restored.to_snapshot().unwrap() == before);
        let row = serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,"valueZat":150000,"height":1});
        restored
            .apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(restored.balance().transparent_available, 150000);
        assert_eq!(restored.utxos[0].value_zat, 150000);
    }

    #[test]
    fn unknown_zero_conf_funding_without_pir_waits_for_classification() {
        let mut w = wallet();
        let script = w.public_transparent_scripts().unwrap()[0].0.clone();
        w.apply_utxos_json(
            &serde_json::json!({"utxos":[{
                "txid":"01".repeat(32),"index":0,"script":super::super::super::to_hex(&script),
                "valueZat":150000,"height":0
            }]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(w.balance().transparent_available, 0);
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_available, 0);
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], false);
        let c = checkpoint(&mut cold, vec![]);
        apply_pir(&mut cold, &c);
        assert_eq!(cold.balance().transparent_available, 0);
    }

    #[test]
    fn unmined_wallet_write_cannot_replace_pending_public_funding_fields() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        let blocks = public_range(&mut w, false);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(vec![
            blocks[0].clone()
        ]))
        .unwrap();
        let script = transparent::address::Script(zcash_script::script::Code(
            super::super::super::from_hex(&w.utxos[0].script).unwrap(),
        ));
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new([1; 32], 0),
            transparent::bundle::TxOut::new(
                zcash_protocol::value::Zatoshis::from_u64(900000).unwrap(),
                script,
            ),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        assert_eq!(w.balance().transparent_available, 0);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(
            blocks[1..].iter().cloned(),
        ))
        .unwrap();
        assert_eq!(w.balance().transparent_available, 150000);
        assert_eq!(w.utxos[0].value_zat, 150000);
    }

    #[test]
    fn public_spend_authority_survives_retirement_of_conflicting_pir_input_index() {
        let mut w = wallet();
        w.block_hashes.insert(1, "01".repeat(32));
        let mut claimed_spend = spend();
        if let TransparentEvent::Spend(ref mut spend) = claimed_spend {
            spend.input_index = 1;
        }
        let c = checkpoint(&mut w, vec![receive(), claimed_spend]);
        apply_pir(&mut w, &c);
        let blocks = public_range(&mut w, true);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
        assert_eq!(w.balance().transparent_available, 0);
        assert!(w.utxos[0].spent);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.balance().transparent_available, 0);
        assert!(restored.utxos[0].spent);
    }

    #[test]
    fn canonical_nonzero_receive_requires_durable_ack_and_survives_reopen() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap();
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "150000");
        assert_eq!(
            w.transparent_pir_status()["completion"],
            "checkpoint-pending"
        );
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 150000);
        assert!(w
            .acknowledge_transparent_pir_checkpoint(&"00".repeat(32))
            .is_err());
        let saved = w.to_snapshot().unwrap();
        let restored = WebWallet::from_snapshot(&saved).unwrap();
        assert_eq!(restored.transparent_pir_status()["confirmedZat"], "150000");
        assert_eq!(
            restored.transparent_pir_status()["completion"],
            "complete-for-enrolled-scope"
        );
        assert_eq!(restored.history(20).len(), 1);
    }
    #[test]
    fn reset_scan_replays_unchanged_checkpoint_without_discarding_acquisition_state() {
        for spent in [false, true] {
            let mut w = wallet();
            let events = if spent {
                vec![receive(), spend()]
            } else {
                vec![receive()]
            };
            let mut c = checkpoint(&mut w, events);
            c.continuation = "opaque-acquisition-with-unpaid-cover-queries".into();
            w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
                .unwrap();
            w.acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
                .unwrap();
            let saved = w.transparent_pir_checkpoint().unwrap();
            let expected = if spent { "0" } else { "150000" };
            assert_eq!(w.transparent_pir_status()["confirmedZat"], expected);
            w.reset_scan();
            assert!(w.utxos.is_empty());
            assert!(w.history(20).is_empty());
            assert!(w.transparent_pir_checkpoint().unwrap() == saved);
            // The shielded scanner has returned to the exact same accepted tip.
            w.scanned_height = 3;
            w.block_hashes.insert(3, "03".repeat(32));
            let mut reopened = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            reopened
                .apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
                .unwrap();
            reopened
                .acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
                .unwrap();
            assert_eq!(reopened.transparent_pir_status()["confirmedZat"], expected);
            assert_eq!(reopened.history(20).len(), if spent { 2 } else { 1 });
            assert!(reopened.transparent_pir_checkpoint().unwrap() == saved);
        }
    }

    #[test]
    fn pir_spend_generic_history_repairs_cold_legacy_and_same_checkpoint_replay() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        apply_pir(&mut w, &c);
        let assert_spend = |w: &WebWallet| {
            let rows = w.history(20);
            let spent = rows.iter().find(|r| r.transparent_spent == 150000).unwrap();
            assert_eq!(spent.spent_zat, 150000);
            assert_eq!(spent.sent_note_count, 1);
            assert_eq!(spent.account_delta_zat, -150000);
        };
        assert_spend(&w);
        apply_pir(&mut w, &c);
        assert_spend(&w);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        for agg in legacy["txs"].as_object_mut().unwrap().values_mut() {
            agg.as_object_mut().unwrap().remove("pirSpentZat");
            agg.as_object_mut().unwrap().remove("pirSpentNotes");
        }
        legacy["txs"]["02".repeat(32)]["spentZat"] = serde_json::json!(0);
        legacy["txs"]["02".repeat(32)]["spentNotes"] = serde_json::json!(0);
        let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_spend(&restored);
        apply_pir(&mut restored, &c);
        assert_spend(&restored);
        let mut progress = c.clone();
        progress.previous_checkpoint_id = Some(c.checkpoint_id);
        progress.checkpoint_id = "ce".repeat(32);
        apply_pir(&mut restored, &progress);
        assert_spend(&restored);
        let mut reopened = WebWallet::from_snapshot(&restored.to_snapshot().unwrap()).unwrap();
        assert_spend(&reopened);
        reopened.block_hashes.insert(1, "01".repeat(32));
        replace_pir(&mut reopened, progress.checkpoint_id, vec![receive()], 1);
        assert!(!reopened.txs.contains_key(&"02".repeat(32)));
        assert_eq!(reopened.history(20).len(), 1);
    }

    #[test]
    fn pir_spend_generic_history_preserves_independent_totals_on_retirement() {
        let mut w = wallet();
        // Existing ordinary/full-transaction accounting, independently of the PIR input.
        w.txs.insert(
            "02".repeat(32),
            crate::web::store::TxAgg {
                mined_height: Some(2),
                spent_zat: 700,
                spent_notes: 2,
                ..Default::default()
            },
        );
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        apply_pir(&mut w, &c);
        assert_eq!(w.txs[&"02".repeat(32)].spent_zat, 700);
        assert_eq!(
            w.history(20)
                .iter()
                .find(|r| r.transparent_spent == 150000)
                .unwrap()
                .spent_zat,
            150700
        );
        assert_eq!(w.txs[&"02".repeat(32)].spent_notes, 2);
        assert_eq!(
            w.history(20)
                .iter()
                .find(|r| r.transparent_spent == 150000)
                .unwrap()
                .sent_note_count,
            3
        );
        apply_pir(&mut w, &c);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        let agg = legacy["txs"]["02".repeat(32)].as_object_mut().unwrap();
        agg.remove("pirSpentZat");
        agg.remove("pirSpentNotes");
        agg.insert("spentZat".into(), serde_json::json!(700));
        agg.insert("spentNotes".into(), serde_json::json!(2));
        let mut restored = WebWallet::from_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(restored.txs[&"02".repeat(32)].spent_zat, 700);
        assert_eq!(
            restored
                .history(20)
                .iter()
                .find(|r| r.transparent_spent == 150000)
                .unwrap()
                .spent_zat,
            150700
        );
        assert_eq!(restored.txs[&"02".repeat(32)].spent_notes, 2);
        assert_eq!(
            restored
                .history(20)
                .iter()
                .find(|r| r.transparent_spent == 150000)
                .unwrap()
                .sent_note_count,
            3
        );
        restored.block_hashes.insert(1, "01".repeat(32));
        replace_pir(&mut restored, c.checkpoint_id, vec![receive()], 1);
        assert_eq!(restored.txs[&"02".repeat(32)].spent_zat, 700);
        assert_eq!(restored.txs[&"02".repeat(32)].spent_notes, 2);
        assert_eq!(restored.txs[&"02".repeat(32)].transparent_spent, 0);
        let reopened = WebWallet::from_snapshot(&restored.to_snapshot().unwrap()).unwrap();
        assert_eq!(reopened.txs[&"02".repeat(32)].spent_zat, 700);
        assert_eq!(reopened.txs[&"02".repeat(32)].spent_notes, 2);
    }

    #[test]
    fn pir_spend_generic_history_transfers_to_public_write_without_doubling() {
        use zcash_client_backend::data_api::{SentTransaction, WalletWrite};
        use zcash_client_backend::proto::compact_formats::{CompactTx, CompactTxIn, TxOut};
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::{
            consensus::{BlockHeight, BranchId},
            value::Zatoshis,
        };
        for rewound in [false, true] {
            let mut w = wallet();
            let script = w.public_transparent_scripts().unwrap()[0].0.clone();
            let address = w.public_transparent_scripts().unwrap()[0].1.clone();
            w.apply_utxos_json(
                &serde_json::json!({"utxos":[{
                    "txid":"03".repeat(32),"index":0,"script":super::super::super::to_hex(&script),
                    "valueZat":700,"height":1,"address":address
                }]})
                .to_string(),
            )
            .unwrap();
            // Structurally valid disposable bytes, with inert script signatures.
            // This exercises WalletWrite accounting, not consensus or signing.
            let mut raw = 1u32.to_le_bytes().to_vec();
            raw.push(2);
            for previous in [1, 3] {
                raw.extend([previous; 32]);
                raw.extend(0u32.to_le_bytes());
                raw.push(0);
                raw.extend(u32::MAX.to_le_bytes());
            }
            raw.push(1);
            raw.extend(140700u64.to_le_bytes());
            raw.push(u8::try_from(script.len()).unwrap());
            raw.extend(&script);
            raw.extend(0u32.to_le_bytes());
            let tx = Transaction::read(&raw[..], BranchId::for_height(&Network::Regtest, 2.into()))
                .unwrap();
            let spender = super::super::super::to_hex(tx.txid().as_ref());
            let mut event = spend();
            if let TransparentEvent::Spend(e) = &mut event {
                e.spending_txid = Txid(*tx.txid().as_ref());
            }
            let c = checkpoint(&mut w, vec![receive(), event]);
            apply_pir(&mut w, &c);
            assert_eq!(
                (w.txs[&spender].spent_zat, w.txs[&spender].spent_notes),
                (0, 0)
            );
            assert_eq!(
                w.history(20)
                    .iter()
                    .find(|r| r.transparent_spent == 150000)
                    .unwrap()
                    .spent_zat,
                150000
            );
            if rewound {
                w.block_hashes.insert(1, "01".repeat(32));
                w.rewind_to_height_checked(1).unwrap();
                assert!(w.utxos.iter().all(|u| !u.spent));
                // The synthetic shielded scanner catches back up to the same accepted tip.
                w.scanned_height = 3;
                w.block_hashes.insert(3, "03".repeat(32));
            }
            let sent = SentTransaction::new(
                &tx,
                std::time::UNIX_EPOCH.into(),
                BlockHeight::from_u32(2).into(),
                0,
                &[],
                Zatoshis::from_u64(10000).unwrap(),
                &[],
            );
            w.store_transactions_to_be_sent(&[sent]).unwrap();
            assert_eq!(
                (w.txs[&spender].spent_zat, w.txs[&spender].spent_notes),
                (150700, 2)
            );
            let pending = w.pending_txs.clone();
            let mut blocked = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            blocked.block_hashes.insert(1, "01".repeat(32));
            let mut retirement = checkpoint(&mut blocked, vec![]);
            retirement.checkpoint_id = "de".repeat(32);
            retirement.previous_checkpoint_id = Some(c.checkpoint_id.clone());
            retirement.rewind_to = Some(TransparentPirAnchor {
                height: 0,
                hash: Network::Regtest.genesis_hash().into(),
            });
            let before = blocked.to_snapshot().unwrap();
            assert!(blocked
                .apply_transparent_pir_checkpoint(&serde_json::to_string(&retirement).unwrap())
                .unwrap_err()
                .to_string()
                .contains("transparent_pir_pending_reconciliation_required"));
            assert_eq!(blocked.to_snapshot().unwrap(), before);
            assert_eq!(blocked.pending_txs, pending);
            let mut blocks = public_range(&mut w, false);
            blocks[0].vtx.push(CompactTx {
                index: 2,
                txid: vec![3; 32],
                vout: vec![TxOut {
                    value: 700,
                    script_pub_key: script.clone(),
                }],
                ..Default::default()
            });
            blocks[1].vtx.push(CompactTx {
                index: 1,
                txid: tx.txid().as_ref().to_vec(),
                vin: [1, 3]
                    .into_iter()
                    .map(|b| CompactTxIn {
                        prevout_txid: vec![b; 32],
                        prevout_index: 0,
                    })
                    .collect(),
                vout: vec![TxOut {
                    value: 140700,
                    script_pub_key: script,
                }],
                ..Default::default()
            });
            w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
                .unwrap();
            assert_eq!(
                (w.txs[&spender].spent_zat, w.txs[&spender].spent_notes),
                (150700, 2)
            );
            assert_eq!(
                (
                    w.txs[&spender].pir_spent_zat,
                    w.txs[&spender].pir_spent_notes
                ),
                (0, 0)
            );
            w.block_hashes.insert(1, "01".repeat(32));
            replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
            assert_eq!(
                (w.txs[&spender].spent_zat, w.txs[&spender].spent_notes),
                (150700, 2)
            );
            assert_eq!(w.pending_txs, pending);
            let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            assert_eq!(
                (cold.txs[&spender].spent_zat, cold.txs[&spender].spent_notes),
                (150700, 2)
            );
            assert_eq!(cold.pending_txs, pending);
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn retirement_keeps_independent_enhancement_of_pir_created_transaction() {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::BranchId;
        let mut w = wallet();
        w.scanned_height = 1_000_001;
        w.block_hashes.insert(1_000_001, "03".repeat(32));
        let recipient = WebWallet::from_account(
            account_from_mnemonic(
                "legal winner thank year wave sausage worth useful legal winner thank yellow",
                Network::Regtest,
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let raw = crate::web::shielded_memo_transaction(
            &w,
            &recipient,
            "independent outgoing",
            50000,
            true,
        );
        let tx =
            Transaction::read(&raw[..], BranchId::for_height(&Network::Regtest, 4.into())).unwrap();
        let id = super::super::super::to_hex(tx.txid().as_ref());
        let mut event = spend();
        if let TransparentEvent::Spend(e) = &mut event {
            e.spending_txid = Txid(*tx.txid().as_ref());
        }
        let c = checkpoint(&mut w, vec![receive(), event]);
        apply_pir(&mut w, &c);
        assert!(w.transparent_pir.projected_transactions.contains(&id));
        w.enhance_raw_tx(&raw).unwrap();
        assert_eq!(w.txs[&id].outgoing_shielded_zat, 50000);
        assert_eq!(w.txs[&id].memos, ["independent outgoing"]);
        w.block_hashes.insert(1, "01".repeat(32));
        replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
        let rows = w.history(20);
        let row = rows
            .iter()
            .find(|row| row.txid == WebWallet::display_txid(&id))
            .expect("independently recovered outgoing history survives PIR retirement");
        assert_eq!(row.outgoing_shielded_zat, 50000);
        assert_eq!(row.memos, ["independent outgoing"]);
        assert_eq!(
            row.mined_height, None,
            "raw enhancement does not prove inclusion"
        );
        assert_eq!((row.spent_zat, row.sent_note_count), (0, 0));
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.txs[&id].outgoing_shielded_zat, 50000);
    }

    #[test]
    fn retirement_removes_pure_pir_receive_history_and_memo_work() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        assert_eq!(w.history(20)[0].received_zat, 150000);
        assert_eq!(w.history(20)[0].received_note_count, 1);
        replace_pir(&mut w, c.checkpoint_id, vec![], 0);
        assert!(
            w.history(20).is_empty(),
            "retired publisher-only receive has no canonical history"
        );
        assert!(w.memo_enhancement_txids(20).is_empty());
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(cold.history(20).is_empty());
        assert!(cold.memo_enhancement_txids(20).is_empty());
    }

    #[test]
    fn retirement_keeps_independent_compact_spend_before_pool_rebuild() {
        use prost::Message;
        use zcash_client_backend::proto::compact_formats::{
            ChainMetadata, CompactBlock, CompactSaplingSpend, CompactTx,
        };
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        apply_pir(&mut w, &c);
        let spender = "02".repeat(32);
        assert!(w.transparent_pir.projected_transactions.contains(&spender));
        // A retained owned note is independently consumed by the actual compact scanner.
        // This is scan/accounting coverage, not a transaction consensus fixture.
        w.notes.push(crate::web::store::TrackedNote {
            pool: "sapling".into(),
            txid: "09".repeat(32),
            output_index: 0,
            value_zat: 700,
            nf: "11".repeat(32),
            is_change: false,
            spent: false,
            spent_in: None,
            spent_height: None,
            mined_height: 1,
        });
        let b = CompactBlock {
            height: 4,
            hash: vec![4; 32],
            prev_hash: vec![3; 32],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            }),
            vtx: vec![CompactTx {
                txid: vec![2; 32],
                spends: vec![CompactSaplingSpend { nf: vec![0x11; 32] }],
                ..Default::default()
            }],
            ..Default::default()
        };
        w.apply_compact_block(&b.encode_to_vec()).unwrap();
        assert_eq!(
            (w.txs[&spender].spent_zat, w.txs[&spender].spent_notes),
            (700, 1)
        );
        assert_eq!(
            w.txs[&spender].sapling_spent, 0,
            "pool rebuild has not happened yet"
        );
        w.block_hashes.insert(1, "01".repeat(32));
        replace_pir(&mut w, c.checkpoint_id, vec![receive()], 1);
        let row = w
            .history(20)
            .into_iter()
            .find(|r| r.txid == WebWallet::display_txid(&spender))
            .unwrap();
        assert_eq!(
            (row.spent_zat, row.sent_note_count, row.sapling_spent),
            (700, 1, 700)
        );
        assert_eq!(row.mined_height, Some(4));
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.txs[&spender].spent_zat, 700);
    }

    #[test]
    fn old_pir_receipt_accounting_requires_explicit_rescan_without_losing_acquisition() {
        let mut w = wallet();
        let mut c = checkpoint(&mut w, vec![receive(), spend()]);
        c.continuation = "opaque-durable-acquisition-with-cover-debt".into();
        apply_pir(&mut w, &c);
        let saved = w.transparent_pir_checkpoint();
        let scripts = w.public_transparent_scripts().unwrap();
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        let agg = old["txs"]["01".repeat(32)].as_object_mut().unwrap();
        agg.remove("pirReceivedZat");
        agg.remove("pirReceivedNotes");
        agg.insert("receivedZat".into(), serde_json::json!(150000));
        agg.insert("receivedNotes".into(), serde_json::json!(1));
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], true);
        assert_eq!(cold.transparent_pir_checkpoint(), saved);
        let before = cold.to_snapshot().unwrap();
        assert!(cold
            .begin_transparent_pir(&"ab".repeat(32))
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_rescan_required"));
        assert!(cold
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_rescan_required"));
        assert_eq!(cold.to_snapshot().unwrap(), before);
        cold.rescan_from(1).unwrap();
        assert_eq!(cold.transparent_pir_checkpoint(), saved);
        assert_eq!(cold.public_transparent_scripts().unwrap(), scripts);
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], false);
        cold.scanned_height = 3;
        cold.block_hashes.insert(3, "03".repeat(32));
        apply_pir(&mut cold, &c);
        apply_pir(&mut cold, &c);
        let rows = cold.history(20);
        assert_eq!(rows.len(), 2);
        let receive = rows
            .iter()
            .find(|r| r.transparent_received == 150000)
            .unwrap();
        assert_eq!(
            (receive.received_zat, receive.received_note_count),
            (150000, 1)
        );
        assert_eq!(cold.transparent_pir_checkpoint(), saved);
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], false);
    }

    #[test]
    fn constructed_transparent_receipt_transfers_only_its_accounting_once() {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::Recipient;
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::{
            consensus::{BlockHeight, BranchId},
            value::Zatoshis,
        };
        let mut w = wallet();
        let script = w.public_transparent_scripts().unwrap()[0].0.clone();
        let address = w.public_transparent_scripts().unwrap()[0].1.clone();
        w.apply_utxos_json(&serde_json::json!({"utxos":[{"txid":"09".repeat(32),"index":0,"script":super::super::super::to_hex(&script),"valueZat":27000,"height":1,"address":address}]}).to_string()).unwrap();
        // Parsed legacy-format disposable transaction; inert script signatures.
        let mut raw = 1u32.to_le_bytes().to_vec();
        raw.push(1);
        raw.extend([9; 32]);
        raw.extend(0u32.to_le_bytes());
        raw.push(0);
        raw.extend(u32::MAX.to_le_bytes());
        raw.push(1);
        raw.extend(17000u64.to_le_bytes());
        raw.push(script.len() as u8);
        raw.extend(&script);
        raw.extend(0u32.to_le_bytes());
        let tx =
            Transaction::read(&raw[..], BranchId::for_height(&Network::Regtest, 3.into())).unwrap();
        let id = super::super::super::to_hex(tx.txid().as_ref());
        let mut event = receive();
        if let TransparentEvent::Receive(e) = &mut event {
            e.txid = Txid(*tx.txid().as_ref());
            e.height = 3;
            e.value = 17000;
        }
        let c = checkpoint(&mut w, vec![event]);
        apply_pir(&mut w, &c);
        let output = SentTransactionOutput::from_parts(
            0,
            Recipient::InternalTransparent {
                receiving_account: 0,
                recipient_address: tx.transparent_bundle().unwrap().vout[0]
                    .recipient_address()
                    .unwrap(),
            },
            Zatoshis::from_u64(17000).unwrap(),
            None,
        );
        let outputs = [output];
        let sent = SentTransaction::new(
            &tx,
            std::time::UNIX_EPOCH.into(),
            BlockHeight::from_u32(3).into(),
            0,
            &outputs,
            Zatoshis::from_u64(10000).unwrap(),
            &[],
        );
        w.store_transactions_to_be_sent(&[sent]).unwrap();
        w.recompute_pool_fields();
        let row = w
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!((row.received_zat, row.received_note_count), (17000, 1));
        assert!(
            w.pir_funding_unverified(&id, 0),
            "construction is not funding confirmation"
        );
        let listed = w.utxos.iter().find(|u| u.txid == id).unwrap();
        let reply=serde_json::json!({"utxos":[{"txid":id,"index":0,"script":listed.script,"height":3,"valueZat":17000,"address":listed.address}]}).to_string();
        w.apply_utxos_json(&reply).unwrap();
        w.apply_utxos_json(&reply).unwrap();
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!((row.received_zat, row.received_note_count), (17000, 1));
    }

    fn builder_accounting_fixture() -> (WebWallet, zcash_primitives::transaction::Transaction) {
        let mut w = wallet();
        let (script, address) = w.public_transparent_scripts().unwrap()[0].clone();
        w.apply_utxos_json(&serde_json::json!({"utxos":[{"txid":"09".repeat(32),"index":0,"script":super::super::super::to_hex(&script),"valueZat":27000,"height":1,"address":address}]}).to_string()).unwrap();
        // Actual parsed transaction/callback metadata; these inert script signatures
        // are not a consensus-valid transaction or an enabled Web TEX proposal.
        let mut raw = 1u32.to_le_bytes().to_vec();
        raw.push(1);
        raw.extend([9; 32]);
        raw.extend(0u32.to_le_bytes());
        raw.push(0);
        raw.extend(u32::MAX.to_le_bytes());
        raw.push(1);
        raw.extend(17000u64.to_le_bytes());
        raw.push(script.len() as u8);
        raw.extend(script);
        raw.extend(0u32.to_le_bytes());
        let tx = zcash_primitives::transaction::Transaction::read(
            &raw[..],
            zcash_protocol::consensus::BranchId::for_height(&Network::Regtest, 3.into()),
        )
        .unwrap();
        (w, tx)
    }

    fn builder_accounting_store_result(
        w: &mut WebWallet,
        tx: &zcash_primitives::transaction::Transaction,
        ephemeral: bool,
    ) -> std::result::Result<(), String> {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::Recipient;
        use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};
        let address = tx.transparent_bundle().unwrap().vout[0]
            .recipient_address()
            .unwrap();
        let recipient = if ephemeral {
            Recipient::EphemeralTransparent {
                receiving_account: 0,
                ephemeral_address: address,
                outpoint: transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            }
        } else {
            Recipient::InternalTransparent {
                receiving_account: 0,
                recipient_address: address,
            }
        };
        let outputs = [SentTransactionOutput::from_parts(
            0,
            recipient,
            tx.transparent_bundle().unwrap().vout[0].value(),
            None,
        )];
        let sent = SentTransaction::new(
            tx,
            std::time::UNIX_EPOCH.into(),
            BlockHeight::from_u32(3).into(),
            0,
            &outputs,
            Zatoshis::from_u64(10000).unwrap(),
            &[],
        );
        w.store_transactions_to_be_sent(&[sent])
    }

    fn builder_accounting_store(
        w: &mut WebWallet,
        tx: &zcash_primitives::transaction::Transaction,
        ephemeral: bool,
    ) {
        builder_accounting_store_result(w, tx, ephemeral).unwrap();
    }

    #[test]
    fn builder_accounting_pending_transparent_change_survives_missing_utxo_and_cold_refresh() {
        for ephemeral in [false, true] {
            let (mut w, tx) = builder_accounting_fixture();
            builder_accounting_store(&mut w, &tx, ephemeral);
            assert!(w
                .utxos
                .iter()
                .all(|u| u.txid != super::super::super::to_hex(tx.txid().as_ref())));
            let assert_pending = |w: &WebWallet| {
                assert_eq!(w.balance().transparent_pending, 17000);
                assert_eq!(w.balance().orchard_pending, 0);
                assert_eq!(w.balance().total_pending, 17000);
                let row = w
                    .history_matching(1, None, Some(&tx.txid().to_string()))
                    .remove(0);
                assert_eq!(
                    (
                        row.received_zat,
                        row.received_note_count,
                        row.transparent_received
                    ),
                    (17000, 1, 17000)
                );
                assert_eq!(w.pending_raw_txs().len(), 1);
            };
            assert_pending(&w);
            let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            assert_pending(&cold);
            cold.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
            assert_pending(&cold);
            let output = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
                tx.transparent_bundle().unwrap().vout[0].clone(),
                None,
                None,
                None,
                None,
            )
            .unwrap();
            zcash_client_backend::data_api::WalletWrite::put_received_transparent_utxo(
                &mut cold, &output,
            )
            .unwrap();
            assert_pending(&cold);
            cold.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
            assert_pending(&cold);
        }
    }

    #[test]
    fn builder_accounting_public_credit_then_callback_is_idempotent() {
        for ephemeral in [false, true] {
            let (mut w, tx) = builder_accounting_fixture();
            let mut event = receive();
            if let TransparentEvent::Receive(e) = &mut event {
                e.txid = Txid(*tx.txid().as_ref());
                e.height = 3;
                e.value = 17000;
            }
            let c = checkpoint(&mut w, vec![event]);
            apply_pir(&mut w, &c);
            let id = super::super::super::to_hex(tx.txid().as_ref());
            let u = w.utxos.iter().find(|u| u.txid == id).unwrap();
            let reply = serde_json::json!({"utxos":[{"txid":u.txid,"index":u.index,"script":u.script,"height":3,"valueZat":17000,"address":u.address}]}).to_string();
            w.apply_utxos_json(&reply).unwrap();
            builder_accounting_store(&mut w, &tx, ephemeral);
            builder_accounting_store(&mut w, &tx, ephemeral);
            let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            let row = cold
                .history_matching(1, None, Some(&tx.txid().to_string()))
                .remove(0);
            assert_eq!(
                (
                    row.received_zat,
                    row.received_note_count,
                    row.transparent_received
                ),
                (17000, 1, 17000)
            );
            assert!(!cold.pir_funding_unverified(&id, 0));
        }
    }

    #[test]
    fn builder_accounting_public_wallet_write_reconciles_history_immediately() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new([1; 32], 0),
            transparent::bundle::TxOut::new(
                zcash_protocol::value::Zatoshis::from_u64(150000).unwrap(),
                transparent::address::Script(zcash_script::script::Code(
                    super::super::super::from_hex(&w.utxos[0].script).unwrap(),
                )),
            ),
            Some(1.into()),
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        // Observe the public history immediately, without a repair/recompute call.
        let row = w.history(20).remove(0);
        assert_eq!((row.received_zat, row.received_note_count), (150000, 1));
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(
            (
                cold.history(20)[0].received_zat,
                cold.history(20)[0].received_note_count
            ),
            (150000, 1)
        );
    }

    #[test]
    fn builder_accounting_ordinary_public_credit_then_callback_is_idempotent() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        for ephemeral in [false, true] {
            let (mut w, tx) = builder_accounting_fixture();
            let output = WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
                tx.transparent_bundle().unwrap().vout[0].clone(),
                Some(3.into()),
                None,
                None,
                None,
            )
            .unwrap();
            w.put_received_transparent_utxo(&output).unwrap();
            builder_accounting_store(&mut w, &tx, ephemeral);
            builder_accounting_store(&mut w, &tx, ephemeral);
            let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            let row = cold
                .history_matching(1, None, Some(&tx.txid().to_string()))
                .remove(0);
            assert_eq!((row.received_zat, row.received_note_count), (17000, 1));
        }
    }

    #[test]
    fn builder_accounting_represented_spent_change_is_not_resurrected_as_pending() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        use zcash_primitives::transaction::{Authorized, TransactionData};
        use zcash_protocol::{consensus::BranchId, value::Zatoshis};
        let (mut w, tx) = builder_accounting_fixture();
        builder_accounting_store(&mut w, &tx, false);
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            tx.transparent_bundle().unwrap().vout[0].clone(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        let child = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            20.into(),
            Some(transparent::bundle::Bundle {
                vin: vec![transparent::bundle::TxIn::from_parts(
                    transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
                    transparent::address::Script::default(),
                    u32::MAX,
                )],
                vout: vec![transparent::bundle::TxOut::new(
                    Zatoshis::from_u64(7000).unwrap(),
                    tx.transparent_bundle().unwrap().vout[0]
                        .script_pubkey()
                        .clone(),
                )],
                authorization: transparent::bundle::Authorized,
            }),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        builder_accounting_store(&mut w, &child, false);
        assert_eq!(w.balance().transparent_pending, 7000);
        assert!(w
            .utxos
            .iter()
            .any(|u| u.txid == super::super::super::to_hex(tx.txid().as_ref()) && u.spent));
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_pending, 7000);
        assert_eq!(cold.pending_raw_txs().len(), 2);
        use zcash_client_backend::proto::compact_formats::{CompactTx, CompactTxIn, TxOut};
        let mut blocks = public_range(&mut cold, false);
        blocks[0].vtx.push(CompactTx {
            index: 2,
            txid: tx.txid().as_ref().to_vec(),
            vout: vec![TxOut {
                value: 17000,
                script_pub_key: tx.transparent_bundle().unwrap().vout[0]
                    .script_pubkey()
                    .0
                     .0
                    .clone(),
            }],
            ..Default::default()
        });
        blocks[1].vtx.push(CompactTx {
            index: 2,
            txid: child.txid().as_ref().to_vec(),
            vin: vec![CompactTxIn {
                prevout_txid: tx.txid().as_ref().to_vec(),
                prevout_index: 0,
            }],
            vout: vec![TxOut {
                value: 7000,
                script_pub_key: child.transparent_bundle().unwrap().vout[0]
                    .script_pubkey()
                    .0
                     .0
                    .clone(),
            }],
            ..Default::default()
        });
        cold.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        builder_accounting_store(&mut cold, &child, false);
        let parent_id = super::super::super::to_hex(tx.txid().as_ref());
        assert_eq!(
            cold.txs()
                .get(&parent_id)
                .unwrap()
                .accounted_transparent_receipts[&0]
                .spent_in
                .as_ref()
                .unwrap()
                .height,
            Some(2)
        );
        cold.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        let cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        // The independently observed child receipt was omitted, and the spent
        // parent stays represented. Neither historical receipt becomes pending.
        assert_eq!(
            cold.balance().transparent_pending + cold.balance().transparent_available,
            0
        );
    }

    #[test]
    fn builder_accounting_expired_transparent_change_is_not_pending() {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        use zcash_protocol::consensus::BranchId;
        let (mut w, template) = builder_accounting_fixture();
        let tx = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            4.into(),
            template.transparent_bundle().cloned(),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        builder_accounting_store(&mut w, &tx, false);
        assert_eq!(w.balance().transparent_pending, 17000);
        let block = zcash_client_backend::proto::compact_formats::CompactBlock {
            height: 4,
            hash: vec![4; 32],
            prev_hash: vec![3; 32],
            ..Default::default()
        };
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([block]))
            .unwrap();
        // Ordinary expiry releases the original27000 input; the17000 change
        // metadata remains history and does not become an additional balance.
        assert_eq!(w.balance().transparent_pending, 27000);
        assert!(w.pending_raw_txs().is_empty());
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_pending, 27000);
    }

    #[cfg(feature = "native")]
    fn builder_mixed_fixture(
        store: bool,
    ) -> (WebWallet, zcash_primitives::transaction::Transaction) {
        builder_mixed_fixture_pool(store, true)
    }

    #[cfg(feature = "native")]
    fn builder_mixed_fixture_pool(
        store: bool,
        ironwood: bool,
    ) -> (WebWallet, zcash_primitives::transaction::Transaction) {
        use zcash_primitives::transaction::{Authorized, Transaction, TransactionData};
        use zcash_protocol::consensus::BranchId;
        let (mut w, transparent) = builder_accounting_fixture();
        let raw = crate::web::shielded_memo_transaction(&w, &w, "mixed receipt", 30000, ironwood);
        let shielded = Transaction::read(&raw[..], BranchId::Nu6_3).unwrap();
        let tx = if ironwood {
            TransactionData::<Authorized>::from_parts_v6(
                BranchId::Nu6_3,
                0,
                20.into(),
                transparent.transparent_bundle().cloned(),
                None,
                None,
                shielded.ironwood_bundle().cloned(),
            )
        } else {
            TransactionData::<Authorized>::from_parts(
                zcash_primitives::transaction::TxVersion::V5,
                BranchId::for_height(&Network::Regtest, 4.into()),
                0,
                20.into(),
                transparent.transparent_bundle().cloned(),
                None,
                None,
                shielded.orchard_bundle().cloned(),
            )
        }
        .freeze()
        .unwrap();
        if store {
            store_mixed_builder_receipt(&mut w, &tx);
        }
        (w, tx)
    }

    #[cfg(feature = "native")]
    fn store_mixed_builder_receipt(
        w: &mut WebWallet,
        tx: &zcash_primitives::transaction::Transaction,
    ) {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::{Note as WalletNote, Recipient};
        use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};
        let keys = std::collections::HashMap::from([(0u32, w.decode_ufvk().unwrap())]);
        let decrypted = zcash_client_backend::decrypt_transaction(
            &Network::Regtest,
            None,
            Some(BlockHeight::from_u32(3)),
            &tx,
            &keys,
        );
        let note = decrypted
            .ironwood_outputs()
            .first()
            .or_else(|| decrypted.orchard_outputs().first())
            .unwrap();
        let outputs = [
            SentTransactionOutput::from_parts(
                0,
                Recipient::InternalTransparent {
                    receiving_account: 0,
                    recipient_address: tx.transparent_bundle().unwrap().vout[0]
                        .recipient_address()
                        .unwrap(),
                },
                Zatoshis::from_u64(17000).unwrap(),
                None,
            ),
            SentTransactionOutput::from_parts(
                note.index(),
                Recipient::InternalShielded {
                    receiving_account: 0,
                    external_address: None,
                    note: Box::new(WalletNote::Orchard {
                        note: note.note().0.clone(),
                        pool: note.note().1,
                    }),
                },
                Zatoshis::from_u64(30000).unwrap(),
                None,
            ),
        ];
        let sent = SentTransaction::new(
            &tx,
            std::time::UNIX_EPOCH.into(),
            BlockHeight::from_u32(3).into(),
            0,
            &outputs,
            Zatoshis::from_u64(10000).unwrap(),
            &[],
        );
        w.store_transactions_to_be_sent(&[sent]).unwrap();
    }

    #[cfg(feature = "native")]
    #[test]
    fn builder_accounting_mixed_pending_receipts_stay_in_their_pools() {
        let (mut w, tx) = builder_mixed_fixture(true);
        for wallet in [
            &w,
            &WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap(),
        ] {
            let balance = wallet.balance();
            assert_eq!(
                (
                    balance.transparent_pending,
                    balance.ironwood_pending,
                    balance.orchard_pending,
                    balance.total_pending
                ),
                (17000, 30000, 0, 47000)
            );
        }
        let mut raw = Vec::new();
        tx.write(&mut raw).unwrap();
        let mut block = w.mined_raw_tx_block(&raw, 1).unwrap();
        block.prev_hash = vec![3; 32];
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([block]))
            .unwrap();
        assert_eq!(
            w.balance().transparent_pending,
            17000,
            "shielded inclusion is not transparent outpoint materialization"
        );
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_pending, 17000);
        store_mixed_builder_receipt(&mut w, &tx);
        let row = w
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.received_zat, row.received_note_count, row.mined_height),
            (47000, 2, Some(4))
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn durable_receipt_raw_mixed_credit_then_callback_is_single() {
        let (mut w, tx) = builder_mixed_fixture(false);
        let mut raw = Vec::new();
        tx.write(&mut raw).unwrap();
        w.apply_raw_tx(&raw).unwrap();
        w.apply_raw_tx(&raw).unwrap();
        let row = w
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.received_zat,
                row.received_note_count,
                row.transparent_received,
                row.ironwood_received
            ),
            (47000, 2, 17000, 30000)
        );
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        assert_eq!(
            w.balance().transparent_pending + w.balance().transparent_available,
            0
        );
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        store_mixed_builder_receipt(&mut cold, &tx);
        store_mixed_builder_receipt(&mut cold, &tx);
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.received_zat,
                row.received_note_count,
                row.transparent_received,
                row.ironwood_received
            ),
            (47000, 2, 17000, 30000)
        );
    }

    #[test]
    fn durable_receipt_ordinary_credit_survives_omission_before_callback() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let (mut w, tx) = builder_accounting_fixture();
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            tx.transparent_bundle().unwrap().vout[0].clone(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        builder_accounting_store(&mut cold, &tx, false);
        builder_accounting_store(&mut cold, &tx, false);
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!((row.received_zat, row.received_note_count), (17000, 1));
    }

    #[test]
    fn durable_receipt_legacy_pir_ambiguity_refuses_callback_before_mutation() {
        let (mut w, tx) = builder_accounting_fixture();
        let mut event = receive();
        if let TransparentEvent::Receive(e) = &mut event {
            e.txid = Txid(*tx.txid().as_ref());
            e.height = 3;
            e.value = 17000;
        }
        let c = checkpoint(&mut w, vec![event]);
        apply_pir(&mut w, &c);
        let id = super::super::super::to_hex(tx.txid().as_ref());
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        old["txs"][&id]["receivedZat"] = 17000.into();
        old["txs"][&id]["receivedNotes"] = 1.into();
        old["txs"][&id]
            .as_object_mut()
            .unwrap()
            .remove("pirReceivedZat");
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], true);
        let before = cold.to_snapshot().unwrap();
        assert!(
            builder_accounting_store_result(&mut cold, &tx, false).is_err(),
            "ambiguous legacy credit must refuse atomically"
        );
        assert!(
            cold.to_snapshot().unwrap() == before,
            "legacy refusal must preserve the entire wallet"
        );
    }

    #[test]
    fn durable_receipt_two_indices_and_late_batch_conflict_are_atomic() {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::{Recipient, WalletTransparentOutput};
        use zcash_primitives::transaction::{Authorized, TransactionData};
        use zcash_protocol::{consensus::BranchId, value::Zatoshis};
        let (mut w, template) = builder_accounting_fixture();
        let output = template.transparent_bundle().unwrap().vout[0].clone();
        let mut bundle = template.transparent_bundle().unwrap().clone();
        bundle.vout.push(transparent::bundle::TxOut::new(
            Zatoshis::from_u64(3000).unwrap(),
            output.script_pubkey().clone(),
        ));
        let tx = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            20.into(),
            Some(bundle),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        for index in 0..2 {
            let output = WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new(*tx.txid().as_ref(), index),
                tx.transparent_bundle().unwrap().vout[index as usize].clone(),
                None,
                None,
                None,
                None,
            )
            .unwrap();
            w.put_received_transparent_utxo(&output).unwrap();
        }
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        let recipient = output.recipient_address().unwrap();
        let make = |index, value| {
            SentTransactionOutput::from_parts(
                index,
                Recipient::InternalTransparent {
                    receiving_account: 0,
                    recipient_address: recipient,
                },
                Zatoshis::from_u64(value).unwrap(),
                None,
            )
        };
        let outputs = [make(0, 17000), make(1, 3000)];
        fn callback<'a>(
            tx: &'a zcash_primitives::transaction::Transaction,
            outputs: &'a [SentTransactionOutput<u32>],
        ) -> SentTransaction<'a, u32> {
            SentTransaction::new(
                tx,
                std::time::UNIX_EPOCH.into(),
                3.into(),
                0,
                outputs,
                Zatoshis::from_u64(7000).unwrap(),
                &[],
            )
        }
        let good = callback(&tx, &outputs);
        let bad_outputs = [make(1, 4000)];
        let bad = callback(&tx, &bad_outputs);
        let before = cold.to_snapshot().unwrap();
        assert!(cold.store_transactions_to_be_sent(&[good, bad]).is_err());
        assert!(
            cold.to_snapshot().unwrap() == before,
            "batch must preflight before saving pending bytes or spending inputs"
        );
        for _ in 0..2 {
            cold.store_transactions_to_be_sent(&[callback(&tx, &outputs)])
                .unwrap();
        }
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.received_zat,
                row.received_note_count,
                row.transparent_received
            ),
            (20000, 2, 20000)
        );
        let before = cold.to_snapshot().unwrap();
        let id = super::super::super::to_hex(tx.txid().as_ref());
        assert!(cold
            .apply_utxos_json(
                &serde_json::json!({"utxos":[{"txid":id,"index":1,"valueZat":4000,"height":1}]})
                    .to_string()
            )
            .is_err());
        assert!(cold.to_snapshot().unwrap() == before);
    }

    #[cfg(feature = "native")]
    #[test]
    fn durable_receipt_builder_inclusion_rewind_and_expiry_keep_claim_pending_only() {
        let (mut w, tx) = builder_mixed_fixture(true);
        let mut raw = Vec::new();
        tx.write(&mut raw).unwrap();
        let mut block = w.mined_raw_tx_block(&raw, 0).unwrap();
        block.prev_hash = vec![3; 32];
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([block]))
            .unwrap();
        assert_eq!(w.balance().transparent_pending, 17000);
        w.rewind_to_height(3).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_pending, 17000);
        store_mixed_builder_receipt(&mut cold, &tx);
        assert_eq!(cold.balance().transparent_pending, 17000);
        assert_eq!(
            cold.txs()
                .get(&super::super::super::to_hex(tx.txid().as_ref()))
                .unwrap()
                .mined_height,
            None
        );
        assert!(cold.abandon_unmined(&tx.txid().to_string()).unwrap());
        assert_eq!(cold.balance().transparent_pending, 27000);
    }

    #[cfg(feature = "native")]
    #[test]
    fn durable_receipt_clean_legacy_shielded_callback_still_works() {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::{Note, Recipient};
        use zcash_protocol::value::Zatoshis;
        let mut w = wallet();
        let raw = crate::web::shielded_memo_transaction(&w, &w, "legacy control", 30000, true);
        let tx = zcash_primitives::transaction::Transaction::read(
            &raw[..],
            zcash_protocol::consensus::BranchId::Nu6_3,
        )
        .unwrap();
        let key = w.decode_ufvk().unwrap();
        let mut keys = std::collections::HashMap::new();
        keys.insert(0, key);
        let decrypted = zcash_client_backend::decrypt_transaction(
            &w.network(),
            None,
            Some(3.into()),
            &tx,
            &keys,
        );
        let note = decrypted
            .ironwood_outputs()
            .iter()
            .find(|o| {
                matches!(
                    o.transfer_type(),
                    zcash_client_backend::TransferType::Incoming
                        | zcash_client_backend::TransferType::AccountInternal
                )
            })
            .unwrap();
        let outputs = [SentTransactionOutput::from_parts(
            note.index(),
            Recipient::InternalShielded {
                receiving_account: 0,
                external_address: None,
                note: Box::new(Note::Orchard {
                    note: note.note().0.clone(),
                    pool: orchard::ValuePool::Ironwood,
                }),
            },
            Zatoshis::from_u64(30000).unwrap(),
            None,
        )];
        let id = super::super::super::to_hex(tx.txid().as_ref());
        w.txs_mut().entry(id.clone()).or_default();
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        old["txs"][&id]
            .as_object_mut()
            .unwrap()
            .remove("receiptAccountingLegacy");
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        let callback = || {
            SentTransaction::new(
                &tx,
                std::time::UNIX_EPOCH.into(),
                3.into(),
                0,
                &outputs,
                Zatoshis::from_u64(10000).unwrap(),
                &[],
            )
        };
        cold.store_transactions_to_be_sent(&[callback()]).unwrap();
        cold.store_transactions_to_be_sent(&[callback()]).unwrap();
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.received_zat,
                row.received_note_count,
                row.ironwood_received
            ),
            (30000, 1, 30000)
        );
    }

    #[test]
    fn durable_receipt_confirmed_materialization_does_not_revive_after_omission() {
        use zcash_client_backend::{data_api::WalletWrite, wallet::WalletTransparentOutput};
        let (mut w, tx) = builder_accounting_fixture();
        builder_accounting_store(&mut w, &tx, true);
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            tx.transparent_bundle().unwrap().vout[0].clone(),
            Some(1.into()),
            None,
            None,
            None,
        )
        .unwrap();
        w.put_received_transparent_utxo(&output).unwrap();
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(
            cold.balance().transparent_pending + cold.balance().transparent_available,
            0
        );
        builder_accounting_store(&mut cold, &tx, true);
        assert_eq!(
            cold.balance().transparent_pending + cold.balance().transparent_available,
            0
        );
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.received_zat,
                row.received_note_count,
                row.transparent_received
            ),
            (17000, 1, 17000)
        );
        assert!(cold
            .txs()
            .get(&super::super::super::to_hex(tx.txid().as_ref()))
            .unwrap()
            .mined_height
            .is_some());
    }

    #[test]
    fn durable_receipt_mapless_legacy_batch_refusal_preserves_all_intent() {
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::Recipient;
        use zcash_protocol::value::Zatoshis;
        let (w, tx) = builder_accounting_fixture();
        let id = super::super::super::to_hex(tx.txid().as_ref());
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        // Genuine old schema shape: historical independent receipt totals, without
        // a per-index receipt ledger or a current accounting marker.
        old["txs"][&id] = serde_json::to_value(TxAgg {
            received_zat: 17000,
            received_notes: 1,
            transparent_received: 17000,
            ..Default::default()
        })
        .unwrap();
        old["txs"][&id]
            .as_object_mut()
            .unwrap()
            .remove("accountedTransparentReceipts");
        old["txs"][&id]
            .as_object_mut()
            .unwrap()
            .remove("receiptAccountingLegacy");
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        let goodtx = zcash_primitives::transaction::TransactionData::<
            zcash_primitives::transaction::Authorized,
        >::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            21.into(),
            tx.transparent_bundle().cloned(),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        assert_ne!(goodtx.txid(), tx.txid());
        let output = |transaction: &zcash_primitives::transaction::Transaction| {
            SentTransactionOutput::from_parts(
                0,
                Recipient::InternalTransparent {
                    receiving_account: 0,
                    recipient_address: transaction.transparent_bundle().unwrap().vout[0]
                        .recipient_address()
                        .unwrap(),
                },
                Zatoshis::from_u64(17000).unwrap(),
                None,
            )
        };
        let good_outputs = [output(&goodtx)];
        let bad_outputs = [output(&tx)];
        let good = SentTransaction::new(
            &goodtx,
            std::time::UNIX_EPOCH.into(),
            3.into(),
            0,
            &good_outputs,
            Zatoshis::from_u64(10000).unwrap(),
            &[],
        );
        let bad = SentTransaction::new(
            &tx,
            std::time::UNIX_EPOCH.into(),
            3.into(),
            0,
            &bad_outputs,
            Zatoshis::from_u64(10000).unwrap(),
            &[],
        );
        let before = cold.to_snapshot().unwrap();
        assert!(cold.store_transactions_to_be_sent(&[good, bad]).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        assert!(cold.pending_raw_txs().is_empty());
    }

    #[cfg(feature = "native")]
    #[test]
    fn durable_receipt_omitted_input_callback_inclusion_rewind_retains_signed_intent() {
        let (mut w, tx) = builder_mixed_fixture(false);
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        assert!(w.utxos.is_empty());
        store_mixed_builder_receipt(&mut w, &tx);
        assert_eq!(w.pending_raw_txs().len(), 1);
        let row = w
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.spent_zat, row.sent_note_count, row.transparent_spent),
            (27000, 1, 27000)
        );
        assert_eq!(w.balance().transparent_pending, 17000);
        let mut raw = Vec::new();
        tx.write(&mut raw).unwrap();
        let mut block = w.mined_raw_tx_block(&raw, 0).unwrap();
        block.prev_hash = vec![3; 32];
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([block]))
            .unwrap();
        assert_eq!(w.pending_raw_txs().len(), 0);
        assert!(w.pending_txs.contains_key(&tx.txid()));
        w.rewind_to_height(3).unwrap();
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(
            cold.pending_raw_txs().len(),
            1,
            "exact saved signed transaction must survive compact rewind despite omitted input row"
        );
        assert_eq!(cold.balance().transparent_pending, 17000);
        let row = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.spent_zat, row.sent_note_count, row.transparent_spent),
            (27000, 1, 27000)
        );
        assert_eq!(
            cold.history_matching(1, None, Some(&tx.txid().to_string()))
                .len(),
            1
        );
    }

    fn confirmed_conflict_fixture() -> (
        WebWallet,
        zcash_primitives::transaction::Transaction,
        zcash_primitives::transaction::Transaction,
    ) {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        let (mut w, original) = builder_accounting_fixture();
        w.scanned_height = 0;
        w.block_hashes.clear();
        let mut chain = public_range(&mut w, false);
        for block in &mut chain {
            block.vtx.clear();
        }
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited(chain))
            .unwrap();
        builder_accounting_store(&mut w, &original, false);
        let mut bundle = original.transparent_bundle().unwrap().clone();
        bundle.vout[0] = transparent::bundle::TxOut::new(
            bundle.vout[0].value(),
            transparent::address::TransparentAddress::PublicKeyHash([5; 20])
                .script()
                .into(),
        );
        let replacement = TransactionData::<Authorized>::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            10.into(),
            Some(bundle),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        (w, original, replacement)
    }

    fn include_conflicting_spender(
        w: &mut WebWallet,
        replacement: &zcash_primitives::transaction::Transaction,
    ) {
        include_conflicting_spenders(w, &[(1, replacement)]);
    }

    fn include_conflicting_spenders(
        w: &mut WebWallet,
        replacements: &[(usize, &zcash_primitives::transaction::Transaction)],
    ) {
        let blocks = public_spender_blocks(w, replacements);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
    }

    fn public_spender_blocks(
        w: &mut WebWallet,
        replacements: &[(usize, &zcash_primitives::transaction::Transaction)],
    ) -> Vec<zcash_client_backend::proto::compact_formats::CompactBlock> {
        use zcash_client_backend::proto::compact_formats::{CompactTx, CompactTxIn, TxOut};
        let mut blocks = public_range(w, false);
        blocks[0].vtx.pop();
        for (block_index, replacement) in replacements {
            blocks[*block_index].vtx.push(CompactTx {
                index: 1,
                txid: replacement.txid().as_ref().to_vec(),
                vin: replacement
                    .transparent_bundle()
                    .unwrap()
                    .vin
                    .iter()
                    .map(|input| CompactTxIn {
                        prevout_txid: input.prevout().hash().to_vec(),
                        prevout_index: input.prevout().n(),
                    })
                    .collect(),
                vout: replacement
                    .transparent_bundle()
                    .unwrap()
                    .vout
                    .iter()
                    .map(|output| TxOut {
                        value: u64::from(output.value()),
                        script_pub_key: output.script_pubkey().0 .0.clone(),
                    })
                    .collect(),
                ..Default::default()
            });
        }
        blocks
    }

    #[test]
    fn confirmed_conflict_preserves_signed_intent_but_stops_submission_and_pending_change() {
        let (mut w, original, replacement) = confirmed_conflict_fixture();
        assert_eq!(w.pending_raw_txs().len(), 1);
        assert_eq!(w.balance().transparent_pending, 17000);
        include_conflicting_spender(&mut w, &replacement);
        let before = w.to_snapshot().unwrap();
        assert!(builder_accounting_store_result(&mut w, &original, false).is_err());
        assert!(w.to_snapshot().unwrap() == before);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(cold.pending_txs.contains_key(&original.txid()));
        assert!(
            cold.pending_submissions().is_empty(),
            "a different confirmed spender prevents rebroadcast of the saved intent"
        );
        assert_eq!(cold.balance().total_pending, 0);
        let row = cold
            .history_matching(1, None, Some(&original.txid().to_string()))
            .remove(0);
        assert_eq!((row.spent_zat, row.sent_note_count), (27000, 1));
        assert_eq!(row.mined_height, None);
    }

    #[test]
    fn confirmed_conflict_does_not_contribute_unmined_change_balance() {
        let (mut w, _original, replacement) = confirmed_conflict_fixture();
        include_conflicting_spender(&mut w, &replacement);
        assert_eq!(w.balance().transparent_pending, 0);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().total_pending, 0);
    }

    #[test]
    fn confirmed_conflict_rewind_restores_saved_claims_and_rescan_cannot_drop_intent() {
        let (mut w, original, replacement) = confirmed_conflict_fixture();
        let saved = w.pending_txs[&original.txid()].clone();
        include_conflicting_spender(&mut w, &replacement);
        let before = w.to_snapshot().unwrap();
        assert!(w
            .rescan_from(1)
            .unwrap_err()
            .to_string()
            .contains("rescan pending payment"));
        assert!(w.to_snapshot().unwrap() == before);
        assert_eq!(w.rewind_to_height(1).unwrap(), 1);
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.pending_txs[&original.txid()], saved);
        assert_eq!(cold.pending_submissions().len(), 1);
        assert_eq!(cold.balance().transparent_pending, 17000);
        let original_id = super::super::super::to_hex(original.txid().as_ref());
        let input = cold
            .utxos
            .iter()
            .find(|u| u.txid == "09".repeat(32))
            .unwrap();
        assert!(input.spent);
        assert_eq!(input.spent_in.as_deref(), Some(original_id.as_str()));
        assert!(cold.abandon_unmined(&original.txid().to_string()).unwrap());
        assert_eq!(cold.balance().transparent_pending, 27000);
        assert!(!cold.pending_txs.contains_key(&original.txid()));
    }

    #[cfg(feature = "native")]
    #[test]
    fn confirmed_conflict_suppresses_mixed_pool_pending_outputs() {
        let (mut w, original) = builder_mixed_fixture(true);
        let mut bundle = original.transparent_bundle().unwrap().clone();
        bundle.vout[0] = transparent::bundle::TxOut::new(
            bundle.vout[0].value(),
            transparent::address::TransparentAddress::PublicKeyHash([5; 20])
                .script()
                .into(),
        );
        let replacement = zcash_primitives::transaction::TransactionData::<
            zcash_primitives::transaction::Authorized,
        >::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            10.into(),
            Some(bundle),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        include_conflicting_spender(&mut w, &replacement);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().total_pending, 0);
        assert!(cold.pending_txs.contains_key(&original.txid()));
        assert!(cold.pending_submissions().is_empty());
        let row = cold
            .history_matching(1, None, Some(&original.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.received_zat, row.ironwood_received, row.spent_zat),
            (47000, 30000, 27000)
        );
    }

    #[test]
    fn confirmed_conflict_requires_all_inputs_clear_before_reactivating_saved_intent() {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        let (mut w, template, _) = confirmed_conflict_fixture();
        w.abandon_unmined(&template.txid().to_string()).unwrap();
        let first = w.utxos[0].clone();
        let mut second = first.clone();
        second.txid = "08".repeat(32);
        second.value_zat = 10000;
        w.apply_utxos_json(&serde_json::json!({"utxos":[
            {"txid":first.txid,"index":0,"script":first.script,"height":1,"valueZat":27000,"address":first.address},
            {"txid":second.txid,"index":0,"script":second.script,"height":1,"valueZat":10000,"address":second.address}
        ]}).to_string()).unwrap();
        let mut bundle = template.transparent_bundle().unwrap().clone();
        bundle.vin.push(transparent::bundle::TxIn::from_parts(
            transparent::bundle::OutPoint::new([8; 32], 0),
            transparent::address::Script::default(),
            u32::MAX,
        ));
        bundle.vout[0] = transparent::bundle::TxOut::new(
            zcash_protocol::value::Zatoshis::from_u64(27000).unwrap(),
            bundle.vout[0].script_pubkey().clone(),
        );
        let original = TransactionData::<Authorized>::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            6.into(),
            Some(bundle.clone()),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        builder_accounting_store(&mut w, &original, false);
        let replacements: Vec<_> = [0usize, 1]
            .into_iter()
            .map(|input| {
                let mut selected = bundle.clone();
                selected.vin = vec![selected.vin[input].clone()];
                selected.vout[0] = transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(if input == 0 {
                        17000
                    } else {
                        7000
                    })
                    .unwrap(),
                    transparent::address::TransparentAddress::PublicKeyHash([5; 20])
                        .script()
                        .into(),
                );
                TransactionData::<Authorized>::from_parts_v6(
                    zcash_protocol::consensus::BranchId::Nu6_3,
                    0,
                    10.into(),
                    Some(selected),
                    None,
                    None,
                    None,
                )
                .freeze()
                .unwrap()
            })
            .collect();
        let mut one_conflict = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        include_conflicting_spender(&mut one_conflict, &replacements[0]);
        assert!(
            one_conflict
                .utxos
                .iter()
                .find(|u| u.txid == "08".repeat(32))
                .unwrap()
                .spent
        );
        assert_eq!(one_conflict.balance().total_pending, 0);
        for height in 4..=6 {
            one_conflict
                .apply_compact_blocks_blob(&super::super::super::encode_delimited([
                    zcash_client_backend::proto::compact_formats::CompactBlock {
                        height,
                        hash: vec![height as u8; 32],
                        prev_hash: vec![height as u8 - 1; 32],
                        ..Default::default()
                    },
                ]))
                .unwrap();
        }
        let expired = WebWallet::from_snapshot(&one_conflict.to_snapshot().unwrap()).unwrap();
        assert!(!expired.pending_txs.contains_key(&original.txid()));
        assert_eq!(expired.balance().total_pending, 10000);
        assert!(
            expired
                .utxos
                .iter()
                .find(|u| u.txid == "09".repeat(32))
                .unwrap()
                .spent
        );
        assert!(
            !expired
                .utxos
                .iter()
                .find(|u| u.txid == "08".repeat(32))
                .unwrap()
                .spent
        );
        include_conflicting_spenders(&mut w, &[(1, &replacements[0]), (2, &replacements[1])]);
        assert!(w.pending_submissions().is_empty());
        assert_eq!(w.rewind_to_height(2).unwrap(), 2);
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(cold.pending_submissions().is_empty());
        assert_eq!(
            cold.utxos
                .iter()
                .find(|u| u.txid == "09".repeat(32))
                .unwrap()
                .spent_in
                .as_deref(),
            Some(super::super::super::to_hex(replacements[0].txid().as_ref()).as_str())
        );
        // Only the second independent input was released by this rewind; the
        // saved transaction's 27000 change remains ineligible.
        assert_eq!(cold.balance().total_pending, 10000);
        assert_eq!(cold.rewind_to_height(1).unwrap(), 1);
        let cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.pending_submissions().len(), 1);
        assert_eq!(cold.balance().transparent_pending, 27000);
        assert!(cold.utxos.iter().all(|u| u.spent));
    }

    #[test]
    fn publisher_only_spend_does_not_disqualify_saved_independent_intent() {
        let (mut w, parent) = builder_accounting_fixture();
        builder_accounting_store(&mut w, &parent, false);
        let own = external_receipt_spender(&parent, 10);
        store_external_receipt_spender(&mut w, &own).unwrap();
        let other = external_receipt_spender(&parent, 11);
        let mut claimed_receive = receive();
        if let TransparentEvent::Receive(event) = &mut claimed_receive {
            event.txid = Txid(*parent.txid().as_ref());
            event.value = 17000;
        }
        let mut claimed_spend = spend();
        if let TransparentEvent::Spend(event) = &mut claimed_spend {
            event.spending_txid = Txid(*other.txid().as_ref());
            event.spent_txid = Txid(*parent.txid().as_ref());
        }
        let cp = checkpoint(&mut w, vec![claimed_receive, claimed_spend]);
        apply_pir(&mut w, &cp);
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(cold
            .pending_submissions()
            .iter()
            .any(|entry| entry.txid == own.txid().to_string()));
        assert!(!cold
            .ineligible_pending_transactions()
            .contains(&super::super::super::to_hex(own.txid().as_ref())));
        assert!(
            cold.pir_funding_unverified(&super::super::super::to_hex(parent.txid().as_ref()), 0)
        );
        assert_eq!(cold.balance().transparent_available, 0);
    }

    #[test]
    fn confirmed_conflict_propagates_through_rewound_saved_descendant_and_cold() {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        let (mut w, template, replacement) = confirmed_conflict_fixture();
        w.abandon_unmined(&template.txid().to_string()).unwrap();
        let parent = TransactionData::<Authorized>::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            20.into(),
            template.transparent_bundle().cloned(),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        builder_accounting_store(&mut w, &parent, false);
        let child = external_receipt_spender(&parent, 21);
        let blocks = public_spender_blocks(&mut w, &[(1, &parent), (2, &child)]);
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks[..2].to_vec()))
            .unwrap();
        assert_eq!(
            w.txs()[&super::super::super::to_hex(parent.txid().as_ref())].mined_height,
            Some(2)
        );
        store_external_receipt_spender(&mut w, &child).unwrap();
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks[2..].to_vec()))
            .unwrap();
        assert_eq!(
            w.txs()[&super::super::super::to_hex(child.txid().as_ref())].mined_height,
            Some(3)
        );
        w.rewind_to_height(1).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.pending_submissions().len(), 2);
        let new_chain = public_spender_blocks(&mut cold, &[(1, &replacement)]);
        cold.apply_compact_blocks_blob(&super::super::super::encode_delimited(
            new_chain[1..]
                .iter()
                .map(|block| {
                    let mut compact = block.clone();
                    compact.vtx.clear();
                    compact
                })
                .collect::<Vec<_>>(),
        ))
        .unwrap();
        cold.apply_transparent_blocks(&super::super::super::encode_delimited(
            new_chain[1..].to_vec(),
        ))
        .unwrap();
        let mut cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        assert!(cold.pending_submissions().is_empty());
        assert!(cold.pending_txs.contains_key(&parent.txid()));
        assert!(cold.pending_txs.contains_key(&child.txid()));
        assert_eq!(cold.balance().total_pending, 0);
        cold.rewind_to_height(1).unwrap();
        let cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.pending_submissions().len(), 2);
        assert_eq!(cold.balance().total_pending, 0);
        let receipt = &cold.txs()[&super::super::super::to_hex(parent.txid().as_ref())]
            .accounted_transparent_receipts[&0];
        assert_eq!(
            receipt.spent_in.as_ref().unwrap().txid,
            super::super::super::to_hex(child.txid().as_ref())
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn confirmed_conflict_preserves_shielded_child_origin_after_actual_mining_rewind_and_cold() {
        use zcash_client_backend::data_api::{SentTransaction, WalletWrite};
        use zcash_primitives::transaction::Transaction;
        let (mut w, parent) = builder_mixed_fixture_pool(true, false);
        let mut parent_raw = Vec::new();
        parent.write(&mut parent_raw).unwrap();
        let mut parent_block = w.mined_raw_tx_block(&parent_raw, 1).unwrap();
        parent_block.prev_hash = vec![3; 32];
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([parent_block]))
            .unwrap();
        assert_eq!(w.balance().orchard_available, 30000);
        let destination = w.unified_address().to_owned();
        // The parent fixture has genuine encrypted notes but inert T signatures.
        // This child uses the normal Rust proposal/proving/WalletWrite path.
        let child_raw = w
            .prove_send(REGTEST_FAUCET_MNEMONIC, &destination, "0.00001", None)
            .unwrap();
        let child =
            Transaction::read(&child_raw[..], zcash_protocol::consensus::BranchId::Nu6_3).unwrap();
        let parent_id = super::super::super::to_hex(parent.txid().as_ref());
        let child_id = super::super::super::to_hex(child.txid().as_ref());
        assert!(w.txs()[&child_id]
            .shielded_input_origins
            .0
            .as_ref()
            .unwrap()
            .contains(&parent_id));
        w.apply_mined_raw_tx(&child_raw, 1).unwrap();
        w.rewind_to_height(3).unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(cold.notes().iter().all(|note| note.txid != parent_id));
        assert!(cold.spend_notes.iter().all(|note| note.txid != parent_id));
        let known = cold.txs()[&child_id].shielded_input_origins.0.clone();
        let fee = zcash_protocol::value::Zatoshis::from_u64(cold.txs()[&child_id].fee_zat.unwrap())
            .unwrap();
        cold.store_transactions_to_be_sent(&[SentTransaction::new(
            &child,
            std::time::UNIX_EPOCH.into(),
            5.into(),
            0,
            &[],
            fee,
            &[],
        )])
        .unwrap();
        assert_eq!(cold.txs()[&child_id].shielded_input_origins.0, known);
        let mut bundle = parent.transparent_bundle().unwrap().clone();
        bundle.vout[0] = transparent::bundle::TxOut::new(
            bundle.vout[0].value(),
            transparent::address::TransparentAddress::PublicKeyHash([5; 20])
                .script()
                .into(),
        );
        let replacement = zcash_primitives::transaction::TransactionData::<
            zcash_primitives::transaction::Authorized,
        >::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            10.into(),
            Some(bundle),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        include_conflicting_spender(&mut cold, &replacement);
        let cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        assert!(cold.ineligible_pending_transactions().contains(&parent_id));
        assert!(cold.ineligible_pending_transactions().contains(&child_id));
        assert!(cold.pending_submissions().is_empty());
        assert!(cold.pending_txs.contains_key(&parent.txid()));
        assert!(cold.pending_txs.contains_key(&child.txid()));
        assert_eq!(cold.balance().total_pending, 0);
    }

    #[test]
    fn confirmed_conflict_unknown_old_own_lineage_refuses_without_erasing_signed_intent() {
        let (w, original, _) = confirmed_conflict_fixture();
        let id = super::super::super::to_hex(original.txid().as_ref());
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        old["txs"][&id]
            .as_object_mut()
            .unwrap()
            .remove("shieldedInputOrigins");
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], true);
        assert!(cold.pending_txs.contains_key(&original.txid()));
        assert!(cold.pending_submissions().is_empty());
        assert_eq!(cold.balance().total_pending, 0);
        let before = cold.to_snapshot().unwrap();
        assert!(builder_accounting_store_result(&mut cold, &original, false).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        assert!(cold.rescan_from(1).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        cold.pending_txs.remove(&original.txid());
        assert!(
            !cold.legacy_transparent_receipt_accounting(),
            "old non-signed history does not require lineage metadata"
        );
    }

    #[test]
    fn expired_debit_is_history_not_an_extra_current_balance() {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        let (mut w, template) = builder_accounting_fixture();
        let tx = TransactionData::<Authorized>::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            4.into(),
            template.transparent_bundle().cloned(),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        builder_accounting_store(&mut w, &tx, false);
        let before = w
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([
            zcash_client_backend::proto::compact_formats::CompactBlock {
                height: 4,
                hash: vec![4; 32],
                prev_hash: vec![3; 32],
                ..Default::default()
            },
        ]))
        .unwrap();
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        let expired = cold
            .history_matching(1, None, Some(&tx.txid().to_string()))
            .remove(0);
        assert!(expired.expired_unmined);
        assert_eq!(
            (
                expired.spent_zat,
                expired.sent_note_count,
                expired.account_delta_zat
            ),
            (
                before.spent_zat,
                before.sent_note_count,
                before.account_delta_zat
            )
        );
        assert!(cold.pending_submissions().is_empty());
        assert_eq!(cold.balance().total_pending, 27000);
    }

    fn external_receipt_spender(
        parent: &zcash_primitives::transaction::Transaction,
        expiry: u32,
    ) -> zcash_primitives::transaction::Transaction {
        use zcash_primitives::transaction::{Authorized, TransactionData};
        TransactionData::<Authorized>::from_parts_v6(
            zcash_protocol::consensus::BranchId::Nu6_3,
            0,
            expiry.into(),
            Some(transparent::bundle::Bundle {
                vin: vec![transparent::bundle::TxIn::from_parts(
                    transparent::bundle::OutPoint::new(*parent.txid().as_ref(), 0),
                    transparent::address::Script::default(),
                    u32::MAX,
                )],
                vout: vec![transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(7000).unwrap(),
                    transparent::address::TransparentAddress::PublicKeyHash([5; 20])
                        .script()
                        .into(),
                )],
                authorization: transparent::bundle::Authorized,
            }),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap()
    }

    fn store_external_receipt_spender(
        w: &mut WebWallet,
        tx: &zcash_primitives::transaction::Transaction,
    ) -> std::result::Result<(), String> {
        use zcash_address::ToAddress;
        use zcash_client_backend::data_api::{SentTransaction, SentTransactionOutput, WalletWrite};
        use zcash_client_backend::wallet::Recipient;
        let outputs = [SentTransactionOutput::from_parts(
            0,
            Recipient::External {
                recipient_address: zcash_address::ZcashAddress::from_transparent_p2pkh(
                    zcash_protocol::consensus::NetworkType::Regtest,
                    [5; 20],
                ),
                output_pool: zcash_protocol::PoolType::Transparent,
            },
            zcash_protocol::value::Zatoshis::from_u64(7000).unwrap(),
            None,
        )];
        w.store_transactions_to_be_sent(&[SentTransaction::new(
            tx,
            std::time::UNIX_EPOCH.into(),
            3.into(),
            0,
            &outputs,
            zcash_protocol::value::Zatoshis::from_u64(10000).unwrap(),
            &[],
        )])
    }

    #[test]
    fn durable_debit_public_receipt_only_spend_survives_expiry_rewind_and_replay() {
        let (mut w, parent) = builder_accounting_fixture();
        builder_accounting_store(&mut w, &parent, true);
        let child = external_receipt_spender(&parent, 4);
        for _ in 0..2 {
            store_external_receipt_spender(&mut w, &child).unwrap();
        }
        assert_eq!(w.balance().transparent_pending, 0);
        let row = w
            .history_matching(1, None, Some(&child.txid().to_string()))
            .remove(0);
        assert_eq!(
            (
                row.spent_zat,
                row.sent_note_count,
                row.transparent_spent,
                row.account_delta_zat
            ),
            (17000, 1, 17000, -17000)
        );
        let other = external_receipt_spender(&parent, 5);
        let before = w.to_snapshot().unwrap();
        assert!(store_external_receipt_spender(&mut w, &other).is_err());
        assert!(
            w.to_snapshot().unwrap() == before,
            "conflicting callback must not save another signed intent"
        );
        assert!(w.abandon_unmined(&child.txid().to_string()).unwrap());
        assert_eq!(w.balance().transparent_pending, 17000);
        for _ in 0..2 {
            store_external_receipt_spender(&mut w, &child).unwrap();
        }
        let row = w
            .history_matching(1, None, Some(&child.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.spent_zat, row.sent_note_count, row.transparent_spent),
            (17000, 1, 17000)
        );
        let mut blocks = public_range(&mut w, false);
        blocks[0].vtx.pop();
        blocks[1]
            .vtx
            .push(zcash_client_backend::proto::compact_formats::CompactTx {
                index: 1,
                txid: child.txid().as_ref().to_vec(),
                vin: vec![zcash_client_backend::proto::compact_formats::CompactTxIn {
                    prevout_txid: parent.txid().as_ref().to_vec(),
                    prevout_index: 0,
                }],
                vout: vec![zcash_client_backend::proto::compact_formats::TxOut {
                    value: 7000,
                    script_pub_key: child.transparent_bundle().unwrap().vout[0]
                        .script_pubkey()
                        .0
                         .0
                        .clone(),
                }],
                ..Default::default()
            });
        w.apply_transparent_blocks(&super::super::super::encode_delimited(blocks))
            .unwrap();
        store_external_receipt_spender(&mut w, &child).unwrap();
        let id = super::super::super::to_hex(child.txid().as_ref());
        assert_eq!(w.txs().get(&id).unwrap().mined_height, Some(2));
        w.apply_compact_blocks_blob(&super::super::super::encode_delimited([
            zcash_client_backend::proto::compact_formats::CompactBlock {
                height: 4,
                hash: vec![4; 32],
                prev_hash: vec![3; 32],
                ..Default::default()
            },
        ]))
        .unwrap();
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        let previous = super::super::super::to_hex(parent.txid().as_ref());
        assert_eq!(
            cold.txs()
                .get(&previous)
                .unwrap()
                .accounted_transparent_receipts[&0]
                .spent_in
                .as_ref()
                .unwrap()
                .height,
            Some(2)
        );
        assert_eq!(cold.balance().transparent_pending, 0);
        assert_eq!(cold.txs().get(&id).unwrap().mined_height, Some(2));
        let row = cold
            .history_matching(1, None, Some(&child.txid().to_string()))
            .remove(0);
        assert_eq!(
            (row.spent_zat, row.sent_note_count, row.transparent_spent),
            (17000, 1, 17000)
        );
        assert!(!cold
            .pending_submissions()
            .iter()
            .any(|pending| pending.txid == child.txid().to_string()));
        let before = cold.to_snapshot().unwrap();
        assert!(cold.abandon_unmined(&child.txid().to_string()).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        cold.rewind_to_height(0).unwrap();
        cold = WebWallet::from_snapshot(&cold.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.txs().get(&id).unwrap().mined_height, None);
        assert!(cold.pending_txs.contains_key(&child.txid()));
        assert_eq!(cold.balance().transparent_pending, 0);
        for height in 1u32..=4 {
            cold.apply_compact_blocks_blob(&super::super::super::encode_delimited([
                zcash_client_backend::proto::compact_formats::CompactBlock {
                    height: height.into(),
                    hash: vec![(height * 11) as u8; 32],
                    prev_hash: if height == 1 {
                        vec![0; 32]
                    } else {
                        vec![((height - 1) * 11) as u8; 32]
                    },
                    ..Default::default()
                },
            ]))
            .unwrap();
        }
        assert_eq!(
            cold.balance().transparent_pending,
            17000,
            "only genuinely unmined expiry releases the parent claim"
        );
    }

    #[test]
    fn durable_debit_old_pointer_without_credit_identity_refuses_before_mutation() {
        let (mut w, parent) = builder_accounting_fixture();
        builder_accounting_store(&mut w, &parent, false);
        let child = external_receipt_spender(&parent, 20);
        store_external_receipt_spender(&mut w, &child).unwrap();
        let previous = super::super::super::to_hex(parent.txid().as_ref());
        let id = super::super::super::to_hex(child.txid().as_ref());
        let mut old: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        old["txs"][&previous]["accountedTransparentReceipts"]["0"]["spentIn"]
            .as_object_mut()
            .unwrap()
            .remove("accounted");
        for (_, agg) in old["txs"].as_object_mut().unwrap() {
            agg.as_object_mut()
                .unwrap()
                .remove("accountedTransparentSpends");
        }
        let mut cold = WebWallet::from_snapshot(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], true);
        let before = cold.to_snapshot().unwrap();
        assert!(store_external_receipt_spender(&mut cold, &child).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        assert!(cold.rescan_from(1).is_err());
        assert!(cold.to_snapshot().unwrap() == before);
        assert!(cold.pending_txs.contains_key(&child.txid()));
        assert!(cold.txs().contains_key(&id));
    }

    fn off_refresh_fixture() -> (WebWallet, TransparentPirCheckpoint) {
        let mut w = wallet();
        w.next_unified_address().unwrap();
        let current = w.transparent_address().unwrap().to_owned();
        let script = w
            .public_transparent_scripts()
            .unwrap()
            .into_iter()
            .find(|(_, address)| address == &current)
            .unwrap()
            .0;
        let mut c = checkpoint(&mut w, vec![receive()]);
        let mut current_receive = receive();
        if let TransparentEvent::Receive(event) = &mut current_receive {
            event.txid = Txid([3; 32]);
            event.height = 3;
            event.value = 17000;
            event.coinbase = true;
            event.transaction_index = 0;
        }
        c.records.push(TransparentPirRecord {
            script: super::super::super::to_hex(&script),
            event: super::super::super::to_hex(&current_receive.to_bytes()),
            shard_id: 0,
            revision: "ef".repeat(32),
        });
        c.continuation = "opaque-acquisition-with-logical-cover-debt".into();
        apply_pir(&mut w, &c);
        assert_ne!(
            w.utxos
                .iter()
                .find(|u| u.txid == "01".repeat(32))
                .unwrap()
                .address,
            current
        );
        (w, c)
    }
    fn off_refresh_listed(w: &WebWallet, byte: u8) -> serde_json::Value {
        let id = format!("{byte:02x}").repeat(32);
        let u = w.utxos.iter().find(|u| u.txid == id).unwrap();
        serde_json::json!({"txid":u.txid,"index":u.index,"script":u.script,
            "valueZat":u.value_zat,"height":u.height,"address":u.address,"coinbase":false})
    }
    #[test]
    fn off_refresh_retains_unverified_pir_facts_on_all_issued_addresses_and_cold() {
        let (mut w, _) = off_refresh_fixture();
        let acquisition = w.transparent_pir_checkpoint();
        assert_eq!(w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap(), 0);
        assert_eq!(w.utxos.len(), 2);
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "167000");
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 167000);
        assert_eq!(w.transparent_pir_checkpoint(), acquisition);
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.utxos.len(), 2);
        assert_eq!(cold.balance().transparent_pending, 167000);
        assert_eq!(cold.transparent_pir_checkpoint(), acquisition);
        let before = cold.to_snapshot().unwrap();
        assert!(cold
            .begin_transparent_pir(&"ac".repeat(32))
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_source_changed"));
        assert_eq!(cold.to_snapshot().unwrap(), before);
    }
    #[test]
    fn off_refresh_preserves_pir_reservations_and_allows_only_authoritative_retirement() {
        use zcash_client_backend::{
            data_api::OutputLockStore,
            wallet::{LockOwner, OutputRef},
        };
        let (mut w, c) = off_refresh_fixture();
        let owner = LockOwner::new([9; 32]);
        let output = OutputRef::new(
            zcash_primitives::transaction::TxId::from_bytes([1; 32]),
            zcash_protocol::PoolType::TRANSPARENT,
            0,
        );
        w.lock_outputs(&[output.clone()], owner, 100.into())
            .unwrap();
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        assert_eq!(w.utxos.len(), 2);
        // OutputLockStore is intentionally session-local; the durable signed
        // submission fixture separately covers cold unresolved-payment ownership.
        assert_eq!(
            w.active_lock_owner(&"01".repeat(32), "transparent", 0),
            Some("09".repeat(32).as_str())
        );
        let mut retirement = checkpoint(&mut w, vec![]);
        retirement.checkpoint_id = "de".repeat(32);
        retirement.previous_checkpoint_id = Some(c.checkpoint_id);
        retirement.rewind_to = Some(TransparentPirAnchor {
            height: 0,
            hash: Network::Regtest.genesis_hash().into(),
        });
        let before = w.to_snapshot().unwrap();
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&retirement).unwrap())
            .unwrap_err()
            .to_string()
            .contains("transparent_pir_pending_reconciliation_required"));
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert!(w.unlock_output(&output, owner).unwrap());
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        apply_pir(&mut cold, &retirement);
        assert!(cold.utxos.is_empty());
        assert_eq!(cold.balance().transparent_pending, 0);
        assert_eq!(cold.transparent_pir_status()["confirmedZat"], "0");
    }
    #[test]
    fn off_refresh_preserves_ordinary_replacement_listed_pir_coinbase_and_atomic_errors() {
        let (mut w, _) = off_refresh_fixture();
        let mut ordinary = off_refresh_listed(&w, 3);
        ordinary["txid"] = serde_json::json!("07".repeat(32));
        ordinary["valueZat"] = serde_json::json!(700);
        w.apply_utxos_json(&serde_json::json!({"utxos":[ordinary]}).to_string())
            .unwrap();
        assert_eq!(w.utxos.len(), 3);
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        assert_eq!(w.utxos.len(), 2);
        assert!(!w.utxos.iter().any(|u| u.txid == "07".repeat(32)));
        let row = off_refresh_listed(&w, 3);
        let before = w.to_snapshot().unwrap();
        assert!(w
            .apply_utxos_json(&serde_json::json!({"utxos":[row.clone(),{"index":0}]}).to_string())
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        w.apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(w.utxos.len(), 2);
        assert!(!w.pir_funding_unverified(&"03".repeat(32), 0));
        assert!(w.pir_funding_unverified(&"01".repeat(32), 0));
        assert_eq!(
            w.utxos
                .iter()
                .find(|u| u.txid == "03".repeat(32))
                .unwrap()
                .coinbase,
            CoinbaseStatus::Coinbase
        );
        assert_eq!(w.balance().transparent_available, 0);
        w.scanned_height = 105;
        assert_eq!(w.balance().transparent_available, 17000);
        assert_eq!(w.balance().transparent_pending, 150000);
        // Independently promoted rows use ordinary replacement semantics thereafter.
        w.apply_utxos_json(r#"{"utxos":[]}"#).unwrap();
        assert_eq!(w.utxos.len(), 1);
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 150000);
    }

    #[test]
    fn receive_then_spend_offline_projects_zero_and_real_history() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![spend(), receive()]);
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap();
        w.acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
            .unwrap();
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "0");
        assert_eq!(w.history(20).len(), 2);
        assert_eq!(w.txs[&"01".repeat(32)].transparent_received, 150000);
        assert_eq!(w.txs[&"02".repeat(32)].transparent_spent, 150000);
        let saved = w.to_snapshot().unwrap();
        let restored = WebWallet::from_snapshot(&saved).unwrap();
        assert_eq!(restored.transparent_pir_status()["confirmedZat"], "0");
        assert_eq!(restored.history(20).len(), 2);
    }
    #[test]
    fn transparent_pir_rejects_impossible_publisher_amount_atomically() {
        let mut w = wallet();
        let mut event = receive();
        if let TransparentEvent::Receive(e) = &mut event {
            e.value = u64::MAX;
        }
        let c = checkpoint(&mut w, vec![event]);
        let before = w.to_snapshot().unwrap();
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert!(w.utxos.is_empty());
    }
    #[test]
    fn late_conflict_rolls_back_and_old_generation_cannot_apply() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        let before = w.to_snapshot().unwrap();
        let mut bad = c.clone();
        bad.records.push(TransparentPirRecord {
            script: "00".into(),
            event: "00".into(),
            shard_id: 0,
            revision: "ef".repeat(32),
        });
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&bad).unwrap())
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert!(w.begin_transparent_pir(&"ff".repeat(32)).is_err());
        w.scanned_height = 4;
        w.block_hashes.insert(4, "04".repeat(32));
        w.begin_transparent_pir(&"ab".repeat(32)).unwrap();
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .is_err());
        assert!(w.utxos.is_empty());
    }
    #[test]
    fn known_coinbase_zero_conf_never_becomes_available() {
        let mut w = wallet();
        let script = w.public_transparent_scripts().unwrap()[0].0.clone();
        let row = serde_json::json!({
            "txid":"01".repeat(32),"index":0,"script":super::super::super::to_hex(&script),
            "valueZat":150000,"height":0,"coinbase":true
        });
        w.apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(w.utxos[0].coinbase, CoinbaseStatus::Coinbase);
        assert_eq!(w.balance().transparent_available, 0);
        assert_eq!(w.balance().transparent_pending, 150000);
        assert!(w.prove_shield(REGTEST_FAUCET_MNEMONIC, 1).is_err());
        assert!(w.pending_txs.is_empty());
        let cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.utxos[0].coinbase, CoinbaseStatus::Coinbase);
        assert_eq!(cold.balance().transparent_available, 0);
        assert_eq!(cold.balance().transparent_pending, 150000);
        w.scanned_height = 1000;
        assert_eq!(w.balance().transparent_available, 0);
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::tests::*;
    use super::*;
    #[test]
    fn transparent_pir_provisional_replacement_retires_only_projected_spend() {
        let mut w = wallet();
        w.block_hashes.insert(1, "01".repeat(32));
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap();
        w.acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
            .unwrap();
        let mut next = checkpoint(&mut w, vec![receive()]);
        next.checkpoint_id = "de".repeat(32);
        next.previous_checkpoint_id = Some(c.checkpoint_id);
        next.rewind_to = Some(TransparentPirAnchor {
            height: 1,
            hash: "01".repeat(32),
        });
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&next).unwrap())
            .unwrap();
        w.acknowledge_transparent_pir_checkpoint(&next.checkpoint_id)
            .unwrap();
        assert_eq!(w.transparent_pir_status()["confirmedZat"], "150000");
        assert_eq!(w.history(20).len(), 1);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.transparent_pir_status()["confirmedZat"], "150000");
        assert_eq!(restored.history(20).len(), 1);
    }
    #[test]
    fn transparent_pir_canonical_rewind_can_resume_partial_and_restore_received_balance() {
        let mut w = wallet();
        w.block_hashes.insert(1, "01".repeat(32));
        let c = checkpoint(&mut w, vec![receive(), spend()]);
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap();
        w.acknowledge_transparent_pir_checkpoint(&c.checkpoint_id)
            .unwrap();
        let saved = w.transparent_pir_checkpoint();
        w.rewind_to_height(1).unwrap();
        assert!(
            w.transparent_pir_checkpoint() == saved,
            "canonical rewind retains private acquisition"
        );
        assert!(
            w.begin_transparent_pir(&"ab".repeat(32)).is_err(),
            "lower target remains closed until scanning catches up"
        );
        let compact = crate::web::encode_delimited((2..=3).map(|height| {
            zcash_client_backend::proto::compact_formats::CompactBlock {
                height,
                hash: vec![height as u8 + 0x80; 32],
                prev_hash: vec![if height == 2 { 1 } else { 0x82 }; 32],
                time: 1,
                chain_metadata: Some(Default::default()),
                ..Default::default()
            }
        }));
        w.apply_compact_blocks_blob(&compact).unwrap();
        let mut partial = checkpoint(&mut w, vec![]);
        partial.checkpoint_id = "de".repeat(32);
        partial.previous_checkpoint_id = Some(c.checkpoint_id);
        partial.rewind_to = Some(TransparentPirAnchor {
            height: 1,
            hash: "01".repeat(32),
        });
        partial.covered_through = 1;
        for c in &mut partial.coverage {
            c.through = 1;
        }
        partial.complete = false;
        partial.pending_pages = 1;
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&partial).unwrap())
            .unwrap();
        let mut restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(
            restored.transparent_pir_status()["completion"],
            "incomplete"
        );
        let mut complete = checkpoint(&mut restored, vec![receive()]);
        complete.checkpoint_id = "ef".repeat(32);
        complete.previous_checkpoint_id = Some(partial.checkpoint_id);
        restored
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&complete).unwrap())
            .unwrap();
        restored
            .acknowledge_transparent_pir_checkpoint(&complete.checkpoint_id)
            .unwrap();
        assert_eq!(restored.transparent_pir_status()["confirmedZat"], "150000");
        assert_eq!(restored.history(20).len(), 1);
    }
    #[test]
    fn transparent_pir_retirement_cannot_remove_reserved_output_and_chain_rewind_invalidates() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        w.apply_transparent_pir_checkpoint(&serde_json::to_string(&c).unwrap())
            .unwrap();
        w.output_locks.push(StoredLock {
            txid: "01".repeat(32),
            pool: "transparent".into(),
            index: 0,
            owner: "ff".repeat(32),
            expiry: 100,
        });
        let mut next = checkpoint(&mut w, vec![]);
        next.checkpoint_id = "de".repeat(32);
        next.previous_checkpoint_id = Some(c.checkpoint_id);
        next.rewind_to = Some(TransparentPirAnchor {
            height: 0,
            hash: Network::Regtest.genesis_hash().into(),
        });
        let before = w.to_snapshot().unwrap();
        assert!(w
            .apply_transparent_pir_checkpoint(&serde_json::to_string(&next).unwrap())
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert_eq!(w.output_locks.len(), 1);
        w.block_hashes.insert(1, "01".repeat(32));
        w.rewind_to_height(1).unwrap();
        assert_ne!(
            w.transparent_pir_status()["completion"],
            "complete-for-enrolled-scope"
        );
    }
}
