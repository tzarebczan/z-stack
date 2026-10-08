//! Hardware signers (Keystone, Ledger) through PCZTs.
//!
//! The wallet builds the PCZT for a send from the same proposal a seed send
//! uses, proves it here (the device never proves), hands the device a
//! redacted copy to sign, then applies the returned signatures, finalizes,
//! extracts and records the transaction. Spending keys never leave the device.
//!
//! Roles, following ZIP 374: Creator/Constructor/IO Finalizer
//! (`create_pczt_from_proposal`), Prover (here), Signer (device), Combiner,
//! Spend Finalizer and Transaction Extractor (here).

use super::prove::map_funds;
use super::write::ACCOUNT;
use super::WebWallet;
use crate::error::{EngineError, Result};
use orchard::circuit::{OrchardCircuitVersion, VerifyingKey};
use pczt::roles::{
    prover::Prover,
    signer::{extract_orchard_spend_auth_signatures, Signer, SpendAuthSignature},
};
use pczt::Pczt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::OnceLock;
use zcash_client_backend::data_api::locking::LockOwner;
use zcash_client_backend::data_api::wallet::{
    create_pczt_from_proposal, extract_and_store_transaction_from_pczt,
    redact_pczt_for_batch_signer, redact_pczt_for_signer, SignerView,
};
use zcash_client_backend::data_api::OutputLockStore;
use zcash_client_backend::wallet::{OutputRef, OvkPolicy};
use zcash_primitives::transaction::builder::{cached_orchard_proving_key, BundlePadding};
use zcash_protocol::consensus::BranchId;

/// Lock owner for notes reserved by in-flight hardware PCZTs.
const HARDWARE_LOCK_OWNER: [u8; 32] = *b"z-stack hardware pczt lock owner";

/// Drop only the locks this attempt added. Other in-flight PCZTs keep theirs.
fn release_locks_taken_since(wallet: &mut WebWallet, before: &[OutputRef]) {
    let Ok(after) = wallet.get_locked_outputs(ACCOUNT) else {
        return;
    };
    for output in after {
        let kept = before.iter().any(|prior| {
            prior.txid() == output.txid()
                && prior.pool() == output.pool()
                && prior.output_index() == output.output_index()
        });
        if kept {
            continue;
        }
        let _ = wallet.unlock_output(&output, LockOwner::new(HARDWARE_LOCK_OWNER));
    }
}

/// A spend authorization signature from a device, for one Orchard or Ironwood action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSignature {
    /// "orchard" or "ironwood".
    pub pool: String,
    pub action_index: usize,
    /// 64 bytes, hex.
    pub signature: String,
}

/// What `hardware_finalize` returns.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalizedTransaction {
    pub txid: String,
    pub hex: String,
}

/// Which redaction to hand the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerCopy {
    /// Everything a general-purpose signer may need (Keystone full-PCZT flow, Ledger).
    Full,
    /// The compact view (fields the signer can recompute are dropped).
    Compact,
    /// Keystone batch signing: signatures only, no FVK, no proprietary fields.
    Batch,
}

impl SignerCopy {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "compact" => Ok(Self::Compact),
            "batch" => Ok(Self::Batch),
            other => Err(EngineError::Message(format!(
                "unknown signer copy \"{other}\" (full, compact, batch)"
            ))),
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Pczt> {
    Pczt::parse(bytes).map_err(|e| EngineError::Message(format!("PCZT: {e:?}")))
}

fn serialize(pczt: Pczt) -> Result<Vec<u8>> {
    pczt.serialize()
        .map_err(|e| EngineError::Message(format!("PCZT encode: {e:?}")))
}

/// The Orchard circuit a PCZT's proofs use is keyed on the consensus branch
/// (ZIP 229), not the transaction version: a post-NU6.3 v5 transaction still
/// needs the post-NU6.3 circuit.
fn circuit_for_branch(branch: u32) -> OrchardCircuitVersion {
    if matches!(
        BranchId::try_from(branch),
        Ok(BranchId::Nu6_3 | BranchId::Nu7)
    ) {
        OrchardCircuitVersion::PostNu6_3
    } else {
        OrchardCircuitVersion::FixedPostNu6_2
    }
}

/// Verifying keys take seconds to build; build each once per process.
fn verifying_key(version: OrchardCircuitVersion) -> &'static VerifyingKey {
    static V2: OnceLock<VerifyingKey> = OnceLock::new();
    static V3: OnceLock<VerifyingKey> = OnceLock::new();
    match version {
        OrchardCircuitVersion::PostNu6_3 => V3.get_or_init(|| VerifyingKey::build(version)),
        _ => V2.get_or_init(|| VerifyingKey::build(version)),
    }
}

