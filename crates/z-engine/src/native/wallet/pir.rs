//! Experimental regtest confirmed discovery. No event reaches native coin
//! selection, signing, balances, history views or transparent fallback RPCs.
#[path = "pir_store.rs"]
mod store;
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use transparent_filter::ShardMap;
use transparent_wallet::{client::Table, transport::BoxError, *};
use zcash_client_backend::data_api::WalletRead;

const GENESIS: &str = "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";
fn failed(code: &str) -> EngineError {
    EngineError::Message(code.into())
}
fn store_failed(error: StoreError) -> EngineError {
    match error {
        StoreError::Corrupt(_) => failed("native_pir_store_invalid"),
        StoreError::Io(ref s) if s.starts_with("native_pir_") => failed(s),
        _ => failed("native_pir_store_failed"),
    }
}

/// A caller supplies hashes from its independently accepted local regtest node.
/// This constructor validates shape/network; it does not authenticate that node.
/// Publisher hashes, map endpoints and cloud-scanned DB hashes are not provenance.
#[derive(Clone)]
pub struct RegtestAcceptedChain {
    hashes: BTreeMap<u64, String>,
    target: Anchor,
    context_identity: Option<String>,
}
impl RegtestAcceptedChain {
    pub fn from_local_node(
        schedule: RegtestScanSchedule,
        headers: Vec<(u32, String)>,
    ) -> Result<Self> {
        if schedule.nu6_3_height != crate::regtest_nu6_3_height()
            || schedule.nu7_height != crate::regtest_nu7_height()
            || headers.len() < 2
            || headers.len() > 2049
        {
            return Err(failed("native_pir_chain_invalid"));
        }
        let mut hashes = BTreeMap::new();
        for (expected, (height, hash)) in headers.into_iter().enumerate() {
            if height as usize != expected
                || hash.len() != 64
                || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(failed("native_pir_chain_invalid"));
            }
            hashes.insert(u64::from(height), hash.to_ascii_lowercase());
        }
        if hashes.get(&0).map(String::as_str) != Some(GENESIS) {
            return Err(failed("native_pir_chain_invalid"));
        }
        let (height, hash) = hashes
            .last_key_value()
            .ok_or_else(|| failed("native_pir_chain_invalid"))?;
        let target = Anchor {
            height: *height,
            hash: hash.clone(),
        };
        Ok(Self {
            hashes,
            target,
            context_identity: None,
        })
    }
    /// Bind the whole enrolled transport/source configuration to this accepted snapshot.
    /// The caller supplies a canonical digest, not endpoint strings or secret material.
    pub fn with_context_identity(mut self, identity: &str) -> Result<Self> {
        if identity.len() != 64
            || !identity
                .bytes()
                .all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
        {
            return Err(failed("native_pir_context_invalid"));
        }
        let bytes = serde_json::to_vec(&(&self.hashes, identity))
            .map_err(|_| failed("native_pir_context_invalid"))?;
        self.context_identity = Some(format!("{:x}", Sha256::digest(bytes)));
        Ok(self)
    }
    pub fn target_height(&self) -> u32 {
        self.target.height as u32
    }
    pub fn target_hash(&self) -> &str {
        &self.target.hash
    }
}
impl ChainView for RegtestAcceptedChain {
    fn is_accepted(&self, h: u64, hash: &str) -> Acceptance {
        match self.hashes.get(&h) {
            Some(known) if known == hash => Acceptance::Accepted,
            Some(_) => Acceptance::Rejected,
            None => Acceptance::Unknown,
        }
    }
    fn tip(&self) -> Option<Anchor> {
        Some(self.target.clone())
    }
    fn hash_at(&self, h: u64) -> Option<String> {
        self.hashes.get(&h).cloned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirDiscoveryReport {
    /// `complete-for-enrolled-scope` never asserts wallet-wide gap discovery.
    pub completion: String,
    pub reconciled: bool,
    pub accepted_anchor: Option<Anchor>,
    pub source_identity: String,
    pub context_identity: Option<String>,
    pub scope_identity: String,
    pub enrolled_script_count: usize,
    pub covered_through: u64,
    pub settled_through: u64,
    /// Confirmed ledger arithmetic only; never a spendable native balance.
    pub confirmed_ledger_zat: u64,
    pub history: Vec<PirConfirmedTransaction>,
    pub pending_pages: usize,
    pub unresolved_spends: usize,
    pub native_scanned_height: u32,
    pub native_chain_height: Option<u32>,
    pub native_anchor_hash: Option<String>,
    /// Missing on older reports, which require a fresh successful reconciliation.
    #[serde(default)]
    pub native_scanner_identity: Option<String>,
    pub publication_start: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirConfirmedTransaction {
    pub txid: String,
    pub height: u32,
    pub received_zat: u64,
    pub spent_zat: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ScopeEntry {
    account: String,
    entry: ScriptEntry,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NativeScope(Vec<ScopeEntry>);
impl NativeScope {
    pub(super) fn continues(&self, old: &Self) -> bool {
        old.0.iter().all(|prior| {
            self.0.iter().any(|next| {
                next.account == prior.account
                    && next.entry.script == prior.entry.script
                    && next.entry.origin == prior.entry.origin
                    && next.entry.required_from <= prior.entry.required_from
            })
        })
    }
    fn unique_script_count(&self) -> usize {
        self.0
            .iter()
            .map(|e| e.entry.script.as_slice())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }
    fn scripts(&self) -> Vec<ScriptEntry> {
        self.0.iter().map(|e| e.entry.clone()).collect()
    }
    fn identity(&self) -> Result<String> {
        serde_json::to_vec(self)
            .map(|b| format!("{:x}", Sha256::digest(b)))
            .map_err(|_| failed("native_pir_scope_invalid"))
    }
}
pub(super) fn native_scope<D: WalletRead<AccountId = AccountUuid>>(
    db: &D,
    start: u64,
) -> std::result::Result<NativeScope, StoreError> {
    let invalid = || StoreError::Io("native_pir_scope_invalid".into());
    let mut scope = Vec::new();
    for account in db.get_account_ids().map_err(|_| invalid())? {
        let birthday = u64::from(u32::from(
            db.get_account_birthday(account).map_err(|_| invalid())?,
        ));
        for (address, metadata) in db
            .get_transparent_receivers(account, true, true)
            .map_err(|_| invalid())?
        {
            let imported = metadata.scope().is_none();
            let required_from = receiver_history_floor(imported, birthday, start)?;
            scope.push(ScopeEntry {
                account: account.expose_uuid().to_string(),
                entry: ScriptEntry {
                    script: {
                        let s: transparent::address::Script = address.script().into();
                        s.0 .0
                    },
                    origin: if imported {
                        ScriptOrigin::Imported
                    } else {
                        ScriptOrigin::Derived
                    },
                    required_from,
                },
            });
        }
    }
    scope.sort_by(|a, b| {
        a.entry
            .script
            .cmp(&b.entry.script)
            .then(a.account.cmp(&b.account))
    });
    if scope.is_empty() || scope.len() > 4096 {
        return Err(invalid());
    }
    Ok(NativeScope(scope))
}

fn receiver_history_floor(
    imported: bool,
    birthday: u64,
    publication_start: u64,
) -> std::result::Result<u64, StoreError> {
    // Standalone receiver metadata has no trustworthy creation-height bound.
    let floor = if imported { 1 } else { birthday };
    if floor < publication_start {
        Err(StoreError::Io("native_pir_scope_invalid".into()))
    } else {
        Ok(floor)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct NativeScanSnapshot {
    anchor_height: u32,
    anchor_hash: Option<String>,
    chain_height: Option<u32>,
    fully_scanned: Option<(u32, String)>,
    max_scanned: Option<(u32, String)>,
    scan_ranges: Vec<(u32, u32, String)>,
}
impl NativeScanSnapshot {
    pub(super) fn capture<D: WalletRead>(
        db: &D,
        anchor_height: u32,
    ) -> std::result::Result<Self, StoreError> {
        let invalid = || StoreError::Io("native_pir_database_failed".into());
        let block = |b: zcash_client_backend::data_api::BlockMetadata| {
            (u32::from(b.block_height()), b.block_hash().to_string())
        };
        let mut scan_ranges = db
            .suggest_scan_ranges()
            .map_err(|_| invalid())?
            .into_iter()
            .map(|r| {
                (
                    u32::from(r.block_range().start),
                    u32::from(r.block_range().end),
                    format!("{:?}", r.priority()),
                )
            })
            .collect::<Vec<_>>();
        scan_ranges.sort();
        Ok(Self {
            anchor_height,
            anchor_hash: db
                .get_block_hash(anchor_height.into())
                .map_err(|_| invalid())?
                .map(|h| h.to_string()),
            chain_height: db.chain_height().map_err(|_| invalid())?.map(u32::from),
            fully_scanned: db.block_fully_scanned().map_err(|_| invalid())?.map(block),
            max_scanned: db.block_max_scanned().map_err(|_| invalid())?.map(block),
            scan_ranges,
        })
    }
    fn scanned_height(&self) -> u32 {
        self.fully_scanned.as_ref().map(|b| b.0).unwrap_or(0)
    }
    pub(super) fn identity(&self) -> std::result::Result<String, StoreError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| StoreError::Io("native_pir_database_failed".into()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native_pir_cancelled")
    }
}
impl std::fmt::Debug for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native_pir_cancelled")
    }
}
impl std::error::Error for Cancelled {}
fn check(cancel: &AtomicBool) -> std::result::Result<(), BoxError> {
    if cancel.load(Ordering::Acquire) {
        Err(Box::new(Cancelled))
    } else {
        Ok(())
    }
}
struct CancelFilters<'a, F> {
    inner: &'a mut F,
    cancel: &'a AtomicBool,
}
impl<F: FilterSource> FilterSource for CancelFilters<'_, F> {
    fn shard_map(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        check(self.cancel)?;
        let r = self.inner.shard_map()?;
        check(self.cancel)?;
        Ok(r)
    }
    fn filter(&mut self, i: u64) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        check(self.cancel)?;
        let r = self.inner.filter(i)?;
        check(self.cancel)?;
        Ok(r)
    }
    // Parent-filter research and prefetch are deliberately not enabled here.
}
struct CancelTransport<'a, T> {
    inner: &'a mut T,
    cancel: &'a AtomicBool,
}
impl<T: ShardTransport> ShardTransport for CancelTransport<'_, T> {
    fn init(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        check(self.cancel)?;
        let r = self.inner.init()?;
        check(self.cancel)?;
        Ok(r)
    }
    fn manifest(&mut self, i: u64, r: &str) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        check(self.cancel)?;
        let r = self.inner.manifest(i, r)?;
        check(self.cancel)?;
        Ok(r)
    }
    fn setup(
        &mut self,
        i: u64,
        r: &str,
        t: Table,
        s: u32,
    ) -> std::result::Result<(Vec<u8>, u64), BoxError> {
        check(self.cancel)?;
        let r = self.inner.setup(i, r, t, s)?;
        check(self.cancel)?;
        Ok(r)
    }
    fn query(
        &mut self,
        i: u64,
        r: &str,
        t: Table,
        b: &[u8],
    ) -> std::result::Result<Vec<u8>, BoxError> {
        check(self.cancel)?;
        let r = self.inner.query(i, r, t, b)?;
        check(self.cancel)?;
        Ok(r)
    }
}
impl NativeWallet {
    /// Rust-only, bounded regtest qualification using caller-owned private transport.
    /// Transport must enforce endpoint consent, deadline and network privacy policy.
    pub fn sync_regtest_pir(
        &self,
        chain: &RegtestAcceptedChain,
        filters: &mut impl FilterSource,
        transport: &mut impl ShardTransport,
        limits: WorkLimits,
        cancel: &AtomicBool,
    ) -> Result<PirDiscoveryReport> {
        if self.network != ZNetwork::Regtest {
            return Err(failed("native_pir_regtest_only"));
        }
        check(cancel).map_err(|_| failed("native_pir_cancelled"))?;
        let mut filters = CancelFilters {
            inner: filters,
            cancel,
        };
        let mut transport = CancelTransport {
            inner: transport,
            cancel,
        };
        let (raw, map_bytes) = filters
            .shard_map()
            .map_err(|_| failed("native_pir_transport_failed"))?;
        if raw.len() > 4 * 1024 * 1024 {
            return Err(failed("native_pir_publication_invalid"));
        }
        let map: ShardMap =
            serde_json::from_slice(&raw).map_err(|_| failed("native_pir_publication_invalid"))?;
        if map.network != "regtest" || map.genesis_hash != GENESIS {
            return Err(failed("native_pir_network_invalid"));
        }
        let (init, _) = transport
            .init()
            .map_err(|_| failed("native_pir_transport_failed"))?;
        if init.len() > 1024 * 1024 {
            return Err(failed("native_pir_publication_invalid"));
        }
        let geometry = parse_init(&init).map_err(|_| failed("native_pir_publication_invalid"))?;
        let mut db = self.open_db()?;
        let (scope, native_snapshot) = db
            .transactionally(|wdb| -> anyhow::Result<_> {
                Ok((
                    native_scope(wdb, map.start_height).map_err(store_failed)?,
                    NativeScanSnapshot::capture(wdb, chain.target.height as u32)
                        .map_err(store_failed)?,
                ))
            })
            .map_err(EngineError::from)?;
        drop(db);
        let mut store = store::NativePirStore::open(self, cancel).map_err(store_failed)?;
        store
            .enroll(scope.clone(), map.start_height)
            .map_err(store_failed)?;
        let mut scripts = StaticScripts(scope.scripts());
        let report = sync_into(
            &mut store,
            &map,
            map_bytes,
            &geometry,
            chain,
            &mut scripts,
            &mut filters,
            &mut transport,
            &limits,
            &chain.target,
        )
        .map_err(|_| {
            if cancel.load(Ordering::Acquire) {
                failed("native_pir_cancelled")
            } else {
                failed("native_pir_sync_failed")
            }
        })?;
        let completion = match report.completion {
            Completion::Complete => "complete-for-enrolled-scope".into(),
            Completion::Incomplete { reason, .. } => match reason {
                IncompleteReason::QueryBudget => "query-budget",
                IncompleteReason::ByteBudget => "byte-budget",
                IncompleteReason::PendingLimit => "pending-limit",
                IncompleteReason::Overloaded { .. } => "overloaded",
                IncompleteReason::ChainUnknown { .. } => "chain-unknown",
                IncompleteReason::DiscoveryUnbounded => "discovery-unbounded",
                IncompleteReason::PublicationBehind { .. } => "publication-behind",
                IncompleteReason::UnresolvedSpends => "unresolved-spends",
            }
            .into(),
        };
        let result = PirDiscoveryReport {
            completion,
            reconciled: true,
            accepted_anchor: Some(chain.target.clone()),
            source_identity: store
                .set_identity()
                .map_err(store_failed)?
                .ok_or_else(|| failed("native_pir_source_invalid"))?
                .digest(),
            context_identity: chain.context_identity.clone(),
            scope_identity: scope.identity()?,
            enrolled_script_count: scope.unique_script_count(),
            covered_through: report.covered_through,
            settled_through: report.settled_through,
            confirmed_ledger_zat: report.ledger.confirmed_balance(),
            history: report
                .ledger
                .history()
                .into_iter()
                .map(|t| PirConfirmedTransaction {
                    txid: t.txid.to_display_hex(),
                    height: t.height,
                    received_zat: t.received,
                    spent_zat: t.spent,
                })
                .collect(),
            pending_pages: store.pending().map_err(store_failed)?.len(),
            unresolved_spends: report.ledger.unresolved().len(),
            native_scanned_height: native_snapshot.scanned_height(),
            native_chain_height: native_snapshot.chain_height,
            native_anchor_hash: native_snapshot.anchor_hash.clone(),
            native_scanner_identity: Some(native_snapshot.identity().map_err(store_failed)?),
            publication_start: map.start_height,
        };
        store
            .persist_report(result.clone(), &native_snapshot)
            .map_err(store_failed)?;
        Ok(result)
    }
    /// Only return a result for this exact independently accepted chain and enrollment.
    /// A changed source key/context requires reconciliation; it does not reset the ledger.
    pub fn regtest_pir_discovery_for(
        &self,
        chain: &RegtestAcceptedChain,
    ) -> Result<Option<PirDiscoveryReport>> {
        let report = self.regtest_pir_discovery()?;
        if report.as_ref().is_some_and(|report| {
            report.accepted_anchor.as_ref() != Some(&chain.target)
                || report.context_identity != chain.context_identity
        }) {
            return Err(failed("native_pir_reconciliation_required"));
        }
        Ok(report)
    }
    /// Historical qualification only; application status must use `regtest_pir_discovery_for`.
    /// Refuses stale discovery after native scope or scanner state changes.
    pub fn regtest_pir_discovery(&self) -> Result<Option<PirDiscoveryReport>> {
        if self.network != ZNetwork::Regtest {
            return Err(failed("native_pir_regtest_only"));
        }
        let cancel = AtomicBool::new(false);
        let store = store::NativePirStore::open(self, &cancel).map_err(store_failed)?;
        let Some((report, scope)) = store.report() else {
            return Ok(None);
        };
        let height = report
            .accepted_anchor
            .as_ref()
            .ok_or_else(|| failed("native_pir_store_invalid"))?
            .height as u32;
        let mut db = self.open_db()?;
        let (snapshot, actual_scope) = db
            .transactionally(|wdb| -> anyhow::Result<_> {
                Ok((
                    NativeScanSnapshot::capture(wdb, height).map_err(store_failed)?,
                    native_scope(wdb, report.publication_start).map_err(store_failed)?,
                ))
            })
            .map_err(EngineError::from)?;
        if snapshot.anchor_hash != report.native_anchor_hash
            || snapshot.scanned_height() != report.native_scanned_height
            || snapshot.chain_height != report.native_chain_height
            || report.native_scanner_identity != Some(snapshot.identity().map_err(store_failed)?)
            || actual_scope != *scope
            || report.scope_identity != scope.identity()?
        {
            return Err(failed("native_pir_reconciliation_required"));
        }
        Ok(Some(report.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::wallet::tests::{fixture_account, fixture_wallet};
    fn fixture() -> (tempfile::TempDir, NativeWallet) {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        (dir, wallet)
    }
    #[test]
    fn independent_snapshot_requires_genesis_contiguity_and_schedule() {
        let schedule = RegtestScanSchedule {
            nu6_3_height: crate::regtest_nu6_3_height(),
            nu7_height: crate::regtest_nu7_height(),
        };
        assert!(RegtestAcceptedChain::from_local_node(
            schedule,
            vec![(0, GENESIS.into()), (1, "11".repeat(32))]
        )
        .is_ok());
        for headers in [
            vec![(0, GENESIS.into())],
            vec![(0, "00".repeat(32)), (1, "11".repeat(32))],
            vec![(0, GENESIS.into()), (2, "11".repeat(32))],
            vec![(0, GENESIS.into()), (1, "bad".into())],
        ] {
            assert!(RegtestAcceptedChain::from_local_node(schedule, headers).is_err());
        }
        assert!(RegtestAcceptedChain::from_local_node(
            RegtestScanSchedule {
                nu6_3_height: schedule.nu6_3_height + 1,
                ..schedule
            },
            vec![(0, GENESIS.into()), (1, "11".repeat(32))]
        )
        .is_err());
    }
    fn make_report(wallet: &NativeWallet, scope: &NativeScope) -> PirDiscoveryReport {
        let snapshot = NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap();
        PirDiscoveryReport {
            completion: "complete-for-enrolled-scope".into(),
            reconciled: true,
            accepted_anchor: Some(Anchor {
                height: 1,
                hash: "11".repeat(32),
            }),
            source_identity: "test-only".into(),
            context_identity: None,
            scope_identity: scope.identity().unwrap(),
            enrolled_script_count: scope.unique_script_count(),
            covered_through: 0,
            settled_through: 0,
            confirmed_ledger_zat: 0,
            history: vec![],
            pending_pages: 0,
            unresolved_spends: 0,
            native_scanned_height: snapshot.scanned_height(),
            native_chain_height: snapshot.chain_height,
            native_anchor_hash: snapshot.anchor_hash.clone(),
            native_scanner_identity: Some(snapshot.identity().unwrap()),
            publication_start: 1,
        }
    }
    #[test]
    fn report_reopen_refuses_native_rewind_scope_change_and_reset() {
        let (_dir, wallet) = fixture();
        let cancel = AtomicBool::new(false);
        let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        let report = make_report(&wallet, &scope);
        store
            .persist_report(
                report.clone(),
                &NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap(),
            )
            .unwrap();
        drop(store);
        assert_eq!(wallet.regtest_pir_discovery().unwrap(), Some(report));
        let mut db = wallet.open_db().unwrap();
        db.update_chain_tip(2.into()).unwrap();
        drop(db);
        assert!(wallet.regtest_pir_discovery().is_err());
        let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        let report = make_report(&wallet, &scope);
        store.enroll(scope.clone(), 1).unwrap();
        store
            .persist_report(
                report,
                &NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap(),
            )
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute("UPDATE accounts SET uuid=?1", [vec![8; 16]])
            .unwrap();
        assert!(wallet.regtest_pir_discovery().is_err());
        assert!(store
            .commit_anchor(
                &Anchor {
                    height: 1,
                    hash: "11".repeat(32)
                },
                1,
                1
            )
            .is_err());
        drop(store);
        drop(conn);
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        assert!(wallet.regtest_pir_discovery().unwrap().is_none());
    }
    #[test]
    fn current_context_and_anchor_are_required_after_reopen() {
        let (_dir, wallet) = fixture();
        let schedule = RegtestScanSchedule {
            nu6_3_height: crate::regtest_nu6_3_height(),
            nu7_height: crate::regtest_nu7_height(),
        };
        let chain = RegtestAcceptedChain::from_local_node(
            schedule,
            vec![(0, GENESIS.into()), (1, "11".repeat(32))],
        )
        .unwrap()
        .with_context_identity(&"aa".repeat(32))
        .unwrap();
        assert!(chain
            .clone()
            .with_context_identity(&"AA".repeat(32))
            .is_err());
        let cancel = AtomicBool::new(false);
        let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        let mut report = make_report(&wallet, &scope);
        report.context_identity = chain.context_identity.clone();
        store
            .persist_report(
                report.clone(),
                &NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap(),
            )
            .unwrap();
        drop(store);
        assert_eq!(
            wallet.regtest_pir_discovery_for(&chain).unwrap(),
            Some(report)
        );
        let changed = chain
            .clone()
            .with_context_identity(&"bb".repeat(32))
            .unwrap();
        assert!(wallet.regtest_pir_discovery_for(&changed).is_err());
        let changed_anchor = RegtestAcceptedChain::from_local_node(
            schedule,
            vec![(0, GENESIS.into()), (1, "22".repeat(32))],
        )
        .unwrap()
        .with_context_identity(&"aa".repeat(32))
        .unwrap();
        assert!(wallet.regtest_pir_discovery_for(&changed_anchor).is_err());
    }
    #[test]
    fn partial_failed_sync_cannot_republish_previous_completion_after_reopen() {
        let (_dir, wallet) = fixture();
        let cancel = AtomicBool::new(false);
        let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        store
            .persist_report(
                make_report(&wallet, &scope),
                &NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap(),
            )
            .unwrap();
        assert!(wallet.regtest_pir_discovery().unwrap().is_some());
        let commit = ShardCommit {
            shard_id: 0,
            revision_digest: "r0".into(),
            sealed: true,
            start_height: 1,
            end_height: 100,
            terminal_block_hash: "11".repeat(32),
            events: vec![transparent_wallet::testing::receive(1, 0, 10_000, 10)],
            covered_scripts: vec![transparent_wallet::testing::script(1)],
            ..Default::default()
        };
        store.commit_shard(commit.clone()).unwrap();
        let mut contradiction = commit;
        contradiction.events[0] = transparent_wallet::testing::receive(1, 0, 11_000, 10);
        assert!(store.commit_shard(contradiction).is_err());
        drop(store);
        assert!(wallet.regtest_pir_discovery().unwrap().is_none());
        let store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        assert_eq!(store.ledger().unwrap().confirmed_balance(), 10_000);
    }
    #[test]
    fn report_commit_refuses_concurrent_native_advance_rewind_and_anchor_change() {
        use prost::Message;
        use zcash_client_backend::{
            data_api::{chain::ChainState, WalletWrite},
            proto::compact_formats::*,
        };
        use zcash_primitives::block::BlockHash;

        let schedule = RegtestScanSchedule {
            nu6_3_height: crate::regtest_nu6_3_height(),
            nu7_height: crate::regtest_nu7_height(),
        };
        let mut genesis = crate::web::from_hex(GENESIS).unwrap();
        genesis.reverse();
        let blocks: Vec<_> = (1..=3)
            .map(|height| CompactBlock {
                height,
                hash: vec![height as u8; 32],
                prev_hash: if height == 1 {
                    genesis.clone()
                } else {
                    vec![(height - 1) as u8; 32]
                },
                chain_metadata: Some(ChainMetadata::default()),
                ..Default::default()
            })
            .collect();
        let encode = |blocks: &[CompactBlock]| {
            blocks
                .iter()
                .flat_map(Message::encode_length_delimited_to_vec)
                .collect::<Vec<_>>()
        };
        for change in ["advance", "rewind", "anchor", "partial", "queue"] {
            let (dir, wallet) = fixture();
            let (key, _) = fixture_account();
            wallet
                .replace_scan_db(
                    &key,
                    &AccountBirthday::from_parts(
                        ChainState::empty(0.into(), BlockHash::from_slice(&genesis)),
                        None,
                    ),
                )
                .unwrap();
            wallet
                .scan_public_regtest_incremental(&encode(&blocks[..2]), schedule)
                .unwrap();
            if change == "partial" || change == "queue" {
                wallet
                    .open_db()
                    .unwrap()
                    .update_chain_tip(5.into())
                    .unwrap();
            }
            let cancel = AtomicBool::new(false);
            let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
            let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
            store.enroll(scope.clone(), 1).unwrap();
            let expected = NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 2).unwrap();
            let mut report = make_report(&wallet, &scope);
            report.accepted_anchor = Some(Anchor {
                height: 2,
                hash: "02".repeat(32),
            });
            report.native_scanned_height = expected.scanned_height();
            report.native_chain_height = expected.chain_height;
            report.native_anchor_hash = expected.anchor_hash.clone();
            report.native_scanner_identity = Some(expected.identity().unwrap());
            store.persist_report(report.clone(), &expected).unwrap();
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let head = || {
                conn.query_row(
                    "SELECT generation,bytes,digest FROM ext_coffer_pir_head WHERE id=1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, u64>(0)?,
                            r.get::<_, u64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .unwrap()
            };
            // A second handle changes the native scanner while the PIR operation is outstanding.
            std::thread::scope(|threads| {
                threads
                    .spawn(|| {
                        let other = NativeWallet::open(dir.path()).unwrap();
                        match change {
                            "advance" => {
                                other
                                    .scan_public_regtest_incremental(&encode(&blocks), schedule)
                                    .unwrap();
                            }
                            "rewind" => {
                                other
                                    .open_db()
                                    .unwrap()
                                    .truncate_to_height(1.into())
                                    .unwrap();
                            }
                            "anchor" => {
                                other
                                    .open_db()
                                    .unwrap()
                                    .truncate_to_height(1.into())
                                    .unwrap();
                                let mut fork = blocks[..2].to_vec();
                                fork[1].hash = vec![22; 32];
                                other
                                    .scan_public_regtest_incremental(&encode(&fork), schedule)
                                    .unwrap();
                            }
                            "partial" => {
                                // Model a separately scanned island beyond the unchanged fully-scanned prefix.
                                let conn = rusqlite::Connection::open(&other.paths.data_db).unwrap();
                                conn.execute("INSERT INTO blocks (height,hash,time,sapling_tree,sapling_commitment_tree_size,orchard_commitment_tree_size,ironwood_commitment_tree_size) SELECT 4,?1,time,sapling_tree,sapling_commitment_tree_size,orchard_commitment_tree_size,ironwood_commitment_tree_size FROM blocks WHERE height=2", [vec![4; 32]]).unwrap();
                                conn.execute_batch("DELETE FROM scan_queue WHERE block_range_start>=3; INSERT INTO scan_queue (block_range_start,block_range_end,priority) VALUES (3,4,50),(4,5,10),(5,6,50);").unwrap();
                            }
                            "queue" => {
                                let conn = rusqlite::Connection::open(&other.paths.data_db).unwrap();
                                assert!(conn.execute("UPDATE scan_queue SET priority=CASE WHEN priority=60 THEN 40 ELSE 60 END WHERE priority>10", []).unwrap()>0);
                            }
                            _ => unreachable!(),
                        }
                    })
                    .join()
                    .unwrap();
            });
            let changed = NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 2).unwrap();
            assert_ne!(expected, changed, "{change}");
            if change == "anchor" {
                assert_eq!(expected.scanned_height(), changed.scanned_height());
                assert_eq!(expected.chain_height, changed.chain_height);
                assert_ne!(expected.anchor_hash, changed.anchor_hash);
            }
            if change == "partial" || change == "queue" {
                assert_eq!(expected.fully_scanned, changed.fully_scanned);
                assert_eq!(expected.chain_height, changed.chain_height);
                assert_eq!(expected.anchor_hash, changed.anchor_hash);
                if change == "partial" {
                    assert_ne!(expected.max_scanned, changed.max_scanned);
                } else {
                    assert_eq!(expected.max_scanned, changed.max_scanned);
                    assert_ne!(expected.scan_ranges, changed.scan_ranges);
                }
            }
            assert!(NativeWallet::open(dir.path())
                .unwrap()
                .regtest_pir_discovery()
                .is_err());
            store
                .commit_shard(ShardCommit {
                    shard_id: 0,
                    revision_digest: "r0".into(),
                    sealed: true,
                    start_height: 1,
                    end_height: 2,
                    terminal_block_hash: "02".repeat(32),
                    events: vec![transparent_wallet::testing::receive(1, 0, 10_000, 2)],
                    covered_scripts: vec![transparent_wallet::testing::script(1)],
                    ..Default::default()
                })
                .unwrap();
            let before = head();
            assert!(matches!(
                store.persist_report(report, &expected),
                Err(StoreError::Io(code)) if code == "native_pir_reconciliation_required"
            ));
            assert_eq!(before, head(), "failed report must not advance its journal");
            assert!(store.report().is_none());
            assert_eq!(store.ledger().unwrap().confirmed_balance(), 10_000);
            drop(store);
            let reopened = NativeWallet::open(dir.path()).unwrap();
            assert!(reopened.regtest_pir_discovery().unwrap().is_none());
            let store = store::NativePirStore::open(&reopened, &cancel).unwrap();
            assert_eq!(store.ledger().unwrap().confirmed_balance(), 10_000);
        }
    }
    #[test]
    fn unique_script_count_retains_every_account_binding_in_identity() {
        let (_dir, wallet) = fixture();
        let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
        let one = NativeScope(vec![scope.0[0].clone()]);
        let mut duplicate = one.0[0].clone();
        duplicate.account = "another-account".into();
        let two = NativeScope(vec![one.0[0].clone(), duplicate]);
        assert_eq!(one.unique_script_count(), 1);
        assert_eq!(two.unique_script_count(), 1);
        assert_ne!(one.identity().unwrap(), two.identity().unwrap());
        assert!(!one.continues(&two));
    }
    #[test]
    fn legacy_report_without_scanner_identity_requires_fresh_reconciliation() {
        let (_dir, wallet) = fixture();
        let cancel = AtomicBool::new(false);
        let mut store = store::NativePirStore::open(&wallet, &cancel).unwrap();
        let scope = native_scope(&wallet.open_db().unwrap(), 1).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        let snapshot = NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 1).unwrap();
        let mut value = serde_json::to_value(make_report(&wallet, &scope)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("native_scanner_identity");
        let legacy: PirDiscoveryReport = serde_json::from_value(value).unwrap();
        assert!(legacy.native_scanner_identity.is_none());
        assert!(store.persist_report(legacy, &snapshot).is_err());
        assert!(store.report().is_none());
        store
            .persist_report(make_report(&wallet, &scope), &snapshot)
            .unwrap();
        drop(store);
        assert!(wallet.regtest_pir_discovery().unwrap().is_some());
    }
    #[test]
    fn scope_is_native_owned_and_missing_prehistory_is_refused() {
        let (_dir, wallet) = fixture();
        let db = wallet.open_db().unwrap();
        let scope = native_scope(&db, 1).unwrap();
        assert!(!scope.0.is_empty());
        assert!(scope
            .0
            .iter()
            .all(|e| e.entry.origin == ScriptOrigin::Derived && e.entry.required_from == 1));
        assert!(native_scope(&db, 2).is_err());
    }
    #[test]
    fn standalone_imports_require_history_from_genesis_independent_of_account_birthday() {
        assert_eq!(receiver_history_floor(true, 100, 1).unwrap(), 1);
        assert!(receiver_history_floor(true, 100, 2).is_err());
        assert!(receiver_history_floor(true, 100, 100).is_err());
        assert_eq!(receiver_history_floor(false, 100, 100).unwrap(), 100);
        assert!(receiver_history_floor(false, 100, 101).is_err());
    }
    #[test]
    fn cancellation_at_transport_boundary_prevents_reply_use() {
        struct Filters<'a>(&'a AtomicBool);
        impl FilterSource for Filters<'_> {
            fn shard_map(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
                self.0.store(true, Ordering::Release);
                Ok((b"private node error text".to_vec(), 0))
            }
            fn filter(&mut self, _: u64) -> std::result::Result<(Vec<u8>, u64), BoxError> {
                unreachable!()
            }
        }
        let cancel = AtomicBool::new(false);
        let mut filters = Filters(&cancel);
        let mut checked = CancelFilters {
            inner: &mut filters,
            cancel: &cancel,
        };
        assert!(checked.shard_map().is_err());
    }
}
