//! Source-bound canonical transparent application. No address queries or raw-transaction fabrication.
use super::*;
use crate::transparent_pir::{
    parse, refused, ProjectionState, TransparentPirAnchor, TransparentPirCheckpoint,
    TransparentPirScope, TransparentPirScript,
};
use rusqlite::OptionalExtension;
use transparent::{
    address::Script,
    bundle::{OutPoint, TxOut},
};
use transparent_events::TransparentEvent;
use zcash_client_backend::{
    data_api::ll::{LowLevelWalletRead, LowLevelWalletWrite},
    wallet::{WalletTransparentOutput, WalletTx},
};

pub struct NativeTransparentPirPass {
    root: PathBuf,
    binding: [u8; 32],
    scope: TransparentPirScope,
    cancel: Arc<RecoveryCancellation>,
    _owner: std::fs::File,
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use zcash_client_backend::data_api::WalletWrite;

    #[test]
    fn native_exposed_ephemeral_overlap_counts_unique_scope_receivers() {
        let root = tempfile::tempdir().unwrap();
        let wallet = crate::native::wallet::tests::fixture_wallet(root.path());
        let (key, birthday) = crate::native::wallet::tests::fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        let mut db = wallet.open_db().unwrap();
        db.update_chain_tip(1.into()).unwrap();
        drop(db);

        // Retain actual Rust-derived public receivers in the pinned SQLite row
        // shape. This fixture has no spends, network transport or user wallet.
        let ivk = key.transparent().unwrap().derive_ephemeral_ivk().unwrap();
        let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let (account, flags): (i64, i64) = conn
            .query_row(
                "SELECT account_id,receiver_flags FROM addresses WHERE key_scope=2 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let tx = conn.transaction().unwrap();
        for index in 0..2049u32 {
            let child = transparent::keys::NonHardenedChildIndex::from_index(index).unwrap();
            let address = ivk.derive_ephemeral_address(child).unwrap();
            let encoded =
                zcash_keys::address::Address::Transparent(address).encode(&ZNetwork::Regtest);
            let mut diversifier = *zip32::DiversifierIndex::from(index).as_bytes();
            diversifier.reverse();
            tx.execute("INSERT OR IGNORE INTO addresses(account_id,diversifier_index_be,key_scope,address,transparent_child_index,cached_transparent_receiver_address,receiver_flags,exposed_at_height) VALUES(?1,?2,2,?3,?4,?3,?5,1)",
                rusqlite::params![account,diversifier.as_slice(),encoded,index,flags]).unwrap();
        }
        tx.execute(
            "UPDATE addresses SET exposed_at_height=1 WHERE key_scope=2",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);

        let db = wallet.open_db().unwrap();
        let account = db.get_account_ids().unwrap()[0];
        assert_eq!(
            db.get_ephemeral_transparent_receivers(account, u32::MAX, false)
                .unwrap()
                .len(),
            2049
        );
        drop(db);
        let inventory = wallet
            .transparent_transaction(|db, tx| Ok(scripts(db, tx, ZNetwork::Regtest, 0)?))
            .unwrap();
        let unique = inventory
            .iter()
            .map(|script| script.script.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert!(inventory.len() > 4096);
        assert!((2049..=4096).contains(&unique));
        let scope = ProjectionState::default()
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 1,
                    hash: "01".repeat(32),
                },
                inventory,
            )
            .unwrap();
        assert_eq!(scope.scripts.len(), unique);
    }
}
impl NativeTransparentPirPass {
    pub fn scope_json(&self) -> Result<String> {
        serde_json::to_string(&self.scope).map_err(|_| refused("transparent_pir_scope_invalid"))
    }
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
fn database(_: impl std::fmt::Display) -> EngineError {
    refused("transparent_pir_database_error")
}
// The native marker distinguishes old projection-only state from funding provenance.
// Migration conservatively quarantines prior receive identities, without changing facts,
// continuation, deletion ownership, reservations, or unknown submissions.
fn decode_native_state(bytes: &[u8]) -> anyhow::Result<(ProjectionState, bool)> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
    let mut state: ProjectionState = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
    let legacy = match value.get("nativeFundingVersion") {
        Some(version) if version.as_u64() == Some(2) && state.unverified_outputs.is_some() => false,
        None => true,
        Some(version) if version.as_u64() == Some(1) => true,
        _ => anyhow::bail!("transparent_pir_state_invalid"),
    };
    if legacy {
        state.unverified_outputs.get_or_insert_default().extend(
            state
                .records
                .keys()
                .filter(|k| k.starts_with("r:"))
                .cloned(),
        );
    }
    state
        .unverified_outputs
        .get_or_insert_default()
        .extend(state.projected_outputs.iter().cloned());
    let unverified = state.unverified_outputs.as_ref().unwrap();
    if unverified.len() > 65_536 {
        anyhow::bail!("transparent_pir_state_invalid");
    }
    for key in unverified {
        let fields: Vec<_> = key.split(':').collect();
        if !matches!(fields.as_slice(), ["r", id, n] if crate::transparent_pir::hash(id) && n.parse::<u32>().is_ok_and(|v|v.to_string()==*n))
        {
            anyhow::bail!("transparent_pir_state_invalid");
        }
    }
    Ok((state, legacy))
}
// Native v1 and older did not reliably distinguish shared publisher-supplied
// confirmation metadata. Migrate once under the native owner; do not infer it.
const LEGACY_OUTPUTS: &str = "SELECT json_group_array(json_array(txid,output_index)) FROM (SELECT lower(hex(t.txid)) AS txid,n.output_index FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id LIMIT 65537)";
fn quarantine_legacy(state: &mut ProjectionState, rows: &str) -> anyhow::Result<()> {
    let rows: Vec<(String, u32)> =
        serde_json::from_str(rows).map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
    if rows.len() > 65_536 || rows.iter().any(|(id, _)| !crate::transparent_pir::hash(id)) {
        anyhow::bail!("transparent_pir_state_invalid");
    }
    state
        .unverified_outputs
        .get_or_insert_default()
        .extend(rows.into_iter().map(|(id, n)| format!("r:{id}:{n}")));
    Ok(())
}
fn encode_native_state(state: &ProjectionState) -> anyhow::Result<Vec<u8>> {
    let mut value = serde_json::to_value(state)?;
    if state
        .unverified_outputs
        .as_ref()
        .is_some_and(|s| s.len() > 65_536)
    {
        anyhow::bail!("transparent_pir_state_invalid");
    }
    value["nativeFundingVersion"] = 2.into();
    if state.unverified_outputs.is_none() {
        value["unverifiedOutputs"] = serde_json::json!([]);
    }
    Ok(serde_json::to_vec(&value)?)
}
fn load(ext: &rusqlite::Transaction<'_>) -> anyhow::Result<ProjectionState> {
    ext.execute("CREATE TABLE IF NOT EXISTS ext_transparent_pir_v1 (id INTEGER PRIMARY KEY CHECK(id=1), state BLOB NOT NULL)", [])?;
    ext.execute("CREATE TABLE IF NOT EXISTS ext_transparent_pir_funding_guard_v1 (id INTEGER PRIMARY KEY CHECK(id=1))", [])?;
    let required: bool = ext.query_row(
        "SELECT EXISTS(SELECT 1 FROM ext_transparent_pir_funding_guard_v1)",
        [],
        |r| r.get(0),
    )?;
    let bytes: Option<Vec<u8>> = ext
        .query_row(
            "SELECT state FROM ext_transparent_pir_v1 WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match bytes {
        Some(bytes) => {
            let (mut state, legacy) = decode_native_state(&bytes)?;
            if legacy {
                let rows: String = ext.query_row(LEGACY_OUTPUTS, [], |r| r.get(0))?;
                quarantine_legacy(&mut state, &rows)?;
                save(ext, &state)?;
            }
            if state.bound_source.is_some() {
                ext.execute(
                    "INSERT OR IGNORE INTO ext_transparent_pir_funding_guard_v1 VALUES(1)",
                    [],
                )?;
            }
            Ok(state)
        }
        None if required => anyhow::bail!("transparent_pir_state_invalid"),
        None => Ok(ProjectionState::default()),
    }
}
fn save(ext: &rusqlite::Transaction<'_>, state: &ProjectionState) -> anyhow::Result<()> {
    ext.execute("INSERT INTO ext_transparent_pir_v1 VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET state=excluded.state", [encode_native_state(state)?])?;
    if state.bound_source.is_some() {
        ext.execute(
            "INSERT OR IGNORE INTO ext_transparent_pir_funding_guard_v1 VALUES(1)",
            [],
        )?;
    }
    Ok(())
}
pub(super) fn funding_exclusions(
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let exists: bool = ext.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_transparent_pir_v1')", [], |r|r.get(0))?;
    let guard_exists: bool = ext.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_transparent_pir_funding_guard_v1')", [], |r|r.get(0))?;
    let required = guard_exists
        && ext.query_row(
            "SELECT EXISTS(SELECT 1 FROM ext_transparent_pir_funding_guard_v1)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
    let bytes: Option<Vec<u8>> = if exists {
        ext.query_row(
            "SELECT state FROM ext_transparent_pir_v1 WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    match bytes {
        Some(bytes) => {
            let (mut state, legacy) = decode_native_state(&bytes)?;
            if state.bound_source.is_some() {
                // DDL is intentionally unavailable to extension transactions. Old
                // wallets without the guard must first load their PIR state through
                // the native owner; funding remains refused in the meantime.
                if !guard_exists {
                    anyhow::bail!("transparent_pir_state_invalid");
                }
                ext.execute(
                    "INSERT OR IGNORE INTO ext_transparent_pir_funding_guard_v1 VALUES(1)",
                    [],
                )?;
            }
            if legacy {
                let rows: String = ext.query_row(LEGACY_OUTPUTS, [], |r| r.get(0))?;
                quarantine_legacy(&mut state, &rows)?;
            }
            // Upgrade before selection in the same wallet transaction.
            ext.execute(
                "UPDATE ext_transparent_pir_v1 SET state=?1 WHERE id=1",
                [encode_native_state(&state)?],
            )?;
            let mut excluded = state.unverified_outputs.unwrap_or_default();
            // Keep pre-birthday transparent outputs excluded until PIR spend coverage
            // connects continuously to the wallet's locally scanned birthday horizon.
            if state.bound_source.is_some() {
                let birthday: u32 = ext
                    .query_row(
                        "SELECT COALESCE(MIN(birthday_height), 0) FROM accounts",
                        [],
                        |r| r.get::<_, u32>(0),
                    )
                    .unwrap_or(0);
                let bday_floor = u64::from(birthday.saturating_sub(1));
                if bday_floor > 0 && state.covered_through < bday_floor {
                    let rows: String = ext.query_row(
                        "SELECT COALESCE(group_concat('r:' || lower(hex(t.txid)) || ':' || n.output_index, ','), '') \
                         FROM transparent_received_outputs n \
                         JOIN transactions t ON t.id_tx = n.transaction_id \
                         WHERE t.mined_height < ?1",
                        [birthday],
                        |r| r.get(0),
                    )?;
                    for key in rows.split(',').filter(|s| !s.is_empty()) {
                        excluded.insert(key.to_owned());
                    }
                }
            }
            Ok(excluded)
        }
        None if required => anyhow::bail!("transparent_pir_state_invalid"),
        None => Ok(Default::default()),
    }
}
fn native_history_floor(
    network: ZNetwork,
    birthday: u32,
    anchor: u64,
    state: &ProjectionState,
) -> u64 {
    if network == crate::Network::Regtest {
        return 0;
    }
    let estimate = u64::from(crate::birthday::coarse_birthday_height(network, birthday));
    if estimate == 0 {
        return 0;
    }
    if state
        .records
        .values()
        .any(|(h, _)| u64::from(*h) < estimate)
    {
        return 0;
    }
    if let Some(cp) = state.continuation.as_ref() {
        let has_below = cp.records.iter().any(|record| {
            let Ok(bytes) = crate::web::from_hex(&record.event) else {
                return true;
            };
            let Ok(event) = TransparentEvent::from_bytes(&bytes) else {
                return true;
            };
            u64::from(crate::transparent_pir::event_height(event)) < estimate
        });
        if has_below {
            return 0;
        }
    }
    estimate.min(anchor)
}

fn scripts<D: WalletRead<AccountId = AccountUuid>>(
    db: &D,
    tx: &rusqlite::Transaction<'_>,
    network: ZNetwork,
    required_from: u64,
) -> Result<Vec<TransparentPirScript>>
where
    D::Error: std::fmt::Display,
{
    let mut result = Vec::new();
    for account in db.get_account_ids().map_err(database)? {
        let mut receivers = db
            .get_transparent_receivers(account, true, true)
            .map_err(database)?;
        if db.chain_height().map_err(database)?.is_some() {
            receivers.extend(
                db.get_ephemeral_transparent_receivers(account, u32::MAX, false)
                    .map_err(database)?,
            );
        }
        for (address, _metadata) in receivers {
            let script: Script = address.script().into();
            result.push(TransparentPirScript {
                script: crate::web::to_hex(&script.0 .0),
                required_from,
            });
        }
    }
    // Pinned WalletRead exposes only exposed ephemeral receivers. Include the
    // engine's already allocated ephemeral gap without deriving outside Rust.
    let mut stmt=tx.prepare("SELECT cached_transparent_receiver_address FROM addresses WHERE key_scope=2 AND cached_transparent_receiver_address IS NOT NULL ORDER BY id LIMIT 4097").map_err(database)?;
    for address in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(database)?
    {
        let address = address.map_err(database)?;
        let address = zcash_keys::address::Address::decode(&network, &address)
            .and_then(|a| a.to_transparent_address())
            .ok_or_else(|| refused("transparent_pir_scope_invalid"))?;
        let script: Script = address.script().into();
        result.push(TransparentPirScript {
            script: crate::web::to_hex(&script.0 .0),
            required_from,
        });
        if result.len() > 8192 {
            return Err(refused("transparent_pir_scope_limit"));
        }
    }
    Ok(result)
}
/// Canonical truncation can erase a projection even after scanning returns to
/// the same hash. Check only this source's owned effects in bounded SQL batches.
fn missing_projection(
    tx: &rusqlite::Transaction<'_>,
    state: &ProjectionState,
) -> anyhow::Result<std::collections::BTreeMap<String, u32>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut expected = Vec::new();
    let mut ids = BTreeSet::new();
    if state
        .projected_outputs
        .len()
        .saturating_add(state.projected_spends.len())
        > 65_536
    {
        anyhow::bail!("transparent_pir_state_invalid");
    }
    for key in state
        .projected_outputs
        .iter()
        .chain(&state.projected_spends)
    {
        let fields: Vec<_> = key.split(':').collect();
        let height = state
            .records
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("transparent_pir_state_invalid"))?
            .0;
        if !matches!(fields.as_slice(), ["r", _, _] | ["s", _, _, _, _]) {
            anyhow::bail!("transparent_pir_state_invalid");
        }
        let id = crate::web::from_hex(fields[1])
            .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
        ids.insert(id.clone());
        let output = fields.last().unwrap().parse::<u32>()?;
        let previous = if fields[0] == "s" {
            Some(
                crate::web::from_hex(fields[3])
                    .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?,
            )
        } else {
            None
        };
        expected.push((key.clone(), id, height, output, previous));
    }
    let ids: Vec<_> = ids.into_iter().collect();
    let mut mined = BTreeMap::new();
    let mut outputs = BTreeSet::new();
    let mut spends = BTreeSet::new();
    for batch in ids.chunks(256) {
        let parameters = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = tx.prepare(&format!(
            "SELECT txid,mined_height,tx_index FROM transactions WHERE txid IN({parameters})"
        ))?;
        for row in stmt.query_map(rusqlite::params_from_iter(batch), |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Option<u32>>(1)?,
                r.get::<_, Option<u32>>(2)?,
            ))
        })? {
            let (id, height, index) = row?;
            mined.insert(id, (height, index));
        }
        let mut stmt = tx.prepare(&format!("SELECT t.txid,n.output_index FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid IN({parameters})"))?;
        for row in stmt.query_map(rusqlite::params_from_iter(batch), |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?))
        })? {
            outputs.insert(row?);
        }
        // A completed source projection must have its actual UTXO spend link.
        // A pending map alone is sufficient only for an explicitly unresolved input.
        let pending_allowed = u8::from(!state.complete && state.unresolved_spends > 0);
        let mut stmt = tx.prepare(&format!("SELECT t.txid,p.txid,n.output_index FROM transparent_received_output_spends s JOIN transactions t ON t.id_tx=s.transaction_id JOIN transparent_received_outputs n ON n.id=s.transparent_received_output_id JOIN transactions p ON p.id_tx=n.transaction_id WHERE t.txid IN({parameters}) UNION SELECT t.txid,s.prevout_txid,s.prevout_output_index FROM transparent_spend_map s JOIN transactions t ON t.id_tx=s.spending_transaction_id WHERE t.txid IN({parameters}) AND {pending_allowed}=1 AND NOT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions p ON p.id_tx=n.transaction_id WHERE p.txid=s.prevout_txid AND n.output_index=s.prevout_output_index)"))?;
        for row in stmt.query_map(rusqlite::params_from_iter(batch.iter().chain(batch)), |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, u32>(2)?,
            ))
        })? {
            spends.insert(row?);
        }
    }
    Ok(expected
        .into_iter()
        .filter_map(|(key, id, height, index, previous)| {
            let present = mined
                .get(&id)
                .is_some_and(|(mined, position)| *mined == Some(height) && position.is_some())
                && match previous {
                    Some(previous) => spends.contains(&(id, previous, index)),
                    None => outputs.contains(&(id, index)),
                };
            (!present).then_some((key, height))
        })
        .collect())
}
fn invalidate_missing(
    state: &mut ProjectionState,
    missing: &std::collections::BTreeMap<String, u32>,
) -> bool {
    let Some(floor) = missing
        .values()
        .copied()
        .map(|height| height.saturating_sub(1))
        .min()
    else {
        return false;
    };
    let changed = !state.projection_invalidated
        || state.complete
        || state.rewind_floor.is_none_or(|old| floor < old);
    state.rewind(floor);
    changed
}

