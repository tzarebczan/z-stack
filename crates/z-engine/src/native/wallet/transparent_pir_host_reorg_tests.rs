//! Composed engine-host recovery on a synthetic accepted chain, with genuine PIR.
use super::*;
use crate::transparent_pir::acquisition::Status;
use zcash_client_backend::data_api::OutputLockStore;
use zcash_client_backend::wallet::{LockOwner, OutputRef};
use zcash_protocol::PoolType;

const SOURCE: &str = "abababababababababababababababababababababababababababababababab";
fn accepted(height: u64, fork: bool) -> String {
    if height == 0 {
        Network::Regtest.genesis_hash().into()
    } else {
        format!(
            "{:02x}",
            (height as u8).wrapping_add(if fork && height >= 2 { 0x80 } else { 0 })
        )
        .repeat(32)
    }
}
fn walk(
    scope: Scope,
    previous: Option<Checkpoint>,
    server: &service::InProcess,
    fork: bool,
    limits: Limits,
    stop_below: Option<u64>,
    mut apply: impl FnMut(&Checkpoint),
) -> (Checkpoint, bool) {
    let mut latest = previous.clone();
    let mut pass = Pass::new(scope, previous, limits).unwrap();
    let mut rewind = false;
    for _ in 0..3000 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => pass.accept(request_id, answer(server, operation)).unwrap(),
            Step::NeedChain { height, .. } => {
                pass.accept_chain(height, accepted(height, fork)).unwrap()
            }
            Step::Checkpoint { checkpoint } => {
                if let Some(point) = &checkpoint.rewind_to {
                    assert_eq!(point.hash, accepted(point.height, fork));
                    rewind = true;
                }
                apply(&checkpoint);
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                let stop = stop_below.is_some_and(|h| checkpoint.covered_through < h);
                latest = Some(checkpoint);
                if stop {
                    pass.cancel();
                    return (latest.unwrap(), rewind);
                }
            }
            Step::Stopped { status } => {
                if stop_below.is_none() {
                    assert_eq!(status, Status::Complete);
                }
                return (latest.unwrap(), rewind);
            }
        }
    }
    panic!("bounded engine host fixture did not stop")
}
fn publish(path: &Path, script: &[u8], fork: bool) -> service::InProcess {
    let receive = TransparentEvent::Receive(ReceiveEvent {
        metadata: None,
        height: 1,
        txid: Txid([0x11; 32]),
        transaction_index: 1,
        output_index: 0,
        value: 150000,
        coinbase: false,
    });
    let spend = TransparentEvent::Spend(SpendEvent {
        metadata: None,
        height: 2,
        spending_txid: Txid([0x22; 32]),
        transaction_index: 1,
        input_index: 0,
        spent_txid: Txid([0x11; 32]),
        spent_output_index: 0,
    });
    let events = if fork {
        vec![receive]
    } else {
        vec![receive, spend]
    };
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    service::publish_for_network(
        path,
        &[service::ShardSpec::first(
            0,
            2,
            false,
            events
                .into_iter()
                .map(|e| (ScriptBytes::new(script.to_vec()), e))
                .collect(),
        )],
        service::SEAL,
        |height| BlockHash::from_display_hex(&accepted(height, fork)).unwrap(),
        vec![],
        "regtest",
        genesis,
    );
    service::InProcess::serve(path)
}
#[test]
fn composed_native_same_height_fork_preserves_private_lineage_and_protected_state() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (fixture, wallet) = runtime.block_on(async {
        let f = Fixture::new(root.path()).await;
        let w = f.restore(root.path()).await;
        w.sync_with_wallet_source_reported_cancellable(
            &f.source,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        (f, w)
    });
    use zcash_keys::encoding::AddressCodec;
    let address = transparent::address::TransparentAddress::decode(
        &Network::Regtest,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    let script = script.0 .0;
    let old_dir = tempfile::tempdir().unwrap();
    let old = publish(old_dir.path(), &script, false);
    let independent = raw_transaction([0x91; 32], 0, 70000, &script);
    wallet
        .open_db()
        .unwrap()
        .transactionally(|db| -> anyhow::Result<()> {
            decrypt_and_store_transaction(&Network::Regtest, db, &independent, Some(1.into()))?;
            Ok(())
        })
        .unwrap();
    for _ in 0..4 {
        let mut host = wallet
            .begin_transparent_pir_with_source(
                &fixture.source,
                SOURCE,
                Arc::new(RecoveryCancellation::new()),
            )
            .unwrap();
        let scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
        let prior = wallet
            .transparent_pir_checkpoint()
            .unwrap()
            .map(|s| serde_json::from_str(&s).unwrap());
        walk(scope, prior, &old, false, Limits::default(), None, |cp| {
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut host,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        });
        drop(host);
        if wallet.transparent_pir_status().unwrap()["completion"] == "complete-for-enrolled-scope" {
            break;
        }
    }
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["completion"],
        "complete-for-enrolled-scope"
    );
    let native_saved = wallet.transparent_pir_checkpoint().unwrap().unwrap();
    // A canonical scanner may rewind and return to the very same accepted hash.
    // The unchanged acquisition checkpoint must reconstruct its erased effects.
    wallet
        .open_db()
        .unwrap()
        .truncate_to_height(1.into())
        .unwrap();
    runtime
        .block_on(wallet.sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ))
        .unwrap();
    let mut replay = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope = serde_json::from_str(&replay.scope_json().unwrap()).unwrap();
    walk(
        scope,
        Some(serde_json::from_str(&native_saved).unwrap()),
        &old,
        false,
        Limits::default(),
        None,
        |cp| {
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut replay,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        },
    );
    drop(replay);
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0",
        "same-branch replay must restore the canonical spend"
    );
    let native_saved = wallet.transparent_pir_checkpoint().unwrap().unwrap();
    let (intent, raw) = saved_transaction(&wallet);
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    conn.execute(
        "UPDATE ext_native_payments_v1 SET phase='unknown' WHERE id=?1",
        [&intent],
    )
    .unwrap();
    conn.execute("UPDATE transparent_received_outputs SET lock_owner=?1,lock_expiry_height=100 WHERE transaction_id IN(SELECT id_tx FROM transactions WHERE txid=?2)",rusqlite::params![[9u8;32].as_slice(),independent.txid().as_ref()]).unwrap();
    // Canonical SQLite rollback and independently selected compact rescan own chain authority.
    wallet
        .open_db()
        .unwrap()
        .truncate_to_height(1.into())
        .unwrap();
    fixture.mode.store(4, Ordering::Release);
    runtime
        .block_on(wallet.sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ))
        .unwrap();
    assert_eq!(
        wallet
            .open_db()
            .unwrap()
            .get_block_hash(2.into())
            .unwrap()
            .unwrap()
            .to_string(),
        accepted(2, true)
    );
    let mut host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    assert!(
        wallet.transparent_pir_checkpoint().unwrap().as_deref() == Some(native_saved.as_str()),
        "native begin must retain exact private evidence"
    );
    let scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let new = publish(new_dir.path(), &script, true);
    let prior = Some(serde_json::from_str(&native_saved).unwrap());
    let canonical_before_partial = wallet.transparent_pir_status().unwrap()["confirmedZat"].clone();
    let (_, rewind) = walk(scope, prior, &new, true, Limits::default(), None, |cp| {
        wallet
            .apply_transparent_pir_checkpoint_with_source(
                &fixture.source,
                &mut host,
                &serde_json::to_string(cp).unwrap(),
            )
            .unwrap();
        if !cp.complete {
            assert_eq!(
                wallet.transparent_pir_status().unwrap()["confirmedZat"],
                canonical_before_partial,
                "header-only progress must preserve canonical effects until verified rewind"
            );
            assert_eq!(wallet.payment_receipt(&intent).unwrap().phase, "unknown");
        }
    });
    assert!(
        rewind,
        "core must emit independently authenticated rollback"
    );
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    assert_eq!(wallet.payment_receipt(&intent).unwrap().phase, "unknown");
    let stored: Vec<u8> = conn
        .query_row(
            "SELECT raw FROM transactions WHERE created IS NOT NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored == raw, "unknown transaction bytes must be unchanged");
    let independent_state:(u64,u32,u32)=conn.query_row("SELECT n.value_zat,t.mined_height,n.lock_expiry_height FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1",[independent.txid().as_ref()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(independent_state, (70000, 1, 100));
    drop(host);
    drop(wallet);
    let reopened = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    assert_eq!(reopened.payment_receipt(&intent).unwrap().phase, "unknown");

    assert!(!fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|m| matches!(m.as_str(), "GetAddressUtxos" | "SendTransaction")));
}

#[test]
fn composed_web_same_height_fork_preserves_private_lineage_and_protected_state() {
    let mut derived = crate::web::WebWallet::from_account(
        account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap(),
        1,
    )
    .unwrap();
    let script = derived.public_transparent_scripts().unwrap()[0].0.clone();
    let independent = raw_transaction([0x91; 32], 0, 70000, &script);
    let pending = raw_transaction([0x92; 32], 0, 1000, &script);
    let mut raw = vec![];
    pending.write(&mut raw).unwrap();
    let old_dir = tempfile::tempdir().unwrap();
    let old = publish(old_dir.path(), &script, false);
    let new_dir = tempfile::tempdir().unwrap();
    let new = publish(new_dir.path(), &script, true);
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut web = crate::web::WebWallet::from_account(account, 1).unwrap();
    let mut blocks: Vec<_> = (1..=2)
        .map(|h| block(h, Network::Regtest.genesis_hash()))
        .collect();
    blocks[0].vtx.push(CompactTx {
        index: 0,
        txid: vec![0x61; 32],
        ..Default::default()
    });
    blocks[1].vtx.push(CompactTx {
        index: 0,
        txid: vec![0x62; 32],
        ..Default::default()
    });
    blocks[0].vtx.push(CompactTx {
        index: 1,
        txid: independent.txid().as_ref().to_vec(),
        vout: vec![TxOut {
            value: 70000,
            script_pub_key: script.clone(),
        }],
        ..Default::default()
    });
    let compact = crate::web::encode_delimited(blocks);
    web.apply_compact_blocks_blob(&compact).unwrap();
    web.apply_transparent_blocks(&compact).unwrap();
    for _ in 0..4 {
        let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
        let prior = web
            .transparent_pir_checkpoint()
            .map(|s| serde_json::from_str(&s).unwrap());
        walk(scope, prior, &old, false, Limits::default(), None, |cp| {
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        });
        if web.transparent_pir_status()["completion"] == "complete-for-enrolled-scope" {
            break;
        }
    }
    assert_eq!(
        web.transparent_pir_status()["completion"],
        "complete-for-enrolled-scope"
    );
    let web_saved = web.transparent_pir_checkpoint().unwrap();
    web.store_pending_tx(&Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap())
        .unwrap();
    let pending = web.pending_raw_txs();
    web.lock_outputs(
        &[OutputRef::new(independent.txid(), PoolType::TRANSPARENT, 0)],
        LockOwner::new([9; 32]),
        100.into(),
    )
    .unwrap();
    web.rewind_to_height_checked(1).unwrap();
    assert!(
        web.transparent_pir_checkpoint().as_deref() == Some(web_saved.as_str()),
        "real web rewind must retain private evidence"
    );
    assert!(
        web.begin_transparent_pir(SOURCE).is_err(),
        "lower target is closed until canonical scanning catches up"
    );
    assert!(
        web.pending_raw_txs() == pending,
        "pending transaction bytes must be unchanged"
    );
    let mut second = block(2, Network::Regtest.genesis_hash());
    second.hash = vec![0x82; 32];
    web.apply_compact_blocks_blob(&crate::web::encode_delimited([second]))
        .unwrap();
    let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
    assert!(
        web.transparent_pir_checkpoint().as_deref() == Some(web_saved.as_str()),
        "same-height web begin must retain evidence"
    );
    let (_, rewind) = walk(
        scope,
        Some(serde_json::from_str(&web_saved).unwrap()),
        &new,
        true,
        Limits::default(),
        None,
        |cp| {
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        },
    );
    assert!(rewind);
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "150000");
    assert!(
        web.pending_raw_txs() == pending,
        "pending transaction bytes must be unchanged"
    );
    assert_eq!(web.output_locks.len(), 1);
    assert!(web.utxos.iter().any(
        |u| u.txid == crate::web::to_hex(independent.txid().as_ref())
            && u.value_zat == 70000
            && u.height == 1
    ));
    let web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "150000");
    assert!(
        web.pending_raw_txs() == pending,
        "pending transaction bytes must be unchanged"
    );
}

