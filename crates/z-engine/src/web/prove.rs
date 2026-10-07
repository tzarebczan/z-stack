//! Orchard/Ironwood-funded propose + prove + serialize for the snapshot wallet.
//!
//! Sapling params stay out of WASM. A sapling spend/output hits `NoSaplingProver`.

use super::write::ACCOUNT;
use super::WebWallet;
use crate::error::{EngineError, Result};
use crate::parse_zec_to_zatoshis;
use rand_core::Rng;
use sapling::prover::{OutputProver, SpendProver};
use std::convert::Infallible;
use zcash_address::ZcashAddress;
#[cfg(feature = "transparent-inputs")]
use zcash_client_backend::data_api::{wallet::propose_shielding, CoinbaseFilter, WalletRead};
use zcash_client_backend::{
    data_api::wallet::{
        create_proposed_transactions,
        input_selection::{GreedyInputSelector, SpendPolicy},
        propose_transfer, ConfirmationsPolicy, SpendingKeys,
    },
    fees::{standard::SingleOutputChangeStrategy, DustOutputPolicy, StandardFeeRule},
    wallet::OvkPolicy,
    zip321::{Payment, TransactionRequest},
};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_protocol::{memo::MemoBytes, value::Zatoshis, ShieldedPool};
use zip32::AccountId as Zip32AccountId;

pub const SHIELD_NOT_READY: &str =
    "wasm snapshot wallet has no spendable transparent UTXOs (loopback GetAddressUtxos first)";

