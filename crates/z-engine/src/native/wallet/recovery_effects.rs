//! Derive owned shielded effects from verified transactions, never cached note rows.
use super::{RecoveryCancellation, VerifiedRegtestRecoveryBlocks, ZNetwork};
use ff::PrimeField;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use zcash_client_backend::{decrypt_transaction, TransferType};
use zcash_keys::{
    address::Receiver,
    keys::{UnifiedAddressRequest, UnifiedFullViewingKey},
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    memo::MemoBytes,
};
use zip32::Scope;

type NoteKey = (String, String, usize);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NoteEffect {
    pool: String,
    txid: String,
    index: usize,
    account: i64,
    diversifier: String,
    value: u64,
    first: String,
    second: String,
    version: Option<i64>,
    nf: Option<String>,
    position: Option<u64>,
    scope: i64,
    change: bool,
    address: Option<String>,
    memo: Option<String>,
}
impl NoteEffect {
    fn key(&self) -> NoteKey {
        (self.pool.clone(), self.txid.clone(), self.index)
    }
}

fn invalid() -> anyhow::Error {
    anyhow::anyhow!("native_recovery_reconciliation_required")
}
fn hex(bytes: &[u8]) -> String {
    crate::web::to_hex(bytes)
}
// Match the pinned SQLite memo_repr encoding, including its empty-memo sentinel.
fn memo_identity(memo: &MemoBytes) -> String {
    if memo == &MemoBytes::empty() {
        hex(&[0xf6])
    } else {
        hex(memo.as_slice())
    }
}
fn bytes(encoded: &str) -> anyhow::Result<Vec<u8>> {
    crate::web::from_hex(encoded).map_err(|_| invalid())
}
fn keys_from_rows(
    rows: impl IntoIterator<Item = (i64, String)>,
    network: ZNetwork,
) -> anyhow::Result<HashMap<i64, UnifiedFullViewingKey>> {
    rows.into_iter()
        .map(|(account, encoded)| {
            UnifiedFullViewingKey::decode(&network, &encoded)
                .map(|key| (account, key))
                .map_err(|_| invalid())
        })
        .collect()
}

fn owned_effects(
    network: ZNetwork,
    transaction: &Transaction,
    height: Option<BlockHeight>,
    tip: BlockHeight,
    keys: &HashMap<i64, UnifiedFullViewingKey>,
    starts: [u64; 3],
) -> anyhow::Result<Vec<NoteEffect>> {
    let decrypted = decrypt_transaction(&network, height, Some(tip), transaction, keys);
    let mut effects = Vec::new();
    for output in decrypted.sapling_outputs() {
        if output.transfer_type() == TransferType::Outgoing {
            continue;
        }
        let internal = output.transfer_type() == TransferType::AccountInternal;
        let scope = if internal {
            Scope::Internal
        } else {
            Scope::External
        };
        let account = *output.account();
        let key = keys.get(&account).ok_or_else(invalid)?;
        let note = output.note();
        let position = height.map(|_| starts[0] + output.index() as u64);
        let nf: Option<[u8; 32]> = position.map(|p| {
            note.nf(
                &key.sapling().expect("key decrypted Sapling").to_nk(scope),
                p,
            )
            .0
        });
        let address = if internal {
            None
        } else {
            let ivk = key.to_unified_incoming_viewing_key();
            let index = ivk
                .sapling()
                .as_ref()
                .ok_or_else(invalid)?
                .decrypt_diversifier(&note.recipient())
                .ok_or_else(invalid)?;
            Some(
                ivk.address(index, UnifiedAddressRequest::ALLOW_ALL)?
                    .encode(&network),
            )
        };
        effects.push(NoteEffect {
            pool: "sapling".into(),
            txid: hex(transaction.txid().as_ref()),
            index: output.index(),
            account,
            diversifier: hex(&note.recipient().diversifier().0),
            value: note.value().inner(),
            first: hex(&note.rcm().to_repr()),
            second: String::new(),
            version: None,
            nf: nf.map(|n| hex(&n)),
            position,
            scope: i64::from(internal),
            change: internal,
            address,
            memo: Some(memo_identity(output.memo())),
        });
    }
    for (pool, outputs, start) in [
        ("orchard", decrypted.orchard_outputs(), starts[1]),
        ("ironwood", decrypted.ironwood_outputs(), starts[2]),
    ] {
        for output in outputs {
            if output.transfer_type() == TransferType::Outgoing {
                continue;
            }
            let internal = output.transfer_type() == TransferType::AccountInternal;
            let account = *output.account();
            let key = keys.get(&account).ok_or_else(invalid)?;
            let note = &output.note().0;
            let position = height.map(|_| start + output.index() as u64);
            let nf = height.map(|_| note.nullifier(key.orchard().expect("key decrypted Orchard")));
            let address = if internal {
                None
            } else {
                let ivk = key.to_unified_incoming_viewing_key();
                let index = ivk
                    .orchard()
                    .as_ref()
                    .ok_or_else(invalid)?
                    .diversifier_index(&note.recipient())
                    .ok_or_else(invalid)?;
                Some(
                    ivk.address(index, UnifiedAddressRequest::ALLOW_ALL)?
                        .encode(&network),
                )
            };
            effects.push(NoteEffect {
                pool: pool.into(),
                txid: hex(transaction.txid().as_ref()),
                index: output.index(),
                account,
                diversifier: hex(note.recipient().diversifier().as_array()),
                value: note.value().inner(),
                first: hex(&note.rho().to_bytes()),
                second: hex(note.rseed().as_bytes()),
                version: Some(match note.version() {
                    orchard::note::NoteVersion::V2 => 2,
                    orchard::note::NoteVersion::V3 => 3,
                }),
                nf: nf.map(|n| hex(&n.to_bytes())),
                position,
                scope: i64::from(internal),
                change: internal,
                address,
                memo: Some(memo_identity(output.memo())),
            });
        }
    }
    Ok(effects)
}

