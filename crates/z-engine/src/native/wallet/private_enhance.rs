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