#[test]
fn composed_native_web_scope_growth_accepts_lower_core_frontier_and_cold_recovers() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (fixture, wallet) = runtime.block_on(async {
        let f = Fixture::new(root.path()).await;
        let w = f.restore(root.path()).await;
        w.sync_with_wallet_source_reported_cancellable(
            &f.source,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        (f, w)
    });
    let host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let native_initial: Scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
    drop(host);
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut web = crate::web::WebWallet::from_account(account.clone(), 1).unwrap();
    web.apply_compact_blocks_blob(&crate::web::encode_delimited(
        (1..=2).map(|h| block(h, Network::Regtest.genesis_hash())),
    ))
    .unwrap();
    let web_initial: Scope =
        serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
    let mut future = crate::web::WebWallet::from_account(account, 1).unwrap();
    future.next_diversifier = 128;
    let future_script = future
        .public_transparent_scripts()
        .unwrap()
        .into_iter()
        .map(|(s, _)| s)
        .find(|s| {
            let hex = crate::web::to_hex(s);
            !native_initial.scripts.iter().any(|s| s.script == hex)
                && !web_initial.scripts.iter().any(|s| s.script == hex)
        })
        .unwrap();
    let native_future_key =
        UnifiedSpendingKey::from_seed(&ZNetwork::Regtest, &[7; 32], Zip32AccountId::ZERO)
            .unwrap()
            .to_unified_full_viewing_key();
    let public = native_future_key
        .transparent()
        .unwrap()
        .derive_address_pubkey(
            transparent::keys::TransparentKeyScope::EXTERNAL,
            transparent::keys::NonHardenedChildIndex::from_index(0).unwrap(),
        )
        .unwrap();
    let address = transparent::address::TransparentAddress::from_pubkey(&public);
    let native_future_script: transparent::address::Script = address.script().into();
    let publication = tempfile::tempdir().unwrap();
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    service::publish_for_network(
        publication.path(),
        &[service::ShardSpec::first(
            0,
            2,
            false,
            vec![
                (
                    ScriptBytes::new(future_script.clone()),
                    TransparentEvent::Receive(ReceiveEvent {
                        metadata: None,
                        height: 1,
                        txid: Txid([0x55; 32]),
                        transaction_index: 0,
                        output_index: 0,
                        value: 17000,
                        coinbase: false,
                    }),
                ),
                (
                    ScriptBytes::new(native_future_script.0 .0),
                    TransparentEvent::Receive(ReceiveEvent {
                        metadata: None,
                        height: 1,
                        txid: Txid([0x66; 32]),
                        transaction_index: 1,
                        output_index: 0,
                        value: 17000,
                        coinbase: false,
                    }),
                ),
            ],
        )],
        service::SEAL,
        |h| BlockHash::from_display_hex(&accepted(h, false)).unwrap(),
        vec![],
        "regtest",
        genesis,
    );
    let server = service::InProcess::serve(publication.path());
    let mut host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    walk(
        serde_json::from_str(&host.scope_json().unwrap()).unwrap(),
        None,
        &server,
        false,
        Limits::default(),
        None,
        |cp| {
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut host,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        },
    );
    drop(host);
    let native_saved = wallet.transparent_pir_checkpoint().unwrap().unwrap();
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["coveredThrough"],
        2
    );
    walk(
        web_initial,
        None,
        &server,
        false,
        Limits::default(),
        None,
        |cp| {
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        },
    );
    let web_saved = web.transparent_pir_checkpoint().unwrap();
    assert_eq!(web.transparent_pir_status()["coveredThrough"], 2);
    // A real account import expands the wallet-owned inventory. Both newly
    // imported and issued receivers have independent publisher fixture events.
    {
        let mut db = wallet.open_db().unwrap();
        let ufvk =
            UnifiedSpendingKey::from_seed(&ZNetwork::Regtest, &[7; 32], Zip32AccountId::ZERO)
                .unwrap()
                .to_unified_full_viewing_key();
        let birthday = AccountBirthday::from_parts(
            zcash_client_backend::data_api::chain::ChainState::empty(
                2.into(),
                zcash_primitives::block::BlockHash([2; 32]),
            ),
            None,
        );
        db.import_account_ufvk(
            "scope growth fixture",
            &ufvk,
            &birthday,
            AccountPurpose::Spending { derivation: None },
            None,
        )
        .unwrap();
    }
    let pass = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let new_scope: Scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    drop(pass);
    assert_ne!(new_scope.scope_id, native_initial.scope_id);
    assert!(
        wallet.transparent_pir_checkpoint().unwrap().as_deref() == Some(native_saved.as_str()),
        "scope growth must retain acquisition"
    );
    let mut host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let (partial, _) = walk(
        new_scope,
        Some(serde_json::from_str(&native_saved).unwrap()),
        &server,
        false,
        Limits::default(),
        Some(2),
        |cp| {
            assert!(cp.rewind_to.is_none());
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut host,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        },
    );
    drop(host);
    assert!(!partial.complete);
    assert_eq!(partial.covered_through, 0);
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["completion"],
        "incomplete"
    );
    drop(wallet);
    let wallet = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    for _ in 0..4 {
        let mut host = wallet
            .begin_transparent_pir_with_source(
                &fixture.source,
                SOURCE,
                Arc::new(RecoveryCancellation::new()),
            )
            .unwrap();
        let scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
        let cp = wallet
            .transparent_pir_checkpoint()
            .unwrap()
            .map(|s| serde_json::from_str(&s).unwrap());
        walk(scope, cp, &server, false, Limits::default(), None, |cp| {
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut host,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        });
        drop(host);
        if wallet.transparent_pir_status().unwrap()["completion"] == "complete-for-enrolled-scope" {
            break;
        }
    }
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "17000"
    );
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["completion"],
        "complete-for-enrolled-scope"
    );
    web.next_diversifier = 128;
    let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
    assert!(
        web.transparent_pir_checkpoint().as_deref() == Some(web_saved.as_str()),
        "web growth must retain acquisition"
    );
    let (partial, _) = walk(
        scope,
        Some(serde_json::from_str(&web_saved).unwrap()),
        &server,
        false,
        Limits::default(),
        Some(2),
        |cp| {
            assert!(cp.rewind_to.is_none());
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        },
    );
    assert_eq!(partial.covered_through, 0);
    assert!(!partial.complete);
    let mut web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    for _ in 0..4 {
        let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
        let cp = web
            .transparent_pir_checkpoint()
            .map(|s| serde_json::from_str(&s).unwrap());
        walk(scope, cp, &server, false, Limits::default(), None, |cp| {
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        });
        if web.transparent_pir_status()["completion"] == "complete-for-enrolled-scope" {
            break;
        }
    }
    assert_eq!(web.transparent_pir_status()["confirmedZat"], "17000");
    assert_eq!(
        web.transparent_pir_status()["completion"],
        "complete-for-enrolled-scope"
    );
}

