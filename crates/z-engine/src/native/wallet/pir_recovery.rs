//! Canonical recovery owns its durable selection barrier and final receipt.
use super::super::{RecoveryCancellation, VerifiedRegtestRecoveryBlocks};
use super::*;
use rusqlite::OptionalExtension;
use transparent::bundle::{OutPoint, TxOut};
use transparent_events::{
    FeeState, ReceiveEvent, SpendEvent, TransactionMetadata, TransparentEvent, Txid,
};
use zcash_client_backend::{data_api::ll::LowLevelWalletWrite, wallet::WalletTx};
use zcash_keys::encoding::AddressCodec;
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
    effect_identity: Option<String>,
    #[serde(default)]
    journal_head: Option<(u64, String)>,
    anchor: Anchor,
}
// Raw authorizing data is not authenticated by accepted-header effect inclusion.
// Keep the wallet's saved bytes, but require an exact canonical encoding of the
// independently matched transaction identity before using its effects.
fn saved_transaction(
    raw: &[u8],
    txid: &[u8],
    branch: zcash_protocol::consensus::BranchId,
) -> anyhow::Result<zcash_primitives::transaction::Transaction> {
    let mut remaining = raw;
    let tx = zcash_primitives::transaction::Transaction::read(&mut remaining, branch)
        .map_err(|_| anyhow::anyhow!("native_recovery_raw_conflict"))?;
    let mut canonical = Vec::new();
    tx.write(&mut canonical)?;
    if !remaining.is_empty() || canonical != raw || tx.txid().as_ref().as_slice() != txid {
        anyhow::bail!("native_recovery_raw_conflict");
    }
    Ok(tx)
}

