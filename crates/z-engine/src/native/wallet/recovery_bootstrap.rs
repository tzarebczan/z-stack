//! Regtest-only birthday-one setup from independently accepted genesis.
use super::*;
use transparent_wallet::ChainView;
use zcash_primitives::block::BlockHash;

impl NativeWallet {
    /// Refuse to reconstruct a wallet that has retained recovery or payment facts.
    /// A seed-only directory or the unpublished initial Pending database can resume.
    pub fn validate_regtest_offline_setup_resume(root: impl Into<PathBuf>) -> Result<()> {
        let paths = WalletPaths::new(root);
        if paths.data_db.exists() {
            let _lease = database_lease::shared(&paths.data_db)?;
            require_initial_recovery_setup(&paths.data_db)?;
        }
        Ok(())
    }

    pub fn create_regtest_offline(
        root: impl Into<PathBuf>,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
    ) -> Result<(Self, CreatedWallet)> {
        let root = root.into();
        let paths = WalletPaths::new(&root);
        if paths.meta_path.exists() || paths.reset_backup().exists() {
            return Err(EngineError::AlreadyExists(paths.root.display().to_string()));
        }
        auth.validate_for_save()?;
        let store = SeedStore::new(&root);
        let mnemonic = if store.exists() {
            Self::validate_regtest_offline_setup_resume(&root)?;
            let retained = store.load(auth.passphrase.as_deref(), auth.windows_credential)?;
            Mnemonic::parse_normalized(retained.expose_secret())
                .map_err(|_| EngineError::Message("native_recovery_mnemonic_invalid".into()))?
        } else {
            let mut entropy = [0u8; 32];
            UnwrapErr(SysRng)
                .try_fill_bytes(&mut entropy)
                .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
            let mnemonic = Mnemonic::from_entropy(&entropy)
                .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
            entropy.zeroize();
            mnemonic
        };
        let (wallet, address) =
            Self::initialize_regtest_offline(root, &mnemonic, chain, auth, account_index, |_| {
                Ok(())
            })?;
        let ufvk = wallet
            .meta
            .ufvk
            .clone()
            .ok_or_else(|| EngineError::Message("native_recovery_setup_failed".into()))?;
        Ok((
            wallet,
            CreatedWallet {
                mnemonic: mnemonic.to_string(),
                birthday_height: 1,
                unified_address: address,
                ufvk,
            },
        ))
    }

    pub fn restore_regtest_offline(
        root: impl Into<PathBuf>,
        mnemonic: &str,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
    ) -> Result<(Self, String)> {
        let mnemonic = Mnemonic::parse_normalized(mnemonic.trim())
            .map_err(|_| EngineError::Message("native_recovery_mnemonic_invalid".into()))?;
        Self::initialize_regtest_offline(root, &mnemonic, chain, auth, account_index, |_| Ok(()))
    }

    fn initialize_regtest_offline(
        root: impl Into<PathBuf>,
        mnemonic: &Mnemonic,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
        before_publish: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<(Self, String)> {
        auth.validate_for_save()?;
        let zip_account = Zip32AccountId::try_from(account_index)
            .map_err(|_| EngineError::Message("native_recovery_account_invalid".into()))?;
        let mut genesis = crate::web::from_hex(
            &chain
                .hash_at(0)
                .ok_or_else(|| EngineError::Message("native_recovery_chain_invalid".into()))?,
        )
        .map_err(|_| EngineError::Message("native_recovery_chain_invalid".into()))?;
        genesis.reverse();
        // RegtestAcceptedChain's constructor enforces the pinned genesis and
        // activation schedule. Empty frontiers are authentic only at genesis.
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                0.into(),
                BlockHash::from_slice(&genesis),
            ),
            Some(chain.target_height().into()),
        );
        let paths = WalletPaths::new(root);
        let store = SeedStore::new(&paths.root);
        let retained_seed = store.exists();
        if paths.data_db.exists() && !retained_seed {
            return Err(EngineError::Message(
                "native_recovery_setup_resume_refused".into(),
            ));
        }
        if retained_seed {
            let retained = store.load(auth.passphrase.as_deref(), auth.windows_credential)?;
            if retained.expose_secret() != &mnemonic.to_string() {
                return Err(EngineError::AlreadyExists(paths.root.display().to_string()));
            }
        }
        paths.prepare_new_wallet_for(NewWalletProfile::VerifiedRegtestRecovery)?;
        paths.ensure_dirs()?;
        if !retained_seed {
            store.save(
                &mnemonic.to_string(),
                auth.passphrase.as_deref(),
                auth.windows_credential,
            )?;
        }
        let seed = SecretVec::new(mnemonic.to_seed("").to_vec());
        let mut db = open_wallet_db(&paths.data_db, ZNetwork::Regtest)?;
        init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().clone())))
            .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
        let (_, usk) = db
            .import_account_hd("primary", &seed, zip_account, &birthday, Some("z-stack"))
            .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
        let ufvk = usk.to_unified_full_viewing_key();
        drop(db);
        let address = ufvk
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?
            .0
            .encode(&ZNetwork::Regtest);
        let server = LightServer::LocalRegtest;
        let meta = WalletMeta {
            network: "regtest".into(),
            server: server.as_url(),
            birthday_height: 1,
            account_index,
            validator_rpc: None,
            ufvk: Some(ufvk.encode(&ZNetwork::Regtest)),
            view_only: false,
            unlock_policy: auth.unlock_policy,
            os_unlock: auth.windows_credential,
            allow_deep_sync: false,
        };
        let wallet = Self {
            paths,
            network_access: false,
            network: ZNetwork::Regtest,
            server,
            meta,
        };
        wallet.begin_regtest_pir_recovery(chain, &RecoveryCancellation::new())?;
        Self::checkpoint_sqlite(&wallet.paths.data_db)?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&wallet.paths.data_db)?
            .sync_all()?;
        #[cfg(unix)]
        std::fs::File::open(&wallet.paths.root)?.sync_all()?;
        before_publish(&wallet)?;
        write_meta(&wallet.paths.meta_path, &wallet.meta)?;
        Ok((wallet, address))
    }
}

