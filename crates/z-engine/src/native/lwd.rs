//! Lightwalletd / Zaino gRPC channel.
//!
//! Zakura `sync::run` always requests `GetSubtreeRoots(Ironwood=2)`. Zaino 0.8's
//! lightwallet-protocol enum is still sapling|orchard (`Invalid shielded protocol
//! value`). Loopback/regtest answer that RPC locally with an empty stream.
//! Public LWD that speaks Ironwood is left alone; a late InvalidArgument still
//! falls back to empty.

use crate::error::{EngineError, Result as EngineResult};
use bytes::Bytes;
use http_body::Frame;
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::body::Body;
use tonic::transport::Channel;
use tower::Service;
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

pub type LwdClient = CompactTxStreamerClient<LwdChannel>;

#[derive(Clone)]
pub struct LwdChannel {
    inner: Channel,
    skip_ironwood_subtrees: bool,
}

impl LwdChannel {
    pub fn new(inner: Channel, skip_ironwood_subtrees: bool) -> Self {
        Self {
            inner,
            skip_ironwood_subtrees,
        }
    }
}

fn grpc_payload(body: &[u8]) -> Option<&[u8]> {
    if body.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes(body[1..5].try_into().ok()?) as usize;
    body.get(5..5 + len)
}

fn is_ironwood_subtree_roots(path: &str, grpc_body: &[u8]) -> bool {
    if !path.ends_with("/GetSubtreeRoots") {
        return false;
    }
    let Some(payload) = grpc_payload(grpc_body) else {
        return false;
    };
    // GetSubtreeRootsArg field 2 (enum) wire type 0 → tag 0x10, Ironwood = 2.
    payload.windows(2).any(|w| w == [0x10, 0x02])
}

struct GrpcOkEmpty {
    sent: bool,
}

impl http_body::Body for GrpcOkEmpty {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        if self.sent {
            return Poll::Ready(None);
        }
        self.sent = true;
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        Poll::Ready(Some(Ok(Frame::trailers(trailers))))
    }

    fn is_end_stream(&self) -> bool {
        self.sent
    }
}

fn grpc_invalid_argument(resp: &http::Response<Body>) -> bool {
    resp.headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        == Some("3")
}

fn empty_grpc_ok() -> http::Response<Body> {
    http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(Body::new(GrpcOkEmpty { sent: false }))
        .expect("empty grpc response")
}

impl Service<http::Request<Body>> for LwdChannel {
    type Response = http::Response<Body>;
    type Error = EngineError;
    type Future = Pin<Box<dyn std::future::Future<Output = EngineResult<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<EngineResult<()>> {
        self.inner
            .poll_ready(cx)
            .map_err(|e| EngineError::Transport(format!("lwd ready: {e}")))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let skip = self.skip_ironwood_subtrees;
        Box::pin(async move {
            async fn ready_call(
                inner: &mut Channel,
                req: http::Request<Body>,
            ) -> EngineResult<http::Response<Body>> {
                std::future::poll_fn(|cx| Service::poll_ready(inner, cx))
                    .await
                    .map_err(|e| EngineError::Transport(format!("lwd ready: {e}")))?;
                inner
                    .call(req)
                    .await
                    .map_err(|e| EngineError::Transport(format!("lwd: {e}")))
            }

            let path = req.uri().path().to_string();
            let is_subtrees = path.ends_with("/GetSubtreeRoots");
            if is_subtrees {
                let (parts, body) = req.into_parts();
                let collected = http_body_util::BodyExt::collect(body)
                    .await
                    .map_err(|e| EngineError::Transport(format!("lwd body: {e}")))?;
                let bytes = collected.to_bytes();
                let ironwood = is_ironwood_subtree_roots(&path, &bytes);
                if skip && ironwood {
                    tracing::debug!("answering GetSubtreeRoots(Ironwood) with empty stream");
                    return Ok(empty_grpc_ok());
                }
                let rebuilt =
                    http::Request::from_parts(parts, Body::new(http_body_util::Full::new(bytes)));
                let resp = ready_call(&mut inner, rebuilt).await?;
                if ironwood && grpc_invalid_argument(&resp) {
                    tracing::warn!(
                        "GetSubtreeRoots(Ironwood) rejected by light server; treating as empty"
                    );
                    return Ok(empty_grpc_ok());
                }
                return Ok(resp);
            }
            ready_call(&mut inner, req).await
        })
    }
}

pub fn client(channel: Channel, skip_ironwood_subtrees: bool) -> LwdClient {
    CompactTxStreamerClient::new(LwdChannel::new(channel, skip_ironwood_subtrees))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_status_3_is_invalid_argument() {
        let resp = http::Response::builder()
            .header("grpc-status", "3")
            .body(Body::new(GrpcOkEmpty { sent: true }))
            .unwrap();
        assert!(grpc_invalid_argument(&resp));
        let ok = http::Response::builder()
            .header("grpc-status", "0")
            .body(Body::new(GrpcOkEmpty { sent: true }))
            .unwrap();
        assert!(!grpc_invalid_argument(&ok));
    }

    #[test]
    fn detects_ironwood_tag() {
        let mut frame = vec![0, 0, 0, 0, 2, 0x10, 0x02];
        frame[4] = 2;
        assert!(is_ironwood_subtree_roots(
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetSubtreeRoots",
            &frame
        ));
        frame[6] = 1;
        assert!(!is_ironwood_subtree_roots(
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetSubtreeRoots",
            &frame
        ));
    }
}
