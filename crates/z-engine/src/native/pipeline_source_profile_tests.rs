use super::*;
use crate::native::lwd::{LwdChannel, LwdClient};
use http_body_util::BodyExt;
use prost::Message;
use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;
use zcash_client_backend::proto::service::{BlockId, BlockRange, TreeState};
#[derive(Clone)]
struct Peer {
    gate: Arc<tokio::sync::Mutex<()>>,
    entered: Arc<std::sync::atomic::AtomicUsize>,
    tree_entered: Arc<std::sync::atomic::AtomicUsize>,
    tree_served: Arc<std::sync::atomic::AtomicUsize>,
}
fn frame<T: Message>(v: &T) -> Vec<u8> {
    let b = v.encode_to_vec();
    let mut f = vec![0];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend(b);
    f
}
fn reply(bytes: Vec<u8>, code: tonic::Code) -> http::Response<tonic::body::Body> {
    http::Response::builder()
        .header("content-type", "application/grpc")
        .header("grpc-status", (code as i32).to_string())
        .body(tonic::body::Body::new(http_body_util::Full::new(
            bytes::Bytes::from(bytes),
        )))
        .unwrap()
}
fn block(h: u64) -> CompactBlock {
    let mut hash = vec![0; 32];
    hash[..8].copy_from_slice(&h.to_le_bytes());
    let mut prev = vec![0; 32];
    prev[..8].copy_from_slice(&h.saturating_sub(1).to_le_bytes());
    CompactBlock {
        height: h,
        hash,
        prev_hash: prev,
        ..Default::default()
    }
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
    fn call(&mut self, r: http::Request<tonic::body::Body>) -> Self::Future {
        let p = self.clone();
        Box::pin(async move {
            let method = r.uri().path().rsplit('/').next().unwrap().to_owned();
            let wire = r.into_body().collect().await.unwrap().to_bytes();
            let mut out = Vec::new();
            match method.as_str() {
                "GetBlockRange" => {
                    let range = BlockRange::decode(&wire[5..]).unwrap();
                    let start = range.start.unwrap().height;
                    let end = range.end.unwrap().height;
                    p.entered.fetch_add(1, Ordering::SeqCst);
                    let mut first = start;
                    while first <= end {
                        let last = end.min(first + 999);
                        let _owner = p.gate.lock().await;
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        for h in first..=last {
                            out.extend(frame(&block(h)))
                        }
                        first = last + 1;
                    }
                }
                "GetTreeState" => {
                    let id = BlockId::decode(&wire[5..]).unwrap();
                    if !id.hash.is_empty() {
                        return Ok(reply(vec![], tonic::Code::Unimplemented));
                    }
                    p.tree_entered.fetch_add(1, Ordering::SeqCst);
                    let _owner = p.gate.lock().await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let hash = block(id.height).hash.into_iter().rev().collect::<Vec<_>>();
                    let state = TreeState {
                        network: "main".into(),
                        height: id.height,
                        hash: crate::web::to_hex(&hash),
                        sapling_tree: "000000".into(),
                        orchard_tree: "000000".into(),
                        ironwood_tree: "000000".into(),
                        ..Default::default()
                    };
                    out = frame(&state);
                    p.tree_served.fetch_add(1, Ordering::SeqCst);
                }
                "GetBlock" => {
                    let id = BlockId::decode(&wire[5..]).unwrap();
                    let _owner = p.gate.lock().await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    out = frame(&block(id.height));
                }
                _ => panic!("diagnostic_method_refused"),
            };
            Ok(reply(out, tonic::Code::Ok))
        })
    }
}
struct Outcome {
    passed: bool,
    transport_error: bool,
    tree_served: usize,
    elapsed: Duration,
}

