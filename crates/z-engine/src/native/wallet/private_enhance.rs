//! Source-bound private enhancement. No selected-transaction transport exists on this path.
use super::{database_lease, NativeWallet};
use crate::{
    enhancement::EnhanceBatch,
    error::{EngineError, Result},
    native::NativeWalletSource,
    Network,
};
use std::{fs::File, path::PathBuf, sync::Arc};
use zcash_client_backend::data_api::enhance_pir::{
    EnhancePirRead, EnhancePirRequest, EnhancePirSnapshotAnchor, EnhancePirSnapshotStatus,
    EnhancePirStoreResult, EnhancePirWork, EnhancePirWrite, EnhancementMode,
    TransactionEnhancementWork,
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::BlockHeight;
fn refused(code: &str) -> EngineError {
    EngineError::Message(code.into())
}

/// Ephemeral pass with retained database ownership. Drop or cancel to retire private contexts.
pub struct NativeEnhancePass {
    root: PathBuf,
    binding: [u8; 32],
    owner: Option<Arc<File>>,
    batch: EnhanceBatch<EnhancePirRequest>,
}
impl NativeEnhancePass {
    pub fn session_ids(&self) -> Vec<String> {
        self.batch.session_ids()
    }
    pub fn add_session(&mut self, bytes: &[u8], now: u64) -> Result<()> {
        if self.owner.is_none() {
            return Err(refused("enhance_context_consumed"));
        }
        self.batch.add_session(bytes, now).map_err(refused)
    }
    /// Public historical chain binding for the closed OHTTP operation envelope.
    pub fn anchor(&self) -> (u64, String) {
        let anchor = self.batch.anchor();
        (anchor.height, crate::web::to_hex(&anchor.hash))
    }
    pub fn cancel(&mut self) {
        self.batch.cancel();
        self.owner.take();
    }
}
impl NativeWallet {
    pub fn begin_private_enhance_with_source(
        &self,
        source: &NativeWalletSource,
        manifest: &[u8],
        now: u64,
    ) -> Result<NativeEnhancePass> {
        let owner = Arc::new(database_lease::shared(&self.paths.data_db)?);
        self.require_wallet_source(source)?;
        if self.network != Network::Mainnet {
            return Err(refused("enhance_network_unsupported"));
        }
        let anchor = z_enhance_pir::manifest_anchor(manifest).map_err(refused)?;
        let db = self
            .open_db()?
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        check_anchor(&db, anchor)?;
        let work = db
            .transaction_enhancement_work()
            .map_err(|_| refused("enhance_database_error"))?
            .into_iter()
            .filter_map(|work| match work {
                TransactionEnhancementWork::Private(EnhancePirWork::Query(request)) => {
                    Some((request, u64::from(request.position())))
                }
                // Rediscovery must use a complete normal public range; it never permits a targeted block or txid fetch.
                _ => None,
            });
        let batch = EnhanceBatch::open(manifest, &anchor, work, now).map_err(refused)?;
        Ok(NativeEnhancePass {
            root: self.paths.root.clone(),
            binding: source.binding(),
            owner: Some(owner),
            batch,
        })
    }
    pub fn prepare_private_enhance_with_source(
        &self,
        source: &NativeWalletSource,
        pass: &mut NativeEnhancePass,
        now: u64,
    ) -> Result<Vec<u8>> {
        self.check_private_enhance_pass(source, pass)?;
        let db = self
            .open_db()?
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        check_anchor(&db, pass.batch.anchor())?;
        pass.batch.prepare(now).map_err(refused)
    }
    pub fn apply_private_enhance_with_source(
        &self,
        source: &NativeWalletSource,
        pass: &mut NativeEnhancePass,
        reply: &[u8],
        now: u64,
    ) -> Result<EnhancePirStoreResult> {
        // Always retire the one-use context before checking mutable wallet or source state.
        let (request, record) = pass.batch.finish(reply, now).map_err(refused)?;
        self.check_private_enhance_pass(source, pass)?;
        let mut db = self
            .open_db()?
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        check_anchor(&db, pass.batch.anchor())?;
        let record = zcash_client_backend::data_api::enhance_pir::EnhanceRecord::from_bytes(record)
            .map_err(|_| refused("enhance_record_invalid"))?;
        // A positive flag is an untrusted indexer assertion, never authority to change this
        // source's durable queue into selected-transaction retrieval.
        if record.has_transparent() {
            return Err(refused("enhance_public_data_required"));
        }
        let result = db
            .apply_ironwood_enhance_record(request, &record)
            .map_err(|_| refused("enhance_database_error"))?;
        if result == EnhancePirStoreResult::LwdRequired {
            return Err(refused("enhance_public_data_required"));
        }
        if result == EnhancePirStoreResult::Rejected {
            return Err(refused("enhance_record_rejected"));
        }
        Ok(result)
    }
    fn check_private_enhance_pass(
        &self,
        source: &NativeWalletSource,
        pass: &NativeEnhancePass,
    ) -> Result<()> {
        if pass.owner.is_none() {
            return Err(refused("enhance_context_consumed"));
        }
        if pass.root != self.paths.root || pass.binding != source.binding() {
            return Err(refused("enhance_wallet_mismatch"));
        }
        self.require_wallet_source(source)
    }
}
fn check_anchor(db: &super::Db, anchor: z_enhance_pir::ChainAnchor) -> Result<()> {
    let mut hash = anchor.hash;
    hash.reverse();
    let status = db
        .enhance_pir_snapshot_status(EnhancePirSnapshotAnchor {
            height: BlockHeight::from_u32(anchor.height as u32),
            block_hash: BlockHash(hash),
            ironwood_tree_size: anchor.ironwood_tree_size,
        })
        .map_err(|_| refused("enhance_database_error"))?;
    if status != EnhancePirSnapshotStatus::Accepted {
        return Err(refused("enhance_anchor_mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{open_wallet_db, LightServer, UnlockPolicy, WalletMeta, WalletPaths};
    use super::*;
    use orchard::{
        note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
        note_encryption::{IronwoodDomain, IronwoodNoteEncryption},
        value::NoteValue,
        Note,
    };
    use zcash_client_backend::data_api::enhance_pir::{
        EnhanceRecord, EnhanceRecordParts, EnhanceTransactionMetadata,
    };
    use zcash_client_backend::{
        data_api::{AccountBirthday, AccountPurpose, WalletRead, WalletWrite},
        proto::compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx},
        scanning::{scan_block, Nullifiers, ScanningKeys},
    };
    use zcash_note_encryption::Domain;
    use zip32::Scope;
    mod server_fixture {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../z-enhance-pir/tests/fixture.rs"
        ));
    }

    #[tokio::test]
    async fn native_source_actual_pir_suffix_apply_and_owned_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = NativeWalletSource::new(
            tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy(),
            Network::Mainnet,
            [0x71; 32],
        );
        crate::native::source::enroll_wallet(root, &source).unwrap();
        let account = crate::keys::account_from_mnemonic(
            crate::keys::REGTEST_FAUCET_MNEMONIC,
            Network::Mainnet,
            0,
        )
        .unwrap();
        let ufvk =
            zcash_keys::keys::UnifiedFullViewingKey::decode(&Network::Mainnet, &account.ufvk)
                .unwrap();
        let state = zcash_client_backend::proto::service::TreeState {
            height: 3499999,
            hash: "00".repeat(32),
            ..Default::default()
        }
        .to_chain_state()
        .unwrap();
        let mut db = open_wallet_db(&root.join("data.sqlite"), Network::Mainnet)
            .unwrap()
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
        db.import_account_ufvk(
            "public vector",
            &ufvk,
            &AccountBirthday::from_parts(state.clone(), None),
            AccountPurpose::Spending { derivation: None },
            None,
        )
        .unwrap();
        let fvk = ufvk.orchard().unwrap();
        let rho = Rho::from_bytes(&[1; 32]).unwrap();
        let note = Note::from_parts(
            fvk.address_at(0u32, Scope::External),
            NoteValue::from_raw(50000),
            rho,
            RandomSeed::from_bytes([7; 32], &rho).unwrap(),
            NoteVersion::V3,
        )
        .unwrap();
        let mut memo = [0; 512];
        memo[..13].copy_from_slice(b"private hello");
        let cmx = ExtractedNoteCommitment::from(note.commitment());
        let enc = IronwoodNoteEncryption::new(None, note, memo);
        let ciphertext = enc.encrypt_note_plaintext();
        let action = CompactOrchardAction {
            nullifier: vec![1; 32],
            cmx: cmx.to_bytes().to_vec(),
            ephemeral_key: IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
            ciphertext: ciphertext[..52].to_vec(),
        };
        let block = CompactBlock {
            height: 3500000,
            hash: (0..32).collect(),
            prev_hash: vec![0; 32],
            time: 1,
            vtx: vec![CompactTx {
                txid: vec![3; 32],
                ironwood_actions: vec![action],
                ..Default::default()
            }],
            chain_metadata: Some(ChainMetadata {
                ironwood_commitment_tree_size: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let keys = ScanningKeys::from_account_ufvks(db.get_unified_full_viewing_keys().unwrap());
        let scanned =
            scan_block(&Network::Mainnet, block, &keys, &Nullifiers::empty(), None).unwrap();
        db.update_chain_tip(3500000.into()).unwrap();
        db.put_blocks(&state, vec![scanned]).unwrap();
        drop(db);
        crate::native::source::enroll_wallet(root, &source).unwrap();
        let wallet = NativeWallet {
            paths: WalletPaths::new(root),
            network: Network::Mainnet,
            network_access: false,
            server: LightServer::Url("source-wallet:offline".into()),
            meta: WalletMeta {
                network: "mainnet".into(),
                server: "source-wallet:offline".into(),
                birthday_height: 3500000,
                account_index: 0,
                validator_rpc: None,
                ufvk: Some(account.ufvk),
                view_only: true,
                unlock_policy: UnlockPolicy::EachSpend,
                os_unlock: false,
                allow_deep_sync: false,
            },
        };
        let record = EnhanceRecord::from_parts(EnhanceRecordParts {
            enc_ciphertext_suffix: ciphertext[52..].try_into().unwrap(),
            cv_net: [0; 32],
            out_ciphertext: [0; 80],
            has_transparent_inputs: false,
            has_transparent_outputs: false,
            metadata: EnhanceTransactionMetadata::new(3500500, Some(10000)).unwrap(),
        });
        let profile = server_fixture::profile();
        let mut display_hash: [u8; 32] = std::array::from_fn(|i| i as u8);
        display_hash.reverse();
        let server =
            server_fixture::Fixture::with_records(&profile, &[*record.as_bytes()], display_hash);
        let mut pass = wallet
            .begin_private_enhance_with_source(&source, &server.manifest, 100)
            .unwrap();
        assert_eq!(pass.session_ids().len(), 1);
        assert!(database_lease::exclusive(&wallet.paths.data_db).is_err());
        pass.add_session(&server.material, 100).unwrap();
        let body = wallet
            .prepare_private_enhance_with_source(&source, &mut pass, 100)
            .unwrap();
        let reply = server.answer(&body);
        assert_eq!(
            wallet
                .apply_private_enhance_with_source(&source, &mut pass, &reply, 100)
                .unwrap(),
            EnhancePirStoreResult::Stored
        );
        assert!(wallet
            .apply_private_enhance_with_source(&source, &mut pass, &reply, 100)
            .is_err());
        pass.cancel();
        assert!(database_lease::exclusive(&wallet.paths.data_db).is_ok());
        assert!(wallet
            .prepare_private_enhance_with_source(&source, &mut pass, 100)
            .is_err());
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let memo: Vec<u8> = conn
            .query_row(
                "SELECT memo FROM ironwood_received_notes LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(&memo[..13], b"private hello");
    }
}
