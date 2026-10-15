use super::*;

fn held() -> Pass {
    let mut pass = Pass::new(super::tests::scope(), None, Limits::default()).unwrap();
    let mut checkpoint = super::tests::snapshot(&pass);
    checkpoint.rewind_to = Some(Anchor {
        height: 20,
        hash: "b".repeat(64),
    });
    pass.driver.chain.borrow_mut().insert(20, "b".repeat(64));
    pass.held = Some(checkpoint);
    pass
}

#[test]
fn accepted_rewind_binds_full_scope_and_full_checkpoint() {
    let pass = held();
    let scope = pass.scope.clone();
    let checkpoint = pass.held.as_ref().unwrap().clone();
    let witness = pass.accepted_rewind().unwrap().unwrap();
    assert!(witness.matches(&scope, &checkpoint));
    let mut scopes = Vec::new();
    let mut changed = scope.clone();
    changed.source_id = "c".repeat(64);
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.scope_id = "c".repeat(64);
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.generation += 1;
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.anchor.hash = "c".repeat(64);
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.scripts[0].required_from += 1;
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.scripts[0].script.push_str("00");
    scopes.push(changed);
    let mut changed = scope.clone();
    changed.accepted_headers.push(Anchor {
        height: 1,
        hash: "c".repeat(64),
    });
    scopes.push(changed);
    for changed in scopes {
        assert!(!witness.matches(&changed, &checkpoint));
    }
    let mut checkpoints = Vec::new();
    let mut changed = checkpoint.clone();
    changed.previous_checkpoint_id = Some("c".repeat(64));
    checkpoints.push(changed);
    let mut changed = checkpoint.clone();
    changed.continuation.push('A');
    checkpoints.push(changed);
    let mut changed = checkpoint.clone();
    changed.complete = !changed.complete;
    checkpoints.push(changed);
    let mut changed = checkpoint.clone();
    changed.rewind_to.as_mut().unwrap().hash = "c".repeat(64);
    checkpoints.push(changed);
    let mut changed = checkpoint.clone();
    changed.records.push(Record {
        script: scope.scripts[0].script.clone(),
        event: "00".into(),
        shard_id: 0,
        revision: "c".repeat(64),
    });
    checkpoints.push(changed);
    for changed in checkpoints {
        assert_eq!(changed.checkpoint_id, checkpoint.checkpoint_id);
        assert!(!witness.matches(&scope, &changed));
    }
    // Identical context is the same authority; the owner still applies its
    // prior-ID/digest and idempotent replay checks. A different held CP is not.
    let other = held();
    assert!(witness.matches(&other.scope, other.held.as_ref().unwrap()));
}

#[test]
fn accepted_rewind_requires_live_resolved_matching_observation() {
    let mut pass = held();
    pass.driver.chain.borrow_mut().remove(&20);
    assert_eq!(
        pass.accepted_rewind().err().unwrap().0,
        "rewind_not_observed"
    );
    pass.driver.chain.borrow_mut().insert(20, "c".repeat(64));
    assert_eq!(
        pass.accepted_rewind().err().unwrap().0,
        "rewind_not_observed"
    );
    pass.driver.chain.borrow_mut().insert(20, "b".repeat(64));
    pass.lineage_pending.set(true);
    assert_eq!(pass.accepted_rewind().err().unwrap().0, "lineage_pending");
    pass.lineage_pending.set(false);
    assert!(pass.accepted_rewind().unwrap().is_some());
    pass.held.as_mut().unwrap().rewind_to = None;
    assert!(pass.accepted_rewind().unwrap().is_none());
    pass.cancel();
    assert_eq!(pass.accepted_rewind().err().unwrap().0, "cancelled");
}