fn retire_projection(
    tx: &rusqlite::Transaction<'_>,
    state: &mut ProjectionState,
    old: &std::collections::BTreeMap<String, (u32, String)>,
    incoming: &[(Vec<u8>, TransparentEvent)],
    height: u32,
) -> anyhow::Result<()> {
    let incoming: std::collections::BTreeMap<_, _> = incoming
        .iter()
        .map(|(_, e)| {
            (
                crate::transparent_pir::event_key(*e),
                crate::transparent_pir::digest(&e.to_bytes()),
            )
        })
        .collect();
    let retired: Vec<_> = old
        .iter()
        .filter(|(key, (h, d))| *h > height && incoming.get(*key) != Some(d))
        .map(|(k, _)| k.clone())
        .collect();
    // Spends retire before outputs. Only relations this source originally created
    // are eligible; existing local pending/independent relations are never owned.
    for key in &retired {
        let fields: Vec<_> = key.split(':').collect();
        if fields.first() == Some(&"s") && state.projected_spends.remove(key) {
            let spending = crate::web::from_hex(fields[1])
                .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
            let previous = crate::web::from_hex(fields[3])
                .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
            let index: u32 = fields[4].parse()?;
            let local: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM transactions WHERE txid=?1 AND created IS NOT NULL)",
                [&spending],
                |r| r.get(0),
            )?;
            if local {
                anyhow::bail!("transparent_pir_pending_reconciliation_required");
            }
            tx.execute("DELETE FROM transparent_received_output_spends WHERE transaction_id IN(SELECT id_tx FROM transactions WHERE txid=?1) AND transparent_received_output_id IN(SELECT n.id FROM transparent_received_outputs n JOIN transactions p ON p.id_tx=n.transaction_id WHERE p.txid=?2 AND n.output_index=?3)",rusqlite::params![spending,previous,index])?;
            tx.execute("DELETE FROM transparent_spend_map WHERE spending_transaction_id IN(SELECT id_tx FROM transactions WHERE txid=?1) AND prevout_txid=?2 AND prevout_output_index=?3",rusqlite::params![spending,previous,index])?;
        }
    }
    for key in &retired {
        let fields: Vec<_> = key.split(':').collect();
        if fields.first() == Some(&"r") && state.projected_outputs.remove(key) {
            let id = crate::web::from_hex(fields[1])
                .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
            let index: u32 = fields[2].parse()?;
            let locked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1 AND n.output_index=?2 AND (n.lock_owner IS NOT NULL OR n.lock_expiry_height IS NOT NULL OR EXISTS(SELECT 1 FROM transparent_received_output_spends s JOIN transactions p ON p.id_tx=s.transaction_id WHERE s.transparent_received_output_id=n.id AND p.created IS NOT NULL)))",rusqlite::params![id,index],|r|r.get(0))?;
            if locked {
                anyhow::bail!("transparent_pir_pending_reconciliation_required");
            }
            // Retained full bytes are independent wallet evidence; leave those rows.
            tx.execute("DELETE FROM transparent_received_outputs WHERE transaction_id IN(SELECT id_tx FROM transactions WHERE txid=?1 AND raw IS NULL AND created IS NULL) AND output_index=?2",rusqlite::params![id,index])?;
        }
    }
    let ids: Vec<_> = state.projected_transactions.iter().cloned().collect();
    for id in ids {
        let bytes = crate::web::from_hex(&id)
            .map_err(|_| anyhow::anyhow!("transparent_pir_state_invalid"))?;
        // Raw/locally-created transactions retain their independent pending authority.
        tx.execute("UPDATE transactions SET mined_height=NULL,block=NULL,tx_index=NULL WHERE txid=?1 AND mined_height>?2 AND raw IS NULL AND created IS NULL",rusqlite::params![bytes,height])?;
    }
    Ok(())
}

