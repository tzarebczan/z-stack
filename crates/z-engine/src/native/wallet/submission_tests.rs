//! Exercise real native gRPC submission using saved wallet receipts.
use super::*;
use prost::Message;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use tower::Service;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::{LightdInfo, SendResponse};
use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};

#[derive(Clone)]
struct Peer {
    chain: &'static str,
    hash: String,
    submissions: Arc<AtomicUsize>,
    db: PathBuf,
}
impl tonic::server::NamedService for Peer {
    const NAME: &'static str = "cash.z.wallet.sdk.rpc.CompactTxStreamer";
}
impl Service<http::Request<tonic::body::Body>> for Peer {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = std::future::Ready<std::result::Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let bytes = match request.uri().path().rsplit('/').next().unwrap() {
            "GetLightdInfo" => LightdInfo {
                chain_name: self.chain.into(),
                ..Default::default()
            }
            .encode_to_vec(),
            "GetBlock" => {
                let mut parent = crate::web::from_hex(&self.hash).unwrap();
                parent.reverse();
                CompactBlock {
                    height: 1,
                    prev_hash: parent,
                    ..Default::default()
                }
                .encode_to_vec()
            }
            "SendTransaction" => {
                // Count reception before checking state so a broken guard cannot
                // pass the no-broadcast assertion through a peer-side panic.
                self.submissions.fetch_add(1, Ordering::SeqCst);
                let conn = rusqlite::Connection::open(&self.db).unwrap();
                let phase: String = conn
                    .query_row("SELECT phase FROM ext_native_payments_v1", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    phase, "unknown",
                    "uncertainty must commit before bytes leave"
                );
                SendResponse {
                    error_code: 0,
                    error_message: String::new(),
                }
                .encode_to_vec()
            }
            other => panic!("unexpected submission RPC: {other}"),
        };
        let mut frame = vec![0];
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&bytes);
        std::future::ready(Ok(http::Response::builder()
            .header("content-type", "application/grpc")
            .header("grpc-status", "0")
            .body(tonic::body::Body::new(http_body_util::Full::new(
                bytes::Bytes::from(frame),
            )))
            .unwrap()))
    }
}

fn signed_fixture(dir: &Path) -> (NativeWallet, String) {
    let wallet = super::tests::fixture_wallet(dir);
    let (ufvk, birthday) = super::tests::fixture_account();
    wallet.replace_scan_db(&ufvk, &birthday).unwrap();
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
    // Create the receipt table through the public read boundary.
    let _ = wallet.payment_receipt(&"aa".repeat(32));
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    conn.execute_batch(
        "INSERT INTO blocks (height,hash,time,sapling_tree) VALUES (1,zeroblob(32),0,X'000000')",
    )
    .unwrap();
    conn.execute("INSERT INTO transactions (txid,created,expiry_height,raw,min_observed_height) VALUES (?1,'2026-09-24T00:00:00Z',?2,?3,1)", rusqlite::params![txid.as_ref(), crate::regtest_nu6_3_height() + 10, raw]).unwrap();
    conn.execute("INSERT INTO transparent_received_outputs (transaction_id,output_index,account_id,address,script,value_zat,address_id) SELECT (SELECT id_tx FROM transactions WHERE txid=?1),0,account_id,'fixture',x'51',10000,id FROM addresses LIMIT 1", [txid.as_ref()]).unwrap();
    let id = "aa".repeat(32);
    conn.execute("INSERT INTO ext_native_payments_v1 (id,proposal,fee,created,expires,height,block_hash,phase,txids,recovery_generation) VALUES (?1,X'00',10000,0,1,1,zeroblob(32),'signed',?2,0)", rusqlite::params![id,serde_json::to_string(&vec![txid.to_string()]).unwrap()]).unwrap();
    (wallet, id)
}

#[tokio::test]
async fn changed_endpoint_identity_refuses_initial_and_recovery_submission_without_mutation() {
    for (chain, hash, accepted) in [
        (
            "main",
            super::super::chain_identity::genesis(ZNetwork::Mainnet),
            false,
        ),
        (
            "test",
            super::super::chain_identity::genesis(ZNetwork::Testnet),
            false,
        ),
        ("regtest", "", false),
        (
            "test",
            super::super::chain_identity::genesis(ZNetwork::Regtest),
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut wallet, id) = signed_fixture(dir.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let submissions = Arc::new(AtomicUsize::new(0));
        let peer = Peer {
            chain,
            hash: hash.into(),
            submissions: submissions.clone(),
            db: wallet.paths.data_db.clone(),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(tonic::transport::Server::builder().serve_with_shutdown(
            addr,
            peer,
            async {
                let _ = stopped.await;
            },
        ));
        tokio::task::yield_now().await;
        wallet
            .set_light_server(LightServer::parse(
                &format!("http://{addr}"),
                ZNetwork::Regtest,
            ))
            .unwrap();
        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
        for phase in ["signed", "unknown"] {
            conn.execute("UPDATE ext_native_payments_v1 SET phase=?1", [phase])
                .unwrap();
            let before: (String, String, Vec<u8>) = conn.query_row("SELECT p.phase,p.txids,t.raw FROM ext_native_payments_v1 p CROSS JOIN transactions t WHERE t.raw IS NOT NULL", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            let result = wallet.submit_payment(&id).await;
            if accepted {
                assert_eq!(result.unwrap().phase, "accepted");
            } else {
                assert!(
                    matches!(result, Err(EngineError::ChainMismatch { .. })),
                    "unexpected refusal: {result:?}"
                );
                let after: (String, String, Vec<u8>) = conn.query_row("SELECT p.phase,p.txids,t.raw FROM ext_native_payments_v1 p CROSS JOIN transactions t WHERE t.raw IS NOT NULL", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
                assert_eq!(before, after);
            }
        }
        assert_eq!(
            submissions.load(Ordering::SeqCst),
            if accepted { 2 } else { 0 }
        );
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn a_verified_light_connection_cannot_reconnect_to_a_replacement_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut wallet, _) = signed_fixture(dir.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let submissions = Arc::new(AtomicUsize::new(0));
    let peer = Peer {
        chain: "regtest",
        hash: super::super::chain_identity::genesis(ZNetwork::Regtest).into(),
        submissions: submissions.clone(),
        db: wallet.paths.data_db.clone(),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(tonic::transport::Server::builder().serve_with_shutdown(
        addr,
        peer.clone(),
        async {
            let _ = stopped.await;
        },
    ));
    tokio::task::yield_now().await;
    wallet
        .set_light_server(LightServer::parse(
            &format!("http://{addr}"),
            ZNetwork::Regtest,
        ))
        .unwrap();
    let owner = wallet.network_operation_lease().unwrap();
    let mut connection = Some(wallet.connect_submission(&owner).await.unwrap());
    rusqlite::Connection::open(&wallet.paths.data_db)
        .unwrap()
        .execute("UPDATE ext_native_payments_v1 SET phase='unknown'", [])
        .unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let replacement = Peer {
        chain: "main",
        hash: super::super::chain_identity::genesis(ZNetwork::Mainnet).into(),
        ..peer
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(tonic::transport::Server::builder().serve_with_shutdown(
        addr,
        replacement,
        async {
            let _ = stopped.await;
        },
    ));
    tokio::task::yield_now().await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        wallet.submit_raw(&mut connection, vec![0xfa, 0xce], &owner),
    )
    .await;
    assert!(matches!(result, Ok(Err(EngineError::Transport(_)))));
    assert_eq!(submissions.load(Ordering::SeqCst), 0);
    drop(connection);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
