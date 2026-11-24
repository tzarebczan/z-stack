//! Pluggable PIR backend descriptors and hop-backed HTTP PIR client.
//!
//! Allows Enhance PIR, Transparent PIR, and Status PIR to point at either one unified
//! PIR service (e.g. Valar `wallet-pir.valargroup.dev` or self-hosted `wallet-pir`)
//! or completely separate operators — each routed over any [`NetworkHop`]
//! (Direct loopback, Tor, Nym, OHTTP, Tor+OHTTP, Nym+OHTTP).

use crate::{HopRequest, HopRole, NetworkHop, TransportError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PirProtocol {
    /// Coffer self-hosted `wallet-pir` Enhance v7 / q48 (`EPQ7` wire envelope).
    EnhanceV7Q48,
    /// Upstream Valar `wallet-pir` Enhance v9+ (`/enhance/v1`).
    EnhanceV9,
    /// Upstream Valar `wallet-pir` Transparent Shard v12 (`/v2/shards/init`, `/v2/filters/shards`, ...).
    TransparentShardV12,
    /// Upstream Valar `wallet-pir` Status v2 (`/status/v1`).
    StatusV2,
}

/// Configuration for a single pluggable PIR backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirEndpointConfig {
    pub base_url: String,
    pub protocol: PirProtocol,
    /// Optional 32-byte hex source identifier (required for Transparent v12).
    pub source_id: Option<String>,
    /// Optional SHA-256 hex digest pinning static offer / publisher trust metadata.
    pub pinned_fingerprint: Option<String>,
}

/// Transport-neutral PIR HTTP adapter backed by any [`NetworkHop`].
pub struct HopPirClient {
    endpoint: PirEndpointConfig,
    hop: Arc<dyn NetworkHop>,
}

impl HopPirClient {
    pub fn new(
        endpoint: PirEndpointConfig,
        hop: Arc<dyn NetworkHop>,
    ) -> Result<Self, TransportError> {
        let trimmed = endpoint.base_url.trim_end_matches('/');
        if trimmed.is_empty()
            || !(trimmed.starts_with("https://")
                || trimmed.starts_with("http://127.0.0.1:")
                || trimmed.starts_with("http://[::1]:")
                || trimmed.starts_with('/'))
        {
            return Err(TransportError::InvalidRequest("pir_base_url_invalid"));
        }
        Ok(Self {
            endpoint: PirEndpointConfig {
                base_url: trimmed.to_string(),
                ..endpoint
            },
            hop,
        })
    }

    pub fn endpoint(&self) -> &PirEndpointConfig {
        &self.endpoint
    }

    pub fn hop(&self) -> &Arc<dyn NetworkHop> {
        &self.hop
    }

    pub async fn get(
        &self,
        path: &str,
        max_bytes: usize,
        timeout_ms: u64,
    ) -> Result<(u16, Vec<u8>), TransportError> {
        if !path.starts_with('/') || path.contains("..") {
            return Err(TransportError::InvalidRequest("pir_path_invalid"));
        }
        let target = format!("{}{}", self.endpoint.base_url, path);
        let req = HopRequest::get(target, HopRole::Pir)
            .with_max_response_bytes(max_bytes)
            .with_timeout_ms(timeout_ms);
        let resp = self.hop.exchange(req).await?;
        Ok((resp.status, resp.body))
    }

    pub async fn post(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        max_bytes: usize,
        timeout_ms: u64,
    ) -> Result<(u16, Vec<u8>), TransportError> {
        if !path.starts_with('/') || path.contains("..") {
            return Err(TransportError::InvalidRequest("pir_path_invalid"));
        }
        let target = format!("{}{}", self.endpoint.base_url, path);
        let req = HopRequest::post(target, content_type, body, HopRole::Pir)
            .with_max_response_bytes(max_bytes)
            .with_timeout_ms(timeout_ms);
        let resp = self.hop.exchange(req).await?;
        Ok((resp.status, resp.body))
    }
}

/// Independent, pluggable PIR backends for a wallet session.
///
/// Any subset may be configured, pointing at either one shared PIR server or distinct operators.
#[derive(Default)]
pub struct PirBackends {
    pub enhance: Option<Arc<HopPirClient>>,
    pub transparent: Option<Arc<HopPirClient>>,
    pub status: Option<Arc<HopPirClient>>,
}

impl PirBackends {
    pub fn has_any(&self) -> bool {
        self.enhance.is_some() || self.transparent.is_some() || self.status.is_some()
    }
}