struct TransactionConnection<'a, 'b>(&'a rusqlite::Transaction<'b>);
impl std::borrow::Borrow<rusqlite::Connection> for TransactionConnection<'_, '_> {
    fn borrow(&self) -> &rusqlite::Connection {
        self.0
    }
}
impl<'a, 'b> std::borrow::Borrow<rusqlite::Transaction<'b>> for TransactionConnection<'a, 'b> {
    fn borrow(&self) -> &rusqlite::Transaction<'b> {
        self.0
    }
}
type ProjectionDb<'a, 'b> =
    WalletDb<TransactionConnection<'a, 'b>, ZNetwork, SystemClock, UnwrapErr<SysRng>>;

impl NativeWallet {
    fn transparent_transaction<T>(
        &self,
        operation: impl FnOnce(
            &mut ProjectionDb<'_, '_>,
            &rusqlite::Transaction<'_>,
        ) -> anyhow::Result<T>,
    ) -> Result<T> {
        // This engine-owned connection wraps one canonical wallet transaction.
        // It is never held over transport and never exposed to the host.
        let mut conn = rusqlite::Connection::open(&self.paths.data_db).map_err(database)?;
        rusqlite::vtab::array::load_module(&conn).map_err(database)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(database)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(database)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(database)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(database)?;
        let mut wdb = WalletDb::from_connection(
            TransactionConnection(&tx),
            self.network,
            SystemClock,
            new_rng(),
        );
        let result = operation(&mut wdb, &tx).map_err(EngineError::from)?;
        drop(wdb);
        tx.commit().map_err(database)?;
        Ok(result)
    }

