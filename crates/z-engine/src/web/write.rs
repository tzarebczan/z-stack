//! `WalletRead` / `WalletWrite` / `InputSource` / trees over the snapshot store.
//!
//! Orchard-only spend. Unused account-admin / transparent methods return errors.

use super::store::{OrchardTree, SaplingTree, StoredLock, StoredNote, WebWallet};
use incrementalmerkletree::{Address as MerkleAddress, Position};
use secrecy::{ExposeSecret, SecretVec};
use shardtree::error::ShardTreeError;
use shardtree::store::{memory::MemoryShardStore, ShardStore};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::num::NonZeroU32;
#[cfg(feature = "transparent-inputs")]
use std::time::SystemTime;
use zcash_client_backend::data_api::{
    chain::{ChainState, CommitmentTreeRoot},
    defaults::find_account_for_address,
    error::{FindAccountForAddressError, RewindError},
    locking::{LockError, LockOwner, OutputLockStore},
    scanning::{ScanPriority, ScanRange},
    wallet::{input_selection::LockFilter, ConfirmationsPolicy, TargetHeight},
    Account, AccountBirthday, AccountMeta, AccountPurpose, AccountSource, AddressInfo,
    AddressSource, BlockMetadata, InputSource, NoteFilter, NullifierQuery, PoolMeta, ReceivedNotes,
    ReceivedTransactionOutput, SeedRelevance, SentTransaction, TargetValue, TransactionDataRequest,
    TransactionStatus, WalletCommitmentTrees, WalletRead, WalletWrite, Zip32Derivation,
    ORCHARD_SHARD_HEIGHT, SAPLING_SHARD_HEIGHT,
};
#[cfg(feature = "transparent-inputs")]
use zcash_client_backend::data_api::{
    TransactionsInvolvingAddress, TransparentBalances, TransparentKeyOrigin,
};
#[cfg(feature = "transparent-inputs")]
use zcash_client_backend::wallet::{Exposure, TransparentAddressMetadata};
use zcash_client_backend::wallet::{
    Note, NoteId, OutputRef, ReceivedNote, WalletTransparentOutput,
};
use zcash_keys::address::{Address, UnifiedAddress};
use zcash_keys::keys::{
    UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedIncomingViewingKey, UnifiedSpendingKey,
};
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::{self, BlockHeight};
use zcash_protocol::memo::Memo;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{PoolType, ShieldedPool, TxId};
use zip32::{AccountId as Zip32AccountId, DiversifierIndex, Scope};

pub const ACCOUNT: u32 = 0;

pub struct WebAccount {
    id: u32,
    ufvk: UnifiedFullViewingKey,
    birthday: BlockHeight,
    source: AccountSource,
}

impl Account for WebAccount {
    type AccountId = u32;

    fn id(&self) -> u32 {
        self.id
    }

    fn name(&self) -> Option<&str> {
        Some("default")
    }

    fn birthday_height(&self) -> BlockHeight {
        self.birthday
    }

    fn source(&self) -> &AccountSource {
        &self.source
    }

    fn ufvk(&self) -> Option<&UnifiedFullViewingKey> {
        Some(&self.ufvk)
    }

    fn uivk(&self) -> UnifiedIncomingViewingKey {
        self.ufvk.to_unified_incoming_viewing_key()
    }
}

impl WebWallet {
    fn account(&self) -> Result<WebAccount, String> {
        let ufvk = self.decode_ufvk().map_err(|e| e.to_string())?;
        Ok(WebAccount {
            id: ACCOUNT,
            ufvk,
            birthday: BlockHeight::from_u32(self.birthday()),
            source: AccountSource::Imported {
                purpose: AccountPurpose::Spending {
                    derivation: self.hardware_derivation()?,
                },
                key_source: Some(
                    self.hardware
                        .as_ref()
                        .map(|h| h.device.clone())
                        .unwrap_or_else(|| "z-stack-wasm".into()),
                ),
            },
        })
    }

    /// ZIP-32 derivation of a hardware account. PCZTs carry it so the device
    /// can check the spend keys are its own.
    fn hardware_derivation(&self) -> Result<Option<Zip32Derivation>, String> {
        let Some(hw) = &self.hardware else {
            return Ok(None);
        };
        let bytes = super::from_hex(&hw.seed_fingerprint).map_err(|e| e.to_string())?;
        let fp: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "seed fingerprint must be 32 bytes".to_string())?;
        let account = Zip32AccountId::try_from(hw.account_index)
            .map_err(|_| format!("account index {} out of range", hw.account_index))?;
        Ok(Some(Zip32Derivation::new(
            zip32::fingerprint::SeedFingerprint::from_bytes(fp),
            account,
        )))
    }

    fn confirmations_ok(
        note: &StoredNote,
        target: TargetHeight,
        policy: ConfirmationsPolicy,
    ) -> bool {
        if note.spent {
            return false;
        }
        let need = if note.is_change || note.scope == 1 {
            u32::from(policy.trusted())
        } else {
            u32::from(policy.untrusted())
        };
        note.mined_height.saturating_add(need) <= u32::from(target)
    }

    fn to_received(
        &self,
        n: &StoredNote,
    ) -> Result<ReceivedNote<u32, orchard::note::Note>, String> {
        let note = Self::decode_spend_note(n).map_err(|e| e.to_string())?;
        let txid = Self::txid_from_hex(&n.txid).map_err(|e| e.to_string())?;
        Ok(ReceivedNote::from_parts(
            n.id,
            txid,
            n.output_index,
            note,
            if n.scope == 1 {
                Scope::Internal
            } else {
                Scope::External
            },
            Position::from(n.position),
            Some(BlockHeight::from_u32(n.mined_height)),
            None,
        ))
    }

    fn pool_of(n: &StoredNote) -> ShieldedPool {
        match n.pool.as_str() {
            "sapling" => ShieldedPool::Sapling,
            "ironwood" => ShieldedPool::Ironwood,
            _ => ShieldedPool::Orchard,
        }
    }

    fn note_filter_ok(n: &StoredNote, selector: &NoteFilter, orchard_total: u64) -> bool {
        match selector {
            NoteFilter::ExceedsMinValue(v) => n.value_zat > u64::from(*v),
            NoteFilter::ExceedsBalancePercentage(p) => {
                let pct = u8::from(*p) as u64;
                n.value_zat.saturating_mul(100) >= orchard_total.saturating_mul(pct)
            }
            NoteFilter::ExceedsPriorSendPercentile(_) => true,
            NoteFilter::Combine(a, b) => {
                Self::note_filter_ok(n, a, orchard_total)
                    && Self::note_filter_ok(n, b, orchard_total)
            }
            NoteFilter::Attempt {
                condition,
                fallback,
            } => {
                if matches!(**condition, NoteFilter::ExceedsPriorSendPercentile(_)) {
                    Self::note_filter_ok(n, fallback, orchard_total)
                } else {
                    Self::note_filter_ok(n, condition, orchard_total)
                }
            }
        }
    }
}