/// Process-wide Orchard proving-key cache (same `OnceLock` `Builder::build` uses).
static ORCHARD_PK_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True after [`warm_orchard_proving_key`] (or a real prove) has filled the cache.
pub fn orchard_proving_key_ready() -> bool {
    ORCHARD_PK_READY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build + cache Orchard proving keys so the first send does not pay 20–60s inline.
///
/// Hits `zcash_primitives::transaction::builder::cached_orchard_proving_key` (the same
/// `OnceLock` create/prove uses). Safe to call from a worker thread; do not run it on
/// the scan Rayon pool while compact-block sync is in flight.
#[cfg(any(feature = "wasm-prove", feature = "native"))]
pub fn warm_orchard_proving_key() -> bool {
    use orchard::circuit::OrchardCircuitVersion;
    use zcash_primitives::transaction::builder::cached_orchard_proving_key;

    let v2 = cached_orchard_proving_key(OrchardCircuitVersion::FixedPostNu6_2);
    let _ = v2.prepare_proving();
    let v3 = cached_orchard_proving_key(OrchardCircuitVersion::PostNu6_3);
    let _ = v3.prepare_proving();
    ORCHARD_PK_READY.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

#[cfg(not(any(feature = "wasm-prove", feature = "native")))]
pub fn warm_orchard_proving_key() -> bool {
    false
}

fn usk_from_mnemonic(wallet: &super::WebWallet, mnemonic: &str) -> Result<UnifiedSpendingKey> {
    // A hardware account spends only through its device (PCZTs the user
    // reviews there), even if its seed turns up elsewhere on the origin.
    if wallet.hardware.is_some() {
        return Err(EngineError::Message(
            "this is a hardware-wallet account: spends are signed on the device".into(),
        ));
    }
    let mnemonic = bip39::Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| EngineError::Message(format!("invalid mnemonic: {e}")))?;
    let seed = mnemonic.to_seed("");
    let zip = Zip32AccountId::try_from(wallet.account_index()).unwrap_or(Zip32AccountId::ZERO);
    let usk = UnifiedSpendingKey::from_seed(&wallet.network(), &seed, zip)
        .map_err(|e| EngineError::Message(format!("USK: {e:?}")))?;
    let derived = usk.to_unified_full_viewing_key().encode(&wallet.network());
    crate::keys::derived_ufvk_covers(wallet.network(), &derived, wallet.ufvk()).map_err(|e| {
        EngineError::Message(format!("{e} (account index {})", wallet.account_index()))
    })?;
    Ok(usk)
}

#[cfg(feature = "transparent-inputs")]
fn shield_confirmations(network: crate::Network) -> ConfirmationsPolicy {
    crate::confirmations_policy(network)
}

/// Sapling proofs are not in the WASM blob. Orchard-only spends never call this.
pub struct NoSaplingProver;

impl SpendProver for NoSaplingProver {
    type Proof = sapling::bundle::GrothProofBytes;

    fn prepare_circuit(
        _proof_generation_key: sapling::ProofGenerationKey,
        _diversifier: sapling::Diversifier,
        _rseed: sapling::Rseed,
        _value: sapling::value::NoteValue,
        _alpha: jubjub::Fr,
        _rcv: sapling::value::ValueCommitTrapdoor,
        _anchor: bls12_381::Scalar,
        _merkle_path: sapling::MerklePath,
    ) -> Option<sapling::circuit::Spend> {
        None
    }

    fn create_proof<R: Rng>(&self, _circuit: sapling::circuit::Spend, _rng: &mut R) -> Self::Proof {
        panic!("sapling proving is not in the wasm blob; send orchard-only")
    }

    fn encode_proof(proof: Self::Proof) -> sapling::bundle::GrothProofBytes {
        proof
    }
}

impl OutputProver for NoSaplingProver {
    type Proof = sapling::bundle::GrothProofBytes;

    fn prepare_circuit(
        _esk: &sapling::keys::EphemeralSecretKey,
        _payment_address: sapling::PaymentAddress,
        _rcm: jubjub::Fr,
        _value: sapling::value::NoteValue,
        _rcv: sapling::value::ValueCommitTrapdoor,
    ) -> sapling::circuit::Output {
        panic!("sapling output proving is not in the wasm blob; send orchard-only")
    }

    fn create_proof<R: Rng>(
        &self,
        _circuit: sapling::circuit::Output,
        _rng: &mut R,
    ) -> Self::Proof {
        panic!("sapling proving is not in the wasm blob")
    }

    fn encode_proof(proof: Self::Proof) -> sapling::bundle::GrothProofBytes {
        proof
    }
}

/// Orchard and Ironwood notes. After NU6.3 every payment to our Orchard
/// receiver arrives as an Ironwood note; Sapling proving is not in the blob.
pub(super) fn orchard_spend_policy() -> SpendPolicy {
    SpendPolicy::shielded_pools([ShieldedPool::Orchard, ShieldedPool::Ironwood])
}

impl WebWallet {
    pub fn prove_shield(&mut self, mnemonic: &str, threshold_zat: u64) -> Result<Vec<u8>> {
        #[cfg(not(feature = "transparent-inputs"))]
        {
            let _ = (mnemonic, threshold_zat);
            return Err(EngineError::Message(SHIELD_NOT_READY.into()));
        }
        #[cfg(feature = "transparent-inputs")]
        {
            let usk = usk_from_mnemonic(self, mnemonic)?;
            self.finalize_for_spend()?;
            if self.scanned_height() == 0 {
                return Err(EngineError::SyncRequired);
            }
            let from_addrs: Vec<_> = self
                .get_transparent_receivers(ACCOUNT, true, true)
                .map_err(|e| EngineError::Message(format!("receivers: {e}")))?
                .into_keys()
                .collect();
            if from_addrs.is_empty() {
                return Err(EngineError::Message("no transparent receivers".into()));
            }
            let input_selector = GreedyInputSelector::new();
            let change_strategy = SingleOutputChangeStrategy::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            );
            let threshold = Zatoshis::from_u64(threshold_zat.max(1))
                .map_err(|_| EngineError::Message("bad threshold".into()))?;
            let proposal = propose_shielding::<_, _, _, _, Infallible>(
                self,
                &self.network(),
                &input_selector,
                &change_strategy,
                threshold,
                &from_addrs,
                ACCOUNT,
                shield_confirmations(self.network()),
                CoinbaseFilter::AllTransparentOutputs,
                None,
            )
            .map_err(|e| map_funds(format!("propose_shielding: {e}")))?;

            let prover = NoSaplingProver;
            let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                self,
                &self.network(),
                &prover,
                &prover,
                &SpendingKeys::from_unified_spending_key(usk),
                OvkPolicy::Sender,
                &proposal,
                None,
            )
            .map_err(|e| map_funds(format!("create shielding tx: {e}")))?;
            let txid = *txids.first();
            let raw = self
                .pending_txs
                .get(&txid)
                .ok_or_else(|| EngineError::Message("proved shield tx missing from store".into()))?
                .clone();
            Ok(raw)
        }
    }

    /// Propose + prove an Orchard send. Returns raw transaction bytes.
    /// Mnemonic is required at prove time and is not stored in the snapshot.
    /// `to` may be a unified address or a ZIP-321 `zcash:` URI (including multi-pay).
    pub fn prove_send(
        &mut self,
        mnemonic: &str,
        to: &str,
        amount_zec: &str,
        memo: Option<&str>,
    ) -> Result<Vec<u8>> {
        if to.trim().to_ascii_lowercase().starts_with("zcash:") {
            let mut req = crate::parse_zip321(to).map_err(EngineError::Message)?;
            if req.payments.len() == 1 {
                if req.payments[0].amount_zec.is_none() && !amount_zec.trim().is_empty() {
                    req.payments[0].amount_zec = Some(amount_zec.trim().to_string());
                }
                if req.payments[0].memo.is_none() {
                    if let Some(m) = memo.map(str::trim).filter(|s| !s.is_empty()) {
                        req.payments[0].memo = Some(m.to_string());
                    }
                }
            }
            return self.prove_send_payments(mnemonic, &req.payments);
        }
        let amount = parse_zec_to_zatoshis(amount_zec).map_err(EngineError::Message)?;
        self.prove_send_zat(mnemonic, to, amount, memo)
    }

    /// Explicit privacy-reducing output for reviewed swaps. The funding policy
    /// remains Orchard/Ironwood-only; this does not enable transparent inputs.
    pub fn prove_transparent_send(
        &mut self,
        mnemonic: &str,
        to: &str,
        amount_zec: &str,
        max_fee_zat: Option<u64>,
    ) -> Result<Vec<u8>> {
        let request = self.transparent_send_request(to, amount_zec)?;
        self.finish_prove_send_capped(mnemonic, request, Some(to), max_fee_zat)
    }

    /// Fee for exactly one bare P2PKH/P2SH output, with shielded change.
    pub fn estimate_transparent_fee(&mut self, to: &str, amount_zec: &str) -> Result<u64> {
        let request = self.transparent_send_request(to, amount_zec)?;
        self.propose_fee(request)
    }

    fn transparent_send_request(&self, to: &str, amount_zec: &str) -> Result<TransactionRequest> {
        if self.hardware.is_some() {
            return Err(EngineError::Message(
                "transparent swap outputs are not supported for hardware wallets".into(),
            ));
        }
        let parsed = crate::keys::parse_address(to)?;
        if !matches!(
            parsed.kind,
            crate::keys::AddressKind::P2pkh | crate::keys::AddressKind::P2sh
        ) {
            return Err(EngineError::Message(
                "swap destination must be a bare transparent P2PKH or P2SH address".into(),
            ));
        }
        // Testnet and regtest intentionally share transparent address prefixes.
        let matches_network = parsed.network == self.network()
            || (self.network() == crate::Network::Regtest
                && parsed.network == crate::Network::Testnet);
        if !matches_network {
            return Err(EngineError::Message(
                "swap destination belongs to another network".into(),
            ));
        }
        let amount = parse_zec_to_zatoshis(amount_zec).map_err(EngineError::Message)?;
        if amount == 0 {
            return Err(EngineError::Message(
                "swap amount must be greater than zero".into(),
            ));
        }
        // No URI, receiver selection, multipay or memo overload exists here.
        let payment = zip_payment(to, amount, None)?;
        TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))
    }

    pub fn prove_send_zat(
        &mut self,
        mnemonic: &str,
        to: &str,
        amount_zat: u64,
        memo: Option<&str>,
    ) -> Result<Vec<u8>> {
        crate::keys::assert_shielded_send_dest(to, crate::keys::SendDestPolicy::OrchardOnly)?;
        let payment = zip_payment(to, amount_zat, memo)?;
        self.finish_prove_send(
            mnemonic,
            TransactionRequest::new(vec![payment])
                .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?,
            Some(to),
        )
    }

    fn prove_send_payments(
        &mut self,
        mnemonic: &str,
        payments: &[crate::Zip321Payment],
    ) -> Result<Vec<u8>> {
        if payments.is_empty() {
            return Err(EngineError::Message("ZIP-321 URI has no payments".into()));
        }
        for p in payments {
            crate::keys::assert_shielded_send_dest(
                &p.address,
                crate::keys::SendDestPolicy::OrchardOnly,
            )?;
        }
        let mut built = Vec::with_capacity(payments.len());
        for p in payments {
            let zat = match p.amount_zec.as_deref() {
                Some(s) => parse_zec_to_zatoshis(s).map_err(EngineError::Message)?,
                None => 0,
            };
            built.push(zip_payment(&p.address, zat, p.memo.as_deref())?);
        }
        let request = TransactionRequest::new(built)
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?;
        let addrs: Vec<String> = payments.iter().map(|p| p.address.clone()).collect();
        let raw = self.finish_prove_send(mnemonic, request, None)?;
        for a in addrs {
            self.record_recipient(&a);
        }
        Ok(raw)
    }

    fn finish_prove_send(
        &mut self,
        mnemonic: &str,
        request: TransactionRequest,
        record_to: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.finish_prove_send_capped(mnemonic, request, record_to, None)
    }

    fn finish_prove_send_capped(
        &mut self,
        mnemonic: &str,
        request: TransactionRequest,
        record_to: Option<&str>,
        max_fee_zat: Option<u64>,
    ) -> Result<Vec<u8>> {
        let usk = usk_from_mnemonic(self, mnemonic)?;
        let proposal = self.propose_send(request, None)?;
        if max_fee_zat.is_some_and(|limit| crate::proposal_fee_zat(&proposal) > limit) {
            return Err(EngineError::Message(
                "swap transaction fee exceeds the approved maximum".into(),
            ));
        }

        let prover = NoSaplingProver;
        let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
            self,
            &self.network(),
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| map_funds(format!("create send tx: {e}")))?;

        let txid = *txids.first();
        let raw = self
            .pending_txs
            .get(&txid)
            .ok_or_else(|| EngineError::Message("proved tx missing from store".into()))?
            .clone();
        if let Some(to) = record_to {
            self.record_recipient(to);
        }
        Ok(raw)
    }

    /// Orchard/Ironwood proposal for a send (the same one a seed send or a
    /// hardware PCZT is built from). Finalizes trees first.
    ///
    /// `lock` reserves the selected notes (hardware PCZTs, which are signed
    /// long after they are proposed); seed sends store the transaction in the
    /// same call and need none.
    pub(super) fn propose_send(
        &mut self,
        request: TransactionRequest,
        lock: Option<zcash_client_backend::data_api::wallet::LockRequest>,
    ) -> Result<
        zcash_client_backend::proposal::Proposal<
            StandardFeeRule,
            <Self as zcash_client_backend::data_api::InputSource>::NoteRef,
        >,
    > {
        self.finalize_for_spend()?;
        if self.scanned_height() == 0 {
            return Err(EngineError::SyncRequired);
        }
        let input_selector = GreedyInputSelector::new();
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::default(),
        );
        propose_transfer::<_, _, _, _, Infallible>(
            self,
            &self.network(),
            ACCOUNT,
            &input_selector,
            &change_strategy,
            request,
            crate::confirmations_policy(self.network()),
            &orchard_spend_policy(),
            lock,
            None,
        )
        .map_err(|e| map_funds(format!("propose_transfer: {e}")))
    }

    /// ZIP-317 fee for a ZIP-321 / unified send. Propose only — no mnemonic, no prove.
    pub fn estimate_fee(&mut self, to: &str, amount_zec: &str, memo: Option<&str>) -> Result<u64> {
        let request = self.send_request(to, amount_zec, memo)?;
        self.propose_fee(request)
    }

    /// Largest orchard amount `propose_transfer` accepts (own UA if `to` is omitted).
    pub fn max_send(&mut self, to: Option<&str>) -> Result<(u64, u64)> {
        let dest = match to.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) if s.to_ascii_lowercase().starts_with("zcash:") => {
                let req = crate::parse_zip321(s).map_err(EngineError::Message)?;
                let addr = req.address().to_string();
                if addr.is_empty() {
                    return Err(EngineError::Message("ZIP-321 URI has no address".into()));
                }
                crate::keys::assert_shielded_send_dest(
                    &addr,
                    crate::keys::SendDestPolicy::OrchardOnly,
                )?;
                addr
            }
            Some(s) => {
                crate::keys::assert_shielded_send_dest(
                    s,
                    crate::keys::SendDestPolicy::OrchardOnly,
                )?;
                s.to_string()
            }
            None => self.unified_address().to_string(),
        };
        let balance = self.balance();
        let available = balance
            .orchard_available
            .saturating_add(balance.ironwood_available);
        if available == 0 {
            return Ok((0, 0));
        }
        let mut pad = crate::FEE_PAD_ZAT;
        for _ in 0..8 {
            let amt = available.saturating_sub(pad);
            if amt == 0 {
                return Ok((0, 0));
            }
            match self.propose_amount(&dest, amt) {
                Ok(fee) => {
                    let exact = available.saturating_sub(fee);
                    if exact > 0 && exact != amt {
                        if let Ok(fee2) = self.propose_amount(&dest, exact) {
                            return Ok((exact, fee2));
                        }
                    }
                    return Ok((amt, fee));
                }
                Err(EngineError::InsufficientFunds) => {
                    pad = pad.saturating_mul(2).max(pad.saturating_add(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok((0, 0))
    }

    pub(super) fn send_request(
        &self,
        to: &str,
        amount_zec: &str,
        memo: Option<&str>,
    ) -> Result<TransactionRequest> {
        if to.trim().to_ascii_lowercase().starts_with("zcash:") {
            let mut req = crate::parse_zip321(to).map_err(EngineError::Message)?;
            if req.payments.len() == 1 {
                if req.payments[0].amount_zec.is_none() && !amount_zec.trim().is_empty() {
                    req.payments[0].amount_zec = Some(amount_zec.trim().to_string());
                }
                if req.payments[0].memo.is_none() {
                    if let Some(m) = memo.map(str::trim).filter(|s| !s.is_empty()) {
                        req.payments[0].memo = Some(m.to_string());
                    }
                }
            }
            return self.zip321_request(&req.payments);
        }
        let amount = parse_zec_to_zatoshis(amount_zec).map_err(EngineError::Message)?;
        crate::keys::assert_shielded_send_dest(to, crate::keys::SendDestPolicy::OrchardOnly)?;
        let payment = zip_payment(to, amount, memo)?;
        TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))
    }

    fn zip321_request(&self, payments: &[crate::Zip321Payment]) -> Result<TransactionRequest> {
        if payments.is_empty() {
            return Err(EngineError::Message("ZIP-321 URI has no payments".into()));
        }
        let mut built = Vec::with_capacity(payments.len());
        for p in payments {
            crate::keys::assert_shielded_send_dest(
                &p.address,
                crate::keys::SendDestPolicy::OrchardOnly,
            )?;
            let zat = match p.amount_zec.as_deref() {
                Some(s) => parse_zec_to_zatoshis(s).map_err(EngineError::Message)?,
                None => 0,
            };
            built.push(zip_payment(&p.address, zat, p.memo.as_deref())?);
        }
        TransactionRequest::new(built).map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))
    }

    fn propose_amount(&mut self, dest: &str, amount_zat: u64) -> Result<u64> {
        let payment = zip_payment(dest, amount_zat, None)?;
        let request = TransactionRequest::new(vec![payment])
            .map_err(|e| EngineError::Message(format!("ZIP-321: {e}")))?;
        self.propose_fee(request)
    }

    fn propose_fee(&mut self, request: TransactionRequest) -> Result<u64> {
        self.finalize_scan_trees()?;
        if self.scanned_height() == 0 {
            return Err(EngineError::SyncRequired);
        }
        let input_selector = GreedyInputSelector::new();
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::default(),
        );
        let proposal = propose_transfer::<_, _, _, _, Infallible>(
            self,
            &self.network(),
            ACCOUNT,
            &input_selector,
            &change_strategy,
            request,
            crate::confirmations_policy(self.network()),
            &orchard_spend_policy(),
            None,
            None,
        )
        .map_err(|e| map_funds(format!("propose_transfer: {e}")))?;
        Ok(crate::proposal_fee_zat(&proposal))
    }
}

