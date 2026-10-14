//! Funding authority is independent of PIR projection/deletion ownership.
use super::*;
use std::collections::BTreeSet;
use transparent::{address::TransparentAddress, bundle::OutPoint};
use zcash_client_backend::{
    data_api::{
        wallet::{input_selection::ShieldingSelector, TargetHeight},
        AccountMeta, NoteFilter, ReceivedNotes,
    },
    wallet::{Note, ReceivedNote, WalletTransparentOutput},
};

fn key(outpoint: &OutPoint) -> String {
    format!("r:{}:{}", crate::web::to_hex(outpoint.hash()), outpoint.n())
}

pub(super) fn check_proposal<F, N>(
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
    proposal: &zcash_client_backend::proposal::Proposal<F, N>,
) -> anyhow::Result<()>
where
    F: zcash_primitives::transaction::fees::FeeRule,
    N: Copy + Eq + Ord,
{
    let excluded = super::transparent_pir::funding_exclusions(ext)?;
    if proposal
        .steps()
        .iter()
        .flat_map(|s| s.transparent_inputs())
        .any(|o| excluded.contains(&key(o.outpoint())))
    {
        anyhow::bail!("transparent_pir_funding_unverified");
    }
    Ok(())
}

pub(super) fn propose<D>(
    db: &D,
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
    network: ZNetwork,
    addresses: &[TransparentAddress],
    account: AccountUuid,
    threshold: Zatoshis,
) -> Result<zcash_client_backend::proposal::Proposal<StandardFeeRule, Infallible>>
where
    D: WalletRead<AccountId = AccountUuid>
        + InputSource<AccountId = AccountUuid, Error = <D as WalletRead>::Error>,
    <D as WalletRead>::Error: std::fmt::Display,
{
    let inputs = FundingInputs {
        db,
        excluded: super::transparent_pir::funding_exclusions(ext).map_err(EngineError::from)?,
    };
    let confirmations = shield_confirmations(network);
    let (target, anchor) = db
        .get_target_and_anchor_heights(confirmations.trusted())
        .map_err(|_| EngineError::SyncRequired)?
        .ok_or(EngineError::SyncRequired)?;
    GreedyInputSelector::new()
        .propose_shielding(
            &network,
            &inputs,
            &SingleOutputChangeStrategy::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            ),
            threshold,
            addresses,
            account,
            target,
            anchor,
            &db.pool_migration_params(),
            confirmations,
            CoinbaseFilter::AllTransparentOutputs,
        )
        .map_err(|e| map_funds_err(format!("propose_shielding: {e}")))
}

struct FundingInputs<'a, D> {
    db: &'a D,
    excluded: BTreeSet<String>,
}
impl<D: InputSource> InputSource for FundingInputs<'_, D> {
    type Error = D::Error;
    type AccountId = D::AccountId;
    type NoteRef = D::NoteRef;
    fn get_spendable_note(
        &self,
        txid: &TxId,
        protocol: ShieldedPool,
        index: u32,
        target: TargetHeight,
        lock: LockFilter<'_>,
    ) -> std::result::Result<Option<ReceivedNote<Self::NoteRef, Note>>, Self::Error> {
        self.db
            .get_spendable_note(txid, protocol, index, target, lock)
    }
    fn anchor_computable(
        &self,
        protocol: ShieldedPool,
        height: BlockHeight,
    ) -> std::result::Result<bool, Self::Error> {
        self.db.anchor_computable(protocol, height)
    }
    fn select_spendable_notes(
        &self,
        account: Self::AccountId,
        value: TargetValue,
        sources: &[ShieldedPool],
        target: TargetHeight,
        confirmations: ConfirmationsPolicy,
        exclude: &[Self::NoteRef],
        lock: LockFilter<'_>,
    ) -> std::result::Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        self.db.select_spendable_notes(
            account,
            value,
            sources,
            target,
            confirmations,
            exclude,
            lock,
        )
    }
    fn select_unspent_notes(
        &self,
        account: Self::AccountId,
        sources: &[ShieldedPool],
        target: TargetHeight,
        exclude: &[Self::NoteRef],
        lock: LockFilter<'_>,
    ) -> std::result::Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        self.db
            .select_unspent_notes(account, sources, target, exclude, lock)
    }
    fn get_account_metadata(
        &self,
        account: Self::AccountId,
        selector: &NoteFilter,
        target: TargetHeight,
        exclude: &[Self::NoteRef],
        lock: LockFilter<'_>,
    ) -> std::result::Result<AccountMeta, Self::Error> {
        self.db
            .get_account_metadata(account, selector, target, exclude, lock)
    }
    fn get_unspent_transparent_output(
        &self,
        outpoint: &OutPoint,
        target: TargetHeight,
    ) -> std::result::Result<Option<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        if self.excluded.contains(&key(outpoint)) {
            return Ok(None);
        }
        self.db.get_unspent_transparent_output(outpoint, target)
    }
    fn get_spendable_transparent_outputs(
        &self,
        address: &TransparentAddress,
        target: TargetHeight,
        confirmations: ConfirmationsPolicy,
        filter: CoinbaseFilter,
        lock: LockFilter<'_>,
    ) -> std::result::Result<Vec<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        self.db
            .get_spendable_transparent_outputs(address, target, confirmations, filter, lock)
            .map(|mut outputs| {
                outputs.retain(|o| !self.excluded.contains(&key(o.outpoint())));
                outputs
            })
    }
    fn get_spendable_transparent_outputs_for_addresses(
        &self,
        addresses: &[TransparentAddress],
        target: TargetHeight,
        confirmations: ConfirmationsPolicy,
        filter: CoinbaseFilter,
        lock: LockFilter<'_>,
    ) -> std::result::Result<Vec<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        self.db
            .get_spendable_transparent_outputs_for_addresses(
                addresses,
                target,
                confirmations,
                filter,
                lock,
            )
            .map(|mut outputs| {
                outputs.retain(|o| !self.excluded.contains(&key(o.outpoint())));
                outputs
            })
    }
}
