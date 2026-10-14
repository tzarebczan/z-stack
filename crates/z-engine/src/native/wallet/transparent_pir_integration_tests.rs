//! Real v12 PIR retrieval projected into both canonical wallets, checked against
//! independent serialized transaction ingestion. This fixture tests wallet data
//! semantics; it does not claim consensus-valid signatures or proof of work.
use super::*;
use crate::keys::account_from_mnemonic;
use crate::transparent_pir::acquisition::{
    Checkpoint, Limits, Operation, Pass, Reply, Scope, Step, Table,
};
use crate::Network;
use base64::{engine::general_purpose::STANDARD, Engine};
use transparent_events::{ReceiveEvent, SpendEvent, TransparentEvent, Txid};
use transparent_filter::{BlockHash, ScriptBytes};
use zcash_client_backend::proto::compact_formats::{CompactTx, CompactTxIn, TxOut};

#[allow(dead_code)]
#[path = "../../../../z-transparent-pir-v2/tests/fixture.rs"]
mod service;

fn answer(server: &service::InProcess, operation: Operation) -> Reply {
    let table = |t: Table| match t {
        Table::Directory => "directory".into(),
        Table::Pages { child } => format!("pages-{child}"),
    };
    let (method, path, body) = match operation {
        Operation::Init => ("GET", "/v2/shards/init".into(), vec![]),
        Operation::Map => ("GET", "/v2/filters/shards".into(), vec![]),
        Operation::Filter { shard_id } => (
            "GET",
            format!("/v2/filters/shards/{shard_id}/filter"),
            vec![],
        ),
        Operation::Manifest { shard_id, revision } => (
            "GET",
            format!("/v2/shards/{shard_id}/revisions/{revision}/manifest"),
            vec![],
        ),
        Operation::Setup {
            shard_id,
            revision,
            table: t,
            segment,
        } => (
            "GET",
            format!(
                "/v2/shards/{shard_id}/revisions/{revision}/setup/{}/{segment}",
                table(t)
            ),
            vec![],
        ),
        Operation::Query {
            shard_id,
            revision,
            table: t,
            body,
        } => (
            "POST",
            format!(
                "/v2/shards/{shard_id}/revisions/{revision}/query/{}",
                table(t)
            ),
            STANDARD.decode(body).unwrap(),
        ),
    };
    let response = server.answer(
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(body)
            .unwrap(),
    );
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    Reply::Data {
        body: STANDARD.encode(response.body()),
    }
}
fn drive(
    scope: Scope,
    checkpoint: Option<Checkpoint>,
    server: &service::InProcess,
    mut apply: impl FnMut(&Checkpoint),
) -> Checkpoint {
    let mut pass = Pass::new(
        scope,
        checkpoint,
        Limits {
            max_exchanges: 256,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut latest = None;
    for _ in 0..2000 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => pass.accept(request_id, answer(server, operation)).unwrap(),
            Step::NeedChain { height, hash } => {
                let expected = if height == 0 {
                    Network::Regtest.genesis_hash().into()
                } else {
                    format!("{:02x}", height as u8).repeat(32)
                };
                assert_eq!(hash, expected);
                pass.accept_chain(height, expected).unwrap();
            }
            Step::Checkpoint { checkpoint } => {
                apply(&checkpoint);
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                latest = Some(checkpoint);
            }
            Step::Stopped { .. } => return latest.unwrap(),
        }
    }
    panic!("bounded fixture did not finish")
}
fn raw_transaction(previous: [u8; 32], index: u32, value: u64, script: &[u8]) -> Transaction {
    let mut raw = vec![];
    raw.extend(1u32.to_le_bytes());
    raw.push(1);
    raw.extend(previous);
    raw.extend(index.to_le_bytes());
    raw.push(0);
    raw.extend(u32::MAX.to_le_bytes());
    raw.push(1);
    raw.extend(value.to_le_bytes());
    raw.push(u8::try_from(script.len()).unwrap());
    raw.extend(script);
    raw.extend(0u32.to_le_bytes());
    let tx = Transaction::read(raw.as_slice(), BranchId::Sprout).unwrap();
    let mut canonical = vec![];
    tx.write(&mut canonical).unwrap();
    assert_eq!(canonical, raw);
    tx
}
fn rows(wallet: &NativeWallet) -> Vec<(Vec<u8>, u32, Vec<u8>, u64, u32, Option<Vec<u8>>)> {
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    let mut stmt=conn.prepare("SELECT t.txid,n.output_index,n.script,n.value_zat,t.mined_height,s.txid FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id LEFT JOIN transparent_received_output_spends l ON l.transparent_received_output_id=n.id LEFT JOIN transactions s ON s.id_tx=l.transaction_id ORDER BY t.txid,n.output_index").unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

#[test]
fn transparent_pir_real_service_matches_independent_transaction_native_and_web_ingestion() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let reference_root = tempfile::tempdir().unwrap();
    let (fixture, wallet, reference_fixture, reference) = runtime.block_on(async {
        let fixture = Fixture::new(root.path()).await;
        let wallet = fixture.restore(root.path()).await;
        wallet
            .sync_with_wallet_source_reported_cancellable(
                &fixture.source,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        let reference_fixture = Fixture::new(reference_root.path()).await;
        let reference = reference_fixture.restore(reference_root.path()).await;
        reference
            .sync_with_wallet_source_reported_cancellable(
                &reference_fixture.source,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        (fixture, wallet, reference_fixture, reference)
    });
    use zcash_keys::encoding::AddressCodec;
    let address = transparent::address::TransparentAddress::decode(
        &Network::Regtest,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    let script = script.0 .0;
    let funding = raw_transaction([9; 32], 0, 150000, &script);
    let outsider: transparent::address::Script =
        transparent::address::TransparentAddress::PublicKeyHash([7; 20])
            .script()
            .into();
    let spending = raw_transaction(*funding.txid().as_ref(), 0, 140000, &outsider.0 .0);
    // The source publication is derived from parsed serialized transactions.
    let received = funding.transparent_bundle().unwrap().vout[0].clone();
    let consumed = spending.transparent_bundle().unwrap().vin[0].prevout();
    let receive = TransparentEvent::Receive(ReceiveEvent {
        metadata: None,
        height: 1,
        txid: Txid(*funding.txid().as_ref()),
        transaction_index: 1,
        output_index: 0,
        value: u64::from(received.value()),
        coinbase: funding.transparent_bundle().unwrap().is_coinbase(),
    });
    let spend = TransparentEvent::Spend(SpendEvent {
        metadata: None,
        height: 2,
        spending_txid: Txid(*spending.txid().as_ref()),
        transaction_index: 1,
        input_index: 0,
        spent_txid: Txid(*consumed.hash()),
        spent_output_index: consumed.n(),
    });
    let publication = tempfile::tempdir().unwrap();
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    service::publish_for_network(
        publication.path(),
        &[service::ShardSpec::first(
            0,
            2,
            false,
            vec![
                (ScriptBytes::new(script.clone()), receive),
                (ScriptBytes::new(script.clone()), spend),
            ],
        )],
        service::SEAL,
        |h| {
            if h == 0 {
                genesis
            } else {
                BlockHash::from_internal_bytes([h as u8; 32])
            }
        },
        vec![],
        "regtest",
        genesis,
    );
    let server = service::InProcess::serve(publication.path());
    // Independent canonical reference consumes whole transactions, not source events.
    let mut db = reference.open_db().unwrap();
    db.transactionally(|wdb| -> anyhow::Result<()> {
        decrypt_and_store_transaction(&Network::Regtest, wdb, &funding, Some(1.into()))?;
        decrypt_and_store_transaction(&Network::Regtest, wdb, &spending, Some(2.into()))?;
        Ok(())
    })
    .unwrap();
    drop(db);
    let mut native = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: Scope = serde_json::from_str(&native.scope_json().unwrap()).unwrap();
    let checkpoint = drive(scope, None, &server, |cp| {
        wallet
            .apply_transparent_pir_checkpoint_with_source(
                &fixture.source,
                &mut native,
                &serde_json::to_string(cp).unwrap(),
            )
            .unwrap();
    });
    assert!(checkpoint.complete);
    assert_eq!(rows(&wallet), rows(&reference));
    assert_eq!(rows(&wallet).len(), 1);
    assert_eq!(rows(&wallet)[0].3, 150000);
    assert_eq!(rows(&wallet)[0].5, Some(spending.txid().as_ref().to_vec()));
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    assert_eq!(wallet.history(20).unwrap().len(), 2);
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut web = crate::web::WebWallet::from_account(account.clone(), 1).unwrap();
    let mut web_reference = crate::web::WebWallet::from_account(account, 1).unwrap();
    let mut first = block(1, Network::Regtest.genesis_hash());
    let mut second = block(2, Network::Regtest.genesis_hash());
    for b in [&mut first, &mut second] {
        b.vtx.push(CompactTx {
            index: 0,
            txid: vec![0x66 + b.height as u8; 32],
            ..Default::default()
        });
    }
    first.vtx.push(CompactTx {
        index: 1,
        txid: funding.txid().as_ref().to_vec(),
        vout: vec![TxOut {
            value: 150000,
            script_pub_key: script.clone(),
        }],
        ..Default::default()
    });
    second.vtx.push(CompactTx {
        index: 1,
        txid: spending.txid().as_ref().to_vec(),
        vin: vec![CompactTxIn {
            prevout_txid: funding.txid().as_ref().to_vec(),
            prevout_index: 0,
        }],
        vout: vec![TxOut {
            value: 140000,
            script_pub_key: outsider.0 .0,
        }],
        ..Default::default()
    });
    let compact = crate::web::encode_delimited(vec![first, second]);
    web.apply_compact_blocks_blob(&compact).unwrap();
    web_reference.apply_compact_blocks_blob(&compact).unwrap();
    web_reference.apply_transparent_blocks(&compact).unwrap();
    let scope =
        serde_json::from_str(&web.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
    drive(scope, None, &server, |cp| {
        web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
            .unwrap();
        let saved = web.to_snapshot().unwrap();
        assert!(!saved.is_empty());
        web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
            .unwrap();
    });
    let actual = web.history(20);
    let expected = web_reference.history(20);
    assert_eq!(actual.len(), 2);
    assert_eq!(actual.len(), expected.len());
    for e in &actual {
        let r = expected.iter().find(|r| r.txid == e.txid).unwrap();
        assert_eq!(
            (e.transparent_received, e.transparent_spent),
            (r.transparent_received, r.transparent_spent)
        );
    }
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "0");
    let restored = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    assert_eq!(restored.history(20).len(), 2);
    assert_eq!(restored.transparent_pir_status()["confirmedZat"], "0");
    // Finish any gap expansion, then reset and rescan to the unchanged tip.
    let scope =
        serde_json::from_str(&web.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
    let prior = serde_json::from_str(&web.transparent_pir_checkpoint().unwrap()).unwrap();
    drive(scope, Some(prior), &server, |cp| {
        web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
            .unwrap();
        web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
            .unwrap();
    });
    assert_eq!(
        web.transparent_pir_status()["completion"],
        "complete-for-enrolled-scope"
    );
    let continuation = web.transparent_pir_checkpoint().unwrap();
    web.reset_scan();
    assert!(web.history(20).is_empty());
    assert!(web.transparent_pir_checkpoint().unwrap() == continuation);
    let mut web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    web.apply_compact_blocks_blob(&compact).unwrap();
    let scope =
        serde_json::from_str(&web.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
    let prior = serde_json::from_str(&web.transparent_pir_checkpoint().unwrap()).unwrap();
    drive(scope, Some(prior), &server, |cp| {
        web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
            .unwrap();
        web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
            .unwrap();
    });
    assert_eq!(
        web.transparent_pir_status()["completion"],
        "complete-for-enrolled-scope"
    );
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "0");
    assert_eq!(web.history(20).len(), 2);
    assert_eq!(
        web.balance().transparent_available + web.balance().transparent_pending,
        0
    );
    drop(native);
    drop(wallet);
    let reopened = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    assert_eq!(rows(&reopened), rows(&reference));
    assert!(!fixture.calls.lock().unwrap().iter().any(|m| matches!(
        m.as_str(),
        "GetTransaction" | "GetAddressUtxos" | "SendTransaction"
    )));
    drop(server);
    drop(fixture);
    drop(reference_fixture);
    drop(runtime);
}

#[test]
fn web_reset_scan_keeps_real_malformed_page_cover_debt_across_cold_resumes() {
    use crate::transparent_pir::acquisition::Status;
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut web = crate::web::WebWallet::from_account(account, 1).unwrap();
    web.next_diversifier = 1;
    let compact =
        crate::web::encode_delimited((1..=3).map(|h| block(h, Network::Regtest.genesis_hash())));
    web.apply_compact_blocks_blob(&compact).unwrap();
    let script = web.public_transparent_scripts().unwrap()[0].0.clone();
    let events = (1..=3)
        .map(|h| {
            (
                ScriptBytes::new(script.clone()),
                TransparentEvent::Receive(ReceiveEvent {
                    metadata: None,
                    height: h,
                    txid: Txid([h as u8 + 0x70; 32]),
                    transaction_index: 0,
                    output_index: 0,
                    value: 100,
                    coinbase: false,
                }),
            )
        })
        .collect();
    let mut shard = service::ShardSpec::first(0, 3, true, events);
    shard.malformed_page_heads = true;
    let publication = tempfile::tempdir().unwrap();
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    service::publish_for_network(
        publication.path(),
        &[shard],
        service::SEAL,
        |h| {
            if h == 0 {
                genesis
            } else {
                BlockHash::from_internal_bytes([h as u8; 32])
            }
        },
        vec![],
        "regtest",
        genesis,
    );
    let server = service::InProcess::serve(publication.path());
    let scope: Scope =
        serde_json::from_str(&web.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
    let mut pass = Pass::new(scope, None, Limits::default()).unwrap();
    let mut first_pages = 0;
    let mut cancelled = false;
    for _ in 0..200 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => {
                first_pages += usize::from(matches!(
                    operation,
                    Operation::Query {
                        table: Table::Pages { .. },
                        ..
                    }
                ));
                pass.accept(request_id, answer(&server, operation)).unwrap();
            }
            Step::NeedChain { height, hash } => pass.accept_chain(height, hash).unwrap(),
            Step::Checkpoint { checkpoint } => {
                web.apply_transparent_pir_checkpoint(&serde_json::to_string(&checkpoint).unwrap())
                    .unwrap();
                web.acknowledge_transparent_pir_checkpoint(&checkpoint.checkpoint_id)
                    .unwrap();
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                if first_pages == 1 {
                    pass.cancel();
                    cancelled = true;
                    break;
                }
            }
            Step::Stopped { .. } => {
                panic!("must stop immediately after the failed head checkpoint")
            }
        }
    }
    assert!(cancelled);
    drop(pass);
    let continuation = web.transparent_pir_checkpoint().unwrap();
    web.reset_scan();
    assert!(web.transparent_pir_checkpoint().unwrap() == continuation);
    let mut web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    web.apply_compact_blocks_blob(&compact).unwrap();
    let mut pages = Vec::new();
    let mut reasons = Vec::new();
    for _ in 0..3 {
        let scope =
            serde_json::from_str(&web.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
        let checkpoint = serde_json::from_str(&web.transparent_pir_checkpoint().unwrap()).unwrap();
        let mut pass = Pass::new(
            scope,
            Some(checkpoint),
            Limits {
                max_queries: 1,
                ..Limits::default()
            },
        )
        .unwrap();
        let mut count = 0;
        let mut stopped = false;
        for _ in 0..200 {
            match pass.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => {
                    count += usize::from(matches!(
                        operation,
                        Operation::Query {
                            table: Table::Pages { .. },
                            ..
                        }
                    ));
                    pass.accept(request_id, answer(&server, operation)).unwrap();
                }
                Step::NeedChain { height, hash } => pass.accept_chain(height, hash).unwrap(),
                Step::Checkpoint { checkpoint } => {
                    web.apply_transparent_pir_checkpoint(
                        &serde_json::to_string(&checkpoint).unwrap(),
                    )
                    .unwrap();
                    web.acknowledge_transparent_pir_checkpoint(&checkpoint.checkpoint_id)
                        .unwrap();
                    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                }
                Step::Stopped {
                    status: Status::Incomplete { reason },
                } => {
                    reasons.push(reason);
                    stopped = true;
                    break;
                }
                Step::Stopped { .. } => panic!("malformed content must remain incomplete"),
            }
        }
        assert!(stopped);
        pages.push(count);
        web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    }
    assert_eq!(pages, vec![1, 1, 1]);
    assert_eq!(
        reasons,
        vec!["query_budget", "query_budget", "content_failed"]
    );
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "200");
    assert_eq!(web.history(20).len(), 2);
    assert_eq!(web.transparent_pir_status()["completion"], "incomplete");
}

#[path = "transparent_pir_host_reorg_tests.rs"]
mod host_reorg;
