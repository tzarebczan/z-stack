use super::*;
use crate::transparent_pir::{TransparentPirCheckpoint, TransparentPirRecord, TransparentPirScope};
use transparent_events::{ReceiveEvent, SpendEvent, TransparentEvent, Txid};
use z_transparent_pir_v2::Coverage;

fn checkpoint(
    scope: TransparentPirScope,
    script: &str,
    events: Vec<TransparentEvent>,
    previous: Option<String>,
    id: u8,
) -> TransparentPirCheckpoint {
    TransparentPirCheckpoint {
        source_id: scope.source_id,
        generation: scope.generation,
        scope_id: scope.scope_id,
        anchor: scope.anchor.clone(),
        checkpoint_id: format!("{id:02x}").repeat(32),
        previous_checkpoint_id: previous,
        covered_through: scope.anchor.height,
        complete: true,
        pending_pages: 0,
        unresolved_spends: 0,
        records: events
            .into_iter()
            .map(|e| TransparentPirRecord {
                script: script.into(),
                event: crate::web::to_hex(&e.to_bytes()),
                shard_id: 0,
                revision: "ef".repeat(32),
            })
            .collect(),
        coverage: scope
            .scripts
            .into_iter()
            .map(|s| Coverage {
                script: s.script,
                required_from: s.required_from,
                through: scope.anchor.height,
                revision: "ef".repeat(32),
            })
            .collect(),
        rewind_to: None,
        continuation: String::new(),
    }
}
fn receive() -> TransparentEvent {
    TransparentEvent::Receive(ReceiveEvent {
        metadata: None,
        height: 1,
        txid: Txid([0x11; 32]),
        transaction_index: 1,
        output_index: 0,
        value: 150000,
        coinbase: false,
    })
}
fn spend() -> TransparentEvent {
    TransparentEvent::Spend(SpendEvent {
        metadata: None,
        height: 2,
        spending_txid: Txid([0x22; 32]),
        transaction_index: 1,
        input_index: 0,
        spent_txid: Txid([0x11; 32]),
        spent_output_index: 0,
    })
}

#[tokio::test]
async fn native_transparent_pass_drop_does_not_commit_or_leave_a_pending_session() {
    let root = tempfile::tempdir().unwrap();
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
    let source_id = "ab".repeat(32);
    let cancel = Arc::new(RecoveryCancellation::new());
    let pass = wallet
        .begin_transparent_pir_with_source(&fixture.source, &source_id, cancel.clone())
        .unwrap();
    let original_scope = pass.scope_json().unwrap();
    assert!(database_lease::exclusive(&wallet.paths.data_db).is_err());
    drop(pass);
    drop(database_lease::exclusive(&wallet.paths.data_db).unwrap());
    assert!(!cancel.is_cancelled());
    assert!(wallet.transparent_pir_checkpoint().unwrap().is_none());
    assert!(wallet.history(20).unwrap().is_empty());

    // A budget/transport abandonment can begin again with the same live token.
    let pass = wallet
        .begin_transparent_pir_with_source(&fixture.source, &source_id, cancel.clone())
        .unwrap();
    assert!(
        pass.scope_json().unwrap() == original_scope,
        "abandoned scope must be reusable"
    );
    cancel.cancel();
    drop(pass);
    assert!(wallet.transparent_pir_checkpoint().unwrap().is_none());
    assert!(wallet.history(20).unwrap().is_empty());

    // Explicit cancellation poisons that token, not the durable wallet session.
    let mut restarted = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            &source_id,
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: TransparentPirScope =
        serde_json::from_str(&restarted.scope_json().unwrap()).unwrap();
    let script = scope.scripts[0].script.clone();
    let complete = checkpoint(scope, &script, vec![], None, 0x71);
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut restarted,
            &serde_json::to_string(&complete).unwrap(),
        )
        .unwrap();
    drop(restarted);
    let enrolled = wallet.transparent_pir_status_for(&source_id).unwrap();
    assert_eq!(enrolled["completion"], "complete-for-enrolled-scope");
    assert_eq!(enrolled["checkpointId"], complete.checkpoint_id);
    let other_source = "cd".repeat(32);
    let masked = wallet.transparent_pir_status_for(&other_source).unwrap();
    assert_eq!(masked["completion"], "incomplete");
    assert!(masked.get("scope").is_none());
    assert!(masked.get("confirmedZat").is_none());
    assert!(wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            &other_source,
            Arc::new(RecoveryCancellation::new()),
        )
        .err()
        .unwrap()
        .to_string()
        .contains("transparent_pir_source_changed"));
    assert_eq!(
        wallet.transparent_pir_status_for(&source_id).unwrap(),
        enrolled
    );
}