impl InputSource for WebWallet {
    type Error = String;
    type AccountId = u32;
    type NoteRef = u32;

    fn get_spendable_note(
        &self,
        txid: &TxId,
        protocol: ShieldedPool,
        index: u32,
        target_height: TargetHeight,
        _lock_filter: LockFilter<'_>,
    ) -> Result<Option<ReceivedNote<Self::NoteRef, Note>>, Self::Error> {
        let want = super::to_hex(txid.as_ref());
        for n in &self.spend_notes {
            if n.txid == want
                && n.output_index as u32 == index
                && Self::pool_of(n) == protocol
                && Self::confirmations_ok(
                    n,
                    target_height,
                    crate::confirmations_policy(self.network),
                )
            {
                let received = self.to_received(n)?;
                return Ok(Some(ReceivedNote::from_parts(
                    *received.internal_note_id(),
                    *received.txid(),
                    received.output_index(),
                    Note::Orchard {
                        note: received.note().clone(),
                        pool: match protocol {
                            ShieldedPool::Ironwood => orchard::ValuePool::Ironwood,
                            _ => orchard::ValuePool::Orchard,
                        },
                    },
                    received.spending_key_scope(),
                    received.note_commitment_tree_position(),
                    received.mined_height(),
                    received.max_shielding_input_height(),
                )));
            }
        }
        Ok(None)
    }

    fn anchor_computable(
        &self,
        protocol: ShieldedPool,
        height: BlockHeight,
    ) -> Result<bool, Self::Error> {
        let tree = match protocol {
            ShieldedPool::Sapling => {
                return Ok(self
                    .sapling_tree
                    .store()
                    .get_checkpoint(&height)
                    .map_err(|e| format!("{e:?}"))?
                    .is_some())
            }
            ShieldedPool::Ironwood => &self.ironwood_tree,
            _ => &self.orchard_tree,
        };
        Ok(tree
            .store()
            .get_checkpoint(&height)
            .map_err(|e| format!("{e:?}"))?
            .is_some())
    }

