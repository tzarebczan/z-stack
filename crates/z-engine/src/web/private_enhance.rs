//! Action-bound Ironwood suffix validation. Transport and query entropy stay outside storage.
use super::{from_hex, store::WebWallet, to_hex};
use crate::error::{EngineError, Result};
use incrementalmerkletree::Position;
use serde::{Deserialize, Serialize};
use zcash_client_backend::{
    data_api::enhance_pir::{
        storage::{
            validate_and_apply_records, EnhancePirStorage, IronwoodEnhancementData,
            IronwoodOutgoingResult, PendingIronwoodMemo, PendingIronwoodMetadata,
            PendingIronwoodOutgoing, StoredIronwoodMetadata, ValidatedIronwoodEnhancement,
        },
        EnhancePirBatchResult, EnhancePirRequest, EnhancePirStoreResult, EnhanceRecord,
        IronwoodEnhanceRequestId,
    },
    proto::compact_formats::CompactBlock,
};
use zcash_protocol::{memo::Memo, TxId};
use zip32::Scope;

fn refused(code: &str) -> EngineError {
    EngineError::Message(code.into())
}
fn bytes<const N: usize>(s: &str) -> Result<[u8; N]> {
    from_hex(s)
        .map_err(|_| refused("enhance_context_invalid"))?
        .try_into()
        .map_err(|_| refused("enhance_context_invalid"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactEnhanceAction {
    txid: String,
    index: u32,
    position: u64,
    height: u32,
    block_hash: String,
    nullifier: String,
    cmx: String,
    ephemeral_key: String,
    prefix: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    suspended: bool,
    #[serde(default)]
    recovered_outgoing_value: Option<u64>,
}

impl CompactEnhanceAction {
    pub(crate) fn height(&self) -> u32 {
        self.height
    }
}

/// Captured before I/O. The private fields prevent callers substituting action identities.
#[derive(Clone)]
pub struct PrivateEnhanceRequest {
    owner: String,
    action: CompactEnhanceAction,
}
impl PrivateEnhanceRequest {
    /// Used only by the Rust query client, never serialized to the host transport.
    pub fn position(&self) -> u64 {
        self.action.position
    }
}

/// Minimal contexts from a complete compact block; keep only wallet-associated transactions.
pub(crate) fn candidates(block: &CompactBlock) -> Vec<CompactEnhanceAction> {
    let Some(metadata) = block.chain_metadata.as_ref() else {
        return vec![];
    };
    let count: u64 = block
        .vtx
        .iter()
        .map(|tx| tx.ironwood_actions.len() as u64)
        .sum();
    let Some(mut position) = (metadata.ironwood_commitment_tree_size as u64).checked_sub(count)
    else {
        return vec![];
    };
    let mut out = Vec::new();
    for tx in &block.vtx {
        let eligible = zcash_client_backend::data_api::enhance_pir::is_ironwood_pir_candidate(tx);
        for (index, action) in tx.ironwood_actions.iter().enumerate() {
            if eligible
                && tx.txid.len() == 32
                && action.nullifier.len() == 32
                && action.cmx.len() == 32
                && action.ephemeral_key.len() == 32
                && action.ciphertext.len() == 52
            {
                out.push(CompactEnhanceAction {
                    txid: to_hex(&tx.txid),
                    index: index as u32,
                    position,
                    height: block.height as u32,
                    block_hash: to_hex(&block.hash),
                    nullifier: to_hex(&action.nullifier),
                    cmx: to_hex(&action.cmx),
                    ephemeral_key: to_hex(&action.ephemeral_key),
                    prefix: to_hex(&action.ciphertext),
                    done: false,
                    suspended: false,
                    recovered_outgoing_value: None,
                });
            }
            position += 1;
        }
    }
    out
}

impl WebWallet {
    pub(crate) fn retain_enhance_candidates(&mut self, actions: Vec<CompactEnhanceAction>) {
        for action in actions {
            if self
                .txs()
                .get(&action.txid)
                .is_none_or(|tx| tx.mined_height != Some(action.height))
            {
                continue;
            }
            if !self.private_enhance.iter().any(|known| {
                known.txid == action.txid
                    && known.index == action.index
                    && known.block_hash == action.block_hash
            }) {
                self.private_enhance.push(action);
            }
        }
    }
    pub fn private_enhance_work(&self) -> Result<Vec<PrivateEnhanceRequest>> {
        self.ensure_scan_healthy()?;
        let owner = self.decode_ufvk()?.encode(&self.network());
        Ok(self
            .private_enhance
            .iter()
            .filter(|a| {
                !a.done
                    && !a.suspended
                    && self
                        .txs()
                        .get(&a.txid)
                        .is_some_and(|tx| !tx.enhancement_complete)
            })
            .take(24)
            .cloned()
            .map(|action| PrivateEnhanceRequest {
                owner: owner.clone(),
                action,
            })
            .collect())
    }
    /// Rechecks the independently scanned historical anchor, including display-order hash.
    pub fn private_enhance_anchor(
        &self,
        height: u32,
        display_hash: &str,
        records: u64,
    ) -> Result<()> {
        self.ensure_scan_healthy()?;
        let hash = self
            .block_hashes
            .get(&height)
            .ok_or_else(|| refused("enhance_anchor_unavailable"))?;
        let mut wire = bytes::<32>(hash)?;
        wire.reverse();
        if to_hex(&wire) != display_hash || self.private_enhance_tree_size(height) != Some(records)
        {
            return Err(refused("enhance_anchor_mismatch"));
        }
        Ok(())
    }
    /// The shared validator authenticates the stitched ciphertext against the scanned note.
    /// One record is staged completely before any state changes; no public fallback is dispatched.
    pub fn apply_private_enhance_record(
        &mut self,
        request: PrivateEnhanceRequest,
        record: &[u8],
    ) -> Result<EnhancePirStoreResult> {
        self.ensure_scan_healthy()?;
        if request.owner != self.decode_ufvk()?.encode(&self.network()) {
            return Err(refused("enhance_wallet_mismatch"));
        }
        let Some(action) = self.private_enhance.iter().find(|a| **a == request.action) else {
            return Ok(EnhancePirStoreResult::AlreadyResolved);
        };
        if action.done
            || action.suspended
            || !self.txs().get(&action.txid).is_some_and(|tx| {
                tx.mined_height == Some(action.height) && !tx.enhancement_complete
            })
        {
            return Ok(EnhancePirStoreResult::AlreadyResolved);
        }
        let record = EnhanceRecord::from_bytes(
            record
                .try_into()
                .map_err(|_| refused("enhance_record_invalid"))?,
        )
        .map_err(|_| refused("enhance_record_invalid"))?;
        let captured = request.action;
        let query = EnhancePirRequest::new(
            captured.position.into(),
            IronwoodEnhanceRequestId::new(TxId::from_bytes(bytes(&captured.txid)?), captured.index),
        );
        let mut adapter = Apply {
            wallet: self,
            captured,
            staged: None,
        };
        let result = validate_and_apply_records(&mut adapter, &[(query, record)])?;
        let EnhancePirBatchResult::Committed(results) = result else {
            return Err(refused("enhance_record_rejected"));
        };
        if let Some(value) = adapter.staged.take() {
            adapter.commit(value)?;
        }
        Ok(results[0])
    }
}

impl WebWallet {
    pub(super) fn reconcile_full_ironwood_decryption(
        &mut self,
        txid: &str,
        recovered: &[(u32, Option<u64>)],
    ) {
        let incoming: Vec<_> = self
            .notes()
            .iter()
            .filter(|note| note.txid == txid && note.pool == "ironwood")
            .collect();
        let incoming_only_complete = !incoming.is_empty()
            && self.txs().get(txid).is_some_and(|tx| tx.spent_notes == 0)
            && !self
                .notes()
                .iter()
                .any(|note| note.spent_in.as_deref() == Some(txid))
            && incoming.iter().all(|note| {
                recovered
                    .iter()
                    .any(|(index, _)| *index == note.output_index)
            });
        // Resolve only indices actually authenticated by complete-transaction decryption.
        // Failed recoveries remain candidates; a visible change note cannot hide another output.
        for action in self
            .private_enhance
            .iter_mut()
            .filter(|action| action.txid == txid)
        {
            if let Some((_, outgoing)) = recovered.iter().find(|(index, _)| *index == action.index)
            {
                action.done = true;
                action.suspended = false;
                action.recovered_outgoing_value = *outgoing;
            }
        }
        self.reconcile_private_enhancement(txid);
        if incoming_only_complete {
            // Full raw authenticated every locally known incoming memo. Unrecovered foreign
            // actions do not require outgoing completion when this wallet spent no notes.
            // Leave their action bits untouched: this does not certify them as padding.
            if let Some(tx) = self.txs_mut().get_mut(txid) {
                tx.enhancement_complete = true;
            }
        }
    }
    pub(super) fn reconcile_private_enhancement(&mut self, txid: &str) {
        self.retire_value_balanced_padding(txid);
        let mut actions = self
            .private_enhance
            .iter()
            .filter(|a| a.txid == txid)
            .peekable();
        if actions.peek().is_some() {
            let complete = actions.all(|a| a.done);
            if let Some(tx) = self.txs_mut().get_mut(txid) {
                tx.enhancement_complete = complete;
            }
        }
    }
    fn retire_value_balanced_padding(&mut self, txid: &str) {
        // NotRecoverable alone cannot distinguish padding from an OVK-discarded payment.
        // Require full-transaction-derived fee metadata, never the fee asserted by PIR.
        let Some(tx) = self.txs().get(txid) else {
            return;
        };
        let Some(fee) = tx.ironwood_fee_from_full_transaction else {
            return;
        };
        let actions: Vec<_> = self
            .private_enhance
            .iter()
            .filter(|a| a.txid == txid)
            .collect();
        if actions.is_empty() || actions.iter().any(|a| !a.done && !a.suspended) {
            return;
        }
        let sum = |values: Vec<u64>| values.into_iter().try_fold(0u64, u64::checked_add);
        let spent: Vec<_> = self
            .notes()
            .iter()
            .filter(|n| n.pool == "ironwood" && n.spent_in.as_deref() == Some(txid))
            .map(|n| n.value_zat)
            .collect();
        if spent.is_empty() {
            return;
        }
        let incoming = sum(self
            .spend_notes
            .iter()
            .filter(|n| n.pool == "ironwood" && n.txid == txid)
            .map(|n| n.value_zat)
            .collect());
        let outgoing = sum(actions
            .iter()
            .filter_map(|a| a.recovered_outgoing_value)
            .collect());
        let recovered = incoming
            .zip(outgoing)
            .and_then(|(a, b)| a.checked_add(b))
            .and_then(|value| value.checked_add(fee));
        if sum(spent)
            .zip(recovered)
            .is_some_and(|(spent, recovered)| spent == recovered)
        {
            for action in self
                .private_enhance
                .iter_mut()
                .filter(|a| a.txid == txid && a.suspended)
            {
                action.done = true;
                action.suspended = false;
            }
        }
    }
}
struct Apply<'a> {
    wallet: &'a mut WebWallet,
    captured: CompactEnhanceAction,
    staged: Option<IronwoodEnhancementData<u32>>,
}
impl Apply<'_> {
    fn request(&self) -> Result<EnhancePirRequest> {
        Ok(EnhancePirRequest::new(
            self.captured.position.into(),
            IronwoodEnhanceRequestId::new(
                TxId::from_bytes(bytes(&self.captured.txid)?),
                self.captured.index,
            ),
        ))
    }
    fn incoming(&self) -> Result<Option<PendingIronwoodMemo<u32>>> {
        self.wallet
            .spend_notes
            .iter()
            .find(|n| {
                n.pool == "ironwood"
                    && n.txid == self.captured.txid
                    && n.output_index as u32 == self.captured.index
                    && n.position == self.captured.position
                    && n.mined_height == self.captured.height
            })
            .map(|n| {
                Ok(PendingIronwoodMemo {
                    request_id: self.request()?.request_id(),
                    account_id: 0,
                    note: WebWallet::decode_spend_note(n)?,
                    scope: if n.scope == 0 {
                        Scope::External
                    } else {
                        Scope::Internal
                    },
                    ephemeral_key: bytes(&self.captured.ephemeral_key)?,
                    compact_ciphertext: bytes(&self.captured.prefix)?,
                })
            })
            .transpose()
    }
    fn commit(&mut self, parts: IronwoodEnhancementData<u32>) -> Result<()> {
        if parts.has_transparent {
            return Err(refused("enhance_public_data_required"));
        }
        let recovered = parts.incoming.is_some()
            || matches!(&parts.outgoing, IronwoodOutgoingResult::Recovered { .. });
        let mut memos = Vec::new();
        if let Some(memo) = parts.incoming {
            if let Ok(Memo::Text(text)) = Memo::try_from(memo) {
                memos.push(text.to_string());
            }
        }
        let (outgoing, suspended) = match parts.outgoing {
            IronwoodOutgoingResult::Recovered {
                value,
                memo,
                recipient,
                ..
            } => {
                let fvk = self.wallet.decode_ufvk()?;
                let internal = fvk
                    .orchard()
                    .is_some_and(|key| key.scope_for_address(&recipient).is_some());
                if let Ok(Memo::Text(text)) = Memo::try_from(memo) {
                    memos.push(text.to_string());
                }
                ((!internal).then_some(u64::from(value)), false)
            }
            IronwoodOutgoingResult::NotRequested => (None, false),
            IronwoodOutgoingResult::NotRecoverable => (None, true),
        };
        // All fallible validation precedes this single in-memory commit.
        let index = self
            .wallet
            .private_enhance
            .iter()
            .position(|a| *a == self.captured)
            .ok_or_else(|| refused("enhance_context_stale"))?;
        let agg = self
            .wallet
            .txs_mut()
            .get_mut(&self.captured.txid)
            .ok_or_else(|| refused("enhance_context_stale"))?;
        agg.fee_zat = agg.fee_zat.or(parts.metadata.fee_zatoshis());
        // Never persist unauthenticated PIR expiry as authoritative spendability data.
        for memo in memos {
            if !memo.is_empty() && !agg.memos.contains(&memo) {
                agg.memos.push(memo);
            }
        }
        self.wallet.private_enhance[index].recovered_outgoing_value = outgoing;
        let outgoing_total = self
            .wallet
            .private_enhance
            .iter()
            .filter(|a| a.txid == self.captured.txid)
            .filter_map(|a| a.recovered_outgoing_value)
            .fold(0u64, u64::saturating_add);
        self.wallet
            .txs_mut()
            .get_mut(&self.captured.txid)
            .expect("validated transaction")
            .outgoing_shielded_zat = outgoing_total;
        self.wallet.private_enhance[index].done = !suspended;
        self.wallet.private_enhance[index].suspended = suspended;
        self.wallet
            .reconcile_private_enhancement(&self.captured.txid);
        let complete = self
            .wallet
            .private_enhance
            .iter()
            .filter(|a| a.txid == self.captured.txid)
            .all(|a| a.done);
        let agg = self
            .wallet
            .txs_mut()
            .get_mut(&self.captured.txid)
            .expect("validated transaction");
        agg.memo_recovered = Some(agg.memo_recovered == Some(true) || recovered);
        agg.enhancement_complete = complete;
        Ok(())
    }
}
impl EnhancePirStorage for Apply<'_> {
    type AccountId = u32;
    type Account = super::write::WebAccount;
    type Error = EngineError;
    fn ironwood_transaction_metadata(&self, txid: TxId) -> Result<Option<StoredIronwoodMetadata>> {
        Ok(self
            .wallet
            .txs()
            .get(&to_hex(txid.as_ref()))
            .map(|tx| StoredIronwoodMetadata {
                fee_zatoshis: tx.fee_zat,
                expiry_height: tx.expiry_height,
            }))
    }
    fn pending_ironwood_metadata(
        &self,
        _: Position,
    ) -> Result<Option<PendingIronwoodMetadata<u32>>> {
        Ok(Some(match self.incoming()? {
            Some(p) => PendingIronwoodMetadata::Incoming(p),
            None => PendingIronwoodMetadata::Compact(self.request()?.request_id()),
        }))
    }
    fn get_account(&self, id: u32) -> Result<Option<Self::Account>> {
        if id != 0 {
            return Ok(None);
        }
        self.wallet
            .account()
            .map(Some)
            .map_err(EngineError::Message)
    }
    fn pending_ironwood_memo(&self, _: Position) -> Result<Option<PendingIronwoodMemo<u32>>> {
        self.incoming()
    }
    fn pending_ironwood_outgoing(
        &self,
        _: Position,
    ) -> Result<Option<PendingIronwoodOutgoing<u32>>> {
        // Authenticated incoming change is complete on the incoming path. Its internal OVK
        // must not be confused with a failed external outgoing recovery.
        if self.incoming()?.is_some() {
            return Ok(None);
        }
        self.wallet
            .txs()
            .get(&self.captured.txid)
            .filter(|_| {
                self.wallet.notes().iter().any(|note| {
                    note.pool == "ironwood"
                        && note.spent_in.as_deref() == Some(self.captured.txid.as_str())
                })
            })
            .map(|_| {
                Ok(PendingIronwoodOutgoing {
                    request_id: self.request()?.request_id(),
                    account_ids: vec![0],
                    nullifier: bytes(&self.captured.nullifier)?,
                    cmx: bytes(&self.captured.cmx)?,
                    ephemeral_key: bytes(&self.captured.ephemeral_key)?,
                    compact_ciphertext: bytes(&self.captured.prefix)?,
                })
            })
            .transpose()
    }
    fn compare_and_apply_ironwood_enhancement(
        &mut self,
        value: ValidatedIronwoodEnhancement<u32>,
    ) -> Result<EnhancePirStoreResult> {
        let parts = value.into_parts();
        if parts.request != self.request()? {
            return Ok(EnhancePirStoreResult::AlreadyResolved);
        }
        if parts.has_transparent {
            return Err(refused("enhance_public_data_required"));
        }
        let current = self.ironwood_transaction_metadata(parts.request.request_id().txid())?;
        if parts.expected_metadata.is_none() || parts.expected_metadata != current {
            return Ok(EnhancePirStoreResult::Rejected);
        }
        let result = if matches!(parts.outgoing, IronwoodOutgoingResult::NotRecoverable) {
            EnhancePirStoreResult::NotRecoverable
        } else {
            EnhancePirStoreResult::Stored
        };
        self.staged = Some(parts);
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC},
        Network,
    };
    use orchard::{
        note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
        note_encryption::{IronwoodDomain, IronwoodNoteEncryption},
        value::NoteValue,
        Note,
    };
    use prost::Message;
    use zcash_client_backend::{
        data_api::enhance_pir::{EnhanceRecordParts, EnhanceTransactionMetadata},
        proto::compact_formats::{ChainMetadata, CompactOrchardAction, CompactTx},
    };
    use zcash_note_encryption::Domain;
    const HEIGHT: u32 = 1_000_001;
    fn wallet() -> WebWallet {
        WebWallet::from_account(
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
            HEIGHT,
        )
        .unwrap()
    }
    fn fixture(w: &WebWallet) -> (CompactBlock, EnhanceRecord) {
        let ufvk = w.decode_ufvk().unwrap();
        let fvk = ufvk.orchard().unwrap();
        let rho = Rho::from_bytes(&[1; 32]).unwrap();
        let note = Note::from_parts(
            fvk.address_at(0u32, Scope::External),
            NoteValue::from_raw(50000),
            rho,
            RandomSeed::from_bytes([7; 32], &rho).unwrap(),
            NoteVersion::V3,
        )
        .unwrap();
        let mut memo = [0; 512];
        memo[..13].copy_from_slice(b"private hello");
        let cmx = ExtractedNoteCommitment::from(note.commitment());
        let enc = IronwoodNoteEncryption::new(None, note, memo);
        let ciphertext = enc.encrypt_note_plaintext();
        let action = CompactOrchardAction {
            nullifier: vec![1; 32],
            cmx: cmx.to_bytes().to_vec(),
            ephemeral_key: IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
            ciphertext: ciphertext[..52].to_vec(),
        };
        let block = CompactBlock {
            height: HEIGHT.into(),
            hash: (0..32).collect(),
            prev_hash: vec![0; 32],
            time: 1,
            vtx: vec![CompactTx {
                txid: vec![3; 32],
                ironwood_actions: vec![action],
                ..Default::default()
            }],
            chain_metadata: Some(ChainMetadata {
                ironwood_commitment_tree_size: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let record = EnhanceRecord::from_parts(EnhanceRecordParts {
            enc_ciphertext_suffix: ciphertext[52..].try_into().unwrap(),
            cv_net: [0; 32],
            out_ciphertext: [0; 80],
            has_transparent_inputs: false,
            has_transparent_outputs: false,
            metadata: EnhanceTransactionMetadata::new(HEIGHT + 500, Some(10000)).unwrap(),
        });
        (block, record)
    }
    fn encrypted_action(
        recipient: orchard::Address,
        ovk: Option<orchard::keys::OutgoingViewingKey>,
        value: u64,
        nf: [u8; 32],
        fee: Option<u64>,
    ) -> (CompactOrchardAction, EnhanceRecord) {
        use orchard::value::{ValueCommitTrapdoor, ValueCommitment, ValueSum};
        let rho = Rho::from_bytes(&nf).unwrap();
        let note = Note::from_parts(
            recipient,
            NoteValue::from_raw(value),
            rho,
            RandomSeed::from_bytes([7; 32], &rho).unwrap(),
            NoteVersion::V3,
        )
        .unwrap();
        let mut memo = [0; 512];
        memo[..13].copy_from_slice(b"private hello");
        let enc = IronwoodNoteEncryption::new(ovk, note, memo);
        let ciphertext = enc.encrypt_note_plaintext();
        let cmx = ExtractedNoteCommitment::from(note.commitment());
        let mut trapdoor = [0; 32];
        trapdoor[0] = 1;
        let cv = ValueCommitment::derive(
            ValueSum::default(),
            ValueCommitTrapdoor::from_bytes(trapdoor).unwrap(),
        );
        let record = EnhanceRecord::from_parts(EnhanceRecordParts {
            enc_ciphertext_suffix: ciphertext[52..].try_into().unwrap(),
            cv_net: cv.to_bytes(),
            out_ciphertext: enc.encrypt_outgoing_plaintext(&cv, &cmx, &mut rand_08::thread_rng()),
            has_transparent_inputs: false,
            has_transparent_outputs: false,
            metadata: EnhanceTransactionMetadata::new(HEIGHT + 500, fee).unwrap(),
        });
        (
            CompactOrchardAction {
                nullifier: nf.to_vec(),
                cmx: cmx.to_bytes().to_vec(),
                ephemeral_key: IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
                ciphertext: ciphertext[..52].to_vec(),
            },
            record,
        )
    }
    #[cfg(feature = "native")]
    fn full_spend_bytes(
        w: &WebWallet,
        outputs: &[(CompactOrchardAction, EnhanceRecord)],
    ) -> Vec<u8> {
        full_spend_bytes_with_balance(w, outputs, 10000, false)
    }
    #[cfg(feature = "native")]
    fn full_spend_bytes_with_balance(
        w: &WebWallet,
        outputs: &[(CompactOrchardAction, EnhanceRecord)],
        balance: i64,
        transparent_input: bool,
    ) -> Vec<u8> {
        use orchard::{
            bundle::{Authorized as Auth, Bundle, BundleVersion},
            note::{Nullifier, TransmittedNoteCiphertext},
            value::ValueCommitment,
            Action, Anchor, Proof,
        };
        use zcash_primitives::transaction::{Authorized, Transaction, TransactionData};
        use zcash_protocol::{
            consensus::{BlockHeight, BranchId},
            value::ZatBalance,
        };
        let template =
            super::super::store::performance_tests::shielded_memo_transaction(w, w, "", 0, true);
        let template = Transaction::read(template.as_slice(), BranchId::Nu6_3).unwrap();
        let key = template.ironwood_bundle().unwrap().actions()[0].rk();
        let mut actions: Vec<_> = outputs
            .iter()
            .map(|(compact, record)| {
                let mut ciphertext = [0; 580];
                ciphertext[..52].copy_from_slice(&compact.ciphertext);
                ciphertext[52..].copy_from_slice(record.enc_ciphertext_suffix());
                Action::from_parts(
                    Nullifier::from_bytes(&compact.nullifier.clone().try_into().unwrap()).unwrap(),
                    key.clone(),
                    ExtractedNoteCommitment::from_bytes(&compact.cmx.clone().try_into().unwrap())
                        .unwrap(),
                    TransmittedNoteCiphertext {
                        epk_bytes: compact.ephemeral_key.clone().try_into().unwrap(),
                        enc_ciphertext: ciphertext,
                        out_ciphertext: *record.out_ciphertext(),
                    },
                    ValueCommitment::from_bytes(record.cv_net()).unwrap(),
                    [0; 64].into(),
                )
                .unwrap()
            })
            .collect();
        let version = BundleVersion::ironwood_v3();
        // Genuine encrypted outputs and linked nullifier, inert signatures/canonical proof.
        // The fixture exercises local storage/decryption, not consensus verification.
        let proof = Proof::new(vec![0; Proof::expected_proof_size(actions.len())]);
        let mut nonempty = template.ironwood_bundle().unwrap().actions().clone();
        nonempty.head = actions.remove(0);
        nonempty.tail = actions;
        let bundle = Bundle::try_from_parts(
            nonempty,
            version.default_flags(),
            ZatBalance::from_i64(balance).unwrap(),
            Anchor::empty_tree(),
            Auth::from_parts(proof, [0; 64].into()),
            version,
        )
        .unwrap();
        let tx = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            BlockHeight::from_u32(HEIGHT + 500),
            transparent_input.then(|| transparent::bundle::Bundle {
                vin: vec![transparent::bundle::TxIn::from_parts(
                    transparent::bundle::OutPoint::new([9; 32], 0),
                    transparent::address::Script::default(),
                    u32::MAX,
                )],
                vout: vec![],
                authorization: transparent::bundle::Authorized,
            }),
            None,
            None,
            Some(bundle),
        )
        .freeze()
        .unwrap();
        let mut raw = Vec::new();
        tx.write(&mut raw).unwrap();
        raw
    }
    fn spent_change_padding(
        fee: Option<u64>,
        discard_payment: bool,
    ) -> (WebWallet, Vec<EnhanceRecord>) {
        spent_change_padding_custom(fee, discard_payment, |_, _| vec![4; 32])
    }
    fn spent_change_padding_custom(
        fee: Option<u64>,
        discard_payment: bool,
        before_scan: impl FnOnce(&mut WebWallet, &[(CompactOrchardAction, EnhanceRecord)]) -> Vec<u8>,
    ) -> (WebWallet, Vec<EnhanceRecord>) {
        spent_change_padding_values(fee, discard_payment, [60000, 30000], before_scan)
    }
    fn spent_change_padding_values(
        fee: Option<u64>,
        discard_payment: bool,
        values: [u64; 2],
        before_scan: impl FnOnce(&mut WebWallet, &[(CompactOrchardAction, EnhanceRecord)]) -> Vec<u8>,
    ) -> (WebWallet, Vec<EnhanceRecord>) {
        let mut w = wallet();
        let fvk = w.decode_ufvk().unwrap().orchard().unwrap().clone();
        let stranger = account_from_mnemonic(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            Network::Regtest,
            0,
        )
        .unwrap();
        let foreign =
            zcash_keys::keys::UnifiedFullViewingKey::decode(&Network::Regtest, &stranger.ufvk)
                .unwrap()
                .orchard()
                .unwrap()
                .clone();
        let (funding, _) = encrypted_action(
            fvk.address_at(0u32, Scope::External),
            None,
            100000,
            [1; 32],
            None,
        );
        let (mut first, _) = fixture(&w);
        first.vtx[0].ironwood_actions = vec![funding];
        w.apply_compact_block(&first.encode_to_vec()).unwrap();
        let nf = bytes(&w.spend_notes[0].nf).unwrap();
        let outputs = [
            encrypted_action(
                foreign.address_at(0u32, Scope::External),
                (!discard_payment).then(|| fvk.to_ovk(Scope::External)),
                values[0],
                nf,
                fee,
            ),
            encrypted_action(
                fvk.address_at(0u32, Scope::Internal),
                Some(fvk.to_ovk(Scope::Internal)),
                values[1],
                [5; 32],
                fee,
            ),
            encrypted_action(
                foreign.address_at(1u32, Scope::External),
                None,
                0,
                [6; 32],
                fee,
            ),
        ];
        let mut second = first.clone();
        second.height += 1;
        second.prev_hash = first.hash;
        second.hash = vec![8; 32];
        second.vtx[0].txid = before_scan(&mut w, &outputs);
        let scanned_id = to_hex(&second.vtx[0].txid);
        second.vtx[0].ironwood_actions = outputs.iter().map(|(action, _)| action.clone()).collect();
        second
            .chain_metadata
            .as_mut()
            .unwrap()
            .ironwood_commitment_tree_size = 4;
        w.apply_compact_block(&second.encode_to_vec()).unwrap();
        assert!(w
            .notes()
            .iter()
            .any(|note| note.spent_in.as_deref() == Some(scanned_id.as_str())));
        (w, outputs.into_iter().map(|(_, record)| record).collect())
    }
    #[cfg(feature = "native")]
    fn caller_order_fixture(own_saved: bool) -> (WebWallet, Vec<EnhanceRecord>, Vec<u8>, String) {
        caller_order_fixture_with_discard(own_saved, false)
    }
    #[cfg(feature = "native")]
    fn caller_order_fixture_with_discard(
        own_saved: bool,
        discard: bool,
    ) -> (WebWallet, Vec<EnhanceRecord>, Vec<u8>, String) {
        use zcash_client_backend::data_api::{SentTransaction, WalletWrite};
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::{
            consensus::{BlockHeight, BranchId},
            value::Zatoshis,
        };
        let mut saved = None;
        let (w, records) = spent_change_padding_custom(Some(10000), discard, |w, outputs| {
            let raw = full_spend_bytes(w, outputs);
            let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
            let id = to_hex(tx.txid().as_ref());
            if own_saved {
                let sent = SentTransaction::new(
                    &tx,
                    std::time::UNIX_EPOCH.into(),
                    BlockHeight::from_u32(HEIGHT + 1).into(),
                    0,
                    &[],
                    Zatoshis::from_u64(10000).unwrap(),
                    #[cfg(feature = "transparent-inputs")]
                    &[],
                );
                w.store_transactions_to_be_sent(&[sent]).unwrap();
            }
            let wire = tx.txid().as_ref().to_vec();
            saved = Some((raw, id));
            wire
        });
        let (raw, id) = saved.unwrap();
        (w, records, raw, id)
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_mixed_or_mismatched_saved_fee_cannot_retire_hidden_positive() {
        use zcash_client_backend::data_api::{SentTransaction, WalletWrite};
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::{
            consensus::{BlockHeight, BranchId},
            value::Zatoshis,
        };
        let mut falsely_complete = Vec::new();
        for mixed in [false, true] {
            let mut id = String::new();
            let (mut w, records) = spent_change_padding_values(
                Some(if mixed { 10000 } else { 70000 }),
                true,
                if mixed {
                    [50000, 90000]
                } else {
                    [60000, 30000]
                },
                |w, outputs| {
                    // Mixed case: IW100k + transparent50k = change90k + hidden50k + fee10k.
                    // Other case deliberately supplies a caller fee70k for pure-IW fee10k.
                    let raw = full_spend_bytes_with_balance(
                        w,
                        outputs,
                        if mixed { -40000 } else { 10000 },
                        mixed,
                    );
                    let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
                    id = to_hex(tx.txid().as_ref());
                    let sent = SentTransaction::new(
                        &tx,
                        std::time::UNIX_EPOCH.into(),
                        BlockHeight::from_u32(HEIGHT + 1).into(),
                        0,
                        &[],
                        Zatoshis::from_u64(if mixed { 10000 } else { 70000 }).unwrap(),
                        #[cfg(feature = "transparent-inputs")]
                        &[],
                    );
                    w.store_transactions_to_be_sent(&[sent]).unwrap();
                    tx.txid().as_ref().to_vec()
                },
            );
            let work: Vec<_> = w
                .private_enhance_work()
                .unwrap()
                .into_iter()
                .filter(|r| r.action.txid == id)
                .collect();
            assert_eq!(work.len(), 3);
            assert_eq!(w.txs()[&id].ironwood_fee_from_full_transaction, None);
            for (request, record) in work.into_iter().zip(records) {
                let before = w.to_snapshot().unwrap();
                if w.apply_private_enhance_record(request, record.as_bytes())
                    .is_err()
                {
                    // The forged pure-IW fee also conflicts with the canonical stored fee.
                    assert!(!mixed);
                    assert_eq!(w.to_snapshot().unwrap(), before);
                }
            }
            let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
            if w.txs()[&id].enhancement_complete || restored.txs()[&id].enhancement_complete {
                falsely_complete.push(mixed);
            } else {
                assert!(
                    w.private_enhance
                        .iter()
                        .filter(|a| a.txid == id && !a.done)
                        .count()
                        >= 2
                );
                assert_eq!(restored.txs()[&id].ironwood_fee_from_full_transaction, None);
            }
        }
        assert!(
            falsely_complete.is_empty(),
            "falsely complete mixed={falsely_complete:?}"
        );
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_incoming_full_raw_before_pir_stays_complete() {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::BranchId;
        let mut w = wallet();
        let fvk = w.decode_ufvk().unwrap().orchard().unwrap().clone();
        let output = encrypted_action(
            fvk.address_at(0u32, Scope::External),
            None,
            50000,
            [1; 32],
            Some(10000),
        );
        let raw = full_spend_bytes(&w, std::slice::from_ref(&output));
        let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
        let id = to_hex(tx.txid().as_ref());
        let (mut block, _) = fixture(&w);
        block.vtx[0].txid = tx.txid().as_ref().to_vec();
        block.vtx[0].ironwood_actions = vec![output.0];
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        assert_eq!(w.private_enhance_work().unwrap().len(), 1);
        w.enhance_raw_tx(&raw).unwrap();
        assert_eq!(w.txs()[&id].memos, ["private hello"]);
        assert!(w.txs()[&id].enhancement_complete);
        assert!(w.private_enhance_work().unwrap().is_empty());
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.txs()[&id].enhancement_complete);
        assert!(!restored
            .memo_enhancement_txids(10)
            .contains(&WebWallet::display_txid(&id)));
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_incoming_full_raw_with_unrecoverable_padding_stays_complete() {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::BranchId;
        let mut w = wallet();
        let fvk = w.decode_ufvk().unwrap().orchard().unwrap().clone();
        let stranger = account_from_mnemonic(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            Network::Regtest,
            0,
        )
        .unwrap();
        let foreign =
            zcash_keys::keys::UnifiedFullViewingKey::decode(&Network::Regtest, &stranger.ufvk)
                .unwrap();
        let outputs = [
            encrypted_action(
                fvk.address_at(0u32, Scope::External),
                None,
                50000,
                [1; 32],
                Some(10000),
            ),
            encrypted_action(
                foreign.orchard().unwrap().address_at(0u32, Scope::External),
                None,
                0,
                [2; 32],
                Some(10000),
            ),
        ];
        let raw = full_spend_bytes(&w, &outputs);
        let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
        let id = to_hex(tx.txid().as_ref());
        let (mut block, _) = fixture(&w);
        block.vtx[0].txid = tx.txid().as_ref().to_vec();
        block.vtx[0].ironwood_actions =
            outputs.iter().map(|(compact, _)| compact.clone()).collect();
        block
            .chain_metadata
            .as_mut()
            .unwrap()
            .ironwood_commitment_tree_size = 2;
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        assert_eq!(w.private_enhance_work().unwrap().len(), 2);
        w.enhance_raw_tx(&raw).unwrap();
        assert_eq!(w.txs()[&id].memos, ["private hello"]);
        assert!(w.txs()[&id].enhancement_complete);
        assert!(w
            .private_enhance
            .iter()
            .any(|action| action.txid == id && !action.done));
        assert!(w.private_enhance_work().unwrap().is_empty());
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.txs()[&id].enhancement_complete);
        assert!(!restored
            .memo_enhancement_txids(10)
            .contains(&WebWallet::display_txid(&id)));
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_full_raw_before_pir_keeps_hidden_positive_unresolved() {
        let (mut w, records, raw, id) = caller_order_fixture_with_discard(false, true);
        w.enhance_raw_tx(&raw).unwrap();
        assert!(!w.txs()[&id].enhancement_complete);
        let work: Vec<_> = w
            .private_enhance_work()
            .unwrap()
            .into_iter()
            .filter(|request| request.action.txid == id)
            .collect();
        assert_eq!(
            work.len(),
            2,
            "only the authenticated change action was resolved"
        );
        for request in work {
            let record = &records[request.action.index as usize];
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap();
        }
        assert!(!w.txs()[&id].enhancement_complete);
        assert_eq!(
            w.private_enhance
                .iter()
                .filter(|a| a.txid == id && a.suspended && !a.done)
                .count(),
            2
        );
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(!restored.txs()[&id].enhancement_complete);
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_full_raw_after_suspended_padding_retires_it() {
        let (mut w, records, raw, id) = caller_order_fixture(false);
        let work: Vec<_> = w
            .private_enhance_work()
            .unwrap()
            .into_iter()
            .filter(|r| r.action.txid == id)
            .collect();
        for (request, record) in work.into_iter().zip(records) {
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap();
        }
        assert!(w
            .private_enhance
            .iter()
            .any(|a| a.txid == id && a.suspended && !a.done));
        assert!(!w.txs()[&id].fee_from_full_transaction);
        w.enhance_raw_tx(&raw).unwrap();
        assert!(w.txs()[&id].fee_from_full_transaction);
        assert!(w
            .private_enhance
            .iter()
            .filter(|a| a.txid == id)
            .all(|a| a.done && !a.suspended));
        assert!(w.txs()[&id].enhancement_complete);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored
            .private_enhance
            .iter()
            .filter(|a| a.txid == id)
            .all(|a| a.done && !a.suspended));
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_own_saved_transaction_fee_is_available_before_pir_apply() {
        let (mut w, records, raw, id) = caller_order_fixture(true);
        assert_eq!(
            w.pending_txs.get(&WebWallet::txid_from_hex(&id).unwrap()),
            Some(&raw)
        );
        assert!(w.txs()[&id].fee_from_full_transaction);
        let work: Vec<_> = w
            .private_enhance_work()
            .unwrap()
            .into_iter()
            .filter(|r| r.action.txid == id)
            .collect();
        for (request, record) in work.into_iter().zip(records) {
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap();
        }
        assert!(w
            .private_enhance
            .iter()
            .filter(|a| a.txid == id)
            .all(|a| a.done && !a.suspended));
        assert!(w.txs()[&id].enhancement_complete);
    }
    #[cfg(feature = "native")]
    #[test]
    fn caller_order_full_raw_cannot_hide_positive_discarded_payment_as_padding() {
        let (mut w, records, raw, id) = caller_order_fixture_with_discard(false, true);
        let work: Vec<_> = w
            .private_enhance_work()
            .unwrap()
            .into_iter()
            .filter(|r| r.action.txid == id)
            .collect();
        for (request, record) in work.into_iter().zip(records) {
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap();
        }
        w.enhance_raw_tx(&raw).unwrap();
        assert!(w.txs()[&id].fee_from_full_transaction);
        assert_eq!(
            w.private_enhance
                .iter()
                .filter(|a| a.txid == id && a.suspended && !a.done)
                .count(),
            2
        );
        assert!(!w.txs()[&id].enhancement_complete);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(!restored.txs()[&id].enhancement_complete);
        assert_eq!(
            restored
                .private_enhance
                .iter()
                .filter(|a| a.txid == id && a.suspended && !a.done)
                .count(),
            2
        );
    }
    #[cfg(feature = "native")]
    #[test]
    fn genuine_spend_change_and_padding_complete_only_after_recovered_value_balance() {
        let (mut w, records, _, id) = caller_order_fixture(true);
        let work: Vec<_> = w
            .private_enhance_work()
            .unwrap()
            .into_iter()
            .filter(|r| r.action.txid == id)
            .collect();
        assert_eq!(work.len(), 3);
        assert_eq!(
            w.apply_private_enhance_record(work[2].clone(), records[2].as_bytes())
                .unwrap(),
            EnhancePirStoreResult::NotRecoverable
        );
        assert!(!w.txs()[&id].enhancement_complete);
        assert_eq!(
            w.apply_private_enhance_record(work[1].clone(), records[1].as_bytes())
                .unwrap(),
            EnhancePirStoreResult::Stored
        );
        assert!(!w.txs()[&id].enhancement_complete);
        assert_eq!(
            w.apply_private_enhance_record(work[0].clone(), records[0].as_bytes())
                .unwrap(),
            EnhancePirStoreResult::Stored
        );
        let tx = &w.txs()[&id];
        assert!(tx.enhancement_complete);
        assert_eq!(tx.outgoing_shielded_zat, 60000);
        assert_eq!(tx.memo_recovered, Some(true));
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.txs()[&id].enhancement_complete);
    }
    #[test]
    fn missing_fee_wrong_balance_and_discarded_payment_remain_unresolved() {
        for (fee, discard) in [
            (None, false),
            (Some(9000), false),
            (Some(10000), false),
            (Some(10000), true),
        ] {
            let (mut w, records) = spent_change_padding(fee, discard);
            let work: Vec<_> = w
                .private_enhance_work()
                .unwrap()
                .into_iter()
                .filter(|r| r.action.txid == "04".repeat(32))
                .collect();
            for (request, record) in work.into_iter().zip(records) {
                if fee.is_none() {
                    let before = w.to_snapshot().unwrap();
                    assert!(w
                        .apply_private_enhance_record(request, record.as_bytes())
                        .is_err());
                    assert_eq!(w.to_snapshot().unwrap(), before);
                } else {
                    w.apply_private_enhance_record(request, record.as_bytes())
                        .unwrap();
                }
            }
            assert!(!w.txs()[&"04".repeat(32)].enhancement_complete);
            if fee.is_none() {
                continue;
            }
            assert!(w
                .private_enhance
                .iter()
                .any(|a| a.txid == "04".repeat(32) && a.suspended && !a.done));
        }
    }
    #[cfg(feature = "native")]
    #[test]
    fn fee_provenance_requires_complete_raw_transaction_and_survives_snapshot() {
        use zcash_primitives::transaction::Transaction;
        use zcash_protocol::consensus::BranchId;
        let mut w = wallet();
        // Genuine encrypted zero-value note; inert consensus proof/signature fixture.
        // Its complete serialized value balance determines fee zero without PIR metadata.
        let raw = super::super::store::performance_tests::shielded_memo_transaction(
            &w,
            &w,
            "fee proof",
            0,
            true,
        );
        let tx = Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap();
        w.enhance_raw_tx(&raw).unwrap();
        let agg = &w.txs()[&to_hex(tx.txid().as_ref())];
        assert_eq!(agg.fee_zat, Some(0));
        assert!(agg.fee_from_full_transaction);
        assert_eq!(agg.ironwood_fee_from_full_transaction, Some(0));
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.txs()[&to_hex(tx.txid().as_ref())].fee_from_full_transaction);
        assert_eq!(
            restored.txs()[&to_hex(tx.txid().as_ref())].ironwood_fee_from_full_transaction,
            Some(0)
        );
        w.reset_scan();
        assert!(w.txs().is_empty());
    }
    #[test]
    fn authentic_suffix_survives_snapshot_and_preserves_untrusted_expiry() {
        let mut w = wallet();
        let (block, record) = fixture(&w);
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        let snapshot = w.to_snapshot().unwrap();
        let mut w = WebWallet::from_snapshot(&snapshot).unwrap();
        let mut work = w.private_enhance_work().unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].position(), 0);
        let request = work.pop().unwrap();
        let repeat = request.clone();
        assert_eq!(
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap(),
            EnhancePirStoreResult::Stored
        );
        let tx = w.txs().get(&"03".repeat(32)).unwrap();
        assert_eq!(tx.memos, vec!["private hello"]);
        assert_eq!(tx.fee_zat, Some(10000));
        assert!(
            !tx.fee_from_full_transaction,
            "PIR metadata cannot grant fee provenance"
        );
        assert_eq!(tx.expiry_height, None);
        assert!(tx.enhancement_complete);
        assert!(!tx.history_metadata_complete);
        assert!(w.private_enhance_work().unwrap().is_empty());
        assert_eq!(
            w.apply_private_enhance_record(repeat, record.as_bytes())
                .unwrap(),
            EnhancePirStoreResult::AlreadyResolved
        );
    }
    #[test]
    fn corrupt_suffix_and_transparent_flag_preserve_every_wallet_byte() {
        let mut w = wallet();
        let (block, record) = fixture(&w);
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        let request = w.private_enhance_work().unwrap().pop().unwrap();
        let before = w.to_snapshot().unwrap();
        let mut corrupt = *record.as_bytes();
        corrupt[7] ^= 1;
        assert!(w
            .apply_private_enhance_record(request.clone(), &corrupt)
            .is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
        let mut transparent = *record.as_bytes();
        transparent[640] |= 1;
        assert!(w
            .apply_private_enhance_record(request.clone(), &transparent)
            .unwrap_err()
            .to_string()
            .contains("enhance_public_data_required"));
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert!(w.apply_private_enhance_record(request, &[0; 652]).is_err());
        assert_eq!(w.to_snapshot().unwrap(), before);
    }
    #[test]
    fn historical_anchor_uses_display_hash_and_stale_action_cannot_apply() {
        let mut w = wallet();
        let (block, record) = fixture(&w);
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        let mut hash = block.hash.clone();
        hash.reverse();
        w.private_enhance_anchor(HEIGHT, &to_hex(&hash), 1).unwrap();
        assert!(w
            .private_enhance_anchor(HEIGHT, &to_hex(&block.hash), 1)
            .is_err());
        assert!(w.private_enhance_anchor(HEIGHT, &to_hex(&hash), 2).is_err());
        let request = w.private_enhance_work().unwrap().pop().unwrap();
        w.rewind_to_height(HEIGHT - 1).unwrap();
        let mut replacement = block;
        replacement.hash[0] ^= 128;
        replacement.vtx[0].txid[0] ^= 128;
        w.apply_compact_block(&replacement.encode_to_vec()).unwrap();
        let before = w.to_snapshot().unwrap();
        assert_eq!(
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap(),
            EnhancePirStoreResult::AlreadyResolved
        );
        assert_eq!(w.to_snapshot().unwrap(), before);
        assert_eq!(w.private_enhance_work().unwrap().len(), 1);
    }
    #[test]
    fn batched_scanning_retains_same_context_as_single_and_refuses_cross_wallet() {
        let mut one = wallet();
        let (block, record) = fixture(&one);
        let mut batch = wallet();
        one.apply_compact_block(&block.encode_to_vec()).unwrap();
        batch
            .apply_compact_blocks_blob(&super::super::encode_delimited([block]))
            .unwrap();
        assert_eq!(one.private_enhance, batch.private_enhance);
        let request = one.private_enhance_work().unwrap().pop().unwrap();
        let mut stranger = WebWallet::from_account(
            account_from_mnemonic(
                "legal winner thank year wave sausage worth useful legal winner thank yellow",
                Network::Regtest,
                0,
            )
            .unwrap(),
            HEIGHT,
        )
        .unwrap();
        assert!(stranger
            .apply_private_enhance_record(request, record.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("enhance_wallet_mismatch"));
    }
    #[test]
    fn reset_drops_pending_action_context_and_old_reply_is_inert() {
        let mut w = wallet();
        let (block, record) = fixture(&w);
        w.apply_compact_block(&block.encode_to_vec()).unwrap();
        let request = w.private_enhance_work().unwrap().pop().unwrap();
        w.reset_scan();
        let before = w.to_snapshot().unwrap();
        assert!(w.private_enhance_work().unwrap().is_empty());
        assert_eq!(
            w.apply_private_enhance_record(request, record.as_bytes())
                .unwrap(),
            EnhancePirStoreResult::AlreadyResolved
        );
        assert_eq!(w.to_snapshot().unwrap(), before);
    }
}
