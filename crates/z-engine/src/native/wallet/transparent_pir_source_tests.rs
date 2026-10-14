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
    let mut after_reorg = reopened
        .begin_transparent_pir_with_source(
            &fixture.source,
            &"ab".repeat(32),
            Arc::new(RecoveryCancellation::new()),
        )
        .unwrap();
    let scope: TransparentPirScope =
        serde_json::from_str(&after_reorg.scope_json().unwrap()).unwrap();
    assert_eq!(scope.anchor.height, 1);
    let mut partial = checkpoint(scope.clone(), &script, vec![], None, 6);
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
