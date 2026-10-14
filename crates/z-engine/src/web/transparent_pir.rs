use super::*;
use crate::transparent_pir::{
    parse, refused, TransparentPirAnchor, TransparentPirCheckpoint, TransparentPirScript,
};
use transparent_events::TransparentEvent;

impl WebWallet {
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

    fn transparent_pir_rescan_required(&self) -> bool {
        self.transparent_pir.unverified_outputs.is_none()
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
    fn independent_address_refresh_promotes_pir_receive_without_releasing_pending_spend() {
        let mut w = wallet();
        let c = checkpoint(&mut w, vec![receive()]);
        apply_pir(&mut w, &c);
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
    fn confirmed_wallet_write_promotes_pir_receive_but_unmined_output_does_not() {
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
            assert_eq!(
                w.balance().transparent_available,
                if mined.is_some() { 150000 } else { 0 }
            );
        }
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
            assert_eq!(w.utxos[0].coinbase, coinbase);
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
        restored
            .apply_utxos_json(&serde_json::json!({"utxos":[row]}).to_string())
            .unwrap();
        assert_eq!(restored.balance().transparent_available, 40000);
        assert_eq!(restored.utxos[0].value_zat, 40000);
    }

    #[test]
    fn ordinary_zero_conf_funding_without_pir_is_unchanged() {
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
        assert_eq!(w.balance().transparent_available, 150000);
        let mut cold = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(cold.balance().transparent_available, 150000);
        assert_eq!(cold.transparent_pir_status()["rescanRequired"], false);
        let c = checkpoint(&mut cold, vec![]);
        apply_pir(&mut cold, &c);
        assert_eq!(cold.balance().transparent_available, 150000);
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
