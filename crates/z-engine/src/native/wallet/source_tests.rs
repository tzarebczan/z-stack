//! Public vectors and memory-only HTTP/2 transport; no wallet TCP listener.
use super::*;
use bytes::Bytes;
use http_body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use zcash_client_backend::proto::{
    compact_formats::{ChainMetadata, CompactBlock},
    service::{BlockRange, LightdInfo, TreeState},
};

const PUBLIC_SEED: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

struct Reply(VecDeque<Frame<Bytes>>);
impl http_body::Body for Reply {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}
fn reply(messages: Vec<Vec<u8>>) -> http::Response<Reply> {
    let mut frames = VecDeque::new();
    for message in messages {
        let mut bytes = vec![0];
        bytes.extend_from_slice(&(message.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&message);
        frames.push_back(Frame::data(Bytes::from(bytes)));
    }
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
    frames.push_back(Frame::trailers(trailers));
    http::Response::builder()
        .header("content-type", "application/grpc")
        .body(Reply(frames))
        .unwrap()
}
fn block(height: u64) -> CompactBlock {
    CompactBlock {
        height,
        hash: vec![height as u8; 32],
        prev_hash: vec![height.saturating_sub(1) as u8; 32],
        time: 1,
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        }),
        ..Default::default()
    }
}
struct Fixture {
    source: super::super::NativeReadOnlySource,
    calls: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicU32>,
    hang_ranges: Arc<AtomicBool>,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}
