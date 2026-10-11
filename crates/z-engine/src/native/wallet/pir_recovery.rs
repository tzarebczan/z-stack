//! Canonical recovery owns its durable selection barrier and final receipt.
use super::super::{RecoveryCancellation, VerifiedRegtestRecoveryBlocks};
use super::*;
use rusqlite::OptionalExtension;
use transparent::bundle::{OutPoint, TxOut};
use transparent_events::{
    FeeState, ReceiveEvent, SpendEvent, TransactionMetadata, TransparentEvent, Txid,
};
use zcash_client_backend::{data_api::ll::LowLevelWalletWrite, wallet::WalletTx};
use zcash_primitives::block::BlockHash;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PirRecoveryReport {
    pub completion: String,
    pub accepted_height: u32,
    pub native_scanned_height: u32,
    pub recovered_outputs: usize,
    pub scope_complete: bool,
    pub selection_blocked: bool,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    report: PirRecoveryReport,
    chain_context: Option<String>,
    scope_identity: String,
    #[serde(default)]
    discovery_scope_identity: Option<String>,
    scanner_identity: String,
    #[serde(default)]
    journal_head: Option<(u64, String)>,
    anchor: Anchor,
}
fn require_journal_head(
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
    expected: Option<&(u64, String)>,
) -> anyhow::Result<()> {
    let expected =
        expected.ok_or_else(|| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
    let actual: (u64, String) = ext.query_row(
        "SELECT generation,digest FROM ext_coffer_pir_head WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if &actual != expected {
        anyhow::bail!("native_recovery_reconciliation_required");
    }
    Ok(())
}
fn require_discovery_scope(discovered: &str, scope: &NativeScope) -> anyhow::Result<()> {
    if discovered != scope.identity()? {
        anyhow::bail!("native_recovery_reconciliation_required");
    }
    Ok(())
}
fn receipt_journal_matches(conn: &rusqlite::Connection, receipt: &Receipt) -> bool {
    if receipt.discovery_scope_identity.as_deref() != Some(receipt.scope_identity.as_str()) {
        return false;
    }
    receipt.journal_head.as_ref().is_some_and(|expected| {
        conn.query_row(
            "SELECT generation,digest FROM ext_coffer_pir_head WHERE id=1",
            [],
            |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)),
        )
        .is_ok_and(|actual| &actual == expected)
    })
}
fn schema(wallet: &NativeWallet) -> Result<()> {
    let conn = rusqlite::Connection::open(&wallet.paths.data_db)
        .map_err(|_| failed("native_recovery_storage_failed"))?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| failed("native_recovery_storage_failed"))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS ext_coffer_pir_recovery_v1(id INTEGER PRIMARY KEY CHECK(id=1), blocked INTEGER NOT NULL CHECK(blocked IN (0,1)), generation INTEGER NOT NULL, receipt BLOB);").map_err(|_| failed("native_recovery_storage_failed"))?;
    Ok(())
}
fn event_key(script: &[u8], mut event: TransparentEvent) -> (Vec<u8>, Vec<u8>) {
    match &mut event {
        TransparentEvent::Receive(e) => e.metadata = None,
        TransparentEvent::Spend(e) => e.metadata = None,
    }
    (script.to_vec(), event.to_bytes())
}
fn expected_events(
    evidence: &VerifiedRegtestRecoveryBlocks,
    scope: &NativeScope,
) -> Result<BTreeMap<(Vec<u8>, Vec<u8>), TransactionMetadata>> {
    let scripts: std::collections::BTreeSet<_> =
        scope.0.iter().map(|s| s.entry.script.clone()).collect();
    let mut outputs: BTreeMap<OutPoint, TxOut> = BTreeMap::new();
    let mut events = BTreeMap::new();
    for block in &evidence.blocks {
        let height = u32::from(block.claimed_height());
        for (index, tx) in block.vtx().iter().enumerate() {
            let transaction_index =
                u16::try_from(index).map_err(|_| failed("native_recovery_blocks_invalid"))?;
            let bundle = tx.transparent_bundle();
            let coinbase = bundle.is_some_and(|b| b.is_coinbase());
            let fee = if coinbase {
                FeeState::NotApplicable
            } else {
                tx.fee_paid::<anyhow::Error, _>(|prev| Ok(outputs.get(prev).map(|out| out.value())))
                    .map_err(|_| failed("native_recovery_blocks_invalid"))?
                    .map_or(FeeState::Unknown, |fee| FeeState::Exact(u64::from(fee)))
            };
            let metadata = TransactionMetadata {
                fee,
                transparent_input_count: if coinbase {
                    0
                } else {
                    bundle.map_or(0, |b| b.vin.len() as u32)
                },
                has_shielded_components: tx.sapling_bundle().is_some()
                    || tx.orchard_bundle().is_some()
                    || tx.ironwood_bundle().is_some(),
            };
            if let Some(bundle) = bundle {
                if !coinbase {
                    for (input_index, input) in bundle.vin.iter().enumerate() {
                        let previous = outputs
                            .get(input.prevout())
                            .ok_or_else(|| failed("native_recovery_prevout_missing"))?;
                        let script = &previous.script_pubkey().0 .0;
                        if scripts.contains(script) {
                            let event = TransparentEvent::Spend(SpendEvent {
                                metadata: Some(metadata),
                                height,
                                spending_txid: Txid(*tx.txid().as_ref()),
                                transaction_index,
                                input_index: input_index as u32,
                                spent_txid: Txid(*input.prevout().hash()),
                                spent_output_index: input.prevout().n(),
                            });
                            events.insert(event_key(script, event), metadata);
                        }
                    }
                }
                for (output_index, output) in bundle.vout.iter().enumerate() {
                    let outpoint = OutPoint::new(*tx.txid().as_ref(), output_index as u32);
                    let script = &output.script_pubkey().0 .0;
                    if scripts.contains(script) {
                        let event = TransparentEvent::Receive(ReceiveEvent {
                            metadata: Some(metadata),
                            height,
                            txid: Txid(*tx.txid().as_ref()),
                            transaction_index,
                            output_index: output_index as u32,
                            value: u64::from(output.value()),
                            coinbase,
                        });
                        events.insert(event_key(script, event), metadata);
                    }
                    if outputs.insert(outpoint, output.clone()).is_some() {
                        return Err(failed("native_recovery_blocks_invalid"));
                    }
                }
            }
        }
    }
    Ok(events)
}
pub(in crate::native::wallet) fn ensure_transaction_receipt<
    D: WalletRead<AccountId = AccountUuid>,