fn seed_debt(
    scope: Scope,
    server: &service::InProcess,
    mut apply: impl FnMut(&Checkpoint),
) -> Checkpoint {
    let mut pass = Pass::new(scope, None, Limits::default()).unwrap();
    let mut pages = 0;
    for _ in 0..1000 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => {
                pages += usize::from(matches!(
                    operation,
                    Operation::Query {
                        table: Table::Pages { .. },
                        ..
                    }
                ));
                pass.accept(request_id, answer(server, operation)).unwrap();
            }
            Step::NeedChain { height, .. } => {
                pass.accept_chain(height, accepted(height, false)).unwrap()
            }
            Step::Checkpoint { checkpoint } => {
                apply(&checkpoint);
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                if pages == 1 {
                    assert!(checkpoint.pending_pages > 0);
                    assert!(!checkpoint.complete);
                    pass.cancel();
                    return checkpoint;
                }
            }
            Step::Stopped { .. } => panic!("malformed head must checkpoint before stopping"),
        }
    }
    panic!("bounded debt fixture did not stop")
}
fn undecided_budget(scope: Scope, prior: Checkpoint) {
    let mut pass = Pass::new(scope, Some(prior), Limits::default()).unwrap();
    assert!(matches!(
        pass.advance().unwrap(),
        Step::Exchange {
            operation: Operation::Init,
            ..
        }
    ));
    pass.finish_budget().unwrap();
    assert!(
        matches!(
            pass.advance().unwrap(),
            Step::Stopped {
                status: Status::Incomplete { .. }
            }
        ),
        "undecided lineage must not replace the private journal"
    );
}
fn pay_debt(
    scope: Scope,
    prior: Checkpoint,
    server: &service::InProcess,
    mut apply: impl FnMut(&Checkpoint),
) -> (usize, String) {
    let mut pass = Pass::new(
        scope,
        Some(prior),
        Limits {
            max_queries: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut pages = 0;
    for _ in 0..1000 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => {
                pages += usize::from(matches!(
                    operation,
                    Operation::Query {
                        table: Table::Pages { .. },
                        ..
                    }
                ));
                pass.accept(request_id, answer(server, operation)).unwrap();
            }
            Step::NeedChain { height, .. } => {
                pass.accept_chain(height, accepted(height, false)).unwrap()
            }
            Step::Checkpoint { checkpoint } => {
                apply(&checkpoint);
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
            }
            Step::Stopped {
                status: Status::Incomplete { reason },
            } => return (pages, reason),
            Step::Stopped { .. } => panic!("malformed history cannot be complete"),
        }
    }
    panic!("bounded cover payment did not stop")
}
#[test]
fn composed_native_web_fork_and_cold_budget_keep_real_owed_cover_queries() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (fixture, wallet) = runtime.block_on(async {
        let f = Fixture::new(root.path()).await;
        let w = f.restore(root.path()).await;
        w.sync_with_wallet_source_reported_cancellable(
            &f.source,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        (f, w)
    });
    use zcash_keys::encoding::AddressCodec;
    let address = transparent::address::TransparentAddress::decode(
        &Network::Regtest,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    let script = script.0 .0;
    let events = (0..3)
        .map(|i| {
            (
                ScriptBytes::new(script.clone()),
                TransparentEvent::Receive(ReceiveEvent {
                    metadata: None,
                    height: if i == 0 { 1 } else { 2 },
                    txid: Txid([0x70 + i; 32]),
                    transaction_index: u16::from(i),
                    output_index: 0,
                    value: 100,
                    coinbase: false,
                }),
            )
        })
        .collect();
    let mut shard = service::ShardSpec::first(0, 2, false, events);
    shard.malformed_page_heads = true;
    let publication = tempfile::tempdir().unwrap();
    let genesis = BlockHash::from_display_hex(Network::Regtest.genesis_hash()).unwrap();
    service::publish_for_network(
        publication.path(),
        &[shard],
        service::SEAL,
        |h| BlockHash::from_display_hex(&accepted(h, false)).unwrap(),
        vec![],
        "regtest",
        genesis,
    );
    let server = service::InProcess::serve(publication.path());
    let mut host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
    seed_debt(scope, &server, |cp| {
        wallet
            .apply_transparent_pir_checkpoint_with_source(
                &fixture.source,
                &mut host,
                &serde_json::to_string(cp).unwrap(),
            )
            .unwrap();
    });
    drop(host);
    let saved = wallet.transparent_pir_checkpoint().unwrap().unwrap();
    wallet
        .open_db()
        .unwrap()
        .truncate_to_height(1.into())
        .unwrap();
    assert!(wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new())
        )
        .is_err());
    assert!(
        wallet.transparent_pir_checkpoint().unwrap().as_deref() == Some(saved.as_str()),
        "lower native target must not discard owed queries"
    );
    fixture.mode.store(4, Ordering::Release);
    runtime
        .block_on(wallet.sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ))
        .unwrap();
    let host = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            SOURCE,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    undecided_budget(
        serde_json::from_str(&host.scope_json().unwrap()).unwrap(),
        serde_json::from_str(&saved).unwrap(),
    );
    drop(host);
    assert!(
        wallet.transparent_pir_checkpoint().unwrap().as_deref() == Some(saved.as_str()),
        "native fork budget must retain debt exactly"
    );
    drop(wallet);
    let wallet = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    wallet
        .open_db()
        .unwrap()
        .truncate_to_height(1.into())
        .unwrap();
    fixture.mode.store(0, Ordering::Release);
    runtime
        .block_on(wallet.sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        ))
        .unwrap();
    let mut native_counts = vec![];
    let mut native_reasons = vec![];
    for _ in 0..3 {
        let mut host = wallet
            .begin_transparent_pir_with_source(
                &fixture.source,
                SOURCE,
                Arc::new(RecoveryCancellation::new()),
            )
            .unwrap();
        let scope = serde_json::from_str(&host.scope_json().unwrap()).unwrap();
        let cp =
            serde_json::from_str(&wallet.transparent_pir_checkpoint().unwrap().unwrap()).unwrap();
        let (count, reason) = pay_debt(scope, cp, &server, |cp| {
            wallet
                .apply_transparent_pir_checkpoint_with_source(
                    &fixture.source,
                    &mut host,
                    &serde_json::to_string(cp).unwrap(),
                )
                .unwrap();
        });
        drop(host);
        native_counts.push(count);
        native_reasons.push(reason);
    }
    assert_eq!(native_counts, vec![1, 1, 1]);
    assert_eq!(
        native_reasons,
        vec!["query_budget", "query_budget", "content_failed"]
    );
    let account = account_from_mnemonic(PUBLIC_SEED, Network::Regtest, 0).unwrap();
    let mut web = crate::web::WebWallet::from_account(account, 1).unwrap();
    web.apply_compact_blocks_blob(&crate::web::encode_delimited(
        (1..=2).map(|h| block(h, Network::Regtest.genesis_hash())),
    ))
    .unwrap();
    let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
    seed_debt(scope, &server, |cp| {
        web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
            .unwrap();
        web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
            .unwrap();
    });
    let saved = web.transparent_pir_checkpoint().unwrap();
    web.rewind_to_height_checked(1).unwrap();
    assert!(
        web.transparent_pir_checkpoint().as_deref() == Some(saved.as_str()),
        "canonical web rewind must retain genuine owed queries"
    );
    assert!(web.begin_transparent_pir(SOURCE).is_err());
    let mut web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    let mut second = block(2, Network::Regtest.genesis_hash());
    second.hash = vec![0x82; 32];
    web.apply_compact_blocks_blob(&crate::web::encode_delimited([second]))
        .unwrap();
    let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
    undecided_budget(scope, serde_json::from_str(&saved).unwrap());
    assert!(
        web.transparent_pir_checkpoint().as_deref() == Some(saved.as_str()),
        "web fork budget must retain debt exactly"
    );
    let mut web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    web.rewind_to_height_checked(1).unwrap();
    web.apply_compact_blocks_blob(&crate::web::encode_delimited([block(
        2,
        Network::Regtest.genesis_hash(),
    )]))
    .unwrap();
    let mut web_counts = vec![];
    let mut web_reasons = vec![];
    for _ in 0..3 {
        let scope = serde_json::from_str(&web.begin_transparent_pir(SOURCE).unwrap()).unwrap();
        let cp = serde_json::from_str(&web.transparent_pir_checkpoint().unwrap()).unwrap();
        let (count, reason) = pay_debt(scope, cp, &server, |cp| {
            web.apply_transparent_pir_checkpoint(&serde_json::to_string(cp).unwrap())
                .unwrap();
            web.acknowledge_transparent_pir_checkpoint(&cp.checkpoint_id)
                .unwrap();
        });
        web_counts.push(count);
        web_reasons.push(reason);
        web = crate::web::WebWallet::from_snapshot(&web.to_snapshot().unwrap()).unwrap();
    }
    assert_eq!(web_counts, vec![1, 1, 1]);
    assert_eq!(
        web_reasons,
        vec!["query_budget", "query_budget", "content_failed"]
    );
    assert_eq!(web.transparent_pir_status()["completion"], "incomplete");
}
