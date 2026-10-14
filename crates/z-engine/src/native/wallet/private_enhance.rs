//! Source-bound private enhancement. No selected-transaction transport exists on this path.
use super::{database_lease, NativeWallet};
use crate::{
    enhancement::EnhanceBatch,
    error::{EngineError, Result},
    native::NativeWalletSource,
    Network,
};
use std::{collections::BTreeMap, fs::File, path::PathBuf, sync::Arc};
use zcash_client_backend::data_api::enhance_pir::{
    EnhancePirRead, EnhancePirRequest, EnhancePirSnapshotAnchor, EnhancePirSnapshotStatus,
    EnhancePirStoreResult, EnhancePirWork, EnhancePirWrite, EnhancementMode,
    TransactionEnhancementWork,
};
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{consensus::BranchId, TxId};
fn refused(code: &str) -> EngineError {
    EngineError::Message(code.into())
}

/// Ephemeral pass with retained database ownership. Drop or cancel to retire private contexts.
pub struct NativeEnhancePass {
    root: PathBuf,
    binding: [u8; 32],
    owner: Option<Arc<File>>,
    batch: EnhanceBatch<EnhancePirRequest>,
    eligible: BTreeMap<TxId, Eligibility>,
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
#[derive(Debug, PartialEq, Eq)]
enum Eligibility {
    Incoming,
    LinkedFee(u64),
    Unsupported,
}
impl Eligibility {
    fn permits_query(&self) -> bool {
        !matches!(self, Self::Unsupported)
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
        let mut work = Vec::new();
        let mut eligible = BTreeMap::new();
        for item in db
            .transaction_enhancement_work()
            .map_err(|_| refused("enhance_database_error"))?
        {
            if let TransactionEnhancementWork::Private(EnhancePirWork::Query(request)) = item {
                let id = request.request_id().txid();
                let evidence = self.private_enhance_eligibility(id)?;
                if evidence.permits_query() {
                    eligible.insert(id, evidence);
                    work.push((request, u64::from(request.position())));
                }
            }
            // Rediscover and suspended work stay durable. They never authorize selected fetches.
        }
        let batch = EnhanceBatch::open(manifest, &anchor, work, now).map_err(refused)?;
        Ok(NativeEnhancePass {
            root: self.paths.root.clone(),
            binding: source.binding(),
            owner: Some(owner),
            batch,
            eligible,
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
        for (id, evidence) in &pass.eligible {
            if self.private_enhance_eligibility(*id)? != *evidence {
                pass.cancel();
                return Err(refused("enhance_fee_evidence_unavailable"));
            }
        }
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
        let evidence = self.private_enhance_eligibility(request.request_id().txid())?;
        if pass.eligible.get(&request.request_id().txid()) != Some(&evidence) {
            pass.cancel();
            return Err(refused("enhance_fee_evidence_unavailable"));
        }
        let record = zcash_client_backend::data_api::enhance_pir::EnhanceRecord::from_bytes(record)
            .map_err(|_| refused("enhance_record_invalid"))?;
        if let Eligibility::LinkedFee(fee) = evidence {
            if record.metadata().fee_zatoshis() != Some(fee) {
                pass.cancel();
                return Err(refused("enhance_fee_mismatch"));
            }
        }
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
    // Upstream padding retirement accepts the indexer's fee assertion. For a linked
    // wallet spend, require preexisting canonical full-transaction evidence instead.
    // Incoming-only work has no outgoing padding value-balance inference.
    fn private_enhance_eligibility(&self, id: TxId) -> Result<Eligibility> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| refused("enhance_database_error"))?;
        let (linked, raw, fee, height): (bool, Option<Vec<u8>>, Option<i64>, Option<u32>) = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_received_note_spends s WHERE s.transaction_id=t.id_tx), t.raw, t.fee, t.mined_height FROM transactions t WHERE t.txid=?1",
            [id.as_ref()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).map_err(|_| refused("enhance_database_error"))?;
        if !linked {
            return Ok(Eligibility::Incoming);
        }
        let (Some(raw), Some(fee), Some(height)) = (raw, fee, height) else {
            return Ok(Eligibility::Unsupported);
        };
        let mut rest = raw.as_slice();
        let Ok(tx) = Transaction::read(
            &mut rest,
            BranchId::for_height(&self.network, height.into()),
        ) else {
            return Ok(Eligibility::Unsupported);
        };
        if !rest.is_empty()
            || tx.txid() != id
            || tx.transparent_bundle().is_some()
            || tx.sapling_bundle().is_some()
            || tx.orchard_bundle().is_some()
            || tx.sprout_bundle().is_some()
        {
            return Ok(Eligibility::Unsupported);
        }
        let mut canonical = Vec::new();
        if tx.write(&mut canonical).is_err() || canonical != raw {
            return Ok(Eligibility::Unsupported);
        }
        let Some(bundle) = tx.ironwood_bundle() else {
            return Ok(Eligibility::Unsupported);
        };
        let derived = i64::from(*bundle.value_balance());
        // For pure Ironwood the positive value balance is exactly the transaction fee.
        Ok(if derived >= 0 && fee == derived {
            Eligibility::LinkedFee(derived as u64)
        } else {
            Eligibility::Unsupported
        })
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

    fn incoming_fixture(
        root: &std::path::Path,
    ) -> (
        NativeWallet,
        NativeWalletSource,
        CompactBlock,
        EnhanceRecord,
    ) {
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
        let scanned = scan_block(
            &Network::Mainnet,
            block.clone(),
            &keys,
            &Nullifiers::empty(),
            None,
        )
        .unwrap();
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
        (wallet, source, block, record)
    }
    #[tokio::test]
    async fn native_source_actual_pir_suffix_apply_and_owned_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let (wallet, source, _block, record) = incoming_fixture(dir.path());
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

    #[tokio::test]
    async fn linked_cold_spend_without_fullraw_fee_never_queries_or_retires_hidden_outputs() {
        use zcash_protocol::consensus::BlockHeight;
        let dir = tempfile::tempdir().unwrap();
        let (wallet, source, first, funding_record) = incoming_fixture(dir.path());
        let profile = server_fixture::profile();
        let mut first_hash: [u8; 32] = first.hash.as_slice().try_into().unwrap();
        first_hash.reverse();
        let initial = server_fixture::Fixture::with_records(
            &profile,
            &[*funding_record.as_bytes()],
            first_hash,
        );
        let mut incoming = wallet
            .begin_private_enhance_with_source(&source, &initial.manifest, 100)
            .unwrap();
        incoming.add_session(&initial.material, 100).unwrap();
        let body = wallet
            .prepare_private_enhance_with_source(&source, &mut incoming, 100)
            .unwrap();
        wallet
            .apply_private_enhance_with_source(&source, &mut incoming, &initial.answer(&body), 100)
            .unwrap();
        incoming.cancel();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let nf: [u8; 32] = conn
            .query_row(
                "SELECT nf FROM ironwood_received_notes LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let account = crate::keys::account_from_mnemonic(
            crate::keys::REGTEST_FAUCET_MNEMONIC,
            Network::Mainnet,
            0,
        )
        .unwrap();
        let own = zcash_keys::keys::UnifiedFullViewingKey::decode(&Network::Mainnet, &account.ufvk)
            .unwrap()
            .orchard()
            .unwrap()
            .clone();
        let stranger = crate::keys::account_from_mnemonic(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            Network::Mainnet,
            0,
        )
        .unwrap();
        let foreign =
            zcash_keys::keys::UnifiedFullViewingKey::decode(&Network::Mainnet, &stranger.ufvk)
                .unwrap()
                .orchard()
                .unwrap()
                .clone();
        let mut records = vec![*funding_record.as_bytes()];
        let mut actions = Vec::new();
        for (recipient, value, nullifier) in [
            (foreign.address_at(0u32, Scope::External), 25000, nf),
            (own.address_at(0u32, Scope::Internal), 20000, [5; 32]),
            (foreign.address_at(1u32, Scope::External), 0, [6; 32]),
        ] {
            let rho = Rho::from_bytes(&nullifier).unwrap();
            let note = Note::from_parts(
                recipient,
                NoteValue::from_raw(value),
                rho,
                RandomSeed::from_bytes([7; 32], &rho).unwrap(),
                NoteVersion::V3,
            )
            .unwrap();
            let enc = IronwoodNoteEncryption::new(None, note, [0; 512]);
            let ciphertext = enc.encrypt_note_plaintext();
            let cmx = ExtractedNoteCommitment::from(note.commitment());
            actions.push(CompactOrchardAction {
                nullifier: nullifier.to_vec(),
                cmx: cmx.to_bytes().to_vec(),
                ephemeral_key: IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
                ciphertext: ciphertext[..52].to_vec(),
            });
            records.push(
                *EnhanceRecord::from_parts(EnhanceRecordParts {
                    enc_ciphertext_suffix: ciphertext[52..].try_into().unwrap(),
                    cv_net: [0; 32],
                    out_ciphertext: [0; 80],
                    has_transparent_inputs: false,
                    has_transparent_outputs: false,
                    metadata: EnhanceTransactionMetadata::new(3500500, Some(5000)).unwrap(),
                })
                .as_bytes(),
            );
        }
        let second = CompactBlock {
            height: 3500001,
            hash: vec![8; 32],
            prev_hash: first.hash.clone(),
            time: 2,
            vtx: vec![CompactTx {
                txid: vec![4; 32],
                ironwood_actions: actions,
                ..Default::default()
            }],
            chain_metadata: Some(ChainMetadata {
                ironwood_commitment_tree_size: 4,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut db = wallet
            .open_db()
            .unwrap()
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        let keys = ScanningKeys::from_account_ufvks(db.get_unified_full_viewing_keys().unwrap());
        let nullifiers = Nullifiers::unspent(&db).unwrap();
        let scanned = scan_block(
            &Network::Mainnet,
            second,
            &keys,
            &nullifiers,
            db.block_max_scanned().unwrap().as_ref(),
        )
        .unwrap();
        let state = zcash_client_backend::proto::service::TreeState {
            height: 3499999,
            hash: "00".repeat(32),
            ..Default::default()
        }
        .to_chain_state()
        .unwrap();
        let rescanned =
            scan_block(&Network::Mainnet, first, &keys, &Nullifiers::empty(), None).unwrap();
        db.update_chain_tip(BlockHeight::from_u32(3500001)).unwrap();
        db.put_blocks(&state, vec![rescanned, scanned]).unwrap();
        let before = db.transaction_enhancement_work().unwrap();
        assert!(before.iter().any(|work|matches!(work,TransactionEnhancementWork::Private(EnhancePirWork::Query(request)) if request.request_id().txid()==zcash_protocol::TxId::from_bytes([4;32]))));
        drop(db);
        let server = server_fixture::Fixture::with_records(&profile, &records, [8; 32]);
        let mut manifest: serde_json::Value = serde_json::from_slice(&server.manifest).unwrap();
        manifest["anchor_height"] = 3500001.into();
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut pass = wallet
            .begin_private_enhance_with_source(&source, &manifest, 100)
            .unwrap();
        assert!(
            pass.session_ids().is_empty(),
            "unproven linked spend must not create a PIR query session"
        );
        assert!(wallet
            .prepare_private_enhance_with_source(&source, &mut pass, 100)
            .unwrap()
            .is_empty());
        assert!(wallet
            .apply_private_enhance_with_source(&source, &mut pass, &[0; 30836], 100)
            .is_err());
        pass.cancel();
        let db = wallet
            .open_db()
            .unwrap()
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        assert_eq!(db.transaction_enhancement_work().unwrap(), before);
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        let fee: Option<i64> = conn
            .query_row(
                "SELECT fee FROM transactions WHERE txid=?1",
                [[4u8; 32].as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fee.is_none());
    }
}
