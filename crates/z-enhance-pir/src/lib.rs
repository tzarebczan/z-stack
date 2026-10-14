//! Transport-neutral v7/q48 queries. The wallet accepts chain state and applies records separately.
use enhance_pir::{
    client::{record_in_row, PreparedQuery, QuerySession},
    protocol::{Manifest, ShardSession},
    EnhanceRecord,
};
use std::sync::Arc;

pub const PROTOCOL: &str = "ironwood-enhance-pir-v7";
pub const MANIFEST_LIMIT: usize = 1024 * 1024;
pub const MATERIAL_LIMIT: usize = 256 * 1024;
pub const QUERY_LIMIT: usize = 282_740;
pub const REPLY_BYTES: usize = 30_836;
pub const RECORD_BYTES: usize = 653;

/// Independently scanned state. `hash` is display-order (the reverse of compact protobuf bytes).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChainAnchor {
    pub height: u64,
    pub hash: [u8; 32],
    pub ironwood_tree_size: u64,
}

fn manifest(bytes: &[u8]) -> Result<Manifest, &'static str> {
    if bytes.len() > MANIFEST_LIMIT {
        return Err("enhance_size_limit");
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|_| "enhance_manifest_invalid")?;
    manifest
        .validate()
        .map_err(|_| "enhance_manifest_invalid")?;
    if enhance_pir::PROTOCOL_REVISION != PROTOCOL
        || manifest.protocol_revision != PROTOCOL
        || manifest.network != "main"
        || manifest.pool != "ironwood"
        || manifest.anchor_height > u32::MAX as u64
        || manifest.anchor_height < enhance_pir::ACTIVATION_HEIGHT
    {
        return Err("enhance_manifest_invalid");
    }
    Ok(manifest)
}
/// Untrusted metadata to compare with the wallet before accepting any session material.
pub fn manifest_anchor(bytes: &[u8]) -> Result<ChainAnchor, &'static str> {
    let value = manifest(bytes)?;
    Ok(ChainAnchor {
        height: value.anchor_height,
        hash: hex::decode(value.anchor_block_hash)
            .map_err(|_| "enhance_manifest_invalid")?
            .try_into()
            .map_err(|_| "enhance_manifest_invalid")?,
        ironwood_tree_size: value.coverage.records,
    })
}
fn fresh(accepted_at: u64, now: u64) -> Result<(), &'static str> {
    if now < accepted_at || now - accepted_at >= 30 {
        Err("enhance_routing_refresh_required")
    } else {
        Ok(())
    }
}
/// An accepted routing snapshot; only canonical public session identifiers leave Rust.
pub struct ManifestPlan {
    manifest: Manifest,
    anchor: ChainAnchor,
    accepted_at: u64,
}
impl ManifestPlan {
    pub fn open(bytes: &[u8], anchor: &ChainAnchor, now: u64) -> Result<Self, &'static str> {
        let value = manifest(bytes)?;
        if value.anchor_height != anchor.height
            || value.anchor_block_hash != hex::encode(anchor.hash)
            || value.coverage.records != anchor.ironwood_tree_size
        {
            return Err("enhance_anchor_mismatch");
        }
        Ok(Self {
            manifest: value,
            anchor: *anchor,
            accepted_at: now,
        })
    }
    pub fn anchor(&self) -> ChainAnchor {
        self.anchor
    }
    pub fn session_id(&self, position: u64) -> Result<String, &'static str> {
        let (shard, _, _) = self
            .manifest
            .coverage
            .locate(position)
            .ok_or("enhance_position_invalid")?;
        Ok(hex::encode(
            self.manifest
                .session_id(shard.id)
                .map_err(|_| "enhance_manifest_invalid")?,
        ))
    }
    pub fn session(&self, bytes: &[u8], now: u64) -> Result<Session, &'static str> {
        fresh(self.accepted_at, now)?;
        if bytes.len() > MATERIAL_LIMIT {
            return Err("enhance_size_limit");
        }
        let material: ShardSession =
            serde_json::from_slice(bytes).map_err(|_| "enhance_material_invalid")?;
        let inner =
            QuerySession::new(&self.manifest, material).map_err(|_| "enhance_material_invalid")?;
        Ok(Session {
            inner: Arc::new(inner),
            accepted_at: self.accepted_at,
        })
    }
}

pub struct Session {
    inner: Arc<QuerySession>,
    accepted_at: u64,
}
impl Session {
    pub fn open(
        manifest: &[u8],
        material: &[u8],
        anchor: &ChainAnchor,
        now: u64,
    ) -> Result<Self, &'static str> {
        ManifestPlan::open(manifest, anchor, now)?.session(material, now)
    }
    pub fn session_id(&self) -> String {
        hex::encode(self.inner.session_id())
    }
    pub fn prepare(&self, position: u64, now: u64) -> Result<Query, &'static str> {
        fresh(self.accepted_at, now)?;
        let (query, slot) = self
            .inner
            .prepare_position(position)
            .map_err(|_| "enhance_position_invalid")?;
        if query.body().len() > QUERY_LIMIT {
            return Err("enhance_size_limit");
        }
        Ok(Query {
            session: self.inner.clone(),
            inner: Some(query),
            slot,
            accepted_at: self.accepted_at,
        })
    }
}

/// One-use response context. The private client seed zeroizes when consumed/dropped.
pub struct Query {
    session: Arc<QuerySession>,
    inner: Option<PreparedQuery>,
    slot: usize,
    accepted_at: u64,
}
impl Query {
    pub fn body(&self) -> Result<Vec<u8>, &'static str> {
        Ok(self
            .inner
            .as_ref()
            .ok_or("enhance_context_consumed")?
            .body()
            .to_vec())
    }
    pub fn finish(&mut self, response: &[u8], now: u64) -> Result<EnhanceRecord, &'static str> {
        let query = self.inner.take().ok_or("enhance_context_consumed")?;
        fresh(self.accepted_at, now)?;
        if response.len() != REPLY_BYTES {
            return Err("enhance_reply_invalid");
        }
        let row = self
            .session
            .decode(query, response)
            .map_err(|_| "enhance_reply_invalid")?;
        record_in_row(&row, self.slot).map_err(|_| "enhance_record_invalid")
    }
    pub fn cancel(&mut self) {
        self.inner.take();
    }
}

#[cfg(test)]
mod tests;
#[cfg(all(target_arch = "wasm32", feature = "protocol-bindings"))]
mod wasm;

#[cfg(all(target_arch = "wasm32", feature = "multicore"))]
pub use wasm_bindgen_rayon::init_thread_pool;