#[tokio::test]
async fn native_transparent_pir_canonical_receive_spend_reopen_and_cancel() {
    use zcash_keys::encoding::AddressCodec;
    let root = tempfile::tempdir().unwrap();
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
    let address = transparent::address::TransparentAddress::decode(
        &wallet.network,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    let script = crate::web::to_hex(&script.0 .0);
    let cancel = Arc::new(RecoveryCancellation::new());
    let mut pass = wallet
        .begin_transparent_pir_with_source(&fixture.source, &"ab".repeat(32), cancel)
        .unwrap();
    let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let mut c = checkpoint(scope, &script, vec![receive()], None, 1);
    c.complete = false;
    c.pending_pages = 1;
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&c).unwrap(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    let (value,raw):(u64,Option<Vec<u8>>)=conn.query_row("SELECT n.value_zat,t.raw FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1",[[0x11u8;32].as_slice()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(value, 150000);
    assert!(raw.is_none());
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    // PIR rows live in the canonical DB, but this source-bound wallet cannot
    // enter ordinary transparent shielding or direct endpoint send routines.
    let calls_before = fixture.calls.lock().unwrap().len();
    code(
        wallet.shield(&auth(), 1).await,
        "native_wallet_source_required",
    );
    code(wallet.max_send(None), "native_wallet_source_required");
    code(
        wallet.send(&auth(), "irrelevant", 1, None).await,
        "native_wallet_source_required",
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), calls_before);
    conn.execute(
        "UPDATE transparent_received_outputs SET lock_expiry_height=100 WHERE value_zat=150000",
        [],
    )
    .unwrap();
    // Reapplying identical evidence must not erase a reservation.
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&c).unwrap(),
        )
        .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT lock_expiry_height FROM transparent_received_outputs WHERE value_zat=150000",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        100
    );
    let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let c2 = checkpoint(
        scope,
        &script,
        vec![receive(), spend()],
        Some(c.checkpoint_id.clone()),
        2,
    );
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&c2).unwrap(),
        )
        .unwrap();
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM transparent_received_output_spends",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(wallet.history(20).unwrap().len(), 2);
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["completion"],
        "incomplete"
    );
    drop(pass);
    drop(wallet);
    let reopened = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    assert_eq!(reopened.history(20).unwrap().len(), 2);
    let mut recut = reopened
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope = serde_json::from_str(&recut.scope_json().unwrap()).unwrap();
    let mut c3 = checkpoint(
        scope,
        &script,
        vec![receive()],
        Some(c2.checkpoint_id.clone()),
        3,
    );
    c3.rewind_to = Some(crate::transparent_pir::TransparentPirAnchor {
        height: 1,
        hash: "01".repeat(32),
    });
    reopened
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut recut,
            &serde_json::to_string(&c3).unwrap(),
        )
        .unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM transparent_received_output_spends",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT lock_expiry_height FROM transparent_received_outputs WHERE value_zat=150000",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        100
    );
    drop(recut);
    let token = Arc::new(RecoveryCancellation::new());
    let mut pass = reopened
        .begin_transparent_pir_with_source(&fixture.source, &"ab".repeat(32), token.clone())
        .unwrap();
    let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let c4 = checkpoint(scope, &script, vec![receive()], Some(c3.checkpoint_id), 4);
    token.cancel();
    assert!(reopened
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&c4).unwrap()
        )
        .is_err());
    assert_eq!(
        reopened
            .transparent_pir_checkpoint()
            .unwrap()
            .map(|s| serde_json::from_str::<TransparentPirCheckpoint>(&s)
                .unwrap()
                .checkpoint_id),
        Some("03".repeat(32))
    );
    drop(pass);
    let mut mined = reopened
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope = serde_json::from_str(&mined.scope_json().unwrap()).unwrap();
    let c5 = checkpoint(
        scope,
        &script,
        vec![receive(), spend()],
        Some("03".repeat(32)),
        5,
    );
    reopened
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut mined,
            &serde_json::to_string(&c5).unwrap(),
        )
        .unwrap();
    drop(mined);
    reopened
        .open_db()
        .unwrap()
        .truncate_to_height(1.into())
        .unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["completion"],
        "incomplete"
    );
    let saved = reopened.transparent_pir_checkpoint().unwrap();
    code(
        reopened.begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        ),
        "transparent_pir_rewind_required",
    );
    assert!(
        reopened.transparent_pir_checkpoint().unwrap() == saved,
        "lower target must retain private acquisition"
    );
    fixture.mode.store(4, Ordering::Release);
    reopened
        .sync_with_wallet_source_reported_cancellable(
            &fixture.source,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    let mut after_reorg = reopened
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: TransparentPirScope =
        serde_json::from_str(&after_reorg.scope_json().unwrap()).unwrap();
    assert_eq!(scope.anchor.height, 2);
    let mut partial = checkpoint(scope.clone(), &script, vec![], Some(c5.checkpoint_id), 6);
    partial.rewind_to = Some(crate::transparent_pir::TransparentPirAnchor {
        height: 1,
        hash: "01".repeat(32),
    });
    partial.complete = false;
    partial.pending_pages = 1;
    reopened
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut after_reorg,
            &serde_json::to_string(&partial).unwrap(),
        )
        .unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["completion"],
        "incomplete"
    );
    let c6 = checkpoint(
        scope,
        &script,
        vec![receive()],
        Some(partial.checkpoint_id),
        7,
    );
    reopened
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut after_reorg,
            &serde_json::to_string(&c6).unwrap(),
        )
        .unwrap();
    assert_eq!(
        reopened.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM transparent_received_output_spends",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT lock_expiry_height FROM transparent_received_outputs WHERE value_zat=150000",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        100
    );
    assert!(!fixture.calls.lock().unwrap().iter().any(|m| matches!(
        m.as_str(),
        "GetTransaction" | "GetAddressUtxos" | "SendTransaction"
    )));
}