// Socket-free serialized public-read model. The real FetchPool and tree helper
// keep their production deadlines. The peer buffers an entire two-chunk RPC
// rather than streaming its first chunk, so this proves the scheduling contract,
// not equivalence to every detail of a production transport.
async fn exercise(local: bool, label: &str) -> Outcome {
    let peer = Peer {
        gate: Arc::new(tokio::sync::Mutex::new(())),
        entered: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tree_entered: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tree_served: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let connector_peer = peer.clone();
    let handles = connections.clone();
    let channel = tonic::transport::Endpoint::from_static("http://coffer.invalid")
        .connect_with_connector(tower::service_fn(move |_| {
            let p = connector_peer.clone();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let serving = tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |r: http::Request<hyper::body::Incoming>| {
                        let mut p = p.clone();
                        async move {
                            let (parts, body) = r.into_parts();
                            p.call(http::Request::from_parts(
                                parts,
                                tonic::body::Body::new(body),
                            ))
                            .await
                        }
                    });
                let _ =
                    hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(hyper_util::rt::TokioIo::new(server), service)
                        .await;
            });
            handles.lock().unwrap().push(serving);
            async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(client)) }
        }))
        .await
        .unwrap();
    let client = LwdClient::new(LwdChannel::new(channel, false));
    let chunk = apply_chunk_width(local);
    let prefetch = initial_prefetch(local).max(sync_tuning(local).prefetch as usize);
    let ranges = (0..prefetch as u32)
        .map(|i| {
            ScanRange::from_parts(
                BlockHeight::from_u32(1000 + i * chunk)
                    ..BlockHeight::from_u32(1000 + (i + 1) * chunk),
                ScanPriority::Historic,
            )
        })
        .collect();
    let mut pool = FetchPool::start(
        "authenticated public-data source",
        false,
        true,
        &client,
        Arc::new(AtomicU32::new(0)),
        prefetch,
        Some(ranges),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while peer.entered.load(Ordering::SeqCst) < prefetch {
            tokio::task::yield_now().await
        }
    })
    .await
    .expect("all production prefetch RPCs must arrive before the control probe");
    let at = std::time::Instant::now();
    let mut control = client.clone();
    let result =
        crate::native::tree_state::fetch(&mut control, 999, Some(crate::Network::Mainnet)).await;
    let elapsed = at.elapsed();
    pool.drain();
    drop(pool);
    drop(control);
    drop(client);
    let tasks = std::mem::take(&mut *connections.lock().unwrap());
    for t in tasks {
        t.abort();
        let _ = t.await;
    }
    let census = serde_json::json!({"label":label,"passed":result.is_ok(),"selectedLocalProfile":local,"chunkBlocks":chunk,"prefetchCount":prefetch,"enteredBulkRpcs":peer.entered.load(Ordering::SeqCst),"enteredHeightTreeRpcs":peer.tree_entered.load(Ordering::SeqCst),"servedHeightTreeRpcs":peer.tree_served.load(Ordering::SeqCst),"controlElapsedMs":elapsed.as_millis(),"perSerializedPublicChunkDelayMs":3000,"treeRpcDeadlineMs":15000,"treeTotalDeadlineMs":45000,"actualProductionFetchPoolAndTreeHelper":true,"modelBuffersTwoChunkReplyUntilComplete":true,"publicNetworkUsed":false,"walletOpened":false,"productionDeadlinesOverriddenByFixture":false});
    if let Ok(dir) = std::env::var("DIAGNOSTIC_REPORT_DIR") {
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{label}.json")),
            serde_json::to_vec_pretty(&census).unwrap(),
        )
        .unwrap();
    }
    Outcome {
        passed: result.is_ok(),
        transport_error: matches!(&result, Err(EngineError::Transport(_))),
        tree_served: peer.tree_served.load(Ordering::SeqCst),
        elapsed,
    }
}

#[test]
fn source_selection_preserves_direct_tuning_and_never_uses_supplied_labels() {
    use NativeSyncProfile::{ConservativeRemote, Dedicated};
    for label in [
        "authenticated public-data source",
        "http://127.0.0.1:8138",
        "https://zec.rocks:443",
        "opaque custom channel",
    ] {
        assert_eq!(
            NativeSyncProfile::for_source(true, label),
            ConservativeRemote
        );
        let local = NativeSyncProfile::for_source(true, label).local_tuning();
        assert_eq!(apply_chunk_width(local), 1_000);
        assert_eq!(initial_prefetch(local), 2);
        assert_eq!(sync_tuning(local).batch, 1_000);
        assert_eq!(sync_tuning(local).prefetch, 2);
    }
    for (url, expected) in [
        ("http://127.0.0.1:8138", Dedicated),
        ("http://203.0.113.9:8137", Dedicated),
        ("https://zec.rocks:443", ConservativeRemote),
        ("https://na.zec.rocks:443", ConservativeRemote),
    ] {
        let profile = NativeSyncProfile::for_source(false, url);
        assert_eq!(profile, expected);
        assert_eq!(profile.local_tuning(), crate::uses_fast_sync(url));
        let local = profile.local_tuning();
        let (chunk, prefetch, batch) = if expected == Dedicated {
            (2_000, 6, 4_000)
        } else {
            (1_000, 2, 1_000)
        };
        assert_eq!(apply_chunk_width(local), chunk);
        assert_eq!(initial_prefetch(local), prefetch);
        assert_eq!(sync_tuning(local).batch, batch);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_channel_profile_keeps_control_tree_within_existing_budget() {
    let local =
        NativeSyncProfile::for_source(true, "authenticated public-data source").local_tuning();
    assert!(!local);
    let outcome = exercise(local, "supplied-profile-green").await;
    assert!(outcome.passed, "required_control_tree_must_complete");
    assert_eq!(outcome.tree_served, 1);
    assert!(outcome.elapsed < Duration::from_secs(15));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_bulk_profile_is_a_negative_control_for_serialized_transport() {
    let outcome = exercise(true, "old-bulk-profile-negative-control").await;
    assert!(!outcome.passed);
    assert!(outcome.transport_error);
    assert_eq!(outcome.tree_served, 0);
    assert!(outcome.elapsed >= Duration::from_secs(15));
}