    pub fn begin_transparent_pir_with_source(
        &self,
        source: &super::super::NativeWalletSource,
        source_id: &str,
        cancel: Arc<RecoveryCancellation>,
    ) -> Result<NativeTransparentPirPass> {
        self.require_wallet_source(source)?;
        let owner = database_lease::shared(&self.paths.data_db)?;
        let scope = cancel.transaction(|| {
            self.transparent_transaction(|wdb, ext| -> anyhow::Result<_> {
                let block = wdb
                    .block_fully_scanned()?
                    .ok_or_else(|| anyhow::anyhow!("transparent_pir_anchor_unavailable"))?;
                let anchor = TransparentPirAnchor {
                    height: u64::from(u32::from(block.block_height())),
                    hash: block.block_hash().to_string(),
                };
                let mut state = load(ext)?;
                let required_from = native_history_floor(
                    self.network,
                    self.meta.birthday_height,
                    anchor.height,
                    &state,
                );
                let mut scope = state.begin(
                    source_id,
                    anchor,
                    scripts(wdb, ext, self.network, required_from)?,
                )?;
                let missing = missing_projection(ext, &state)?;
                invalidate_missing(&mut state, &missing);
                scope.accepted_headers.retain(|h| h.height != 0);
                scope.accepted_headers.push(TransparentPirAnchor {
                    height: 0,
                    hash: self.network.genesis_hash().into(),
                });
                state.scope = Some(scope.clone());
                save(ext, &state)?;
                Ok(scope)
            })
        })?;
        Ok(NativeTransparentPirPass {
            root: self.paths.root.clone(),
            binding: source.binding(),
            scope,
            cancel,
            _owner: owner,
        })
    }

