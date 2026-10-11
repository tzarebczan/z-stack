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
        classification_candidates(&conn, target_height, &recent)
            .map_err(|e| EngineError::WalletDb(format!("shield funding metadata: {e}")))?
    };
    let count = candidates.len();
    for (txid, height) in candidates {
        let raw = fetch(txid).await.map_err(|e| {
            EngineError::Transport(format!(
                "cannot classify recent transparent funding transaction {txid} before shielding: {e}"
            ))
        })?;
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

fn balance_candidates(
    conn: &rusqlite::Connection,
    target: u32,
) -> rusqlite::Result<Vec<(TxId, BlockHeight)>> {
    let expiry = zcash_primitives::transaction::builder::DEFAULT_TX_EXPIRY_DELTA;
    let sql = format!("SELECT DISTINCT t.txid,t.mined_height
        FROM transactions t JOIN transparent_received_outputs u ON u.transaction_id=t.id_tx
        WHERE t.tx_index IS NULL AND t.raw IS NULL
          AND t.mined_height < :target AND t.mined_height > :floor
          AND NOT EXISTS (
              SELECT 1 FROM transparent_received_output_spends s
              JOIN transactions st ON st.id_tx=s.transaction_id
              WHERE s.transparent_received_output_id=u.id
                AND (st.mined_height < :target OR st.expiry_height=0 OR st.expiry_height >= :target
                     OR (st.expiry_height IS NULL AND COALESCE(st.target_height,st.min_observed_height)+{expiry} >= :target))
          ) ORDER BY t.mined_height,t.txid LIMIT :limit");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":target":target, ":floor":target.saturating_sub(COINBASE_MATURITY_BLOCKS),
            ":limit": BALANCE_CLASSIFICATION_BATCH + 1,
        },
        |r| {
            let id: Vec<u8> = r.get(0)?;
            let bytes: [u8; 32] = id.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok((TxId::from_bytes(bytes), BlockHeight::from_u32(r.get(1)?)))
        },
    )?;
    rows.collect()
}

fn retained_balance_candidates(db_path: &Path, target: u32) -> Result<Vec<(TxId, BlockHeight)>> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| {
                EngineError::WalletDb("transparent funding metadata unavailable".into())
            })?;
    balance_candidates(&conn, target)
        .map_err(|_| EngineError::WalletDb("transparent funding metadata invalid".into()))
}

pub(super) fn require_balance_classified(db_path: &Path, target: u32) -> Result<()> {
    if !retained_balance_candidates(db_path, target)?.is_empty() {
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
    let candidates = retained_balance_candidates(db_path, target)?;
    let count = candidates.len().min(BALANCE_CLASSIFICATION_BATCH);
    for (txid, height) in candidates.into_iter().take(BALANCE_CLASSIFICATION_BATCH) {
        let raw = fetch(txid).await?;
        let tx = validated_transaction(&raw, network, height, txid)?;
        decrypt_and_store_transaction(&network, db, &tx, Some(height)).map_err(|_| {
            EngineError::WalletDb("transparent funding classification failed".into())
        })?;
    }
    require_balance_classified(db_path, target)?;
    Ok(count)
}

fn could_be_immature(height: u32, target: u32) -> bool {
    height < target && target - height < COINBASE_MATURITY_BLOCKS
}

fn classification_candidates(
    conn: &rusqlite::Connection,
    target: u32,
    eligible: &[TxId],
) -> rusqlite::Result<Vec<(TxId, BlockHeight)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mined_height FROM transactions
         WHERE txid = ?1 AND tx_index IS NULL AND raw IS NULL
           AND mined_height < ?2 AND mined_height > ?3",
    )?;
    let floor = target.saturating_sub(COINBASE_MATURITY_BLOCKS);
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for &txid in eligible {
        if !seen.insert(txid) {
            continue;
        }
        let height: Option<u32> = stmt
            .query_row(
                rusqlite::params![txid.as_ref().as_slice(), target, floor],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(height) = height {
            candidates.push((txid, BlockHeight::from_u32(height)));
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
            (5, 199, None, Some(vec![1u8])),
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
        let found = balance_candidates(&conn, 200).unwrap();
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
    fn only_recent_unknown_eligible_funding_is_fetched_once() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE transactions (txid BLOB PRIMARY KEY, mined_height INTEGER, tx_index INTEGER, raw BLOB)").unwrap();
        for (id, height, index, raw) in [
            (1u8, Some(101), None, None),          // 99 confirmations: classify
            (2, Some(100), None, None),            // exactly mature: no fetch
            (3, Some(99), None, None),             // old: no fetch
            (4, Some(199), Some(0), None),         // known coinbase
            (5, Some(199), Some(1), None),         // known ordinary
            (6, Some(199), None, Some(vec![1u8])), // fully enhanced ordinary
            (7, Some(200), None, None),            // not mined before target
            (8, None, None, None),                 // unmined
            (9, Some(199), None, None),            // not an eligible unspent output
        ] {
            conn.execute(
                "INSERT INTO transactions VALUES (?1,?2,?3,?4)",
                rusqlite::params![vec![id; 32], height, index, raw],
            )
            .unwrap();
        }
        let mut eligible: Vec<_> = (1..=8).map(|id| TxId::from_bytes([id; 32])).collect();
        eligible.push(eligible[0]);
        let result = classification_candidates(&conn, 200, &eligible).unwrap();
        assert_eq!(result, [(eligible[0], 101.into())]);
        assert!(could_be_immature(101, 200));
        assert!(!could_be_immature(100, 200));
        assert!(!could_be_immature(201, 200));
        // Successful non-coinbase enhancement stores raw but legitimately leaves
        // tx_index NULL. That completion must still prevent another fetch.
        conn.execute(
            "UPDATE transactions SET raw = X'01' WHERE txid = ?1",
            [eligible[0].as_ref().as_slice()],
        )
        .unwrap();
        assert!(classification_candidates(&conn, 200, &eligible)
            .unwrap()
            .is_empty());
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
            require_balance_classified(&path, 200),
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
            require_balance_classified(&path, 200),
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
        require_balance_classified(&path, 200).unwrap();
        assert_eq!(
            classify_balance_funding(&mut db, &path, Network::Regtest, |_| std::future::ready(
                Err(EngineError::Transport("unexpected refetch".into()))
            ))
            .await
            .unwrap(),
            0
        );
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
            classification_candidates(&conn, 200, &[tx.txid()])
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
            require_balance_classified(&path, 200),
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
        require_balance_classified(&path, 200).unwrap();
    }
}