fn from_hex_sig(hex: &str) -> Result<[u8; 64]> {
    let bytes =
        super::from_hex(hex.trim()).map_err(|e| EngineError::Message(format!("signature: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| EngineError::Message("signature must be 64 bytes".into()))
}

impl WebWallet {
    fn require_hardware(&self) -> Result<()> {
        if self.hardware.is_none() {
            return Err(EngineError::Message(
                "this wallet is not a hardware-wallet account (use the seed send path)".into(),
            ));
        }
        Ok(())
    }

    /// PCZT for a send to a unified address or ZIP-321 URI, before proofs and
    /// signatures. The inputs it spends are locked until it is finalized or
    /// expires. Only Orchard and Ironwood notes are spent (both devices sign
    /// those; neither signs Sapling in a PCZT).
    pub fn hardware_create_send(
        &mut self,
        to: &str,
        amount_zec: &str,
        memo: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.require_hardware()?;
        let request = self.send_request(to, amount_zec, memo)?;
        let recipients: Vec<String> = request
            .payments()
            .values()
            .map(|p| p.recipient_address().to_string())
            .collect();
        // Reserve the notes until the transaction would expire: signing on a
        // device takes minutes, and a second send must not pick them again.
        let lock = zcash_client_backend::data_api::wallet::LockRequest::new(
            zcash_client_backend::data_api::locking::LockOwner::new(HARDWARE_LOCK_OWNER),
            zcash_primitives::transaction::builder::DEFAULT_TX_EXPIRY_DELTA,
        );
        let before = self
            .get_locked_outputs(ACCOUNT)
            .map_err(|e| map_funds(format!("note locks: {e}")))?;
        let proposal = match self.propose_send(request, Some(lock)) {
            Ok(proposal) => proposal,
            Err(e) => {
                release_locks_taken_since(self, &before);
                return Err(e);
            }
        };
        let pczt = match create_pczt_from_proposal::<_, _, Infallible, _, Infallible, _>(
            self,
            &self.network(),
            ACCOUNT,
            OvkPolicy::Sender,
            &proposal,
            None,
            BundlePadding::DEFAULT,
        ) {
            Ok(pczt) => pczt,
            Err(e) => {
                release_locks_taken_since(self, &before);
                return Err(map_funds(format!("create PCZT: {e}")));
            }
        };
        for r in recipients {
            self.record_recipient(&r);
        }
        match serialize(pczt) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                release_locks_taken_since(self, &before);
                Err(e)
            }
        }
    }

    /// Adds the Orchard/Ironwood proofs (see [`hardware_prove`]).
    pub fn hardware_prove(&self, pczt: &[u8]) -> Result<Vec<u8>> {
        hardware_prove(pczt)
    }

    /// See [`hardware_signer_copy`].
    pub fn hardware_signer_copy(&self, pczt: &[u8], copy: SignerCopy) -> Result<Vec<u8>> {
        hardware_signer_copy(pczt, copy)
    }

    /// See [`hardware_apply_signatures`].
    pub fn hardware_apply_signatures(
        &self,
        pczt: &[u8],
        signatures: &[DeviceSignature],
    ) -> Result<Vec<u8>> {
        hardware_apply_signatures(pczt, signatures)
    }

    /// See [`hardware_combine`] (applies only the device's signatures).
    pub fn hardware_combine(&self, proved: &[u8], signed: &[u8]) -> Result<Vec<u8>> {
        hardware_combine(proved, signed)
    }

    /// Releases notes reserved by unfinished hardware PCZTs. Locks held by
    /// other owners, including a seed send, stay reserved.
    pub fn hardware_release_locks(&mut self) -> usize {
        let Ok(locked) = self.get_locked_outputs(ACCOUNT) else {
            return 0;
        };
        let mut released = 0;
        for output in locked {
            if self
                .unlock_output(&output, LockOwner::new(HARDWARE_LOCK_OWNER))
                .unwrap_or(false)
            {
                released += 1;
            }
        }
        released
    }

    fn ledger_account(&self) -> Result<crate::ledger::ExpectedAccount> {
        let hw = self
            .hardware
            .as_ref()
            .filter(|h| h.device == "ledger")
            .ok_or_else(|| EngineError::Message("this wallet is not a Ledger account".into()))?;
        let fp = super::from_hex(&hw.seed_fingerprint)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .ok_or_else(|| EngineError::Message("stored seed fingerprint is invalid".into()))?;
        Ok(crate::ledger::ExpectedAccount {
            coin_type: {
                use zcash_protocol::consensus::{NetworkConstants, Parameters};
                self.network().network_type().coin_type()
            },
            account_index: hw.account_index,
            seed_fingerprint: fp,
        })
    }

    /// The APDUs that have the connected Ledger (`app_version` from its app
    /// info) review and sign `pczt`. Works on the PCZT before or after proving,
    /// so proving can run while the user reviews on the device.
    pub fn ledger_signing_plan(
        &self,
        pczt: &[u8],
        app_version: &str,
    ) -> Result<crate::ledger::SigningPlan> {
        crate::ledger::signing_plan(pczt, &self.ledger_account()?, app_version)
            .map_err(EngineError::Message)
    }

    /// Applies the Ledger's raw responses to a signing plan (status words
    /// included) to `pczt`; every signature is verified before it is kept.
    pub fn ledger_apply_responses(&self, pczt: &[u8], responses: &[Vec<u8>]) -> Result<Vec<u8>> {
        let signatures =
            crate::ledger::signatures_from_responses(pczt, &self.ledger_account()?, responses)
                .map_err(EngineError::Message)?;
        let mut signer = Signer::new(parse(pczt)?)
            .map_err(|e| EngineError::Message(format!("PCZT signer: {e:?}")))?;
        for sig in &signatures {
            signer.apply_orchard_spend_auth_signature(sig).map_err(|e| {
                EngineError::Message(format!(
                    "ledger_signature_mismatch: {:?} action {} was not signed by this account's key: {e:?}",
                    sig.value_pool(),
                    sig.action_index()
                ))
            })?;
        }
        serialize(signer.finish())
    }

    /// Finalizes a proved and signed PCZT, records the transaction as pending
    /// (its inputs stay spent until it is mined or expires), and returns it
    /// for broadcast.
    pub fn hardware_finalize(&mut self, pczt: &[u8]) -> Result<FinalizedTransaction> {
        self.require_hardware()?;
        let pczt = parse(pczt)?;
        let vk = verifying_key(circuit_for_branch(*pczt.global().consensus_branch_id()));
        let txid = extract_and_store_transaction_from_pczt::<
            _,
            <Self as zcash_client_backend::data_api::InputSource>::NoteRef,
        >(self, pczt, None, Some(vk))
        .map_err(|e| EngineError::Message(format!("finalize PCZT: {e}")))?;
        let raw = self.pending_txs.get(&txid).ok_or_else(|| {
            EngineError::Message("finalized transaction missing from the wallet".into())
        })?;
        Ok(FinalizedTransaction {
            txid: txid.to_string(),
            hex: super::to_hex(raw),
        })
    }
}

/// Adds the Orchard/Ironwood proofs (the slow part; the device never proves).
pub fn hardware_prove(pczt: &[u8]) -> Result<Vec<u8>> {
    let pczt = parse(pczt)?;
    let circuit = circuit_for_branch(*pczt.global().consensus_branch_id());
    let mut prover = Prover::new(pczt);
    if prover.requires_sapling_proofs() {
        return Err(EngineError::Message(
            "this PCZT needs Sapling proofs, which the web engine does not make".into(),
        ));
    }
    if prover.requires_orchard_proof() {
        prover = prover
            .create_orchard_proof(cached_orchard_proving_key(circuit))
            .map_err(|e| EngineError::Message(format!("Orchard proof: {e:?}")))?;
    }
    if prover.requires_ironwood_proof() {
        prover = prover
            .create_ironwood_proof(cached_orchard_proving_key(OrchardCircuitVersion::PostNu6_3))
            .map_err(|e| EngineError::Message(format!("Ironwood proof: {e:?}")))?;
    }
    serialize(prover.finish())
}

/// The copy of a PCZT to hand a device: redacted so it carries what the
/// signer needs and no more (no witnesses; no FVK in the batch view).
pub fn hardware_signer_copy(pczt: &[u8], copy: SignerCopy) -> Result<Vec<u8>> {
    let pczt = parse(pczt)?;
    let redacted = match copy {
        SignerCopy::Full => redact_pczt_for_signer(&pczt, SignerView::Full),
        SignerCopy::Compact => redact_pczt_for_signer(&pczt, SignerView::Compact),
        SignerCopy::Batch => redact_pczt_for_batch_signer(&pczt),
    };
    serialize(redacted)
}

/// Applies spend authorization signatures returned by a device (Ledger,
/// Keystone batch). Each is verified against its action's `rk` first.
pub fn hardware_apply_signatures(pczt: &[u8], signatures: &[DeviceSignature]) -> Result<Vec<u8>> {
    let mut signer = Signer::new(parse(pczt)?)
        .map_err(|e| EngineError::Message(format!("PCZT signer: {e:?}")))?;
    for sig in signatures {
        let pool = match sig.pool.trim().to_ascii_lowercase().as_str() {
            "orchard" => orchard::ValuePool::Orchard,
            "ironwood" => orchard::ValuePool::Ironwood,
            other => return Err(EngineError::Message(format!("unknown pool \"{other}\""))),
        };
        let parts =
            SpendAuthSignature::from_parts(pool, sig.action_index, from_hex_sig(&sig.signature)?);
        signer
            .apply_orchard_spend_auth_signature(&parts)
            .map_err(|e| {
                EngineError::Message(format!(
                    "hardware_mismatch: {} action {} was not signed by this account's key: {e:?}",
                    sig.pool, sig.action_index
                ))
            })?;
    }
    serialize(signer.finish())
}

/// Takes the spend authorization signatures from a device-signed PCZT
/// (Keystone full flow) and applies them to the proved one, verifying each.
/// Nothing else the device returns is used.
pub fn hardware_combine(proved: &[u8], signed: &[u8]) -> Result<Vec<u8>> {
    let signatures = extract_orchard_spend_auth_signatures(&parse(signed)?);
    if signatures.is_empty() {
        return Err(EngineError::Message(
            "the device returned no signatures".into(),
        ));
    }
    let mut signer = Signer::new(parse(proved)?)
        .map_err(|e| EngineError::Message(format!("PCZT signer: {e:?}")))?;
    for sig in &signatures {
        signer
            .apply_orchard_spend_auth_signature(sig)
            .map_err(|e| {
                EngineError::Message(format!(
                    "hardware_mismatch: {:?} action {} was not signed by this account's key: {e:?}",
                    sig.value_pool(),
                    sig.action_index()
                ))
            })?;
    }
    serialize(signer.finish())
}

/// Signs every unsigned Orchard/Ironwood spend in a PCZT with keys derived
/// from `mnemonic`, the way a device would. For tests and device simulators
/// only: real hardware wallets keep the keys on the device.
pub fn sign_pczt_with_mnemonic(
    pczt: &[u8],
    mnemonic: &str,
    network: crate::Network,
    account_index: u32,
) -> Result<Vec<u8>> {
    use zcash_keys::keys::UnifiedSpendingKey;
    let words = bip39::Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| EngineError::Message(format!("invalid mnemonic: {e}")))?;
    let seed = words.to_seed("");
    let account = zip32::AccountId::try_from(account_index)
        .map_err(|_| EngineError::Message("account index out of range".into()))?;
    let usk = UnifiedSpendingKey::from_seed(&network, &seed, account)
        .map_err(|e| EngineError::Message(format!("USK: {e:?}")))?;
    let ask = orchard::keys::SpendAuthorizingKey::from(usk.orchard());
    let parsed = parse(pczt)?;
    let (orchard_n, ironwood_n) = (
        parsed.orchard().actions().len(),
        parsed.ironwood().actions().len(),
    );
    // The IO Finalizer already signed the dummy spends, so an unsigned spend is
    // one of ours.
    let unsigned_spend = |spend: &pczt::orchard::Spend| spend.spend_auth_sig().is_none();
    let orchard_todo: Vec<usize> = (0..orchard_n)
        .filter(|&i| unsigned_spend(parsed.orchard().actions()[i].spend()))
        .collect();
    let ironwood_todo: Vec<usize> = (0..ironwood_n)
        .filter(|&i| unsigned_spend(parsed.ironwood().actions()[i].spend()))
        .collect();
    let mut signer =
        Signer::new(parsed).map_err(|e| EngineError::Message(format!("PCZT signer: {e:?}")))?;
    for i in orchard_todo {
        signer
            .sign_orchard(i, &ask)
            .map_err(|e| EngineError::Message(format!("sign orchard {i}: {e:?}")))?;
    }
    for i in ironwood_todo {
        signer
            .sign_ironwood(i, &ask)
            .map_err(|e| EngineError::Message(format!("sign ironwood {i}: {e:?}")))?;
    }
    serialize(signer.finish())
}