    pub fn apply_transparent_pir_checkpoint_with_source(
        &self,
        source: &super::super::NativeWalletSource,
        pass: &mut NativeTransparentPirPass,
        json: &str,
    ) -> Result<serde_json::Value> {
        self.apply_transparent_pir_checkpoint_inner(source, pass, json, None)
    }

    pub fn apply_transparent_pir_checkpoint_with_source_and_rewind(
        &self,
        source: &super::super::NativeWalletSource,
        pass: &mut NativeTransparentPirPass,
        json: &str,
        rewind: z_transparent_pir_v2::AcceptedRewind<'_>,
    ) -> Result<serde_json::Value> {
        self.apply_transparent_pir_checkpoint_inner(source, pass, json, Some(rewind))
    }

    fn apply_transparent_pir_checkpoint_inner(
        &self,
        source: &super::super::NativeWalletSource,
        pass: &mut NativeTransparentPirPass,
        json: &str,
        rewind: Option<z_transparent_pir_v2::AcceptedRewind<'_>>,
    ) -> Result<serde_json::Value> {
        self.require_wallet_source(source)?;
        if pass.root != self.paths.root || pass.binding != source.binding() {
            return Err(refused("transparent_pir_context_changed"));
        }
        let checkpoint: TransparentPirCheckpoint = parse(json)?;
        if checkpoint.scope_id != pass.scope.scope_id
            || checkpoint.generation != pass.scope.generation
        {
            return Err(refused("transparent_pir_context_changed"));
        }
        pass.cancel.transaction(|| self.transparent_transaction(|wdb,ext| -> anyhow::Result<_> {
            let mut state = load(ext)?;
            let witnessed = rewind.as_ref().is_some_and(|witness| {
                witness.matches(&pass.scope, &checkpoint)
                    && state.scope.as_ref().is_some_and(|scope| witness.matches(scope, &checkpoint))
            });
            if rewind.is_some() && !witnessed { anyhow::bail!("transparent_pir_rewind_evidence_invalid"); }
            let old_records = state.records.clone();
            let missing = missing_projection(ext,&state)?;
            invalidate_missing(&mut state,&missing);
            let replay = state.checkpoint_id.as_ref()==Some(&checkpoint.checkpoint_id);
            let repair = replay && state.projection_invalidated;
            let retirement_floor = state.rewind_floor.into_iter().chain(checkpoint.rewind_to.as_ref().map(|a|a.height as u32)).min();
            if !repair { if let Some(floor)=retirement_floor { state.prepare_rewind(floor); } }
            if let Some(ancestor)=&checkpoint.rewind_to {
                let height=u32::try_from(ancestor.height)?;
                let known=if height==0 {Some(self.network.genesis_hash().to_string())} else {wdb.get_block_hash(height.into())?.map(|h|h.to_string())};
                if known.as_deref()!=Some(&ancestor.hash) && !(known.is_none() && witnessed) {anyhow::bail!("transparent_pir_rewind_anchor_unknown");}
                if !repair { state.prepare_rewind(height); }
            }
            let mut records = state.validate(&checkpoint)?;
            let height = u32::try_from(checkpoint.anchor.height)?;
            let block = wdb.block_fully_scanned()?.ok_or_else(||anyhow::anyhow!("transparent_pir_anchor_unavailable"))?;
            if u32::from(block.block_height()) != height || block.block_hash().to_string() != checkpoint.anchor.hash { anyhow::bail!("transparent_pir_chain_changed"); }
            if state.checkpoint_id.as_ref() == Some(&checkpoint.checkpoint_id) && !state.projection_invalidated {
                state.durable = true;
                return Ok(state.status());
            }
            let required_from = pass
                .scope
                .scripts
                .first()
                .map_or(0, |s| s.required_from);
            let current_scripts = scripts(wdb, ext, self.network, required_from)?;
            let current_id = crate::transparent_pir::digest(&serde_json::to_vec(&{let mut s=current_scripts.clone();s.sort_by(|a,b|a.script.cmp(&b.script));s.dedup();s})?);
            if pass.scope.scripts.iter().any(|enrolled| !current_scripts.contains(enrolled)) { anyhow::bail!("transparent_pir_scope_changed"); }
            if retirement_floor.is_some() && !checkpoint.complete {
                state.records = old_records;
                state.stage(&checkpoint,&[])?;
                state.rewind_floor = retirement_floor;
                state.durable = true;
                save(ext,&state)?;
                return Ok(state.status());
            }
            // Snapshot confirmation authority for every transaction before ANY
            // publisher metadata writes (including spend-only events). Promoting
            // one output also confirms its existing pending siblings in WalletDb.
            let mut original_meta=std::collections::BTreeMap::new();
            let mut pending_siblings=std::collections::BTreeSet::new();
            for (_,event) in &records {
                let id=match event {TransparentEvent::Receive(e)=>e.txid.0,TransparentEvent::Spend(e)=>e.spending_txid.0};
                if let std::collections::btree_map::Entry::Vacant(entry)=original_meta.entry(id) {
                    let prior:Option<(Option<u32>,Option<u32>)>=ext.query_row("SELECT mined_height,tx_index FROM transactions WHERE txid=?1",[id.as_slice()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    if prior.is_none_or(|(h,_)|h.is_none()) {
                        let mut stmt=ext.prepare("SELECT n.output_index FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1")?;
                        for index in stmt.query_map([id.as_slice()],|r|r.get::<_,u32>(0))? {
                            pending_siblings.insert(format!("r:{}:{}",crate::web::to_hex(&id),index?));
                            if pending_siblings.len()>65_536 {anyhow::bail!("transparent_pir_state_invalid");}
                        }
                    }
                    entry.insert(prior);
                }
            }
            state.unverified_outputs.get_or_insert_default().extend(pending_siblings);
            if repair {
                // The original source-bound checkpoint is already accepted. Rebuild
                // only its erased owned effects; do not retire or rewrite independent rows.
                records.retain(|(_,event)|missing.contains_key(&crate::transparent_pir::event_key(*event)));
            } else if let Some(floor)=retirement_floor { retire_projection(ext,&mut state,&old_records,&records,floor)?; }
            for (script,event) in &records {
                let (id,position,mined) = match event { TransparentEvent::Receive(e) => (e.txid,e.transaction_index,e.height), TransparentEvent::Spend(e)=>(e.spending_txid,e.transaction_index,e.height) };
                let prior_meta=original_meta[&id.0];
                    if prior_meta.is_some_and(|(h,i)|h.is_some_and(|h|h!=mined) || i.is_some_and(|i|i!=u32::from(position))) { anyhow::bail!("transparent_pir_transaction_conflict"); }
                    if prior_meta.is_none() { state.projected_transactions.insert(crate::web::to_hex(&id.0)); }
                    let meta = WalletTx::new(TxId::from_bytes(id.0),position.into(),vec![],vec![],vec![],vec![],vec![],vec![],vec![]);
                let tx_ref = wdb.put_tx_meta(&meta,mined.into())?;
                match event {
                    TransparentEvent::Receive(e) => {
                        let output = WalletTransparentOutput::from_parts(OutPoint::new(e.txid.0,e.output_index),TxOut::new(Zatoshis::from_u64(e.value)?, Script(zcash_script::script::Code(script.clone()))),Some(e.height.into()),None,None,None).ok_or_else(||anyhow::anyhow!("transparent_pir_script_invalid"))?;
                        if wdb.find_account_for_transparent_address(output.recipient_address())?.is_none() { anyhow::bail!("transparent_pir_script_not_owned"); }
                        let old:Option<(Vec<u8>,i64,Option<u32>)> = ext.query_row("SELECT n.script,n.value_zat,t.mined_height FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1 AND n.output_index=?2",rusqlite::params![e.txid.0.as_slice(),e.output_index],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                        if old.is_none() {state.projected_outputs.insert(crate::transparent_pir::event_key(*event));}
                        if old.is_none() || prior_meta.is_none_or(|(h,_)|h.is_none()) {
                            state.unverified_outputs.get_or_insert_default().insert(crate::transparent_pir::event_key(*event));
                        }
                        if old.is_some_and(|(s,v,h)| s != *script || u64::try_from(v).ok()!=Some(e.value) || h.is_some_and(|h|h!=e.height)) { anyhow::bail!("transparent_pir_output_conflict"); }
                        // Native upstream reconciliation retains existing spend/lock metadata.
                        wdb.put_transparent_output(&output,height.into(),true)?;
                    }
                    TransparentEvent::Spend(e) => {
                        let prior:bool=ext.query_row("SELECT EXISTS(SELECT 1 FROM transparent_spend_map s JOIN transactions t ON t.id_tx=s.spending_transaction_id WHERE t.txid=?1 AND s.prevout_txid=?2 AND s.prevout_output_index=?3) OR EXISTS(SELECT 1 FROM transparent_received_output_spends s JOIN transactions t ON t.id_tx=s.transaction_id JOIN transparent_received_outputs n ON n.id=s.transparent_received_output_id JOIN transactions p ON p.id_tx=n.transaction_id WHERE t.txid=?1 AND p.txid=?2 AND n.output_index=?3)",rusqlite::params![e.spending_txid.0.as_slice(),e.spent_txid.0.as_slice(),e.spent_output_index],|r|r.get(0))?;
                        if !prior {state.projected_spends.insert(crate::transparent_pir::event_key(*event));}
                        let found = wdb.mark_transparent_utxo_spent(&OutPoint::new(e.spent_txid.0,e.spent_output_index),tx_ref)?;
                        if !found && checkpoint.complete { anyhow::bail!("transparent_pir_unresolved_spend"); }
                    }
                }
            }
            let unspent:u64=ext.query_row("SELECT COALESCE(SUM(n.value_zat),0) FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.mined_height IS NOT NULL AND NOT EXISTS(SELECT 1 FROM transparent_received_output_spends s WHERE s.transparent_received_output_id=n.id)",[],|r|r.get(0))?;
            Zatoshis::from_u64(unspent).map_err(|_|anyhow::anyhow!("transparent_pir_amount_invalid"))?;
            state.stage(&checkpoint,&records)?;
            if repair && !missing_projection(ext,&state)?.is_empty() { anyhow::bail!("transparent_pir_projection_incomplete"); }
            state.projection_invalidated=false;
            let next_scripts = scripts(wdb, ext, self.network, required_from)?;
            if next_scripts.len() != current_scripts.len() || current_id != pass.scope.scope_id { state.complete=false; }
            state.durable=true;
            save(ext,&state)?;
            Ok(state.status())
        }))
    }

    /// Distinct mined heights of unspent transparent UTXOs whose funding is still
    /// unverified in Transparent PIR state. Callers fetch aligned 500-block all-pool
    /// compact buckets covering these heights and verify them via
    /// [`Self::verify_transparent_compact_bucket`] before shielding.
    pub fn unverified_transparent_heights(&self) -> Result<Vec<u32>> {
        self.transparent_transaction(|_, ext| -> anyhow::Result<_> {
            let state = load(ext)?;
            let Some(unverified) = state.unverified_outputs.as_ref() else {
                return Ok(Vec::new());
            };
            if unverified.is_empty() {
                return Ok(Vec::new());
            }
            let mut heights = std::collections::BTreeSet::new();
            let mut stmt = ext.prepare(
                "SELECT t.txid, n.output_index, t.mined_height \
                 FROM transparent_received_outputs n \
                 JOIN transactions t ON t.id_tx = n.transaction_id \
                 WHERE t.mined_height IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM transparent_received_output_spends s WHERE s.transparent_received_output_id = n.id)",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, u32>(2)?,
                ))
            })?;
            for row in rows {
                let (txid, index, height) = row?;
                let key = format!("r:{}:{index}", crate::web::to_hex(&txid));
                if unverified.contains(&key) {
                    heights.insert(height);
                }
            }
            Ok(heights.into_iter().collect())
        })
    }

