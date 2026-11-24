//! Composable, fail-closed network hops and pluggable privacy policy primitives.
//!
//! Design boundaries:
//! - Zero wallet keys, seeds, or Zcash transaction cryptography live here.
//! - Every hop is nestable (`OhttpHop<TorHop>`, `OhttpHop<Socks5Hop>`, `OhttpHop<DirectHop>`).
//! - Failures are typed and never include secret payloads or silently fall back to clearnet.
//! - Retries apply strictly to idempotent reads (`Read`, `Pir`), never to `Submit`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use thiserror::Error;

#[cfg(feature = "native")]
pub mod native;
#[cfg(feature = "ohttp")]
pub mod ohttp_hop;
pub mod pir;
pub mod policy;

/// Logical traffic class used by [`SplitHopRouter`] and retry/fail-closed guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HopRole {
    /// Compact blocks, chain tips, subtree roots, tree states.
    Read,
    /// Transaction submission (`SendTransaction` / signed submit). Never retried automatically.
    Submit,
    /// Private Information Retrieval queries (Enhance, Transparent, Status).
    Pir,
    /// External APIs (swaps, prices, ZNS, Base/Solana RPC, encrypted metadata).
    External,
}

impl HopRole {
    /// Only idempotent read/PIR requests may be automatically retried on transient transport failures.
    pub fn is_idempotent_default(self) -> bool {
        matches!(self, Self::Read | Self::Pir)
    }
}

/// Transport mechanism implemented by a [`NetworkHop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HopKind {
    Direct,
    Socks5,
    Tor,
    Nym,
    Ohttp,
}

/// Bounded HTTP request passed through a [`NetworkHop`].
#[derive(Debug, Clone)]
pub struct HopRequest {
    pub method: String,
    /// Full HTTPS/loopback-HTTP URL for direct/proxy/RFC-9458 hops, or inner route path for gateway hops.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub role: HopRole,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
    /// When true, callers explicitly assert the request is safe to retry on transient network failure.
    /// Ignored when `role == HopRole::Submit` (submissions never auto-retry).
    pub idempotent: bool,
}

impl HopRequest {
    pub fn get(target: impl Into<String>, role: HopRole) -> Self {
        Self {
            method: "GET".into(),
            target: target.into(),
            headers: Vec::new(),
            body: Vec::new(),
            role,
            max_response_bytes: 16 * 1024 * 1024,
            timeout_ms: 30_000,
            idempotent: role.is_idempotent_default(),
        }
    }

