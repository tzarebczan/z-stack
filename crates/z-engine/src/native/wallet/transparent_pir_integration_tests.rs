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

#[test]
fn historical_birthday_gap_provisional_replacement_accepts_independent_ancestor() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (mut fixture, mut wallet) = runtime.block_on(async {
        let fixture = Fixture::new_with_tip(root.path(), 4).await;
        let wallet = NativeWallet::restore_with_wallet_source(
            root.path(),
            PUBLIC_SEED,
            &fixture.source,
            3,
            auth(),
            0,
        )
        .await
        .unwrap()
        .0;
        wallet
            .sync_with_wallet_source_reported_cancellable(
                &fixture.source,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        (fixture, wallet)
    });
    let latest = runtime.block_on(async {
        let mut client = zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient::new(fixture.channel.clone());
        client.get_latest_block(zcash_client_backend::proto::service::ChainSpec::default()).await.unwrap().into_inner()
    });
    assert_eq!(latest.height, 4);
    assert_eq!(latest.hash, vec![4; 32]);
    let birthday = 3u64;
    let missing = wallet
        .open_db()
        .unwrap()
        .get_block_hash(1.into())
        .unwrap()
        .is_none();
    assert!(
        missing,
        "historical ancestor must be outside stored canonical block rows"
    );
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    let hash = |height| {
        if height == 0 {
            genesis
        } else {
            BlockHash::from_display_hex(&format!("{:02x}", height as u8).repeat(32)).unwrap()
        }
    };
    let old_dir = tempfile::tempdir().unwrap();
    let old_map = service::publish_for_network(
        old_dir.path(),
        &[
            service::ShardSpec::first(0, 1, true, vec![]),
            service::ShardSpec::first(2, 4, false, vec![]),
        ],
        service::SEAL,
        hash,
        vec![],
        "regtest",
        genesis,
    );
    let old_server = service::InProcess::serve(old_dir.path());
    let mut previous = None;
    let mut observed = std::collections::BTreeMap::new();
    let mut old_point_reads = 0;
    let mut prior_tail = None;
    for phase in 0..4 {
        if phase == 3 {
            fixture = runtime.block_on(Fixture::new_with_tip(root.path(), 5));
            runtime
                .block_on(wallet.sync_with_wallet_source_reported_cancellable(
                    &fixture.source,
                    None,
                    Arc::new(AtomicBool::new(false)),
                ))
                .unwrap();
        }
        wallet = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
        let next_dir = tempfile::tempdir().unwrap();
        let target = if phase == 3 { 5 } else { 4 };
        let mut tail = service::ShardSpec::first(2, target, false, vec![]);
        tail.revision = if phase == 3 { 2 } else { 1 };
        tail.supersedes = if phase == 3 {
            prior_tail.clone().unwrap()
        } else {
            old_map.shards[1].manifest_digest.clone()
        };
        let next_server = if phase > 0 {
            let map = service::publish_for_network(
                next_dir.path(),
                &[service::ShardSpec::first(0, 1, true, vec![]), tail],
                service::SEAL,
                hash,
                vec![],
                "regtest",
                genesis,
            );
            prior_tail = Some(map.shards[1].manifest_digest.clone());
            Some(service::InProcess::serve(next_dir.path()))
        } else {
            None
        };
        let server = next_server.as_ref().unwrap_or(&old_server);
        let mut native = wallet
            .begin_transparent_pir_with_source(
                &fixture.source,
                &"ab".repeat(32),
                Arc::new(RecoveryCancellation::new()),
            )
            .unwrap();
        let scope: Scope = serde_json::from_str(&native.scope_json().unwrap()).unwrap();
        assert_eq!(scope.anchor.height, target);
        if phase == 1 {
            // The continuation's SHA is a content address, not authentication.
            // Recompute it after tampering, while leaving the owner's saved
            // checkpoint, rows and sidecar untouched. Owned lineage must refuse.
            let mut forged: Checkpoint = previous.clone().unwrap();
            let mut bytes = STANDARD.decode(&forged.continuation).unwrap();
            let needle = "01".repeat(32).into_bytes();
            let offset = bytes
                .windows(64)
                .rposition(|window| window == needle)
                .unwrap();
            bytes[offset..offset + 64].copy_from_slice("ee".repeat(32).as_bytes());
            forged.continuation = STANDARD.encode(&bytes);
            use sha2::Digest;
            let mut digest = sha2::Sha256::new();
            digest.update(forged.source_id.as_bytes());
            digest.update(forged.scope_id.as_bytes());
            digest.update(forged.generation.to_le_bytes());
            digest.update(forged.anchor.height.to_le_bytes());
            digest.update(forged.anchor.hash.as_bytes());
            digest.update([u8::from(forged.complete)]);
            digest.update(&bytes);
            forged.checkpoint_id = crate::web::to_hex(&digest.finalize());
            assert_ne!(
                forged.checkpoint_id,
                previous.as_ref().unwrap().checkpoint_id
            );
            let mut attacker = Pass::new(
                scope.clone(),
                Some(forged),
                Limits {
                    max_exchanges: 256,
                    ..Limits::default()
                },
            )
            .unwrap();
            let owned_checkpoint = wallet.transparent_pir_checkpoint().unwrap();
            let owned_rows = rows(&wallet);
            let mut rejected = false;
            let mut chain_client = zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient::new(fixture.channel.clone());
            for _ in 0..2000 {
                match attacker.advance().unwrap() {
                    Step::Exchange {
                        request_id,
                        operation,
                    } => attacker
                        .accept(request_id, answer(server, operation))
                        .unwrap(),
                    Step::NeedChain { height, .. } => {
                        let point = runtime
                            .block_on(chain_client.get_tree_state(BlockId {
                                height,
                                hash: vec![],
                            }))
                            .unwrap()
                            .into_inner();
                        attacker.accept_chain(height, point.hash).unwrap();
                    }
                    Step::Checkpoint { checkpoint } => {
                        let json = serde_json::to_string(&checkpoint).unwrap();
                        let error = match attacker.accepted_rewind().unwrap() {
                            Some(witness) => wallet
                                .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                    &fixture.source,
                                    &mut native,
                                    &json,
                                    witness,
                                )
                                .unwrap_err(),
                            None => wallet
                                .apply_transparent_pir_checkpoint_with_source(
                                    &fixture.source,
                                    &mut native,
                                    &json,
                                )
                                .unwrap_err(),
                        };
                        assert_eq!(error.to_string(), "transparent_pir_checkpoint_invalid");
                        assert_eq!(
                            wallet.transparent_pir_checkpoint().unwrap(),
                            owned_checkpoint
                        );
                        assert!(
                            rows(&wallet) == owned_rows,
                            "owned wallet state changed unexpectedly"
                        );
                        rejected = true;
                        break;
                    }
                    Step::Stopped { .. } => {
                        panic!("forged continuation must reach owner rejection")
                    }
                }
            }
            assert!(rejected);
        }
        let mut pass = Pass::new(
            scope,
            previous.clone(),
            Limits {
                max_exchanges: 256,
                ..Limits::default()
            },
        )
        .unwrap();
        let mut client=zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient::new(fixture.channel.clone());
        let mut reached = false;
        for _ in 0..2000 {
            match pass.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => pass.accept(request_id, answer(server, operation)).unwrap(),
                Step::NeedChain { height, .. } => {
                    let point = runtime
                        .block_on(client.get_tree_state(BlockId {
                            height,
                            hash: vec![],
                        }))
                        .unwrap()
                        .into_inner();
                    assert_eq!(point.height, height);
                    assert_eq!(point.network, "regtest");
                    assert_eq!(point.hash, hash(height).to_display_hex());
                    if height == 1 {
                        old_point_reads += 1;
                    }
                    observed.insert(height, point.hash.clone());
                    pass.accept_chain(height, point.hash).unwrap();
                }
                Step::Checkpoint { checkpoint } => {
                    if let Some(a) = checkpoint.rewind_to.as_ref() {
                        reached = true;
                        assert!(phase > 0);
                        assert_eq!(a.height, 1);
                        assert_eq!(a.hash, observed[&1]);
                        assert!(a.height < birthday);
                        assert!(missing);
                        eprintln!("SAFE_CENSUS phase={} ancestor_height={} birthday={} db_hash_absent={} independently_observed={} cached_without_second_read={}",phase,a.height,birthday,missing,true,old_point_reads==1);
                    }
                    if phase == 1 && checkpoint.rewind_to.is_some() {
                        let calls = fixture.calls.lock().unwrap().len();
                        pass.finish_budget().unwrap();
                        assert_eq!(fixture.calls.lock().unwrap().len(), calls);
                    }
                    let json = serde_json::to_string(&checkpoint).unwrap();
                    let before = wallet.transparent_pir_checkpoint().unwrap();
                    if checkpoint.rewind_to.is_some() {
                        let error = wallet
                            .apply_transparent_pir_checkpoint_with_source(
                                &fixture.source,
                                &mut native,
                                &json,
                            )
                            .unwrap_err();
                        assert_eq!(error.to_string(), "transparent_pir_rewind_anchor_unknown");
                        assert!(
                            wallet.transparent_pir_checkpoint().unwrap() == before,
                            "owned wallet state changed unexpectedly"
                        );
                    }
                    if checkpoint.rewind_to.is_some() {
                        // Existing canonical evidence always wins over the witness fallback.
                        let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
                        conn.execute("INSERT INTO blocks (height,hash,time,sapling_tree) VALUES (1,?1,0,X'000000')",[vec![0xee;32]]).unwrap();
                        let error = wallet
                            .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                &fixture.source,
                                &mut native,
                                &json,
                                pass.accepted_rewind().unwrap().unwrap(),
                            )
                            .unwrap_err();
                        assert_eq!(error.to_string(), "transparent_pir_rewind_anchor_unknown");
                        assert!(
                            wallet.transparent_pir_checkpoint().unwrap() == before,
                            "owned wallet state changed unexpectedly"
                        );
                        conn.execute("DELETE FROM blocks WHERE height=1", [])
                            .unwrap();
                        drop(conn);
                        let mut changed = checkpoint.clone();
                        changed.pending_pages += 1;
                        let witness = pass.accepted_rewind().unwrap().unwrap();
                        let error = wallet
                            .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                &fixture.source,
                                &mut native,
                                &serde_json::to_string(&changed).unwrap(),
                                witness,
                            )
                            .unwrap_err();
                        assert_eq!(error.to_string(), "transparent_pir_rewind_evidence_invalid");
                        assert!(
                            wallet.transparent_pir_checkpoint().unwrap() == before,
                            "owned wallet state changed unexpectedly"
                        );
                        let mut cancelled = wallet
                            .begin_transparent_pir_with_source(
                                &fixture.source,
                                &"ab".repeat(32),
                                Arc::new(RecoveryCancellation::new()),
                            )
                            .unwrap();
                        cancelled.cancel();
                        let witness = pass.accepted_rewind().unwrap().unwrap();
                        let error = wallet
                            .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                &fixture.source,
                                &mut cancelled,
                                &json,
                                witness,
                            )
                            .unwrap_err();
                        assert_eq!(error.to_string(), "native_recovery_cancelled");
                        assert!(
                            wallet.transparent_pir_checkpoint().unwrap() == before,
                            "owned wallet state changed unexpectedly"
                        );
                    }
                    let result = match pass.accepted_rewind().unwrap() {
                        Some(witness) => wallet
                            .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                &fixture.source,
                                &mut native,
                                &json,
                                witness,
                            ),
                        None => wallet.apply_transparent_pir_checkpoint_with_source(
                            &fixture.source,
                            &mut native,
                            &json,
                        ),
                    };
                    assert!(result.is_ok(),"valid independently observed unchanged-chain historical rewind must apply: {}",result.err().map(|e|e.to_string()).unwrap_or_default());
                    let applied = wallet.transparent_pir_checkpoint().unwrap();
                    let applied_rows = rows(&wallet);
                    match pass.accepted_rewind().unwrap() {
                        Some(witness) => wallet
                            .apply_transparent_pir_checkpoint_with_source_and_rewind(
                                &fixture.source,
                                &mut native,
                                &json,
                                witness,
                            )
                            .unwrap(),
                        None => wallet
                            .apply_transparent_pir_checkpoint_with_source(
                                &fixture.source,
                                &mut native,
                                &json,
                            )
                            .unwrap(),
                    };
                    assert!(
                        wallet.transparent_pir_checkpoint().unwrap() == applied,
                        "owned wallet state changed unexpectedly"
                    );
                    assert!(
                        rows(&wallet) == applied_rows,
                        "owned wallet state changed unexpectedly"
                    );
                    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                    previous = Some(checkpoint);
                }
                Step::Stopped { status } => {
                    if phase == 1 {
                        assert_eq!(
                            status,
                            crate::transparent_pir::acquisition::Status::Incomplete {
                                reason: "budget".into()
                            }
                        );
                    } else {
                        assert_eq!(
                            status,
                            crate::transparent_pir::acquisition::Status::Complete
                        );
                    }
                    break;
                }
            }
        }
        if phase == 1 || phase == 3 {
            assert!(reached, "replacement must produce historical rewind");
        }
        assert_eq!(
            old_point_reads, 1,
            "cold and moving-anchor lineage reuse the authenticated old historical point"
        );
    }
}