#[cfg(test)]
mod tests {
    use super::super::recovery_blocks::tests::fixture_with_script;
    use super::*;
    #[tokio::test]
    async fn offline_create_restore_nonzero_account_encrypt_and_cold_reopen_without_network() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let auth = || SeedAuth::passphrase("fixture encrypted offline bootstrap passphrase");
        let (mut created, details) =
            NativeWallet::create_regtest_offline(source.path(), &chain, auth(), 2).unwrap();
        let (restored, address) = NativeWallet::restore_regtest_offline(
            target.path(),
            &details.mnemonic,
            &chain,
            auth(),
            2,
        )
        .unwrap();
        assert_eq!(address, details.unified_address);
        assert_eq!(
            created.viewing_key().unwrap(),
            restored.viewing_key().unwrap()
        );
        assert_eq!(
            restored.account_index(),
            Zip32AccountId::try_from(2).unwrap()
        );
        assert_eq!(restored.birthday_height(), 1);
        assert!(created.load_seed(&auth()).is_ok());
        assert!(restored.load_seed(&auth()).is_ok());
        let encrypted = std::fs::read(source.path().join("seed.enc")).unwrap();
        assert!(!encrypted
            .windows(details.mnemonic.len())
            .any(|w| w == details.mnemonic.as_bytes()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        created
            .set_light_server(LightServer::parse(
                &format!("http://{}", listener.local_addr().unwrap()),
                ZNetwork::Regtest,
            ))
            .unwrap();
        let reopened = NativeWallet::open_offline(source.path()).unwrap();
        assert!(reopened.load_seed(&auth()).is_ok());
        assert!(reopened.connect().await.is_err());
        assert!(restored.connect().await.is_err());
        assert!(reopened.ensure_recovery_selection_ready().is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn incomplete_seed_setup_cannot_overwrite_authentication_material() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let words = crate::keys::REGTEST_FAUCET_MNEMONIC;
        let password = "fixture crash setup encrypted passphrase";
        SeedStore::new(dir.path())
            .save(words, Some(password), false)
            .unwrap();
        let before = std::fs::read(dir.path().join("seed.enc")).unwrap();
        let (resumed, details) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(details.mnemonic == words);
        assert_eq!(std::fs::read(dir.path().join("seed.enc")).unwrap(), before);
        assert!(SeedStore::new(dir.path())
            .load(Some(password), false)
            .is_ok());
        assert!(resumed.load_seed(&SeedAuth::passphrase(password)).is_ok());
        assert_eq!(std::fs::read(dir.path().join("seed.enc")).unwrap(), before);
    }

    #[tokio::test]
    async fn interrupted_bootstrap_persists_pending_before_metadata_and_resumes_the_same_seed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let words = crate::keys::REGTEST_FAUCET_MNEMONIC;
        let password = "fixture pending publication encrypted passphrase";
        let mnemonic = Mnemonic::parse_normalized(words).unwrap();
        let failure = NativeWallet::initialize_regtest_offline(
            dir.path(),
            &mnemonic,
            &chain,
            SeedAuth::passphrase(password),
            0,
            |wallet| {
                assert!(!wallet.paths.meta_path.exists());
                let conn = rusqlite::Connection::open_with_flags(
                    &wallet.paths.data_db,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let pending: (bool, u64, bool) = conn
                    .query_row(
                        "SELECT blocked,generation,receipt IS NULL FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
                assert_eq!(pending, (true, 1, true));
                Err(EngineError::Message(
                    "fixture_publication_interrupted".into(),
                ))
            },
        );
        assert!(
            matches!(failure, Err(EngineError::Message(message)) if message == "fixture_publication_interrupted")
        );
        assert!(!dir.path().join("wallet.json").exists());
        assert!(NativeWallet::open(dir.path()).is_err());
        let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
        let ordinary = NativeWallet::create(
            dir.path(),
            ZNetwork::Regtest,
            None,
            Some(1),
            SeedAuth::passphrase(password),
            0,
        )
        .await;
        assert!(
            matches!(ordinary, Err(EngineError::Message(message)) if message == "native_recovery_profile_required")
        );
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        let (resumed, recovered) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(recovered.mnemonic == words);
        assert!(resumed.paths.meta_path.exists());
        assert!(resumed.ensure_recovery_selection_ready().is_err());
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        assert!(NativeWallet::open_offline(dir.path())
            .unwrap()
            .ensure_recovery_selection_ready()
            .is_err());
    }

    #[test]
    fn metadata_loss_cannot_rebootstrap_completed_or_intent_bearing_pending_wallets() {
        let (_, chain) = fixture_with_script(&[0x51]);
        let words = crate::keys::REGTEST_FAUCET_MNEMONIC;
        let password = "fixture metadata loss encrypted passphrase";
        for completed in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (wallet, _) = NativeWallet::restore_regtest_offline(
                dir.path(),
                words,
                &chain,
                SeedAuth::passphrase(password),
                0,
            )
            .unwrap();
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            if completed {
                conn.execute("UPDATE ext_coffer_pir_recovery_v1 SET blocked=0,generation=2,receipt=x'00' WHERE id=1", []).unwrap();
            } else {
                conn.execute_batch("CREATE TABLE ext_native_payments_v1 (
                    id TEXT PRIMARY KEY, proposal BLOB NOT NULL, fee INTEGER NOT NULL,
                    created INTEGER NOT NULL, expires INTEGER NOT NULL, height INTEGER NOT NULL,
                    block_hash BLOB NOT NULL, phase TEXT NOT NULL, txids TEXT NOT NULL);
                    INSERT INTO ext_native_payments_v1 VALUES ('fixture-intent',x'00',10000,1,121,1,zeroblob(32),'signed','[]');").unwrap();
            }
            drop(conn);
            NativeWallet::checkpoint_sqlite(&wallet.paths.data_db).unwrap();
            std::fs::remove_file(&wallet.paths.meta_path).unwrap();
            let read_guard = rusqlite::Connection::open_with_flags(
                &wallet.paths.data_db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            read_guard
                .query_row(
                    "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap();
            let database = std::fs::read(&wallet.paths.data_db).unwrap();
            let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
            let files = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>();
            assert!(
                NativeWallet::validate_regtest_offline_setup_resume(dir.path())
                    .unwrap_err()
                    .to_string()
                    .contains("native_recovery_setup_resume_refused")
            );
            let resumed = NativeWallet::restore_regtest_offline(
                dir.path(),
                words,
                &chain,
                SeedAuth::passphrase(password),
                0,
            );
            assert!(
                matches!(resumed, Err(EngineError::Message(message)) if message == "native_recovery_setup_resume_refused")
            );
            assert!(std::fs::read(&wallet.paths.data_db).unwrap() == database);
            assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
            assert_eq!(
                std::fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<std::collections::BTreeSet<_>>(),
                files
            );
        }
    }

    #[test]
    fn interrupted_generated_create_returns_the_original_phrase_without_replacing_seed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let password = "fixture interrupted generated creation passphrase";
        let (wallet, created) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        std::fs::remove_file(&wallet.paths.meta_path).unwrap();
        let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
        let wrong = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase("wrong password for interrupted create"),
            0,
        );
        assert!(wrong.is_err());
        assert!(!wallet.paths.meta_path.exists());
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        let (resumed, recovered) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(recovered.mnemonic == created.mnemonic);
        assert!(recovered.unified_address == created.unified_address);
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        assert!(resumed.ensure_recovery_selection_ready().is_err());
    }

    #[test]
    fn interrupted_pending_database_without_its_encrypted_seed_refuses_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let password = "fixture interrupted missing seed passphrase";
        let (wallet, _) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        std::fs::remove_file(&wallet.paths.meta_path).unwrap();
        std::fs::remove_file(dir.path().join("seed.enc")).unwrap();
        NativeWallet::checkpoint_sqlite(&wallet.paths.data_db).unwrap();
        let database = std::fs::read(&wallet.paths.data_db).unwrap();
        let replacement = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        );
        assert!(
            matches!(replacement, Err(EngineError::Message(message)) if message == "native_recovery_setup_resume_refused")
        );
        assert!(std::fs::read(&wallet.paths.data_db).unwrap() == database);
        assert!(!wallet.paths.meta_path.exists());
        assert!(!dir.path().join("seed.enc").exists());
    }
}
