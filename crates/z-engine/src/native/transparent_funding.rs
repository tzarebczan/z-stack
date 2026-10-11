//! Classify recent compact-UTXO funding transactions before shielding.
//!
//! GetAddressUtxos does not identify coinbase transactions. The pinned sqlite
//! backend treats an unknown transaction index as non-coinbase, so its maturity
//! filter cannot work until the actual funding transaction has been stored.

use super::wallet::SyncDb;
use crate::error::{EngineError, Result};
use crate::Network;
use rusqlite::OptionalExtension;
use std::{collections::HashSet, future::Future, path::Path};
use transparent::address::TransparentAddress;
use zcash_client_backend::data_api::{
    wallet::{
        decrypt_and_store_transaction,
        input_selection::{LockFilter, LockedInputPolicy},
        ConfirmationsPolicy,
    },
    CoinbaseFilter, InputSource, WalletRead,
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId, COINBASE_MATURITY_BLOCKS},
    TxId,
};

pub(super) async fn classify_recent_funding<F, Fut>(
    db: &mut SyncDb,
    db_path: &Path,
    network: Network,
    addresses: &[TransparentAddress],
    confirmations: ConfirmationsPolicy,
    mut fetch: F,
) -> Result<usize>
where
    F: FnMut(TxId) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let Some((target, _)) = db
        .get_target_and_anchor_heights(confirmations.trusted())
        .map_err(|e| EngineError::WalletDb(format!("shield funding target: {e}")))?
    else {
        // The proposal retains the existing SyncRequired error.
        return Ok(0);
    };
    let target_height = u32::from(target);
    let eligible = db
        .get_spendable_transparent_outputs_for_addresses(
            addresses,
            target,
            confirmations,
            CoinbaseFilter::AllTransparentOutputs,
            LockFilter::Policy(&LockedInputPolicy::Exclude),
        )
        .map_err(|e| EngineError::WalletDb(format!("shield funding candidates: {e}")))?;
    let recent: Vec<_> = eligible
        .iter()
        .filter(|output| {
            output
                .mined_height()
                .is_some_and(|height| could_be_immature(u32::from(height), target_height))
        })
        .map(|output| TxId::from_bytes(*output.outpoint().hash()))
        .collect();
    if recent.is_empty() {
        return Ok(0);
    }
    let candidates = {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| EngineError::WalletDb(format!("shield funding metadata: {e}")))?;
        classification_candidates(&conn, target_height, &recent, network)
            .map_err(|e| EngineError::WalletDb(format!("shield funding metadata: {e}")))?
    };
    let count = candidates.len();
    for (txid, height) in candidates {
        let raw = if let Some(raw) = retained_transaction(db_path, txid)? {
            raw
        } else {
            fetch(txid).await.map_err(|e| {
            EngineError::Transport(format!(
                "cannot classify recent transparent funding transaction {txid} before shielding: {e}"
            ))
        })?
        };
        let tx = validated_transaction(&raw, network, height, txid)?;
        decrypt_and_store_transaction(&network, db, &tx, Some(height)).map_err(|e| {
            EngineError::WalletDb(format!("store transparent funding transaction {txid}: {e}"))
        })?;
    }
    Ok(count)
}

// Unlike shielding selection, a balance read also covers locked and below-minconf
// outputs. Classification is local after verified raw retrieval; unknown recent
// funding must not be reported as available while a fetch is pending or fails.
const BALANCE_CLASSIFICATION_BATCH: usize = 256;
const BALANCE_INSPECTION_ROWS: usize = 4096;
const BALANCE_INSPECTION_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RETAINED_TRANSACTION_BYTES: u64 = 2 * 1024 * 1024;