    /// Verify unverified Transparent PIR UTXOs against an aligned 500-block all-pool
    /// compact block bucket. Requires cryptographic chain anchoring against SQLite
    /// scanned hashes (or an explicit publisher-signed end-block hash via
    /// [`Self::verify_transparent_compact_bucket_anchored`]).
    pub fn verify_transparent_compact_bucket(&self, blob: &[u8]) -> Result<u32> {
        self.verify_transparent_compact_bucket_anchored(blob, None)
    }

    /// Same as [`Self::verify_transparent_compact_bucket`], accepting an optional
    /// 32-byte wire-order end-block hash attested by the caller for strictly
    /// pre-birthday buckets that do not overlap SQLite scanned block hashes.
    ///
    /// Security requirement: callers MUST derive `trusted_end_hash` exclusively from
    /// cryptographic verification of a pinned publisher signature over the complete
    /// bucket payload (never from an unauthenticated transport response or by echoing
    /// `blocks.last().hash`). Passing `None` enforces anchoring strictly against
    /// locally scanned SQLite block hashes.
    pub fn verify_transparent_compact_bucket_anchored(
        &self,
        blob: &[u8],
        trusted_end_hash: Option<&[u8]>,
    ) -> Result<u32> {
        const VERIFY_BUCKET_WIDTH: u32 = 500;
        let blocks = crate::web::decode_delimited(blob)?;
        if blocks.is_empty() {
            return Err(refused("transparent_pir_verification_empty"));
        }
        let start = u32::try_from(blocks[0].height)
            .map_err(|_| refused("transparent_pir_verification_invalid"))?;
        let width = u32::try_from(blocks.len())
            .map_err(|_| refused("transparent_pir_verification_invalid"))?;
        if start == 0 || width > VERIFY_BUCKET_WIDTH || (start - 1) % VERIFY_BUCKET_WIDTH != 0 {
            return Err(refused("transparent_pir_verification_alignment"));
        }
        let mut expected_height = start;
        let mut prev_hash: Option<Vec<u8>> = None;
        for block in &blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| refused("transparent_pir_verification_invalid"))?;
            if height != expected_height
                || block.hash.len() != 32
                || block.prev_hash.len() != 32
                || prev_hash.as_ref().is_some_and(|p| *p != block.prev_hash)
            {
                return Err(refused("transparent_pir_verification_not_contiguous"));
            }
            if block.vtx.first().is_none_or(|t| t.index != 0) {
                return Err(refused("transparent_pir_verification_no_coinbase"));
            }
            let mut ids = std::collections::BTreeSet::new();
            let mut last_idx = None;
            for tx in &block.vtx {
                if tx.txid.len() != 32
                    || !ids.insert(&tx.txid)
                    || last_idx.is_some_and(|i| tx.index <= i)
                    || tx.vin.iter().any(|v| v.prevout_txid.len() != 32)
                    || tx
                        .vout
                        .iter()
                        .any(|v| v.value > 2_100_000_000_000_000 || v.script_pub_key.len() > 10_000)
                {
                    return Err(refused("transparent_pir_verification_invalid"));
                }
                last_idx = Some(tx.index);
            }
            expected_height = height.saturating_add(1);
            prev_hash = Some(block.hash.clone());
        }
        let end = expected_height.saturating_sub(1);

        let mut bucket_outputs =
            std::collections::BTreeMap::<(u32, Vec<u8>, u32), (u64, Vec<u8>, u64)>::new();
        let mut bucket_spends =
            std::collections::BTreeMap::<(Vec<u8>, u32), (Vec<u8>, u32, u64)>::new();
        for block in &blocks {
            let height = block.height as u32;
            for tx in &block.vtx {
                for (vout_idx, out) in tx.vout.iter().enumerate() {
                    bucket_outputs.insert(
                        (height, tx.txid.clone(), vout_idx as u32),
                        (out.value, out.script_pub_key.clone(), tx.index),
                    );
                }
                for vin in &tx.vin {
                    bucket_spends.insert(
                        (vin.prevout_txid.clone(), vin.prevout_index),
                        (tx.txid.clone(), height, tx.index),
                    );
                }
            }
        }

        self.transparent_transaction(|wdb, ext| -> anyhow::Result<u32> {
            let scanned = wdb
                .block_fully_scanned()?
                .map(|b| u32::from(b.block_height()))
                .unwrap_or(0);
            if width < VERIFY_BUCKET_WIDTH && end != scanned {
                anyhow::bail!("transparent_pir_verification_alignment");
            }
            // Verify known block hashes in SQLite agree with this bucket and require anchoring.
            let mut anchored = false;
            for block in &blocks {
                let height = block.height as u32;
                if height > 1 {
                    if let Some(known_prev) = wdb.get_block_hash((height - 1).into())? {
                        if known_prev.0.as_slice() != block.prev_hash.as_slice() {
                            anyhow::bail!("transparent_pir_verification_chain_mismatch");
                        }
                        anchored = true;
                    }
                }
                if let Some(known) = wdb.get_block_hash(height.into())? {
                    if known.0.as_slice() != block.hash.as_slice() {
                        anyhow::bail!("transparent_pir_verification_chain_mismatch");
                    }
                    anchored = true;
                }
            }
            if self.network != ZNetwork::Regtest && trusted_end_hash.is_none() {
                anyhow::bail!("transparent_pir_verification_unauthenticated");
            }
            if let Some(expected_end) = trusted_end_hash {
                if expected_end.len() != 32
                    || blocks
                        .last()
                        .is_none_or(|b| b.hash.as_slice() != expected_end)
                {
                    anyhow::bail!("transparent_pir_verification_chain_mismatch");
                }
                anchored = true;
            }
            if !anchored {
                anyhow::bail!("transparent_pir_verification_unanchored");
            }
            let mut state = load(ext)?;
            // Require continuous spend coverage between the verified bucket and the
            // wallet's locally scanned birthday horizon before promoting any output.
            let birthday = wdb.get_wallet_birthday()?.map(u32::from).unwrap_or(0);
            let bday_floor = u64::from(birthday.saturating_sub(1));
            if u64::from(end) < bday_floor && state.covered_through < bday_floor {
                anyhow::bail!("transparent_pir_spend_coverage_gap");
            }
            let unverified = state.unverified_outputs.clone().unwrap_or_default();
            if unverified.is_empty() {
                return Ok(0);
            }
            let mut stmt = ext.prepare(
                "SELECT t.txid, n.output_index, n.value_zat, n.script, t.mined_height \
                 FROM transparent_received_outputs n \
                 JOIN transactions t ON t.id_tx = n.transaction_id \
                 WHERE t.mined_height BETWEEN ?1 AND ?2",
            )?;
            let candidates: Vec<(Vec<u8>, u32, u64, Vec<u8>, u32)> = stmt
                .query_map(rusqlite::params![start, end], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, u64>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, u32>(4)?,
                    ))
                })?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);

            let mut to_confirm: Vec<(String, Vec<u8>, u64)> = Vec::new();
            for (txid, index, value_zat, script, height) in candidates {
                let key = format!("r:{}:{index}", crate::web::to_hex(&txid));
                if !unverified.contains(&key) {
                    continue;
                }
                let Some((b_val, b_script, tx_index)) =
                    bucket_outputs.get(&(height, txid.clone(), index))
                else {
                    anyhow::bail!("transparent_pir_verification_missing");
                };
                if *b_val != value_zat || *b_script != script {
                    anyhow::bail!("transparent_pir_verification_mismatch");
                }
                to_confirm.push((key, txid, *tx_index));
            }

            let mut confirmed = 0u32;
            for (key, txid, tx_index) in to_confirm {
                ext.execute(
                    "UPDATE transactions SET tx_index = ?2 WHERE txid = ?1",
                    rusqlite::params![txid, tx_index as i64],
                )?;
                if let Some(set) = &mut state.unverified_outputs {
                    set.remove(&key);
                }
                state.projected_outputs.remove(&key);
                confirmed += 1;
            }

            // Record transparent spends inside the anchored bucket ONLY for verified outputs.
            let remaining_unverified = state.unverified_outputs.clone().unwrap_or_default();
            for ((prev_txid, prev_idx), (spender_txid, spend_height, spend_tx_index)) in
                &bucket_spends
            {
                let key = format!("r:{}:{prev_idx}", crate::web::to_hex(prev_txid));
                if remaining_unverified.contains(&key) {
                    continue;
                }
                let exists: bool = ext.query_row(
                    "SELECT EXISTS(SELECT 1 FROM transparent_received_outputs n \
                     JOIN transactions t ON t.id_tx = n.transaction_id \
                     WHERE t.txid = ?1 AND n.output_index = ?2)",
                    rusqlite::params![prev_txid, prev_idx],
                    |r| r.get(0),
                )?;
                if exists && spender_txid.len() == 32 && prev_txid.len() == 32 {
                    let mut sid = [0u8; 32];
                    sid.copy_from_slice(spender_txid);
                    let mut pid = [0u8; 32];
                    pid.copy_from_slice(prev_txid);
                    let meta = WalletTx::new(
                        TxId::from_bytes(sid),
                        u16::try_from(*spend_tx_index).unwrap_or(u16::MAX).into(),
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                    );
                    let tx_ref = wdb.put_tx_meta(&meta, (*spend_height).into())?;
                    let _ =
                        wdb.mark_transparent_utxo_spent(&OutPoint::new(pid, *prev_idx), tx_ref)?;
                }
            }

            save(ext, &state)?;
            Ok(confirmed)
        })
    }

    pub fn transparent_pir_status(&self) -> Result<serde_json::Value> {
        self.transparent_transaction(|wdb,ext| -> anyhow::Result<_> {
            let mut state=load(ext)?;
            let missing=missing_projection(ext,&state)?;
            if invalidate_missing(&mut state,&missing) { save(ext,&state)?; }
            state.durable=state.checkpoint_id.is_some();
            if let Some(scope)=&state.scope {
                if wdb.block_fully_scanned()?.is_none_or(|b|u64::from(u32::from(b.block_height()))!=scope.anchor.height || b.block_hash().to_string()!=scope.anchor.hash) { state.complete=false; }
            }
            let mut rows=ext.prepare("SELECT t.txid,n.output_index,n.value_zat FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.mined_height IS NOT NULL AND NOT EXISTS(SELECT 1 FROM transparent_received_output_spends s WHERE s.transparent_received_output_id=n.id)")?;
            let outputs=rows.query_map([],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,u32>(1)?,r.get::<_,u64>(2)?)))?;
            let mut total=0u64;
            for output in outputs {
                let (txid,index,value)=output?;
                if state.records.contains_key(&format!("r:{}:{index}",crate::web::to_hex(&txid))) {
                    total=total.checked_add(value).ok_or_else(||anyhow::anyhow!("transparent_pir_amount_invalid"))?;
                }
            }
            Zatoshis::from_u64(total).map_err(|_|anyhow::anyhow!("transparent_pir_amount_invalid"))?;
            let mut status=state.status();status["confirmedZat"]=serde_json::json!(total.to_string());Ok(status)
        })
    }

    pub fn transparent_pir_status_for(&self, source_id: &str) -> Result<serde_json::Value> {
        let status = self.transparent_pir_status()?;
        if status.pointer("/scope/sourceId").and_then(|v| v.as_str()) != Some(source_id) {
            return Ok(
                serde_json::json!({"completion":"incomplete","spendingEnabled":false,"trust":"publisher-provided-confirmed-history"}),
            );
        }
        Ok(status)
    }

    pub fn transparent_pir_checkpoint(&self) -> Result<Option<String>> {
        self.transparent_transaction(|_, ext| -> anyhow::Result<_> {
            Ok(load(ext)?
                .continuation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?)
        })
    }
}