pub(super) fn reconcile_shielded_outputs(
    tx: &rusqlite::Transaction<'_>,
    network: ZNetwork,
    evidence: &VerifiedRegtestRecoveryBlocks,
    pending: &[Transaction],
    cancel: &RecoveryCancellation,
) -> anyhow::Result<()> {
    let rows = tx
        .prepare("SELECT id,ufvk FROM accounts WHERE ufvk IS NOT NULL")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let keys = keys_from_rows(rows, network)?;
    let mut expected = BTreeMap::new();
    let mut starts = [0u64; 3];
    for block in &evidence.blocks {
        for transaction in block.vtx() {
            if cancel.is_cancelled() {
                return Err(invalid());
            }
            for effect in owned_effects(
                network,
                transaction,
                Some(block.claimed_height()),
                evidence.target_height().into(),
                &keys,
                starts,
            )? {
                if expected.insert(effect.key(), effect).is_some() {
                    return Err(invalid());
                }
            }
            starts[0] += transaction
                .sapling_bundle()
                .map_or(0, |b| b.shielded_outputs().len()) as u64;
            starts[1] += transaction
                .orchard_bundle()
                .map_or(0, |b| b.actions().len()) as u64;
            starts[2] += transaction
                .ironwood_bundle()
                .map_or(0, |b| b.actions().len()) as u64;
        }
    }
    let mut pending_nfs = BTreeSet::new();
    for transaction in pending {
        for effect in owned_effects(
            network,
            transaction,
            None,
            evidence.target_height().into(),
            &keys,
            [0; 3],
        )? {
            if expected.insert(effect.key(), effect).is_some() {
                return Err(invalid());
            }
        }
        if let Some(b) = transaction.sapling_bundle() {
            pending_nfs.extend(
                b.shielded_spends()
                    .iter()
                    .map(|s| ("sapling".to_owned(), hex(&s.nullifier().0))),
            );
        }
        if let Some(b) = transaction.orchard_bundle() {
            pending_nfs.extend(
                b.actions()
                    .iter()
                    .map(|a| ("orchard".to_owned(), hex(&a.nullifier().to_bytes()))),
            );
        }
        if let Some(b) = transaction.ironwood_bundle() {
            pending_nfs.extend(
                b.actions()
                    .iter()
                    .map(|a| ("ironwood".to_owned(), hex(&a.nullifier().to_bytes()))),
            );
        }
    }
    for pool in ["sapling", "orchard", "ironwood"] {
        let index = if pool == "sapling" {
            "output_index"
        } else {
            "action_index"
        };
        let rows = tx.prepare(&format!("SELECT n.id,hex(t.txid),n.{index},hex(n.nf),n.lock_expiry_height FROM {pool}_received_notes n JOIN transactions t ON t.id_tx=n.transaction_id"))?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?.to_ascii_lowercase(), r.get::<_, usize>(2)?, r.get::<_, String>(3)?.to_ascii_lowercase(), r.get::<_, Option<u32>>(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, txid, output_index, nf, lock) in rows {
            if let Some(effect) = expected.get(&(pool.to_owned(), txid, output_index)) {
                if effect.nf.as_deref().unwrap_or("") != nf
                    && pending_nfs.contains(&(pool.to_owned(), nf))
                {
                    return Err(invalid());
                }
            } else {
                // An unresolved authored spend or held lock cannot be discarded as corruption.
                if lock.is_some() || pending_nfs.contains(&(pool.to_owned(), nf)) {
                    return Err(invalid());
                }
                tx.execute(
                    &format!(
                        "DELETE FROM {pool}_received_note_spends WHERE {pool}_received_note_id=?1"
                    ),
                    [id],
                )?;
                tx.execute(
                    &format!("DELETE FROM {pool}_received_notes WHERE id=?1"),
                    [id],
                )?;
            }
        }
    }
    for effect in expected.values() {
        if cancel.is_cancelled() {
            return Err(invalid());
        }
        repair_note(tx, effect)?;
    }
    Ok(())
}

