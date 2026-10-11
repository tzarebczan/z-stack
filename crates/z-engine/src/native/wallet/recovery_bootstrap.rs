//! Regtest-only birthday-one setup from independently accepted genesis.
use super::*;
use transparent_wallet::ChainView;
use zcash_primitives::block::BlockHash;

impl NativeWallet {
    pub fn create_regtest_offline(
        root: impl Into<PathBuf>,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
    ) -> Result<(Self, CreatedWallet)> {
        let mut entropy = [0u8; 32];
        UnwrapErr(SysRng)
            .try_fill_bytes(&mut entropy)
            .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
        let mnemonic = Mnemonic::from_entropy(&entropy)
            .map_err(|_| EngineError::Message("native_recovery_setup_failed".into()))?;
        entropy.zeroize();
        let (wallet, address) =
            Self::initialize_regtest_offline(root, &mnemonic, chain, auth, account_index)?;
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
        Self::initialize_regtest_offline(root, &mnemonic, chain, auth, account_index)
    }

    fn initialize_regtest_offline(
        root: impl Into<PathBuf>,
        mnemonic: &Mnemonic,
        chain: &RegtestAcceptedChain,
        auth: SeedAuth,
        account_index: u32,
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
        if retained_seed {
            let retained = store.load(auth.passphrase.as_deref(), auth.windows_credential)?;
            if retained.expose_secret() != &mnemonic.to_string() {
                return Err(EngineError::AlreadyExists(paths.root.display().to_string()));
            }
        }
        paths.prepare_new_wallet()?;
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
        write_meta(&paths.meta_path, &meta)?;
        let wallet = Self {
            paths,
            network_access: false,
            network: ZNetwork::Regtest,
            server,
            meta,
        };
        wallet.begin_regtest_pir_recovery(chain, &RecoveryCancellation::new())?;
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
        assert!(NativeWallet::create_regtest_offline(
            dir.path(),
            &chain,
            SeedAuth::passphrase(password),
            0
        )
        .is_err());
        assert_eq!(std::fs::read(dir.path().join("seed.enc")).unwrap(), before);
        assert!(SeedStore::new(dir.path())
            .load(Some(password), false)
            .is_ok());
        let (resumed, _) = NativeWallet::restore_regtest_offline(
            dir.path(),
            words,
            &chain,
            SeedAuth::passphrase(password),
            0,
        )
        .unwrap();
        assert!(resumed.load_seed(&SeedAuth::passphrase(password)).is_ok());
        assert_eq!(std::fs::read(dir.path().join("seed.enc")).unwrap(), before);
    }
}
