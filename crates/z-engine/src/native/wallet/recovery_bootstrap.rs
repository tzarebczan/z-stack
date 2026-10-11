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
        let _ownership = database_lease::bootstrap(&root)?;
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
        let root = root.into();
        let _ownership = database_lease::bootstrap(&root)?;
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
        Self::initialize_regtest_offline_before_enrollment(
            root,
            mnemonic,
            chain,
            auth,
            account_index,
            (|_| Ok(()), before_publish),
        )
    }

    fn initialize_regtest_offline_before_enrollment(
        root: impl Into<PathBuf>,
        mnemonic: &Mnemonic,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
        (before_enrollment, before_publish): (
            impl FnOnce(&Self) -> Result<()>,
            impl FnOnce(&Self) -> Result<()>,
        ),
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
        let mut paths = WalletPaths::new(root);
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
        // Keep every pre-Pending database outside the wallet directory. A crash here
        // leaves a seed-only setup, never a markerless live database that resembles
        // an ordinary wallet whose metadata was lost.
        let destination = paths.data_db.clone();
        let mut existing_lease = None;
        let staging = if destination.exists() {
            existing_lease = Some(database_lease::exclusive(&destination)?);
            if paths.meta_path.exists() || paths.reset_backup().exists() {
                return Err(EngineError::AlreadyExists(paths.root.display().to_string()));
            }
            require_initial_recovery_setup(&destination)?;
            None
        } else {
            let parent = paths
                .root
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let staging = tempfile::Builder::new()
                .prefix(".z-engine-bootstrap-")
                .tempdir_in(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700))?;
            }
            paths.data_db = staging.path().join("data.sqlite");
            Some(staging)
        };
        let seed = SecretVec::new(mnemonic.to_seed("").to_vec());
        let mut db = open_wallet_db_with_durability(&paths.data_db, ZNetwork::Regtest, true)?;
        let ufvk = if staging.is_some() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&paths.data_db, std::fs::Permissions::from_mode(0o600))?;
            }
            init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().clone())))
                .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
            let (_, usk) = db
                .import_account_hd("primary", &seed, zip_account, &birthday, Some("z-stack"))
                .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
            usk.to_unified_full_viewing_key()
        } else {
            let refused = || EngineError::Message("native_recovery_setup_resume_refused".into());
            let expected = UnifiedSpendingKey::from_seed(
                &ZNetwork::Regtest,
                seed.expose_secret(),
                zip_account,
            )
            .map_err(|_| refused())?
            .to_unified_full_viewing_key();
            let accounts = db.get_account_ids().map_err(|_| refused())?;
            if accounts.len() != 1
                || db
                    .get_account_birthday(accounts[0])
                    .map_err(|_| refused())?
                    != BlockHeight::from_u32(1)
            {
                return Err(refused());
            }
            if !db
                .validate_seed(accounts[0], &seed)
                .map_err(|_| refused())?
            {
                return Err(refused());
            }
            let account = db
                .get_account(accounts[0])
                .map_err(|_| refused())?
                .ok_or_else(refused)?;
            if account.ufvk().map(|key| key.encode(&ZNetwork::Regtest))
                != Some(expected.encode(&ZNetwork::Regtest))
            {
                return Err(refused());
            }
            expected
        };
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
        let mut wallet = Self {
            paths,
            network_access: false,
            network: ZNetwork::Regtest,
            server,
            meta,
        };
        before_enrollment(&wallet)?;
        if staging.is_some() {
            wallet.begin_regtest_pir_recovery(chain, &RecoveryCancellation::new())?;
        }
        Self::checkpoint_sqlite(&wallet.paths.data_db)?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&wallet.paths.data_db)?
            .sync_all()?;
        // All SQLite handles are closed and its WAL is checkpointed. A hard link
        // publishes the complete file atomically and refuses an existing destination.
        // The private staging directory is on the same filesystem as the wallet.
        if let Some(staging) = &staging {
            #[cfg(unix)]
            std::fs::File::open(staging.path())?.sync_all()?;
            std::fs::hard_link(&wallet.paths.data_db, &destination)?;
        }
        #[cfg(unix)]
        std::fs::File::open(&wallet.paths.root)?.sync_all()?;
        wallet.paths.data_db = destination;
        drop(staging);
        before_publish(&wallet)?;
        write_meta(&wallet.paths.meta_path, &wallet.meta)?;
        drop(existing_lease);
        Ok((wallet, address))
    }
}

