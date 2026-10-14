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
            if !self
                .txs()
                .get(&action.txid)
                .is_some_and(|tx| tx.mined_height == Some(action.height))
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
            .filter(|a| !a.done && !a.suspended)
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
            || !self
                .txs()
                .get(&action.txid)
                .is_some_and(|tx| tx.mined_height == Some(action.height))
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
        if let Some(value) = outgoing {
            agg.outgoing_shielded_zat = agg.outgoing_shielded_zat.saturating_add(value);
        }
        self.wallet.private_enhance[index].done = !suspended;
        self.wallet.private_enhance[index].suspended = suspended;
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
        Ok(self
            .wallet
            .txs()
            .get(&self.captured.txid)
            .filter(|_| {
                self.wallet.notes().iter().any(|note| {
                    note.pool == "ironwood"
                        && note.spent_in.as_deref() == Some(self.captured.txid.as_str())
                })
            })
            .map(|_| {
                Ok::<_, EngineError>(PendingIronwoodOutgoing {
                    request_id: self.request()?.request_id(),
                    account_ids: vec![0],
                    nullifier: bytes(&self.captured.nullifier)?,
                    cmx: bytes(&self.captured.cmx)?,
                    ephemeral_key: bytes(&self.captured.ephemeral_key)?,
                    compact_ciphertext: bytes(&self.captured.prefix)?,
                })
            })
            .transpose()?)
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
