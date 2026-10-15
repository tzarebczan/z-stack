use super::*;
use futures::Future;
pub(super) fn scope() -> Scope {
    Scope {
        source_id: "1".repeat(64),
        generation: 1,
        scope_id: "2".repeat(64),
        anchor: Anchor {
            height: 100,
            hash: "a".repeat(64),
        },
        scripts: vec![Script {
            script: "76a914000000000000000000000000000000000000000088ac".into(),
            required_from: 0,
        }],
        accepted_headers: vec![
            Anchor {
                height: 0,
                hash: transparent_filter::MAINNET_GENESIS_DISPLAY.into(),
            },
            Anchor {
                height: 100,
                hash: "a".repeat(64),
            },
        ],
    }
}
pub(super) fn snapshot(p: &Pass) -> Checkpoint {
    let bytes = durable(
        &p.scope,
        &p.store.inner.borrow(),
        &p.driver.chain.borrow(),
        p.store.policy_limit.get(),
    )
    .unwrap();
    p.checkpoint(&bytes).unwrap()
}
#[test]
fn version_two_migrates_only_after_its_original_digest_is_verified() {
    let p = Pass::new(scope(), None, Limits::default()).unwrap();
    let script = hex::decode(&scope().scripts[0].script).unwrap();
    p.store
        .inner
        .borrow_mut()
        .add_scripts(&[ScriptEntry {
            script: script.clone(),
            origin: ScriptOrigin::Derived,
            required_from: 0,
        }])
        .unwrap();
    p.store
        .inner
        .borrow_mut()
        .commit_shard(crate::store::ShardCommit {
            end_height: 100,
            terminal_block_hash: "a".repeat(64),
            revision_digest: "b".repeat(64),
            failed_entries: vec![script.clone()],
            pending_upsert: vec![crate::store::PendingPages {
                id: None,
                shard_id: 0,
                revision_digest: "b".repeat(64),
                script: script.clone(),
                cursor: crate::store::RunCursor {
                    child: 0,
                    row: 0,
                    older_runs: None,
                },
                carried: vec![],
                fragments: vec![],
                dummy_debt: Some((2, 3)),
                attempts: 1,
                target_anchor: Some(crate::store::Anchor {
                    height: 100,
                    hash: "a".repeat(64),
                }),
            }],
            ..Default::default()
        })
        .unwrap();
    let mut cp = snapshot(&p);
    let state = p.store.inner.borrow().clone();
    cp.continuation = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .serialize(&LegacyContinuation {
                version: 2,
                source_id: cp.source_id.clone(),
                state: state.clone(),
            })
            .unwrap(),
    );
    cp.checkpoint_id = checkpoint_id(
        &cp.source_id,
        &cp.scope_id,
        cp.generation,
        &cp.anchor,
        false,
        &encoded(&state).unwrap(),
    );
    let restored = Pass::new(scope(), Some(cp.clone()), Limits::default()).unwrap();
    assert_eq!(
        restored.store.inner.borrow().pending().unwrap()[0].dummy_debt,
        Some((2, 3))
    );
    assert_eq!(
        restored
            .store
            .inner
            .borrow()
            .entry_failures(0, &script)
            .unwrap(),
        1
    );
    let upgraded = snapshot(&restored);
    assert_eq!(
        &STANDARD.decode(&upgraded.continuation).unwrap()[..4],
        &4u32.to_le_bytes()
    );
    let mut moved = scope();
    moved.anchor = Anchor {
        height: 101,
        hash: "c".repeat(64),
    };
    moved.accepted_headers.pop();
    moved.accepted_headers.push(moved.anchor.clone());
    let mut interrupted = Pass::new(moved, Some(upgraded), Limits::default()).unwrap();
    interrupted.finish_budget().unwrap();
    assert!(
        matches!(interrupted.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="budget")
    );
    assert_eq!(
        interrupted.store.inner.borrow().pending().unwrap()[0].dummy_debt,
        Some((2, 3))
    );
    assert_eq!(
        interrupted
            .store
            .inner
            .borrow()
            .entry_failures(0, &script)
            .unwrap(),
        1
    );
    interrupted.cancel();
    assert!(interrupted.finish_budget().is_err());
    cp.checkpoint_id = "f".repeat(64);
    assert!(Pass::new(scope(), Some(cp), Limits::default()).is_err());
}
#[test]
fn observed_cache_is_bound_and_rejects_tampering_and_oversized_maps() {
    let p = Pass::new(scope(), None, Limits::default()).unwrap();
    p.driver.chain.borrow_mut().insert(50, "b".repeat(64));
    let cp = snapshot(&p);
    let same = Pass::new(scope(), Some(cp.clone()), Limits::default()).unwrap();
    assert!(same.driver.chain.borrow().contains_key(&50));
    let mut changed = scope();
    changed.anchor.height += 1;
    changed.anchor.hash = "c".repeat(64);
    changed.accepted_headers.pop();
    changed.accepted_headers.push(changed.anchor.clone());
    let moving = Pass::new(changed, Some(cp.clone()), Limits::default()).unwrap();
    assert!(
        !moving.driver.chain.borrow().contains_key(&50),
        "advancing anchor must first be revalidated"
    );
    let mut changed = scope();
    changed.source_id = "d".repeat(64);
    assert!(Pass::new(changed, Some(cp.clone()), Limits::default()).is_err());
    let mut changed = scope();
    changed.accepted_headers[0].hash = "d".repeat(64);
    let other_network = Pass::new(changed, Some(cp.clone()), Limits::default()).unwrap();
    assert!(!other_network.driver.chain.borrow().contains_key(&50));
    let mut tampered = cp;
    let mut bytes = STANDARD.decode(&tampered.continuation).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    tampered.continuation = STANDARD.encode(bytes);
    assert!(Pass::new(scope(), Some(tampered), Limits::default()).is_err());
    let oversized = ObservedChain {
        anchor: scope().anchor,
        genesis: None,
        headers: (0..MAX_OBSERVED_POINTS as u64 + 1)
            .map(|h| (h, "b".repeat(64)))
            .collect(),
    };
    let bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&oversized)
        .unwrap();
    assert!(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .deserialize::<ObservedChain>(&bytes)
        .is_err());
}
#[test]
fn budget_finish_preserves_held_checkpoint_and_fences_late_io() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    pass.driver.chain.borrow_mut().insert(50, "b".repeat(64));
    let cp = snapshot(&pass);
    pass.held = Some(cp.clone());
    pass.finish_budget().unwrap();
    assert_eq!(
        pass.advance().unwrap(),
        Step::Checkpoint {
            checkpoint: cp.clone()
        }
    );
    assert!(pass.accept(1, Reply::Unavailable).is_err());
    assert!(pass.accept_chain(50, "b".repeat(64)).is_err());
    pass.acknowledge(&cp.checkpoint_id).unwrap();
    assert_eq!(
        pass.advance().unwrap(),
        Step::Stopped {
            status: Status::Incomplete {
                reason: "budget".into()
            }
        }
    );
    pass.cancel();
    assert!(pass.finish_budget().is_err());
    assert_eq!(
        pass.advance().unwrap(),
        Step::Stopped {
            status: Status::Cancelled
        }
    );
    let mut untouched = Pass::new(scope(), None, Limits::default()).unwrap();
    untouched.finish_budget().unwrap();
    assert!(matches!(
        untouched.advance().unwrap(),
        Step::Checkpoint { .. }
    ));
}
#[test]
fn unchanged_cold_incomplete_stops_without_self_referential_replay() {
    let pass = Pass::new(scope(), None, Limits::default()).unwrap();
    let cp = snapshot(&pass);
    let mut reopened = Pass::new(scope(), Some(cp.clone()), Limits::default()).unwrap();
    let Step::Exchange { request_id, .. } = reopened.advance().unwrap() else {
        panic!("init required");
    };
    reopened.accept(request_id, Reply::Unavailable).unwrap();
    assert!(matches!(
        reopened.advance().unwrap(),
        Step::Stopped {
            status: Status::Incomplete { .. }
        }
    ));
    // A complete replay keeps the exact original receipt link and rewind
    // metadata; resetting the canonical scan may require applying it again.
    let recreated = snapshot(&reopened);
    assert_eq!(recreated, cp);
}
#[test]
fn obsolete_tips_and_recut_boundaries_do_not_accumulate() {
    let pass = Pass::new(scope(), None, Limits::default()).unwrap();
    for tip in 101..10000 {
        let mut needed = std::collections::BTreeSet::from([0, tip]);
        let boundary = 1 + tip % 90;
        needed.insert(boundary);
        pass.driver.chain.borrow_mut().insert(tip, "b".repeat(64));
        pass.driver
            .chain
            .borrow_mut()
            .insert(boundary, "c".repeat(64));
        pass.driver.retain_points(&needed);
        assert_eq!(pass.driver.chain.borrow().len(), 3);
        assert!(pass
            .driver
            .chain
            .borrow()
            .keys()
            .all(|h| needed.contains(h)));
    }
}
#[test]
fn mismatched_independent_reply_is_durable_canonical_evidence() {
    let pass = Pass::new(scope(), None, Limits::default()).unwrap();
    let driver = pass.driver.clone();
    let expected = "b".repeat(64);
    let mut f = Box::pin(driver.point(50, &expected));
    let mut cx = Context::from_waker(noop_waker_ref());
    assert!(f.as_mut().poll(&mut cx).is_pending());
    pass.driver.io.borrow_mut().chain_reply = Some(Anchor {
        height: 50,
        hash: "c".repeat(64),
    });
    assert!(matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    assert_eq!(pass.driver.chain.borrow().get(&50), Some(&"c".repeat(64)));
    let restored = Pass::new(scope(), Some(snapshot(&pass)), Limits::default()).unwrap();
    assert_eq!(
        restored.driver.chain.borrow().get(&50),
        Some(&"c".repeat(64))
    );
}

#[test]
fn unchanged_complete_snapshot_reuses_exact_prior_metadata_and_rejects_conflict() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    let script = hex::decode(&scope().scripts[0].script).unwrap();
    pass.store
        .inner
        .borrow_mut()
        .add_scripts(&[ScriptEntry {
            script: script.clone(),
            origin: ScriptOrigin::Derived,
            required_from: 0,
        }])
        .unwrap();
    pass.store
        .inner
        .borrow_mut()
        .commit_shard(crate::store::ShardCommit {
            start_height: 0,
            end_height: 100,
            sealed: true,
            terminal_block_hash: "a".repeat(64),
            revision_digest: "b".repeat(64),
            covered_scripts: vec![script],
            ..Default::default()
        })
        .unwrap();
    *pass.store.rewind.borrow_mut() = Some(crate::store::Anchor {
        height: 0,
        hash: scope().accepted_headers[0].hash.clone(),
    });
    pass.finished = Some(Status::Complete);
    let mut cp = snapshot(&pass);
    cp.previous_checkpoint_id = Some("c".repeat(64));
    let mut reopened = Pass::new(scope(), Some(cp.clone()), Limits::default()).unwrap();
    reopened.finished = Some(Status::Complete);
    assert_eq!(snapshot(&reopened), cp);
    let mut conflicting = cp;
    conflicting.covered_through = 99;
    let mut conflict = Pass::new(scope(), Some(conflicting), Limits::default()).unwrap();
    conflict.finished = Some(Status::Complete);
    let bytes = durable(
        &conflict.scope,
        &conflict.store.inner.borrow(),
        &conflict.driver.chain.borrow(),
        conflict.store.policy_limit.get(),
    )
    .unwrap();
    assert!(conflict.checkpoint(&bytes).is_err());
}

