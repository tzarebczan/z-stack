//! Persisted recovery state must survive feature/profile downgrade safely.
use super::*;
pub(super) fn transaction_recovery_ready<D: WalletRead<AccountId = AccountUuid>>(
    db: &D,
    ext: &zcash_client_sqlite::ExtensionTransaction<'_>,
) -> anyhow::Result<()> {
    let exists:bool=ext.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_coffer_pir_recovery_v1')",[],|r|r.get(0))?;
    if !exists {
        return Ok(());
    }
    #[cfg(feature = "native-pir")]
    return super::pir::ensure_transaction_receipt(db, ext);
    #[cfg(not(feature = "native-pir"))]
    {
        let _ = db;
        anyhow::bail!("native_recovery_profile_required");
    }
}
impl NativeWallet {
    pub(super) fn recovery_generation(&self) -> Result<Option<u64>> {
        (|| -> anyhow::Result<Option<u64>> {
            let conn=rusqlite::Connection::open_with_flags(&self.paths.data_db,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_coffer_pir_recovery_v1')",[],|r|r.get(0))?;
            if !exists { return Ok(None); }
            Ok(Some(conn.query_row("SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",[],|r|r.get(0))?))
        })().map_err(|_|EngineError::Message("native_recovery_storage_failed".into()))
    }
    pub(super) fn ensure_recovery_selection_ready(&self) -> Result<()> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| EngineError::Message("native_recovery_storage_failed".into()))?;
        let marker:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_coffer_pir_recovery_v1')",[],|row|row.get(0)).map_err(|_|EngineError::Message("native_recovery_storage_failed".into()))?;
        if !marker {
            return Ok(());
        }
        #[cfg(feature = "native-pir")]
        return self.ensure_pir_recovery_ready();
        #[cfg(not(feature = "native-pir"))]
        Err(EngineError::Message(
            "native_recovery_profile_required".into(),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::wallet::tests::{fixture_account, fixture_wallet};
    #[tokio::test]
    async fn offline_open_refuses_stored_endpoint_without_a_socket_connection() {
        let dir = tempfile::tempdir().unwrap();
        let mut wallet = fixture_wallet(dir.path());
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        wallet
            .set_light_server(LightServer::parse(
                &format!("http://{}", listener.local_addr().unwrap()),
                ZNetwork::Regtest,
            ))
            .unwrap();
        wallet
            .set_validator_rpc(Some(format!("http://{}", listener.local_addr().unwrap())))
            .unwrap();
        let offline = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(offline.validator_rpc_url().is_none());
        for result in [
            offline.connect().await.map(|_| ()),
            offline.rewind_scan_to_gap().await.map(|_| ()),
            offline.reset_scan().await,
            offline.submit_raw(&mut None, vec![]).await,
        ] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("native_offline_wallet"));
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // Endpoint edits cannot restore this instance's transport capability.
        let mut offline = offline;
        offline.set_light_server(LightServer::LocalRegtest).unwrap();
        assert!(offline.connect().await.is_err());
    }
    #[test]
    fn persisted_pending_blocks_default_or_recovery_capable_engine() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        assert!(wallet.ensure_recovery_selection_ready().is_ok());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute_batch("CREATE TABLE ext_coffer_pir_recovery_v1(id INTEGER PRIMARY KEY,blocked INTEGER NOT NULL,generation INTEGER NOT NULL,receipt BLOB); INSERT INTO ext_coffer_pir_recovery_v1 VALUES(1,1,1,NULL);").unwrap();
        assert!(wallet.ensure_recovery_selection_ready().is_err());
        assert!(NativeWallet::open(dir.path())
            .unwrap()
            .ensure_recovery_selection_ready()
            .is_err());
        assert!(wallet.prepare_payment("unused", 1, None).is_err());
        assert!(wallet
            .sign_payment(&SeedAuth::mnemonic_once("unused"), "unused")
            .is_err());
    }
}