#[tokio::test]
async fn native_transparent_pir_amount_counts_only_pir_recovered_receives() {
    use zcash_keys::encoding::AddressCodec;
    let root = tempfile::tempdir().unwrap();
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
    let address = transparent::address::TransparentAddress::decode(
        &wallet.network,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    {
        let mut db = wallet.open_db().unwrap();
        let account = NativeWallet::primary_account_id(&db).unwrap();
        for (txid, value) in [([0x11; 32], 150000), ([0x33; 32], 40000)] {
            let output = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new(txid, 0),
                transparent::bundle::TxOut::new(
                    zcash_protocol::value::Zatoshis::from_u64(value).unwrap(),
                    script.clone(),
                ),
                Some(1.into()),
                Some(account),
                None,
                None,
            )
            .unwrap();
            db.put_received_transparent_utxo(&output).unwrap();
        }
    }
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    let mut pass = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: TransparentPirScope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let script = crate::web::to_hex(&script.0 .0);
    let empty = checkpoint(scope.clone(), &script, vec![], None, 0x81);
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&empty).unwrap(),
        )
        .unwrap();
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    let recovered = checkpoint(
        scope.clone(),
        &script,
        vec![receive()],
        Some(empty.checkpoint_id),
        0x82,
    );
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&recovered).unwrap(),
        )
        .unwrap();
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "150000"
    );
    let spent = checkpoint(
        scope,
        &script,
        vec![receive(), spend()],
        Some(recovered.checkpoint_id),
        0x83,
    );
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&spent).unwrap(),
        )
        .unwrap();
    assert_eq!(
        wallet.transparent_pir_status().unwrap()["confirmedZat"],
        "0"
    );
    let db = rusqlite::Connection::open(&wallet.paths.data_db).unwrap();
    let ordinary:u64 = db.query_row("SELECT n.value_zat FROM transparent_received_outputs n JOIN transactions t ON t.id_tx=n.transaction_id WHERE t.txid=?1", [[0x33u8;32].as_slice()], |r|r.get(0)).unwrap();
    assert_eq!(ordinary, 40000);
}