#[cfg(test)]
mod tests {
    use super::super::recovery_blocks::tests::fixture_with_script;
    use super::*;
    #[tokio::test]
    async fn ordinary_create_ownership_fences_canonical_setup_and_preserves_its_generated_seed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wallet");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = LightServer::parse(
            &format!("http://{}", listener.local_addr().unwrap()),
            ZNetwork::Regtest,
        );
        let password = "fixture cross API bootstrap password";
        let (_, chain) = fixture_with_script(&[0x51]);
        let encrypted;
        {
            // A real ordinary public create saves its generated encrypted seed,
            // then holds ownership while its first network request is stalled.
            let ordinary = NativeWallet::create(
                &root,
                ZNetwork::Regtest,
                Some(server),
                Some(1),
                SeedAuth::passphrase(password),
                0,
            );
            tokio::pin!(ordinary);
            let accepting = async {
                loop {
                    match listener.accept() {
                        Ok((socket, _)) => return socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(Duration::from_millis(5)).await
                        }
                        Err(_) => panic!("fixture canary accept failed"),
                    }
                }
            };
            let socket = tokio::select! {
                _ = &mut ordinary => panic!("ordinary create must remain pending"),
                accepted = tokio::time::timeout(Duration::from_secs(5), accepting) => accepted.unwrap(),
            };
            encrypted = std::fs::read(root.join("seed.enc")).unwrap();
            assert!(!root.join("data.sqlite").exists());
            assert!(!root.join("wallet.json").exists());
            for refused in [
                NativeWallet::create_regtest_offline(
                    &root,
                    &chain,
                    SeedAuth::passphrase("fixture competing password"),
                    0,
                )
                .err()
                .unwrap(),
                NativeWallet::restore_regtest_offline(
                    &root,
                    crate::keys::REGTEST_FAUCET_MNEMONIC,
                    &chain,
                    SeedAuth::passphrase("fixture competing password"),
                    0,
                )
                .err()
                .unwrap(),
            ] {
                assert!(
                    matches!(refused, EngineError::Message(message) if message == "native_pir_database_busy")
                );
            }
            assert!(std::fs::read(root.join("seed.enc")).unwrap() == encrypted);
            drop(socket);
            // Dropping the actual in-flight public future releases setup ownership.
        }
        let retained = SeedStore::new(&root).load(Some(password), false).unwrap();
        let (wallet, created) =
            NativeWallet::create_regtest_offline(&root, &chain, SeedAuth::passphrase(password), 0)
                .unwrap();
        assert!(retained.expose_secret() == &created.mnemonic);
        assert!(std::fs::read(root.join("seed.enc")).unwrap() == encrypted);
        drop(wallet);
        let cold = NativeWallet::open_offline(&root).unwrap();
        let seed = cold.load_seed(&SeedAuth::passphrase(password)).unwrap();
        let db = cold.open_recovery_db().unwrap();
        let accounts = db.get_account_ids().unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(db.validate_seed(accounts[0], &seed).unwrap());
        assert!(cold.viewing_key().unwrap() == created.ufvk);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn concurrent_public_creates_publish_one_account_matching_its_retained_encrypted_seed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("missing/parent/wallet");
        let (_, chain) = fixture_with_script(&[0x51]);
        let start = Arc::new(std::sync::Barrier::new(2));
        let passwords = [
            "fixture concurrent first password",
            "fixture concurrent second password",
        ];
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (index, password) in passwords.iter().enumerate() {
                let start = start.clone();
                let root = root.clone();
                let chain = chain.clone();
                handles.push(scope.spawn(move || {
                    start.wait();
                    (
                        index,
                        NativeWallet::create_regtest_offline(
                            root,
                            &chain,
                            SeedAuth::passphrase(*password),
                            0,
                        ),
                    )
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        for (_, result) in &results {
            if let Err(error) = result {
                assert!(
                    matches!(error, EngineError::AlreadyExists(_))
                        || error.to_string() == "native_pir_database_busy"
                );
            }
        }
        let (winner, (wallet, created)) = results
            .into_iter()
            .find_map(|(index, result)| result.ok().map(|value| (index, value)))
            .unwrap();
        let retained = SeedStore::new(&root)
            .load(Some(passwords[winner]), false)
            .unwrap();
        assert!(retained.expose_secret() == &created.mnemonic);
        assert!(SeedStore::new(&root)
            .load(Some(passwords[1 - winner]), false)
            .is_err());
        let encrypted = std::fs::read(root.join("seed.enc")).unwrap();
        let database = std::fs::read(root.join("data.sqlite")).unwrap();
        drop(wallet);
        let cold = NativeWallet::open_offline(&root).unwrap();
        let seed = cold
            .load_seed(&SeedAuth::passphrase(passwords[winner]))
            .unwrap();
        let db = cold.open_recovery_db().unwrap();
        let accounts = db.get_account_ids().unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(db.validate_seed(accounts[0], &seed).unwrap());
        assert!(cold.viewing_key().unwrap() == created.ufvk);
        drop(db);
        let rejected = NativeWallet::create_regtest_offline(
            &root,
            &chain,
            SeedAuth::passphrase(passwords[1 - winner]),
            0,
        );
        assert!(matches!(rejected, Err(EngineError::AlreadyExists(_))));
        assert!(std::fs::read(root.join("seed.enc")).unwrap() == encrypted);
        assert!(std::fs::read(root.join("data.sqlite")).unwrap() == database);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_ownership_fences_public_create_and_restore_through_parent_aliases() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("real");
        std::fs::create_dir(&parent).unwrap();
        let alias = dir.path().join("alias");
        symlink(&parent, &alias).unwrap();
        let root = parent.join("wallet");
        let alias_root = alias.join("wallet");
        let (_, chain) = fixture_with_script(&[0x51]);
        let ownership = database_lease::bootstrap(&root).unwrap();
        assert!(!root.exists());
        let refused = NativeWallet::create_regtest_offline(
            &alias_root,
            &chain,
            SeedAuth::passphrase("fixture alias password"),
            0,
        );
        assert!(
            matches!(refused, Err(EngineError::Message(message)) if message == "native_pir_database_busy")
        );
        let refused = NativeWallet::restore_regtest_offline(
            &alias_root,
            crate::keys::REGTEST_FAUCET_MNEMONIC,
            &chain,
            SeedAuth::passphrase("fixture alias password"),
            0,
        );
        assert!(
            matches!(refused, Err(EngineError::Message(message)) if message == "native_pir_database_busy")
        );
        assert!(!root.exists());
        let directory = parent.join(".z-engine-bootstrap-locks");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        let lock = std::fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            lock.metadata().unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(ownership);
        let (wallet, _) = NativeWallet::create_regtest_offline(
            &alias_root,
            &chain,
            SeedAuth::passphrase("fixture alias password"),
            0,
        )
        .unwrap();
        assert!(wallet.paths.meta_path.exists());
        let ownership = database_lease::bootstrap(&root).unwrap();
        let root_alias = dir.path().join("wallet-alias");
        symlink(&root, &root_alias).unwrap();
        let refused = NativeWallet::create_regtest_offline(
            root_alias,
            &chain,
            SeedAuth::passphrase("fixture alias password"),
            0,
        );
        assert!(
            matches!(refused, Err(EngineError::Message(message)) if message == "native_pir_database_busy")
        );
        drop(ownership);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_metadata_and_retained_temp_publish_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let auth = || SeedAuth::passphrase("fixture private bootstrap metadata passphrase");
        let (wallet, created) =
            NativeWallet::create_regtest_offline(dir.path(), &chain, auth(), 0).unwrap();
        assert_eq!(
            std::fs::metadata(&wallet.paths.meta_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let seed = std::fs::read(dir.path().join("seed.enc")).unwrap();
        std::fs::remove_file(&wallet.paths.meta_path).unwrap();
        let tmp = dir.path().join("wallet.json.tmp");
        std::fs::write(&tmp, b"interrupted fixture metadata").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (_, resumed) =
            NativeWallet::create_regtest_offline(dir.path(), &chain, auth(), 0).unwrap();
        assert!(resumed.mnemonic == created.mnemonic);
        assert_eq!(resumed.unified_address, created.unified_address);
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == seed);
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::metadata(&wallet.paths.meta_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let cold = NativeWallet::open_offline(dir.path()).unwrap();
        assert!(cold.ensure_recovery_selection_ready().is_err());
        assert!(cold.viewing_key().unwrap() == wallet.viewing_key().unwrap());
    }

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
    fn interrupted_account_import_stays_seed_only_and_public_create_reuses_authenticated_seed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let words = crate::keys::REGTEST_FAUCET_MNEMONIC;
        let mnemonic = Mnemonic::parse_normalized(words).unwrap();
        let password = "fixture interrupted account import passphrase";
        let failed = NativeWallet::initialize_regtest_offline_before_enrollment(
            dir.path(),
            &mnemonic,
            &chain,
            SeedAuth::passphrase(password),
            0,
            (
                |wallet| {
                    assert!(!dir.path().join("data.sqlite").exists());
                    assert!(!wallet.paths.meta_path.exists());
                    assert!(!has_recovery_marker(&wallet.paths.data_db).unwrap());
                    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
                    let accounts: usize = conn
                        .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))
                        .unwrap();
                    assert_eq!(accounts, 1);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        assert_eq!(
                            std::fs::metadata(&wallet.paths.data_db)
                                .unwrap()
                                .permissions()
                                .mode()
                                & 0o777,
                            0o600
                        );
                        assert_eq!(
                            std::fs::metadata(wallet.paths.data_db.parent().unwrap())
                                .unwrap()
                                .permissions()
                                .mode()
                                & 0o777,
                            0o700
                        );
                    }
                    Err(EngineError::Message("fixture_import_interrupted".into()))
                },
                |_| Ok(()),
            ),
        );
        assert!(
            matches!(failed, Err(EngineError::Message(message)) if message == "fixture_import_interrupted")
        );
        assert!(!dir.path().join("data.sqlite").exists());
        assert!(!dir.path().join("wallet.json").exists());
        let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
        assert!(NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase("wrong interrupted password"),
            0
        )
        .is_err());
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        let (wallet, recovered) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(recovered.mnemonic == words);
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        assert!(wallet.ensure_recovery_selection_ready().is_err());
        assert!(NativeWallet::open_offline(dir.path())
            .unwrap()
            .ensure_recovery_selection_ready()
            .is_err());
    }

    #[test]
    fn bootstrap_publication_never_overwrites_a_markerless_imported_database() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let mnemonic = Mnemonic::parse_normalized(crate::keys::REGTEST_FAUCET_MNEMONIC).unwrap();
        let password = "fixture no clobber account import passphrase";
        let mut retained = Vec::new();
        let failed = NativeWallet::initialize_regtest_offline_before_enrollment(
            dir.path(),
            &mnemonic,
            &chain,
            SeedAuth::passphrase(password),
            0,
            (
                |wallet| {
                    NativeWallet::checkpoint_sqlite(&wallet.paths.data_db).unwrap();
                    std::fs::copy(&wallet.paths.data_db, dir.path().join("data.sqlite")).unwrap();
                    retained = std::fs::read(dir.path().join("data.sqlite")).unwrap();
                    Ok(())
                },
                |_| Ok(()),
            ),
        );
        assert!(failed.is_err());
        assert!(!dir.path().join("wallet.json").exists());
        assert!(std::fs::read(dir.path().join("data.sqlite")).unwrap() == retained);
        let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
        let retried = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        );
        assert!(
            matches!(retried, Err(EngineError::Message(message)) if message == "native_recovery_setup_resume_refused")
        );
        assert!(std::fs::read(dir.path().join("data.sqlite")).unwrap() == retained);
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
    }

    #[test]
    fn repeated_interrupted_pending_resume_retains_the_live_database_and_same_seed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, chain) = fixture_with_script(&[0x51]);
        let password = "fixture repeated interrupted pending passphrase";
        let (wallet, created) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        std::fs::remove_file(&wallet.paths.meta_path).unwrap();
        let encrypted = std::fs::read(dir.path().join("seed.enc")).unwrap();
        let database = std::fs::read(&wallet.paths.data_db).unwrap();
        let mismatched_account = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            1,
        );
        assert!(
            matches!(mismatched_account, Err(EngineError::Message(message)) if message == "native_recovery_setup_resume_refused")
        );
        assert!(std::fs::read(&wallet.paths.data_db).unwrap() == database);
        let mnemonic = Mnemonic::parse_normalized(&created.mnemonic).unwrap();
        for _ in 0..2 {
            let interrupted = NativeWallet::initialize_regtest_offline(
                dir.path(),
                &mnemonic,
                &chain,
                SeedAuth::passphrase(password),
                0,
                |_| Err(EngineError::Message("fixture_retry_interrupted".into())),
            );
            assert!(
                matches!(interrupted, Err(EngineError::Message(message)) if message == "fixture_retry_interrupted")
            );
            assert!(wallet.paths.data_db.exists());
            assert!(!wallet.paths.meta_path.exists());
            assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
            assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("incomplete")));
            NativeWallet::validate_regtest_offline_setup_resume(dir.path()).unwrap();
            let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
            let generation: u64 = conn
                .query_row(
                    "SELECT generation FROM ext_coffer_pir_recovery_v1 WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(generation, 1);
        }
        let (resumed, recovered) = NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(recovered.mnemonic == created.mnemonic);
        assert!(std::fs::read(dir.path().join("seed.enc")).unwrap() == encrypted);
        assert!(resumed.ensure_recovery_selection_ready().is_err());
        assert!(NativeWallet::open_offline(dir.path())
            .unwrap()
            .ensure_recovery_selection_ready()
            .is_err());
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
            drop(database_lease::shared(&wallet.paths.data_db).unwrap());
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