    fn select_spendable_notes(
        &self,
        account: Self::AccountId,
        target_value: TargetValue,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        exclude: &[Self::NoteRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        if account != ACCOUNT {
            return Ok(ReceivedNotes::empty());
        }
        let mut orchard = Vec::new();
        let mut ironwood = Vec::new();
        // Like zcash_client_sqlite, skip notes worth no more than the ZIP-317
        // marginal fee. Orchard spends from NU6.3 on leave zero-value Orchard
        // notes behind; offered to the greedy selector, they come back as dust
        // and excluding them adds no value, so it reports insufficient funds.
        let min_value = u64::from(zcash_primitives::transaction::fees::zip317::MARGINAL_FEE);
        let mut chosen: Vec<&StoredNote> = self
            .spend_notes
            .iter()
            .filter(|n| {
                !n.spent
                    && n.value_zat > min_value
                    && !exclude.contains(&n.id)
                    && sources.contains(&Self::pool_of(n))
                    && Self::confirmations_ok(n, target_height, confirmations_policy)
                    && self.lock_admits(n, &lock_filter)
            })
            .collect();
        chosen.sort_by_key(|n| (n.mined_height, n.position));
        let mut acc = 0u64;
        let want = match target_value {
            TargetValue::AtLeast(v) => Some(u64::from(v)),
            TargetValue::AllFunds(_) => None,
        };
        for n in chosen {
            if n.pool == "sapling" {
                continue;
            }
            let rec = self.to_received(n)?;
            if n.pool == "ironwood" {
                ironwood.push(rec);
            } else {
                orchard.push(rec);
            }
            acc = acc.saturating_add(n.value_zat);
            if let Some(w) = want {
                if acc >= w {
                    break;
                }
            }
        }
        Ok(ReceivedNotes::new(vec![], orchard, ironwood))
    }

    fn select_unspent_notes(
        &self,
        account: Self::AccountId,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        self.select_spendable_notes(
            account,
            TargetValue::AllFunds(zcash_client_backend::data_api::MaxSpendMode::MaxSpendable),
            sources,
            target_height,
            ConfirmationsPolicy::MIN,
            exclude,
            lock_filter,
        )
    }

    fn get_account_metadata(
        &self,
        account: Self::AccountId,
        selector: &NoteFilter,
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
        _lock_filter: LockFilter<'_>,
    ) -> Result<AccountMeta, Self::Error> {
        if account != ACCOUNT {
            return Ok(AccountMeta::new(None, None, None));
        }
        let total: u64 = self
            .spend_notes
            .iter()
            .filter(|n| !n.spent)
            .map(|n| n.value_zat)
            .sum();
        let mut o_count = 0usize;
        let mut o_val = 0u64;
        let mut i_count = 0usize;
        let mut i_val = 0u64;
        for n in &self.spend_notes {
            if n.spent
                || exclude.contains(&n.id)
                || !Self::confirmations_ok(
                    n,
                    target_height,
                    crate::confirmations_policy(self.network),
                )
                || !Self::note_filter_ok(n, selector, total)
            {
                continue;
            }
            match n.pool.as_str() {
                "ironwood" => {
                    i_count += 1;
                    i_val = i_val.saturating_add(n.value_zat);
                }
                "sapling" => {}
                _ => {
                    o_count += 1;
                    o_val = o_val.saturating_add(n.value_zat);
                }
            }
        }
        Ok(AccountMeta::new(
            Some(PoolMeta::new(0, Zatoshis::ZERO)),
            Some(PoolMeta::new(
                o_count,
                Zatoshis::from_u64(o_val).unwrap_or(Zatoshis::ZERO),
            )),
            Some(PoolMeta::new(
                i_count,
                Zatoshis::from_u64(i_val).unwrap_or(Zatoshis::ZERO),
            )),
        ))
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_unspent_transparent_output(
        &self,
        outpoint: &transparent::bundle::OutPoint,
        target_height: TargetHeight,
    ) -> Result<Option<WalletTransparentOutput<u32>>, Self::Error> {
        let hx = super::to_hex(outpoint.hash());
        Ok(self
            .utxos
            .iter()
            .find(|u| !u.spent && u.txid == hx && u.index == outpoint.n())
            .filter(|u| {
                self.utxo_confirms(u, target_height, crate::confirmations_policy(self.network))
            })
            .and_then(|u| self.stored_to_wto(u)))
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_spendable_transparent_outputs(
        &self,
        address: &transparent::address::TransparentAddress,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        _output_filter: zcash_client_backend::data_api::CoinbaseFilter,
        _lock_filter: LockFilter<'_>,
    ) -> Result<Vec<WalletTransparentOutput<u32>>, Self::Error> {
        use zcash_keys::encoding::AddressCodec;
        let want = address.encode(&self.network);
        Ok(self
            .utxos
            .iter()
            .filter(|u| {
                !u.spent
                    && (u.address.is_empty() || u.address == want)
                    && self.utxo_confirms(u, target_height, confirmations_policy)
            })
            .filter_map(|u| self.stored_to_wto(u))
            .collect())
    }

    #[cfg(feature = "transparent-inputs")]
    fn select_spendable_transparent_outputs(
        &self,
        account: Self::AccountId,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        output_filter: zcash_client_backend::data_api::CoinbaseFilter,
        address_allow_list: Option<&[transparent::address::TransparentAddress]>,
        target_value: TargetValue,
        max_inputs: usize,
        _fee_rule: &zcash_client_backend::fees::StandardFeeRule,
        lock_filter: LockFilter<'_>,
    ) -> Result<Vec<WalletTransparentOutput<u32>>, Self::Error> {
        if account != ACCOUNT {
            return Ok(vec![]);
        }
        let addrs: Vec<transparent::address::TransparentAddress> = match address_allow_list {
            Some(a) => a.to_vec(),
            None => self.transparent_taddr().into_iter().collect(),
        };
        let mut all = Vec::new();
        for a in &addrs {
            all.extend(self.get_spendable_transparent_outputs(
                a,
                target_height,
                confirmations_policy,
                output_filter,
                lock_filter,
            )?);
        }
        all.sort_by_key(|o| std::cmp::Reverse(u64::from(o.txout().value())));
        let want = match target_value {
            TargetValue::AtLeast(v) => Some(u64::from(v)),
            TargetValue::AllFunds(_) => None,
        };
        let mut acc = 0u64;
        let mut out = Vec::new();
        for o in all {
            if out.len() >= max_inputs {
                break;
            }
            acc = acc.saturating_add(u64::from(o.txout().value()));
            out.push(o);
            if let Some(w) = want {
                if acc >= w {
                    break;
                }
            }
        }
        Ok(out)
    }
}

impl WebWallet {
    fn utxo_confirms(
        &self,
        u: &super::store::StoredUtxo,
        target: TargetHeight,
        policy: ConfirmationsPolicy,
    ) -> bool {
        if u.spent {
            return false;
        }
        self.utxo_ready(u, target, &policy)
    }

    #[cfg(feature = "transparent-inputs")]
    fn transparent_taddr(&self) -> Option<transparent::address::TransparentAddress> {
        use zcash_keys::encoding::AddressCodec;
        let s = self.transparent_address()?;
        transparent::address::TransparentAddress::decode(&self.network, s).ok()
    }

    #[cfg(feature = "transparent-inputs")]
    fn stored_to_wto(&self, u: &super::store::StoredUtxo) -> Option<WalletTransparentOutput<u32>> {
        use transparent::address::Script;
        use transparent::bundle::{OutPoint, TxOut};
        let txid = super::from_hex(&u.txid).ok()?;
        let hash: [u8; 32] = txid.try_into().ok()?;
        let mut script_pubkey = Script::default();
        script_pubkey.0 .0 = super::from_hex(&u.script).unwrap_or_default();
        let value = Zatoshis::from_u64(u.value_zat).ok()?;
        WalletTransparentOutput::from_parts(
            OutPoint::new(hash, u.index),
            TxOut::new(value, script_pubkey),
            Some(BlockHeight::from_u32(u.height)),
            Some(ACCOUNT),
            Some(transparent::keys::TransparentKeyScope::EXTERNAL),
            None,
        )
    }

    #[cfg(feature = "transparent-inputs")]
    fn taddr_metadata() -> TransparentAddressMetadata {
        use transparent::keys::{NonHardenedChildIndex, TransparentKeyScope};
        TransparentAddressMetadata::derived(
            TransparentKeyScope::EXTERNAL,
            NonHardenedChildIndex::ZERO,
            Exposure::Unknown,
            None,
        )
    }

    #[cfg(feature = "transparent-inputs")]
    fn taddr_metadata_for(
        &self,
        addr: &transparent::address::TransparentAddress,
    ) -> TransparentAddressMetadata {
        use transparent::keys::{NonHardenedChildIndex, TransparentKeyScope};
        use zcash_keys::encoding::AddressCodec;
        let encoded = addr.encode(&self.network);
        if let Ok(ufvk) = self.decode_ufvk() {
            if let Some(tpk) = ufvk.transparent() {
                let hi = self.next_diversifier.saturating_add(32).max(32);
                for i in 0..=hi {
                    let Some(idx) = NonHardenedChildIndex::from_index(i) else {
                        break;
                    };
                    if let Ok(pk) = tpk.derive_address_pubkey(TransparentKeyScope::EXTERNAL, idx) {
                        let derived = transparent::address::TransparentAddress::from_pubkey(&pk);
                        if derived.encode(&self.network) == encoded {
                            return TransparentAddressMetadata::derived(
                                TransparentKeyScope::EXTERNAL,
                                idx,
                                Exposure::Unknown,
                                None,
                            );
                        }
                    }
                    if let Ok(pk) = tpk.derive_address_pubkey(TransparentKeyScope::INTERNAL, idx) {
                        let derived = transparent::address::TransparentAddress::from_pubkey(&pk);
                        if derived.encode(&self.network) == encoded {
                            return TransparentAddressMetadata::derived(
                                TransparentKeyScope::INTERNAL,
                                idx,
                                Exposure::Unknown,
                                None,
                            );
                        }
                    }
                }
            }
        }
        Self::taddr_metadata()
    }

    #[cfg(feature = "transparent-inputs")]
    fn known_taddrs(&self) -> Vec<transparent::address::TransparentAddress> {
        use zcash_keys::encoding::AddressCodec;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut push = |s: &str| {
            if s.is_empty() || !seen.insert(s.to_string()) {
                return;
            }
            if let Ok(a) = transparent::address::TransparentAddress::decode(&self.network, s) {
                out.push(a);
            }
        };
        if let Some(s) = self.transparent_address() {
            push(s);
        }
        for u in &self.utxos {
            push(&u.address);
        }
        out
    }
}

impl WalletRead for WebWallet {
    type Error = String;
    type AccountId = u32;
    type Account = WebAccount;

    fn get_account_ids(&self) -> Result<Vec<Self::AccountId>, Self::Error> {
        Ok(vec![ACCOUNT])
    }

    fn get_account(
        &self,
        account_id: Self::AccountId,
    ) -> Result<Option<Self::Account>, Self::Error> {
        if account_id != ACCOUNT {
            return Ok(None);
        }
        self.account().map(Some)
    }

    fn get_derived_account(
        &self,
        derivation: &Zip32Derivation,
    ) -> Result<Option<Self::Account>, Self::Error> {
        match self.hardware_derivation()? {
            Some(ours) if &ours == derivation => self.account().map(Some),
            _ => Ok(None),
        }
    }

    fn validate_seed(
        &self,
        account_id: Self::AccountId,
        seed: &SecretVec<u8>,
    ) -> Result<bool, Self::Error> {
        if account_id != ACCOUNT {
            return Ok(false);
        }
        let zip = Zip32AccountId::try_from(self.account_index).unwrap_or(Zip32AccountId::ZERO);
        let usk = UnifiedSpendingKey::from_seed(&self.network, seed.expose_secret(), zip)
            .map_err(|e| format!("USK: {e:?}"))?;
        Ok(usk.to_unified_full_viewing_key().encode(&self.network) == self.ufvk())
    }

    fn seed_relevance_to_derived_accounts(
        &self,
        _seed: &SecretVec<u8>,
    ) -> Result<SeedRelevance<Self::AccountId>, Self::Error> {
        Ok(SeedRelevance::NoAccounts)
    }

    fn get_account_for_ufvk(
        &self,
        ufvk: &UnifiedFullViewingKey,
    ) -> Result<Option<Self::Account>, Self::Error> {
        if ufvk.encode(&self.network) == self.ufvk() {
            self.account().map(Some)
        } else {
            Ok(None)
        }
    }

    fn list_addresses(&self, account: Self::AccountId) -> Result<Vec<AddressInfo>, Self::Error> {
        if account != ACCOUNT {
            return Ok(vec![]);
        }
        let addr = Address::decode(&self.network, self.unified_address())
            .ok_or_else(|| "decode UA".to_string())?;
        let info = AddressInfo::from_parts(
            addr,
            AddressSource::Derived {
                diversifier_index: DiversifierIndex::from(self.next_diversifier.saturating_sub(1)),
                #[cfg(feature = "transparent-inputs")]
                transparent_key_scope: None,
            },
        )
        .ok_or_else(|| "address info".to_string())?;
        Ok(vec![info])
    }

    fn find_account_for_address<P: consensus::Parameters>(
        &self,
        params: &P,
        address: &Address,
    ) -> Result<Option<Self::AccountId>, FindAccountForAddressError<Self::Error>> {
        find_account_for_address(self, params, address)
    }

    fn get_last_generated_address_matching(
        &self,
        account: Self::AccountId,
        _address_filter: UnifiedAddressRequest,
    ) -> Result<Option<UnifiedAddress>, Self::Error> {
        if account != ACCOUNT {
            return Ok(None);
        }
        match Address::decode(&self.network, self.unified_address()) {
            Some(Address::Unified(ua)) => Ok(Some(ua)),
            _ => Ok(None),
        }
    }

    fn get_account_birthday(&self, account: Self::AccountId) -> Result<BlockHeight, Self::Error> {
        if account != ACCOUNT {
            return Err("unknown account".into());
        }
        Ok(BlockHeight::from_u32(self.birthday()))
    }

    fn get_wallet_birthday(&self) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(Some(BlockHeight::from_u32(self.birthday())))
    }

    fn get_wallet_recover_until(&self) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(None)
    }

    fn get_wallet_summary(
        &self,
        _confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Option<zcash_client_backend::data_api::WalletSummary<Self::AccountId>>, Self::Error>
    {
        Ok(None)
    }

    fn chain_height(&self) -> Result<Option<BlockHeight>, Self::Error> {
        if self.scanned_height() == 0 {
            Ok(None)
        } else {
            Ok(Some(BlockHeight::from_u32(self.scanned_height())))
        }
    }

    fn get_block_hash(&self, block_height: BlockHeight) -> Result<Option<BlockHash>, Self::Error> {
        let Some(p) = &self.prior else {
            return Ok(None);
        };
        if BlockHeight::from_u32(p.height) == block_height {
            let b = super::from_hex(&p.hash).map_err(|e| e)?;
            if b.len() == 32 {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                return Ok(Some(BlockHash(h)));
            }
        }
        Ok(None)
    }

    fn block_metadata(&self, height: BlockHeight) -> Result<Option<BlockMetadata>, Self::Error> {
        self.prior_metadata()
            .map_err(|e| e.to_string())
            .map(|p| p.filter(|m| m.block_height() == height))
    }

    fn block_fully_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        self.prior_metadata().map_err(|e| e.to_string())
    }

    fn get_max_height_hash(&self) -> Result<Option<(BlockHeight, BlockHash)>, Self::Error> {
        let Some(p) = &self.prior else {
            return Ok(None);
        };
        let b = super::from_hex(&p.hash).map_err(|e| e)?;
        if b.len() != 32 {
            return Ok(None);
        }
        let mut h = [0u8; 32];
        h.copy_from_slice(&b);
        Ok(Some((BlockHeight::from_u32(p.height), BlockHash(h))))
    }

    fn block_max_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        self.block_fully_scanned()
    }