fn balance_candidates(
    conn: &rusqlite::Connection,
    target: u32,
    network: Network,
) -> rusqlite::Result<Vec<(TxId, BlockHeight)>> {
    let expiry = zcash_primitives::transaction::builder::DEFAULT_TX_EXPIRY_DELTA;
    let sql = format!("SELECT DISTINCT t.txid,t.mined_height,length(t.raw)
        FROM transactions t JOIN transparent_received_outputs u ON u.transaction_id=t.id_tx
        WHERE t.tx_index IS NULL
          AND t.mined_height < :target AND t.mined_height > :floor
          AND NOT EXISTS (
              SELECT 1 FROM transparent_received_output_spends s
              JOIN transactions st ON st.id_tx=s.transaction_id
              WHERE s.transparent_received_output_id=u.id
                AND (st.mined_height < :target OR st.expiry_height=0 OR st.expiry_height >= :target
                     OR (st.expiry_height IS NULL AND COALESCE(st.target_height,st.min_observed_height)+{expiry} >= :target))
          ) ORDER BY t.mined_height,t.txid LIMIT :inspected_limit");
    let mut stmt = conn.prepare(&sql)?;
    let mut inspected = 0usize;
    let mut raw_bytes = 0u64;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":target":target, ":floor":target.saturating_sub(COINBASE_MATURITY_BLOCKS),
            ":inspected_limit": BALANCE_INSPECTION_ROWS + 1,
        },
        |r| {
            inspected += 1;
            let length: Option<u64> = r.get(2)?;
            let length = length.unwrap_or(0);
            raw_bytes = raw_bytes
                .checked_add(length)
                .ok_or(rusqlite::Error::InvalidQuery)?;
            if inspected > BALANCE_INSPECTION_ROWS
                || length > MAX_RETAINED_TRANSACTION_BYTES
                || raw_bytes > BALANCE_INSPECTION_BYTES
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let id: Vec<u8> = r.get(0)?;
            let bytes: [u8; 32] = id.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok((
                TxId::from_bytes(bytes),
                BlockHeight::from_u32(r.get(1)?),
                conn.query_row(
                    "SELECT raw FROM transactions WHERE txid=?1",
                    [bytes.as_slice()],
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                )?,
            ))
        },
    )?;
    let mut candidates = Vec::new();
    for row in rows {
        let (txid, height, raw) = row?;
        if !needs_classification(raw.as_deref(), network, height, txid)? {
            continue;
        }
        candidates.push((txid, height));
        if candidates.len() > BALANCE_CLASSIFICATION_BATCH {
            break;
        }
    }
    Ok(candidates)
}

fn retained_balance_candidates(
    db_path: &Path,
    target: u32,
    network: Network,
) -> Result<Vec<(TxId, BlockHeight)>> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| {
                EngineError::WalletDb("transparent funding metadata unavailable".into())
            })?;
    balance_candidates(&conn, target, network)
        .map_err(|_| EngineError::WalletDb("transparent funding metadata invalid".into()))
}

pub(super) fn require_balance_classified(
    db_path: &Path,
    target: u32,
    network: Network,
) -> Result<()> {
    if !retained_balance_candidates(db_path, target, network)?.is_empty() {
        return Err(EngineError::SyncRequired);
    }
    Ok(())
}

pub(super) async fn classify_balance_funding<F, Fut>(
    db: &mut SyncDb,
    db_path: &Path,
    network: Network,
    mut fetch: F,
) -> Result<usize>
where
    F: FnMut(TxId) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let Some(tip) = db
        .chain_height()
        .map_err(|_| EngineError::WalletDb("transparent funding height unavailable".into()))?
    else {
        return Ok(0);
    };
    let target = u32::from(tip).saturating_add(1);
    let candidates = retained_balance_candidates(db_path, target, network)?;
    let count = candidates.len().min(BALANCE_CLASSIFICATION_BATCH);
    for (txid, height) in candidates.into_iter().take(BALANCE_CLASSIFICATION_BATCH) {
        let raw = if let Some(raw) = retained_transaction(db_path, txid)? {
            raw
        } else {
            fetch(txid).await?
        };
        let tx = validated_transaction(&raw, network, height, txid)?;
        decrypt_and_store_transaction(&network, db, &tx, Some(height)).map_err(|_| {
            EngineError::WalletDb("transparent funding classification failed".into())
        })?;
    }
    require_balance_classified(db_path, target, network)?;
    Ok(count)
}

