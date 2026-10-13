//! Public vectors and memory-only HTTP/2 transport; no wallet TCP listener.
use super::*;
use bytes::Bytes;
use http_body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::task::{Context, Poll};
use zcash_client_backend::proto::{
    compact_formats::{ChainMetadata, CompactBlock},
    service::{BlockRange, LightdInfo, TreeState},
};

const PUBLIC_SEED: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn reset_fixture(root: &Path) -> (NativeWallet, UnifiedFullViewingKey, AccountBirthday) {
    let mut wallet = tests::fixture_wallet(root);
    let keys = crate::keys::account_from_mnemonic(PUBLIC_SEED, ZNetwork::Regtest, 0).unwrap();
    let ufvk = UnifiedFullViewingKey::decode(&ZNetwork::Regtest, &keys.ufvk).unwrap();
    wallet.meta.ufvk = Some(keys.ufvk);
    wallet.save_meta().unwrap();
    let birthday = AccountBirthday::from_parts(
        zcash_client_backend::data_api::chain::ChainState::empty(
            0.into(),
            zcash_primitives::block::BlockHash([0; 32]),
        ),
        None,
    );
    wallet.replace_scan_db(&ufvk, &birthday).unwrap();
    (wallet, ufvk, birthday)
}

#[tokio::test]
async fn source_enrollment_between_reset_preflight_and_swap_preserves_marked_database() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (stale, ufvk, birthday) = reset_fixture(root.path());
    stale.require_network_access().unwrap();
    NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    let before = std::fs::read(&stale.paths.data_db).unwrap();
    code(
        stale.replace_scan_db(&ufvk, &birthday),
        "native_read_only_source",
    );
    assert_eq!(std::fs::read(&stale.paths.data_db).unwrap(), before);
    std::fs::remove_file(root.path().join("native-source-policy")).unwrap();
    assert!(NativeWallet::open(root.path())
        .unwrap()
        .uses_read_only_source()
        .unwrap());
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn public_reset_holds_exclusive_policy_ownership_before_first_network_await() {
    use std::future::Future;
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (mut stale, _, _) = reset_fixture(root.path());
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    stale
        .set_light_server(LightServer::parse(
            &format!("http://{}", server.local_addr().unwrap()),
            ZNetwork::Regtest,
        ))
        .unwrap();
    let mut reset = Box::pin(stale.reset_scan());
    assert!(reset
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()))
        .is_pending());
    code(
        NativeWallet::open_with_source(root.path(), &fixture.source),
        "native_pir_database_busy",
    );
    assert!(!root.path().join("native-source-policy").exists());
    drop(reset);
    NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    assert!(stale.uses_read_only_source().unwrap());
    code(stale.reset_scan().await, "native_read_only_source");
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn source_enrollment_waits_for_network_operation_ownership_and_stale_payments_refuse() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (stale, _, _) = reset_fixture(root.path());
    let operation = stale.network_operation_lease().unwrap();
    code(
        NativeWallet::open_with_source(root.path(), &fixture.source),
        "native_pir_database_busy",
    );
    assert!(!root.path().join("native-source-policy").exists());
    drop(operation);
    NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    let before = std::fs::read(&stale.paths.data_db).unwrap();
    code(
        stale.send(&auth(), "irrelevant", 1, None).await,
        "native_read_only_source",
    );
    code(
        stale
            .send_with_policy(&auth(), "irrelevant", 1, None, shielded_spend_policy())
            .await,
        "native_read_only_source",
    );
    code(
        stale
            .send_payments(&auth(), &[], shielded_spend_policy())
            .await,
        "native_read_only_source",
    );
    #[cfg(feature = "transparent-inputs")]
    code(stale.shield(&auth(), 1).await, "native_read_only_source");
    code(
        stale.submit_payment("irrelevant").await,
        "native_read_only_source",
    );
    code(
        stale.estimate_fee("irrelevant", 1, None),
        "native_read_only_source",
    );
    assert_eq!(std::fs::read(&stale.paths.data_db).unwrap(), before);
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_enrollment_cannot_interleave_with_ordinary_retry_endpoint_probes() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (mut wallet, _, _) = reset_fixture(root.path());
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    wallet
        .set_light_server(LightServer::parse(
            &format!("http://{}", server.local_addr().unwrap()),
            ZNetwork::Regtest,
        ))
        .unwrap();
    let stopped_server = tokio::spawn(async move {
        while let Ok((socket, _)) = server.accept().await {
            drop(socket);
        }
    });
    let progress = Arc::new(Mutex::new(SyncProgress::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (p, c) = (progress.clone(), cancel.clone());
    let syncing =
        tokio::spawn(async move { wallet.sync_reported_resilient_cancellable(Some(p), c).await });
    let waiting = tokio::time::timeout(Duration::from_secs(5), async {
        while !progress.lock().unwrap().message.contains("reconnecting") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let enrollment = NativeWallet::open_with_source(root.path(), &fixture.source);
    cancel.store(true, Ordering::Release);
    let completed = tokio::time::timeout(Duration::from_secs(2), syncing).await;
    stopped_server.abort();
    let _ = stopped_server.await;
    waiting.unwrap();
    code(
        completed.unwrap().unwrap(),
        "sync cancelled; saved scan retained",
    );
    code(enrollment, "native_pir_database_busy");
    assert!(!root.path().join("native-source-policy").exists());
    NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
}

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
    fail_tree: Arc<AtomicBool>,
    tip: Arc<AtomicU64>,
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
        let fail_tree = Arc::new(AtomicBool::new(false));
        let tip = Arc::new(AtomicU64::new(tip));
        let (ft, tp) = (fail_tree.clone(), tip.clone());
        let (c, h, t, n) = (
            calls.clone(),
            hang_ranges.clone(),
            tasks.clone(),
            connections.clone(),
        );
        let connector = tower::service_fn(move |_: http::Uri| {
            let (c, h, t, n) = (c.clone(), h.clone(), t.clone(), n.clone());
            let (ft, tp) = (ft.clone(), tp.clone());
            async move {
                n.fetch_add(1, Ordering::Relaxed);
                let (client, server) = tokio::io::duplex(1024 * 1024);
                let service = hyper::service::service_fn(
                    move |request: http::Request<hyper::body::Incoming>| {
                        let (c, h) = (c.clone(), h.clone());
                        let (ft, tp) = (ft.clone(), tp.clone());
                        async move {
                            let tip = tp.load(Ordering::Acquire);
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
                                        sapling_tree: if ft.load(Ordering::Acquire) {
                                            "invalid".into()
                                        } else {
                                            "000000".into()
                                        },
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
            fail_tree,
            tip,
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
    let owner = Arc::new(database_lease::shared(&cold.paths.data_db).unwrap());
    assert!(cold.validator_rpc_url().is_none());
    code(cold.sync_reported(None).await, "native_read_only_source");
    code(
        cold.enhance_memos(&mut cold.open_db().unwrap(), &owner)
            .await,
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
    assert!(!stale.uses_read_only_source().unwrap());
    let enrolled = NativeWallet::open_with_source(root.path(), &fixture.source).unwrap();
    assert!(enrolled.uses_read_only_source().unwrap());
    assert!(stale.uses_read_only_source().unwrap());
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
        enrolled.uses_read_only_source(),
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
    assert!(copied.uses_read_only_source().unwrap());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_setup_refuses_marked_database_without_metadata_before_seed_or_source_access() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (wallet, _) =
        NativeWallet::restore_with_source(root.path(), PUBLIC_SEED, &fixture.source, 1, auth(), 0)
            .await
            .unwrap();
    std::fs::remove_file(&wallet.paths.meta_path).unwrap();
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let db = std::fs::read(&wallet.paths.data_db).unwrap();
    let before = fixture.calls.lock().unwrap().len();
    code(
        NativeWallet::create_with_source(root.path(), &fixture.source, Some(1), auth(), 0).await,
        "native_read_only_source",
    );
    code(
        NativeWallet::restore_with_source(root.path(), PUBLIC_SEED, &fixture.source, 1, auth(), 0)
            .await,
        "native_read_only_source",
    );
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
    assert!(std::fs::read(&wallet.paths.data_db).unwrap() == db);
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_seed_only_create_retry_reuses_original_authenticated_entropy() {
    let failed = Fixture::new(ZNetwork::Mainnet, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    assert!(NativeWallet::create_with_source(
        root.path(),
        &failed.source,
        Some(419_201),
        auth(),
        0
    )
    .await
    .is_err());
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let words = SeedStore::new(root.path())
        .load(auth().passphrase.as_deref(), false)
        .unwrap();
    let healthy = Fixture::new(ZNetwork::Mainnet, "main", 419_201).await;
    let (_, created) =
        NativeWallet::create_with_source(root.path(), &healthy.source, Some(419_201), auth(), 0)
            .await
            .unwrap();
    assert!(created.mnemonic == *words.expose_secret());
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_seed_only_restore_rejects_replacement_before_network_or_writes() {
    let failed = Fixture::new(ZNetwork::Mainnet, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    assert!(NativeWallet::restore_with_source(
        root.path(),
        PUBLIC_SEED,
        &failed.source,
        419_201,
        auth(),
        0
    )
    .await
    .is_err());
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let healthy = Fixture::new(ZNetwork::Mainnet, "main", 419_201).await;
    code(
        NativeWallet::restore_with_source(
            root.path(),
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            &healthy.source,
            419_201,
            auth(),
            0,
        )
        .await,
        "native_source_seed_mismatch",
    );
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
    assert!(healthy.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_source_birthday_is_bound_before_seed_and_survives_newer_tip_retry() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 200).await;
    fixture.fail_tree.store(true, Ordering::Release);
    let root = tempfile::tempdir().unwrap();
    assert!(
        NativeWallet::create_with_source(root.path(), &fixture.source, None, auth(), 0)
            .await
            .is_err()
    );
    let setup = super::super::source::setup(root.path()).unwrap().unwrap();
    assert_eq!(setup.birthday_height, 100);
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let retained = SeedStore::new(root.path())
        .load(auth().passphrase.as_deref(), false)
        .unwrap();
    fixture.tip.store(400, Ordering::Release);
    fixture.fail_tree.store(false, Ordering::Release);
    let (wallet, created) =
        NativeWallet::create_with_source(root.path(), &fixture.source, None, auth(), 0)
            .await
            .unwrap();
    assert_eq!(created.birthday_height, 100);
    assert_eq!(wallet.birthday_height(), 100);
    assert!(created.mnemonic == *retained.expose_secret());
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_seed_only_binding_refuses_changed_scope_missing_or_malformed_record() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 200).await;
    fixture.fail_tree.store(true, Ordering::Release);
    let root = tempfile::tempdir().unwrap();
    assert!(NativeWallet::restore_with_source(
        root.path(),
        PUBLIC_SEED,
        &fixture.source,
        100,
        auth(),
        0
    )
    .await
    .is_err());
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let before = fixture.calls.lock().unwrap().len();
    fixture.fail_tree.store(false, Ordering::Release);
    code(
        NativeWallet::restore_with_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            101,
            auth(),
            0,
        )
        .await,
        "native_source_setup_mismatch",
    );
    code(
        NativeWallet::restore_with_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            100,
            auth(),
            1,
        )
        .await,
        "native_source_setup_mismatch",
    );
    let mainnet = Fixture::new(ZNetwork::Mainnet, "main", 419_201).await;
    code(
        NativeWallet::restore_with_source(
            root.path(),
            PUBLIC_SEED,
            &mainnet.source,
            100,
            auth(),
            0,
        )
        .await,
        "native_source_setup_mismatch",
    );
    assert!(mainnet.calls.lock().unwrap().is_empty());
    std::fs::remove_file(root.path().join("native-source-setup.json")).unwrap();
    code(
        NativeWallet::create_with_source(root.path(), &fixture.source, None, auth(), 0).await,
        "native_source_setup_binding_required",
    );
    std::fs::write(root.path().join("native-source-setup.json"), b"{broken").unwrap();
    code(
        NativeWallet::restore_with_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            100,
            auth(),
            0,
        )
        .await,
        "native_source_policy_invalid",
    );
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_setup_never_quarantines_an_unmarked_retained_database() {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let wallet = tests::fixture_wallet(root.path());
    rusqlite::Connection::open(&wallet.paths.data_db)
        .unwrap()
        .execute_batch("CREATE TABLE fixture(value); INSERT INTO fixture VALUES(1)")
        .unwrap();
    std::fs::remove_file(&wallet.paths.meta_path).unwrap();
    let original = std::fs::read(&wallet.paths.data_db).unwrap();
    code(
        NativeWallet::create_with_source(root.path(), &fixture.source, Some(1), auth(), 0).await,
        "native_read_only_source",
    );
    code(
        NativeWallet::restore_with_source(root.path(), PUBLIC_SEED, &fixture.source, 1, auth(), 0)
            .await,
        "native_read_only_source",
    );
    assert!(std::fs::read(&wallet.paths.data_db).unwrap() == original);
    assert!(fixture.calls.lock().unwrap().is_empty());
    assert!(!root.path().join("native-source-policy").exists());
    assert!(!root.path().join("seed.enc").exists());
}

struct StalledValidator {
    endpoint: String,
    entered: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl StalledValidator {
    fn new(stall_method: &'static str) -> Self {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture accept deadline");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => panic!("fixture accept failed"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = std::io::BufReader::new(socket);
            loop {
                let mut length = None;
                let mut total = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).unwrap();
                    assert!(!line.is_empty());
                    total += line.len();
                    assert!(total <= 16 * 1024);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let length = length.unwrap();
                assert!(length <= 64 * 1024);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let method = request["method"].as_str().unwrap();
                if method == stall_method {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(5));
                    break;
                }
                let result = match method {
                    "getblockchaininfo" => serde_json::json!({"chain":"regtest"}),
                    "getblockhash" => {
                        serde_json::json!(super::super::chain_identity::genesis(ZNetwork::Regtest))
                    }
                    _ => panic!("unexpected fixture method"),
                };
                let response = serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result,"error":null}).to_string();
                write!(socket.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{response}", response.len()).unwrap();
                socket.get_mut().flush().unwrap();
            }
        });
        Self {
            endpoint,
            entered,
            release: Some(release),
            worker: Some(worker),
        }
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.entered.try_recv() {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Err(_) => panic!("fixture ended before request"),
                }
            }
        })
        .await
        .unwrap();
    }

    fn finish(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

impl Drop for StalledValidator {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn detached_validator_worker_keeps_policy_owner(operation: &'static str) {
    let fixture = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (mut wallet, _, _) = reset_fixture(root.path());
    let method = match operation {
        "connect" => "getblockchaininfo",
        "send" => "sendrawtransaction",
        "verbose" | "raw" | "shield" => "getrawtransaction",
        _ => unreachable!(),
    };
    if operation == "shield" {
        use zcash_client_backend::{
            data_api::WalletCommitmentTrees, wallet::WalletTransparentOutput,
        };
        use zcash_primitives::transaction::Transaction;
        let mut db = wallet.open_db().unwrap();
        let account = NativeWallet::primary_account_id(&db).unwrap();
        db.update_chain_tip(199.into()).unwrap();
        db.with_orchard_tree_mut(|tree| tree.checkpoint(199.into()))
            .unwrap();
        let addresses = db.get_transparent_receivers(account, true, true).unwrap();
        let address = addresses.keys().next().unwrap();
        let script: transparent::address::Script = address.script().into();
        let mut raw = Vec::new();
        raw.extend(1u32.to_le_bytes());
        raw.push(1);
        raw.extend([0; 32]);
        raw.extend(u32::MAX.to_le_bytes());
        raw.extend([2, 1, 1]);
        raw.extend(u32::MAX.to_le_bytes());
        raw.push(1);
        raw.extend(50_000u64.to_le_bytes());
        script.write(&mut raw).unwrap();
        raw.extend(0u32.to_le_bytes());
        let tx = Transaction::read(&raw[..], BranchId::Sprout).unwrap();
        let output = WalletTransparentOutput::from_parts(
            transparent::bundle::OutPoint::new(*tx.txid().as_ref(), 0),
            tx.transparent_bundle().unwrap().vout[0].clone(),
            Some(101.into()),
            Some(account),
            None,
            None,
        )
        .unwrap();
        db.put_received_transparent_utxo(&output).unwrap();
    }
    let mut validator = StalledValidator::new(method);
    wallet
        .set_validator_rpc(Some(validator.endpoint.clone()))
        .unwrap();
    let task = tokio::spawn(async move {
        let owner = wallet.network_operation_lease().unwrap();
        match operation {
            "connect" => {
                let _ = wallet.connect_submission(&owner).await;
            }
            "send" => {
                let _ = wallet.submit_raw(&mut None, vec![0xfa, 0xce], &owner).await;
            }
            "verbose" => {
                let _ = wallet
                    .lookup_transaction(None, TxId::from_bytes([17; 32]), &owner)
                    .await;
            }
            "shield" => {
                let mut unlock = auth();
                unlock.mnemonic = Some(PUBLIC_SEED.into());
                let _ = wallet.shield(&unlock, 1).await;
            }
            "raw" => {
                let _ = NativeWallet::fetch_raw_transaction_with_rpc_owned(
                    &wallet.server,
                    wallet.validator_rpc_url().as_deref(),
                    &"11".repeat(32),
                    owner,
                )
                .await;
            }
            _ => unreachable!(),
        }
    });
    validator.wait_entered().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let before = std::fs::read(root.path().join("data.sqlite")).unwrap();
    let enrollment = NativeWallet::open_with_source(root.path(), &fixture.source);
    let database_was_unchanged = std::fs::read(root.path().join("data.sqlite")).unwrap() == before;
    let policy_was_absent = !root.path().join("native-source-policy").exists();
    validator.finish();
    // Draining the fixture socket lets the detached worker finish and release its owner.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match NativeWallet::open_with_source(root.path(), &fixture.source) {
                Ok(_) => break,
                Err(EngineError::Message(code)) if code == "native_pir_database_busy" => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Err(_) => panic!("unexpected enrollment refusal"),
            }
        }
    })
    .await
    .unwrap();
    code(enrollment, "native_pir_database_busy");
    assert!(policy_was_absent);
    assert!(database_was_unchanged);
    assert!(NativeWallet::open(root.path())
        .unwrap()
        .uses_read_only_source()
        .unwrap());
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_validator_identity_worker_retains_policy_owner() {
    detached_validator_worker_keeps_policy_owner("connect").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_validator_send_worker_retains_policy_owner() {
    detached_validator_worker_keeps_policy_owner("send").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_validator_verbose_worker_retains_policy_owner() {
    detached_validator_worker_keeps_policy_owner("verbose").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_validator_raw_worker_retains_policy_owner() {
    detached_validator_worker_keeps_policy_owner("raw").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_shield_funding_worker_retains_policy_owner() {
    detached_validator_worker_keeps_policy_owner("shield").await;
}

#[tokio::test]
async fn next_address_refuses_live_reset_ownership_and_source_rotation_remains_local() {
    use std::future::Future;
    let source = Fixture::new(ZNetwork::Regtest, "regtest", 2).await;
    let root = tempfile::tempdir().unwrap();
    let (mut wallet, _, _) = reset_fixture(root.path());
    let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    wallet
        .set_light_server(LightServer::parse(
            &format!("http://{}", endpoint.local_addr().unwrap()),
            ZNetwork::Regtest,
        ))
        .unwrap();
    wallet
        .open_db()
        .unwrap()
        .update_chain_tip(2.into())
        .unwrap();
    let original = wallet.next_unified_address().unwrap();
    let mut reset = Box::pin(wallet.reset_scan());
    assert!(reset
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()))
        .is_pending());
    code(wallet.next_unified_address(), "native_pir_database_busy");
    assert!(wallet.unified_address().unwrap() == original);
    drop(reset);
    let next = wallet.next_unified_address().unwrap();
    assert!(next != original);
    let enrolled = NativeWallet::open_with_source(root.path(), &source.source).unwrap();
    let source_next = enrolled.next_unified_address().unwrap();
    assert!(source_next != next);
    assert!(enrolled.unified_address().unwrap() == source_next);
    assert!(source.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guarded_birthday_setup_refuses_seed_only_source_before_date_or_height_io() {
    let root = tempfile::tempdir().unwrap();
    super::super::source::enroll(root.path()).unwrap();
    SeedStore::new(root.path())
        .save(PUBLIC_SEED, auth().passphrase.as_deref(), false)
        .unwrap();
    let seed = std::fs::read(root.path().join("seed.enc")).unwrap();
    let policy = std::fs::read(root.path().join("native-source-policy")).unwrap();
    let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    endpoint.set_nonblocking(true).unwrap();
    let server = LightServer::parse(
        &format!("http://{}", endpoint.local_addr().unwrap()),
        ZNetwork::Regtest,
    );
    let ufvk = crate::keys::account_from_mnemonic(PUBLIC_SEED, ZNetwork::Regtest, 0)
        .unwrap()
        .ufvk;
    for birthday in [
        NativeBirthday::Date("2026-10-01".into()),
        NativeBirthday::Height(1),
    ] {
        code(
            NativeWallet::create_with_birthday(
                root.path(),
                ZNetwork::Regtest,
                Some(server.clone()),
                Some(birthday.clone()),
                auth(),
                0,
            )
            .await,
            "native_read_only_source",
        );
        code(
            NativeWallet::restore_with_birthday(
                root.path(),
                PUBLIC_SEED,
                ZNetwork::Regtest,
                Some(server.clone()),
                birthday.clone(),
                auth(),
                0,
            )
            .await,
            "native_read_only_source",
        );
        code(
            NativeWallet::restore_ufvk_with_birthday(
                root.path(),
                &ufvk,
                ZNetwork::Regtest,
                Some(server.clone()),
                birthday,
                0,
            )
            .await,
            "native_read_only_source",
        );
    }
    assert_eq!(
        endpoint.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(std::fs::read(root.path().join("seed.enc")).unwrap() == seed);
    assert!(std::fs::read(root.path().join("native-source-policy")).unwrap() == policy);
    assert!(!root.path().join("data.sqlite").exists());
    assert!(!root.path().join("wallet.json").exists());
}

#[tokio::test]
async fn invalid_date_setup_refuses_before_endpoint_contact_or_seed_writes() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    endpoint.set_nonblocking(true).unwrap();
    let server = LightServer::parse(
        &format!("http://{}", endpoint.local_addr().unwrap()),
        ZNetwork::Regtest,
    );
    let ufvk = crate::keys::account_from_mnemonic(PUBLIC_SEED, ZNetwork::Regtest, 0)
        .unwrap()
        .ufvk;
    for invalid in [
        "",
        "auto",
        "100",
        "2026-1-1",
        "not-a-date",
        "2026-02-30",
        "2026-13-01",
    ] {
        let birthday = NativeBirthday::Date(invalid.into());
        code(
            NativeWallet::create_with_birthday(
                root.path(),
                ZNetwork::Regtest,
                Some(server.clone()),
                Some(birthday.clone()),
                auth(),
                0,
            )
            .await,
            "native_birthday_date_invalid",
        );
        code(
            NativeWallet::restore_with_birthday(
                root.path(),
                PUBLIC_SEED,
                ZNetwork::Regtest,
                Some(server.clone()),
                birthday.clone(),
                auth(),
                0,
            )
            .await,
            "native_birthday_date_invalid",
        );
        code(
            NativeWallet::restore_ufvk_with_birthday(
                root.path(),
                &ufvk,
                ZNetwork::Regtest,
                Some(server.clone()),
                birthday,
                0,
            )
            .await,
            "native_birthday_date_invalid",
        );
    }
    assert_eq!(
        endpoint.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!root.path().join("seed.enc").exists());
    assert!(!root.path().join("data.sqlite").exists());
    assert!(!root.path().join("wallet.json").exists());
}

#[tokio::test]
async fn enrollment_network_mismatch_preserves_valid_existing_wallet_before_policy_mutation() {
    let source = Fixture::new(ZNetwork::Mainnet, "main", 419_201).await;
    let root = tempfile::tempdir().unwrap();
    let (wallet, _, _) = reset_fixture(root.path());
    let database = std::fs::read(&wallet.paths.data_db).unwrap();
    let metadata = std::fs::read(&wallet.paths.meta_path).unwrap();
    code(
        NativeWallet::open_with_source(root.path(), &source.source),
        "native_source_network_mismatch",
    );
    assert!(std::fs::read(&wallet.paths.data_db).unwrap() == database);
    assert!(std::fs::read(&wallet.paths.meta_path).unwrap() == metadata);
    assert!(!root.path().join("native-source-policy").exists());
    assert!(!wallet.uses_read_only_source().unwrap());
    assert!(source.calls.lock().unwrap().is_empty());
}
