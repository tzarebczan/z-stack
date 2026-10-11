//! Persisted recovery state must survive feature/profile downgrade safely.
use super::*;
impl NativeWallet {
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