fn retained_transaction(db_path: &Path, txid: TxId) -> Result<Option<Vec<u8>>> {
    rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|conn| {
            conn.query_row(
                "SELECT length(raw),raw FROM transactions WHERE txid=?1",
                [txid.as_ref().as_slice()],
                |row| {
                    if row.get::<_, Option<u64>>(0)?.unwrap_or(0) > MAX_RETAINED_TRANSACTION_BYTES {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    row.get(1)
                },
            )
        })
        .map_err(|_| EngineError::WalletDb("transparent funding metadata unavailable".into()))
}

fn needs_classification(
    raw: Option<&[u8]>,
    network: Network,
    height: BlockHeight,
    txid: TxId,
) -> rusqlite::Result<bool> {
    let Some(raw) = raw else {
        return Ok(true);
    };
    let tx = validated_transaction(raw, network, height, txid)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(tx
        .transparent_bundle()
        .is_some_and(|bundle| bundle.is_coinbase()))
}

fn could_be_immature(height: u32, target: u32) -> bool {
    height < target && target - height < COINBASE_MATURITY_BLOCKS
}

fn classification_candidates(
    conn: &rusqlite::Connection,
    target: u32,
    eligible: &[TxId],
    network: Network,
) -> rusqlite::Result<Vec<(TxId, BlockHeight)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mined_height, length(raw), raw FROM transactions
         WHERE txid = ?1 AND tx_index IS NULL
           AND mined_height < ?2 AND mined_height > ?3",
    )?;
    let floor = target.saturating_sub(COINBASE_MATURITY_BLOCKS);
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut inspected = 0usize;
    let mut raw_bytes = 0u64;
    for &txid in eligible {
        if !seen.insert(txid) {
            continue;
        }
        inspected += 1;
        if inspected > BALANCE_INSPECTION_ROWS {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let retained: Option<(u32, Option<Vec<u8>>)> = stmt
            .query_row(
                rusqlite::params![txid.as_ref().as_slice(), target, floor],
                |row| {
                    let length = row.get::<_, Option<u64>>(1)?.unwrap_or(0);
                    raw_bytes = raw_bytes
                        .checked_add(length)
                        .ok_or(rusqlite::Error::InvalidQuery)?;
                    if length > MAX_RETAINED_TRANSACTION_BYTES
                        || raw_bytes > BALANCE_INSPECTION_BYTES
                    {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    Ok((row.get(0)?, row.get(2)?))
                },
            )
            .optional()?;
        if let Some((height, raw)) = retained {
            let height = BlockHeight::from_u32(height);
            if needs_classification(raw.as_deref(), network, height, txid)? {
                candidates.push((txid, height));
            }
        }
    }
    Ok(candidates)
}

fn validated_transaction(
    raw: &[u8],
    network: Network,
    height: BlockHeight,
    expected: TxId,
) -> Result<Transaction> {
    let mut remaining = raw;
    let tx =
        Transaction::read(&mut remaining, BranchId::for_height(&network, height)).map_err(|e| {
            EngineError::Transport(format!("invalid funding transaction {expected}: {e}"))
        })?;
    if !remaining.is_empty() || tx.txid() != expected {
        return Err(EngineError::Transport(format!(
            "funding transaction bytes do not match requested transaction {expected}"
        )));
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_candidates_include_locked_and_below_minconf_but_skip_live_spends() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE transactions(id_tx INTEGER PRIMARY KEY,txid BLOB,mined_height INTEGER,tx_index INTEGER,raw BLOB,expiry_height INTEGER,target_height INTEGER,min_observed_height INTEGER);
            CREATE TABLE transparent_received_outputs(id INTEGER PRIMARY KEY,transaction_id INTEGER,lock_expiry_height INTEGER);
            CREATE TABLE transparent_received_output_spends(transparent_received_output_id INTEGER,transaction_id INTEGER);").unwrap();
        for (id, height, index, raw) in [
            (1u8, 101, None, None),
            (2, 199, None, None),
            (3, 100, None, None),
            (4, 199, Some(0), None),
            (5, 199, Some(1), Some(vec![1u8])),
            (6, 199, None, None),
            (7, 199, None, None),
        ] {
            conn.execute(
                "INSERT INTO transactions VALUES(?1,?2,?3,?4,?5,NULL,NULL,1)",
                rusqlite::params![id, vec![id; 32], height, index, raw],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO transparent_received_outputs VALUES(?1,?1,999)",
                [id],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO transactions VALUES(8,zeroblob(32),199,NULL,NULL,0,NULL,1);
            INSERT INTO transparent_received_output_spends VALUES(6,8);
            INSERT INTO transactions VALUES(9,zeroblob(32),NULL,NULL,NULL,198,NULL,1);
            INSERT INTO transparent_received_output_spends VALUES(7,9);",
        )
        .unwrap();
        let found = balance_candidates(&conn, 200, Network::Regtest).unwrap();
        assert_eq!(
            found,
            [
                (TxId::from_bytes([1; 32]), 101.into()),
                (TxId::from_bytes([2; 32]), 199.into()),
                (TxId::from_bytes([7; 32]), 199.into())
            ]
        );
    }

    #[test]
    fn balance_inspection_exhaustion_refuses_instead_of_skipping_later_rows() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE transactions(id_tx INTEGER PRIMARY KEY,txid BLOB,mined_height INTEGER,tx_index INTEGER,raw BLOB,expiry_height INTEGER,target_height INTEGER,min_observed_height INTEGER);
            CREATE TABLE transparent_received_outputs(id INTEGER PRIMARY KEY,transaction_id INTEGER,lock_expiry_height INTEGER);
            CREATE TABLE transparent_received_output_spends(transparent_received_output_id INTEGER,transaction_id INTEGER);").unwrap();
        let insertion = conn.transaction().unwrap();
        let mut eligible = Vec::new();
        for id in 0..=BALANCE_INSPECTION_ROWS {
            let mut raw = coinbase_bytes();
            raw[5] = 1; // Real parseable noncoinbase with a legitimately unknown ordinal.
            let end = raw.len();
            raw[end - 4..].copy_from_slice(&(id as u32).to_le_bytes());
            let tx = Transaction::read(&raw[..], BranchId::Sprout).unwrap();
            eligible.push(tx.txid());
            insertion
                .execute(
                    "INSERT INTO transactions VALUES(?1,?2,199,NULL,?3,NULL,NULL,1)",
                    rusqlite::params![id, tx.txid().as_ref().as_slice(), raw],
                )
                .unwrap();
            insertion
                .execute(
                    "INSERT INTO transparent_received_outputs VALUES(?1,?1,NULL)",
                    [id],
                )
                .unwrap();
        }
        insertion.commit().unwrap();
        assert!(balance_candidates(&conn, 200, Network::Regtest).is_err());
        assert!(classification_candidates(&conn, 200, &eligible, Network::Regtest).is_err());
        let repeated = vec![eligible[0]; BALANCE_INSPECTION_ROWS + 2];
        assert!(
            classification_candidates(&conn, 200, &repeated, Network::Regtest)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_recent_unknown_eligible_funding_is_fetched_once() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE transactions (txid BLOB PRIMARY KEY, mined_height INTEGER, tx_index INTEGER, raw BLOB)").unwrap();
        for (id, height, index, raw) in [
            (1u8, Some(101), None, None),             // 99 confirmations: classify
            (2, Some(100), None, None),               // exactly mature: no fetch
            (3, Some(99), None, None),                // old: no fetch
            (4, Some(199), Some(0), None),            // known coinbase
            (5, Some(199), Some(1), None),            // known ordinary
            (6, Some(199), Some(1), Some(vec![1u8])), // fully enhanced ordinary
            (7, Some(200), None, None),               // not mined before target
            (8, None, None, None),                    // unmined
            (9, Some(199), None, None),               // not an eligible unspent output
        ] {
            conn.execute(
                "INSERT INTO transactions VALUES (?1,?2,?3,?4)",
                rusqlite::params![vec![id; 32], height, index, raw],
            )
            .unwrap();
        }
        let mut eligible: Vec<_> = (1..=8).map(|id| TxId::from_bytes([id; 32])).collect();
        eligible.push(eligible[0]);
        let result = classification_candidates(&conn, 200, &eligible, Network::Regtest).unwrap();
        assert_eq!(result, [(eligible[0], 101.into())]);
        assert!(could_be_immature(101, 200));
        assert!(!could_be_immature(100, 200));
        assert!(!could_be_immature(201, 200));
        // A known ordinal needs no further classification. Retained raw with a
        // NULL ordinal is covered by the actual transaction fixtures below.
        conn.execute(
            "UPDATE transactions SET tx_index = 0 WHERE txid = ?1",
            [eligible[0].as_ref().as_slice()],
        )
        .unwrap();
        assert!(
            classification_candidates(&conn, 200, &eligible, Network::Regtest)
                .unwrap()
                .is_empty()
        );
    }

    fn coinbase_bytes() -> Vec<u8> {
        coinbase_bytes_for(&transparent::address::Script(zcash_script::script::Code(
            vec![0x51],
        )))
    }

    fn coinbase_bytes_for(script: &transparent::address::Script) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend(1u32.to_le_bytes()); // legacy transparent transaction
        raw.push(1); // one input
        raw.extend([0; 32]);
        raw.extend(u32::MAX.to_le_bytes()); // coinbase outpoint
        raw.extend([2, 1, 1]); // scriptSig pushes height1
        raw.extend(u32::MAX.to_le_bytes());
        raw.push(1); // one output
        raw.extend(50_000u64.to_le_bytes());
        script.write(&mut raw).unwrap();
        raw.extend(0u32.to_le_bytes());
        raw
    }

    #[test]
    fn actual_raw_transaction_is_required_and_txid_is_checked() {
        let raw = coinbase_bytes();
        let tx = Transaction::read(&raw[..], BranchId::Sprout).unwrap();
        let validated =
            validated_transaction(&raw, Network::Regtest, 101.into(), tx.txid()).unwrap();
        assert!(validated.transparent_bundle().unwrap().is_coinbase());
        assert!(validated_transaction(
            &raw,
            Network::Regtest,
            101.into(),
            TxId::from_bytes([9; 32])
        )
        .is_err());
        assert!(validated_transaction(&[], Network::Regtest, 101.into(), tx.txid()).is_err());
        let mut trailing = raw;
        trailing.push(0);
        assert!(validated_transaction(&trailing, Network::Regtest, 101.into(), tx.txid()).is_err());
    }

    #[tokio::test]
    async fn coinbase_classification_is_durable_and_failed_fetch_retries() {
        use zcash_client_backend::{
            data_api::{chain::ChainState, AccountBirthday, WalletCommitmentTrees, WalletWrite},
            wallet::WalletTransparentOutput,
        };
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("wallet.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        rusqlite::vtab::array::load_module(&conn).unwrap();
        let mut db = zcash_client_sqlite::WalletDb::from_connection(
            conn,
            Network::Regtest,
            zcash_client_sqlite::util::SystemClock,
            rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        );
        let seed = secrecy::SecretVec::new(vec![7; 32]);
        zcash_client_sqlite::wallet::init::init_wallet_db(
            &mut db,
            Some(secrecy::SecretVec::new(vec![7u8; 32])),
        )
        .unwrap();
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(0.into(), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );
        let (account, _) = db.create_account("test", &seed, &birthday, None).unwrap();
        db.update_chain_tip(199.into()).unwrap();
        db.with_orchard_tree_mut(|tree| tree.checkpoint(199.into()))
            .unwrap();
        let addresses: Vec<_> = db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .into_keys()
            .collect();
        let raw = coinbase_bytes_for(&addresses[0].script().into());
        let tx = Transaction::read(&raw[..], BranchId::Sprout).unwrap();
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            tx.transparent_bundle().unwrap().vout[0].clone(),
            Some(101.into()),
            Some(account),
            None,
            None,
        )
        .unwrap();
        db.put_received_transparent_utxo(&output).unwrap();
        let confirmations = crate::confirmations_policy(Network::Regtest);
        assert!(matches!(
            require_balance_classified(&path, 200, Network::Regtest),
            Err(EngineError::SyncRequired)
        ));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE transparent_received_outputs SET lock_expiry_height=250",
            [],
        )
        .unwrap();
        assert!(db
            .get_spendable_transparent_outputs_for_addresses(
                &addresses,
                200.into(),
                confirmations,
                CoinbaseFilter::AllTransparentOutputs,
                LockFilter::Policy(&LockedInputPolicy::Exclude)
            )
            .unwrap()
            .is_empty());
        assert!(classify_balance_funding(
            &mut db,
            &path,
            Network::Regtest,
            |_| std::future::ready(Err(EngineError::Transport("unavailable".into())))
        )
        .await
        .is_err());
        assert!(matches!(
            require_balance_classified(&path, 200, Network::Regtest),
            Err(EngineError::SyncRequired)
        ));
        let mut classified = 0;
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |requested| {
                assert_eq!(requested, tx.txid());
                classified += 1;
                std::future::ready(Ok(raw.clone()))
            })
            .await
            .unwrap(),
            1
        );
        assert_eq!(classified, 1);
        require_balance_classified(&path, 200, Network::Regtest).unwrap();
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |_| std::future::ready(
                Err(EngineError::Transport("unexpected refetch".into()))
            ))
            .await
            .unwrap(),
            0
        );
        // Reorg relinking can retain validated raw while dropping only the mined
        // ordinal. Treat that cold state as unresolved until local coinbase
        // classification restores index zero, without any network fetch.
        conn.execute(
            "UPDATE transactions SET mined_height=199,tx_index=NULL WHERE txid=?1",
            [tx.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert!(matches!(
            require_balance_classified(&path, 200, Network::Regtest),
            Err(EngineError::SyncRequired)
        ));
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |_| {
                std::future::ready(Err(EngineError::Transport(
                    "unexpected retained refetch".into(),
                )))
            })
            .await
            .unwrap(),
            1
        );
        require_balance_classified(&path, 200, Network::Regtest).unwrap();
        let cold = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let restored: (u32, u32, Vec<u8>) = cold
            .query_row(
                "SELECT mined_height,tx_index,raw FROM transactions WHERE txid=?1",
                [tx.txid().as_ref().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(restored == (199, 0, raw.clone()));
        assert!(db
            .get_spendable_transparent_outputs_for_addresses(
                &addresses,
                200.into(),
                confirmations,
                CoinbaseFilter::AllTransparentOutputs,
                LockFilter::Policy(&LockedInputPolicy::Exclude)
            )
            .unwrap()
            .is_empty());
        conn.execute(
            "UPDATE transactions SET mined_height=101 WHERE txid=?1",
            [tx.txid().as_ref().as_slice()],
        )
        .unwrap();
        // Restore an unresolved output for the independent shielding retry checks.
        conn.execute(
            "UPDATE transactions SET tx_index=NULL,raw=NULL WHERE txid=?1",
            [tx.txid().as_ref().as_slice()],
        )
        .unwrap();
        conn.execute(
            "UPDATE transparent_received_outputs SET lock_expiry_height=NULL",
            [],
        )
        .unwrap();
        let failure = classify_recent_funding(
            &mut db,
            &path,
            Network::Regtest,
            &addresses,
            confirmations,
            |_| std::future::ready(Err(EngineError::Transport("unavailable".into()))),
        )
        .await;
        assert!(failure.is_err());
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            classification_candidates(&conn, 200, &[tx.txid()], Network::Regtest)
                .unwrap()
                .len(),
            1
        );
        let mut fetched = 0;
        let count = classify_recent_funding(
            &mut db,
            &path,
            Network::Regtest,
            &addresses,
            confirmations,
            |requested| {
                assert_eq!(requested, tx.txid());
                fetched += 1;
                std::future::ready(Ok(raw.clone()))
            },
        )
        .await
        .unwrap();
        assert_eq!((count, fetched), (1, 1));
        let stored: (i64, usize) = conn
            .query_row(
                "SELECT tx_index, length(raw) FROM transactions WHERE txid = ?1",
                [tx.txid().as_ref().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (0, raw.len()));
        assert!(
            db.get_spendable_transparent_outputs_for_addresses(
                &addresses,
                200.into(),
                confirmations,
                CoinbaseFilter::AllTransparentOutputs,
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .unwrap()
            .is_empty(),
            "verified coinbase must now be excluded as immature"
        );
        let repeat = classify_recent_funding(
            &mut db,
            &path,
            Network::Regtest,
            &addresses,
            confirmations,
            |_| {
                std::future::ready(Err(EngineError::Message(
                    "unexpected repeated fetch".into(),
                )))
            },
        )
        .await
        .unwrap();
        assert_eq!(repeat, 0);
        let mut regular_raw = raw.clone();
        regular_raw[5] = 1; // This fixture spends a non-null outpoint, not coinbase.
        let regular = Transaction::read(&regular_raw[..], BranchId::Sprout).unwrap();
        assert!(!regular.transparent_bundle().unwrap().is_coinbase());
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*regular.txid().as_ref(), 0),
            regular.transparent_bundle().unwrap().vout[0].clone(),
            Some(199.into()),
            Some(account),
            None,
            None,
        )
        .unwrap();
        db.put_received_transparent_utxo(&output).unwrap();
        assert!(matches!(
            require_balance_classified(&path, 200, Network::Regtest),
            Err(EngineError::SyncRequired)
        ));
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |requested| {
                assert_eq!(requested, regular.txid());
                std::future::ready(Ok(regular_raw.clone()))
            })
            .await
            .unwrap(),
            1
        );
        let stored: (Option<u32>, Vec<u8>) = conn
            .query_row(
                "SELECT tx_index,raw FROM transactions WHERE txid=?1",
                [regular.txid().as_ref().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(stored.0.is_none());
        assert!(stored.1 == regular_raw);
        require_balance_classified(&path, 200, Network::Regtest).unwrap();
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |_| {
                std::future::ready(Err(EngineError::Transport(
                    "unexpected ordinary refetch".into(),
                )))
            })
            .await
            .unwrap(),
            0
        );
        assert!(
            classification_candidates(&conn, 200, &[regular.txid()], Network::Regtest)
                .unwrap()
                .is_empty()
        );
        conn.execute(
            "UPDATE transactions SET raw=zeroblob(?1) WHERE txid=?2",
            rusqlite::params![
                MAX_RETAINED_TRANSACTION_BYTES + 1,
                regular.txid().as_ref().as_slice()
            ],
        )
        .unwrap();
        assert!(require_balance_classified(&path, 200, Network::Regtest).is_err());
        conn.execute(
            "UPDATE transactions SET raw=x'00' WHERE txid=?1",
            [regular.txid().as_ref().as_slice()],
        )
        .unwrap();
        assert!(require_balance_classified(&path, 200, Network::Regtest).is_err());
        assert!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |_| {
                std::future::ready(Err(EngineError::Transport(
                    "invalid local raw must not refetch".into(),
                )))
            })
            .await
            .is_err()
        );
        conn.execute(
            "UPDATE transactions SET raw=?1 WHERE txid=?2",
            rusqlite::params![regular_raw, regular.txid().as_ref().as_slice()],
        )
        .unwrap();
        require_balance_classified(&path, 200, Network::Regtest).unwrap();
    }
}