#[tokio::test]
async fn native_pir_only_funding_is_excluded_from_real_shielding_proposal() {
    use zcash_keys::encoding::AddressCodec;
    let root = tempfile::tempdir().unwrap();
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
    let address = transparent::address::TransparentAddress::decode(
        &wallet.network,
        &wallet.transparent_address().unwrap().unwrap(),
    )
    .unwrap();
    let script: transparent::address::Script = address.script().into();
    let mut pass = wallet
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: TransparentPirScope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let cp = checkpoint(
        scope,
        &crate::web::to_hex(&script.0 .0),
        vec![receive()],
        None,
        0x91,
    );
    wallet
        .apply_transparent_pir_checkpoint_with_source(
            &fixture.source,
            &mut pass,
            &serde_json::to_string(&cp).unwrap(),
        )
        .unwrap();
    drop(pass);
    let mut db = wallet.open_db().unwrap();
    let account = NativeWallet::primary_account_id(&db).unwrap();
    let control = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
        transparent::bundle::OutPoint::new([0x33; 32], 0),
        transparent::bundle::TxOut::new(Zatoshis::from_u64(40000).unwrap(), script.clone()),
        Some(1.into()),
        Some(account),
        None,
        None,
    )
    .unwrap();
    db.put_received_transparent_utxo(&control).unwrap();
    drop(db);
    assert_eq!(
        native_funding_selection(&wallet, address).unwrap(),
        vec![[0x33; 32]],
        "PIR-only funding must not enter actual shielding selection alongside independent control"
    );
    // Public source binding remains a separate, earlier spending boundary.
    code(
        wallet.shield(&auth(), 1).await,
        "native_wallet_source_required",
    );
    assert_eq!(
        wallet.transparent_pir_status_for(&"ab".repeat(32)).unwrap()["confirmedZat"],
        "150000"
    );

    // Explicit transparent SpendPolicy and decoding a proposal do not bypass the
    // funding boundary; the create paths repeat this check in their write txn.
    let mut db = wallet.open_db().unwrap();
    let request = TransactionRequest::new(vec![zip_payment(
        &wallet.unified_address().unwrap(),
        100000,
        None,
    )
    .unwrap()])
    .unwrap();
    let policy=shielded_spend_policy().with_transparent(zcash_client_backend::data_api::wallet::input_selection::TransparentSpendPolicy::any_account_addr());
    let result = db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<_> {
        wallet
            .propose_fee_in_snapshot(wdb, ext, request, policy)
            .map_err(anyhow::Error::new)
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("transparent_pir_funding_unverified"));
    let raw_proposal =
        zcash_client_backend::data_api::wallet::propose_shielding::<_, _, _, _, Infallible>(
            &mut db,
            &wallet.network,
            &GreedyInputSelector::new(),
            &SingleOutputChangeStrategy::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            ),
            Zatoshis::from_u64(1).unwrap(),
            &[address],
            account,
            shield_confirmations(wallet.network),
            CoinbaseFilter::AllTransparentOutputs,
            None,
        )
        .unwrap();
    let result = db.transactionally_with_extension(|_, ext| {
        super::super::transparent_pir_funding::check_proposal(ext, &raw_proposal)
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("transparent_pir_funding_unverified"));

    // Locking the independent output cannot make the selector fall back to PIR.
    use zcash_client_backend::{
        data_api::OutputLockStore,
        wallet::{LockOwner, OutputRef},
    };
    let owner = LockOwner::new([0x55; 32]);
    let reference = OutputRef::new(
        TxId::from_bytes([0x33; 32]),
        zcash_protocol::PoolType::Transparent,
        0,
    );
    db.lock_outputs(&[reference], owner, 100.into()).unwrap();
    assert!(native_funding_selection(&wallet, address).is_err());
    assert!(
        db.unlock_output(&reference, owner).unwrap(),
        "selection must not release another flow's reservation"
    );
    drop(db);

    // Re-observation alone never grants the native promotion authority this
    // version does not implement. Dropping enrollment and reopening does not either.
    let pir_output = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
        transparent::bundle::OutPoint::new([0x11; 32], 0),
        transparent::bundle::TxOut::new(Zatoshis::from_u64(150000).unwrap(), script.clone()),
        Some(1.into()),
        Some(account),
        None,
        None,
    )
    .unwrap();
    wallet
        .open_db()
        .unwrap()
        .put_received_transparent_utxo(&pir_output)
        .unwrap();
    drop(wallet);
    let cold = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
    assert_eq!(
        native_funding_selection(&cold, address).unwrap(),
        vec![[0x33; 32]]
    );
    code(
        cold.shield(&auth(), 1).await,
        "native_wallet_source_required",
    );
    let ordinary_open = NativeWallet::open(root.path()).unwrap();
    code(
        ordinary_open.shield(&auth(), 1).await,
        "native_wallet_source_required",
    );
    drop(ordinary_open);

    // Absent/v1 funding provenance cannot establish sibling authority.
    // The native owner durably quarantines ambiguous legacy canonical funds.
    let conn = rusqlite::Connection::open(&cold.paths.data_db).unwrap();
    let original: Vec<u8> = conn
        .query_row("SELECT state FROM ext_transparent_pir_v1", [], |r| r.get(0))
        .unwrap();
    conn.execute("DROP TABLE ext_transparent_pir_funding_guard_v1", [])
        .unwrap();
    assert!(native_funding_selection(&cold, address).is_err());
    cold.transparent_pir_status().unwrap(); // native owner reinstates the durable guard
    assert_eq!(
        native_funding_selection(&cold, address).unwrap(),
        vec![[0x33; 32]]
    );

    for (version, missing) in [(None, false), (None, true), (Some(1), false)] {
        let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("nativeFundingVersion");
        if let Some(v) = version {
            legacy["nativeFundingVersion"] = serde_json::json!(v);
        }
        legacy["unverifiedOutputs"] = if missing {
            serde_json::Value::Null
        } else {
            serde_json::json!([])
        };
        conn.execute(
            "UPDATE ext_transparent_pir_v1 SET state=?1",
            [serde_json::to_vec(&legacy).unwrap()],
        )
        .unwrap();
        cold.transparent_pir_status().unwrap(); // owner commits the conservative migration
        assert!(native_funding_selection(&cold, address).is_err());
        let saved: Vec<u8> = conn
            .query_row("SELECT state FROM ext_transparent_pir_v1", [], |r| r.get(0))
            .unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved["nativeFundingVersion"], 2);
        assert_eq!(saved["unverifiedOutputs"].as_array().unwrap().len(), 2);
    }
    let mut invalid: serde_json::Value = serde_json::from_slice(&original).unwrap();
    invalid["nativeFundingVersion"] = 3.into();
    conn.execute(
        "UPDATE ext_transparent_pir_v1 SET state=?1",
        [serde_json::to_vec(&invalid).unwrap()],
    )
    .unwrap();
    assert!(native_funding_selection(&cold, address).is_err());
    invalid["nativeFundingVersion"] = 2.into();
    invalid["unverifiedOutputs"] = serde_json::Value::Null;
    conn.execute(
        "UPDATE ext_transparent_pir_v1 SET state=?1",
        [serde_json::to_vec(&invalid).unwrap()],
    )
    .unwrap();
    assert!(native_funding_selection(&cold, address).is_err());
    conn.execute("UPDATE ext_transparent_pir_v1 SET state=?1", [original])
        .unwrap();
    drop(conn);

    // A real source checkpoint retirement removes deletion ownership, while
    // funding quarantine survives even if the canonical outpoint is reinserted.
    let mut pass = cold
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
    let mut retired = checkpoint(
        scope,
        &crate::web::to_hex(&script.0 .0),
        vec![],
        Some(cp.checkpoint_id),
        0x92,
    );
    retired.rewind_to = Some(crate::transparent_pir::TransparentPirAnchor {
        height: 0,
        hash: cold.network.genesis_hash().into(),
    });
    cold.apply_transparent_pir_checkpoint_with_source(
        &fixture.source,
        &mut pass,
        &serde_json::to_string(&retired).unwrap(),
    )
    .unwrap();
    drop(pass);
    cold.open_db()
        .unwrap()
        .put_received_transparent_utxo(&pir_output)
        .unwrap();
    assert_eq!(
        native_funding_selection(&cold, address).unwrap(),
        vec![[0x33; 32]]
    );
    let conn = rusqlite::Connection::open(&cold.paths.data_db).unwrap();
    let bytes: Vec<u8> = conn
        .query_row("SELECT state FROM ext_transparent_pir_v1", [], |r| r.get(0))
        .unwrap();
    let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(state["projectedOutputs"].as_array().unwrap().is_empty());
    assert!(state["unverifiedOutputs"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(format!("r:{}:0", "11".repeat(32)))));

    // The durable receipt fixture contains serialized disposable bytes, not a
    // real submission. Its lifecycle and bytes must survive funding refusal.
    let (receipt_id, receipt_raw) = saved_transaction(&cold);
    let receipt_before = serde_json::to_value(cold.payment_receipt(&receipt_id).unwrap()).unwrap();

    // Losing or corrupting durable provenance fails closed, preserving rows.
    conn.execute("UPDATE ext_transparent_pir_v1 SET state=X'00'", [])
        .unwrap();
    assert!(native_funding_selection(&cold, address).is_err());
    conn.execute("DELETE FROM ext_transparent_pir_v1", [])
        .unwrap();
    assert!(native_funding_selection(&cold, address).is_err());
    assert!(cold.transparent_pir_status().is_err());
    let outputs: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_outputs",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outputs, 3);
    assert_eq!(
        serde_json::to_value(cold.payment_receipt(&receipt_id).unwrap()).unwrap(),
        receipt_before
    );
    let retained: Vec<u8> = conn
        .query_row(
            "SELECT raw FROM transactions WHERE created IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        retained == receipt_raw,
        "funding checks must retain saved transaction bytes"
    );
}

