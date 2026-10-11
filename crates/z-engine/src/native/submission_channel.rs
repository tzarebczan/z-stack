//! Submission clients must not reconnect after their identity was checked.
use super::lwd::{self, LwdClient};
use crate::error::{EngineError, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tonic::transport::{ClientTlsConfig, Endpoint};

pub(super) async fn connect(url: &str, skip_ironwood_subtrees: bool) -> Result<LwdClient> {
    let url = crate::normalize_grpc_url(url);
    let mut endpoint = Endpoint::from_shared(url.clone())
        .map_err(|_| EngineError::Transport("submission URL invalid".into()))?
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .tcp_nodelay(true);
    if url.starts_with("https://") {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_webpki_roots())
            .map_err(|_| EngineError::Transport("submission TLS configuration failed".into()))?;
    }
    let attempted = Arc::new(AtomicBool::new(false));
    // Keep tonic's IPv6/IPv4 Happy Eyeballs behavior, TLS wrapper and bounded
    // connect timeout. Only later reconnects are denied.
    let mut dial = hyper_util::client::legacy::connect::HttpConnector::new();
    dial.enforce_http(false);
    dial.set_nodelay(true);
    dial.set_connect_timeout(Some(Duration::from_secs(15)));
    let connector = tower::service_fn(move |uri: http::Uri| {
        let admitted = !attempted.swap(true, Ordering::SeqCst);
        let mut dial = dial.clone();
        async move {
            if !admitted {
                return Err(std::io::Error::other(
                    "verified submission connection closed",
                ));
            }
            use tower::Service;
            dial.call(uri)
                .await
                .map_err(|_| std::io::Error::other("submission connection failed"))
        }
    });
    let channel = endpoint
        .connect_with_connector(connector)
        .await
        .map_err(|_| EngineError::Transport("submission connection failed".into()))?;
    Ok(lwd::client(channel, skip_ironwood_subtrees))
}
