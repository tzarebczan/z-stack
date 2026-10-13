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

const TREE_STATUS_ORIGIN: &str = "x-z-stack-tree-status-origin";

fn eligible_status(code: tonic::Code) -> Option<&'static str> {
    match code {
        tonic::Code::Internal => Some("13"),
        tonic::Code::Unavailable => Some("14"),
        tonic::Code::Unknown => Some("2"),
        tonic::Code::DeadlineExceeded => Some("4"),
        _ => None,
    }
}

pub(super) fn remote_tree_status(status: &tonic::Status) -> bool {
    eligible_status(status.code()).is_some_and(|code| {
        status
            .metadata()
            .get(TREE_STATUS_ORIGIN)
            .is_some_and(|marker| marker.as_encoded_bytes() == code.as_bytes())
    })
}

// Tonic uses INTERNAL for both remote errors and local protobuf/framing errors.
// Only an observed remote status can authorize the tree-state compatibility
// branch. Never trust the marker supplied by the server itself.
fn mark_tree_status(headers: &mut http::HeaderMap) {
    headers.remove(TREE_STATUS_ORIGIN);
    let code = match headers.get("grpc-status").map(|value| value.as_bytes()) {
        Some(b"13") => "13",
        Some(b"14") => "14",
        Some(b"2") => "2",
        Some(b"4") => "4",
        _ => return,
    };
    headers.insert(TREE_STATUS_ORIGIN, http::HeaderValue::from_static(code));
}

struct TreeStatusBody(Body);
impl http_body::Body for TreeStatusBody {
    type Data = Bytes;
    type Error = tonic::Status;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
        match Pin::new(&mut self.0).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(match frame.into_trailers() {
                Ok(mut trailers) => {
                    mark_tree_status(&mut trailers);
                    Frame::trailers(trailers)
                }
                Err(frame) => frame,
            }))),
            other => other,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct LwdChannel {
    inner: Channel,
    skip_ironwood_subtrees: bool,
    read_only: bool,
}

impl LwdChannel {
    pub fn new(inner: Channel, skip_ironwood_subtrees: bool) -> Self {
        Self {
            inner,
            skip_ironwood_subtrees,
            read_only: false,
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
        let read_only = self.read_only;
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
            if read_only
                && !matches!(
                    path.as_str(),
                    "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo"
                        | "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock"
                        | "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTreeState"
                        | "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock"
                        | "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange"
                        | "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetSubtreeRoots"
                )
            {
                return Err(super::source::refused());
            }
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
                if !read_only && ironwood && grpc_invalid_argument(&resp) {
                    tracing::warn!(
                        "GetSubtreeRoots(Ironwood) rejected by light server; treating as empty"
                    );
                    return Ok(empty_grpc_ok());
                }
                return Ok(resp);
            }
            let response = ready_call(&mut inner, req).await?;
            if path.ends_with("/GetTreeState") {
                let (mut parts, body) = response.into_parts();
                mark_tree_status(&mut parts.headers);
                return Ok(http::Response::from_parts(
                    parts,
                    Body::new(TreeStatusBody(body)),
                ));
            }
            Ok(response)
        })
    }
}

pub fn client(channel: Channel, skip_ironwood_subtrees: bool) -> LwdClient {
    CompactTxStreamerClient::new(LwdChannel::new(channel, skip_ironwood_subtrees))
}

pub(super) fn read_only_client(channel: Channel) -> LwdClient {
    let mut channel = LwdChannel::new(channel, false);
    channel.read_only = true;
    CompactTxStreamerClient::new(channel)
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