fn repair_note(tx: &rusqlite::Transaction<'_>, effect: &NoteEffect) -> anyhow::Result<()> {
    let pool = &effect.pool;
    let index = if pool == "sapling" {
        "output_index"
    } else {
        "action_index"
    };
    let address_id = if let Some(address) = &effect.address {
        Some(
            tx.query_row(
                "SELECT id FROM addresses WHERE account_id=?1 AND address=?2 AND key_scope=0",
                params![effect.account, address],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(invalid)?,
        )
    } else {
        None
    };
    let secret = if pool == "sapling" {
        "rcm=?9"
    } else {
        "rho=?9,rseed=?10,note_version=?11"
    };
    let sql = format!("UPDATE {pool}_received_notes SET account_id=?1,address_id=?2,diversifier=?3,value=?4,nf=?5,commitment_tree_position=?6,recipient_key_scope=?7,is_change=?8,witness_stabilized=0,memo=?14,{secret} WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?12) AND {index}=?13");
    let mut stmt = tx.prepare(&sql)?;
    stmt.raw_bind_parameter(1, effect.account)?;
    stmt.raw_bind_parameter(2, address_id)?;
    stmt.raw_bind_parameter(3, bytes(&effect.diversifier)?)?;
    stmt.raw_bind_parameter(4, effect.value)?;
    stmt.raw_bind_parameter(5, effect.nf.as_deref().map(bytes).transpose()?)?;
    stmt.raw_bind_parameter(6, effect.position)?;
    stmt.raw_bind_parameter(7, effect.scope)?;
    stmt.raw_bind_parameter(8, effect.change)?;
    stmt.raw_bind_parameter(9, bytes(&effect.first)?)?;
    if pool != "sapling" {
        stmt.raw_bind_parameter(10, bytes(&effect.second)?)?;
        stmt.raw_bind_parameter(11, effect.version)?;
    }
    stmt.raw_bind_parameter(12, bytes(&effect.txid)?)?;
    stmt.raw_bind_parameter(13, effect.index as u64)?;
    stmt.raw_bind_parameter(14, effect.memo.as_deref().map(bytes).transpose()?)?;
    if stmt.raw_execute()? != 1 {
        return Err(invalid());
    }
    Ok(())
}

fn note_json(pool: &str, filter: &str) -> String {
    let index = if pool == "sapling" {
        "output_index"
    } else {
        "action_index"
    };
    let first = if pool == "sapling" { "rcm" } else { "rho" };
    let second = if pool == "sapling" {
        "''"
    } else {
        "lower(hex(n.rseed))"
    };
    let version = if pool == "sapling" {
        "NULL"
    } else {
        "n.note_version"
    };
    format!("SELECT coalesce(json_group_array(json(entry)), '[]') FROM (SELECT json_object('pool','{pool}','txid',lower(hex(t.txid)),'index',n.{index},'account',n.account_id,'diversifier',lower(hex(n.diversifier)),'value',n.value,'first',lower(hex(n.{first})),'second',{second},'version',{version},'nf',CASE WHEN n.nf IS NULL THEN NULL ELSE lower(hex(n.nf)) END,'position',n.commitment_tree_position,'scope',n.recipient_key_scope,'change',json(CASE WHEN n.is_change THEN 'true' ELSE 'false' END),'address',a.address,'memo',CASE WHEN n.memo IS NULL THEN NULL ELSE lower(hex(n.memo)) END) AS entry FROM {pool}_received_notes n JOIN transactions t ON t.id_tx=n.transaction_id LEFT JOIN addresses a ON a.id=n.address_id WHERE {filter} ORDER BY t.txid,n.{index})")
}

/// Bind fixed mined note effects. Validate newly signed pending outputs against
/// native decryption without including those legitimate additions in the receipt.
pub(super) fn shielded_effect_identity(
    mut query: impl FnMut(&str) -> rusqlite::Result<String>,
    network: ZNetwork,
    height: u32,
    authored: &BTreeSet<String>,
) -> anyhow::Result<String> {
    let key_rows = query("SELECT coalesce(json_group_array(json(entry)),'[]') FROM (SELECT json_array(id,ufvk) AS entry FROM accounts WHERE ufvk IS NOT NULL ORDER BY id)")?;
    let keys = keys_from_rows(
        serde_json::from_str::<Vec<(i64, String)>>(&key_rows)?,
        network,
    )?;
    let mut hash = Sha256::new();
    hash.update(key_rows.as_bytes());
    hash.update([0]);
    let mut pending = BTreeMap::new();
    let raws = query("SELECT coalesce(json_group_array(json(entry)),'[]') FROM (SELECT json_array(lower(hex(txid)),lower(hex(raw)),created IS NOT NULL) AS entry FROM transactions WHERE mined_height IS NULL ORDER BY txid)")?;
    for (id, raw, created) in serde_json::from_str::<Vec<(String, String, i64)>>(&raws)? {
        let mut display = bytes(&id)?;
        display.reverse();
        if created == 0 && !authored.contains(&hex(&display)) {
            continue;
        }
        let raw = bytes(&raw)?;
        let mut remainder = raw.as_slice();
        let transaction = Transaction::read(
            &mut remainder,
            BranchId::for_height(&network, height.into()),
        )
        .map_err(|_| invalid())?;
        let mut canonical = Vec::new();
        transaction.write(&mut canonical)?;
        if !remainder.is_empty() || canonical != raw || hex(transaction.txid().as_ref()) != id {
            return Err(invalid());
        }
        for note in owned_effects(network, &transaction, None, height.into(), &keys, [0; 3])? {
            if pending.insert(note.key(), note).is_some() {
                return Err(invalid());
            }
        }
    }
    let mut actual_pending = BTreeMap::new();
    for pool in ["sapling", "orchard", "ironwood"] {
        let mined = query(&note_json(pool, "t.mined_height IS NOT NULL"))?;
        hash.update(mined.as_bytes());
        hash.update([0]);
        // The flag can bypass all maturity and scan checks; native full-prefix
        // recovery resets it instead of trusting a previous scanner's claim.
        let flags = query(&format!("SELECT coalesce(group_concat(entry,'|'),'') FROM (SELECT hex(t.txid)||':'||n.id||':'||n.witness_stabilized||':'||t.mined_height||':'||coalesce(t.tx_index,-1)||':'||hex(a.uuid)||':'||coalesce(n.address_id,-1) AS entry FROM {pool}_received_notes n JOIN transactions t ON t.id_tx=n.transaction_id JOIN accounts a ON a.id=n.account_id WHERE t.mined_height IS NOT NULL ORDER BY t.txid,n.id)"))?;
        hash.update(flags.as_bytes());
        hash.update([0]);
        let rows: Vec<NoteEffect> =
            serde_json::from_str(&query(&note_json(pool, "t.mined_height IS NULL"))?)?;
        for row in rows {
            if pending.get(&row.key()) != Some(&row) {
                return Err(invalid());
            }
            if query(&format!("SELECT CAST(witness_stabilized AS TEXT) FROM {pool}_received_notes n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=x'{}' AND n.{}={}",row.txid,if pool == "sapling" {"output_index"} else {"action_index"},row.index))? != "0" { return Err(invalid()); }
            if actual_pending.insert(row.key(), row).is_some() {
                return Err(invalid());
            }
        }
    }
    if actual_pending != pending {
        return Err(invalid());
    }
    hash.update(sent_effect_identity(&mut query, network, height, authored, &keys)?.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

type SentKey = (String, i64, usize);
struct SentEffect {
    txid: String,
    pool: i64,
    index: usize,
    from: i64,
    to: Option<i64>,
    receiver: Receiver,
    value: u64,
    memo: Option<String>,
}
impl SentEffect {
    fn key(&self) -> SentKey {
        (self.txid.clone(), self.pool, self.index)
    }
}
#[derive(Deserialize)]
struct SentRow {
    id: i64,
    txid: String,
    pool: i64,
    index: usize,
    from: i64,
    to: Option<i64>,
    address: Option<String>,
    value: u64,
    created: i64,
    mined: i64,
    memo: Option<String>,
}
impl SentRow {
    fn key(&self) -> SentKey {
        (self.txid.clone(), self.pool, self.index)
    }
}
fn sent_json(filter: &str) -> String {
    format!("SELECT coalesce(json_group_array(json(entry)),'[]') FROM (SELECT json_object('id',s.id,'txid',lower(hex(t.txid)),'pool',s.output_pool,'index',s.output_index,'from',s.from_account_id,'to',s.to_account_id,'address',s.to_address,'value',s.value,'memo',CASE WHEN s.memo IS NULL THEN NULL ELSE lower(hex(s.memo)) END,'created',t.created IS NOT NULL,'mined',t.mined_height IS NOT NULL) AS entry FROM sent_notes s JOIN transactions t ON t.id_tx=s.transaction_id WHERE {filter} ORDER BY t.txid,s.output_pool,s.output_index)")
}
fn funding_account(
    query: &mut impl FnMut(&str) -> rusqlite::Result<String>,
    transaction: &Transaction,
) -> anyhow::Result<Option<i64>> {
    let mut accounts = BTreeSet::new();
    for (pool, nfs) in [
        (
            "sapling",
            transaction.sapling_bundle().map_or_else(Vec::new, |b| {
                b.shielded_spends()
                    .iter()
                    .map(|s| hex(&s.nullifier().0))
                    .collect::<Vec<_>>()
            }),
        ),
        (
            "orchard",
            transaction.orchard_bundle().map_or_else(Vec::new, |b| {
                b.actions()
                    .iter()
                    .map(|a| hex(&a.nullifier().to_bytes()))
                    .collect()
            }),
        ),
        (
            "ironwood",
            transaction.ironwood_bundle().map_or_else(Vec::new, |b| {
                b.actions()
                    .iter()
                    .map(|a| hex(&a.nullifier().to_bytes()))
                    .collect()
            }),
        ),
    ] {
        for nf in nfs {
            let encoded = query(&format!("SELECT coalesce(json_group_array(account_id),'[]') FROM {pool}_received_notes WHERE nf=x'{nf}'"))?;
            accounts.extend(serde_json::from_str::<Vec<i64>>(&encoded)?);
        }
    }
    if let Some(b) = transaction.transparent_bundle() {
        if !b.is_coinbase() {
            for input in &b.vin {
                let previous = input.prevout();
                let encoded = query(&format!("SELECT coalesce(json_group_array(n.account_id),'[]') FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=x'{}' AND n.output_index={}",hex(previous.hash()),previous.n()))?;
                accounts.extend(serde_json::from_str::<Vec<i64>>(&encoded)?);
            }
        }
    }
    // Pinned backend selects one arbitrary funder for a multi-account payment.
    // This recovery profile cannot authenticate that choice from transaction effects.
    if accounts.len() > 1 {
        return Err(invalid());
    }
    Ok(accounts.into_iter().next())
}
fn sent_effects(
    query: &mut impl FnMut(&str) -> rusqlite::Result<String>,
    network: ZNetwork,
    transaction: &Transaction,
    height: Option<BlockHeight>,
    tip: BlockHeight,
    keys: &HashMap<i64, UnifiedFullViewingKey>,
) -> anyhow::Result<Vec<SentEffect>> {
    let funding = funding_account(query, transaction)?;
    let decrypted = decrypt_transaction(&network, height, Some(tip), transaction, keys);
    let mut effects = Vec::new();
    let parties = |account, transfer| match transfer {
        TransferType::Outgoing => Some((account, None)),
        TransferType::AccountInternal => Some((account, Some(account))),
        TransferType::Incoming => funding.map(|from| (from, Some(account))),
        TransferType::WalletInternal => None,
    };
    for output in decrypted.sapling_outputs() {
        if let Some((from, to)) = parties(*output.account(), output.transfer_type()) {
            effects.push(SentEffect {
                txid: hex(transaction.txid().as_ref()),
                pool: 2,
                index: output.index(),
                from,
                to,
                receiver: Receiver::Sapling(output.note().recipient()),
                value: output.note().value().inner(),
                memo: Some(memo_identity(output.memo())),
            });
        }
    }
    for (pool, outputs) in [
        (3, decrypted.orchard_outputs()),
        (4, decrypted.ironwood_outputs()),
    ] {
        for output in outputs {
            if let Some((from, to)) = parties(*output.account(), output.transfer_type()) {
                effects.push(SentEffect {
                    txid: hex(transaction.txid().as_ref()),
                    pool,
                    index: output.index(),
                    from,
                    to,
                    receiver: Receiver::Orchard(output.note().0.recipient()),
                    value: output.note().0.value().inner(),
                    memo: Some(memo_identity(output.memo())),
                });
            }
        }
    }
    if let (Some(from), Some(bundle)) = (funding, transaction.transparent_bundle()) {
        for (index, output) in bundle.vout.iter().enumerate() {
            if let Some(address) = output.recipient_address() {
                let encoded=query(&format!("SELECT coalesce(json_group_array(account_id),'[]') FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=x'{}' AND n.output_index={}",hex(transaction.txid().as_ref()),index))?;
                let to = serde_json::from_str::<Vec<i64>>(&encoded)?;
                if to.len() > 1 {
                    return Err(invalid());
                }
                effects.push(SentEffect {
                    txid: hex(transaction.txid().as_ref()),
                    pool: 0,
                    index,
                    from,
                    to: to.into_iter().next(),
                    receiver: Receiver::Transparent(address),
                    value: u64::from(output.value()),
                    memo: None,
                });
            }
        }
    }
    Ok(effects)
}
fn matches_recipient(network: ZNetwork, effect: &SentEffect, row: &SentRow) -> bool {
    match &row.address {
        Some(address) => {
            zcash_keys::address::Address::decode(&network, address).is_some()
                && address
                    .parse::<zcash_address::ZcashAddress>()
                    .ok()
                    .is_some_and(|a| effect.receiver.corresponds(&a))
        }
        None => effect.to.is_some(),
    }
}

/// The caller has already reconciled all owned parents before deriving funding.
pub(super) fn reconcile_sent_outputs(
    tx: &rusqlite::Transaction<'_>,
    network: ZNetwork,
    evidence: &VerifiedRegtestRecoveryBlocks,
    pending: &[Transaction],
    cancel: &RecoveryCancellation,
) -> anyhow::Result<()> {
    let keys = keys_from_rows(
        tx.prepare("SELECT id,ufvk FROM accounts WHERE ufvk IS NOT NULL")?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        network,
    )?;
    let mut expected = BTreeMap::new();
    let mut transactions = BTreeMap::new();
    for block in &evidence.blocks {
        for transaction in block.vtx() {
            transactions.insert(hex(transaction.txid().as_ref()), transaction);
            for effect in sent_effects(
                &mut |sql| tx.query_row(sql, [], |r| r.get(0)),
                network,
                transaction,
                Some(block.claimed_height()),
                evidence.target_height().into(),
                &keys,
            )? {
                if expected.insert(effect.key(), effect).is_some() {
                    return Err(invalid());
                }
            }
        }
    }
    for transaction in pending {
        transactions.insert(hex(transaction.txid().as_ref()), transaction);
        for effect in sent_effects(
            &mut |sql| tx.query_row(sql, [], |r| r.get(0)),
            network,
            transaction,
            None,
            evidence.target_height().into(),
            &keys,
        )? {
            if expected.insert(effect.key(), effect).is_some() {
                return Err(invalid());
            }
        }
    }
    let rows: Vec<SentRow> =
        serde_json::from_str(&tx.query_row(&sent_json("1"), [], |r| r.get::<_, String>(0))?)?;
    let mut actual = BTreeSet::new();
    for row in rows {
        if cancel.is_cancelled() {
            return Err(invalid());
        }
        if let Some(effect) = expected.get(&row.key()) {
            if !matches_recipient(network, effect, &row)
                || (effect.memo.is_some() && row.memo != effect.memo)
            {
                return Err(invalid());
            }
            tx.execute(
                "UPDATE sent_notes SET from_account_id=?1,to_account_id=?2,value=?3 WHERE id=?4",
                params![effect.from, effect.to, effect.value, row.id],
            )?;
            actual.insert(row.key());
        } else {
            // Pending metadata is part of an unresolved durable intent. Refuse
            // unprovable effects without deleting the wallet's authored history.
            if row.mined == 0 {
                return Err(invalid());
            }
            if row.created != 0 {
                let transaction = transactions.get(&row.txid).ok_or_else(invalid)?;
                let count = match row.pool {
                    0 => transaction.transparent_bundle().map_or(0, |b| b.vout.len()),
                    2 => transaction
                        .sapling_bundle()
                        .map_or(0, |b| b.shielded_outputs().len()),
                    3 => transaction
                        .orchard_bundle()
                        .map_or(0, |b| b.actions().len()),
                    4 => transaction
                        .ironwood_bundle()
                        .map_or(0, |b| b.actions().len()),
                    _ => 0,
                };
                if row.index < count {
                    return Err(invalid());
                }
            }
            tx.execute("DELETE FROM sent_notes WHERE id=?1", [row.id])?;
        }
    }
    if actual != expected.keys().cloned().collect() {
        return Err(invalid());
    }
    Ok(())
}
fn sent_effect_identity(
    query: &mut impl FnMut(&str) -> rusqlite::Result<String>,
    network: ZNetwork,
    height: u32,
    authored: &BTreeSet<String>,
    keys: &HashMap<i64, UnifiedFullViewingKey>,
) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    hash.update(query(&sent_json("t.mined_height IS NOT NULL"))?.as_bytes());
    let raws=query("SELECT coalesce(json_group_array(json(entry)),'[]') FROM (SELECT json_array(lower(hex(txid)),lower(hex(raw)),created IS NOT NULL) AS entry FROM transactions WHERE mined_height IS NULL ORDER BY txid)")?;
    let mut expected = BTreeMap::new();
    for (id, raw, created) in serde_json::from_str::<Vec<(String, String, i64)>>(&raws)? {
        let mut display = bytes(&id)?;
        display.reverse();
        if created == 0 && !authored.contains(&hex(&display)) {
            continue;
        }
        let raw = bytes(&raw)?;
        let mut remainder = raw.as_slice();
        let transaction = Transaction::read(
            &mut remainder,
            BranchId::for_height(&network, height.into()),
        )
        .map_err(|_| invalid())?;
        let mut canonical = Vec::new();
        transaction.write(&mut canonical)?;
        if !remainder.is_empty() || canonical != raw || hex(transaction.txid().as_ref()) != id {
            return Err(invalid());
        }
        for effect in sent_effects(query, network, &transaction, None, height.into(), keys)? {
            if expected.insert(effect.key(), effect).is_some() {
                return Err(invalid());
            }
        }
    }
    let mut actual = BTreeSet::new();
    let rows: Vec<SentRow> = serde_json::from_str(&query(&sent_json("t.mined_height IS NULL"))?)?;
    for row in rows {
        let effect = expected.get(&row.key()).ok_or_else(invalid)?;
        if row.from != effect.from
            || row.to != effect.to
            || row.value != effect.value
            || !matches_recipient(network, effect, &row)
            || (effect.memo.is_some() && row.memo != effect.memo)
        {
            return Err(invalid());
        }
        actual.insert(row.key());
    }
    if actual != expected.keys().cloned().collect() {
        return Err(invalid());
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::wallet::{
        pir::recovery::tests::encrypted_orchard_transaction,
        recovery_blocks::{
            tests::fixture_prefix_with_transactions, verify_regtest_recovery_blocks,
        },
        tests::{fixture_account, fixture_wallet},
        NativeWallet, RegtestScanSchedule, WalletRead,
    };
    use prost::Message;
    use std::{num::NonZeroU32, sync::atomic::AtomicBool};
    use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;

    fn scanned_fixture() -> (
        tempfile::TempDir,
        NativeWallet,
        VerifiedRegtestRecoveryBlocks,
    ) {
        let (key, _) = fixture_account();
        scanned_fixture_with_transaction(encrypted_orchard_transaction(&key, 7, [0; 32]))
    }

    fn scanned_fixture_with_transaction(
        transaction: Transaction,
    ) -> (
        tempfile::TempDir,
        NativeWallet,
        VerifiedRegtestRecoveryBlocks,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        let (bytes, chain) = fixture_prefix_with_transactions(&[0x51], 3, vec![(3, transaction)]);
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        let compact = evidence.compact_blocks().unwrap();
        wallet
            .scan_public_regtest(
                &compact
                    .iter()
                    .flat_map(Message::encode_length_delimited_to_vec)
                    .collect::<Vec<_>>(),
                RegtestScanSchedule {
                    nu6_3_height: crate::regtest_nu6_3_height(),
                    nu7_height: crate::regtest_nu7_height(),
                },
            )
            .unwrap();
        (dir, wallet, evidence)
    }

    #[test]
    fn independent_decryption_removes_extra_indices_and_repairs_real_note_without_losing_lock() {
        let (_dir, wallet, evidence) = scanned_fixture();
        let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let original: (i64, Vec<u8>, Vec<u8>, u64) = conn
            .query_row(
                "SELECT id,nf,rseed,commitment_tree_position FROM orchard_received_notes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        conn.execute_batch("INSERT INTO orchard_received_notes(transaction_id,action_index,account_id,address_id,diversifier,value,rho,rseed,nf,is_change,commitment_tree_position,recipient_key_scope,note_version) SELECT transaction_id,99,account_id,address_id,diversifier,999000,rho,rseed,zeroblob(32),is_change,commitment_tree_position,recipient_key_scope,note_version FROM orchard_received_notes LIMIT 1;
            ").unwrap();
        conn.execute("UPDATE orchard_received_notes SET value=443331,rseed=zeroblob(32),nf=?1,recipient_key_scope=1,is_change=1,witness_stabilized=1,commitment_tree_position=999,lock_expiry_height=42,memo=x'f6' WHERE action_index=0",[vec![0xff;32]]).unwrap();
        let tx = conn.transaction().unwrap();
        reconcile_shielded_outputs(
            &tx,
            ZNetwork::Regtest,
            &evidence,
            &[],
            &RecoveryCancellation::new(),
        )
        .unwrap();
        tx.commit().unwrap();
        let actual:(i64,Vec<u8>,Vec<u8>,u64,u64,bool,bool,u32,Vec<u8>) = conn.query_row("SELECT id,nf,rseed,commitment_tree_position,value,is_change,witness_stabilized,lock_expiry_height,memo FROM orchard_received_notes",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).unwrap();
        assert_eq!((actual.0, actual.1, actual.2, actual.3), original);
        assert_eq!(
            (actual.4, actual.5, actual.6, actual.7, actual.8),
            (50_000, false, false, 42, vec![])
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM orchard_received_notes", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            1
        );
        let before = shielded_effect_identity(
            |sql| conn.query_row(sql, [], |r| r.get(0)),
            ZNetwork::Regtest,
            3,
            &BTreeSet::new(),
        )
        .unwrap();
        conn.execute("UPDATE orchard_received_notes SET witness_stabilized=1", [])
            .unwrap();
        let after = shielded_effect_identity(
            |sql| conn.query_row(sql, [], |r| r.get(0)),
            ZNetwork::Regtest,
            3,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn mined_received_memo_repairs_stale_nonnull_bytes_and_binds_the_actual_receipt() {
        for memo in [
            MemoBytes::empty(),
            MemoBytes::from_bytes(b"verified native memo").unwrap(),
        ] {
            let (key, _) = fixture_account();
            let transaction = sender_encrypted_transaction_with_memo(
                &key,
                &key,
                Scope::External,
                [0; 32],
                *memo.as_array(),
            );
            let (_dir, wallet, evidence) = scanned_fixture_with_transaction(transaction);
            let actual = &evidence.blocks[2].vtx()[1];
            zcash_client_backend::data_api::wallet::decrypt_and_store_transaction(
                &ZNetwork::Regtest,
                &mut wallet.open_db().unwrap(),
                actual,
                Some(3.into()),
            )
            .unwrap();
            let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let identity = |conn: &rusqlite::Connection| {
                shielded_effect_identity(
                    |sql| conn.query_row(sql, [], |r| r.get(0)),
                    ZNetwork::Regtest,
                    3,
                    &BTreeSet::new(),
                )
                .unwrap()
            };
            let before = identity(&conn);
            let original: (i64, Vec<u8>) = conn
                .query_row("SELECT id,memo FROM orchard_received_notes", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!(original.1, bytes(&memo_identity(&memo)).unwrap());
            conn.execute(
                "UPDATE orchard_received_notes SET memo=x'01',lock_expiry_height=42",
                [],
            )
            .unwrap();
            assert_ne!(identity(&conn), before);
            let tx = conn.transaction().unwrap();
            reconcile_shielded_outputs(
                &tx,
                ZNetwork::Regtest,
                &evidence,
                &[],
                &RecoveryCancellation::new(),
            )
            .unwrap();
            tx.commit().unwrap();
            let repaired: (i64, Vec<u8>, u32) = conn
                .query_row(
                    "SELECT id,memo,lock_expiry_height FROM orchard_received_notes",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!((repaired.0, repaired.1), original);
            assert_eq!(repaired.2, 42);
            assert_eq!(identity(&conn), before);
            conn.execute("UPDATE orchard_received_notes SET memo=x'02'", [])
                .unwrap();
            assert_ne!(identity(&conn), before);
        }
    }

    #[test]
    fn real_immature_note_cannot_keep_fabricated_stabilized_spendability() {
        let (_dir, wallet, evidence) = scanned_fixture();
        let policy = ConfirmationsPolicy::new_symmetrical(NonZeroU32::new(10).unwrap(), false);
        let spendable = || -> u64 {
            wallet
                .open_db()
                .unwrap()
                .get_wallet_summary(policy)
                .unwrap()
                .unwrap()
                .account_balances()
                .values()
                .map(|b| u64::from(b.orchard_balance().spendable_value()))
                .sum()
        };
        assert_eq!(spendable(), 0);
        let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute("UPDATE orchard_received_notes SET witness_stabilized=1", [])
            .unwrap();
        assert_eq!(spendable(), 50_000);
        let tx = conn.transaction().unwrap();
        reconcile_shielded_outputs(
            &tx,
            ZNetwork::Regtest,
            &evidence,
            &[],
            &RecoveryCancellation::new(),
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(spendable(), 0);
    }

    #[test]
    fn unexplained_locked_extra_note_fails_without_deleting_the_lock() {
        let (_dir, wallet, evidence) = scanned_fixture();
        let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute_batch("INSERT INTO orchard_received_notes(transaction_id,action_index,account_id,address_id,diversifier,value,rho,rseed,nf,is_change,commitment_tree_position,recipient_key_scope,note_version,lock_expiry_height) SELECT transaction_id,99,account_id,address_id,diversifier,999000,rho,rseed,zeroblob(32),is_change,commitment_tree_position,recipient_key_scope,note_version,42 FROM orchard_received_notes LIMIT 1;").unwrap();
        let tx = conn.transaction().unwrap();
        assert!(reconcile_shielded_outputs(
            &tx,
            ZNetwork::Regtest,
            &evidence,
            &[],
            &RecoveryCancellation::new()
        )
        .is_err());
        drop(tx);
        assert_eq!(
            conn.query_row(
                "SELECT lock_expiry_height FROM orchard_received_notes WHERE action_index=99",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            42
        );
    }
    #[test]
    fn authored_pending_owned_outputs_are_decrypted_and_complete_without_staling_mined_receipt() {
        let (_dir, wallet, _evidence) = scanned_fixture();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let identity = |conn: &rusqlite::Connection| {
            shielded_effect_identity(
                |sql| conn.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                3,
                &BTreeSet::new(),
            )
        };
        let before = identity(&conn).unwrap();
        let (key, _) = fixture_account();
        let pending = encrypted_orchard_transaction(&key, 8, [1; 32]);
        zcash_client_backend::data_api::wallet::decrypt_and_store_transaction(
            &ZNetwork::Regtest,
            &mut wallet.open_db().unwrap(),
            &pending,
            None,
        )
        .unwrap();
        // Disposable encryption/effects fixture. The authored marker models the
        // native store_transaction_to_be_sent row; it does not assert valid proofs.
        conn.execute(
            "UPDATE transactions SET created=1 WHERE txid=?1",
            [pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert_eq!(identity(&conn).unwrap(), before);
        conn.execute("UPDATE orchard_received_notes SET memo=x'01' WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert!(identity(&conn).is_err());
        conn.execute("UPDATE orchard_received_notes SET memo=x'' WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert_eq!(identity(&conn).unwrap(), before);
        conn.execute("UPDATE orchard_received_notes SET value=value+1 WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert!(identity(&conn).is_err());
        conn.execute("UPDATE orchard_received_notes SET value=value-1 WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert_eq!(identity(&conn).unwrap(), before);
        conn.execute("DELETE FROM orchard_received_notes WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[pending.txid().as_ref().as_slice()]).unwrap();
        assert!(identity(&conn).is_err());
    }

    fn sender_encrypted_transaction(
        sender: &UnifiedFullViewingKey,
        recipient: &UnifiedFullViewingKey,
        scope: Scope,
        spend: [u8; 32],
    ) -> Transaction {
        sender_encrypted_transaction_with_memo(sender, recipient, scope, spend, [7; 512])
    }

    fn sender_encrypted_transaction_with_memo(
        sender: &UnifiedFullViewingKey,
        recipient: &UnifiedFullViewingKey,
        scope: Scope,
        spend: [u8; 32],
        memo: [u8; 512],
    ) -> Transaction {
        use chacha20poly1305::{
            aead::{AeadInPlace, KeyInit},
            ChaCha20Poly1305,
        };
        use orchard::{
            note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
            note_encryption::{OrchardDomain, OrchardNoteEncryption},
            value::{NoteValue, ValueCommitment},
            Note,
        };
        use zcash_note_encryption::Domain;
        use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
        let rho = Rho::from_bytes(&spend).unwrap();
        let note = Note::from_parts(
            recipient.orchard().unwrap().address_at(0u32, scope),
            NoteValue::from_raw(50_000),
            rho,
            RandomSeed::from_bytes([9; 32], &rho).unwrap(),
            NoteVersion::V2,
        )
        .unwrap();
        let cmx = ExtractedNoteCommitment::from(note.commitment());
        let cv = ValueCommitment::from_bytes(&[0; 32]).unwrap();
        let ovk = sender.orchard().unwrap().to_ovk(Scope::External);
        let enc = OrchardNoteEncryption::new(Some(ovk.clone()), note, memo);
        let epk = OrchardDomain::epk_bytes(enc.epk());
        // Encrypt the known outgoing plaintext through the pinned domain. This
        // avoids introducing a second rand_core version solely for this fixture.
        let ock = OrchardDomain::derive_ock(&ovk, &cv, &cmx.to_bytes(), &epk);
        let plaintext = OrchardDomain::outgoing_plaintext_bytes(&note, enc.esk());
        let mut out_cipher = [0u8; 80];
        out_cipher[..64].copy_from_slice(&plaintext.0);
        let tag = ChaCha20Poly1305::new(ock.as_ref().into())
            .encrypt_in_place_detached([0u8; 12][..].into(), &[], &mut out_cipher[..64])
            .unwrap();
        out_cipher[64..].copy_from_slice(&tag);
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
        let mut raw = Vec::new();
        empty.write(&mut raw).unwrap();
        raw.pop();
        raw.push(1);
        raw.extend(cv.to_bytes());
        raw.extend(spend);
        raw.extend(epk.0);
        raw.extend(cmx.to_bytes());
        raw.extend(epk.0);
        raw.extend(enc.encrypt_note_plaintext());
        raw.extend(out_cipher);
        raw.push(3);
        raw.extend(0i64.to_le_bytes());
        raw.extend([0; 32]);
        let length = orchard::Proof::expected_proof_size(1);
        raw.push(253);
        raw.extend((length as u16).to_le_bytes());
        raw.extend(vec![0; length]);
        raw.extend([0; 128]);
        // Encryption/effects fixture only, without consensus-valid proofs/signatures.
        Transaction::read(raw.as_slice(), BranchId::Nu6_2).unwrap()
    }

    #[test]
    fn outgoing_and_self_send_history_reconcile_actual_values_preserve_ua_and_bind_receipt_drift() {
        for internal in [false, true] {
            let (_dir, wallet, _prefix) = scanned_fixture();
            let (key, _) = fixture_account();
            let foreign = crate::native::selective_scan::tests::test_ufvk(8);
            let recipient = if internal { &key } else { &foreign };
            let scope = if internal {
                Scope::Internal
            } else {
                Scope::External
            };
            let mut conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let nf: Vec<u8> = conn
                .query_row("SELECT nf FROM orchard_received_notes", [], |r| r.get(0))
                .unwrap();
            let outgoing =
                sender_encrypted_transaction(&key, recipient, scope, nf.try_into().unwrap());
            let id = outgoing.txid();
            let (bytes, chain) = fixture_prefix_with_transactions(
                &[0x51],
                8,
                vec![
                    (3, encrypted_orchard_transaction(&key, 7, [0; 32])),
                    (6, outgoing),
                ],
            );
            let evidence =
                verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
            let compact = evidence.compact_blocks().unwrap();
            wallet
                .scan_public_regtest(
                    &compact
                        .iter()
                        .flat_map(Message::encode_length_delimited_to_vec)
                        .collect::<Vec<_>>(),
                    RegtestScanSchedule {
                        nu6_3_height: crate::regtest_nu6_3_height(),
                        nu7_height: crate::regtest_nu7_height(),
                    },
                )
                .unwrap();
            let outgoing = &evidence.blocks[5].vtx()[1];
            assert_eq!(outgoing.txid(), id);
            zcash_client_backend::data_api::wallet::decrypt_and_store_transaction(
                &ZNetwork::Regtest,
                &mut wallet.open_db().unwrap(),
                outgoing,
                Some(6.into()),
            )
            .unwrap();
            let ua = if internal {
                None
            } else {
                Some(
                    zcash_keys::address::UnifiedAddress::from_receivers(
                        Some(recipient.orchard().unwrap().address_at(0u32, scope)),
                        None,
                        Some(transparent::address::TransparentAddress::PublicKeyHash(
                            [5; 20],
                        )),
                    )
                    .unwrap()
                    .encode(&ZNetwork::Regtest),
                )
            };
            if let Some(ua) = &ua {
                conn.execute("UPDATE sent_notes SET to_address=?1 WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?2)",params![ua,id.as_ref().as_slice()]).unwrap();
            }
            let original:(i64,Vec<u8>)=conn.query_row("SELECT id,memo FROM sent_notes WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)",[id.as_ref().as_slice()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            conn.execute("INSERT INTO sent_notes(transaction_id,output_pool,output_index,from_account_id,to_address,to_account_id,value,memo) SELECT transaction_id,output_pool,99,from_account_id,to_address,to_account_id,999999,memo FROM sent_notes WHERE id=?1",[original.0]).unwrap();
            conn.execute(
                "UPDATE sent_notes SET value=777777 WHERE id=?1",
                [original.0],
            )
            .unwrap();
            let tx = conn.transaction().unwrap();
            reconcile_shielded_outputs(
                &tx,
                ZNetwork::Regtest,
                &evidence,
                &[],
                &RecoveryCancellation::new(),
            )
            .unwrap();
            reconcile_sent_outputs(
                &tx,
                ZNetwork::Regtest,
                &evidence,
                &[],
                &RecoveryCancellation::new(),
            )
            .unwrap();
            tx.commit().unwrap();
            let actual: (i64, u64, Vec<u8>, Option<String>, Option<i64>) = conn
                .query_row(
                    "SELECT id,value,memo,to_address,to_account_id FROM sent_notes",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .unwrap();
            assert_eq!(
                (actual.0, actual.1, actual.2),
                (original.0, 50_000, original.1)
            );
            assert_eq!(actual.3, ua);
            assert_eq!(actual.4.is_some(), internal);
            let before = shielded_effect_identity(
                |sql| conn.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                8,
                &BTreeSet::new(),
            )
            .unwrap();
            conn.execute("UPDATE sent_notes SET value=value+1", [])
                .unwrap();
            let after = shielded_effect_identity(
                |sql| conn.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                8,
                &BTreeSet::new(),
            )
            .unwrap();
            assert_ne!(before, after);
        }
    }
    fn assert_pending_empty_memo_preserves_identity(sent: bool) {
        let (_dir, wallet, _) = scanned_fixture();
        let (key, _) = fixture_account();
        let foreign = crate::native::selective_scan::tests::test_ufvk(8);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let identity = || {
            shielded_effect_identity(
                |sql| conn.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                3,
                &BTreeSet::new(),
            )
        };
        let before = identity().unwrap();
        let spend = if sent {
            conn.query_row("SELECT nf FROM orchard_received_notes", [], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap()
            .try_into()
            .unwrap()
        } else {
            [1; 32]
        };
        let pending = sender_encrypted_transaction_with_memo(
            &key,
            if sent { &foreign } else { &key },
            Scope::External,
            spend,
            *MemoBytes::empty().as_array(),
        );
        zcash_client_backend::data_api::wallet::decrypt_and_store_transaction(
            &ZNetwork::Regtest,
            &mut wallet.open_db().unwrap(),
            &pending,
            None,
        )
        .unwrap();
        conn.execute(
            "UPDATE transactions SET created=1 WHERE txid=?1",
            [pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        let table = if sent {
            "sent_notes"
        } else {
            "orchard_received_notes"
        };
        let filter = "transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1)";
        let stored: Vec<u8> = conn
            .query_row(
                &format!("SELECT memo FROM {table} WHERE {filter}"),
                [pending.txid().as_ref().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, vec![0xf6]);
        assert_eq!(identity().unwrap(), before);
        conn.execute(
            &format!("UPDATE {table} SET memo=x'01' WHERE {filter}"),
            [pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert!(identity().is_err());
        conn.execute(
            &format!("UPDATE {table} SET memo=x'f6' WHERE {filter}"),
            [pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert_eq!(identity().unwrap(), before);
    }

    #[test]
    fn genuine_empty_pending_received_memo_uses_native_storage_encoding() {
        assert_pending_empty_memo_preserves_identity(false);
    }

    #[test]
    fn genuine_empty_pending_sent_memo_uses_native_storage_encoding() {
        assert_pending_empty_memo_preserves_identity(true);
    }

    #[test]
    fn authored_pending_shielded_sent_memo_drift_is_rejected_without_staling_mined_identity() {
        let (_dir, wallet, _) = scanned_fixture();
        let (key, _) = fixture_account();
        let foreign = crate::native::selective_scan::tests::test_ufvk(8);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let identity = || {
            shielded_effect_identity(
                |sql| conn.query_row(sql, [], |r| r.get(0)),
                ZNetwork::Regtest,
                3,
                &BTreeSet::new(),
            )
        };
        let before = identity().unwrap();
        let nf: Vec<u8> = conn
            .query_row("SELECT nf FROM orchard_received_notes", [], |r| r.get(0))
            .unwrap();
        let pending =
            sender_encrypted_transaction(&key, &foreign, Scope::External, nf.try_into().unwrap());
        zcash_client_backend::data_api::wallet::decrypt_and_store_transaction(
            &ZNetwork::Regtest,
            &mut wallet.open_db().unwrap(),
            &pending,
            None,
        )
        .unwrap();
        conn.execute(
            "UPDATE transactions SET created=1 WHERE txid=?1",
            [pending.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert_eq!(identity().unwrap(), before);
        let memo: Vec<u8> = conn
            .query_row("SELECT memo FROM sent_notes", [], |r| r.get(0))
            .unwrap();
        conn.execute("UPDATE sent_notes SET memo=x'01'", [])
            .unwrap();
        assert!(identity().is_err());
        conn.execute("UPDATE sent_notes SET memo=?1", [memo])
            .unwrap();
        assert_eq!(identity().unwrap(), before);
    }
}
