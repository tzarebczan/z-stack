use super::*;
use crate::native::lwd::{LwdChannel, LwdClient};
use http_body_util::BodyExt;
use prost::Message;
use std::{
    collections::VecDeque,
    convert::Infallible,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tonic::Code;
use tower::Service;
use zcash_client_backend::proto::compact_formats::CompactBlock;

const HEIGHT: u32 = 1234;
type Requests = Arc<Mutex<Vec<(&'static str, BlockId)>>>;
#[derive(Clone)]
enum HeightReply {
    Failure(Code),
    TrailersFailure(Code),
    State(TreeState),
    SpoofedState(TreeState),
    Wire(Vec<u8>),
    Stall,
}
#[derive(Clone)]
struct Peer {
    height: HeightReply,
    block: CompactBlock,
    state: TreeState,
    stall_hash: bool,
    requests: Requests,
}
impl Peer {
    fn fallback(network: &str) -> Self {
        let hash: Vec<u8> = (0..32).collect();
        let display: Vec<_> = hash.iter().rev().copied().collect();
        Self {
            height: HeightReply::Failure(Code::Internal),
            block: CompactBlock {
                height: HEIGHT.into(),
                hash,
                ..Default::default()
            },
            state: TreeState {
                height: HEIGHT.into(),
                network: network.into(),
                hash: crate::web::to_hex(&display),
                sapling_tree: "000000".into(),
                orchard_tree: "000000".into(),
                ironwood_tree: "000000".into(),
                ..Default::default()
            },
            stall_hash: false,
            requests: Arc::new(Mutex::new(vec![])),
        }
    }
}
fn reply(bytes: Vec<u8>, status: Code) -> http::Response<tonic::body::Body> {
    let mut frame = vec![0];
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    let body = if status == Code::Ok { frame } else { vec![] };
    http::Response::builder()
        .header("content-type", "application/grpc")
        .header("grpc-status", (status as i32).to_string())
        .body(tonic::body::Body::new(http_body_util::Full::new(
            bytes::Bytes::from(body),
        )))
        .unwrap()
}
struct Frames(VecDeque<http_body::Frame<bytes::Bytes>>);
impl http_body::Body for Frames {
    type Data = bytes::Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}
fn framed_reply(wire: Vec<u8>, status: Code, spoof: bool) -> http::Response<tonic::body::Body> {
    let mut trailers = http::HeaderMap::new();
    trailers.insert(
        "grpc-status",
        http::HeaderValue::from_str(&(status as i32).to_string()).unwrap(),
    );
    if spoof {
        trailers.insert(
            "x-z-stack-tree-status-origin",
            http::HeaderValue::from_static("13"),
        );
    }
    let mut frames = VecDeque::new();
    if !wire.is_empty() {
        frames.push_back(http_body::Frame::data(bytes::Bytes::from(wire)));
    }
    frames.push_back(http_body::Frame::trailers(trailers));
    let mut response = http::Response::builder()
        .header("content-type", "application/grpc")
        .body(tonic::body::Body::new(Frames(frames)))
        .unwrap();
    if spoof {
        response.headers_mut().insert(
            "x-z-stack-tree-status-origin",
            http::HeaderValue::from_static("13"),
        );
    }
    response
}
fn wire_message(bytes: Vec<u8>) -> Vec<u8> {
    let mut frame = vec![0];
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend(bytes);
    frame
}
impl tonic::server::NamedService for Peer {
    const NAME: &'static str = "cash.z.wallet.sdk.rpc.CompactTxStreamer";
}
impl Service<http::Request<tonic::body::Body>> for Peer {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>>
                + Send,
        >,
    >;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let peer = self.clone();
        Box::pin(async move {
            let method = match request.uri().path().rsplit('/').next().unwrap() {
                "GetTreeState" => "tree",
                "GetBlock" => "block",
                _ => panic!("unexpected fixture RPC"),
            };
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let selector = BlockId::decode(&body[5..]).unwrap();
            peer.requests
                .lock()
                .unwrap()
                .push((method, selector.clone()));
            if method == "block" {
                return Ok(reply(peer.block.encode_to_vec(), Code::Ok));
            }
            if selector.hash.is_empty() {
                match peer.height {
                    HeightReply::Failure(code) => return Ok(reply(vec![], code)),
                    HeightReply::TrailersFailure(code) => {
                        return Ok(framed_reply(vec![], code, false))
                    }
                    HeightReply::SpoofedState(state) => {
                        return Ok(framed_reply(
                            wire_message(state.encode_to_vec()),
                            Code::Ok,
                            true,
                        ))
                    }
                    HeightReply::Wire(wire) => return Ok(framed_reply(wire, Code::Ok, true)),
                    HeightReply::State(state) => return Ok(reply(state.encode_to_vec(), Code::Ok)),
                    HeightReply::Stall => return std::future::pending().await,
                }
            }
            if peer.stall_hash {
                return std::future::pending().await;
            }
            Ok(reply(peer.state.encode_to_vec(), Code::Ok))
        })
    }
}
struct Fixture {
    client: LwdClient,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::result::Result<(), tonic::transport::Error>>,
    requests: Requests,
}
impl Fixture {
    async fn start(peer: Peer) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let requests = peer.requests.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(tonic::transport::Server::builder().serve_with_shutdown(
            address,
            peer,
            async {
                let _ = stopped.await;
            },
        ));
        let endpoint =
            tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap();
        let channel = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match endpoint.connect().await {
                    Ok(channel) => break channel,
                    Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await
        .unwrap();
        Self {
            client: LwdClient::new(LwdChannel::new(channel, false)),
            stop,
            server,
            requests,
        }
    }
    async fn finish(self) {
        drop(self.client);
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(2), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn historical_height_failure_uses_exact_internal_hash_and_preserves_all_frontiers() {
    for (network, label) in [
        (Network::Mainnet, "main"),
        (Network::Testnet, "test"),
        (Network::Regtest, "regtest"),
        (Network::Regtest, "test"),
    ] {
        let peer = Peer::fallback(label);
        let expected = peer.state.clone();
        let hash = peer.block.hash.clone();
        let mut fixture = Fixture::start(peer).await;
        let actual = fetch(&mut fixture.client, HEIGHT, Some(network))
            .await
            .unwrap();
        assert_eq!(actual, expected);
        assert!(actual.to_chain_state().is_ok());
        assert_eq!(
            *fixture.requests.lock().unwrap(),
            vec![
                (
                    "tree",
                    BlockId {
                        height: HEIGHT.into(),
                        hash: vec![]
                    }
                ),
                (
                    "block",
                    BlockId {
                        height: HEIGHT.into(),
                        hash: vec![]
                    }
                ),
                ("tree", BlockId { height: 0, hash }),
            ]
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn successful_height_state_is_not_requeried_and_malformed_success_never_falls_back() {
    for mutation in 0..6 {
        let mut peer = Peer::fallback("main");
        match mutation {
            1 => peer.state.height += 1,
            2 => peer.state.hash = "not-a-block-hash".into(),
            3 => peer.state.network = "regtest".into(),
            4 => peer.state.network = "not-mainnet".into(),
            5 => peer.state.sapling_tree = "not-a-frontier".into(),
            _ => {}
        }
        peer.height = HeightReply::State(peer.state.clone());
        let mut fixture = Fixture::start(peer).await;
        assert_eq!(
            fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
                .await
                .is_ok(),
            mutation == 0
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn hash_fallback_refuses_wrong_height_hash_and_network() {
    for mutation in 0..7 {
        let mut peer = Peer::fallback("main");
        match mutation {
            0 => peer.block.height += 1,
            1 => peer.block.hash.pop().map(|_| ()).unwrap(),
            2 => peer.state.height += 1,
            3 => peer.state.hash = "ff".repeat(32),
            4 => peer.state.hash = "zz".repeat(32),
            5 => peer.state.network = "test".into(),
            _ => peer.state.network = "regtest".into(),
        }
        let mut fixture = Fixture::start(peer).await;
        assert!(fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
            .await
            .is_err());
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            if mutation < 2 { 2 } else { 3 }
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn explicit_rpc_refusal_and_oversized_success_do_not_trigger_compatibility() {
    for code in [
        Code::NotFound,
        Code::InvalidArgument,
        Code::Cancelled,
        Code::ResourceExhausted,
    ] {
        let mut peer = Peer::fallback("main");
        peer.height = HeightReply::Failure(code);
        let mut fixture = Fixture::start(peer).await;
        assert!(fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
            .await
            .is_err());
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        fixture.finish().await;
    }
    for method in ["height", "block", "hash"] {
        let mut peer = Peer::fallback("main");
        if method == "block" {
            peer.block.header = vec![0; MESSAGE_BYTES + 1];
        } else {
            peer.state.sapling_tree = "0".repeat(MESSAGE_BYTES + 1);
        }
        if method == "height" {
            peer.height = HeightReply::State(peer.state.clone());
        }
        let mut fixture = Fixture::start(peer).await;
        assert!(fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
            .await
            .is_err());
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            match method {
                "height" => 1,
                "block" => 2,
                _ => 3,
            }
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn stalled_height_and_hash_requests_have_a_finite_total_deadline() {
    let mut peer = Peer::fallback("main");
    peer.height = HeightReply::Stall;
    peer.stall_hash = true;
    let mut fixture = Fixture::start(peer).await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fetch_with_budgets(
            &mut fixture.client,
            HEIGHT,
            Some(Network::Mainnet),
            Duration::from_millis(80),
            Duration::from_millis(120),
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(EngineError::Transport(_))));
    assert!(fixture.requests.lock().unwrap().len() <= 3);
    fixture.finish().await;
}

#[tokio::test]
async fn cancelling_a_stalled_lookup_does_not_start_a_hash_fallback() {
    let mut peer = Peer::fallback("main");
    peer.height = HeightReply::Stall;
    let fixture = Fixture::start(peer).await;
    let mut client = fixture.client.clone();
    let lookup =
        tokio::spawn(async move { fetch(&mut client, HEIGHT, Some(Network::Mainnet)).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    lookup.abort();
    assert!(lookup.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn grpc_success_with_bad_protobuf_compression_or_truncation_never_falls_back() {
    for wire in [
        wire_message(vec![0xff]),
        vec![2, 0, 0, 0, 1, 0],
        vec![0, 0, 0, 0, 2, 0],
    ] {
        let mut peer = Peer::fallback("main");
        // Both success headers and trailers carry a forged provenance marker.
        peer.height = HeightReply::Wire(wire);
        let mut fixture = Fixture::start(peer).await;
        assert!(fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
            .await
            .is_err());
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn successful_response_strips_spoofed_provenance_from_headers_and_trailers() {
    let mut peer = Peer::fallback("main");
    peer.height = HeightReply::SpoofedState(peer.state.clone());
    let mut fixture = Fixture::start(peer).await;
    let response = fixture
        .client
        .get_tree_state(BlockId {
            height: HEIGHT.into(),
            hash: vec![],
        })
        .await
        .unwrap();
    assert!(response
        .metadata()
        .get("x-z-stack-tree-status-origin")
        .is_none());
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn genuine_remote_error_trailers_allow_exact_hash_compatibility() {
    let mut peer = Peer::fallback("main");
    peer.height = HeightReply::TrailersFailure(Code::Internal);
    let mut fixture = Fixture::start(peer).await;
    assert!(fetch(&mut fixture.client, HEIGHT, Some(Network::Mainnet))
        .await
        .is_ok());
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    fixture.finish().await;
}