// Authenticated fixture RPC is the chain boundary. The callback itself may not
// perform any RPC; this also models the caller's no-I/O checkpoint drain.
fn drive_observed_rewind(
    scope: Scope,
    previous: Option<Checkpoint>,
    server: &service::InProcess,
    fixture: &Fixture,
    runtime: &tokio::runtime::Runtime,
    mut apply: impl FnMut(&Pass, &Checkpoint),
) -> (Checkpoint, Vec<u64>) {
    let mut pass = Pass::new(
        scope,
        previous,
        Limits {
            max_exchanges: 256,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut last = None;
    let mut reads = Vec::new();
    let mut client = zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient::new(fixture.channel.clone());
    for _ in 0..2000 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => pass.accept(request_id, answer(server, operation)).unwrap(),
            Step::NeedChain { height, .. } => {
                let point = runtime
                    .block_on(client.get_tree_state(BlockId {
                        height,
                        hash: vec![],
                    }))
                    .unwrap()
                    .into_inner();
                assert_eq!(point.height, height);
                assert_eq!(point.network, "regtest");
                reads.push(height);
                pass.accept_chain(height, point.hash).unwrap();
            }
            Step::Checkpoint { checkpoint } => {
                let before = fixture.calls.lock().unwrap().len();
                apply(&pass, &checkpoint);
                assert_eq!(
                    fixture.calls.lock().unwrap().len(),
                    before,
                    "mint/apply must perform no I/O"
                );
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                last = Some(checkpoint);
            }
            Step::Stopped { status } => {
                assert_eq!(
                    status,
                    crate::transparent_pir::acquisition::Status::Complete
                );
                return (last.unwrap(), reads);
            }
        }
    }
    panic!("bounded observed-rewind fixture did not complete")
}

