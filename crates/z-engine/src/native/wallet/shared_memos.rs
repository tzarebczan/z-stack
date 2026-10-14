//! Complete public memo buckets. Hosts never receive wallet-selected transaction IDs.
use super::{database_lease, NativeWallet};
use crate::{
    error::{EngineError, Result},
    native::NativeWalletSource,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    path::PathBuf,
    sync::Arc,
};
use zcash_client_backend::data_api::{
    enhance_pir::{EnhancePirRead, EnhancementMode, TransactionEnhancementWork},
    wallet::decrypt_and_store_transaction,
    WalletRead,
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    TxId,
};

fn refused(code: &str) -> EngineError {
    EngineError::Message(code.into())
}
fn database_error(_: impl std::fmt::Display) -> EngineError {
    refused("shared_memos_database_error")
}

/// An ephemeral, source-bound plan for at most two common ten-block buckets.
/// Cancellation and every attempted application retire the corresponding planned range.
pub struct NativeSharedMemosPass {
    root: PathBuf,
    binding: [u8; 32],
    owner: Option<Arc<File>>,
    ranges: VecDeque<(u32, u32)>,
    anchors: BTreeMap<u32, [u8; 32]>,
    bucket_ends: BTreeSet<u32>,
    pending: Vec<(u32, Transaction)>,
    last_block: Option<(u32, [u8; 32])>,
    accepted_bytes: usize,
}
impl NativeSharedMemosPass {
    pub fn ranges(&self) -> Vec<(u32, u32)> {
        self.ranges.iter().copied().collect()
    }
    pub fn cancel(&mut self) {
        self.ranges.clear();
        self.pending.clear();
        self.last_block = None;
        self.owner.take();
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    start: u32,
    end: u32,
    blocks: String,
    transactions: Vec<String>,
}

impl NativeWallet {
    pub fn begin_shared_memos_with_source(
        &self,
        source: &NativeWalletSource,
    ) -> Result<NativeSharedMemosPass> {
        let owner = Arc::new(database_lease::shared(&self.paths.data_db)?);
        self.require_wallet_source(source)?;
        let db = self
            .open_db()?
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        let tip = db
            .block_fully_scanned()
            .map_err(database_error)?
            .map(|b| u32::from(b.block_height()))
            .unwrap_or(0);
        let mut starts = BTreeSet::new();
        for work in db.transaction_enhancement_work().map_err(database_error)? {
            if let TransactionEnhancementWork::Public(request) = work {
                if let Some(height) = db.get_tx_height(request.txid()).map_err(database_error)? {
                    let height = u32::from(height);
                    if height > 0 && height <= tip {
                        starts.insert(((height - 1) / 10) * 10 + 1);
                    }
                }
            }
        }
        let buckets: Vec<_> = starts
            .into_iter()
            .take(2)
            .map(|start| (start, start.saturating_add(9).min(tip)))
            .collect();
        let mut anchors = BTreeMap::new();
        for &(start, end) in &buckets {
            for height in start.saturating_sub(1)..=end {
                if let Some(hash) = db.get_block_hash(height.into()).map_err(database_error)? {
                    anchors.insert(height, hash.0);
                }
            }
            if !anchors.contains_key(&end) {
                return Err(refused("shared_memos_anchor_unavailable"));
            }
        }
        let ranges = buckets
            .iter()
            .flat_map(|(start, end)| (*start..=*end).map(|h| (h, h)))
            .collect();
        let bucket_ends = buckets.iter().map(|(_, end)| *end).collect();
        Ok(NativeSharedMemosPass {
            root: self.paths.root.clone(),
            binding: source.binding(),
            owner: Some(owner),
            ranges,
            anchors,
            bucket_ends,
            pending: Vec::new(),
            last_block: None,
            accepted_bytes: 0,
        })
    }