#[test]
fn supported_domain_includes_disjoint_maps_history_and_three_anchors() {
    let old: std::collections::BTreeSet<_> = (1..=MAX_OLD_ENDPOINTS as u64).collect();
    let needed = needed_points(
        10_000..10_000 + 2 * MAX_MAP_SHARDS as u64,
        &old,
        50_000,
        Some(49_999),
    );
    assert_eq!(needed.len(), MAX_OBSERVED_POINTS);
    let pass = Pass::new(scope(), None, Limits::default()).unwrap();
    *pass.driver.chain.borrow_mut() = needed.iter().map(|h| (*h, "b".repeat(64))).collect();
    let encoded = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&ObservedChain {
            anchor: Anchor {
                height: 50_000,
                hash: "b".repeat(64),
            },
            genesis: Some("b".repeat(64)),
            headers: pass.driver.chain.borrow().clone(),
        })
        .unwrap();
    let restored: ObservedChain = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .deserialize(&encoded)
        .unwrap();
    assert_eq!(restored.headers.len(), MAX_OBSERVED_POINTS);
    for tip in 50_001..50_101 {
        let recut = needed_points(
            20_000..20_000 + 2 * MAX_MAP_SHARDS as u64,
            &old,
            tip,
            Some(tip - 1),
        );
        pass.driver.retain_points(&recut);
        for h in &recut {
            pass.driver.chain.borrow_mut().insert(*h, "b".repeat(64));
        }
        assert_eq!(pass.driver.chain.borrow().len(), MAX_OBSERVED_POINTS);
    }
}
#[test]
fn excessive_history_metadata_is_closed_without_checkpoint_or_evidence_loss() {
    let mut requested = scope();
    requested.anchor.height = 10_000;
    requested.accepted_headers[1] = requested.anchor.clone();
    let p = Pass::new(requested.clone(), None, Limits::default()).unwrap();
    let script = hex::decode(&requested.scripts[0].script).unwrap();
    p.store
        .inner
        .borrow_mut()
        .add_scripts(&[ScriptEntry {
            script: script.clone(),
            origin: ScriptOrigin::Derived,
            required_from: 0,
        }])
        .unwrap();
    let mut supported = None;
    for h in 1..=MAX_OLD_ENDPOINTS as u64 + 1 {
        p.store
            .inner
            .borrow_mut()
            .commit_shard(crate::store::ShardCommit {
                start_height: h,
                end_height: h,
                sealed: true,
                shard_id: h,
                terminal_block_hash: "b".repeat(64),
                revision_digest: "c".repeat(64),
                covered_scripts: vec![script.clone()],
                ..Default::default()
            })
            .unwrap();
        if h == MAX_OLD_ENDPOINTS as u64 {
            supported = Some(snapshot(&p));
            assert_eq!(
                retained_endpoints(&p.store.inner.borrow()).unwrap().len(),
                MAX_OLD_ENDPOINTS
            );
        }
    }
    let mut generated = Pass::new(requested.clone(), supported, Limits::default()).unwrap();
    generated
        .store
        .inner
        .borrow_mut()
        .commit_shard(crate::store::ShardCommit {
            start_height: 9000,
            end_height: 9000,
            sealed: true,
            shard_id: 9000,
            terminal_block_hash: "b".repeat(64),
            revision_digest: "c".repeat(64),
            covered_scripts: vec![script.clone()],
            ..Default::default()
        })
        .unwrap();
    generated.future = None;
    generated.finished = Some(Status::Complete);
    let Step::Checkpoint { checkpoint } = generated.advance().unwrap() else {
        panic!("new accepted state requires a durable incomplete checkpoint");
    };
    assert!(!checkpoint.complete);
    generated.acknowledge(&checkpoint.checkpoint_id).unwrap();
    assert!(
        matches!(generated.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
    );
    let restored_generated =
        Pass::new(requested.clone(), Some(checkpoint), Limits::default()).unwrap();
    assert_eq!(
        encoded(&generated.store.inner.borrow()).unwrap(),
        encoded(&restored_generated.store.inner.borrow()).unwrap()
    );
    let cp = snapshot(&p);
    let original_state = encoded(&p.store.inner.borrow()).unwrap();
    for _ in 0..3 {
        let mut cold = Pass::new(requested.clone(), Some(cp.clone()), Limits::default()).unwrap();
        assert_eq!(encoded(&cold.store.inner.borrow()).unwrap(), original_state);
        assert!(
            matches!(cold.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
        );
        cold.finish_budget().unwrap();
        assert!(
            matches!(cold.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
        );
        assert_eq!(encoded(&cold.store.inner.borrow()).unwrap(), original_state);
        assert!(cold.driver.io.borrow().pending.is_none());
    }
}

fn history_cap_seed(pass: &mut Pass, events: Vec<crate::store::StoredEvent>) {
    let script = hex::decode(&scope().scripts[0].script).unwrap();
    pass.store
        .add_scripts(&[ScriptEntry {
            script: script.clone(),
            origin: ScriptOrigin::Derived,
            required_from: 0,
        }])
        .unwrap();
    pass.store
        .commit_shard(crate::store::ShardCommit {
            start_height: 0,
            end_height: 100,
            terminal_block_hash: "a".repeat(64),
            revision_digest: "b".repeat(64),
            events,
            covered_scripts: vec![script.clone()],
            pending_upsert: vec![crate::store::PendingPages {
                id: None,
                shard_id: 0,
                revision_digest: "b".repeat(64),
                script,
                cursor: crate::store::RunCursor {
                    child: 0,
                    row: 0,
                    older_runs: None,
                },
                carried: vec![],
                fragments: vec![],
                dummy_debt: Some((2, 3)),
                attempts: 1,
                target_anchor: Some(crate::store::Anchor {
                    height: 100,
                    hash: "a".repeat(64),
                }),
            }],
            ..Default::default()
        })
        .unwrap();
}
fn history_cap_event(index: u32) -> crate::store::StoredEvent {
    crate::store::StoredEvent {
        script: hex::decode(&scope().scripts[0].script).unwrap(),
        event: transparent_events::TransparentEvent::Receive(transparent_events::ReceiveEvent {
            metadata: None,
            height: 1,
            txid: transparent_events::Txid([7; 32]),
            transaction_index: 1,
            output_index: index,
            value: 1,
            coinbase: false,
        }),
        shard_id: 0,
        revision_digest: "b".repeat(64),
    }
}
fn history_cap_ack_baseline(pass: &mut Pass) -> Checkpoint {
    pass.future = None;
    pass.finished = Some(Status::Incomplete {
        reason: "budget".into(),
    });
    let Step::Checkpoint { checkpoint } = pass.advance().unwrap() else {
        panic!("durable baseline required")
    };
    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
    pass.finished = None;
    pass.final_ack = false;
    checkpoint
}
fn history_cap_assert_cold(checkpoint: Checkpoint, repeats: usize) {
    for _ in 0..repeats {
        let mut cold = Pass::new(scope(), Some(checkpoint.clone()), Limits::default()).unwrap();
        assert!(
            matches!(cold.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
        );
        assert!(
            cold.driver.io.borrow().pending.is_none(),
            "terminal cold resume must not allocate IO"
        );
        assert_eq!(
            cold.store.inner.borrow().pending().unwrap()[0].dummy_debt,
            Some((2, 3))
        );
        cold.finish_budget().unwrap();
        assert!(
            matches!(cold.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
        );
    }
    let mut changed_scope = scope();
    changed_scope.generation += 1;
    changed_scope.scope_id = "e".repeat(64);
    let mut changed = Pass::new(changed_scope, Some(checkpoint), Limits::default()).unwrap();
    assert!(matches!(changed.advance().unwrap(), Step::Stopped {
        status: Status::Incomplete { reason }
    } if reason == "history_metadata_limit"));
    assert!(changed.driver.io.borrow().pending.is_none());
}
#[test]
fn history_cap_event_boundary_preserves_projection_debt_and_cold_terminal() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    history_cap_seed(&mut pass, (0..65_536).map(history_cap_event).collect());
    pass.store
        .commit_shard(crate::store::ShardCommit {
            events: vec![history_cap_event(0), history_cap_event(0)],
            ..Default::default()
        })
        .unwrap();
    let baseline = history_cap_ack_baseline(&mut pass);
    assert_eq!(baseline.records.len(), 65_536);
    let mut retired = Pass::new(scope(), Some(baseline.clone()), Limits::default()).unwrap();
    retired
        .store
        .rollback_above(
            &crate::store::Anchor {
                height: 0,
                hash: scope().accepted_headers[0].hash.clone(),
            },
            "fixture",
        )
        .unwrap();
    retired
        .store
        .commit_shard(crate::store::ShardCommit {
            events: vec![history_cap_event(65_536)],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(retired.store.events().unwrap().len(), 1);
    assert!(!retired.store.policy_limit.get());
    let before = encoded(&pass.store.inner.borrow()).unwrap();
    let pending = pass.store.inner.borrow().pending().unwrap()[0].id.unwrap();
    let refused = pass.store.commit_shard(crate::store::ShardCommit {
        events: vec![history_cap_event(65_536)],
        pending_complete: vec![pending],
        ..Default::default()
    });
    let Step::Checkpoint { checkpoint } = pass.advance().expect(
        "event policy limit must yield a durable terminal checkpoint, not event_limit exception",
    ) else {
        panic!("terminal checkpoint required")
    };
    assert!(refused.is_err());
    assert!(!checkpoint.complete);
    assert_eq!(checkpoint.records, baseline.records);
    assert_eq!(
        encoded(&pass.store.inner.borrow()).unwrap(),
        before,
        "refused commit cannot delete owed debt or accepted facts"
    );
    assert_eq!(
        checkpoint.previous_checkpoint_id,
        Some(baseline.checkpoint_id)
    );
    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
    assert!(
        matches!(pass.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
    );
    history_cap_assert_cold(checkpoint, 3);
}
#[test]
fn history_cap_byte_boundary_preserves_journal_and_cold_terminal() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    history_cap_seed(&mut pass, vec![]);
    let keys: Vec<_> = (0..8).map(|n| format!("{n:064x}")).collect();
    for key in &keys {
        pass.store
            .put_filter(key, &"c".repeat(64), false, &[])
            .unwrap();
    }
    let overhead = encoded(&pass.store.inner.borrow()).unwrap().len();
    let mut remaining = 63 * 1024 * 1024 - overhead;
    let mut last = 0;
    for key in &keys {
        let n = remaining.min(8 * 1024 * 1024);
        remaining -= n;
        last = n;
        pass.store
            .put_filter(key, &"c".repeat(64), false, &vec![0; n])
            .unwrap();
    }
    assert_eq!(remaining, 0);
    assert_eq!(
        encoded(&pass.store.inner.borrow()).unwrap().len(),
        63 * 1024 * 1024
    );
    let baseline = history_cap_ack_baseline(&mut pass);
    let refused = pass.store.put_filter(
        keys.last().unwrap(),
        &"c".repeat(64),
        false,
        &vec![0; last + 1],
    );
    let Step::Checkpoint { checkpoint } = pass
        .advance()
        .expect("metadata policy must yield terminal checkpoint")
    else {
        panic!("terminal checkpoint required")
    };
    assert!(refused.is_err());
    assert_eq!(checkpoint.records, baseline.records);
    assert!(!checkpoint.complete);
    assert_eq!(
        encoded(&pass.store.inner.borrow()).unwrap().len(),
        63 * 1024 * 1024
    );
    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
    assert!(
        matches!(pass.advance().unwrap(),Step::Stopped{status:Status::Incomplete{reason}} if reason=="history_metadata_limit")
    );
    history_cap_assert_cold(checkpoint, 1);
    pass.store
        .inner
        .borrow_mut()
        .put_filter(
            keys.last().unwrap(),
            &"c".repeat(64),
            false,
            &vec![0; last + 1],
        )
        .unwrap();
    let legacy = history_cap_v3_checkpoint(&pass);
    assert!(STANDARD.decode(&legacy.continuation).unwrap().len() < MAX_CONTINUATION_BYTES);
    let before = encoded(&pass.store.inner.borrow()).unwrap();
    history_cap_assert_cold(legacy.clone(), 1);
    let cold = Pass::new(scope(), Some(legacy), Limits::default()).unwrap();
    assert_eq!(encoded(&cold.store.inner.borrow()).unwrap(), before);
    // The real failed-walk path changes only its existing pending debt and one
    // failure count. Start from the acknowledged, nonterminal at-cap baseline.
    pass = Pass::new(scope(), Some(baseline.clone()), Limits::default()).unwrap();
    assert!(!pass.store.policy_limit.get());
    let mut owed = pass.store.pending().unwrap().remove(0);
    owed.dummy_debt = Some((4, 5));
    let before = encoded(&pass.store.inner.borrow()).unwrap();
    let mut invalid = owed.clone();
    invalid.cursor.row += 1;
    assert!(matches!(
        pass.store.commit_shard(crate::store::ShardCommit {
            revision_digest: owed.revision_digest.clone(),
            failed_entries: vec![owed.script.clone()],
            pending_upsert: vec![invalid],
            ..Default::default()
        }),
        Err(crate::store::StoreError::HistoryMetadataLimit)
    ));
    assert_eq!(encoded(&pass.store.inner.borrow()).unwrap(), before);
    pass = Pass::new(scope(), Some(baseline.clone()), Limits::default()).unwrap();
    assert!(!pass.store.policy_limit.get());
    pass.store
        .commit_shard(crate::store::ShardCommit {
            revision_digest: owed.revision_digest.clone(),
            failed_entries: vec![owed.script.clone()],
            pending_upsert: vec![owed],
            ..Default::default()
        })
        .unwrap();
    assert!(pass.store.policy_limit.get());
    let Step::Checkpoint { checkpoint } = pass.advance().unwrap() else {
        panic!("newly owed cover work requires a terminal durable checkpoint");
    };
    assert!(!checkpoint.complete);
    assert_eq!(checkpoint.records, baseline.records);
    assert!(pass.driver.io.borrow().pending.is_none());
    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
    let mut cold = Pass::new(scope(), Some(checkpoint), Limits::default()).unwrap();
    assert!(matches!(cold.advance().unwrap(), Step::Stopped {
        status: Status::Incomplete {reason}
    } if reason == "history_metadata_limit"));
    assert_eq!(cold.store.pending().unwrap()[0].dummy_debt, Some((4, 5)));
    assert!(cold.driver.io.borrow().pending.is_none());
}

fn history_cap_v3_checkpoint(pass: &Pass) -> Checkpoint {
    let mut cp = snapshot(pass);
    let bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&LegacyContinuationV3 {
            version: 3,
            source_id: pass.scope.source_id.clone(),
            state: pass.store.inner.borrow().clone(),
            chain: ObservedChain {
                anchor: pass.scope.anchor.clone(),
                genesis: pass.driver.chain.borrow().get(&0).cloned(),
                headers: pass.driver.chain.borrow().clone(),
            },
        })
        .unwrap();
    cp.checkpoint_id = checkpoint_id(
        &cp.source_id,
        &cp.scope_id,
        cp.generation,
        &cp.anchor,
        cp.complete,
        &bytes,
    );
    cp.continuation = STANDARD.encode(bytes);
    cp
}

#[test]
fn history_cap_v3_migration_preserves_exact_state_debt_and_source_digest() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    history_cap_seed(&mut pass, vec![history_cap_event(0)]);
    pass.driver.chain.borrow_mut().insert(50, "b".repeat(64));
    let cp = history_cap_v3_checkpoint(&pass);
    let before = encoded(&pass.store.inner.borrow()).unwrap();
    let restored = Pass::new(scope(), Some(cp.clone()), Limits::default()).unwrap();
    assert_eq!(encoded(&restored.store.inner.borrow()).unwrap(), before);
    assert_eq!(
        restored.driver.chain.borrow().get(&50),
        Some(&"b".repeat(64))
    );
    assert!(!restored.store.policy_limit.get());
    let upgraded = snapshot(&restored);
    assert_eq!(
        &STANDARD.decode(&upgraded.continuation).unwrap()[..4],
        &4u32.to_le_bytes()
    );
    let cold = Pass::new(scope(), Some(upgraded), Limits::default()).unwrap();
    assert_eq!(encoded(&cold.store.inner.borrow()).unwrap(), before);
    let mut tampered = cp.clone();
    tampered.checkpoint_id = "f".repeat(64);
    assert!(Pass::new(scope(), Some(tampered), Limits::default()).is_err());
    let mut other_source = scope();
    other_source.source_id = "e".repeat(64);
    assert!(Pass::new(other_source, Some(cp), Limits::default()).is_err());
}

#[test]
fn history_cap_reserve_encodes_the_maximum_supported_header_domain() {
    let mut maximal = scope();
    maximal.anchor.height = u64::MAX;
    let headers: BTreeMap<_, _> = (0..MAX_OBSERVED_POINTS as u64 - 1)
        .chain(std::iter::once(u64::MAX))
        .map(|height| (height, "f".repeat(64)))
        .collect();
    assert_eq!(headers.len(), MAX_OBSERVED_POINTS);
    let memory = MemoryStore::new();
    let continuation = durable(&maximal, &memory, &headers, true).unwrap();
    // Measure the actual fixed-int serializer, including both anchors, source,
    // genesis, version and terminal flag. This reserve is independent of state.
    let overhead = continuation.len() - encoded(&memory).unwrap().len();
    assert!(overhead < 512 * 1024);
    assert!(crate::shared_store::MAX_TERMINAL_STORE_BYTES + overhead < MAX_CONTINUATION_BYTES);
}

#[test]
fn history_cap_invalid_commit_is_not_a_local_policy_failure_and_cancel_wins() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    history_cap_seed(&mut pass, vec![history_cap_event(0)]);
    pass.store.inner.borrow_mut().set_event_limit(1);
    let before = encoded(&pass.store.inner.borrow()).unwrap();
    let mut invalid = history_cap_event(0);
    if let transparent_events::TransparentEvent::Receive(ref mut event) = invalid.event {
        event.value = 2;
    }
    let result = pass.store.commit_shard(crate::store::ShardCommit {
        events: vec![history_cap_event(1), invalid],
        ..Default::default()
    });
    assert!(result.is_err());
    assert!(!matches!(
        result,
        Err(crate::store::StoreError::HistoryMetadataLimit)
    ));
    assert!(!pass.store.policy_limit.get());
    assert_eq!(encoded(&pass.store.inner.borrow()).unwrap(), before);
    let refused = pass.store.commit_shard(crate::store::ShardCommit {
        events: vec![history_cap_event(1)],
        ..Default::default()
    });
    assert!(matches!(
        refused,
        Err(crate::store::StoreError::HistoryMetadataLimit)
    ));
    pass.cancel();
    assert!(matches!(
        pass.advance().unwrap(),
        Step::Stopped {
            status: Status::Cancelled
        }
    ));
    assert!(pass.finish_budget().is_err());
    assert_eq!(encoded(&pass.store.inner.borrow()).unwrap(), before);
}

#[test]
fn history_cap_cover_failure_encoder_reserve_bounds_all_pending_work() {
    let mut state = MemoryStore::new();
    let pending: Vec<_> = (0..crate::memory_store::DEFAULT_PENDING_LIMIT)
        .map(|index| {
            let mut script = vec![0; 40];
            script[..8].copy_from_slice(&(index as u64).to_le_bytes());
            crate::store::PendingPages {
                id: None,
                shard_id: 0,
                revision_digest: "b".repeat(64),
                script,
                cursor: crate::store::RunCursor {
                    child: 0,
                    row: 0,
                    older_runs: None,
                },
                carried: vec![],
                fragments: vec![],
                dummy_debt: None,
                attempts: 0,
                target_anchor: Some(crate::store::Anchor {
                    height: 100,
                    hash: "a".repeat(64),
                }),
            }
        })
        .collect();
    state
        .commit_shard(crate::store::ShardCommit {
            pending_upsert: pending,
            ..Default::default()
        })
        .unwrap();
    let before = encoded(&state).unwrap().len();
    let mut pending = state.pending().unwrap();
    for item in &mut pending {
        item.dummy_debt = Some((u8::MAX, u32::MAX));
    }
    let scripts = pending.iter().map(|item| item.script.clone()).collect();
    state
        .commit_shard(crate::store::ShardCommit {
            failed_entries: scripts,
            pending_upsert: pending,
            ..Default::default()
        })
        .unwrap();
    let delta = encoded(&state).unwrap().len() - before;
    assert_eq!(delta, crate::memory_store::DEFAULT_PENDING_LIMIT * 65);
    assert!(delta < crate::shared_store::MAX_TERMINAL_STORE_BYTES - MAX_STORE_BYTES);
}

#[test]
fn history_cap_transport_and_malformed_reply_are_not_terminal_policy() {
    for reply in [
        Reply::Unavailable,
        Reply::Data {
            body: "invalid-base64!".into(),
        },
    ] {
        let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
        let Step::Exchange { request_id, .. } = pass.advance().unwrap() else {
            panic!("init");
        };
        pass.accept(request_id, reply).unwrap();
        let Step::Checkpoint { checkpoint } = pass.advance().unwrap() else {
            panic!("failure checkpoint");
        };
        assert!(!pass.store.policy_limit.get());
        pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
        assert!(matches!(pass.advance().unwrap(), Step::Stopped {
            status: Status::Incomplete {reason}
        } if reason != "history_metadata_limit"));
    }
}
