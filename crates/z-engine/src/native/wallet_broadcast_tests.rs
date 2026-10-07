use super::*;
use crate::native::selective_scan;
use rusqlite::types::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use zcash_client_backend::data_api::OutputLockStore;
use zcash_client_backend::wallet::{LockOwner, OutputRef, WalletTransparentOutput};
use zcash_protocol::PoolType;

fn contents(path: &Path) -> BTreeMap<String, Vec<Vec<Value>>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let mut query = conn
                .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
                .unwrap();
            let width = query.column_count();
            let rows = query
                .query_map([], |row| (0..width).map(|i| row.get(i)).collect())
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            (name, rows)
        })
        .collect()
}

/// A real HTTP peer consumes the complete raw transaction and then either loses
/// the reply or rejects a later batch member. No validator or chain is involved.
fn validator(
    responses: Vec<Option<serde_json::Value>>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let job = std::thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(peer) => break peer,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        other => panic!("missing test broadcast: {other:?}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let (header_end, size) = loop {
                    let mut buf = [0; 1024];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("Content-Length: ")
                                    .map(|s| s.parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while request.len() < header_end + size {
                    let mut buf = [0; 1024];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&request[header_end..header_end + size]).unwrap();
                assert_eq!(body["method"], "sendrawtransaction");
                if let Some(response) = response {
                    let body = response.to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
                body["params"][0].as_str().unwrap().to_owned()
            })
            .collect()
    });
    (url, job)
}

#[tokio::test]
async fn uncertain_or_partially_rejected_submission_preserves_all_saved_state() {
    for partial_success in [false, true] {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = WalletPaths::new(dir.path());
        let mut db = open_wallet_db(&paths.data_db, ZNetwork::Regtest).unwrap();
        init_wallet_db(&mut db, None).unwrap();
        let seed = SecretVec::new(vec![7; 32]);
        let (account, _) = db
            .create_account(
                "broadcast regression",
                &seed,
                &AccountBirthday::from_parts(selective_scan::tests::empty_state(0), None),
                None,
            )
            .unwrap();
        db.update_chain_tip(100.into()).unwrap();
        let blocks: Vec<_> = (1..=100).map(selective_scan::tests::compact_at).collect();
        let mut offload = selective_scan::NativeOffload::new(0, 0, 0);
        selective_scan::scan_batch(
            ZNetwork::Regtest,
            &mut db,
            &selective_scan::tests::empty_state(0),
            1.into(),
            &blocks,
            &mut offload,
            None,
        )
        .unwrap()
        .unwrap();
        selective_scan::flush_remaining(&mut db, &mut offload).unwrap();
        let address = *db
            .get_transparent_receivers(account, true, true)
            .unwrap()
            .keys()
            .next()
            .unwrap();
        for id in [1, 2] {
            let output = WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new([id; 32], 0),
                transparent::bundle::TxOut::new(
                    Zatoshis::from_u64(150_000).unwrap(),
                    address.script().into(),
                ),
                Some(50.into()),
                Some(account),
                None,
                None,
            )
            .unwrap();
            db.put_received_transparent_utxo(&output).unwrap();
        }
        // This lock belongs to an unrelated proposal and must never be cleared.
        db.lock_outputs(
            &[OutputRef::new(
                TxId::from_bytes([2; 32]),
                PoolType::TRANSPARENT,
                0,
            )],
            LockOwner::new([9; 32]),
            200.into(),
        )
        .unwrap();
        let conn = rusqlite::Connection::open(&paths.data_db).unwrap();
        let mut ids = Vec::new();
        let mut raw_hex = Vec::new();
        for lock_time in [1u8, 2] {
            // Valid v1 encoding is sufficient to exercise persistence/transport;
            // the fake validator intentionally does not validate consensus.
            let raw = [1, 0, 0, 0, 0, 0, lock_time, 0, 0, 0];
            let tx = Transaction::read(raw.as_slice(), BranchId::Sprout).unwrap();
            let txid = tx.txid();
            conn.execute("INSERT INTO transactions (txid, raw, expiry_height, min_observed_height) VALUES (?1, ?2, 200, 100)", rusqlite::params![txid.as_ref(), raw.as_slice()]).unwrap();
            ids.push(txid);
            raw_hex.push(to_hex(&raw));
        }
        conn.execute("INSERT INTO transparent_received_output_spends (transparent_received_output_id, transaction_id) SELECT o.id, tx.id_tx FROM transparent_received_outputs o, transactions tx WHERE o.transaction_id=(SELECT id_tx FROM transactions WHERE txid=?1) AND tx.txid=?2", rusqlite::params![[1u8;32].as_slice(), ids[0].as_ref()]).unwrap();
        drop(conn);
        let responses = if partial_success {
            vec![
                Some(serde_json::json!({"result":ids[0].to_string(),"error":null})),
                Some(
                    serde_json::json!({"result":null,"error":{"code":-26,"message":"rejected test transaction"}}),
                ),
            ]
        } else {
            vec![None]
        };
        let (url, peer) = validator(responses);
        let wallet = NativeWallet {
            paths,
            network: ZNetwork::Regtest,
            // This unavailable gRPC endpoint is intentionally unnecessary when
            // the explicit validator is the selected submission transport.
            server: LightServer::Url("http://127.0.0.1:1".into()),
            meta: WalletMeta {
                network: "regtest".into(),
                server: "http://127.0.0.1:1".into(),
                birthday_height: 1,
                account_index: 0,
                validator_rpc: Some(url),
                ufvk: None,
                view_only: true,
                unlock_policy: UnlockPolicy::Session,
                os_unlock: false,
                allow_deep_sync: false,
            },
        };
        let before = contents(&wallet.paths.data_db);
        let error = wallet
            .broadcast_all(&mut db, ids.iter().copied())
            .await
            .unwrap_err();
        let sent = peer.join().unwrap();
        assert_eq!(sent, raw_hex[..if partial_success { 2 } else { 1 }]);
        assert!(matches!(error, EngineError::BroadcastFailed(_)));
        let message = error.to_string();
        assert!(message.contains(if partial_success {
            "submission rejected"
        } else {
            "submission outcome unknown"
        }));
        assert!(message.contains("saved transaction retained"));
        for txid in ids {
            assert!(message.contains(&txid.to_string()));
        }
        assert_eq!(contents(&wallet.paths.data_db), before, "submission errors must not clear locks, spends, raw transactions or rewind checkpoints/scan rows");
        assert_eq!(u32::from(db.chain_height().unwrap().unwrap()), 100);
        // Prove that the removed account-wide cleanup would damage this fixture.
        assert_eq!(db.clear_locked_outputs(account).unwrap(), 1);
    }
}
