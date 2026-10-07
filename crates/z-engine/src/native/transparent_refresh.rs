//! Bounded UTXO refresh without repeated strict gap-key checks.
//!
//! Pinned zakura-client-sqlite reconciles every output (including spends, block
//! linkage, observation/exposure heights and address discovery) in its low-level
//! put. WalletWrite then generates the same address gap a second time with
//! `require_key=true`. Account keys cannot change inside this transaction, so
//! retain the normal operation for each actual account/scope's first output and
//! reuse that key check for subsequent outputs. The low-level reconciliation and
//! gap maintenance still run for EVERY output, even an unchanged existing row.

use std::collections::HashSet;

use super::wallet::SyncDb;
use zcash_client_backend::{
    data_api::{
        ll::{LowLevelWalletRead, LowLevelWalletWrite},
        WalletRead, WalletWrite,
    },
    wallet::WalletTransparentOutput,
};
use zcash_client_sqlite::{error::SqliteClientError, AccountUuid};

pub(super) const UTXO_REFRESH_BATCH: usize = 256;

pub(super) fn refresh_transparent_outputs(
    db: &mut SyncDb,
    outputs: &[WalletTransparentOutput<AccountUuid>],
) -> Result<(), SqliteClientError> {
    for chunk in outputs.chunks(UTXO_REFRESH_BATCH) {
        db.transactionally(|db| {
            let observation_height = db
                .chain_height()?
                .ok_or(SqliteClientError::ChainHeightUnknown)?;
            let mut checked = HashSet::new();
            for output in chunk {
                // Gap discovery can upgrade/reassign imported addresses. Resolve
                // the current database owner rather than trusting the RPC's
                // account hint or retaining an address cache between outputs.
                let owner = db.find_account_for_transparent_address(output.recipient_address())?;
                if owner.is_some_and(|owner| checked.contains(&owner)) {
                    db.put_transparent_output(output, observation_height, true)?;
                } else {
                    db.put_received_transparent_utxo(output)?;
                    if let Some(owner) = owner {
                        checked.insert(owner);
                    }
                }
            }
            Ok::<_, SqliteClientError>(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Network;
    use rusqlite::{types::Value, Connection};
    use std::path::{Path, PathBuf};
    use transparent::{
        address::TransparentAddress,
        bundle::{OutPoint, TxOut},
        keys::TransparentKeyScope,
    };
    use zcash_client_backend::data_api::{
        chain::ChainState,
        wallet::{ConfirmationsPolicy, TargetHeight},
        AccountBirthday, OutputLockStore,
    };
    use zcash_client_backend::wallet::{LockOwner, OutputRef};
    use zcash_protocol::{consensus::BlockHeight, value::Zatoshis, PoolType, TxId};

    fn open(path: &Path) -> SyncDb {
        let conn = Connection::open(path).unwrap();
        rusqlite::vtab::array::load_module(&conn).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        zcash_client_sqlite::WalletDb::from_connection(
            conn,
            Network::Regtest,
            zcash_client_sqlite::util::SystemClock,
            rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        )
    }

    struct Pair {
        _dir: tempfile::TempDir,
        old_path: PathBuf,
        new_path: PathBuf,
        old: SyncDb,
        new: SyncDb,
        accounts: Vec<AccountUuid>,
    }

    impl Pair {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base_path = dir.path().join("base.sqlite");
            let mut db = open(&base_path);
            let seed = secrecy::SecretVec::new(vec![7; 32]);
            zcash_client_sqlite::wallet::init::init_wallet_db(
                &mut db,
                Some(secrecy::SecretVec::new(vec![7; 32])),
            )
            .unwrap();
            let birthday = AccountBirthday::from_parts(
                ChainState::empty(0.into(), zcash_primitives::block::BlockHash([0; 32])),
                None,
            );
            let accounts = (0..2)
                .map(|_| {
                    db.create_account("UTXO fixture", &seed, &birthday, None)
                        .unwrap()
                        .0
                })
                .collect();
            db.update_chain_tip(200.into()).unwrap();
            // Close the last connection before copying, ensuring WAL is checkpointed.
            drop(db);
            let old_path = dir.path().join("old.sqlite");
            let new_path = dir.path().join("new.sqlite");
            std::fs::copy(&base_path, &old_path).unwrap();
            std::fs::copy(&base_path, &new_path).unwrap();
            Self {
                old: open(&old_path),
                new: open(&new_path),
                _dir: dir,
                old_path,
                new_path,
                accounts,
            }
        }

        fn addresses(&self, account: usize, scope: TransparentKeyScope) -> Vec<TransparentAddress> {
            let mut addresses: Vec<_> = self
                .old
                .get_transparent_receivers(self.accounts[account], true, true)
                .unwrap()
                .into_iter()
                .filter(|(_, meta)| meta.scope() == Some(scope))
                .collect();
            addresses.sort_by_key(|(_, meta)| meta.address_index());
            addresses.into_iter().map(|(address, _)| address).collect()
        }

        fn apply(&mut self, outputs: &[WalletTransparentOutput<AccountUuid>]) {
            for output in outputs {
                self.old.put_received_transparent_utxo(output).unwrap();
            }
            refresh_transparent_outputs(&mut self.new, outputs).unwrap();
            self.assert_equal();
        }

        fn mutate_both(&self, sql: &str) {
            for path in [&self.old_path, &self.new_path] {
                Connection::open(path).unwrap().execute_batch(sql).unwrap();
            }
        }

        fn assert_equal(&self) {
            assert_eq!(
                database_state(&self.old_path),
                database_state(&self.new_path),
                "all SQLite metadata must match the original path"
            );
            for &account in &self.accounts {
                let balances = |db: &SyncDb| {
                    db.get_transparent_balances(
                        account,
                        TargetHeight::from(201),
                        ConfirmationsPolicy::default(),
                    )
                    .unwrap()
                    .into_iter()
                    .map(|(address, balance)| (address, format!("{balance:?}")))
                    .collect::<std::collections::BTreeMap<_, _>>()
                };
                assert_eq!(
                    balances(&self.old),
                    balances(&self.new),
                    "public balances must agree"
                );
                assert_eq!(
                    self.old
                        .get_transparent_receivers(account, true, true)
                        .unwrap(),
                    self.new
                        .get_transparent_receivers(account, true, true)
                        .unwrap(),
                    "address discovery and exposure must agree"
                );
                assert_eq!(
                    self.old.get_locked_outputs(account).unwrap(),
                    self.new.get_locked_outputs(account).unwrap()
                );
            }
        }
    }

    fn database_state(path: &Path) -> Vec<(String, Vec<Vec<Value>>)> {
        let conn = Connection::open(path).unwrap();
        let tables: Vec<String> = conn.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
            .query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        tables
            .into_iter()
            .map(|table| {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
                    .unwrap();
                let columns = stmt.column_count();
                let mut rows: Vec<Vec<Value>> = stmt
                    .query_map([], |row| {
                        (0..columns).map(|column| row.get(column)).collect()
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                rows.sort_by_cached_key(|row| format!("{row:?}"));
                (table, rows)
            })
            .collect()
    }

    fn output(
        id: u32,
        address: TransparentAddress,
        account_hint: AccountUuid,
        height: Option<u32>,
        value: u64,
    ) -> WalletTransparentOutput<AccountUuid> {
        let mut txid = [0; 32];
        txid[..4].copy_from_slice(&id.to_le_bytes());
        WalletTransparentOutput::from_parts(
            OutPoint::new(txid, 0),
            TxOut::new(Zatoshis::from_u64(value).unwrap(), address.script().into()),
            height.map(BlockHeight::from_u32),
            Some(account_hint),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn refresh_matches_all_metadata_for_accounts_scopes_changed_pending_spent_and_locked_outputs() {
        let mut pair = Pair::new();
        let external = pair.addresses(0, TransparentKeyScope::EXTERNAL);
        let internal = pair.addresses(0, TransparentKeyScope::INTERNAL);
        let second = pair.addresses(1, TransparentKeyScope::EXTERNAL);
        assert!(!internal.is_empty());
        // Wrong account hints intentionally prove that database ownership wins.
        let account = pair.accounts[1];
        let outputs = [
            output(1, external[0], account, Some(10), 10_000),
            output(2, external[0], account, None, 20_000),
            output(3, *external.last().unwrap(), account, Some(20), 30_000),
            output(4, internal[0], account, Some(20), 40_000),
            output(5, internal[0], account, Some(20), 50_000),
            output(6, second[0], pair.accounts[0], Some(20), 60_000),
            output(7, second[0], pair.accounts[0], Some(20), 70_000),
        ];
        pair.apply(&outputs);
        assert!(
            pair.addresses(0, TransparentKeyScope::EXTERNAL).len() > external.len(),
            "receiving at the last gap address expands discovery"
        );

        let lock = OutputRef::new(
            TxId::from_bytes(*outputs[2].outpoint().hash()),
            PoolType::TRANSPARENT,
            0,
        );
        for db in [&mut pair.old, &mut pair.new] {
            db.lock_outputs(&[lock], LockOwner::new([8; 32]), 500.into())
                .unwrap();
            db.update_chain_tip(250.into()).unwrap();
        }
        // Fixture mutations represent scanned spends, a delayed block linkage,
        // and a reorg that unmined previously stored transactions.
        pair.mutate_both("INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (12, zeroblob(32), 0, X'000000'), (15, zeroblob(32), 0, X'000000');
            INSERT INTO transactions (id_tx, txid, mined_height, min_observed_height) VALUES (100, zeroblob(32), 80, 80);
            INSERT INTO transparent_received_output_spends (transparent_received_output_id, transaction_id) SELECT id, 100 FROM transparent_received_outputs WHERE value_zat=40000;
            INSERT INTO transparent_spend_map (spending_transaction_id, prevout_txid, prevout_output_index) SELECT 100, txid, 0 FROM transactions WHERE id_tx=(SELECT transaction_id FROM transparent_received_outputs WHERE value_zat=50000);
            UPDATE transactions SET mined_height=NULL, block=NULL, confirmed_unmined_at_height=210 WHERE id_tx=(SELECT transaction_id FROM transparent_received_outputs WHERE value_zat=30000);");
        let updated = [
            output(1, external[0], account, None, 11_000), // unknown height must retain mined height
            output(2, external[0], account, Some(15), 20_000), // pending becomes mined
            output(3, *external.last().unwrap(), account, Some(12), 31_000), // lower-height reorg
            output(4, internal[0], account, Some(20), 40_000),
            output(5, internal[0], account, Some(20), 50_000),
            output(6, second[0], pair.accounts[0], Some(20), 60_000),
        ];
        pair.apply(&updated);
        pair.apply(&updated); // Repeated refresh must preserve spend and lock metadata too.
        let conn = Connection::open(&pair.new_path).unwrap();
        assert_eq!(conn.query_row("SELECT max_observed_unspent_height FROM transparent_received_outputs WHERE value_zat=40000", [], |r| r.get::<_, u32>(0)).unwrap(), 79);
        assert_eq!(conn.query_row("SELECT max_observed_unspent_height FROM transparent_received_outputs WHERE value_zat=11000", [], |r| r.get::<_, u32>(0)).unwrap(), 250);
        assert_eq!(conn.query_row("SELECT block FROM transactions WHERE id_tx=(SELECT transaction_id FROM transparent_received_outputs WHERE value_zat=20000)", [], |r| r.get::<_, u32>(0)).unwrap(), 15);
        assert_eq!(
            pair.new.get_locked_outputs(pair.accounts[0]).unwrap(),
            [lock]
        );
        // Run the real library rewind too, then reconcile the same outputs on
        // the replacement chain. No live chain or wallet fixture is involved.
        for db in [&mut pair.old, &mut pair.new] {
            db.truncate_to_chain_state(ChainState::empty(
                5.into(),
                zcash_primitives::block::BlockHash([5; 32]),
            ))
            .unwrap();
            db.update_chain_tip(250.into()).unwrap();
        }
        pair.assert_equal();
        pair.apply(&updated);
    }

    #[test]
    fn failed_refresh_batch_rolls_back_rows_and_address_discovery_then_retries() {
        let mut pair = Pair::new();
        let addresses = pair.addresses(0, TransparentKeyScope::EXTERNAL);
        let account = pair.accounts[0];
        let outputs = [
            output(1, *addresses.last().unwrap(), account, Some(10), 1000),
            output(2, addresses[0], account, Some(10), 777),
        ];
        let conn = Connection::open(&pair.new_path).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_fixture_output BEFORE INSERT ON transparent_received_outputs WHEN NEW.value_zat=777 BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;").unwrap();
        let before = database_state(&pair.new_path);
        assert!(refresh_transparent_outputs(&mut pair.new, &outputs).is_err());
        assert_eq!(before, database_state(&pair.new_path));
        conn.execute_batch("DROP TRIGGER fail_fixture_output")
            .unwrap();
        pair.apply(&outputs);
    }

    #[test]
    fn each_scope_retains_strict_missing_key_failure_without_partial_writes() {
        let mut pair = Pair::new();
        let external = pair.addresses(0, TransparentKeyScope::EXTERNAL)[0];
        let internal = pair.addresses(0, TransparentKeyScope::INTERNAL)[0];
        let account = pair.accounts[0];
        // Model a viewing-only account with cached internal addresses but no full key.
        pair.mutate_both("UPDATE accounts SET account_kind=1, ufvk=NULL, hd_seed_fingerprint=NULL, hd_account_index=NULL");
        let outputs = [
            output(1, external, account, Some(10), 1000),
            output(2, external, account, Some(10), 2000),
            output(3, internal, account, Some(10), 3000),
        ];
        let old_error = pair
            .old
            .transactionally(|db| {
                for output in &outputs {
                    db.put_received_transparent_utxo(output)?;
                }
                Ok::<_, SqliteClientError>(())
            })
            .unwrap_err();
        let before = database_state(&pair.new_path);
        let new_error = refresh_transparent_outputs(&mut pair.new, &outputs).unwrap_err();
        assert_eq!(old_error.to_string(), new_error.to_string());
        assert_eq!(before, database_state(&pair.new_path));
        assert_eq!(
            database_state(&pair.old_path),
            database_state(&pair.new_path)
        );
    }

    #[test]
    fn a_later_failed_chunk_preserves_only_previously_committed_chunks() {
        let mut pair = Pair::new();
        let address = pair.addresses(0, TransparentKeyScope::EXTERNAL)[0];
        let outputs: Vec<_> = (0..UTXO_REFRESH_BATCH + 2)
            .map(|i| {
                output(
                    i as u32 + 1,
                    address,
                    pair.accounts[0],
                    Some(10),
                    if i == UTXO_REFRESH_BATCH + 1 {
                        777
                    } else {
                        1000
                    },
                )
            })
            .collect();
        let conn = Connection::open(&pair.new_path).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_fixture_output BEFORE INSERT ON transparent_received_outputs WHEN NEW.value_zat=777 BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;").unwrap();
        assert!(refresh_transparent_outputs(&mut pair.new, &outputs).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM transparent_received_outputs",
                [],
                |row| row.get::<_, usize>(0)
            )
            .unwrap(),
            UTXO_REFRESH_BATCH
        );
        conn.execute_batch("DROP TRIGGER fail_fixture_output")
            .unwrap();
        pair.apply(&outputs);
    }
}