>(
    db: &D,
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
) -> anyhow::Result<()> {
    let (blocked, bytes): (bool, Option<Vec<u8>>) = ext.query_row(
        "SELECT blocked,receipt FROM ext_coffer_pir_recovery_v1 WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if blocked {
        anyhow::bail!("native_recovery_pending");
    }
    let receipt: Receipt =
        serde_json::from_slice(&bytes.ok_or_else(|| anyhow::anyhow!("native_recovery_pending"))?)?;
    if receipt.discovery_scope_identity.as_deref() != Some(receipt.scope_identity.as_str()) {
        anyhow::bail!("native_recovery_reconciliation_required");
    }
    require_journal_head(ext, receipt.journal_head.as_ref())?;
    let snapshot =
        NativeScanSnapshot::capture(db, receipt.report.accepted_height).map_err(store_failed)?;
    if receipt.report.selection_blocked
        || !receipt.report.scope_complete
        || receipt.report.completion != "complete"
        || !snapshot.matches_anchor(&receipt.anchor)
        || snapshot.identity().map_err(store_failed)? != receipt.scanner_identity
        || native_scope_in_transaction(db, ext, 1)?.identity()? != receipt.scope_identity
    {
        anyhow::bail!("native_recovery_reconciliation_required");
    }
    Ok(())
}
impl NativeWallet {
    fn recovery_scope(&self) -> Result<NativeScope> {
        self.open_db()?
            .transactionally_with_extension(|wdb, ext| native_scope_in_transaction(wdb, ext, 1))
            .map_err(EngineError::from)
    }

