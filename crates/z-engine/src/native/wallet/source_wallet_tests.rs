//! Fresh source stores and durable submissions over real memory-only HTTP/2.
use super::*;
use bytes::Bytes;
use http_body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use std::sync::atomic::AtomicU8;
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};
use zcash_client_backend::proto::{
    compact_formats::{ChainMetadata, CompactBlock},
    service::{BlockRange, LightdInfo, SendResponse, TreeState},
};
use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};

const PUBLIC_SEED: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const BINDING: [u8; 32] = [0x71; 32];
fn auth() -> SeedAuth {
    SeedAuth::passphrase("disposable-public-vector")
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
fn reply(messages: Vec<Vec<u8>>, status: &'static str) -> http::Response<Reply> {
    let mut frames = VecDeque::new();
    for message in messages {
        let mut bytes = vec![0];
        bytes.extend_from_slice(&(message.len() as u32).to_be_bytes());
        bytes.extend(message);
        frames.push_back(Frame::data(Bytes::from(bytes)));
    }
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static(status));
    frames.push_back(Frame::trailers(trailers));
    http::Response::builder()
        .header("content-type", "application/grpc")
        .body(Reply(frames))
        .unwrap()
}
fn block(height: u64, genesis: &str) -> CompactBlock {
    let parent = if height == 1 {
        let mut bytes = crate::web::from_hex(genesis).unwrap();
        bytes.reverse();
        bytes
    } else {
        vec![height.saturating_sub(1) as u8; 32]
    };
    CompactBlock {
        height,
        hash: vec![height as u8; 32],
        prev_hash: parent,
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
    source: super::super::NativeWalletSource,
    channel: tonic::transport::Channel,
    calls: Arc<Mutex<Vec<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    chain: Arc<Mutex<String>>,
    genesis: Arc<Mutex<String>>,
    mode: Arc<AtomicU8>,
    release: Arc<tokio::sync::Notify>,
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
    async fn new(root: &Path) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let chain = Arc::new(Mutex::new("regtest".to_owned()));
        let genesis = Arc::new(Mutex::new(
            super::super::chain_identity::genesis(ZNetwork::Regtest).to_owned(),
        ));
        let mode = Arc::new(AtomicU8::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let db = root.join("data.sqlite");
        let (c, s, n, g, m, r, t) = (
            calls.clone(),
            sent.clone(),
            chain.clone(),
            genesis.clone(),
            mode.clone(),
            release.clone(),
            tasks.clone(),
        );
        let connector = tower::service_fn(move |_: http::Uri| {
            let (c, s, n, g, m, r, t, db) = (
                c.clone(),
                s.clone(),
                n.clone(),
                g.clone(),
                m.clone(),
                r.clone(),
                t.clone(),
                db.clone(),
            );
            async move {
                let (client, server) = tokio::io::duplex(1024 * 1024);
                let service = hyper::service::service_fn(
                    move |request: http::Request<hyper::body::Incoming>| {
                        let (c, s, n, g, m, r, db) = (
                            c.clone(),
                            s.clone(),
                            n.clone(),
                            g.clone(),
                            m.clone(),
                            r.clone(),
                            db.clone(),
                        );
                        async move {
                            let method =
                                request.uri().path().rsplit('/').next().unwrap().to_owned();
                            c.lock().unwrap().push(method.clone());
                            let raw = http_body_util::BodyExt::collect(request.into_body())
                                .await
                                .unwrap()
                                .to_bytes();
                            let payload = &raw[5..];
                            // Mode 4 is an independently selected synthetic fork for host reorg tests.
                            let compact = |height| {
                                let mut result = block(height, &g.lock().unwrap());
                                if m.load(Ordering::Acquire) == 4 && height >= 2 {
                                    result.hash = vec![(height as u8).wrapping_add(0x80); 32];
                                    if height > 2 {
                                        result.prev_hash =
                                            vec![(height as u8 - 1).wrapping_add(0x80); 32];
                                    }
                                }
                                result
                            };
                            let response = match method.as_str() {
                                "GetLightdInfo" => reply(
                                    vec![LightdInfo {
                                        chain_name: n.lock().unwrap().clone(),
                                        block_height: 2,
                                        ..Default::default()
                                    }
                                    .encode_to_vec()],
                                    "0",
                                ),
                                "GetLatestBlock" => reply(
                                    vec![BlockId {
                                        height: 2,
                                        hash: compact(2).hash,
                                    }
                                    .encode_to_vec()],
                                    "0",
                                ),
                                "GetBlock" => reply(
                                    vec![compact(BlockId::decode(payload).unwrap().height)
                                        .encode_to_vec()],
                                    "0",
                                ),
                                "GetTreeState" => {
                                    let id = BlockId::decode(payload).unwrap();
                                    reply(
                                        vec![TreeState {
                                            network: n.lock().unwrap().clone(),
                                            height: id.height,
                                            hash: if id.height == 0 {
                                                g.lock().unwrap().clone()
                                            } else {
                                                let height = id.height as u8;
                                                format!(
                                                    "{:02x}",
                                                    if m.load(Ordering::Acquire) == 4
                                                        && id.height >= 2
                                                    {
                                                        height.wrapping_add(0x80)
                                                    } else {
                                                        height
                                                    }
                                                )
                                                .repeat(32)
                                            },
                                            time: 1,
                                            sapling_tree: "000000".into(),
                                            orchard_tree: "000000".into(),
                                            ironwood_tree: "000000".into(),
                                        }
                                        .encode_to_vec()],
                                        "0",
                                    )
                                }
                                "GetBlockRange" => {
                                    let range = BlockRange::decode(payload).unwrap();
                                    reply(
                                        (range.start.unwrap().height..=range.end.unwrap().height)
                                            .map(|h| compact(h).encode_to_vec())
                                            .collect(),
                                        "0",
                                    )
                                }
                                "GetSubtreeRoots" => reply(Vec::new(), "0"),
                                "SendTransaction" => {
                                    let phase: String = rusqlite::Connection::open(db)
                                        .unwrap()
                                        .query_row(
                                            "SELECT phase FROM ext_native_payments_v1",
                                            [],
                                            |row| row.get(0),
                                        )
                                        .unwrap();
                                    assert_eq!(
                                        phase, "unknown",
                                        "the receipt must commit before network bytes leave"
                                    );
                                    s.lock()
                                        .unwrap()
                                        .push(RawTransaction::decode(payload).unwrap().data);
                                    match m.load(Ordering::Acquire) {
                                        1 => reply(Vec::new(), "14"),
                                        3 => reply(
                                            vec![SendResponse {
                                                error_code: -1,
                                                error_message: "synthetic unfiltered node output"
                                                    .into(),
                                            }
                                            .encode_to_vec()],
                                            "0",
                                        ),
                                        2 => {
                                            r.notified().await;
                                            reply(
                                                vec![SendResponse::default().encode_to_vec()],
                                                "0",
                                            )
                                        }
                                        _ => reply(
                                            vec![SendResponse::default().encode_to_vec()],
                                            "0",
                                        ),
                                    }
                                }
                                other => panic!("private source sent forbidden RPC {other}"),
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
            tonic::transport::Endpoint::from_static("http://application-authenticated.invalid")
                .connect_with_connector(connector)
                .await
                .unwrap();
        Self {
            source: super::super::NativeWalletSource::new(
                channel.clone(),
                ZNetwork::Regtest,
                BINDING,
            ),
            channel,
            calls,
            sent,
            chain,
            genesis,
            mode,
            release,
            tasks,
        }
    }
    async fn restore(&self, root: &Path) -> NativeWallet {
        NativeWallet::restore_with_wallet_source(root, PUBLIC_SEED, &self.source, 1, auth(), 0)
            .await
            .unwrap()
            .0
    }
}
fn code<T>(result: Result<T>, expected: &str) {
    match result {
        Err(error) => assert_eq!(error.to_string(), expected),
        Ok(_) => panic!("expected {expected}"),
    }
}
fn saved_transaction(wallet: &NativeWallet) -> (String, Vec<u8>) {
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V5,
        BranchId::Nu6_3,
        0,
        (crate::regtest_nu6_3_height() + 10).into(),
        None,
        None,
        None,
        None,
    )
    .freeze()
    .unwrap();
    let txid = tx.txid();
    let mut raw = Vec::new();
    tx.write(&mut raw).unwrap();
    let id = "aa".repeat(32);
    let _ = wallet.payment_receipt(&id);
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    conn.execute_batch("INSERT OR IGNORE INTO blocks (height,hash,time,sapling_tree) VALUES (1,zeroblob(32),0,X'000000')").unwrap();
    conn.execute("INSERT INTO transactions (txid,created,expiry_height,raw,min_observed_height) VALUES (?1,'2026-09-24T00:00:00Z',?2,?3,1)",rusqlite::params![txid.as_ref(),crate::regtest_nu6_3_height()+10,raw]).unwrap();
    conn.execute("INSERT INTO transparent_received_outputs (transaction_id,output_index,account_id,address,script,value_zat,address_id) SELECT (SELECT id_tx FROM transactions WHERE txid=?1),0,account_id,'fixture',x'51',10000,id FROM addresses LIMIT 1",[txid.as_ref()]).unwrap();
    conn.execute("INSERT INTO ext_native_payments_v1 (id,proposal,fee,created,expires,height,block_hash,phase,txids,recovery_generation) VALUES (?1,X'00',10000,0,1,1,zeroblob(32),'signed',?2,0)",rusqlite::params![id,serde_json::to_string(&vec![txid.to_string()]).unwrap()]).unwrap();
    (id, raw)
}

#[tokio::test]
async fn fresh_source_scans_and_cold_open_cannot_use_any_direct_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let wallet = fixture.restore(root.path()).await;
    assert!(wallet.uses_wallet_source().unwrap());
    assert!(!wallet.uses_read_only_source().unwrap());
    wallet
        .sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    assert!(fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|m| m == "GetBlockRange"));
    assert!(!fixture.calls.lock().unwrap().iter().any(|m| matches!(
        m.as_str(),
        "GetTransaction" | "GetAddressUtxos" | "SendTransaction"
    )));
    let mut cold = NativeWallet::open(root.path()).unwrap();
    code(cold.sync().await, "native_wallet_source_required");
    cold.set_light_server(LightServer::parse("http://127.0.0.1:1", ZNetwork::Regtest))
        .unwrap();
    code(cold.sync().await, "native_wallet_source_required");
    code(
        cold.prepare_payment("irrelevant", 1, None),
        "native_wallet_source_required",
    );
    code(
        cold.sign_payment(&auth(), "irrelevant"),
        "native_wallet_source_required",
    );
    code(
        cold.submit_payment("irrelevant").await,
        "native_wallet_source_required",
    );
    let wrong = super::super::NativeWalletSource::new(
        fixture.channel.clone(),
        ZNetwork::Regtest,
        [0x72; 32],
    );
    code(
        NativeWallet::open_with_wallet_source(root.path(), &wrong),
        "native_wallet_source_mismatch",
    );
    std::fs::remove_file(root.path().join("native-wallet-source.json")).unwrap();
    let cold = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    assert!(cold.uses_wallet_source().unwrap());
    code(
        cold.prepare_payment("irrelevant", 1, None),
        "native_wallet_source_required",
    );
}

#[tokio::test]
async fn writable_source_does_not_adopt_ordinary_or_historical_readonly_wallets() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let ordinary = tests::fixture_wallet(root.path());
    let (ufvk, birthday) = tests::fixture_account();
    ordinary.replace_scan_db(&ufvk, &birthday).unwrap();
    let before = std::fs::read(&ordinary.paths.meta_path).unwrap();
    code(
        NativeWallet::open_with_wallet_source(root.path(), &fixture.source),
        "native_wallet_source_required",
    );
    code(
        NativeWallet::restore_with_wallet_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            1,
            auth(),
            0,
        )
        .await,
        "native_wallet_source_fresh_required",
    );
    assert_eq!(std::fs::read(&ordinary.paths.meta_path).unwrap(), before);
    let ro = super::super::NativeReadOnlySource::new(fixture.channel.clone(), ZNetwork::Regtest);
    let readonly = NativeWallet::open_with_source(root.path(), &ro).unwrap();
    code(
        readonly.prepare_payment_with_source(&fixture.source, "irrelevant", 1, None),
        "native_read_only_source",
    );
    code(
        readonly.sign_payment_with_source(&fixture.source, &auth(), "irrelevant"),
        "native_read_only_source",
    );
    code(
        readonly
            .submit_payment_with_source(
                &fixture.source,
                "irrelevant",
                Arc::new(AtomicBool::new(false)),
            )
            .await,
        "native_read_only_source",
    );
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explicit_source_submit_verifies_genesis_before_unknown_and_replays_only_saved_bytes() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let wallet = fixture.restore(root.path()).await;
    let (id, raw) = saved_transaction(&wallet);
    let direct = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    direct.set_nonblocking(true).unwrap();
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&wallet.paths.meta_path).unwrap()).unwrap();
    meta["server"] = format!("http://{}", direct.local_addr().unwrap()).into();
    std::fs::write(&wallet.paths.meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let cold = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    *fixture.chain.lock().unwrap() = "test".into();
    *fixture.genesis.lock().unwrap() =
        super::super::chain_identity::genesis(ZNetwork::Testnet).into();
    assert!(matches!(
        cold.submit_payment_with_source(&fixture.source, &id, Arc::new(AtomicBool::new(false)))
            .await,
        Err(EngineError::ChainMismatch { .. })
    ));
    assert_eq!(cold.payment_receipt(&id).unwrap().phase, "signed");
    assert!(fixture.sent.lock().unwrap().is_empty());
    *fixture.chain.lock().unwrap() = "regtest".into();
    *fixture.genesis.lock().unwrap() =
        super::super::chain_identity::genesis(ZNetwork::Regtest).into();
    fixture.mode.store(1, Ordering::Release);
    let receipt = cold
        .submit_payment_with_source(&fixture.source, &id, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    assert_eq!(receipt.phase, "unknown");
    assert_eq!(*fixture.sent.lock().unwrap(), vec![raw.clone()]);
    let cold = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    fixture.mode.store(0, Ordering::Release);
    assert_eq!(
        cold.submit_payment_with_source(&fixture.source, &id, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap()
            .phase,
        "accepted"
    );
    assert_eq!(*fixture.sent.lock().unwrap(), vec![raw.clone(), raw]);
    assert_eq!(
        direct.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn cancellation_after_submission_preserves_unknown_and_cannot_apply_a_late_ack() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let wallet = fixture.restore(root.path()).await;
    let (id, raw) = saved_transaction(&wallet);
    let cancel = Arc::new(AtomicBool::new(true));
    code(
        wallet
            .submit_payment_with_source(&fixture.source, &id, cancel.clone())
            .await,
        "native_operation_cancelled",
    );
    assert_eq!(wallet.payment_receipt(&id).unwrap().phase, "signed");
    assert!(fixture.sent.lock().unwrap().is_empty());
    cancel.store(false, Ordering::Release);
    fixture.mode.store(2, Ordering::Release);
    let submission = wallet.submit_payment_with_source(&fixture.source, &id, cancel.clone());
    tokio::pin!(submission);
    tokio::select! { result=&mut submission => panic!("unexpected completion {result:?}"), _=async {loop {if !fixture.sent.lock().unwrap().is_empty(){break;}tokio::time::sleep(Duration::from_millis(2)).await;}}=>{} }
    assert!(
        database_lease::exclusive(&wallet.paths.data_db).is_err(),
        "source submit retains database ownership"
    );
    cancel.store(true, Ordering::Release);
    code(submission.await, "native_operation_cancelled");
    fixture.release.notify_waiters();
    tokio::task::yield_now().await;
    assert_eq!(wallet.payment_receipt(&id).unwrap().phase, "unknown");
    assert_eq!(*fixture.sent.lock().unwrap(), vec![raw]);
}

#[tokio::test]
async fn source_clients_refuse_selected_transaction_queries_and_scan_cannot_submit() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let mut scan = fixture.source.client();
    assert!(scan
        .send_transaction(RawTransaction::default())
        .await
        .is_err());
    let mut submission = fixture.source.submission_client();
    assert!(submission
        .get_transaction(TxFilter::default())
        .await
        .is_err());
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn conflicting_source_policy_is_preserved_and_refused_before_transport() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let wallet = fixture.restore(root.path()).await;
    let path = root.path().join("native-wallet-source.json");
    let mut policy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    policy["binding"][0] = 99.into();
    let altered = serde_json::to_vec(&policy).unwrap();
    std::fs::write(&path, &altered).unwrap();
    fixture.calls.lock().unwrap().clear();
    code(
        NativeWallet::open(root.path()),
        "native_wallet_source_policy_invalid",
    );
    code(
        wallet
            .submit_payment_with_source(
                &fixture.source,
                "irrelevant",
                Arc::new(AtomicBool::new(false)),
            )
            .await,
        "native_wallet_source_policy_invalid",
    );
    assert_eq!(std::fs::read(&path).unwrap(), altered);
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn writable_source_refuses_unbound_retained_seed_before_any_policy_write() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let store = SeedStore::new(root.path());
    store
        .save(PUBLIC_SEED, Some("disposable-public-vector"), false)
        .unwrap();
    code(
        NativeWallet::restore_with_wallet_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            1,
            auth(),
            0,
        )
        .await,
        "native_wallet_source_fresh_required",
    );
    assert!(!root.path().join("native-wallet-source.json").exists());
    assert!(!root.path().join("native-wallet-source-setup.json").exists());
    assert!(!root.path().join("data.sqlite").exists());
    assert!(fixture.calls.lock().unwrap().is_empty());
    assert_eq!(
        store
            .load(Some("disposable-public-vector"), false)
            .unwrap()
            .expose_secret(),
        PUBLIC_SEED
    );
}

#[tokio::test]
async fn source_rejection_keeps_saved_intent_and_exposes_only_safe_error_text() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(root.path()).await;
    let wallet = fixture.restore(root.path()).await;
    let (id, raw) = saved_transaction(&wallet);
    fixture.mode.store(3, Ordering::Release);
    let error = wallet
        .submit_payment_with_source(&fixture.source, &id, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        EngineError::BroadcastRejected { code: -1, .. }
    ));
    assert!(!error
        .to_string()
        .contains("synthetic unfiltered node output"));
    assert_eq!(wallet.payment_receipt(&id).unwrap().phase, "unknown");
    assert_eq!(*fixture.sent.lock().unwrap(), vec![raw]);
}

#[cfg(feature = "transparent-pir")]
#[path = "transparent_pir_source_tests.rs"]
mod transparent_pir_projection_tests;

#[cfg(feature = "transparent-pir")]
#[path = "transparent_pir_integration_tests.rs"]
mod transparent_pir_integration_tests;
