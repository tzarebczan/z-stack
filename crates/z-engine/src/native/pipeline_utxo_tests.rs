fn utxo_benchmark_wallet() -> (
    tempfile::TempDir,
    super::super::wallet::SyncDb,
    Vec<zcash_client_backend::wallet::WalletTransparentOutput<zcash_client_sqlite::AccountUuid>>,
) {
    use zcash_client_backend::{data_api::AccountBirthday, wallet::WalletTransparentOutput};
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("wallet.sqlite")).unwrap();
    rusqlite::vtab::array::load_module(&conn).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    let mut db = zcash_client_sqlite::WalletDb::from_connection(
        conn,
        ZNetwork::Regtest,
        zcash_client_sqlite::util::SystemClock,
        rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    );
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
    let (account, _) = db
        .create_account("UTXO benchmark", &seed, &birthday, None)
        .unwrap();
    db.update_chain_tip(10_100.into()).unwrap();
    let addresses = db.get_transparent_receivers(account, true, true).unwrap();
    let address = *addresses.keys().min().unwrap();
    let outputs = (1u32..=1_000)
        .map(|id| {
            let mut txid = [0; 32];
            txid[..4].copy_from_slice(&id.to_le_bytes());
            WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new(txid, 0),
                transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(150_000).unwrap(),
                    address.script().into(),
                ),
                Some(id.into()),
                Some(account),
                None,
                None,
            )
            .unwrap()
        })
        .collect();
    (dir, db, outputs)
}

#[test]
#[ignore = "manual disk SQLite UTXO import and repeated-refresh comparison"]
fn benchmark_native_utxo_refresh() {
    use zcash_client_sqlite::error::SqliteClientError;
    let variants = ["per-output", "transaction-batches", "checked-key-batches"];
    let mut fixtures = variants.map(|_| utxo_benchmark_wallet());
    // Independent identical workloads and rotating order avoid measuring only
    // the first variant's import or assigning every warm-cache run to the last.
    for pass in 0..4 {
        for offset in 0..variants.len() {
            let index = (pass + offset) % variants.len();
            let variant = variants[index];
            let (_dir, db, outputs) = &mut fixtures[index];
            let started = std::time::Instant::now();
            match variant {
                "per-output" => {
                    for output in &*outputs {
                        db.put_received_transparent_utxo(output).unwrap();
                    }
                }
                "transaction-batches" => {
                    for chunk in outputs.chunks(256) {
                        db.transactionally(|db| {
                            for output in chunk {
                                db.put_received_transparent_utxo(output)?;
                            }
                            Ok::<_, SqliteClientError>(())
                        })
                        .unwrap();
                    }
                }
                _ => super::super::transparent_refresh::refresh_transparent_outputs(db, outputs)
                    .unwrap(),
            }
            eprintln!(
                "native UTXO {variant} {}: {} outputs, {}us; profile={}",
                if pass == 0 {
                    "initial import".to_string()
                } else {
                    format!("refresh{pass}")
                },
                outputs.len(),
                started.elapsed().as_micros(),
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
            );
        }
    }
}
