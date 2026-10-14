use super::*;
use crate::transparent_pir::{
    parse, refused, TransparentPirAnchor, TransparentPirCheckpoint, TransparentPirScript,
};
use transparent_events::TransparentEvent;

impl WebWallet {
    /// The caller has accepted this exact transparent publisher profile. All scripts
    /// and the target are obtained from this wallet; no host-provided amount is used.
    pub fn begin_transparent_pir(&mut self, source_id: &str) -> Result<String> {
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
        self.transparent_pir.wallet_cursor = Some(self.next_diversifier);
        serde_json::to_string(&scope).map_err(|_| refused("transparent_pir_scope_invalid"))
    }

    pub fn transparent_pir_checkpoint(&self) -> Option<String> {
        self.transparent_pir
            .continuation
            .as_ref()
            .and_then(|c| serde_json::to_string(c).ok())
    }

    pub fn transparent_pir_status(&self) -> serde_json::Value {
        let mut state = self.transparent_pir.clone();
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
                }
            }
            for id in &state.projected_transactions {
                if !self.pending_txs.contains_key(&Self::txid_from_hex(id)?) {
                    if let Some(agg) = txs.get_mut(id) {
                        if agg.mined_height.is_some_and(|h| h > floor)
                            && !agg.has_shielded_activity()
                        {
                            agg.mined_height = None;
                            agg.transparent_received = 0;
                            agg.transparent_spent = 0;
                            agg.received_zat = 0;
                            agg.spent_zat = 0;
                        }
                    }
                }
            }
            txs.retain(|id, agg| {
                !state.projected_transactions.contains(id)
                    || agg.mined_height.is_some()
                    || agg.has_shielded_activity()
                    || Self::txid_from_hex(id)
                        .ok()
                        .is_some_and(|id| self.pending_txs.contains_key(&id))
            });
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
                        u.height = e.height;
                        u.coinbase = e.coinbase;
                    } else {
                        state
                            .projected_outputs
                            .insert(crate::transparent_pir::event_key(*event));
                        utxos.push(StoredUtxo {
                            txid: txid.clone(),
                            index: e.output_index,
                            script: super::super::to_hex(script),
                            value_zat: e.value,
                            height: e.height,
                            address: address.clone(),
                            coinbase: e.coinbase,
                            spent: false,
                            spent_in: None,
                        });
                    }
                    if !txs.contains_key(&txid) {
                        state.projected_transactions.insert(txid.clone());
                    }
                    let agg = txs.entry(txid).or_default();
                    if agg.mined_height.is_some_and(|h| h != e.height) {
                        return Err(refused("transparent_pir_transaction_conflict"));
                    }
                    agg.mined_height = Some(e.height);
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
        TransparentPirCheckpoint {
            source_id: scope.source_id,
            generation: scope.generation,
            scope_id: scope.scope_id,
            anchor: scope.anchor,
            checkpoint_id: "cd".repeat(32),
            previous_checkpoint_id: None,
            covered_through: 3,
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
                    through: 3,
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
        w.rewind_to_height(1).unwrap();
        let mut partial = checkpoint(&mut w, vec![]);
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
        complete.covered_through = 1;
        for c in &mut complete.coverage {
            c.through = 1;
        }
        complete.checkpoint_id = "de".repeat(32);
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