    /// Validate the entire next planned bucket before atomically enhancing Public work only.
    pub fn apply_shared_memos_with_source(
        &self,
        source: &NativeWalletSource,
        pass: &mut NativeSharedMemosPass,
        bytes: &[u8],
    ) -> Result<u32> {
        // Consume before parsing, policy checks or database access: a bad response is never reusable.
        let range = pass
            .ranges
            .pop_front()
            .ok_or_else(|| refused("shared_memos_context_consumed"))?;
        let result = (|| {
            let total = pass
                .accepted_bytes
                .checked_add(bytes.len())
                .filter(|n| *n <= 64 * 1024 * 1024)
                .ok_or_else(|| refused("shared_memos_size_limit"))?;
            let (decoded, hash) = self.validate_shared_memos(source, pass, range, bytes)?;
            if decoded.iter().any(|(_, tx)| {
                pass.pending
                    .iter()
                    .any(|(_, known)| known.txid() == tx.txid())
            }) {
                return Err(refused("shared_memos_transaction_mismatch"));
            }
            pass.accepted_bytes = total;
            pass.last_block = Some((range.1, hash));
            pass.pending.extend(decoded);
            if pass.bucket_ends.contains(&range.1) {
                let result = self.commit_shared_memos_bucket(source, pass)?;
                pass.last_block = None;
                Ok(result)
            } else {
                Ok(0)
            }
        })();
        if result.is_err() || pass.ranges.is_empty() {
            pass.cancel();
        }
        result
    }
    fn validate_shared_memos(
        &self,
        source: &NativeWalletSource,
        pass: &NativeSharedMemosPass,
        range: (u32, u32),
        bytes: &[u8],
    ) -> Result<(Vec<(u32, Transaction)>, [u8; 32])> {
        if pass.owner.is_none() {
            return Err(refused("shared_memos_context_consumed"));
        }
        if pass.root != self.paths.root || pass.binding != source.binding() {
            return Err(refused("shared_memos_wallet_mismatch"));
        }
        self.require_wallet_source(source)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(refused("shared_memos_size_limit"));
        }
        let bundle: Bundle =
            serde_json::from_slice(bytes).map_err(|_| refused("shared_memos_invalid"))?;
        if (bundle.start, bundle.end) != range {
            return Err(refused("shared_memos_range_mismatch"));
        }
        let framed =
            crate::web::from_hex(&bundle.blocks).map_err(|_| refused("shared_memos_invalid"))?;
        let blocks =
            crate::web::decode_delimited(&framed).map_err(|_| refused("shared_memos_invalid"))?;
        if blocks.len() != (range.1 - range.0 + 1) as usize {
            return Err(refused("shared_memos_incomplete"));
        }
        let mut expected = Vec::new();
        let mut ids = BTreeSet::new();
        let mut previous = pass.last_block.as_ref().and_then(|(height, hash)| {
            (*height == range.0.saturating_sub(1)).then_some(hash.as_slice())
        });
        for (offset, block) in blocks.iter().enumerate() {
            let height = range.0 + offset as u32;
            if block.height != u64::from(height)
                || block.hash.len() != 32
                || block.prev_hash.len() != 32
                || previous.is_some_and(|hash: &[u8]| hash != block.prev_hash.as_slice())
                || pass
                    .anchors
                    .get(&height)
                    .is_some_and(|hash| hash.as_slice() != block.hash)
                || pass
                    .anchors
                    .get(&height.saturating_sub(1))
                    .is_some_and(|hash| hash.as_slice() != block.prev_hash)
            {
                return Err(refused("shared_memos_chain_mismatch"));
            }
            let mut index = None;
            for tx in &block.vtx {
                if tx.txid.len() != 32
                    || !ids.insert(tx.txid.clone())
                    || index.is_some_and(|last| tx.index <= last)
                {
                    return Err(refused("shared_memos_invalid"));
                }
                index = Some(tx.index);
                if !tx.spends.is_empty()
                    || !tx.outputs.is_empty()
                    || !tx.actions.is_empty()
                    || !tx.ironwood_actions.is_empty()
                {
                    expected.push((height, tx.txid.as_slice()));
                }
            }
            previous = Some(block.hash.as_slice());
        }
        if expected.len() != bundle.transactions.len() {
            return Err(refused("shared_memos_incomplete"));
        }
        let mut decoded = Vec::new();
        for ((height, id), raw) in expected.iter().zip(&bundle.transactions) {
            let raw = crate::web::from_hex(raw).map_err(|_| refused("shared_memos_invalid"))?;
            let mut rest = raw.as_slice();
            let tx = Transaction::read(
                &mut rest,
                BranchId::for_height(&self.network, BlockHeight::from_u32(*height)),
            )
            .map_err(|_| refused("shared_memos_invalid"))?;
            if !rest.is_empty() || tx.txid().as_ref() != *id {
                return Err(refused("shared_memos_transaction_mismatch"));
            }
            decoded.push((*height, tx));
        }
        // Locally known shielded transactions cannot be hidden by deleting both compact and raw entries.
        let conn = rusqlite::Connection::open_with_flags(
            &self.paths.data_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(database_error)?;
        let mut statement=conn.prepare("SELECT DISTINCT txid,mined_height,raw FROM v_transactions WHERE mined_height BETWEEN ?1 AND ?2").map_err(database_error)?;
        let known = statement
            .query_map([range.0, range.1], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })
            .map_err(database_error)?;
        for item in known {
            let (id, height, raw) = item.map_err(database_error)?;
            let shielded = if let Some(raw) = raw {
                let mut rest = raw.as_slice();
                let tx = Transaction::read(
                    &mut rest,
                    BranchId::for_height(&self.network, height.into()),
                )
                .map_err(|_| refused("shared_memos_database_error"))?;
                if !rest.is_empty() {
                    return Err(refused("shared_memos_database_error"));
                }
                tx.sapling_bundle().is_some()
                    || tx.orchard_bundle().is_some()
                    || tx.ironwood_bundle().is_some()
            } else {
                true
            };
            if shielded
                && !expected
                    .iter()
                    .any(|(h, txid)| *h == height && *txid == id.as_slice())
            {
                return Err(refused("shared_memos_omitted_wallet_transaction"));
            }
        }
        drop(statement);
        drop(conn);
        let hash = blocks
            .last()
            .expect("validated single-block range")
            .hash
            .as_slice()
            .try_into()
            .map_err(|_| refused("shared_memos_invalid"))?;
        Ok((decoded, hash))
    }
    fn commit_shared_memos_bucket(
        &self,
        source: &NativeWalletSource,
        pass: &mut NativeSharedMemosPass,
    ) -> Result<u32> {
        self.require_wallet_source(source)?;
        let decoded = std::mem::take(&mut pass.pending);
        let mut db = self
            .open_db()?
            .with_enhancement_mode(EnhancementMode::PrivateIronwood);
        let applied = db
            .transactionally(|db| -> std::result::Result<u32, ApplyError> {
                for (height, hash) in &pass.anchors {
                    if db
                        .get_block_hash((*height).into())
                        .map_err(|_| ApplyError)?
                        .is_none_or(|current| current.0 != *hash)
                    {
                        return Err(ApplyError);
                    }
                }
                let public: BTreeSet<TxId> = db
                    .transaction_enhancement_work()
                    .map_err(|_| ApplyError)?
                    .into_iter()
                    .filter_map(|work| match work {
                        TransactionEnhancementWork::Public(request) => Some(request.txid()),
                        _ => None,
                    })
                    .collect();
                let mut count = 0;
                for (height, tx) in decoded {
                    if public.contains(&tx.txid()) {
                        if db.get_tx_height(tx.txid()).map_err(|_| ApplyError)?
                            != Some(height.into())
                        {
                            return Err(ApplyError);
                        }
                        decrypt_and_store_transaction(&self.network, db, &tx, Some(height.into()))
                            .map_err(|_| ApplyError)?;
                        count += 1;
                    }
                }
                Ok(count)
            })
            .map_err(|_| refused("shared_memos_database_or_chain_changed"))?;
        Ok(applied)
    }
}
struct ApplyError;
impl From<rusqlite::Error> for ApplyError {
    fn from(_: rusqlite::Error) -> Self {
        Self
    }
}

#[cfg(test)]
#[path = "shared_memos_tests.rs"]
mod tests;
