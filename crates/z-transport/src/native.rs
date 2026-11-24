//! Native Rust network hops: `DirectHop`, `Socks5Hop` (system Tor / Orbot / Nym SOCKS5),
//! and `TorHop` (SOCKS5h proxy or embedded Arti client behind the `tor-arti` feature).
//!
//! All proxy/Tor hops fail closed and never resolve hostnames locally (`socks5h://`).

use crate::{HopKind, HopRequest, HopResponse, NetworkHop, TransportError};
use async_trait::async_trait;
use reqwest::{Method, Proxy};
#[cfg(feature = "tor-arti")]
use std::sync::Arc;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use url::Url;

fn validate_http_target(raw: &str) -> Result<Url, TransportError> {
    let url = Url::parse(raw).map_err(|_| TransportError::InvalidRequest("target_url_invalid"))?;
    let loopback = url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("[::1]") | Some("localhost")
        )
        && url.port().is_some();
    if !(url.scheme() == "https" || loopback) {
        return Err(TransportError::InvalidRequest("target_scheme_refused"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TransportError::InvalidRequest("target_credentials_refused"));
    }
    Ok(url)
}

/// Validates a SOCKS5h proxy URL (forces remote DNS resolution to prevent DNS leaks).
pub fn validate_socks5h_proxy(raw: &str) -> Result<Url, TransportError> {
    let url = Url::parse(raw).map_err(|_| TransportError::InvalidRequest("proxy_url_invalid"))?;
    if url.scheme() != "socks5h" {
        return Err(TransportError::InvalidRequest(
            "proxy_must_use_socks5h_remote_dns",
        ));
    }
    if url.host_str().is_none() || url.port().is_none() {
        return Err(TransportError::InvalidRequest("proxy_host_port_required"));
    }
    Ok(url)
}