    pub(in crate::native::wallet) fn ensure_pir_recovery_ready(&self) -> Result<()> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| failed("native_recovery_storage_failed"))?;
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_coffer_pir_recovery_v1')",[],|r|r.get(0)).map_err(|_|failed("native_recovery_storage_failed"))?;
        if !exists {
            return Ok(());
        }
        let blocked: Option<bool> = conn
            .query_row(
                "SELECT blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| failed("native_recovery_storage_failed"))?;
        if blocked != Some(false) {
            return Err(failed("native_recovery_pending"));
        }
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT receipt FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| r.get(0),
            )
            .map_err(|_| failed("native_recovery_pending"))?;
        let receipt: Receipt =
            serde_json::from_slice(&bytes).map_err(|_| failed("native_recovery_storage_failed"))?;
        let blocked: bool = conn
            .query_row(
                "SELECT blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| failed("native_recovery_storage_failed"))?;
        if blocked != receipt.report.selection_blocked || receipt.report.scope_complete == blocked {
            return Err(failed("native_recovery_reconciliation_required"));
        }
        let db = self.open_db()?;
        let snapshot = NativeScanSnapshot::capture(&db, receipt.report.accepted_height)
            .map_err(store_failed)?;
        if receipt.report.selection_blocked
            || receipt.report.completion != "complete"
            || !snapshot.matches_anchor(&receipt.anchor)
            || snapshot.identity().map_err(store_failed)? != receipt.scanner_identity
            || self.recovery_scope()?.identity()? != receipt.scope_identity
            || !receipt_journal_matches(&conn, &receipt)
        {
            return Err(failed("native_recovery_reconciliation_required"));
        }
        Ok(())
    }
    pub(in crate::native::wallet) fn has_complete_pir_recovery(&self) -> Result<bool> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| failed("native_recovery_storage_failed"))?;
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ext_coffer_pir_recovery_v1')",[],|r|r.get(0)).map_err(|_|failed("native_recovery_storage_failed"))?;
        if exists {
            self.ensure_pir_recovery_ready()?;
        }
        Ok(exists)
    }
    /// Persist the selection barrier before any source transport can fail.
    pub fn begin_regtest_pir_recovery(
        &self,
        chain: &RegtestAcceptedChain,
        cancel: &RecoveryCancellation,
    ) -> Result<u64> {
        if self.network != ZNetwork::Regtest || self.birthday_height() != 1 {
            return Err(failed("native_recovery_regtest_only"));
        }
        if chain.target_height() == 0 {
            return Err(failed("native_recovery_chain_invalid"));
        }
        let _lease = database_lease::shared(&self.paths.data_db)?;
        cancel.transaction(|| schema(self))?;
        let mut db = self.open_recovery_db()?;
        cancel.transaction(||db.transactionally_with_extension(|_,ext|->anyhow::Result<u64>{
            ext.execute("INSERT INTO ext_coffer_pir_recovery_v1 VALUES(1,1,1,NULL) ON CONFLICT(id) DO UPDATE SET blocked=1,generation=generation+1,receipt=NULL",[])?;
            Ok(ext.query_row("SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",[],|r|r.get(0))?)
        }).map_err(EngineError::from))
    }
    pub fn recover_regtest_pir(
        &self,
        chain: &RegtestAcceptedChain,
        evidence: VerifiedRegtestRecoveryBlocks,
        filters: &mut impl FilterSource,
        transport: &mut impl ShardTransport,
        limits: WorkLimits,
        cancel: &RecoveryCancellation,
    ) -> Result<PirRecoveryReport> {
        if self.network != ZNetwork::Regtest
            || self.birthday_height() != 1
            || evidence.target_height() != chain.target_height()
            || evidence.target_hash() != chain.target_hash()
        {
            return Err(failed("native_recovery_regtest_only"));
        }
        let _lease = database_lease::shared(&self.paths.data_db)?;
        cancel.transaction(|| schema(self))?;
        let generation = self.begin_regtest_pir_recovery(chain, cancel)?;
        let mut db = self.open_recovery_db()?;
        let compact = evidence.compact_blocks()?;
        let mut genesis = crate::web::from_hex(
            &chain
                .hash_at(0)
                .ok_or_else(|| failed("native_recovery_chain_invalid"))?,
        )
        .map_err(|_| failed("native_recovery_chain_invalid"))?;
        genesis.reverse();
        let mut recovered_outputs = 0usize;
        cancel.transaction(|| {
            db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<()> {
                let current: u64 = ext.query_row(
                    "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |r| r.get(0),
                )?;
                if current != generation {
                    anyhow::bail!("native_recovery_generation_changed");
                }
                let before: usize = ext.query_row(
                    "SELECT count(*) FROM transparent_received_outputs",
                    [],
                    |r| r.get(0),
                )?;
                for block in &evidence.blocks {
                    for tx in block.vtx() {
                        let raw: Option<Vec<u8>> = ext
                            .query_row(
                                "SELECT raw FROM transactions WHERE txid=?1",
                                [tx.txid().as_ref().as_slice()],
                                |r| r.get(0),
                            )
                            .optional()?
                            .flatten();
                        if let Some(raw) = raw {
                            let mut incoming = Vec::new();
                            tx.write(&mut incoming)?;
                            if raw != incoming {
                                anyhow::bail!("native_recovery_raw_conflict");
                            }
                        }
                    }
                }
                super::super::public_scan::scan_in_transaction(
                    wdb,
                    self.network,
                    compact.clone(),
                    true,
                    BlockHash::from_slice(&genesis),
                )?;
                super::super::public_scan::scan_in_transaction(
                    wdb,
                    self.network,
                    compact,
                    false,
                    BlockHash::from_slice(&genesis),
                )?;
                for _round in 0..32 {
                    let prior = native_scope_in_transaction(wdb, ext, 1)?;
                    for block in &evidence.blocks {
                        for (index, tx) in block.vtx().iter().enumerate() {
                            if cancel.is_cancelled() {
                                anyhow::bail!("native_recovery_cancelled");
                            }
                            decrypt_and_store_transaction(
                                &self.network,
                                wdb,
                                tx,
                                Some(block.claimed_height()),
                            )?;
                            if wdb.get_tx_height(tx.txid())?.is_some() {
                                let observed = WalletTx::new(
                                    tx.txid(),
                                    u16::try_from(index)?.into(),
                                    vec![],
                                    vec![],
                                    vec![],
                                    vec![],
                                    vec![],
                                    vec![],
                                    vec![],
                                );
                                wdb.put_tx_meta(&observed, block.claimed_height())?;
                            }
                        }
                    }
                    let scope = native_scope_in_transaction(wdb, ext, 1)?;
                    if scope == prior {
                        let after: usize = ext.query_row(
                            "SELECT count(*) FROM transparent_received_outputs",
                            [],
                            |r| r.get(0),
                        )?;
                        recovered_outputs = after.saturating_sub(before);
                        if cancel.is_cancelled() {
                            anyhow::bail!("native_recovery_cancelled");
                        }
                        return Ok(());
                    }
                }
                anyhow::bail!("native_recovery_scope_limit")
            })
            .map_err(EngineError::from)
        })?;
        drop(db);
        let discovery = self.sync_regtest_pir_with_commit_gate(
            chain,
            filters,
            transport,
            limits,
            cancel.flag(),
            Some(cancel),
        )?;
        let mut db = self.open_recovery_db()?;
        let scope = self.recovery_scope()?;
        let expected = expected_events(&evidence, &scope)?;
        let store = store::NativePirStore::open_recovery(self, cancel).map_err(store_failed)?;
        let mut actual = BTreeMap::new();
        let mut discrepancy = false;
        for event in store.events().map_err(store_failed)? {
            let key = event_key(&event.script, event.event);
            if let Some(metadata) = event.event.metadata() {
                if expected.get(&key) != Some(&metadata) {
                    discrepancy = true;
                }
            }
            if actual.insert(key, ()).is_some() {
                discrepancy = true;
            }
        }
        if actual.len() != expected.len() || actual.keys().ne(expected.keys()) {
            discrepancy = true;
        }
        let journal_head = store.journal_head();
        drop(store);
        let complete = discovery.completion == "complete-for-enrolled-scope" && !discrepancy;
        let report = PirRecoveryReport {
            completion: if discrepancy {
                "provider-discrepancy".into()
            } else if complete {
                "complete".into()
            } else {
                discovery.completion.clone()
            },
            accepted_height: chain.target_height(),
            native_scanned_height: discovery.native_scanned_height,
            recovered_outputs,
            scope_complete: complete,
            selection_blocked: !complete,
        };
        cancel.transaction(|| {
            db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<()> {
                let current: u64 = ext.query_row(
                    "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |r| r.get(0),
                )?;
                if current != generation {
                    anyhow::bail!("native_recovery_generation_changed");
                }
                require_journal_head(ext, Some(&journal_head))?;
                require_discovery_scope(
                    &discovery.scope_identity,
                    &native_scope_in_transaction(wdb, ext, 1)?,
                )?;
                let snapshot = NativeScanSnapshot::capture(wdb, chain.target_height())
                    .map_err(store_failed)?;
                if !snapshot.matches_chain(chain)
                    || discovery.native_scanner_identity
                        != Some(snapshot.identity().map_err(store_failed)?)
                    || native_scope_in_transaction(wdb, ext, 1)? != scope
                {
                    anyhow::bail!("native_recovery_reconciliation_required");
                }
                let receipt = Receipt {
                    report: report.clone(),
                    chain_context: chain.context_identity.clone(),
                    scope_identity: scope.identity()?,
                    discovery_scope_identity: Some(discovery.scope_identity.clone()),
                    scanner_identity: snapshot.identity().map_err(store_failed)?,
                    journal_head: Some(journal_head.clone()),
                    anchor: chain.target.clone(),
                };
                let bytes = serde_json::to_vec(&receipt)?;
                ext.execute(
                    "UPDATE ext_coffer_pir_recovery_v1 SET blocked=?1,receipt=?2 WHERE id=1",
                    (!complete, bytes),
                )?;
                if cancel.is_cancelled() {
                    anyhow::bail!("native_recovery_cancelled");
                }
                Ok(())
            })
            .map_err(EngineError::from)
        })?;
        Ok(report)
    }
    /// Atomically rewind native canonical state and staged discovery to the
    /// highest retained independently accepted overlap; preserve Pending on fork.
    pub fn rewind_regtest_pir(
        &self,
        chain: &RegtestAcceptedChain,
        cancel: &RecoveryCancellation,
    ) -> Result<u32> {
        if self.network != ZNetwork::Regtest || self.birthday_height() != 1 {
            return Err(failed("native_recovery_regtest_only"));
        }
        let _lease = database_lease::shared(&self.paths.data_db)?;
        self.begin_regtest_pir_recovery(chain, cancel)?;
        let db = self.open_db()?;
        let current = db
            .block_max_scanned()
            .map_err(|_| failed("native_recovery_storage_failed"))?
            .ok_or_else(|| failed("native_recovery_reset_required"))?;
        let snapshot = NativeScanSnapshot::capture(&db, u32::from(current.block_height()))
            .map_err(store_failed)?;
        let mut common = None;
        for height in (1..=chain.target_height().min(u32::from(current.block_height()))).rev() {
            if let Some(hash) = db
                .get_block_hash(height.into())
                .map_err(|_| failed("native_recovery_storage_failed"))?
            {
                if chain.is_accepted(u64::from(height), &hash.to_string()) == Acceptance::Accepted {
                    common = Some(Anchor {
                        height: u64::from(height),
                        hash: hash.to_string(),
                    });
                    break;
                }
            }
        }
        let anchor = common.ok_or_else(|| failed("native_recovery_reset_required"))?;
        drop(db);
        let mut store = store::NativePirStore::open_recovery(self, cancel).map_err(store_failed)?;
        store.rewind_native(anchor, &snapshot).map_err(store_failed)
    }
    pub fn regtest_pir_recovery_for(
        &self,
        chain: &RegtestAcceptedChain,
    ) -> Result<Option<PirRecoveryReport>> {
        if self.network != ZNetwork::Regtest {
            return Err(failed("native_recovery_regtest_only"));
        }
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| failed("native_recovery_storage_failed"))?;
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ext_coffer_pir_recovery_v1')",[],|r|r.get(0)).map_err(|_|failed("native_recovery_storage_failed"))?;
        if !exists {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT receipt FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| failed("native_recovery_storage_failed"))?
            .flatten();
        let Some(bytes) = bytes else {
            return Err(failed("native_recovery_pending"));
        };
        let receipt: Receipt =
            serde_json::from_slice(&bytes).map_err(|_| failed("native_recovery_storage_failed"))?;
        let blocked: bool = conn
            .query_row(
                "SELECT blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| failed("native_recovery_storage_failed"))?;
        if blocked != receipt.report.selection_blocked || receipt.report.scope_complete == blocked {
            return Err(failed("native_recovery_reconciliation_required"));
        }
        let db = self.open_db()?;
        let snapshot =
            NativeScanSnapshot::capture(&db, chain.target_height()).map_err(store_failed)?;
        if receipt.anchor != chain.target
            || receipt.chain_context != chain.context_identity
            || !snapshot.matches_chain(chain)
            || receipt.scanner_identity != snapshot.identity().map_err(store_failed)?
            || receipt.scope_identity != self.recovery_scope()?.identity()?
            || !receipt_journal_matches(&conn, &receipt)
        {
            return Err(failed("native_recovery_reconciliation_required"));
        }
        Ok(Some(receipt.report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::wallet::recovery_blocks::{
        tests::fixture_with_script, verify_regtest_recovery_blocks,
    };
    use crate::native::wallet::tests::{fixture_account, fixture_wallet};
    struct Unavailable;
    impl FilterSource for Unavailable {
        fn shard_map(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
            Err("fixture_unavailable".into())
        }
        fn filter(&mut self, _: u64) -> std::result::Result<(Vec<u8>, u64), BoxError> {
            Err("fixture_unavailable".into())
        }
    }
    impl ShardTransport for Unavailable {
        fn init(&mut self) -> std::result::Result<(Vec<u8>, u64), BoxError> {
            Err("fixture_unavailable".into())
        }
        fn manifest(&mut self, _: u64, _: &str) -> std::result::Result<(Vec<u8>, u64), BoxError> {
            Err("fixture_unavailable".into())
        }
        fn setup(
            &mut self,
            _: u64,
            _: &str,
            _: Table,
            _: u32,
        ) -> std::result::Result<(Vec<u8>, u64), BoxError> {
            Err("fixture_unavailable".into())
        }
        fn query(
            &mut self,
            _: u64,
            _: &str,
            _: Table,
            _: &[u8],
        ) -> std::result::Result<Vec<u8>, BoxError> {
            Err("fixture_unavailable".into())
        }
    }
    fn wallet() -> (tempfile::TempDir, NativeWallet) {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (key, _) = fixture_account();
        let mut genesis = crate::web::from_hex(GENESIS).unwrap();
        genesis.reverse();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                0.into(),
                BlockHash::from_slice(&genesis),
            ),
            None,
        );
        wallet.replace_scan_db(&key, &birthday).unwrap();
        (dir, wallet)
    }
    #[test]
    fn unavailable_private_source_preserves_authenticated_coinbase_but_blocks_selection_on_reopen()
    {
        let (dir, wallet) = wallet();
        let db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let address = db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .into_keys()
            .next()
            .unwrap();
        let script: transparent::address::Script = address.script().into();
        let (bytes, chain) = fixture_with_script(&script.0 .0);
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        assert!(wallet.history(10).unwrap().is_empty());
        assert!(wallet
            .recover_regtest_pir(
                &chain,
                evidence,
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024)
                },
                &RecoveryCancellation::new()
            )
            .is_err());
        assert_eq!(wallet.scanned_height().unwrap(), 1);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let (index,count):(u32,u32)=conn.query_row("SELECT tx_index,(SELECT count(*) FROM transparent_received_outputs) FROM transactions WHERE mined_height=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(index, 0);
        assert_eq!(count, 1);
        assert_eq!(wallet.balance().unwrap().transparent_available, 0);
        assert!(wallet.ensure_pir_recovery_ready().is_err());
        assert!(NativeWallet::open(dir.path())
            .unwrap()
            .ensure_pir_recovery_ready()
            .is_err());
        assert!(wallet.prepare_payment("unused", 1, None).is_err());
    }
    fn completed_fixture() -> (
        tempfile::TempDir,
        NativeWallet,
        RegtestAcceptedChain,
        Receipt,
    ) {
        let (dir, wallet) = wallet();
        let (bytes, chain) = fixture_with_script(&[0x51]);
        let _ = wallet.recover_regtest_pir(
            &chain,
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap(),
            &mut Unavailable,
            &mut Unavailable,
            WorkLimits {
                max_queries: Some(1),
                max_private_bytes: Some(1024),
            },
            &RecoveryCancellation::new(),
        );
        let db = wallet.open_db().unwrap();
        // Install a correctly bound receipt to isolate the readiness race from
        // transport fixtures; no signing or balance assertion uses this receipt.
        let receipt = Receipt {
            report: PirRecoveryReport {
                completion: "complete".into(),
                accepted_height: 1,
                native_scanned_height: 1,
                recovered_outputs: 0,
                scope_complete: true,
                selection_blocked: false,
            },
            chain_context: chain.context_identity.clone(),
            scope_identity: wallet.recovery_scope().unwrap().identity().unwrap(),
            discovery_scope_identity: Some(wallet.recovery_scope().unwrap().identity().unwrap()),
            scanner_identity: NativeScanSnapshot::capture(&db, 1)
                .unwrap()
                .identity()
                .unwrap(),
            journal_head: Some(
                store::NativePirStore::open(&wallet, &AtomicBool::new(false))
                    .unwrap()
                    .journal_head(),
            ),
            anchor: chain.target.clone(),
        };
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute(
            "UPDATE ext_coffer_pir_recovery_v1 SET blocked=0,receipt=?1 WHERE id=1",
            [serde_json::to_vec(&receipt).unwrap()],
        )
        .unwrap();
        assert!(wallet.ensure_recovery_selection_ready().is_ok());
        drop(db);
        drop(conn);
        (dir, wallet, chain, receipt)
    }
    #[test]
    fn writer_snapshot_refuses_scope_expansion_after_an_earlier_ready_check() {
        let (_dir, wallet, _chain, _receipt) = completed_fixture();
        let mut db = wallet.open_db().unwrap();
        let mut concurrent = wallet.open_db().unwrap();
        let seed = SecretVec::new(
            Mnemonic::parse_normalized(crate::keys::REGTEST_FAUCET_MNEMONIC)
                .unwrap()
                .to_seed("")
                .to_vec(),
        );
        let mut genesis = crate::web::from_hex(GENESIS).unwrap();
        genesis.reverse();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                0.into(),
                BlockHash::from_slice(&genesis),
            ),
            None,
        );
        concurrent
            .import_account_hd(
                "second",
                &seed,
                Zip32AccountId::try_from(1).unwrap(),
                &birthday,
                None,
            )
            .unwrap();
        // The same writer-bound check called by prepare/sign catches allocation
        // even though Pending stayed false and the chain anchor did not change.
        let result = db.transactionally_with_extension(|wdb, ext| {
            super::super::super::recovery_guard::transaction_recovery_ready(wdb, ext)
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("native_recovery_reconciliation_required"));
    }
    #[test]
    fn final_recovery_writer_refuses_scope_allocated_after_discovery_without_new_events() {
        let (_dir, wallet, chain, receipt) = completed_fixture();
        let old_scope = wallet.recovery_scope().unwrap();
        let (bytes, _) = fixture_with_script(&[0x51]);
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        let before_events = expected_events(&evidence, &old_scope).unwrap();
        wallet
            .begin_regtest_pir_recovery(&chain, &RecoveryCancellation::new())
            .unwrap();
        let mut allocator = wallet.open_db().unwrap();
        let before_scan = NativeScanSnapshot::capture(&allocator, 1).unwrap();
        let seed = SecretVec::new(
            Mnemonic::parse_normalized(crate::keys::REGTEST_FAUCET_MNEMONIC)
                .unwrap()
                .to_seed("")
                .to_vec(),
        );
        let mut hash = crate::web::from_hex(chain.target_hash()).unwrap();
        hash.reverse();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                1.into(),
                BlockHash::from_slice(&hash),
            ),
            None,
        );
        allocator
            .import_account_hd(
                "future",
                &seed,
                Zip32AccountId::try_from(1).unwrap(),
                &birthday,
                None,
            )
            .unwrap();
        assert_eq!(
            NativeScanSnapshot::capture(&allocator, 1).unwrap(),
            before_scan
        );
        drop(allocator);
        let scope = wallet.recovery_scope().unwrap();
        assert_ne!(scope.identity().unwrap(), receipt.scope_identity);
        assert_eq!(expected_events(&evidence, &scope).unwrap(), before_events);
        let mut db = wallet.open_recovery_db().unwrap();
        let result = db.transactionally_with_extension(|wdb, ext| {
            require_journal_head(ext, receipt.journal_head.as_ref())?;
            let current_scope = native_scope_in_transaction(wdb, ext, 1)?;
            require_discovery_scope(&receipt.scope_identity, &current_scope)
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("native_recovery_reconciliation_required"));
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let pending: bool = conn
            .query_row(
                "SELECT blocked=1 AND receipt IS NULL FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(pending);
    }
    #[test]
    fn recovery_marker_writers_use_full_durability_and_pending_survives_early_return() {
        let (dir, wallet, chain, _) = completed_fixture();
        let ordinary =
            open_wallet_connection_with_durability(&wallet.paths.data_db, false).unwrap();
        assert_eq!(
            ordinary
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let recovery = open_wallet_connection_with_durability(&wallet.paths.data_db, true).unwrap();
        assert_eq!(
            recovery
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let generation = wallet
            .begin_regtest_pir_recovery(&chain, &RecoveryCancellation::new())
            .unwrap();
        drop(ordinary);
        drop(recovery);
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let actual: (bool, u64, bool) = conn.query_row("SELECT blocked,generation,receipt IS NULL FROM ext_coffer_pir_recovery_v1 WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(actual, (true, generation, true));
    }
    #[test]
    fn legacy_complete_without_discovery_scope_binding_refuses_on_reopen_and_payment_writer() {
        let (dir, wallet, chain, receipt) = completed_fixture();
        let mut old = serde_json::to_value(receipt).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("discovery_scope_identity");
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute(
            "UPDATE ext_coffer_pir_recovery_v1 SET receipt=?1 WHERE id=1",
            [serde_json::to_vec(&old).unwrap()],
        )
        .unwrap();
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        assert!(reopened.regtest_pir_recovery_for(&chain).is_err());
        let mut db = reopened.open_recovery_db().unwrap();
        assert!(db
            .transactionally_with_extension(|wdb, ext| {
                super::super::super::recovery_guard::transaction_recovery_ready(wdb, ext)
            })
            .is_err());
    }
    #[test]
    fn changed_journal_invalidates_complete_and_stale_receipts_on_cold_reopen() {
        let (dir, wallet, chain, receipt) = completed_fixture();
        let flag = AtomicBool::new(false);
        let mut store = store::NativePirStore::open(&wallet, &flag).unwrap();
        store.bind_set(&testing::identity()).unwrap();
        assert_ne!(Some(store.journal_head()), receipt.journal_head);
        drop(store);
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let (blocked, empty): (bool, bool) = conn
            .query_row(
                "SELECT blocked,receipt IS NULL FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(blocked && empty);
        // Even a stale Complete publication cannot authorize this new journal.
        conn.execute(
            "UPDATE ext_coffer_pir_recovery_v1 SET blocked=0,receipt=?1 WHERE id=1",
            [serde_json::to_vec(&receipt).unwrap()],
        )
        .unwrap();
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        assert!(reopened.regtest_pir_recovery_for(&chain).is_err());
        let mut db = reopened.open_db().unwrap();
        assert!(db
            .transactionally_with_extension(|_, ext| {
                require_journal_head(ext, receipt.journal_head.as_ref())
            })
            .is_err());
        assert!(db
            .transactionally_with_extension(|wdb, ext| {
                super::super::super::recovery_guard::transaction_recovery_ready(wdb, ext)
            })
            .is_err());
    }
    #[test]
    fn no_overlap_rewind_invalidates_complete_before_refusing_and_preserves_pending() {
        let (dir, wallet, mut fork, _) = completed_fixture();
        let wrong_hash = "11".repeat(32);
        fork.hashes.insert(1, wrong_hash.clone());
        fork.target.hash = wrong_hash;
        let before = wallet.scanned_height().unwrap();
        let error = wallet
            .rewind_regtest_pir(&fork, &RecoveryCancellation::new())
            .unwrap_err();
        assert!(error.to_string().contains("native_recovery_reset_required"));
        assert_eq!(wallet.scanned_height().unwrap(), before);
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let (blocked, empty): (bool, bool) = conn
            .query_row(
                "SELECT blocked,receipt IS NULL FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(blocked && empty);
    }
    #[test]
    fn rewind_refuses_non_genesis_birthday_before_persisting_any_recovery_marker() {
        let (_dir, mut wallet) = wallet();
        wallet.meta.birthday_height = 2;
        let (_, chain) = fixture_with_script(&[0x51]);
        assert!(wallet
            .rewind_regtest_pir(&chain, &RecoveryCancellation::new())
            .is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ext_coffer_pir_recovery_v1')", [],
            |r| r.get(0),
        ).unwrap();
        assert!(!exists);
        assert_eq!(wallet.birthday_height(), 2);
    }
    #[test]
    fn recovered_coinbase_maturity_is_native_policy_and_pending_still_blocks_payment() {
        let (_dir, wallet) = wallet();
        let db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let address = db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .into_keys()
            .next()
            .unwrap();
        let script: transparent::address::Script = address.script().into();
        for target in [1, 103] {
            let (bytes, chain) =
                super::super::super::recovery_blocks::tests::fixture_prefix(&script.0 .0, target);
            assert!(wallet
                .recover_regtest_pir(
                    &chain,
                    verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
                        .unwrap(),
                    &mut Unavailable,
                    &mut Unavailable,
                    WorkLimits {
                        max_queries: Some(1),
                        max_private_bytes: Some(1024)
                    },
                    &RecoveryCancellation::new()
                )
                .is_err());
            let available = wallet.balance().unwrap().transparent_available;
            if target == 1 {
                assert_eq!(available, 0);
            } else {
                assert!(available > 0);
            }
            assert!(wallet.ensure_recovery_selection_ready().is_err());
            assert!(wallet.prepare_payment("unused", 1, None).is_err());
        }
    }
    #[test]
    fn native_rewind_atomically_truncates_canonical_blocks_and_keeps_pending_on_reopen() {
        let (dir, wallet) = wallet();
        let db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let address = db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .into_keys()
            .next()
            .unwrap();
        let script: transparent::address::Script = address.script().into();
        let (bytes, chain) =
            super::super::super::recovery_blocks::tests::fixture_prefix(&script.0 .0, 2);
        assert!(wallet
            .recover_regtest_pir(
                &chain,
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap(),
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024)
                },
                &RecoveryCancellation::new()
            )
            .is_err());
        assert_eq!(wallet.scanned_height().unwrap(), 2);
        let (_, short) =
            super::super::super::recovery_blocks::tests::fixture_prefix(&script.0 .0, 1);
        assert_eq!(
            wallet
                .rewind_regtest_pir(&short, &RecoveryCancellation::new())
                .unwrap(),
            1
        );
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert_eq!(reopened.scanned_height().unwrap(), 1);
        let conn = rusqlite::Connection::open(&reopened.paths.data_db).unwrap();
        let (above,blocked):(u64,bool)=conn.query_row("SELECT (SELECT count(*) FROM blocks WHERE height>1),blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(above, 0);
        assert!(blocked);
        assert!(reopened.ensure_recovery_selection_ready().is_err());
    }
    #[test]
    fn allocated_unexposed_ephemeral_output_enters_native_canonical_store() {
        let (_dir, wallet) = wallet();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let encoded:String=conn.query_row("SELECT cached_transparent_receiver_address FROM addresses WHERE key_scope=2 ORDER BY id LIMIT 1",[],|r|r.get(0)).unwrap();
        let address = zcash_keys::address::Address::decode(&ZNetwork::Regtest, &encoded)
            .unwrap()
            .to_transparent_address()
            .unwrap();
        let script: transparent::address::Script = address.script().into();
        let (bytes, chain) = fixture_with_script(&script.0 .0);
        assert!(wallet
            .recover_regtest_pir(
                &chain,
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap(),
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024)
                },
                &RecoveryCancellation::new()
            )
            .is_err());
        let count:u64=conn.query_row("SELECT count(*) FROM transparent_received_outputs t JOIN addresses a ON a.id=t.address_id WHERE a.key_scope=2",[],|r|r.get(0)).unwrap();
        assert_eq!(count, 1);
        assert!(wallet.ensure_recovery_selection_ready().is_err());
        assert!(wallet
            .recovery_scope()
            .unwrap()
            .0
            .iter()
            .any(|entry| entry.entry.script == script.0 .0));
    }
    #[test]
    fn foreign_imported_receiver_rows_are_hard_refused_by_initial_profile() {
        let (_dir, wallet) = wallet();
        let (_, chain) = fixture_with_script(&[0x51]);
        wallet
            .begin_regtest_pir_recovery(&chain, &RecoveryCancellation::new())
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute("UPDATE addresses SET key_scope=-1,diversifier_index_be=NULL WHERE id=(SELECT min(id) FROM addresses WHERE key_scope=0)",[]).unwrap();
        assert!(wallet
            .recovery_scope()
            .unwrap_err()
            .to_string()
            .contains("native_recovery_imports_unsupported"));
    }
    #[test]
    fn conflicting_saved_raw_is_preserved_and_recovery_remains_pending() {
        let (_dir, wallet) = wallet();
        let db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let address = db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .into_keys()
            .next()
            .unwrap();
        let script: transparent::address::Script = address.script().into();
        let (bytes, chain) = fixture_with_script(&script.0 .0);
        let recover = || {
            wallet.recover_regtest_pir(
                &chain,
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap(),
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024),
                },
                &RecoveryCancellation::new(),
            )
        };
        assert!(recover().is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let original: Vec<u8> = conn
            .query_row(
                "SELECT raw FROM transactions WHERE mined_height=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut saved = original;
        saved.push(0x42);
        conn.execute(
            "UPDATE transactions SET raw=?1 WHERE mined_height=1",
            [&saved],
        )
        .unwrap();
        let error = recover().unwrap_err();
        assert!(error.to_string().contains("native_recovery_raw_conflict"));
        let after: Vec<u8> = conn
            .query_row(
                "SELECT raw FROM transactions WHERE mined_height=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, saved);
        assert_eq!(wallet.scanned_height().unwrap(), 1);
        assert!(wallet.ensure_recovery_selection_ready().is_err());
    }
    #[test]
    fn canonical_recovery_refuses_unverified_native_prefix_effects_and_retains_pending() {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix;

        for boundary in [4, 8] {
            let (_dir, wallet) = wallet();
            let (bytes, chain) = fixture_prefix(&[0x51], 8);
            let evidence =
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
            let mut forged = evidence.compact_blocks().unwrap();
            let (key, _) = fixture_account();
            crate::native::selective_scan::tests::pay_orchard(&mut forged[2], &key);
            forged[2].vtx.last_mut().unwrap().index = 1;
            let commitments = forged[2].vtx.iter().map(|tx| tx.actions.len() as u32).sum();
            for block in forged.iter_mut().skip(2) {
                block
                    .chain_metadata
                    .as_mut()
                    .unwrap()
                    .orchard_commitment_tree_size = commitments;
            }
            let mut genesis = crate::web::from_hex(GENESIS).unwrap();
            genesis.reverse();
            wallet
                .open_db()
                .unwrap()
                .transactionally_with_extension(|wdb, _| {
                    super::super::super::public_scan::scan_in_transaction(
                        wdb,
                        wallet.network,
                        forged[..boundary].to_vec(),
                        false,
                        BlockHash::from_slice(&genesis),
                    )
                })
                .unwrap();
            let before = NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 8).unwrap();
            let result = wallet.recover_regtest_pir(
                &chain,
                evidence,
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024),
                },
                &RecoveryCancellation::new(),
            );
            assert!(result.is_err());
            assert!(NativeScanSnapshot::capture(&wallet.open_db().unwrap(), 8).unwrap() == before);
            assert!(wallet.ensure_pir_recovery_ready().is_err());
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let blocked: bool = conn
                .query_row(
                    "SELECT blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(blocked);
        }
    }
    #[test]
    fn discovery_writes_bind_the_complete_allocated_canonical_scope() {
        use transparent_wallet::WalletStore;

        let (_dir, wallet) = wallet();
        let (_, chain) = fixture_with_script(&[0x51]);
        let token = RecoveryCancellation::new();
        wallet.begin_regtest_pir_recovery(&chain, &token).unwrap();
        let mut db = wallet.open_db().unwrap();
        db.update_chain_tip(1.into()).unwrap();
        let scope = wallet.recovery_scope().unwrap();
        assert!(scope != native_scope(&db, 1).unwrap());
        let mut store = store::NativePirStore::open(&wallet, token.flag()).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        let identity = transparent_wallet::testing::identity();
        store.bind_set(&identity).unwrap();
        drop(store);
        let mut store = store::NativePirStore::open(&wallet, token.flag()).unwrap();
        store.enroll(scope.clone(), 1).unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        assert_eq!(
            conn.execute(
                "DELETE FROM addresses WHERE id=(SELECT max(id) FROM addresses WHERE key_scope=2 AND exposed_at_height IS NULL)",
                [],
            )
            .unwrap(),
            1
        );
        assert!(wallet.recovery_scope().unwrap() != scope);
        let before: u64 = conn
            .query_row(
                "SELECT generation FROM ext_coffer_pir_head WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(store.bind_set(&identity).is_err());
        let after: u64 = conn
            .query_row(
                "SELECT generation FROM ext_coffer_pir_head WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, before);
        assert!(wallet.ensure_pir_recovery_ready().is_err());
    }
    #[test]
    fn begin_is_durable_and_all_allocated_native_receiver_scopes_are_enrolled() {
        let (_dir, wallet) = wallet();
        let (_, chain) = fixture_with_script(&[0x51]);
        let token = RecoveryCancellation::new();
        assert_eq!(
            wallet.begin_regtest_pir_recovery(&chain, &token).unwrap(),
            1
        );
        assert_eq!(
            wallet.begin_regtest_pir_recovery(&chain, &token).unwrap(),
            2
        );
        assert!(wallet.ensure_pir_recovery_ready().is_err());
        let (bytes, _) = fixture_with_script(&[0x51]);
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        let _ = wallet.recover_regtest_pir(
            &chain,
            evidence,
            &mut Unavailable,
            &mut Unavailable,
            WorkLimits {
                max_queries: Some(1),
                max_private_bytes: Some(1024),
            },
            &token,
        );
        let db = wallet.open_db().unwrap();
        let scope = wallet.recovery_scope().unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let mut statement=conn.prepare("SELECT cached_transparent_receiver_address FROM addresses WHERE key_scope=2 AND cached_transparent_receiver_address IS NOT NULL").unwrap();
        let allocated = statement
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(!allocated.is_empty());
        for encoded in allocated {
            let address = zcash_keys::address::Address::decode(&ZNetwork::Regtest, &encoded)
                .unwrap()
                .to_transparent_address()
                .unwrap();
            let script: transparent::address::Script = address.script().into();
            assert!(scope
                .0
                .iter()
                .any(|entry| entry.entry.script == script.0 .0));
        }
        for account in db.get_account_ids().unwrap() {
            for address in db
                .get_transparent_receivers(account, true, true)
                .unwrap()
                .into_keys()
                .chain(
                    db.get_ephemeral_transparent_receivers(account, u32::MAX, false)
                        .unwrap()
                        .into_keys(),
                )
            {
                let script: transparent::address::Script = address.script().into();
                assert!(scope
                    .0
                    .iter()
                    .any(|entry| entry.account == account.expose_uuid().to_string()
                        && entry.entry.script == script.0 .0));
            }
        }
        token.cancel();
        assert!(wallet.begin_regtest_pir_recovery(&chain, &token).is_err());
    }
}