fn zip_payment(to: &str, zat: u64, memo: Option<&str>) -> Result<Payment> {
    let address = ZcashAddress::try_from_encoded(to.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    let value = Zatoshis::from_u64(zat).map_err(|_| EngineError::InsufficientFunds)?;
    let memo_bytes = match memo.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let mut buf = [0u8; 512];
            let raw = s.as_bytes();
            if raw.len() > 512 {
                return Err(EngineError::Message("memo longer than 512 bytes".into()));
            }
            buf[..raw.len()].copy_from_slice(raw);
            Some(
                MemoBytes::from_bytes(&buf)
                    .map_err(|e| EngineError::Message(format!("memo: {e:?}")))?,
            )
        }
    };
    if zat == 0 && memo_bytes.is_none() {
        return Err(EngineError::Message(
            "zero-value send needs a memo (encrypted note)".into(),
        ));
    }
    match memo_bytes {
        None => Ok(Payment::without_memo(address, value)),
        Some(m) => Payment::new(address, Some(value), Some(m), None, None, vec![])
            .map_err(|e| EngineError::Message(format!("payment: {e}"))),
    }
}

pub(super) fn map_funds(msg: String) -> EngineError {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("insufficient") {
        EngineError::InsufficientFunds
    } else {
        EngineError::Message(msg)
    }
}