    pub fn post(
        target: impl Into<String>,
        content_type: impl Into<String>,
        body: Vec<u8>,
        role: HopRole,
    ) -> Self {
        Self {
            method: "POST".into(),
            target: target.into(),
            headers: vec![("content-type".into(), content_type.into())],
            body,
            role,
            max_response_bytes: 16 * 1024 * 1024,
            timeout_ms: 30_000,
            idempotent: role.is_idempotent_default(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_max_response_bytes(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn can_retry(&self) -> bool {
        self.idempotent && self.role != HopRole::Submit
    }
}

/// Bounded HTTP response returned by a [`NetworkHop`].
#[derive(Debug, Clone)]
pub struct HopResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HopResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Privacy-safe transport error (never embeds raw request bodies, keys, or arbitrary upstream text).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("transport hop closed")]
    Closed,
    #[error("transport hop unavailable ({0})")]
    Unavailable(&'static str),
    #[error("fail-closed policy blocked fallback ({0})")]
    FailClosed(&'static str),
    #[error("transport request timed out")]
    Timeout,
    #[error("transport response exceeded size limit")]
    SizeLimit,
    #[error("transport request invalid ({0})")]
    InvalidRequest(&'static str),
    #[error("transport protocol error ({0})")]
    Protocol(&'static str),
    #[error("OHTTP encapsulation error ({0})")]
    Ohttp(&'static str),
}

impl TransportError {
    /// Whether this error is transient and eligible for bounded retry on idempotent reads.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Timeout)
    }
}

/// Bounded retry configuration for idempotent reads (`Read` / `Pir`).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 250,
            max_delay_ms: 4_000,
        }
    }
}

impl RetryPolicy {
    pub const NONE: Self = Self {
        max_attempts: 1,
        base_delay_ms: 0,
        max_delay_ms: 0,
    };

    /// Deterministic exponential backoff delay (caller may add jitter).
    pub fn delay_for_attempt(&self, attempt_zero_based: usize) -> Duration {
        let shift = (attempt_zero_based as u32).min(10);
        let ms = self
            .base_delay_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_delay_ms);
        Duration::from_millis(ms)
    }
}

/// Core composable network hop trait.
///
/// Any hop can wrap another hop (e.g. `OhttpHop<TorHop>`, `OhttpHop<Socks5Hop>`, `OhttpHop<DirectHop>`).
#[async_trait]
pub trait NetworkHop: Send + Sync {
    fn kind(&self) -> HopKind;
    fn label(&self) -> &str;
    async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError>;
    async fn close(&self) {}
}

/// Routes traffic classes (`Read`, `Submit`, `Pir`, `External`) to independent composable hops
/// with a shared generation epoch so transport switches invalidate every active channel cleanly.
pub struct SplitHopRouter {
    read: Arc<dyn NetworkHop>,
    submit: Arc<dyn NetworkHop>,
    pir: Arc<dyn NetworkHop>,
    external: Arc<dyn NetworkHop>,
    retry: RetryPolicy,
    closed: AtomicBool,
    epoch: AtomicU64,
}

impl SplitHopRouter {
    pub fn uniform(hop: Arc<dyn NetworkHop>) -> Self {
        Self {
            read: hop.clone(),
            submit: hop.clone(),
            pir: hop.clone(),
            external: hop,
            retry: RetryPolicy::default(),
            closed: AtomicBool::new(false),
            epoch: AtomicU64::new(1),
        }
    }

    pub fn split(
        read: Arc<dyn NetworkHop>,
        submit: Arc<dyn NetworkHop>,
        pir: Arc<dyn NetworkHop>,
        external: Arc<dyn NetworkHop>,
    ) -> Self {
        Self {
            read,
            submit,
            pir,
            external,
            retry: RetryPolicy::default(),
            closed: AtomicBool::new(false),
            epoch: AtomicU64::new(1),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn hop_for(&self, role: HopRole) -> Arc<dyn NetworkHop> {
        match role {
            HopRole::Read => self.read.clone(),
            HopRole::Submit => self.submit.clone(),
            HopRole::Pir => self.pir.clone(),
            HopRole::External => self.external.clone(),
        }
    }

    /// Execute a request on the role's configured hop.
    ///
    /// - Refuses immediately if the router was closed (prevents stale-transport leakage on switch).
    /// - Retries only when `req.can_retry()` (never retries `HopRole::Submit`).
    pub async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let start_epoch = self.epoch();
        let hop = self.hop_for(req.role);
        let attempts = if req.can_retry() {
            self.retry.max_attempts.max(1)
        } else {
            1
        };
        let mut last_err = TransportError::Unavailable("no_attempt");
        for attempt in 0..attempts {
            if self.closed.load(Ordering::Acquire) || self.epoch() != start_epoch {
                return Err(TransportError::Closed);
            }
            match hop.exchange(req.clone()).await {
                Ok(resp) => {
                    if self.closed.load(Ordering::Acquire) || self.epoch() != start_epoch {
                        return Err(TransportError::Closed);
                    }
                    // Retry 502/503/504 on idempotent reads only.
                    if req.can_retry()
                        && matches!(resp.status, 502 | 503 | 504)
                        && attempt + 1 < attempts
                    {
                        last_err = TransportError::Unavailable("upstream_transient_status");
                        #[cfg(feature = "native")]
                        tokio::time::sleep(self.retry.delay_for_attempt(attempt)).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(err) => {
                    if !req.can_retry() || !err.is_retryable() || attempt + 1 >= attempts {
                        return Err(err);
                    }
                    last_err = err;
                    #[cfg(feature = "native")]
                    tokio::time::sleep(self.retry.delay_for_attempt(attempt)).await;
                }
            }
        }
        Err(last_err)
    }

    /// Close and fence every configured hop (e.g., when user switches Direct -> Tor or changes profile).
    pub async fn close_all(&self) {
        self.closed.store(true, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.read.close().await;
        self.submit.close().await;
        self.pir.close().await;
        self.external.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct CountingHop {
        kind: HopKind,
        fail_times: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl NetworkHop for CountingHop {
        fn kind(&self) -> HopKind {
            self.kind
        }
        fn label(&self) -> &str {
            "counting"
        }
        async fn exchange(&self, _req: HopRequest) -> Result<HopResponse, TransportError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err(TransportError::Unavailable("transient"))
            } else {
                Ok(HopResponse {
                    status: 200,
                    headers: vec![],
                    body: b"ok".to_vec(),
                })
            }
        }
    }

    #[tokio::test]
    async fn retries_idempotent_reads_but_never_retries_submit() {
        let read_hop = Arc::new(CountingHop {
            kind: HopKind::Direct,
            fail_times: 2,
            calls: AtomicUsize::new(0),
        });
        let submit_hop = Arc::new(CountingHop {
            kind: HopKind::Ohttp,
            fail_times: 1,
            calls: AtomicUsize::new(0),
        });
        let router = SplitHopRouter::split(
            read_hop.clone(),
            submit_hop.clone(),
            read_hop.clone(),
            read_hop.clone(),
        )
        .with_retry(RetryPolicy {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 5,
        });

        let read_res = router
            .exchange(HopRequest::get(
                "https://example.invalid/tip",
                HopRole::Read,
            ))
            .await
            .unwrap();
        assert_eq!(read_res.status, 200);
        assert_eq!(read_hop.calls.load(Ordering::SeqCst), 3);

        // Even if caller sets idempotent = true on Submit, router must NEVER retry a broadcast.
        let mut submit_req = HopRequest::post(
            "https://example.invalid/submit",
            "application/octet-stream",
            vec![1, 2, 3],
            HopRole::Submit,
        );
        submit_req.idempotent = true;
        let submit_err = router.exchange(submit_req).await.unwrap_err();
        assert_eq!(submit_err, TransportError::Unavailable("transient"));
        assert_eq!(submit_hop.calls.load(Ordering::SeqCst), 1);

        router.close_all().await;
        let after_close = router
            .exchange(HopRequest::get(
                "https://example.invalid/tip",
                HopRole::Read,
            ))
            .await
            .unwrap_err();
        assert_eq!(after_close, TransportError::Closed);
    }
}