// Bind verified mined effects, while allowing native signing to add actual
// pending reservations. Each pending association must name an input in its
// saved canonical transaction; arbitrary extra relations are never exempted.
fn effect_identity(
    mut query: impl FnMut(&str) -> rusqlite::Result<String>,
    network: ZNetwork,
    height: u32,
) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    let mut authored = std::collections::BTreeSet::new();
    if query("SELECT CAST(EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_native_payments_v1') AS TEXT)")? == "1" {
        let rows = query("SELECT coalesce(group_concat(txids,'|'),'') FROM ext_native_payments_v1 WHERE phase != 'prepared'")?;
        for row in rows.split('|').filter(|r| !r.is_empty()) {
            authored.extend(serde_json::from_str::<Vec<String>>(row)?);
        }
    }
    for id in &authored {
        let mut bytes = crate::web::from_hex(id)
            .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
        if bytes.len() != 32 {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
        bytes.reverse();
        if query(&format!("SELECT CAST(EXISTS(SELECT 1 FROM transactions WHERE txid=x'{}' AND raw IS NOT NULL) AS TEXT)",crate::web::to_hex(&bytes)))? != "1" { anyhow::bail!("native_recovery_reconciliation_required"); }
    }
    let rows=query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(txid)||':'||hex(raw)||':'||(created IS NOT NULL) AS entry FROM transactions WHERE mined_height IS NULL ORDER BY txid)")?;
    for row in rows.split('|').filter(|r| !r.is_empty()) {
        let fields: Vec<_> = row.split(':').collect();
        if fields.len() != 3 {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
        let decode = |s: &str| {
            crate::web::from_hex(s)
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))
        };
        let id = decode(fields[0])?;
        let mut display = id.clone();
        display.reverse();
        if fields[2] != "1" && !authored.contains(&crate::web::to_hex(&display)) {
            continue;
        }
        let tx = saved_transaction(
            &decode(fields[1])?,
            &id,
            zcash_protocol::consensus::BranchId::for_height(&network, height.into()),
        )?;
        let mut inputs = Vec::new();
        if let Some(bundle) = tx.sapling_bundle() {
            inputs.extend(
                bundle
                    .shielded_spends()
                    .iter()
                    .map(|s| ("sapling", s.nullifier().0)),
            );
        }
        if let Some(bundle) = tx.orchard_bundle() {
            inputs.extend(
                bundle
                    .actions()
                    .iter()
                    .map(|a| ("orchard", a.nullifier().to_bytes())),
            );
        }
        if let Some(bundle) = tx.ironwood_bundle() {
            inputs.extend(
                bundle
                    .actions()
                    .iter()
                    .map(|a| ("ironwood", a.nullifier().to_bytes())),
            );
        }
        for (pool, nf) in inputs {
            let sql=format!("SELECT CAST(NOT EXISTS(SELECT 1 FROM {pool}_received_notes n WHERE n.nf=x'{}' AND NOT EXISTS(SELECT 1 FROM {pool}_received_note_spends s JOIN transactions t ON s.transaction_id=t.id_tx WHERE s.{pool}_received_note_id=n.id AND t.txid=x'{}')) AS TEXT)",crate::web::to_hex(&nf),crate::web::to_hex(&id));
            if query(&sql)? != "1" {
                anyhow::bail!("native_recovery_reconciliation_required");
            }
        }
        if let Some(bundle) = tx.transparent_bundle() {
            for input in &bundle.vin {
                let outpoint = input.prevout();
                let sql=format!("SELECT CAST(NOT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions parent ON n.transaction_id=parent.id_tx WHERE parent.txid=x'{}' AND n.output_index={} AND NOT EXISTS(SELECT 1 FROM transparent_received_output_spends s JOIN transactions t ON s.transaction_id=t.id_tx WHERE s.transparent_received_output_id=n.id AND t.txid=x'{}')) AS TEXT)",crate::web::to_hex(outpoint.hash()),outpoint.n(),crate::web::to_hex(&id));
                if query(&sql)? != "1" {
                    anyhow::bail!("native_recovery_reconciliation_required");
                }
            }
        }
    }
    for pool in ["sapling", "orchard", "ironwood"] {
        let index = if pool == "sapling" {
            "output_index"
        } else {
            "action_index"
        };
        for sql in [
            format!("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||n.{index}||':'||n.value||':'||hex(n.nf)||':'||coalesce(n.commitment_tree_position,-1) AS entry FROM {pool}_received_notes n JOIN transactions t ON n.transaction_id=t.id_tx WHERE t.mined_height IS NOT NULL ORDER BY t.txid,n.{index})"),
            format!("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(n.nf)||':'||hex(t.txid) AS entry FROM {pool}_received_note_spends s JOIN {pool}_received_notes n ON s.{pool}_received_note_id=n.id JOIN transactions t ON s.transaction_id=t.id_tx WHERE t.mined_height IS NOT NULL ORDER BY n.nf,t.txid)"),
        ] { hash.update(query(&sql)?.as_bytes()); hash.update([0]); }
        let pending = query(&format!("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||hex(t.raw)||':'||hex(n.nf)||':'||(t.created IS NOT NULL) AS entry FROM {pool}_received_note_spends s JOIN {pool}_received_notes n ON s.{pool}_received_note_id=n.id JOIN transactions t ON s.transaction_id=t.id_tx WHERE t.mined_height IS NULL ORDER BY t.txid,n.nf)"))?;
        for row in pending.split('|').filter(|r| !r.is_empty()) {
            let fields: Vec<_> = row.split(':').collect();
            if fields.len() != 4 {
                anyhow::bail!("native_recovery_reconciliation_required");
            }
            let id = crate::web::from_hex(fields[0])
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
            let raw = crate::web::from_hex(fields[1])
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
            let nf = crate::web::from_hex(fields[2])
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
            let tx = saved_transaction(
                &raw,
                &id,
                zcash_protocol::consensus::BranchId::for_height(&network, height.into()),
            )?;
            if fields[3] != "1" && !authored.contains(&tx.txid().to_string()) {
                anyhow::bail!("native_recovery_reconciliation_required");
            }
            let matches = match pool {
                "sapling" => tx.sapling_bundle().is_some_and(|b| {
                    b.shielded_spends()
                        .iter()
                        .any(|s| s.nullifier().0.as_slice() == nf)
                }),
                "orchard" => tx.orchard_bundle().is_some_and(|b| {
                    b.actions()
                        .iter()
                        .any(|a| a.nullifier().to_bytes().as_slice() == nf)
                }),
                _ => tx.ironwood_bundle().is_some_and(|b| {
                    b.actions()
                        .iter()
                        .any(|a| a.nullifier().to_bytes().as_slice() == nf)
                }),
            };
            if !matches {
                anyhow::bail!("native_recovery_reconciliation_required");
            }
        }
    }
    let metadata=query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(txid)||':'||mined_height||':'||coalesce(tx_index,-1)||':'||coalesce(expiry_height,-1)||':'||coalesce(fee,-1) AS entry FROM transactions WHERE mined_height IS NOT NULL ORDER BY txid)")?;
    hash.update(metadata.as_bytes());
    hash.update([0]);
    hash.update(
        super::super::recovery_effects::shielded_effect_identity(
            &mut query, network, height, &authored,
        )?
        .as_bytes(),
    );
    hash.update([0]);
    let received=query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||n.output_index||':'||hex(n.script)||':'||n.value_zat||':'||hex(a.uuid)||':'||n.address_id||':'||n.address||':'||coalesce(n.max_observed_unspent_height,-1)||':'||t.mined_height||':'||coalesce(t.tx_index,-1) AS entry FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id JOIN accounts a ON a.id=n.account_id WHERE t.mined_height IS NOT NULL ORDER BY t.txid,n.output_index)")?;
    hash.update(received.as_bytes());
    hash.update([0]);
    let pending_outputs=query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||hex(t.raw)||':'||(t.created IS NOT NULL)||':'||n.output_index||':'||hex(n.script)||':'||n.value_zat||':'||n.address||':'||(EXISTS(SELECT 1 FROM addresses a WHERE a.id=n.address_id AND a.account_id=n.account_id AND a.cached_transparent_receiver_address=n.address)) AS entry FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.mined_height IS NULL ORDER BY t.txid,n.output_index)")?;
    for row in pending_outputs.split('|').filter(|r| !r.is_empty()) {
        let f: Vec<_> = row.split(':').collect();
        if f.len() != 8 {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
        let tx = saved_transaction(
            &crate::web::from_hex(f[1])
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?,
            &crate::web::from_hex(f[0])
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?,
            zcash_protocol::consensus::BranchId::for_height(&network, height.into()),
        )?;
        let index: usize = f[3].parse()?;
        let script = crate::web::from_hex(f[4])
            .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
        let value: u64 = f[5].parse()?;
        let address = transparent::address::TransparentAddress::decode(&network, f[6])
            .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))?;
        let encoded_script: transparent::address::Script = address.script().into();
        if (f[2] != "1" && !authored.contains(&tx.txid().to_string()))
            || f[7] != "1"
            || encoded_script.0 .0 != script
            || !tx.transparent_bundle().is_some_and(|b| {
                b.vout.get(index).is_some_and(|o| {
                    o.script_pubkey().0 .0 == script && u64::from(o.value()) == value
                })
            })
        {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
    }
    let transparent = query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(parent.txid)||':'||n.output_index||':'||hex(t.txid) AS entry FROM transparent_received_output_spends s JOIN transparent_received_outputs n ON s.transparent_received_output_id=n.id JOIN transactions parent ON n.transaction_id=parent.id_tx JOIN transactions t ON s.transaction_id=t.id_tx WHERE t.mined_height IS NOT NULL ORDER BY parent.txid,n.output_index,t.txid)")?;
    hash.update(transparent.as_bytes());
    hash.update([0]);
    let pending = query("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||hex(t.raw)||':'||hex(parent.txid)||':'||n.output_index||':'||(t.created IS NOT NULL) AS entry FROM transparent_received_output_spends s JOIN transparent_received_outputs n ON s.transparent_received_output_id=n.id JOIN transactions parent ON n.transaction_id=parent.id_tx JOIN transactions t ON s.transaction_id=t.id_tx WHERE t.mined_height IS NULL UNION ALL SELECT hex(t.txid)||':'||hex(t.raw)||':'||hex(s.prevout_txid)||':'||s.prevout_output_index||':'||(t.created IS NOT NULL) FROM transparent_spend_map s JOIN transactions t ON t.id_tx=s.spending_transaction_id WHERE t.mined_height IS NULL)")?;
    for row in pending.split('|').filter(|r| !r.is_empty()) {
        let fields: Vec<_> = row.split(':').collect();
        if fields.len() != 5 {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
        let decode = |s: &str| {
            crate::web::from_hex(s)
                .map_err(|_| anyhow::anyhow!("native_recovery_reconciliation_required"))
        };
        let tx = saved_transaction(
            &decode(fields[1])?,
            &decode(fields[0])?,
            zcash_protocol::consensus::BranchId::for_height(&network, height.into()),
        )?;
        let parent = decode(fields[2])?;
        let index: u32 = fields[3].parse()?;
        if (fields[4] != "1" && !authored.contains(&tx.txid().to_string()))
            || !tx.transparent_bundle().is_some_and(|b| {
                b.vin.iter().any(|input| {
                    input.prevout().hash().as_slice() == parent && input.prevout().n() == index
                })
            })
        {
            anyhow::bail!("native_recovery_reconciliation_required");
        }
    }
    for sql in [
        "SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||hex(s.prevout_txid)||':'||s.prevout_output_index AS entry FROM transparent_spend_map s JOIN transactions t ON t.id_tx=s.spending_transaction_id WHERE t.mined_height IS NOT NULL ORDER BY t.txid,s.prevout_txid,s.prevout_output_index)",
        "SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT spend_pool||':'||hex(nf)||':'||block_height||':'||tx_index AS entry FROM nullifier_map ORDER BY spend_pool,nf)",
        "SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT block_height||':'||tx_index||':'||hex(txid) AS entry FROM tx_locator_map ORDER BY block_height,tx_index)",
    ] { hash.update(query(sql)?.as_bytes()); hash.update([0]); }
    Ok(format!("{:x}", hash.finalize()))
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
fn events_disagree(
    expected: &BTreeMap<(Vec<u8>, Vec<u8>), TransactionMetadata>,
    events: Vec<StoredEvent>,
) -> bool {
    let mut actual = BTreeMap::new();
    let mut discrepancy = false;
    for event in events {
        let key = event_key(&event.script, event.event);
        match event.event.metadata() {
            Some(metadata) if expected.get(&key) == Some(&metadata) => {}
            _ => discrepancy = true,
        }
        if actual.insert(key, ()).is_some() {
            discrepancy = true;
        }
    }
    discrepancy || actual.len() != expected.len() || actual.keys().ne(expected.keys())
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
    if receipt.effect_identity.as_deref()
        != Some(
            effect_identity(
                |sql| ext.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                receipt.report.accepted_height,
            )?
            .as_str(),
        )
    {
        anyhow::bail!("native_recovery_reconciliation_required");
    }
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
            || receipt.effect_identity.as_deref()
                != Some(
                    effect_identity(
                        |sql| conn.query_row(sql, [], |r| r.get(0)),
                        self.network,
                        receipt.report.accepted_height,
                    )
                    .map_err(|_| failed("native_recovery_reconciliation_required"))?
                    .as_str(),
                )
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
        let mut db = self.open_pir_db(Some(cancel))?;
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
        self.recover_regtest_pir_after_replay(
            chain,
            evidence,
            filters,
            transport,
            limits,
            cancel,
            || {},
        )
    }
    fn recover_regtest_pir_after_replay(
        &self,
        chain: &RegtestAcceptedChain,
        evidence: VerifiedRegtestRecoveryBlocks,
        filters: &mut impl FilterSource,
        transport: &mut impl ShardTransport,
        limits: WorkLimits,
        cancel: &RecoveryCancellation,
        after_replay: impl FnOnce(),
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
        let mut db = self.open_pir_db(Some(cancel))?;
        let compact = evidence.compact_blocks()?;
        let mut genesis = crate::web::from_hex(
            &chain
                .hash_at(0)
                .ok_or_else(|| failed("native_recovery_chain_invalid"))?,
        )
        .map_err(|_| failed("native_recovery_chain_invalid"))?;
        genesis.reverse();
        let mut recovered_outputs = 0usize;
        let mut before = 0usize;
        let mut saved = BTreeMap::new();
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
                before = ext.query_row(
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
                            let retained = saved_transaction(
                                &raw,
                                tx.txid().as_ref().as_slice(),
                                tx.consensus_branch_id(),
                            )?;
                            let mut incoming = Vec::new();
                            tx.write(&mut incoming)?;
                            if raw != incoming {
                                saved.insert(tx.txid(), retained);
                            }
                        }
                    }
                }
                // Retained chain/hash/root overlap is checked before any cleanup.
                // A changed overlap still requires an explicit native rewind.
                super::super::public_scan::scan_in_transaction(
                    wdb,
                    self.network,
                    compact.clone(),
                    true,
                    BlockHash::from_slice(&genesis),
                )?;
                Ok(())
            })
            .map_err(EngineError::from)
        })?;
        let pending = self.saved_pending_transactions(generation, &evidence, cancel)?;
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
                                saved.get(&tx.txid()).unwrap_or(tx),
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
                    for tx in &pending {
                        decrypt_and_store_transaction(&self.network, wdb, tx, None)?;
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
        after_replay();
        let reconciled_scope =
            self.reconcile_verified_effects(generation, &evidence, &pending, cancel)?;
        let discovery = self.sync_regtest_pir_with_commit_gate(
            chain,
            filters,
            transport,
            limits,
            cancel.flag(),
            Some(cancel),
        )?;
        self.finish_regtest_pir_recovery(
            chain,
            &evidence,
            generation,
            recovered_outputs,
            &reconciled_scope,
            discovery,
            cancel,
        )
    }
    fn finish_regtest_pir_recovery(
        &self,
        chain: &RegtestAcceptedChain,
        evidence: &VerifiedRegtestRecoveryBlocks,
        generation: u64,
        recovered_outputs: usize,
        reconciled_scope: &str,
        discovery: PirDiscoveryReport,
        cancel: &RecoveryCancellation,
    ) -> Result<PirRecoveryReport> {
        let mut db = self.open_pir_db(Some(cancel))?;
        let scope = cancel.transaction(|| {
            db.transactionally_with_extension(|wdb, ext| native_scope_in_transaction(wdb, ext, 1))
                .map_err(EngineError::from)
        })?;
        let expected = expected_events(evidence, &scope)?;
        let store = store::NativePirStore::open_recovery(self, cancel).map_err(store_failed)?;
        let discrepancy = events_disagree(&expected, store.events().map_err(store_failed)?);
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
                    reconciled_scope,
                    &native_scope_in_transaction(wdb, ext, 1)?,
                )?;
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
                    effect_identity: Some(effect_identity(
                        |sql| ext.query_row(sql, [], |r| r.get(0)),
                        self.network,
                        chain.target_height(),
                    )?),
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
    fn saved_pending_transactions(
        &self,
        generation: u64,
        evidence: &VerifiedRegtestRecoveryBlocks,
        cancel: &RecoveryCancellation,
    ) -> Result<Vec<zcash_primitives::transaction::Transaction>> {
        cancel.transaction(|| {
            (|| -> anyhow::Result<Vec<zcash_primitives::transaction::Transaction>> {
            let mut conn = rusqlite::Connection::open(&self.paths.data_db)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.pragma_update(None, "synchronous", "FULL")?;
            conn.pragma_update(None, "foreign_keys", true)?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state: (u64, bool) = tx.query_row("SELECT generation,blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if state != (generation, true) { anyhow::bail!("native_recovery_generation_changed"); }
            let verified: std::collections::BTreeSet<Vec<u8>> = evidence.blocks.iter().flat_map(|b| b.vtx()).map(|t| t.txid().as_ref().to_vec()).collect();
            let mut authored = std::collections::BTreeSet::new();
            let intents: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_native_payments_v1')", [], |r| r.get(0))?;
            if intents {
                let mut stmt = tx.prepare("SELECT txids FROM ext_native_payments_v1 WHERE phase != 'prepared'")?;
                for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                    for id in serde_json::from_str::<Vec<String>>(&row?)? { authored.insert(id); }
                }
            }
            let mut pending = Vec::new();
            {
                let mut stmt = tx.prepare("SELECT txid,raw,created IS NOT NULL FROM transactions WHERE mined_height IS NULL")?;
                for row in stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?,r.get::<_, Option<Vec<u8>>>(1)?,r.get::<_, bool>(2)?)))? {
                    let (id, raw, created) = row?;
                    if verified.contains(&id) { continue; }
                    let mut display = id.clone(); display.reverse();
                    if created || authored.contains(&crate::web::to_hex(&display)) {
                        let raw = raw.ok_or_else(|| anyhow::anyhow!("native_recovery_raw_conflict"))?;
                        pending.push(saved_transaction(&raw, &id, zcash_protocol::consensus::BranchId::for_height(&self.network, evidence.target_height().into()))?);
                    }
                }
            }
            if cancel.is_cancelled() { anyhow::bail!("native_recovery_cancelled"); }
            tx.commit()?;
            Ok::<_, anyhow::Error>(pending)
            })().map_err(EngineError::from)
        })
    }

    fn reconcile_verified_effects(
        &self,
        generation: u64,
        evidence: &VerifiedRegtestRecoveryBlocks,
        pending: &[zcash_primitives::transaction::Transaction],
        cancel: &RecoveryCancellation,
    ) -> Result<String> {
        let scope = self.recovery_scope()?;
        cancel.transaction(|| {
            (|| -> anyhow::Result<String> {
                let mut conn = rusqlite::Connection::open(&self.paths.data_db)?;
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn.pragma_update(None, "synchronous", "FULL")?;
                conn.pragma_update(None, "foreign_keys", true)?;
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let state: (u64, bool) = tx.query_row("SELECT generation,blocked FROM ext_coffer_pir_recovery_v1 WHERE id=1", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
                if state != (generation, true) { anyhow::bail!("native_recovery_generation_changed"); }
                tx.execute_batch("CREATE TEMP TABLE expected_spends(pool TEXT,nf BLOB,txid BLOB);
                    CREATE TEMP TABLE expected_transparent_spends(parent BLOB,output_index INTEGER,txid BLOB);
                    CREATE TEMP TABLE expected_outputs(txid BLOB,output_index INTEGER,script BLOB,value INTEGER);
                    CREATE TEMP TABLE expected_owners(script BLOB,account_id INTEGER);
                    CREATE TEMP TABLE expected_locators(height INTEGER,tx_index INTEGER,txid BLOB);
                    CREATE TEMP TABLE expected_nullifiers(pool INTEGER,nf BLOB,height INTEGER,tx_index INTEGER);")?;
                for entry in &scope.0 {
                    let uuid = crate::web::from_hex(&entry.account.replace('-', "")).map_err(|_|anyhow::anyhow!("native_recovery_reconciliation_required"))?;
                    let account: i64 = tx.query_row("SELECT id FROM accounts WHERE uuid=?1", [&uuid], |r| r.get(0))?;
                    tx.execute("INSERT INTO expected_owners VALUES(?1,?2)", rusqlite::params![entry.entry.script,account])?;
                }
                let add = |transaction: &zcash_primitives::transaction::Transaction, locator: Option<(u32,usize)>| -> anyhow::Result<()> {
                    let id = transaction.txid();
                    let mut nullifiers = Vec::new();
                    if let Some(b) = transaction.sapling_bundle() { nullifiers.extend(b.shielded_spends().iter().map(|s| ("sapling",2,s.nullifier().0))); }
                    if let Some(b) = transaction.orchard_bundle() { nullifiers.extend(b.actions().iter().map(|a| ("orchard",3,a.nullifier().to_bytes()))); }
                    if let Some(b) = transaction.ironwood_bundle() { nullifiers.extend(b.actions().iter().map(|a| ("ironwood",4,a.nullifier().to_bytes()))); }
                    for (pool,code,nf) in nullifiers {
                        tx.execute("INSERT INTO expected_spends VALUES(?1,?2,?3)",rusqlite::params![pool,nf.as_slice(),id.as_ref().as_slice()])?;
                        if let Some((height,index)) = locator { tx.execute("INSERT INTO expected_nullifiers VALUES(?1,?2,?3,?4)",rusqlite::params![code,nf.as_slice(),height,index])?; }
                    }
                    if let Some((height,index)) = locator { tx.execute("INSERT INTO expected_locators VALUES(?1,?2,?3)",rusqlite::params![height,index,id.as_ref().as_slice()])?; }
                    if let Some(b) = transaction.transparent_bundle() {
                        if !b.is_coinbase() { for input in &b.vin { let p=input.prevout(); tx.execute("INSERT INTO expected_transparent_spends VALUES(?1,?2,?3)",rusqlite::params![p.hash().as_slice(),p.n(),id.as_ref().as_slice()])?; } }
                        for (index,output) in b.vout.iter().enumerate() { tx.execute("INSERT INTO expected_outputs VALUES(?1,?2,?3,?4)",rusqlite::params![id.as_ref().as_slice(),index,output.script_pubkey().0.0,u64::from(output.value())])?; }
                    }
                    Ok(())
                };
                for block in &evidence.blocks { for (index,transaction) in block.vtx().iter().enumerate() { add(transaction,Some((u32::from(block.claimed_height()),index)))?; } }
                let mut previous=BTreeMap::<OutPoint,TxOut>::new();
                for block in &evidence.blocks { for (index,transaction) in block.vtx().iter().enumerate() {
                    let fee=if transaction.transparent_bundle().is_some_and(|b|b.is_coinbase()) { None } else { transaction.fee_paid::<anyhow::Error,_>(|outpoint|Ok(previous.get(outpoint).map(|o|o.value())))?.map(u64::from) };
                    tx.execute("UPDATE transactions SET fee=?1,expiry_height=?2,mined_height=?3,tx_index=?4 WHERE txid=?5",rusqlite::params![fee,u32::from(transaction.expiry_height()),u32::from(block.claimed_height()),index,transaction.txid().as_ref().as_slice()])?;
                    if let Some(bundle)=transaction.transparent_bundle() { for (index,output) in bundle.vout.iter().enumerate() { previous.insert(OutPoint::new(*transaction.txid().as_ref(),u32::try_from(index)?),output.clone()); } }
                } }
                for transaction in pending { add(transaction,None)?; }
                // Never discard a purported owned parent of an unresolved authored input.
                for transaction in pending { if let Some(b) = transaction.transparent_bundle() { for input in &b.vin {
                    let p=input.prevout();
                    let invalid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions t ON n.transaction_id=t.id_tx WHERE t.txid=?1 AND n.output_index=?2 AND NOT EXISTS(SELECT 1 FROM expected_outputs e WHERE e.txid=t.txid AND e.output_index=n.output_index AND e.script=n.script AND e.value=n.value_zat))",rusqlite::params![p.hash().as_slice(),p.n()],|r|r.get(0))?;
                    if invalid { anyhow::bail!("native_recovery_reconciliation_required"); }
                } } }
                super::super::recovery_effects::reconcile_shielded_outputs(&tx,self.network,evidence,pending,cancel)?;
                // Replay has already committed verified additions. Remove only relations
                // absent from the exact verified or authored pending inputs, atomically.
                for pool in ["sapling","orchard","ironwood"] {
                    tx.execute(&format!("DELETE FROM {pool}_received_note_spends AS s WHERE NOT EXISTS(SELECT 1 FROM {pool}_received_notes n JOIN transactions t ON t.id_tx=s.transaction_id JOIN expected_spends e ON e.pool='{pool}' AND e.nf=n.nf AND e.txid=t.txid WHERE n.id=s.{pool}_received_note_id)"),[])?;
                }
                tx.execute("DELETE FROM transparent_received_output_spends AS s WHERE NOT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions parent ON parent.id_tx=n.transaction_id JOIN transactions t ON t.id_tx=s.transaction_id JOIN expected_transparent_spends e ON e.parent=parent.txid AND e.output_index=n.output_index AND e.txid=t.txid WHERE n.id=s.transparent_received_output_id)",[])?;
                tx.execute("DELETE FROM transparent_spend_map AS s WHERE NOT EXISTS(SELECT 1 FROM transactions t JOIN expected_transparent_spends e ON e.txid=t.txid AND e.parent=s.prevout_txid AND e.output_index=s.prevout_output_index WHERE t.id_tx=s.spending_transaction_id)",[])?;
                tx.execute("DELETE FROM nullifier_map AS n WHERE NOT EXISTS(SELECT 1 FROM expected_nullifiers e WHERE e.pool=n.spend_pool AND e.nf=n.nf AND e.height=n.block_height AND e.tx_index=n.tx_index)",[])?;
                tx.execute("DELETE FROM tx_locator_map AS n WHERE NOT EXISTS(SELECT 1 FROM expected_locators e WHERE e.height=n.block_height AND e.tx_index=n.tx_index AND e.txid=n.txid)",[])?;
                tx.execute("DELETE FROM transparent_received_output_spends WHERE transparent_received_output_id IN(SELECT n.id FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE NOT EXISTS(SELECT 1 FROM expected_outputs e WHERE e.txid=t.txid AND e.output_index=n.output_index))",[])?;
                tx.execute("DELETE FROM transparent_received_outputs AS n WHERE NOT EXISTS(SELECT 1 FROM transactions t JOIN expected_outputs e ON e.txid=t.txid AND e.output_index=n.output_index WHERE t.id_tx=n.transaction_id)",[])?;
                let invalid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE NOT EXISTS(SELECT 1 FROM expected_outputs e JOIN expected_owners o ON o.script=e.script AND o.account_id=n.account_id JOIN addresses a ON a.id=n.address_id AND a.account_id=n.account_id AND a.cached_transparent_receiver_address=n.address WHERE e.txid=t.txid AND e.output_index=n.output_index AND e.script=n.script AND e.value=n.value_zat))",[],|r|r.get(0))?;
                if invalid { anyhow::bail!("native_recovery_reconciliation_required"); }
                // Every verified output matching the final enrolled scripts must be
                // present, not merely every retained row valid. Allocation can occur
                // after replay, so missing historical receivers must remain Pending.
                let missing: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM expected_outputs e JOIN expected_owners o ON o.script=e.script WHERE NOT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=e.txid AND n.output_index=e.output_index AND n.script=e.script AND n.value_zat=e.value AND n.account_id=o.account_id))",[],|r|r.get(0))?;
                if missing { anyhow::bail!("native_recovery_reconciliation_required"); }
                super::super::recovery_effects::reconcile_sent_outputs(&tx,self.network,evidence,pending,cancel)?;
                effect_identity(|sql| tx.query_row(sql, [], |r| r.get(0)),self.network,evidence.target_height())?;
                if cancel.is_cancelled() { anyhow::bail!("native_recovery_cancelled"); }
                let reconciled_scope = scope.identity()?;
                tx.commit()?;
                Ok(reconciled_scope)
            })().map_err(EngineError::from)
        })
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
        let db = self.open_pir_db(Some(cancel))?;
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
            || receipt.effect_identity.as_deref()
                != Some(
                    effect_identity(
                        |sql| conn.query_row(sql, [], |r| r.get(0)),
                        self.network,
                        chain.target_height(),
                    )
                    .map_err(|_| failed("native_recovery_reconciliation_required"))?
                    .as_str(),
                )
        {
            return Err(failed("native_recovery_reconciliation_required"));
        }
        Ok(Some(receipt.report))
    }
}