async fn run_reqwest(
    client: &reqwest::Client,
    closed: &AtomicBool,
    req: HopRequest,
) -> Result<HopResponse, TransportError> {
    if closed.load(Ordering::Acquire) {
        return Err(TransportError::Closed);
    }
    let url = validate_http_target(&req.target)?;
    let method = Method::from_bytes(req.method.as_bytes())
        .map_err(|_| TransportError::InvalidRequest("http_method_invalid"))?;
    let timeout = Duration::from_millis(req.timeout_ms.clamp(500, 120_000));
    let mut builder = client.request(method, url).timeout(timeout);
    for (k, v) in &req.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if !req.body.is_empty() {
        builder = builder.body(req.body);
    }
    let mut response = builder.send().await.map_err(|err| {
        if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Unavailable("http_send_failed")
        }
    })?;
    if closed.load(Ordering::Acquire) {
        return Err(TransportError::Closed);
    }
    if response
        .content_length()
        .is_some_and(|n| n > req.max_response_bytes as u64)
    {
        return Err(TransportError::SizeLimit);
    }
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_owned(), s.to_owned()))
        })
        .collect();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Unavailable("http_read_failed")
        }
    })? {
        if closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        if body.len().saturating_add(chunk.len()) > req.max_response_bytes {
            return Err(TransportError::SizeLimit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HopResponse {
        status,
        headers,
        body,
    })
}

/// Direct HTTPS / loopback-HTTP hop. Never uses ambient environment proxies or redirects.
pub struct DirectHop {
    client: reqwest::Client,
    label: String,
    closed: AtomicBool,
}

impl DirectHop {
    pub fn new(label: impl Into<String>) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| TransportError::Unavailable("direct_client_init"))?;
        Ok(Self {
            client,
            label: label.into(),
            closed: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl NetworkHop for DirectHop {
    fn kind(&self) -> HopKind {
        HopKind::Direct
    }
    fn label(&self) -> &str {
        &self.label
    }
    async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
        run_reqwest(&self.client, &self.closed, req).await
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// Fail-closed SOCKS5h hop (remote DNS resolution).
///
/// Works identically for system Tor (`socks5h://127.0.0.1:9050`), Orbot,
/// Nym SOCKS5 client (`socks5h://127.0.0.1:1080`), or gateway-side egress proxies.
/// Never falls back to direct clearnet if the proxy is unreachable.
pub struct Socks5Hop {
    client: reqwest::Client,
    kind: HopKind,
    label: String,
    closed: AtomicBool,
}

impl Socks5Hop {
    pub fn new(
        proxy_url: &str,
        kind: HopKind,
        label: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let validated = validate_socks5h_proxy(proxy_url)?;
        let proxy = Proxy::all(validated.as_str())
            .map_err(|_| TransportError::InvalidRequest("socks5_proxy_invalid"))?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| TransportError::Unavailable("socks5_client_init"))?;
        Ok(Self {
            client,
            kind,
            label: label.into(),
            closed: AtomicBool::new(false),
        })
    }

    pub fn nym(proxy_url: &str) -> Result<Self, TransportError> {
        Self::new(proxy_url, HopKind::Nym, "Nym SOCKS5h")
    }

    pub fn tor_proxy(proxy_url: &str) -> Result<Self, TransportError> {
        Self::new(proxy_url, HopKind::Tor, "Tor SOCKS5h")
    }
}

#[async_trait]
impl NetworkHop for Socks5Hop {
    fn kind(&self) -> HopKind {
        self.kind
    }
    fn label(&self) -> &str {
        &self.label
    }
    async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
        // Keep transient transport errors retryable for idempotent reads (SplitHopRouter
        // enforces never retrying submits and never falling back to clearnet).
        run_reqwest(&self.client, &self.closed, req).await
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// Configuration for [`TorHop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorConfig {
    /// Route through an external Tor / Orbot SOCKS5h daemon (`socks5h://127.0.0.1:9050`).
    Socks5 { proxy_url: String },
    /// Run an embedded in-process Arti Tor client (requires `tor-arti` Cargo feature).
    #[cfg(feature = "tor-arti")]
    EmbeddedArti {
        state_dir: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
    },
}

/// Fail-closed native Tor hop supporting both external SOCKS5h and embedded Arti.
pub struct TorHop {
    inner: TorInner,
}

enum TorInner {
    Socks5(Socks5Hop),
    #[cfg(feature = "tor-arti")]
    Arti(Arc<arti_impl::ArtiTorInner>),
}

impl TorHop {
    pub fn from_socks5(proxy_url: &str) -> Result<Self, TransportError> {
        Ok(Self {
            inner: TorInner::Socks5(Socks5Hop::tor_proxy(proxy_url)?),
        })
    }

    pub async fn connect(config: TorConfig) -> Result<Self, TransportError> {
        match config {
            TorConfig::Socks5 { proxy_url } => Self::from_socks5(&proxy_url),
            #[cfg(feature = "tor-arti")]
            TorConfig::EmbeddedArti {
                state_dir,
                cache_dir,
            } => {
                let arti = arti_impl::ArtiTorInner::bootstrap(state_dir, cache_dir).await?;
                Ok(Self {
                    inner: TorInner::Arti(Arc::new(arti)),
                })
            }
        }
    }
}

#[async_trait]
impl NetworkHop for TorHop {
    fn kind(&self) -> HopKind {
        HopKind::Tor
    }
    fn label(&self) -> &str {
        match &self.inner {
            TorInner::Socks5(s) => s.label(),
            #[cfg(feature = "tor-arti")]
            TorInner::Arti(_) => "Tor (Embedded Arti)",
        }
    }
    async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
        match &self.inner {
            TorInner::Socks5(s) => s.exchange(req).await,
            #[cfg(feature = "tor-arti")]
            TorInner::Arti(a) => a.exchange(req).await,
        }
    }
    async fn close(&self) {
        match &self.inner {
            TorInner::Socks5(s) => s.close().await,
            #[cfg(feature = "tor-arti")]
            TorInner::Arti(a) => a.close(),
        }
    }
}

#[cfg(feature = "tor-arti")]
mod arti_impl {
    use super::*;
    use arti_client::{config::TorClientConfigBuilder, IsolationToken, StreamPrefs, TorClient};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{client::conn::http1, Request};
    use hyper_util::rt::TokioIo;
    use std::path::PathBuf;
    use tokio_rustls::{
        rustls::{pki_types::ServerName, ClientConfig, RootCertStore},
        TlsConnector,
    };

    pub struct ArtiTorInner {
        client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
        tls: TlsConnector,
        closed: AtomicBool,
    }

    impl ArtiTorInner {
        pub async fn bootstrap(
            state_dir: PathBuf,
            cache_dir: PathBuf,
        ) -> Result<Self, TransportError> {
            let cfg = TorClientConfigBuilder::from_directories(state_dir, cache_dir)
                .build()
                .map_err(|_| TransportError::Unavailable("arti_config_invalid"))?;
            let client = TorClient::create_bootstrapped(cfg)
                .await
                .map_err(|_| TransportError::FailClosed("arti_bootstrap_failed"))?;
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls_cfg = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Self {
                client,
                tls: TlsConnector::from(Arc::new(tls_cfg)),
                closed: AtomicBool::new(false),
            })
        }

        pub fn close(&self) {
            self.closed.store(true, Ordering::Release);
        }

        pub async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
            if self.closed.load(Ordering::Acquire) {
                return Err(TransportError::Closed);
            }
            let url = validate_http_target(&req.target)?;
            if url.scheme() != "https" {
                return Err(TransportError::InvalidRequest("arti_requires_https"));
            }
            let host = url
                .host_str()
                .ok_or(TransportError::InvalidRequest("target_host_missing"))?
                .to_owned();
            let port = url.port_or_known_default().unwrap_or(443);
            let timeout = Duration::from_millis(req.timeout_ms.clamp(1_000, 120_000));
            tokio::time::timeout(timeout, self.exchange_inner(url, host, port, req))
                .await
                .map_err(|_| TransportError::Timeout)?
        }

        async fn exchange_inner(
            &self,
            url: Url,
            host: String,
            port: u16,
            req: HopRequest,
        ) -> Result<HopResponse, TransportError> {
            // Isolate submit and PIR calls on fresh Tor circuits so requests cannot be linked.
            let mut prefs = StreamPrefs::new();
            if matches!(req.role, crate::HopRole::Submit | crate::HopRole::Pir) {
                prefs.set_isolation(IsolationToken::new());
            }
            let stream = self
                .client
                .connect_with_prefs((host.as_str(), port), &prefs)
                .await
                .map_err(|_| TransportError::Unavailable("arti_connect_failed"))?;
            let server_name = ServerName::try_from(host.clone())
                .map_err(|_| TransportError::InvalidRequest("tls_sni_invalid"))?;
            let tls_stream = self
                .tls
                .connect(server_name, stream)
                .await
                .map_err(|_| TransportError::Unavailable("arti_tls_failed"))?;
            let (mut sender, conn) = http1::handshake(TokioIo::new(tls_stream))
                .await
                .map_err(|_| TransportError::Protocol("arti_http_handshake_failed"))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });

            let path_and_query = match url.query() {
                Some(q) => format!("{}?{}", url.path(), q),
                None => url.path().to_owned(),
            };
            let host_header = if port == 443 {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            let mut builder = Request::builder()
                .method(req.method.as_str())
                .uri(path_and_query)
                .header("host", &host_header);
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let http_req = builder
                .body(Full::new(Bytes::from(req.body)))
                .map_err(|_| TransportError::InvalidRequest("http_build_failed"))?;
            let mut resp = sender
                .send_request(http_req)
                .await
                .map_err(|_| TransportError::FailClosed("arti_http_send_failed"))?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|s| (k.as_str().to_owned(), s.to_owned()))
                })
                .collect();
            let mut body = Vec::new();
            while let Some(frame) = resp
                .body_mut()
                .frame()
                .await
                .transpose()
                .map_err(|_| TransportError::FailClosed("arti_http_read_failed"))?
            {
                if self.closed.load(Ordering::Acquire) {
                    return Err(TransportError::Closed);
                }
                if let Ok(data) = frame.into_data() {
                    if body.len().saturating_add(data.len()) > req.max_response_bytes {
                        return Err(TransportError::SizeLimit);
                    }
                    body.extend_from_slice(&data);
                }
            }
            Ok(HopResponse {
                status,
                headers,
                body,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5h_validation_rejects_local_dns_schemes() {
        assert!(validate_socks5h_proxy("socks5://127.0.0.1:9050").is_err());
        assert!(validate_socks5h_proxy("http://127.0.0.1:8080").is_err());
        assert!(validate_socks5h_proxy("socks5h://127.0.0.1:9050").is_ok());
        assert!(validate_socks5h_proxy("socks5h://127.0.0.1:1080").is_ok());
    }

    #[test]
    fn target_validation_requires_https_or_loopback() {
        assert!(validate_http_target("https://cloud.cofferwallet.com/sync-ohttp").is_ok());
        assert!(validate_http_target("http://127.0.0.1:8137/test").is_ok());
        assert!(validate_http_target("http://example.com/insecure").is_err());
        assert!(validate_http_target("https://user:pass@example.com/").is_err());
    }
}
