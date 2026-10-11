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
    scanner_identity: String,
    anchor: Anchor,
}
fn schema(wallet: &NativeWallet) -> Result<()> {
    let conn = rusqlite::Connection::open(&wallet.paths.data_db)
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
        schema(self)?;
        let mut db = self.open_db()?;
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
        schema(self)?;
        let generation = self.begin_regtest_pir_recovery(chain, cancel)?;
        let mut db = self.open_db()?;
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
                    compact,
                    true,
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
        let discovery = self.sync_regtest_pir(chain, filters, transport, limits, cancel.flag())?;
        let mut db = self.open_db()?;
        let scope = self.recovery_scope()?;
        let expected = expected_events(&evidence, &scope)?;
        let store = store::NativePirStore::open(self, cancel.flag()).map_err(store_failed)?;
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
                    scanner_identity: snapshot.identity().map_err(store_failed)?,
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
        if self.network != ZNetwork::Regtest {
            return Err(failed("native_recovery_regtest_only"));
        }
        let _lease = database_lease::shared(&self.paths.data_db)?;
        schema(self)?;
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
        let mut store = store::NativePirStore::open(self, cancel.flag()).map_err(store_failed)?;
        cancel.transaction(|| store.rewind_native(anchor, &snapshot).map_err(store_failed))
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
    #[test]
    fn writer_snapshot_refuses_scope_expansion_after_an_earlier_ready_check() {
        let (_dir, wallet) = wallet();
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
        let mut db = wallet.open_db().unwrap();
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
            scanner_identity: NativeScanSnapshot::capture(&db, 1)
                .unwrap()
                .identity()
                .unwrap(),
            anchor: chain.target.clone(),
        };
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute(
            "UPDATE ext_coffer_pir_recovery_v1 SET blocked=0,receipt=?1 WHERE id=1",
            [serde_json::to_vec(&receipt).unwrap()],
        )
        .unwrap();
        assert!(wallet.ensure_recovery_selection_ready().is_ok());
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