#[test]
fn historical_rewind_web_candidate_and_cold_replay_remain_scoped() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture = runtime.block_on(Fixture::new_with_tip(root.path(), 4));
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut wallet = crate::web::WebWallet::from_account(account, 3).unwrap();
    let mut client = zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient::new(fixture.channel.clone());
    let tree = runtime
        .block_on(client.get_tree_state(BlockId {
            height: 2,
            hash: vec![],
        }))
        .unwrap()
        .into_inner();
    wallet.apply_tree_state_json(&serde_json::json!({"network":tree.network,"height":tree.height,"hash":tree.hash,"saplingTree":tree.sapling_tree,"orchardTree":tree.orchard_tree,"ironwoodTree":tree.ironwood_tree}).to_string()).unwrap();
    wallet
        .apply_compact_blocks_blob(&crate::web::encode_delimited(vec![
            block(3, Network::Regtest.genesis_hash()),
            block(4, Network::Regtest.genesis_hash()),
        ]))
        .unwrap();
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    let hash = |height| {
        if height == 0 {
            genesis
        } else {
            BlockHash::from_display_hex(&format!("{:02x}", height as u8).repeat(32)).unwrap()
        }
    };
    let publication = tempfile::tempdir().unwrap();
    let first = service::publish_for_network(
        publication.path(),
        &[
            service::ShardSpec::first(0, 1, true, vec![]),
            service::ShardSpec::first(2, 4, false, vec![]),
        ],
        service::SEAL,
        hash,
        vec![],
        "regtest",
        genesis,
    );
    let old_server = service::InProcess::serve(publication.path());
    let replacement = tempfile::tempdir().unwrap();
    let mut tail = service::ShardSpec::first(2, 4, false, vec![]);
    tail.revision = 1;
    tail.supersedes = first.shards[1].manifest_digest.clone();
    service::publish_for_network(
        replacement.path(),
        &[service::ShardSpec::first(0, 1, true, vec![]), tail],
        service::SEAL,
        hash,
        vec![],
        "regtest",
        genesis,
    );
    let next_server = service::InProcess::serve(replacement.path());
    let mut previous = None;
    let mut historic_reads = 0;
    for phase in 0..3 {
        wallet = crate::web::WebWallet::from_snapshot(&wallet.to_snapshot().unwrap()).unwrap();
        let scope =
            serde_json::from_str(&wallet.begin_transparent_pir(&"ab".repeat(32)).unwrap()).unwrap();
        let expected_replay = (phase == 2).then(|| previous.clone().unwrap());
        let mut saw_rewind = false;
        let (last, reads) = drive_observed_rewind(
            scope,
            previous,
            if phase == 0 {
                &old_server
            } else {
                &next_server
            },
            &fixture,
            &runtime,
            |pass, checkpoint| {
                let json = serde_json::to_string(checkpoint).unwrap();
                let before = wallet.to_snapshot().unwrap();
                if checkpoint.rewind_to.is_some() {
                    saw_rewind = true;
                    assert!(phase > 0);
                    assert_eq!(checkpoint.rewind_to.as_ref().unwrap().height, 1);
                    assert_eq!(
                        wallet
                            .apply_transparent_pir_checkpoint(&json)
                            .unwrap_err()
                            .to_string(),
                        "transparent_pir_rewind_anchor_unknown"
                    );
                    assert!(
                        wallet.to_snapshot().unwrap() == before,
                        "owned wallet state changed unexpectedly"
                    );
                    // A different scope cannot supply an authority token for this owner.
                    // Only a missing canonical point admits the fallback. A known
                    // conflict or malformed stored point must never be replaced.
                    for canonical in ["ee".repeat(32), "malformed".into()] {
                        wallet.block_hashes.insert(1, canonical);
                        let conflicting = wallet.to_snapshot().unwrap();
                        assert_eq!(
                            wallet
                                .apply_transparent_pir_checkpoint_with_rewind(
                                    &json,
                                    pass.accepted_rewind().unwrap().unwrap()
                                )
                                .unwrap_err()
                                .to_string(),
                            "transparent_pir_rewind_anchor_unknown"
                        );
                        assert!(
                            wallet.to_snapshot().unwrap() == conflicting,
                            "owned wallet state changed unexpectedly"
                        );
                        wallet.block_hashes.remove(&1);
                    }
                    let mut changed = checkpoint.clone();
                    changed.source_id = "cd".repeat(32);
                    assert_eq!(
                        wallet
                            .apply_transparent_pir_checkpoint_with_rewind(
                                &serde_json::to_string(&changed).unwrap(),
                                pass.accepted_rewind().unwrap().unwrap()
                            )
                            .unwrap_err()
                            .to_string(),
                        "transparent_pir_rewind_evidence_invalid"
                    );
                    assert!(
                        wallet.to_snapshot().unwrap() == before,
                        "owned wallet state changed unexpectedly"
                    );
                }
                let (candidate, _) = match pass.accepted_rewind().unwrap() {
                    Some(witness) => wallet
                        .transparent_pir_checkpoint_snapshot_with_rewind(&json, witness)
                        .unwrap(),
                    None => wallet.transparent_pir_checkpoint_snapshot(&json).unwrap(),
                };
                assert!(
                    wallet.to_snapshot().unwrap() == before,
                    "candidate must not publish to live wallet"
                );
                let cold_candidate = crate::web::WebWallet::from_snapshot(&candidate).unwrap();
                assert!(
                    cold_candidate.transparent_pir_checkpoint().as_deref() == Some(json.as_str()),
                    "candidate lost its exact checkpoint"
                );
                match pass.accepted_rewind().unwrap() {
                    Some(witness) => wallet
                        .apply_transparent_pir_checkpoint_with_rewind(&json, witness)
                        .unwrap(),
                    None => wallet.apply_transparent_pir_checkpoint(&json).unwrap(),
                };
                let applied = wallet.to_snapshot().unwrap();
                match pass.accepted_rewind().unwrap() {
                    Some(witness) => wallet
                        .apply_transparent_pir_checkpoint_with_rewind(&json, witness)
                        .unwrap(),
                    None => wallet.apply_transparent_pir_checkpoint(&json).unwrap(),
                };
                assert!(
                    wallet.to_snapshot().unwrap() == applied,
                    "identical full context is an idempotent owned replay"
                );
                wallet
                    .acknowledge_transparent_pir_checkpoint(&checkpoint.checkpoint_id)
                    .unwrap();
            },
        );
        if let Some(expected) = expected_replay {
            // Reopening may normalize opaque acquisition journals. It must keep
            // the exact public facts and reuse the historical chain observation.
            let mut public = last.clone();
            public.checkpoint_id = expected.checkpoint_id.clone();
            public.previous_checkpoint_id = expected.previous_checkpoint_id.clone();
            public.continuation = expected.continuation.clone();
            assert!(
                public == expected,
                "cold resume changed public checkpoint facts"
            );
        }
        if phase == 1 {
            assert!(saw_rewind);
        }
        historic_reads += reads.iter().filter(|height| **height == 1).count();
        assert_eq!(
            historic_reads, 1,
            "cold exact-anchor replay reuses the previously observed historical point"
        );
        previous = Some(last);
    }
}