    fn suggest_scan_ranges(&self) -> Result<Vec<ScanRange>, Self::Error> {
        Ok(vec![])
    }

    fn get_target_and_anchor_heights(
        &self,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<(TargetHeight, BlockHeight)>, Self::Error> {
        let tip = self.scanned_height();
        if tip == 0 {
            return Ok(None);
        }
        let target = TargetHeight::from(tip.saturating_add(1));
        let maximum = target.saturating_sub(u32::from(min_confirmations));
        Ok(self
            .spend_anchor_height(maximum)
            .map(|anchor| (target, anchor)))
    }

    fn get_tx_height(&self, txid: TxId) -> Result<Option<BlockHeight>, Self::Error> {
        let hex = super::to_hex(txid.as_ref());
        Ok(self
            .spend_notes
            .iter()
            .find(|n| n.txid == hex)
            .map(|n| BlockHeight::from_u32(n.mined_height)))
    }

    fn get_unified_full_viewing_keys(
        &self,
    ) -> Result<HashMap<Self::AccountId, UnifiedFullViewingKey>, Self::Error> {
        let ufvk = self.decode_ufvk().map_err(|e| e.to_string())?;
        Ok(HashMap::from([(ACCOUNT, ufvk)]))
    }

    fn get_memo(&self, _note_id: NoteId) -> Result<Option<Memo>, Self::Error> {
        Ok(None)
    }

    fn get_transaction(&self, txid: TxId) -> Result<Option<Transaction>, Self::Error> {
        let Some(bytes) = self.pending_txs.get(&txid) else {
            return Ok(None);
        };
        let height = BlockHeight::from_u32(self.scanned_height().saturating_add(1));
        let branch = zcash_protocol::consensus::BranchId::for_height(&self.network, height);
        Transaction::read(&bytes[..], branch)
            .map(Some)
            .map_err(|e| format!("read tx: {e}"))
    }

    fn get_sapling_nullifiers(
        &self,
        _query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, sapling::Nullifier)>, Self::Error> {
        Ok(vec![])
    }

    fn get_orchard_nullifiers(
        &self,
        query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, orchard::note::Nullifier)>, Self::Error> {
        self.nullifiers("orchard", query)
    }

    fn get_ironwood_nullifiers(
        &self,
        query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, orchard::note::Nullifier)>, Self::Error> {
        self.nullifiers("ironwood", query)
    }

    fn transaction_data_requests(&self) -> Result<Vec<TransactionDataRequest>, Self::Error> {
        Ok(vec![])
    }

    fn get_received_outputs(
        &self,
        txid: TxId,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Vec<ReceivedTransactionOutput>, Self::Error> {
        let hex = super::to_hex(txid.as_ref());
        let mut out = Vec::new();
        for n in &self.spend_notes {
            if n.txid != hex {
                continue;
            }
            let need = if n.is_change {
                u32::from(confirmations_policy.trusted())
            } else {
                u32::from(confirmations_policy.untrusted())
            };
            let have = u32::from(target_height).saturating_sub(n.mined_height);
            let until = need.saturating_sub(have);
            out.push(ReceivedTransactionOutput::from_parts(
                PoolType::Shielded(Self::pool_of(n)),
                n.output_index as usize,
                Zatoshis::from_u64(n.value_zat).unwrap_or(Zatoshis::ZERO),
                until,
            ));
        }
        Ok(out)
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_transparent_receivers(
        &self,
        account: Self::AccountId,
        _include_change: bool,
        _include_standalone: bool,
    ) -> Result<
        HashMap<transparent::address::TransparentAddress, TransparentAddressMetadata>,
        Self::Error,
    > {
        if account != ACCOUNT {
            return Ok(HashMap::new());
        }
        Ok(self
            .known_taddrs()
            .into_iter()
            .map(|a| {
                let meta = self.taddr_metadata_for(&a);
                (a, meta)
            })
            .collect())
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_ephemeral_transparent_receivers(
        &self,
        _account: Self::AccountId,
        _exposure_depth: u32,
        _exclude_used: bool,
    ) -> Result<
        HashMap<transparent::address::TransparentAddress, TransparentAddressMetadata>,
        Self::Error,
    > {
        Ok(HashMap::new())
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_transparent_balances(
        &self,
        account: Self::AccountId,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<TransparentBalances, Self::Error> {
        use zcash_keys::encoding::AddressCodec;
        if account != ACCOUNT {
            return Ok(HashMap::new());
        }
        let mut map = HashMap::new();
        for addr in self.known_taddrs() {
            let want = addr.encode(&self.network);
            let mut bal = zcash_client_backend::data_api::Balance::ZERO;
            for u in &self.utxos {
                if u.spent {
                    continue;
                }
                if !u.address.is_empty() && u.address != want {
                    continue;
                }
                if u.address.is_empty() && self.transparent_address() != Some(want.as_str()) {
                    continue;
                }
                let v = Zatoshis::from_u64(u.value_zat).unwrap_or(Zatoshis::ZERO);
                if self.utxo_confirms(u, target_height, confirmations_policy) {
                    let _ = bal.add_spendable_value(v);
                } else {
                    let _ = bal.add_pending_spendable_value(v);
                }
            }
            map.insert(
                addr,
                (
                    TransparentKeyOrigin::Derived {
                        scope: transparent::keys::TransparentKeyScope::EXTERNAL,
                    },
                    bal,
                ),
            );
        }
        Ok(map)
    }

    #[cfg(feature = "transparent-inputs")]
    fn get_transparent_address_metadata(
        &self,
        account: Self::AccountId,
        address: &transparent::address::TransparentAddress,
    ) -> Result<Option<TransparentAddressMetadata>, Self::Error> {
        if account != ACCOUNT {
            return Ok(None);
        }
        Ok(self
            .known_taddrs()
            .into_iter()
            .find(|a| a == address)
            .map(|a| self.taddr_metadata_for(&a)))
    }

    #[cfg(feature = "transparent-inputs")]
    fn utxo_query_height(&self, account: Self::AccountId) -> Result<BlockHeight, Self::Error> {
        if account != ACCOUNT {
            return Err("unknown account".into());
        }
        let h = self
            .utxos
            .iter()
            .filter(|u| !u.spent)
            .map(|u| u.height)
            .min()
            .unwrap_or_else(|| self.birthday().saturating_sub(1));
        Ok(BlockHeight::from_u32(h))
    }
}

impl WebWallet {
    fn nullifiers(
        &self,
        pool: &str,
        query: NullifierQuery,
    ) -> Result<Vec<(u32, orchard::note::Nullifier)>, String> {
        let mut out = Vec::new();
        for n in &self.spend_notes {
            if n.pool != pool || n.nf.is_empty() {
                continue;
            }
            if matches!(query, NullifierQuery::Unspent) && n.spent {
                continue;
            }
            let b = super::from_hex(&n.nf).map_err(|e| e)?;
            if b.len() != 32 {
                continue;
            }
            let mut nf = [0u8; 32];
            nf.copy_from_slice(&b);
            if let Some(parsed) = Option::from(orchard::note::Nullifier::from_bytes(&nf)) {
                out.push((ACCOUNT, parsed));
            }
        }
        Ok(out)
    }
}

fn lock_pool_name(pool: PoolType) -> &'static str {
    match pool {
        PoolType::Transparent => "transparent",
        PoolType::SAPLING => "sapling",
        PoolType::IRONWOOD => "ironwood",
        _ => "orchard",
    }
}

impl WebWallet {
    /// Owner (hex) of the unexpired lock on an output, if any. Spent outputs
    /// carry no meaningful lock.
    pub(crate) fn active_lock_owner(&self, txid_hex: &str, pool: &str, index: u32) -> Option<&str> {
        let next = self.next_height();
        self.output_locks
            .iter()
            .find(|l| l.txid == txid_hex && l.pool == pool && l.index == index && l.expiry >= next)
            .map(|l| l.owner.as_str())
    }

    fn lock_admits(&self, n: &StoredNote, filter: &LockFilter<'_>) -> bool {
        match filter {
            LockFilter::Unfiltered => true,
            LockFilter::Policy(policy) => {
                match self.active_lock_owner(&n.txid, &n.pool, u32::from(n.output_index)) {
                    None => true,
                    Some(owner) => policy
                        .overridable_owners()
                        .iter()
                        .any(|o| super::to_hex(o.as_bytes()) == owner),
                }
            }
        }
    }

    /// Drop locks that expired or whose output was spent.
    fn prune_locks(&mut self) {
        let next = self.next_height();
        let spent: std::collections::BTreeSet<(String, String, u32)> = self
            .spend_notes
            .iter()
            .filter(|n| n.spent)
            .map(|n| (n.txid.clone(), n.pool.clone(), u32::from(n.output_index)))
            .collect();
        self.output_locks.retain(|l| {
            l.expiry >= next && !spent.contains(&(l.txid.clone(), l.pool.clone(), l.index))
        });
    }
}

impl OutputLockStore for WebWallet {
    type Error = String;
    type AccountId = u32;

    fn lock_outputs(
        &mut self,
        outputs: &[OutputRef],
        owner: LockOwner,
        lock_expiry_height: BlockHeight,
    ) -> Result<usize, LockError<Self::Error>> {
        self.prune_locks();
        let owner_hex = super::to_hex(owner.as_bytes());
        // All or nothing: refuse before changing anything if another owner holds any.
        for o in outputs {
            let txid = super::to_hex(o.txid().as_ref());
            if let Some(held) =
                self.active_lock_owner(&txid, lock_pool_name(o.pool()), o.output_index())
            {
                if held != owner_hex {
                    return Err(LockError::LockFailure(o.clone()));
                }
            }
        }
        for o in outputs {
            let txid = super::to_hex(o.txid().as_ref());
            let pool = lock_pool_name(o.pool());
            let index = o.output_index();
            self.output_locks
                .retain(|l| !(l.txid == txid && l.pool == pool && l.index == index));
            self.output_locks.push(StoredLock {
                txid,
                pool: pool.into(),
                index,
                owner: owner_hex.clone(),
                expiry: u32::from(lock_expiry_height),
            });
        }
        Ok(outputs.len())
    }

    fn unlock_output(&mut self, output: &OutputRef, owner: LockOwner) -> Result<bool, Self::Error> {
        let txid = super::to_hex(output.txid().as_ref());
        let pool = lock_pool_name(output.pool());
        let owner_hex = super::to_hex(owner.as_bytes());
        let before = self.output_locks.len();
        self.output_locks.retain(|l| {
            !(l.txid == txid
                && l.pool == pool
                && l.index == output.output_index()
                && l.owner == owner_hex)
        });
        Ok(self.output_locks.len() != before)
    }

    fn clear_locked_outputs(&mut self, account: Self::AccountId) -> Result<usize, Self::Error> {
        if account != ACCOUNT {
            return Ok(0);
        }
        let n = self.output_locks.len();
        self.output_locks.clear();
        Ok(n)
    }

    fn get_locked_outputs(&self, account: Self::AccountId) -> Result<Vec<OutputRef>, Self::Error> {
        if account != ACCOUNT {
            return Ok(vec![]);
        }
        let next = self.next_height();
        self.output_locks
            .iter()
            .filter(|l| l.expiry >= next)
            .map(|l| {
                let pool = match l.pool.as_str() {
                    "transparent" => PoolType::Transparent,
                    "sapling" => PoolType::SAPLING,
                    "ironwood" => PoolType::IRONWOOD,
                    _ => PoolType::ORCHARD,
                };
                Ok(OutputRef::new(
                    Self::txid_from_hex(&l.txid).map_err(|e| e.to_string())?,
                    pool,
                    l.index,
                ))
            })
            .collect()
    }
}

impl WalletWrite for WebWallet {
    type UtxoRef = u32;

    fn create_account(
        &mut self,
        _account_name: &str,
        _seed: &SecretVec<u8>,
        _birthday: &AccountBirthday,
        _key_source: Option<&str>,
    ) -> Result<(u32, UnifiedSpendingKey), String> {
        Err("web wallet is single-account".into())
    }

    fn import_account_hd(
        &mut self,
        _account_name: &str,
        _seed: &SecretVec<u8>,
        _account_index: Zip32AccountId,
        _birthday: &AccountBirthday,
        _key_source: Option<&str>,
    ) -> Result<(Self::Account, UnifiedSpendingKey), String> {
        Err("web wallet is single-account".into())
    }

    fn import_account_ufvk(
        &mut self,
        _account_name: &str,
        _unified_key: &UnifiedFullViewingKey,
        _birthday: &AccountBirthday,
        _purpose: AccountPurpose,
        _key_source: Option<&str>,
    ) -> Result<Self::Account, String> {
        Err("web wallet is single-account".into())
    }

    fn delete_account(&mut self, _account: u32) -> Result<(), String> {
        Err("web wallet is single-account".into())
    }

    fn get_next_available_address(
        &mut self,
        account: u32,
        _request: UnifiedAddressRequest,
    ) -> Result<Option<(UnifiedAddress, DiversifierIndex)>, String> {
        if account != ACCOUNT {
            return Ok(None);
        }
        let ua = self.next_unified_address().map_err(|e| e.to_string())?;
        match Address::decode(&self.network, &ua) {
            Some(Address::Unified(u)) => Ok(Some((
                u,
                DiversifierIndex::from(self.next_diversifier.saturating_sub(1)),
            ))),
            _ => Ok(None),
        }
    }

    fn get_address_for_index(
        &mut self,
        account: u32,
        diversifier_index: DiversifierIndex,
        request: UnifiedAddressRequest,
    ) -> Result<Option<UnifiedAddress>, String> {
        if account != ACCOUNT {
            return Ok(None);
        }
        let ufvk = self.decode_ufvk().map_err(|e| e.to_string())?;
        match ufvk.find_address(diversifier_index, request) {
            Ok((ua, _)) => Ok(Some(ua)),
            Err(_) => Ok(None),
        }
    }

    fn update_chain_tip(&mut self, _tip_height: BlockHeight) -> Result<(), String> {
        Ok(())
    }

    fn prune_scan_queue_below(
        &mut self,
        _height: BlockHeight,
        _retain_with_priority: Option<ScanPriority>,
    ) -> Result<u64, String> {
        Ok(0)
    }

    fn put_blocks(
        &mut self,
        _from_state: &ChainState,
        _blocks: Vec<zcash_client_backend::data_api::ScannedBlock<u32>>,
    ) -> Result<(), String> {
        Err("web wallet scans compact blocks directly".into())
    }

    fn put_received_transparent_utxo(
        &mut self,
        output: &WalletTransparentOutput<u32>,
    ) -> Result<Self::UtxoRef, String> {
        let txid = super::to_hex(output.outpoint().hash());
        let index = output.outpoint().n();
        if let Some((i, existing)) = self
            .utxos
            .iter_mut()
            .enumerate()
            .find(|(_, u)| u.txid == txid && u.index == index)
        {
            if !existing.spent {
                existing.height = output
                    .mined_height()
                    .map(u32::from)
                    .unwrap_or(existing.height);
                existing.value_zat = u64::from(output.txout().value());
                existing.script = super::to_hex(&output.txout().script_pubkey().0 .0);
            }
            return Ok(i as u32);
        }
        #[cfg(feature = "transparent-inputs")]
        let address = {
            use zcash_keys::encoding::AddressCodec;
            output.recipient_address().encode(&self.network)
        };
        #[cfg(not(feature = "transparent-inputs"))]
        let address = String::new();
        self.utxos.push(super::store::StoredUtxo {
            coinbase: false,
            txid,
            index,
            script: super::to_hex(&output.txout().script_pubkey().0 .0),
            value_zat: u64::from(output.txout().value()),
            height: output.mined_height().map(u32::from).unwrap_or(0),
            address,
            spent: false,
            spent_in: None,
        });
        Ok(self.utxos.len() as u32 - 1)
    }

    fn store_decrypted_tx(
        &mut self,
        _received_tx: zcash_client_backend::data_api::DecryptedTransaction<Transaction, u32>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn set_tx_trust(&mut self, _txid: TxId, _trusted: bool) -> Result<(), String> {
        Ok(())
    }

    fn store_transactions_to_be_sent(
        &mut self,
        transactions: &[SentTransaction<u32>],
    ) -> Result<(), String> {
        use zcash_client_backend::wallet::Recipient;
        for sent in transactions {
            self.store_pending_tx(sent.tx())
                .map_err(|e| e.to_string())?;
            let txid = super::to_hex(sent.tx().txid().as_ref());
            let mut spent_zat = 0u64;
            let mut spent_notes = 0u32;
            let mut spent_t = 0u64;
            let mut orchard_spent = 0u64;
            let mut ironwood_spent = 0u64;
            // Reserve inputs from both Orchard-protocol bundles: after NU6.3
            // our Ironwood notes are spent in the Ironwood bundle.
            let actions = sent
                .tx()
                .orchard_bundle()
                .into_iter()
                .chain(sent.tx().ironwood_bundle())
                .flat_map(|bundle| bundle.actions().iter());
            for action in actions {
                let nf = super::to_hex(&action.nullifier().to_bytes());
                for n in &mut self.spend_notes {
                    if !n.spent && n.nf == nf {
                        n.spent = true;
                        n.spent_in = Some(txid.clone());
                        spent_zat = spent_zat.saturating_add(n.value_zat);
                        if n.pool == "ironwood" {
                            ironwood_spent = ironwood_spent.saturating_add(n.value_zat);
                        } else {
                            orchard_spent = orchard_spent.saturating_add(n.value_zat);
                        }
                        spent_notes += 1;
                    }
                }
                for n in self.notes_mut() {
                    if !n.spent && n.nf == nf {
                        n.spent = true;
                        n.spent_in = Some(txid.clone());
                    }
                }
            }
            #[cfg(feature = "transparent-inputs")]
            if let Some(tb) = sent.tx().transparent_bundle() {
                for vin in &tb.vin {
                    let prev = vin.prevout();
                    let hx = super::to_hex(prev.hash());
                    let idx = prev.n();
                    for u in &mut self.utxos {
                        if !u.spent && u.txid == hx && u.index == idx {
                            u.spent = true;
                            u.spent_in = Some(txid.clone());
                            spent_t = spent_t.saturating_add(u.value_zat);
                            spent_zat = spent_zat.saturating_add(u.value_zat);
                            spent_notes += 1;
                        }
                    }
                }
            }
            let mut received_zat = 0u64;
            let mut received_notes = 0u32;
            let mut ironwood_received = 0u64;
            let mut has_change = false;
            let mut memos = Vec::new();
            for o in sent.outputs() {
                match o.recipient() {
                    Recipient::InternalShielded { note, .. } => {
                        received_zat = received_zat.saturating_add(u64::from(o.value()));
                        received_notes += 1;
                        has_change = true;
                        if matches!(
                            note.as_ref(),
                            zcash_client_backend::wallet::Note::Orchard {
                                pool: orchard::ValuePool::Ironwood,
                                ..
                            }
                        ) {
                            ironwood_received =
                                ironwood_received.saturating_add(u64::from(o.value()));
                        }
                    }
                    #[cfg(feature = "transparent-inputs")]
                    Recipient::InternalTransparent { .. }
                    | Recipient::EphemeralTransparent { .. } => {
                        received_zat = received_zat.saturating_add(u64::from(o.value()));
                        received_notes += 1;
                    }
                    Recipient::External { .. } => {}
                }
                if let Some(mb) = o.memo() {
                    if let Ok(Memo::Text(t)) = Memo::try_from(mb) {
                        memos.push(t.to_string());
                    }
                }
            }
            let agg = self.txs_mut().entry(txid).or_default();
            agg.mined_height = None;
            agg.spent_zat = agg.spent_zat.saturating_add(spent_zat);
            agg.spent_notes = agg.spent_notes.saturating_add(spent_notes);
            agg.received_zat = agg.received_zat.saturating_add(received_zat);
            agg.received_notes = agg.received_notes.saturating_add(received_notes);
            agg.has_change = agg.has_change || has_change;
            agg.is_shielding = agg.is_shielding || (spent_t > 0 && received_zat > 0);
            agg.fee_zat = Some(u64::from(sent.fee_amount()));
            agg.expiry_height = Some(u32::from(sent.tx().expiry_height()));
            agg.transparent_spent = agg.transparent_spent.saturating_add(spent_t);
            agg.orchard_spent = agg.orchard_spent.saturating_add(orchard_spent);
            agg.ironwood_spent = agg.ironwood_spent.saturating_add(ironwood_spent);
            agg.ironwood_received = agg.ironwood_received.saturating_add(ironwood_received);
            agg.orchard_received = agg
                .orchard_received
                .saturating_add(received_zat - ironwood_received);
            if !memos.is_empty() {
                agg.memos.extend(memos);
            }
        }
        // Unlock-on-store: the spend records now guard these notes.
        self.prune_locks();
        Ok(())
    }

    fn truncate_to_height(&mut self, max_height: BlockHeight) -> Result<BlockHeight, String> {
        let h = u32::from(max_height).min(self.scanned_height());
        self.rewind_to_height(h).map_err(|e| e.to_string())?;
        Ok(BlockHeight::from_u32(self.scanned_height()))
    }

    fn truncate_to_chain_state(&mut self, chain_state: ChainState) -> Result<(), String> {
        let h = u32::from(chain_state.block_height());
        self.rewind_to_height(h).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn rewind_to_chain_state(
        &mut self,
        chain_state: ChainState,
        _reset_account_birthdays: HashSet<u32>,
    ) -> Result<(), RewindError<u32, String>> {
        let h = u32::from(chain_state.block_height());
        self.rewind_to_height(h)
            .map(|_| ())
            .map_err(|e| RewindError::DataSource(e.to_string()))
    }

    // Snapshot enhancement is owned by the SDK's durable memo queue, not WalletRead.
    fn notify_transaction_enhancement_not_found(&mut self, _txid: TxId) -> Result<(), String> {
        Ok(())
    }

    fn set_transaction_status(
        &mut self,
        _txid: TxId,
        _status: TransactionStatus,
    ) -> Result<(), String> {
        Ok(())
    }

    #[cfg(feature = "transparent-inputs")]
    fn reserve_next_n_ephemeral_addresses(
        &mut self,
        _account_id: u32,
        n: usize,
    ) -> Result<
        Vec<(
            transparent::address::TransparentAddress,
            TransparentAddressMetadata,
        )>,
        String,
    > {
        if n == 0 {
            return Ok(vec![]);
        }
        Err("wasm snapshot wallet has no ephemeral t-addrs".into())
    }

    #[cfg(feature = "transparent-inputs")]
    fn reserve_next_n_internal_addresses(
        &mut self,
        _account_id: u32,
        n: usize,
    ) -> Result<
        Vec<(
            transparent::address::TransparentAddress,
            TransparentAddressMetadata,
        )>,
        String,
    > {
        if n == 0 {
            return Ok(vec![]);
        }
        Err("wasm snapshot wallet has no internal t-addrs".into())
    }

    #[cfg(feature = "transparent-inputs")]
    fn schedule_next_check(
        &mut self,
        _address: &transparent::address::TransparentAddress,
        _offset_seconds: u32,
    ) -> Result<Option<SystemTime>, String> {
        Ok(None)
    }

    #[cfg(feature = "transparent-inputs")]
    fn mark_transparent_addresses_exposed(
        &mut self,
        _exposures: &[(transparent::address::TransparentAddress, BlockHeight)],
    ) -> Result<(), String> {
        Ok(())
    }

    #[cfg(feature = "transparent-inputs")]
    fn notify_address_checked(
        &mut self,
        _request: TransactionsInvolvingAddress,
        _as_of_height: BlockHeight,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl WalletCommitmentTrees for WebWallet {
    type Error = Infallible;
    type SaplingShardStore<'a> = MemoryShardStore<sapling::Node, BlockHeight>;
    type OrchardShardStore<'a> = MemoryShardStore<orchard::tree::MerkleHashOrchard, BlockHeight>;

    fn with_sapling_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(&'a mut ShardTreeWrap<'_>) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        callback(&mut self.sapling_tree)
    }

    fn put_sapling_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<sapling::Node>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        self.with_sapling_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr =
                    MerkleAddress::from_parts(SAPLING_SHARD_HEIGHT.into(), start_index + i);
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok(())
        })
    }

    fn get_sapling_subtree_root(
        &mut self,
        index: u64,
    ) -> Result<Option<sapling::Node>, ShardTreeError<Self::Error>> {
        self.with_sapling_tree_mut(|t| {
            let addr = MerkleAddress::from_parts(SAPLING_SHARD_HEIGHT.into(), index);
            Ok(t.store()
                .get_shard(addr)
                .map_err(ShardTreeError::Storage)?
                .and_then(|shard| match shard.root() {
                    tree if tree.is_leaf() => tree.leaf_value().copied(),
                    tree => tree.annotation().and_then(|ann| ann.as_deref().copied()),
                }))
        })
    }

    fn with_orchard_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(&'a mut OrchardTree) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        callback(&mut self.orchard_tree)
    }

    fn put_orchard_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<orchard::tree::MerkleHashOrchard>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        self.with_orchard_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr =
                    MerkleAddress::from_parts(ORCHARD_SHARD_HEIGHT.into(), start_index + i);
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok(())
        })
    }

    fn get_orchard_subtree_root(
        &mut self,
        index: u64,
    ) -> Result<Option<orchard::tree::MerkleHashOrchard>, ShardTreeError<Self::Error>> {
        self.with_orchard_tree_mut(|t| {
            let addr = MerkleAddress::from_parts(ORCHARD_SHARD_HEIGHT.into(), index);
            Ok(t.store()
                .get_shard(addr)
                .map_err(ShardTreeError::Storage)?
                .and_then(|shard| match shard.root() {
                    tree if tree.is_leaf() => tree.leaf_value().copied(),
                    tree => tree.annotation().and_then(|ann| ann.as_deref().copied()),
                }))
        })
    }

    fn with_ironwood_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<Option<A>, E>
    where
        for<'a> F: FnMut(&'a mut OrchardTree) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        callback(&mut self.ironwood_tree).map(Some)
    }
}

// Helper alias so the sapling callback signature matches the trait.
type ShardTreeWrap<'a> = SaplingTree;