impl Fixture {
    async fn new(network: ZNetwork, reported: &'static str, tip: u64) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicU32::new(0));
        let hang_ranges = Arc::new(AtomicBool::new(false));
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let (c, h, t, n) = (
            calls.clone(),
            hang_ranges.clone(),
            tasks.clone(),
            connections.clone(),
        );
        let connector = tower::service_fn(move |_: http::Uri| {
            let (c, h, t, n) = (c.clone(), h.clone(), t.clone(), n.clone());
            async move {
                n.fetch_add(1, Ordering::Relaxed);
                let (client, server) = tokio::io::duplex(1024 * 1024);
                let service = hyper::service::service_fn(
                    move |request: http::Request<hyper::body::Incoming>| {
                        let (c, h) = (c.clone(), h.clone());
                        async move {
                            let method =
                                request.uri().path().rsplit('/').next().unwrap().to_owned();
                            c.lock().unwrap().push(method.clone());
                            let bytes = http_body_util::BodyExt::collect(request.into_body())
                                .await
                                .unwrap()
                                .to_bytes();
                            let payload = &bytes[5..];
                            let response = match method.as_str() {
                                "GetLightdInfo" => reply(vec![LightdInfo {
                                    chain_name: reported.into(),
                                    block_height: tip,
                                    ..Default::default()
                                }
                                .encode_to_vec()]),
                                "GetLatestBlock" => reply(vec![BlockId {
                                    height: tip,
                                    hash: vec![tip as u8; 32],
                                }
                                .encode_to_vec()]),
                                "GetTreeState" => {
                                    let id = BlockId::decode(payload).unwrap();
                                    reply(vec![TreeState {
                                        network: reported.into(),
                                        height: id.height,
                                        hash: format!("{:02x}", id.height as u8).repeat(32),
                                        time: 1,
                                        sapling_tree: "000000".into(),
                                        orchard_tree: "000000".into(),
                                        ironwood_tree: "000000".into(),
                                    }
                                    .encode_to_vec()])
                                }
                                "GetBlock" => {
                                    reply(vec![block(BlockId::decode(payload).unwrap().height)
                                        .encode_to_vec()])
                                }
                                "GetBlockRange" => {
                                    if h.load(Ordering::Acquire) {
                                        std::future::pending::<()>().await;
                                    }
                                    let range = BlockRange::decode(payload).unwrap();
                                    assert!(range.pool_types.is_empty());
                                    reply(
                                        (range.start.unwrap().height..=range.end.unwrap().height)
                                            .map(|height| block(height).encode_to_vec())
                                            .collect(),
                                    )
                                }
                                "GetSubtreeRoots" => reply(Vec::new()),
                                _ => panic!("forbidden RPC reached source fixture"),
                            };
                            Ok::<_, Infallible>(response)
                        }
                    },
                );
                let task = tokio::spawn(async move {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(server), service)
                        .await;
                });
                t.lock().unwrap().push(task);
                Ok::<_, std::io::Error>(TokioIo::new(client))
            }
        });
        let channel =
            tonic::transport::Endpoint::from_static("http://authenticated-source.invalid")
                .connect_with_connector(connector)
                .await
                .unwrap();
        Self {
            source: super::super::NativeReadOnlySource::new(channel, network),
            calls,
            connections,
            hang_ranges,
            tasks,
        }
    }
}
fn auth() -> SeedAuth {
    SeedAuth::passphrase("public-vector-fixture-only")
}
fn code<T>(result: Result<T>, expected: &str) {
    assert!(matches!(result,Err(EngineError::Message(ref message)) if message == expected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provided_channel_restores_scans_and_cold_opens_without_private_rpc_or_url_fallback() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (mut wallet, _) =
        NativeWallet::restore_with_source(root.path(), PUBLIC_SEED, &fixture.source, 1, auth(), 0)
            .await
            .unwrap();
    // Stored endpoints are inert even if edited before source sync.
    wallet
        .set_light_server(LightServer::parse(
            "http://direct-fallback.invalid:1",
            ZNetwork::Regtest,
        ))
        .unwrap();
    wallet
        .set_validator_rpc(Some("http://direct-fallback.invalid:2".into()))
        .unwrap();
    let (height, _) = tokio::time::timeout(
        Duration::from_secs(15),
        wallet.sync_with_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(height, 2);
    assert_eq!(fixture.connections.load(Ordering::Relaxed), 1);
    let calls = fixture.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|method| method == "GetBlockRange"));
    assert!(calls.iter().any(|method| method == "GetSubtreeRoots"));
    for method in calls {
        assert!(matches!(
            method.as_str(),
            "GetLightdInfo"
                | "GetLatestBlock"
                | "GetTreeState"
                | "GetBlock"
                | "GetBlockRange"
                | "GetSubtreeRoots"
        ));
    }
    let before = fixture.calls.lock().unwrap().len();
    let cold = NativeWallet::open(root.path()).unwrap();
    assert!(cold.validator_rpc_url().is_none());
    code(cold.sync_reported(None).await, "native_read_only_source");
    code(
        cold.enhance_memos(&mut cold.open_db().unwrap()).await,
        "native_read_only_source",
    );
    code(cold.max_send(None), "native_read_only_source");
    code(
        cold.prepare_payment("irrelevant", 1, None),
        "native_read_only_source",
    );
    code(
        cold.sign_payment(&auth(), "irrelevant"),
        "native_read_only_source",
    );
    code(
        cold.submit_payment("irrelevant").await,
        "native_read_only_source",
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
    let mut client = fixture.source.client();
    assert!(client.get_transaction(TxFilter::default()).await.is_err());
    assert!(client
        .send_transaction(RawTransaction::default())
        .await
        .is_err());
    assert!(client
        .get_address_utxos(GetAddressUtxosArg::default())
        .await
        .is_err());
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_create_uses_explicit_mainnet_channel_and_rejects_chain_mismatch() {
    let fixture = Fixture::new(ZNetwork::Mainnet, "main", 419_201).await;
    let root = tempfile::tempdir().unwrap();
    let (wallet, created) =
        NativeWallet::create_with_source(root.path(), &fixture.source, Some(419_201), auth(), 0)
            .await
            .unwrap();
    assert_eq!(created.birthday_height, 419_201);
    assert_eq!(wallet.network, ZNetwork::Mainnet);
    code(wallet.max_send(None), "native_read_only_source");
    let mismatch = Fixture::new(ZNetwork::Mainnet, "regtest", 2).await;
    let other = tempfile::tempdir().unwrap();
    assert!(NativeWallet::restore_with_source(
        other.path(),
        PUBLIC_SEED,
        &mismatch.source,
        1,
        auth(),
        0
    )
    .await
    .is_err());
    assert_eq!(*mismatch.calls.lock().unwrap(), ["GetLightdInfo"]);
    code(
        NativeWallet::restore(
            other.path(),
            PUBLIC_SEED,
            ZNetwork::Regtest,
            None,
            1,
            auth(),
            0,
        )
        .await,
        "native_read_only_source",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_enrollment_blocks_stale_handles_and_malformed_policy_without_database_writes() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let stale = tests::fixture_wallet(root.path());
    rusqlite::Connection::open(&stale.paths.data_db)
        .unwrap()
        .execute_batch("CREATE TABLE fixture(value); INSERT INTO fixture VALUES (1)")
        .unwrap();
    let enrolled = NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    let before = std::fs::read(&stale.paths.data_db).unwrap();
    code(
        stale.sign_payment(&auth(), "irrelevant"),
        "native_read_only_source",
    );
    code(stale.sync_reported(None).await, "native_read_only_source");
    assert_eq!(std::fs::read(&stale.paths.data_db).unwrap(), before);
    assert!(fixture.calls.lock().unwrap().is_empty());
    std::fs::write(root.path().join("native-source-policy"), b"unknown policy").unwrap();
    code(
        NativeWallet::open(root.path()),
        "native_source_policy_invalid",
    );
    code(
        enrolled.sign_payment(&auth(), "irrelevant"),
        "native_source_policy_invalid",
    );
    assert_eq!(std::fs::read(&stale.paths.data_db).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_cancel_stalled_rpc_preserves_committed_wallet_and_resumes_same_channel() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (wallet, _) =
        NativeWallet::restore_with_source(root.path(), PUBLIC_SEED, &fixture.source, 1, auth(), 0)
            .await
            .unwrap();
    fixture.hang_ranges.store(true, Ordering::Release);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancellation = cancel.clone();
    let calls = fixture.calls.clone();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "GetBlockRange")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancellation.store(true, Ordering::Release);
    });
    code(
        tokio::time::timeout(
            Duration::from_secs(8),
            wallet.sync_with_source_reported_cancellable(&fixture.source, None, cancel),
        )
        .await
        .unwrap(),
        "sync cancelled; saved scan retained",
    );
    task.await.unwrap();
    assert!(NativeWallet::open(root.path()).is_ok());
    fixture.hang_ranges.store(false, Ordering::Release);
    let (height, _) = tokio::time::timeout(
        Duration::from_secs(15),
        wallet.sync_with_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(height, 2);
    assert_eq!(fixture.connections.load(Ordering::Relaxed), 1);
    let before = fixture.calls.lock().unwrap().len();
    code(
        wallet
            .sync_with_source_reported_cancellable(
                &fixture.source,
                None,
                Arc::new(AtomicBool::new(true)),
            )
            .await,
        "sync cancelled; saved scan retained",
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_sidecar_and_database_copy_stay_read_only_and_malformed_singleton_refuses() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let wallet = tests::fixture_wallet(root.path());
    rusqlite::Connection::open(&wallet.paths.data_db)
        .unwrap()
        .execute_batch("CREATE TABLE fixture(value); INSERT INTO fixture VALUES (1)")
        .unwrap();
    NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    std::fs::remove_file(root.path().join("native-source-policy")).unwrap();
    let cold = NativeWallet::open(root.path()).unwrap();
    code(cold.sync_reported(None).await, "native_read_only_source");
    code(
        cold.sign_payment(&auth(), "irrelevant"),
        "native_read_only_source",
    );
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(&wallet.paths.data_db, copy.path().join("data.sqlite")).unwrap();
    std::fs::copy(&wallet.paths.meta_path, copy.path().join("wallet.json")).unwrap();
    let copied = NativeWallet::open(copy.path()).unwrap();
    code(copied.sync_reported(None).await, "native_read_only_source");
    code(
        copied.sign_payment(&auth(), "irrelevant"),
        "native_read_only_source",
    );
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints=ON; INSERT INTO ext_native_source_policy_v1 VALUES(2,99,1)").unwrap();
    let before = std::fs::read(&wallet.paths.data_db).unwrap();
    code(
        NativeWallet::open(root.path()),
        "native_source_policy_invalid",
    );
    code(
        NativeWallet::open_with_source(root.path(), &fixture.source),
        "native_source_policy_invalid",
    );
    code(
        wallet.sign_payment(&auth(), "irrelevant"),
        "native_source_policy_invalid",
    );
    assert_eq!(std::fs::read(&wallet.paths.data_db).unwrap(), before);
    assert!(fixture.calls.lock().unwrap().is_empty());
}