fn native_funding_selection(
    wallet: &NativeWallet,
    address: transparent::address::TransparentAddress,
) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut db = wallet.open_db()?;
    let account = NativeWallet::primary_account_id(&db)?;
    let proposal = db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<_> {
        Ok(wallet.propose_shielding_in_snapshot(
            wdb,
            ext,
            &[address],
            account,
            Zatoshis::from_u64(1).unwrap(),
        )?)
    })?;
    Ok(proposal
        .steps()
        .iter()
        .flat_map(|s| s.transparent_inputs())
        .map(|o| *o.outpoint().hash())
        .collect())
}

async fn pending_sibling_confirmation_case(split: bool) {
    use zcash_keys::encoding::AddressCodec;
    let mut observed = Vec::new();
    {
        let root = tempfile::tempdir().unwrap();
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
        let address = transparent::address::TransparentAddress::decode(
            &wallet.network,
            &wallet.transparent_address().unwrap().unwrap(),
        )
        .unwrap();
        let script: transparent::address::Script = address.script().into();
        let mut db = wallet.open_db().unwrap();
        let account = NativeWallet::primary_account_id(&db).unwrap();
        for (id, index, value, height) in [
            (0x33, 0, 40000, Some(1.into())),
            (0x44, 0, 60000, None),
            (0x44, 1, 70000, None),
        ] {
            let output = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
                transparent::bundle::OutPoint::new([id; 32], index),
                transparent::bundle::TxOut::new(Zatoshis::from_u64(value).unwrap(), script.clone()),
                height,
                Some(account),
                None,
                None,
            )
            .unwrap();
            db.put_received_transparent_utxo(&output).unwrap();
        }
        drop(db);
        let records = vec![
            receive(),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 1,
                txid: Txid([0x33; 32]),
                transaction_index: 1,
                output_index: 0,
                value: 40000,
                coinbase: false,
            }),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 1,
                txid: Txid([0x33; 32]),
                transaction_index: 1,
                output_index: 1,
                value: 50000,
                coinbase: false,
            }),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 1,
                txid: Txid([0x44; 32]),
                transaction_index: 1,
                output_index: 0,
                value: 60000,
                coinbase: false,
            }),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 1,
                txid: Txid([0x44; 32]),
                transaction_index: 1,
                output_index: 1,
                value: 70000,
                coinbase: false,
            }),
        ];
        let mut pass = wallet
            .begin_transparent_pir_with_source(
                &fixture.source,
                &"ab".repeat(32),
                Arc::new(RecoveryCancellation::new()),
            )
            .unwrap();
        let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
        let first = if split {
            records[..4].to_vec()
        } else {
            records.clone()
        };
        let mut cp = checkpoint(scope, &crate::web::to_hex(&script.0 .0), first, None, 0xa1);
        if split {
            cp.complete = false;
            cp.pending_pages = 1;
        }
        wallet
            .apply_transparent_pir_checkpoint_with_source(
                &fixture.source,
                &mut pass,
                &serde_json::to_string(&cp).unwrap(),
            )
            .unwrap();
        drop(pass);
        observed.push(native_funding_selection(&wallet, address).unwrap());
        drop(wallet);
        let cold = NativeWallet::open_with_wallet_source(root.path(), &fixture.source).unwrap();
        observed.push(native_funding_selection(&cold, address).unwrap());
        code(
            cold.shield(&auth(), 1).await,
            "native_wallet_source_required",
        );
        if split {
            let mut pass = cold
                .begin_transparent_pir_with_source(
                    &fixture.source,
                    &"ab".repeat(32),
                    Arc::new(RecoveryCancellation::new()),
                )
                .unwrap();
            let scope = serde_json::from_str(&pass.scope_json().unwrap()).unwrap();
            let next = checkpoint(
                scope,
                &crate::web::to_hex(&script.0 .0),
                records,
                Some(cp.checkpoint_id),
                0xa2,
            );
            cold.apply_transparent_pir_checkpoint_with_source(
                &fixture.source,
                &mut pass,
                &serde_json::to_string(&next).unwrap(),
            )
            .unwrap();
            drop(pass);
            observed.push(native_funding_selection(&cold, address).unwrap());
        }
    }
    assert_eq!(observed,vec![vec![[0x33;32]];if split {3}else{2}],"publisher metadata cannot confer funding authority on pending siblings in hot, cold, or later checkpoint stages");
}

#[tokio::test]
async fn native_pir_same_checkpoint_pending_siblings_remain_read_only() {
    pending_sibling_confirmation_case(false).await;
}
#[tokio::test]
async fn native_pir_cross_checkpoint_pending_siblings_remain_read_only() {
    pending_sibling_confirmation_case(true).await;
}
