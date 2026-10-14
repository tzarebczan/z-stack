use super::*;
use futures::Future;
fn scope() -> Scope {
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
fn snapshot(p: &Pass) -> Checkpoint {
    let bytes = durable(
        &p.scope,
        &p.store.inner.borrow(),
        &p.driver.verified.borrow(),
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
        &STANDARD.decode(upgraded.continuation).unwrap()[..4],
        &3u32.to_le_bytes()
    );
    cp.checkpoint_id = "f".repeat(64);
    assert!(Pass::new(scope(), Some(cp), Limits::default()).is_err());
}
#[test]
fn verified_cache_is_bound_and_rejects_tampering_and_oversized_maps() {
    let p = Pass::new(scope(), None, Limits::default()).unwrap();
    p.driver.chain.borrow_mut().insert(50, "b".repeat(64));
    p.driver.verified.borrow_mut().insert(50, "b".repeat(64));
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
    let oversized = AcceptedChain {
        anchor: scope().anchor,
        genesis: None,
        headers: (0..4097).map(|h| (h, "b".repeat(64))).collect(),
    };
    let bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&oversized)
        .unwrap();
    assert!(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .deserialize::<AcceptedChain>(&bytes)
        .is_err());
}
#[test]
fn budget_finish_preserves_held_checkpoint_and_fences_late_io() {
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    pass.driver.verified.borrow_mut().insert(50, "b".repeat(64));
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
            .verified
            .borrow_mut()
            .insert(tip, "b".repeat(64));
        pass.driver
            .chain
            .borrow_mut()
            .insert(boundary, "c".repeat(64));
        pass.driver
            .verified
            .borrow_mut()
            .insert(boundary, "c".repeat(64));
        pass.driver.retain_points(&needed);
        assert_eq!(pass.driver.verified.borrow().len(), 3);
        assert!(pass
            .driver
            .verified
            .borrow()
            .keys()
            .all(|h| needed.contains(h)));
    }
}
#[test]
fn mismatched_independent_reply_never_enters_verified_cache() {
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
    assert!(!pass.driver.verified.borrow().contains_key(&50));
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
        &conflict.driver.verified.borrow(),
    )
    .unwrap();
    assert!(conflict.checkpoint(&bytes).is_err());
}