#[cfg(test)]
pub(in crate::native::wallet) mod tests {
    use super::*;
    use crate::native::wallet::recovery_blocks::{
        tests::fixture_with_script, verify_regtest_recovery_blocks,
    };
    use crate::native::wallet::tests::{fixture_account, fixture_wallet};
    use zcash_client_backend::data_api::WalletCommitmentTrees;
    #[test]
    #[ignore = "private offline categorical diagnostic"]
    fn retained_fork_raw_categories() {
        use zcash_primitives::{block::Block, transaction::Transaction};
        use zcash_protocol::consensus::BranchId;
        let root = std::path::PathBuf::from(std::env::var("COFFER_PRIVATE_FIXTURE").unwrap());
        let conn = rusqlite::Connection::open_with_flags(
            root.join("canonical-target/data.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut identical = 0usize;
        let mut auth_variant = 0usize;
        let mut malformed = 0usize;
        for entry in std::fs::read_dir(root.join("fork/canonical-blocks")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                continue;
            }
            let bytes = std::fs::read(path).unwrap();
            let mut cursor = &bytes[b"COFFER-REGTEST-BLOCKS-V1\n".len()..];
            let _start = u32::from_le_bytes(cursor[..4].try_into().unwrap());
            cursor = &cursor[4..];
            let count = u32::from_le_bytes(cursor[..4].try_into().unwrap());
            cursor = &cursor[4..];
            for _ in 0..count {
                let len = u32::from_le_bytes(cursor[..4].try_into().unwrap()) as usize;
                cursor = &cursor[4..];
                let block = Block::read(&cursor[..len], &ZNetwork::Regtest).unwrap();
                cursor = &cursor[len..];
                for tx in block.vtx() {
                    let raw: Option<Vec<u8>> = conn
                        .query_row(
                            "SELECT raw FROM transactions WHERE txid=?1",
                            [tx.txid().as_ref().as_slice()],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap()
                        .flatten();
                    if let Some(raw) = raw {
                        let mut incoming = vec![];
                        tx.write(&mut incoming).unwrap();
                        if raw == incoming {
                            identical += 1;
                            continue;
                        }
                        let mut remaining = &raw[..];
                        match Transaction::read(&mut remaining, BranchId::Nu6_3) {
                            Ok(saved) if remaining.is_empty() && saved.txid() == tx.txid() => {
                                let mut canonical = vec![];
                                saved.write(&mut canonical).unwrap();
                                if canonical == raw {
                                    auth_variant += 1;
                                } else {
                                    malformed += 1;
                                }
                            }
                            _ => malformed += 1,
                        }
                    }
                }
            }
        }
        eprintln!("retained raw categories: identical={identical}, canonical_same_effect_auth_variants={auth_variant}, malformed_or_effect_conflicts={malformed}");
        assert_eq!(malformed, 0);
    }
    #[tokio::test]
    #[ignore = "private retained fork classification diagnostic"]
    async fn retained_fork_reference_classifies_local_coinbases_and_matches_canonical_balance() {
        let root = std::path::PathBuf::from(std::env::var("COFFER_PRIVATE_FIXTURE").unwrap());
        let reference_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        for (name, dir) in [
            ("canonical-reference", &reference_dir),
            ("canonical-target", &target_dir),
        ] {
            for file in ["wallet.json", "seed.enc", "data.sqlite"] {
                std::fs::copy(root.join(name).join(file), dir.path().join(file)).unwrap();
            }
        }
        let reference = NativeWallet::open_offline(reference_dir.path())
            .unwrap_or_else(|_| panic!("private_reference_open_failed"));
        let target = NativeWallet::open_offline(target_dir.path())
            .unwrap_or_else(|_| panic!("private_target_open_failed"));
        assert!(matches!(
            reference.balance(),
            Err(EngineError::SyncRequired)
        ));
        let mut db = reference.open_db().unwrap();
        let classified = crate::native::transparent_funding::classify_balance_funding(
            &mut db,
            &reference.paths.data_db,
            reference.network,
            |_| async { panic!("retained_fork_must_not_refetch") },
        )
        .await
        .unwrap_or_else(|_| panic!("private_local_classification_failed"));
        assert_eq!(classified, 2);
        drop(db);
        let cold = NativeWallet::open_offline(reference_dir.path())
            .unwrap_or_else(|_| panic!("private_reference_reopen_failed"));
        let reference_balance = cold
            .balance()
            .unwrap_or_else(|_| panic!("private_reference_balance_failed"));
        let target_balance = target
            .balance()
            .unwrap_or_else(|_| panic!("private_target_balance_failed"));
        assert!(
            serde_json::to_value(reference_balance).unwrap()
                == serde_json::to_value(target_balance).unwrap()
        );
    }

    #[test]
    #[ignore = "private retained fork replay diagnostic"]
    fn retained_fork_replay_preserves_saved_raw_and_reaches_verified_native_tip() {
        let root = std::path::PathBuf::from(std::env::var("COFFER_PRIVATE_FIXTURE").unwrap());
        let stage = tempfile::tempdir().unwrap();
        for name in ["wallet.json", "seed.enc", "data.sqlite"] {
            std::fs::copy(
                root.join("canonical-target").join(name),
                stage.path().join(name),
            )
            .unwrap();
        }
        let trust: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("fork/accepted-headers.json")).unwrap(),
        )
        .unwrap();
        let headers: Vec<(u32, String)> = serde_json::from_value(trust["headers"].clone()).unwrap();
        let chain = RegtestAcceptedChain::from_local_node(
            crate::native::RegtestScanSchedule {
                nu6_3_height: 150,
                nu7_height: None,
            },
            headers,
        )
        .unwrap();
        let mut batches = BTreeMap::new();
        for entry in std::fs::read_dir(root.join("fork/canonical-blocks")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|p| p.to_str()) != Some("bin") {
                continue;
            }
            let bytes = std::fs::read(path).unwrap();
            let offset = b"COFFER-REGTEST-BLOCKS-V1\n".len();
            let start = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            batches.insert(start, bytes[offset + 8..].to_vec());
        }
        let mut bytes = b"COFFER-REGTEST-BLOCKS-V1\n".to_vec();
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(chain.target_height().to_le_bytes());
        for body in batches.values() {
            bytes.extend(body);
        }
        let evidence = verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
            .unwrap_or_else(|_| panic!("offline_verified_prefix_invalid"));
        let conn = rusqlite::Connection::open(stage.path().join("data.sqlite")).unwrap();
        let saved = conn
            .prepare("SELECT txid,raw FROM transactions WHERE raw IS NOT NULL")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let wallet = NativeWallet::open(stage.path())
            .unwrap_or_else(|_| panic!("offline_native_open_failed"));
        let error = wallet
            .recover_regtest_pir(
                &chain,
                evidence,
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024),
                },
                &RecoveryCancellation::new(),
            )
            .err();
        assert!(error
            .as_ref()
            .is_none_or(|e| !e.to_string().contains("native_recovery_raw_conflict")));
        assert!(error
            .as_ref()
            .is_none_or(|e| !e.to_string().contains("public_scan")));
        assert_eq!(wallet.scanned_height().unwrap(), chain.target_height());
        for (id, raw) in &saved {
            assert!(
                conn.query_row("SELECT raw FROM transactions WHERE txid=?1", [id], |r| r
                    .get::<_, Vec<u8>>(0))
                    .unwrap()
                    == *raw
            );
        }
        assert!(effect_identity(
            |sql| conn.query_row(sql, [], |r| r.get(0)),
            wallet.network,
            chain.target_height()
        )
        .is_ok());
        eprintln!("retained fork offline replay reached verified tip and preserved all saved raw rows; transport remains unavailable and receipt remains Pending");
        assert!(wallet.ensure_pir_recovery_ready().is_err());
    }
    pub(in crate::native::wallet) fn encrypted_orchard_transaction(
        key: &zcash_keys::keys::UnifiedFullViewingKey,
        seed: u8,
        spend: [u8; 32],
    ) -> zcash_primitives::transaction::Transaction {
        use orchard::{
            note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
            note_encryption::{OrchardDomain, OrchardNoteEncryption},
            value::NoteValue,
            Note,
        };
        use zcash_note_encryption::Domain;
        use zcash_primitives::transaction::{Authorized, Transaction, TransactionData, TxVersion};
        use zcash_protocol::consensus::BranchId;
        let rho = Rho::from_bytes(&spend).unwrap();
        let note = Note::from_parts(
            key.orchard()
                .unwrap()
                .address_at(0u32, zip32::Scope::External),
            NoteValue::from_raw(50_000),
            rho,
            RandomSeed::from_bytes([seed; 32], &rho).unwrap(),
            NoteVersion::V2,
        )
        .unwrap();
        let cmx = ExtractedNoteCommitment::from(note.commitment()).to_bytes();
        let enc = OrchardNoteEncryption::new(None, note, [0; 512]);
        let epk = OrchardDomain::epk_bytes(enc.epk()).0;
        let empty = TransactionData::<Authorized>::from_parts(
            TxVersion::V5,
            BranchId::Nu6_2,
            0,
            0.into(),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        let mut raw = vec![];
        empty.write(&mut raw).unwrap();
        raw.pop();
        raw.push(1);
        raw.extend([0; 32]);
        raw.extend(spend);
        raw.extend(epk);
        raw.extend(cmx);
        raw.extend(epk);
        raw.extend(enc.encrypt_note_plaintext());
        raw.extend([0; 80]);
        raw.push(3);
        raw.extend(0i64.to_le_bytes());
        raw.extend([0; 32]);
        let length = orchard::Proof::expected_proof_size(1);
        raw.push(253);
        raw.extend((length as u16).to_le_bytes());
        raw.extend(vec![0; length]);
        raw.extend([0; 128]);
        // Effects/encryption fixture only: dummy proofs/signatures do not claim
        // consensus-valid authorization. Accepted fixture headers bind its txid.
        Transaction::read(&raw[..], BranchId::Nu6_2).unwrap()
    }
    fn finish_fixture_recovery(wallet: &NativeWallet, bytes: &[u8], chain: &RegtestAcceptedChain) {
        let cancel = RecoveryCancellation::new();
        let evidence =
            verify_regtest_recovery_blocks(bytes, chain, &AtomicBool::new(false)).unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let generation = conn
            .query_row(
                "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let scope = wallet.recovery_scope().unwrap();
        let expected = expected_events(&evidence, &scope).unwrap();
        if !expected.is_empty() {
            let mut store = store::NativePirStore::open_recovery(wallet, &cancel).unwrap();
            let covered_scripts = scope.0.iter().map(|s| s.entry.script.clone()).collect();
            store.add_scripts(&scope.scripts()).unwrap();
            store
                .commit_shard(ShardCommit {
                    shard_id: 0,
                    revision_digest: "verified-events-fixture".into(),
                    sealed: true,
                    start_height: 1,
                    end_height: u64::from(chain.target_height()),
                    terminal_block_hash: chain.target.hash.clone(),
                    events: expected
                        .into_iter()
                        .map(|((script, bytes), metadata)| StoredEvent {
                            script,
                            event: TransparentEvent::from_bytes(&bytes)
                                .unwrap()
                                .with_metadata(Some(metadata)),
                            shard_id: 0,
                            revision_digest: "verified-events-fixture".into(),
                        })
                        .collect(),
                    covered_scripts,
                    ..Default::default()
                })
                .unwrap();
        }
        let snapshot =
            NativeScanSnapshot::capture(&wallet.open_db().unwrap(), chain.target_height()).unwrap();
        let discovery = PirDiscoveryReport {
            completion: "complete-for-enrolled-scope".into(),
            reconciled: true,
            accepted_anchor: Some(chain.target.clone()),
            source_identity: "fixture".into(),
            context_identity: chain.context_identity.clone(),
            scope_identity: scope.identity().unwrap(),
            enrolled_script_count: scope.0.len(),
            covered_through: u64::from(chain.target_height()),
            settled_through: u64::from(chain.target_height()),
            confirmed_ledger_zat: 0,
            history: vec![],
            pending_pages: 0,
            unresolved_spends: 0,
            native_scanned_height: chain.target_height(),
            native_chain_height: Some(chain.target_height()),
            native_anchor_hash: Some(chain.target.hash.clone()),
            native_scanner_identity: Some(snapshot.identity().unwrap()),
            publication_start: 1,
        };
        assert!(
            wallet
                .finish_regtest_pir_recovery(
                    chain,
                    &evidence,
                    generation,
                    0,
                    &scope.identity().unwrap(),
                    discovery,
                    &cancel
                )
                .unwrap()
                .scope_complete
        );
    }
    fn assert_selected_orchard_witnesses(wallet: &NativeWallet, count: usize) {
        use zcash_client_backend::data_api::{
            wallet::input_selection::{LockFilter, LockedInputPolicy},
            InputSource, TargetValue,
        };
        use zcash_protocol::{value::Zatoshis, ShieldedPool};
        let mut db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        let (target, anchor) = db
            .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
            .unwrap()
            .unwrap();
        let notes = db
            .select_spendable_notes(
                account,
                TargetValue::AtLeast(Zatoshis::from_u64(100_000).unwrap()),
                &[ShieldedPool::Orchard],
                target,
                crate::confirmations_policy(ZNetwork::Regtest),
                &[],
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .unwrap();
        assert_eq!(notes.orchard().len(), count);
        db.with_orchard_tree_mut::<_, _, anyhow::Error>(|tree| {
            let root = tree.root_at_checkpoint_id(&anchor)?.unwrap();
            for note in notes.orchard() {
                let witness = tree
                    .witness_at_checkpoint_id_caching(
                        note.note_commitment_tree_position(),
                        &anchor,
                    )?
                    .unwrap();
                let leaf =
                    orchard::tree::MerkleHashOrchard::from_cmx(&note.note().commitment().into());
                assert_eq!(witness.root(leaf), root);
            }
            Ok(())
        })
        .unwrap();
    }
    #[test]
    fn canonical_replay_removes_invented_transparent_outputs_preserves_locks_and_binds_history_metadata(
    ) {
        let (dir, wallet) = wallet();
        let script = wallet.recovery_scope().unwrap().0[0].entry.script.clone();
        let (bytes, chain) = fixture_with_script(&script);
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
        let original: i64 = conn
            .query_row("SELECT id FROM transparent_received_outputs", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("UPDATE transparent_received_outputs SET lock_owner=x'0102',lock_expiry_height=100 WHERE id=?1",[original]).unwrap();
        conn.execute_batch("INSERT INTO transparent_received_outputs(transaction_id,output_index,account_id,address_id,address,script,value_zat,max_observed_unspent_height) SELECT transaction_id,99,account_id,address_id,address,script,123456,1 FROM transparent_received_outputs LIMIT 1; UPDATE transactions SET fee=12345 WHERE mined_height=1;").unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM transparent_received_outputs",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            2
        );
        assert!(recover().is_err());
        let retained: (i64, Vec<u8>, u32) = conn
            .query_row(
                "SELECT id,lock_owner,lock_expiry_height FROM transparent_received_outputs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(retained.0, original);
        assert!(retained.1 == [1, 2]);
        assert_eq!(retained.2, 100);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM transparent_received_outputs",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            1
        );
        assert!(conn
            .query_row(
                "SELECT fee IS NULL FROM transactions WHERE mined_height=1",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
        finish_fixture_recovery(&wallet, &bytes, &chain);
        let cold = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(cold.ensure_pir_recovery_ready().is_ok());
        assert!(
            cold.regtest_pir_recovery_for(&chain)
                .unwrap()
                .unwrap()
                .scope_complete
        );
        for sql in [
            "UPDATE transactions SET fee=10000 WHERE mined_height=1",
            "UPDATE transparent_received_outputs SET value_zat=value_zat+1",
        ] {
            conn.execute_batch(sql).unwrap();
            let cold = NativeWallet::open_offline(dir.path()).unwrap();
            assert!(cold.ensure_pir_recovery_ready().is_err());
            assert!(cold.regtest_pir_recovery_for(&chain).is_err());
            let mut db = cold.open_recovery_db().unwrap();
            assert!(db
                .transactionally_with_extension(|wdb, ext| ensure_transaction_receipt(wdb, ext))
                .is_err());
        }
    }
    #[test]
    fn replay_failure_and_cancellation_preserve_committed_spends_and_viewing_balance() {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix_with_transactions;
        use prost::Message;
        for abort_replay in [false, true] {
            let (dir, wallet) = wallet();
            let (key, _) = fixture_account();
            let (prefix, prefix_chain) = fixture_prefix_with_transactions(
                &[0x51],
                3,
                vec![(3, encrypted_orchard_transaction(&key, 7, [0; 32]))],
            );
            let scan = |bytes: &[u8], chain: &RegtestAcceptedChain| {
                let compact = verify_regtest_recovery_blocks(bytes, chain, &AtomicBool::new(false))
                    .unwrap()
                    .compact_blocks()
                    .unwrap();
                wallet
                    .scan_public_regtest(
                        &compact
                            .iter()
                            .flat_map(Message::encode_length_delimited_to_vec)
                            .collect::<Vec<_>>(),
                        crate::native::RegtestScanSchedule {
                            nu6_3_height: crate::regtest_nu6_3_height(),
                            nu7_height: crate::regtest_nu7_height(),
                        },
                    )
                    .unwrap();
            };
            scan(&prefix, &prefix_chain);
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let nf: Vec<u8> = conn
                .query_row("SELECT nf FROM orchard_received_notes", [], |r| r.get(0))
                .unwrap();
            let foreign = crate::native::selective_scan::tests::test_ufvk(8);
            let (bytes, chain) = fixture_prefix_with_transactions(
                &[0x51],
                8,
                vec![
                    (3, encrypted_orchard_transaction(&key, 7, [0; 32])),
                    (
                        6,
                        encrypted_orchard_transaction(&foreign, 9, nf.try_into().unwrap()),
                    ),
                ],
            );
            scan(&bytes, &chain);
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM orchard_received_note_spends",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
                1
            );
            if abort_replay {
                conn.execute_batch("CREATE TRIGGER abort_verified_replay BEFORE INSERT ON orchard_received_notes BEGIN SELECT RAISE(ABORT,'fixture_replay_failure'); END;").unwrap();
            }
            let cancel = RecoveryCancellation::new();
            let error = wallet
                .recover_regtest_pir_after_replay(
                    &chain,
                    verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
                        .unwrap(),
                    &mut Unavailable,
                    &mut Unavailable,
                    WorkLimits {
                        max_queries: Some(1),
                        max_private_bytes: Some(1024),
                    },
                    &cancel,
                    || cancel.cancel(),
                )
                .unwrap_err();
            assert!(abort_replay || error.to_string().contains("native_recovery_cancelled"));
            let cold = NativeWallet::open_offline(dir.path()).unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM orchard_received_note_spends",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
                1
            );
            assert_eq!(cold.balance().unwrap().orchard_available, 0);
            assert!(!cold.history(100).unwrap().is_empty());
            assert!(cold.ensure_pir_recovery_ready().is_err());
            assert!(cold.regtest_pir_recovery_for(&chain).is_err());
        }
    }
    #[test]
    fn a_conflicting_mined_spend_refuses_replay_without_unlocking_the_prior_view() {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix_with_transactions;
        use prost::Message;
        let (dir, wallet) = wallet();
        let (key, _) = fixture_account();
        let foreign = crate::native::selective_scan::tests::test_ufvk(8);
        let (prefix, initial_chain) = fixture_prefix_with_transactions(
            &[0x51],
            3,
            vec![(3, encrypted_orchard_transaction(&key, 7, [0; 32]))],
        );
        let schedule = crate::native::RegtestScanSchedule {
            nu6_3_height: crate::regtest_nu6_3_height(),
            nu7_height: crate::regtest_nu7_height(),
        };
        let initial =
            verify_regtest_recovery_blocks(&prefix, &initial_chain, &AtomicBool::new(false))
                .unwrap()
                .compact_blocks()
                .unwrap();
        wallet
            .scan_public_regtest(
                &initial
                    .iter()
                    .flat_map(Message::encode_length_delimited_to_vec)
                    .collect::<Vec<_>>(),
                schedule,
            )
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let nf: Vec<u8> = conn
            .query_row("SELECT nf FROM orchard_received_notes", [], |r| r.get(0))
            .unwrap();
        let (bytes, chain) = fixture_prefix_with_transactions(
            &[0x51],
            8,
            vec![
                (3, encrypted_orchard_transaction(&key, 7, [0; 32])),
                (6, encrypted_orchard_transaction(&foreign, 9, [0; 32])),
                (
                    7,
                    encrypted_orchard_transaction(&foreign, 11, nf.clone().try_into().unwrap()),
                ),
            ],
        );
        let mut forged = verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
            .unwrap()
            .compact_blocks()
            .unwrap();
        forged[5].vtx[1].actions[0].nullifier = nf;
        forged[6].vtx[1].actions[0].nullifier = vec![0; 32];
        wallet
            .scan_public_regtest(
                &forged
                    .iter()
                    .flat_map(Message::encode_length_delimited_to_vec)
                    .collect::<Vec<_>>(),
                schedule,
            )
            .unwrap();
        let prior_spend: u64 = conn
            .query_row(
                "SELECT transaction_id FROM orchard_received_note_spends",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let prior_history = serde_json::to_value(wallet.history(100).unwrap()).unwrap();
        assert_eq!(wallet.balance().unwrap().orchard_available, 0);
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
        assert_eq!(
            conn.query_row(
                "SELECT transaction_id FROM orchard_received_note_spends",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            prior_spend
        );
        let cold = NativeWallet::open_offline(dir.path()).unwrap();
        assert_eq!(cold.balance().unwrap().orchard_available, 0);
        assert!(serde_json::to_value(cold.history(100).unwrap()).unwrap() == prior_history);
        assert!(cold.ensure_pir_recovery_ready().is_err());
        assert!(cold.regtest_pir_recovery_for(&chain).is_err());
    }
    #[test]
    fn canonical_replay_removes_forged_nullifiers_and_preserves_verified_spends_and_witnesses() {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix_with_transactions;
        use prost::Message;
        for genuinely_spent in [false, true] {
            let (dir, wallet) = wallet();
            let (key, _) = fixture_account();
            let stranger = crate::native::selective_scan::tests::test_ufvk(8);
            let payment = encrypted_orchard_transaction(&key, 7, [0; 32]);
            let (prefix, _) = fixture_prefix_with_transactions(&[0x51], 3, vec![(3, payment)]);
            let prefix_chain = fixture_prefix_with_transactions(
                &[0x51],
                3,
                vec![(3, encrypted_orchard_transaction(&key, 7, [0; 32]))],
            )
            .1;
            let initial =
                verify_regtest_recovery_blocks(&prefix, &prefix_chain, &AtomicBool::new(false))
                    .unwrap()
                    .compact_blocks()
                    .unwrap();
            wallet
                .scan_public_regtest(
                    &initial
                        .iter()
                        .flat_map(Message::encode_length_delimited_to_vec)
                        .collect::<Vec<_>>(),
                    crate::native::RegtestScanSchedule {
                        nu6_3_height: crate::regtest_nu6_3_height(),
                        nu7_height: crate::regtest_nu7_height(),
                    },
                )
                .unwrap();
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let nf: Vec<u8> = conn
                .query_row("SELECT nf FROM orchard_received_notes", [], |r| r.get(0))
                .unwrap();
            let spend = if genuinely_spent {
                nf.clone().try_into().unwrap()
            } else {
                [0; 32]
            };
            let (bytes, chain) = fixture_prefix_with_transactions(
                &[0x51],
                8,
                vec![
                    (3, encrypted_orchard_transaction(&key, 7, [0; 32])),
                    (6, encrypted_orchard_transaction(&stranger, 9, spend)),
                ],
            );
            let verified = verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
                .unwrap()
                .compact_blocks()
                .unwrap();
            let mut forged = verified.clone();
            forged[5].vtx[1].actions[0].nullifier = nf.clone();
            assert_eq!(forged[5].hash, verified[5].hash);
            assert_eq!(forged[5].chain_metadata, verified[5].chain_metadata);
            assert_eq!(
                forged[5].vtx[1].actions[0].cmx,
                verified[5].vtx[1].actions[0].cmx
            );
            wallet
                .scan_public_regtest(
                    &forged
                        .iter()
                        .flat_map(Message::encode_length_delimited_to_vec)
                        .collect::<Vec<_>>(),
                    crate::native::RegtestScanSchedule {
                        nu6_3_height: crate::regtest_nu6_3_height(),
                        nu7_height: crate::regtest_nu7_height(),
                    },
                )
                .unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM orchard_received_note_spends",
                    [],
                    |r| r.get::<_, u32>(0)
                )
                .unwrap(),
                1
            );
            // A provider can also leave a locator for the forged spend;
            // cleanup must actually cascade it out of the nullifier map.
            conn.execute(
                "INSERT OR IGNORE INTO tx_locator_map(block_height,tx_index,txid) VALUES(6,1,?1)",
                [forged[5].vtx[1].txid.as_slice()],
            )
            .unwrap();
            conn.execute("INSERT INTO nullifier_map(spend_pool,nf,block_height,tx_index) VALUES(3,?1,6,1) ON CONFLICT(spend_pool,nf) DO UPDATE SET block_height=6,tx_index=1",[&nf]).unwrap();
            let _recovery_error = wallet
                .recover_regtest_pir(
                    &chain,
                    verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
                        .unwrap(),
                    &mut Unavailable,
                    &mut Unavailable,
                    WorkLimits {
                        max_queries: Some(1),
                        max_private_bytes: Some(1024),
                    },
                    &RecoveryCancellation::new(),
                )
                .unwrap_err();

            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM orchard_received_note_spends",
                    [],
                    |r| r.get::<_, u32>(0)
                )
                .unwrap(),
                u32::from(genuinely_spent)
            );
            if !genuinely_spent {
                assert_eq!(
                    conn.query_row(
                        "SELECT count(*) FROM nullifier_map WHERE nf=?1",
                        [&nf],
                        |r| r.get::<_, u32>(0)
                    )
                    .unwrap(),
                    0
                );
            }
            let (_oracle_dir, oracle) = self::wallet();
            oracle
                .scan_public_regtest(
                    &verified
                        .iter()
                        .flat_map(Message::encode_length_delimited_to_vec)
                        .collect::<Vec<_>>(),
                    crate::native::RegtestScanSchedule {
                        nu6_3_height: crate::regtest_nu6_3_height(),
                        nu7_height: crate::regtest_nu7_height(),
                    },
                )
                .unwrap();
            // Compare with an ordinary native scan enhanced from the same raw
            // fixture, so its fee metadata is ready rather than still compact-only.
            let verified_raw =
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
            oracle
                .open_db()
                .unwrap()
                .transactionally(|wdb| {
                    for block in &verified_raw.blocks {
                        for tx in block.vtx() {
                            decrypt_and_store_transaction(
                                &oracle.network,
                                wdb,
                                tx,
                                Some(block.claimed_height()),
                            )?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .unwrap();
            assert!(
                serde_json::to_value(wallet.history(100).unwrap()).unwrap()
                    == serde_json::to_value(oracle.history(100).unwrap()).unwrap()
            );
            finish_fixture_recovery(&wallet, &bytes, &chain);
            let cold = NativeWallet::open(dir.path()).unwrap();
            cold.ensure_pir_recovery_ready().unwrap();
            assert_selected_orchard_witnesses(&cold, usize::from(!genuinely_spent));
            if !genuinely_spent {
                conn.execute("INSERT INTO orchard_received_note_spends SELECT n.id,t.id_tx FROM orchard_received_notes n,transactions t WHERE t.mined_height=6 AND t.tx_index=1",[]).unwrap();
                assert!(NativeWallet::open(dir.path())
                    .unwrap()
                    .ensure_pir_recovery_ready()
                    .is_err());
                let mut db = cold.open_db().unwrap();
                assert!(db
                    .transactionally_with_extension(|wdb, ext| ensure_transaction_receipt(wdb, ext))
                    .is_err());
            }
        }
    }
    #[test]
    fn canonical_replay_rebuilds_only_actual_saved_pending_reservations_and_refuses_malformed_intents(
    ) {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix_with_transactions;
        use prost::Message;
        let (dir, wallet) = wallet();
        let (key, _) = fixture_account();
        let stranger = crate::native::selective_scan::tests::test_ufvk(8);
        let (bytes, chain) = fixture_prefix_with_transactions(
            &[0x51],
            8,
            vec![
                (3, encrypted_orchard_transaction(&key, 7, [0; 32])),
                (4, encrypted_orchard_transaction(&key, 9, [0; 32])),
            ],
        );
        let compact = verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false))
            .unwrap()
            .compact_blocks()
            .unwrap();
        wallet
            .scan_public_regtest(
                &compact
                    .iter()
                    .flat_map(Message::encode_length_delimited_to_vec)
                    .collect::<Vec<_>>(),
                crate::native::RegtestScanSchedule {
                    nu6_3_height: crate::regtest_nu6_3_height(),
                    nu7_height: crate::regtest_nu7_height(),
                },
            )
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let nf:Vec<u8>=conn.query_row("SELECT n.nf FROM orchard_received_notes n JOIN transactions t ON n.transaction_id=t.id_tx WHERE t.mined_height=3",[],|r|r.get(0)).unwrap();
        let pending = encrypted_orchard_transaction(&stranger, 11, nf.try_into().unwrap());
        let mut raw = vec![];
        pending.write(&mut raw).unwrap();
        let mut db = wallet.open_db().unwrap();
        db.transactionally(|wdb| {
            decrypt_and_store_transaction(&wallet.network, wdb, &pending, None)
        })
        .unwrap();
        conn.execute_batch("CREATE TABLE ext_native_payments_v1(id TEXT PRIMARY KEY, proposal BLOB NOT NULL,fee INTEGER NOT NULL,created INTEGER NOT NULL,expires INTEGER NOT NULL,height INTEGER NOT NULL,block_hash BLOB NOT NULL,phase TEXT NOT NULL,txids TEXT NOT NULL)").unwrap();
        let intent = serde_json::to_string(&vec![pending.txid().to_string()]).unwrap();
        conn.execute("INSERT INTO ext_native_payments_v1 VALUES('saved',x'00',10000,1,2,8,x'00','unknown',?1)",[&intent]).unwrap();
        conn.execute("UPDATE orchard_received_notes SET lock_owner=x'0102',lock_expiry_height=100 WHERE transaction_id IN(SELECT id_tx FROM transactions WHERE mined_height=3)",[]).unwrap();
        conn.execute("INSERT INTO orchard_received_note_spends SELECT n.id,t.id_tx FROM orchard_received_notes n JOIN transactions parent ON n.transaction_id=parent.id_tx,transactions t WHERE parent.mined_height=4 AND t.txid=?1",[pending.txid().as_ref().as_slice()]).unwrap();
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
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM orchard_received_note_spends",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(conn.query_row("SELECT parent.mined_height FROM orchard_received_note_spends s JOIN orchard_received_notes n ON s.orchard_received_note_id=n.id JOIN transactions parent ON n.transaction_id=parent.id_tx",[],|r|r.get::<_,u32>(0)).unwrap(),3);
        assert!(
            conn.query_row(
                "SELECT raw FROM transactions WHERE txid=?1",
                [pending.txid().as_ref().as_slice()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap()
                == raw
        );
        assert!(
            conn.query_row(
                "SELECT phase||':'||txids FROM ext_native_payments_v1 WHERE id='saved'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap()
                == format!("unknown:{intent}")
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM orchard_received_notes WHERE lock_owner=x'0102' AND lock_expiry_height=100",[],|r|r.get::<_,u32>(0)).unwrap(),1);
        finish_fixture_recovery(&wallet, &bytes, &chain);
        let cold = NativeWallet::open(dir.path()).unwrap();
        cold.ensure_pir_recovery_ready().unwrap();
        assert_selected_orchard_witnesses(&cold, 1);
        conn.execute("DELETE FROM orchard_received_note_spends WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert!(NativeWallet::open(dir.path())
            .unwrap()
            .ensure_pir_recovery_ready()
            .is_err());
        let mut writer = cold.open_db().unwrap();
        assert!(writer
            .transactionally_with_extension(|wdb, ext| ensure_transaction_receipt(wdb, ext))
            .is_err());
        writer
            .transactionally(|wdb| {
                decrypt_and_store_transaction(&wallet.network, wdb, &pending, None)
            })
            .unwrap();
        NativeWallet::open(dir.path())
            .unwrap()
            .ensure_pir_recovery_ready()
            .unwrap();
        // A valid pending raw transaction cannot legitimize an extra unrelated
        // association introduced after Complete, even though its txid is real.
        conn.execute("INSERT INTO orchard_received_note_spends SELECT n.id,t.id_tx FROM orchard_received_notes n JOIN transactions parent ON n.transaction_id=parent.id_tx,transactions t WHERE parent.mined_height=4 AND t.txid=?1",[pending.txid().as_ref().as_slice()]).unwrap();
        assert!(NativeWallet::open(dir.path())
            .unwrap()
            .ensure_pir_recovery_ready()
            .is_err());
        let before: u32 = conn
            .query_row(
                "SELECT count(*) FROM orchard_received_note_spends",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut malformed = raw;
        malformed.push(0x42);
        conn.execute(
            "UPDATE transactions SET raw=?1 WHERE txid=?2",
            rusqlite::params![malformed, pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert!(recover()
            .unwrap_err()
            .to_string()
            .contains("native_recovery_raw_conflict"));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM orchard_received_note_spends",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            before
        );
        assert!(
            conn.query_row(
                "SELECT raw FROM transactions WHERE txid=?1",
                [pending.txid().as_ref().as_slice()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap()
                == malformed
        );
        assert_eq!(
            conn.query_row(
                "SELECT phase FROM ext_native_payments_v1 WHERE id='saved'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "unknown"
        );
        assert!(NativeWallet::open(dir.path())
            .unwrap()
            .ensure_pir_recovery_ready()
            .is_err());
    }
    #[test]
    fn same_effect_authorization_variants_preserve_the_wallet_saved_raw_and_replay_metadata() {
        use crate::native::wallet::recovery_blocks::tests::fixture_prefix_with_transactions;
        let (_dir, wallet) = wallet();
        let (key, _) = fixture_account();
        let tx = encrypted_orchard_transaction(&key, 7, [0; 32]);
        let id = tx.txid();
        let (bytes, chain) = fixture_prefix_with_transactions(&[0x51], 8, vec![(3, tx)]);
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
                "SELECT raw FROM transactions WHERE txid=?1",
                [id.as_ref().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        let mut saved = original.clone();
        let end = saved.len() - 1;
        saved[end] ^= 1;
        // Binding-signature bytes are authorizing data excluded from ZIP244's
        // effect txid. Neither the fixture nor evidence authenticates them.
        assert!(saved_transaction(
            &saved,
            id.as_ref().as_slice(),
            zcash_protocol::consensus::BranchId::Nu6_2
        )
        .is_ok());
        conn.execute(
            "UPDATE transactions SET raw=?1 WHERE txid=?2",
            rusqlite::params![saved, id.as_ref().as_slice()],
        )
        .unwrap();
        assert!(recover().is_err());
        assert!(
            conn.query_row(
                "SELECT raw FROM transactions WHERE txid=?1",
                [id.as_ref().as_slice()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap()
                == saved
        );
        assert_eq!(
            conn.query_row(
                "SELECT mined_height,tx_index FROM transactions WHERE txid=?1",
                [id.as_ref().as_slice()],
                |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
            )
            .unwrap(),
            (3, 1)
        );
        assert_selected_orchard_witnesses(&wallet, 1);
    }
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
    fn cancelled_recovery_database_open_cannot_initialize_or_migrate_after_return() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE retained_fixture(value INTEGER); INSERT INTO retained_fixture VALUES(7);",
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&wallet.paths.data_db).unwrap();
        let cancel = RecoveryCancellation::new();
        cancel.cancel();
        assert!(wallet.open_pir_db(Some(&cancel)).is_err());
        assert!(std::fs::read(&wallet.paths.data_db).unwrap() == before);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let schemas: u64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(schemas, 1);
        drop(conn);
        // The exact same retained database genuinely needs and can run native
        // initialization when its canonical cancellation token is still live.
        let live = RecoveryCancellation::new();
        let db = wallet.open_pir_db(Some(&live)).unwrap();
        assert!(db.get_account_ids().unwrap().is_empty());
        drop(db);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        assert_eq!(
            conn.query_row("SELECT value FROM retained_fixture", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            7
        );
    }
    #[test]
    fn durable_discovery_events_require_verified_metadata_before_completion() {
        for variant in 0..3 {
            let (dir, wallet) = wallet();
            let scope = wallet.recovery_scope().unwrap();
            let script = scope.0[0].entry.script.clone();
            let (bytes, chain) = fixture_with_script(&script);
            let evidence =
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
            let expected = expected_events(&evidence, &scope).unwrap();
            assert_eq!(expected.len(), 1);
            let ((script, event_bytes), metadata) = expected.iter().next().unwrap();
            let mut event = TransparentEvent::from_bytes(event_bytes).unwrap();
            let mut supplied = *metadata;
            if variant == 1 {
                supplied.has_shielded_components = !supplied.has_shielded_components;
            }
            match &mut event {
                TransparentEvent::Receive(e) => e.metadata = (variant != 0).then_some(supplied),
                TransparentEvent::Spend(e) => e.metadata = (variant != 0).then_some(supplied),
            }
            let cancel = RecoveryCancellation::new();
            // Replay independently verified native blocks through public recovery;
            // transport failure leaves its actual durable Pending marker in place.
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
                    &cancel,
                )
                .is_err());
            let generation: u64 = rusqlite::Connection::open(&wallet.paths.data_db)
                .unwrap()
                .query_row(
                    "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let mut store = store::NativePirStore::open_recovery(&wallet, &cancel).unwrap();
            store
                .add_scripts(&scope.0.iter().map(|s| s.entry.clone()).collect::<Vec<_>>())
                .unwrap();
            store
                .commit_shard(ShardCommit {
                    shard_id: 0,
                    revision_digest: "metadata-fixture".into(),
                    sealed: true,
                    start_height: 1,
                    end_height: 1,
                    terminal_block_hash: chain.target.hash.clone(),
                    events: vec![StoredEvent {
                        script: script.clone(),
                        event,
                        shard_id: 0,
                        revision_digest: "metadata-fixture".into(),
                    }],
                    covered_scripts: vec![script.clone()],
                    ..Default::default()
                })
                .unwrap();
            drop(store);
            let cold = NativeWallet::open(dir.path()).unwrap();
            let store = store::NativePirStore::open_recovery(&cold, &cancel).unwrap();
            // Reconciliation reads the durable journal, not the verified native
            // transaction metadata or an in-memory provider response.
            assert_eq!(
                events_disagree(&expected, store.events().unwrap()),
                variant != 2
            );
            drop(store);
            let snapshot = NativeScanSnapshot::capture(&cold.open_db().unwrap(), 1).unwrap();
            let final_scope = cold.recovery_scope().unwrap();
            // Isolate the production completion boundary from PIR transport: claim
            // complete discovery while its journal is missing/wrong/correct metadata.
            let discovery = PirDiscoveryReport {
                completion: "complete-for-enrolled-scope".into(),
                reconciled: true,
                accepted_anchor: Some(chain.target.clone()),
                source_identity: "fixture".into(),
                context_identity: chain.context_identity.clone(),
                scope_identity: final_scope.identity().unwrap(),
                enrolled_script_count: final_scope.0.len(),
                covered_through: 1,
                settled_through: 1,
                confirmed_ledger_zat: 0,
                history: vec![],
                pending_pages: 0,
                unresolved_spends: 0,
                native_scanned_height: 1,
                native_chain_height: Some(1),
                native_anchor_hash: Some(chain.target.hash.clone()),
                native_scanner_identity: Some(snapshot.identity().unwrap()),
                publication_start: 1,
            };
            let report = cold
                .finish_regtest_pir_recovery(
                    &chain,
                    &evidence,
                    generation,
                    1,
                    &final_scope.identity().unwrap(),
                    discovery,
                    &cancel,
                )
                .unwrap();
            assert_eq!(report.scope_complete, variant == 2);
            assert_eq!(report.selection_blocked, variant != 2);
            assert_eq!(
                report.completion,
                if variant == 2 {
                    "complete"
                } else {
                    "provider-discrepancy"
                }
            );
            let reopened = NativeWallet::open(dir.path()).unwrap();
            let receipt = reopened.regtest_pir_recovery_for(&chain).unwrap().unwrap();
            assert_eq!(receipt.scope_complete, variant == 2);
            assert_eq!(reopened.ensure_pir_recovery_ready().is_ok(), variant == 2);
        }
    }
    pub(in crate::native::wallet) fn completed_wallet_fixture(
    ) -> (tempfile::TempDir, NativeWallet, RegtestAcceptedChain) {
        let (dir, wallet, chain, _) = completed_fixture();
        (dir, wallet, chain)
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
        let db_conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
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
            effect_identity: Some(
                effect_identity(
                    |sql| db_conn.query_row(sql, [], |r| r.get(0)),
                    wallet.network,
                    1,
                )
                .unwrap(),
            ),
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
    fn proposal_only_selectors_refuse_pending_and_stale_completed_recovery() {
        let (dir, wallet) = wallet();
        let script = wallet.recovery_scope().unwrap().0[0].entry.script.clone();
        let (bytes, chain) = fixture_with_script(&script);
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
        finish_fixture_recovery(&wallet, &bytes, &chain);
        let destination = wallet.unified_address().unwrap();
        // Mutating a receipt-bound native effect must block proposal-only APIs too.
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        assert!(conn.execute("UPDATE transactions SET fee=123", []).unwrap() > 0);
        let cold = NativeWallet::open_offline(dir.path()).unwrap();
        for result in [
            cold.estimate_fee(&destination, 1, None),
            cold.max_send(None).map(|v| v.0),
        ] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("native_recovery_reconciliation_required"));
        }
        cold.begin_regtest_pir_recovery(&chain, &RecoveryCancellation::new())
            .unwrap();
        for result in [
            cold.estimate_fee(&destination, 1, None),
            cold.max_send(None).map(|v| v.0),
        ] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("native_recovery_pending"));
        }
    }

    #[test]
    fn funded_receiver_allocated_after_replay_stays_pending_until_verified_replay_retries() {
        let (dir, wallet) = wallet();
        let (_oracle_dir, oracle) = self::wallet();
        let before = wallet.recovery_scope().unwrap();
        let allocate = |wallet: &NativeWallet| {
            let seed = SecretVec::new(
                Mnemonic::parse_normalized(crate::keys::REGTEST_FAUCET_MNEMONIC)
                    .unwrap()
                    .to_seed("")
                    .to_vec(),
            );
            let mut hash = crate::web::from_hex(GENESIS).unwrap();
            hash.reverse();
            let birthday = AccountBirthday::from_parts(
                zcash_client_backend::data_api::chain::ChainState::empty(
                    0.into(),
                    BlockHash::from_slice(&hash),
                ),
                None,
            );
            wallet
                .open_db()
                .unwrap()
                .import_account_hd(
                    "future",
                    &seed,
                    Zip32AccountId::try_from(1).unwrap(),
                    &birthday,
                    None,
                )
                .unwrap();
        };
        // An actual native HD account import allocates its receivers. Derive the
        // funded receiver independently, then allocate it only after replay.
        allocate(&oracle);
        let future = oracle
            .recovery_scope()
            .unwrap()
            .0
            .into_iter()
            .find(|entry| {
                !before
                    .0
                    .iter()
                    .any(|old| old.entry.script == entry.entry.script)
            })
            .expect("future account receiver outside enrolled scope");
        let (bytes, chain) = fixture_with_script(&future.entry.script);
        let cancel = RecoveryCancellation::new();
        let error = wallet
            .recover_regtest_pir_after_replay(
                &chain,
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap(),
                &mut Unavailable,
                &mut Unavailable,
                WorkLimits {
                    max_queries: Some(1),
                    max_private_bytes: Some(1024),
                },
                &cancel,
                || allocate(&wallet),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("native_recovery_reconciliation_required"));
        let cold = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(cold.ensure_pir_recovery_ready().is_err());
        assert!(cold.regtest_pir_recovery_for(&chain).is_err());
        assert!(cold
            .estimate_fee(&cold.unified_address().unwrap(), 1, None)
            .is_err());
        let conn = rusqlite::Connection::open(&cold.paths.data_db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM transparent_received_outputs WHERE script=?1",
                [&future.entry.script],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            0
        );
        // The next actual public recovery replays with the enlarged scope. The
        // unavailable provider still refuses discovery; isolate completion using
        // its real verified journal/receipt writer after checking the native row.
        let _ = cold.recover_regtest_pir(
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
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM transparent_received_outputs WHERE script=?1",
                [&future.entry.script],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            1
        );
        finish_fixture_recovery(&cold, &bytes, &chain);
        let reopened = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(
            reopened
                .regtest_pir_recovery_for(&chain)
                .unwrap()
                .unwrap()
                .scope_complete
        );
        reopened.ensure_pir_recovery_ready().unwrap();
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
