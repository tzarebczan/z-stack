use super::*;
use crate::store::{ScriptEntry, ScriptOrigin};
use crate::{
    shared_store::{SharedStore, MAX_STORE_BYTES},
    types, Limits, Pass, Status, Step,
};
use bincode::Options;
use transparent_events::{ReceiveEvent, Txid};
use transparent_shard::page_row::RunLink;
#[allow(dead_code)]
#[path = "../../tests/fixture.rs"]
mod fixture;

struct Fixture {
    _dir: tempfile::TempDir,
    entry: transparent_filter::ShardMapEntry,
    manifest: ShardManifest,
    identities: Vec<String>,
    salts: Vec<[u8; 32]>,
    target: Anchor,
    script: Vec<u8>,
    head: PageEntry,
    fragments: Vec<Vec<u8>>,
    bad: PageEntry,
    carried: Vec<TransparentEvent>,
}
fn receive(height: u32, tx: u8, output: u32) -> TransparentEvent {
    TransparentEvent::Receive(ReceiveEvent {
        metadata: None,
        height,
        txid: Txid([tx; 32]),
        transaction_index: 0,
        output_index: output,
        value: 1,
        coinbase: false,
    })
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut script = vec![0x76, 0xa9, 0x14];
        script.extend([1; 20]);
        script.extend([0x88, 0xac]);
        let mut run = vec![receive(2, 2, 0), receive(3, 3, 0)];
        run.extend((0..78).map(|output| receive(60, 40, output)));
        let carried = vec![receive(61, 61, 0), receive(62, 62, 0)];
        let events = std::iter::once(receive(1, 1, 0))
            .chain(run)
            .chain(carried.iter().copied())
            .map(|event| (transparent_filter::ScriptBytes::new(script.clone()), event))
            .collect();
        let mut spec = fixture::ShardSpec::first(0, 100, true, events);
        spec.page_span = 50;
        let map = fixture::publish(
            dir.path(),
            &[spec],
            fixture::SEAL,
            fixture::synthetic,
            vec![],
        );
        let entry = map.shards[0].clone();
        let published = dir.path().join(&entry.manifest_digest);
        let manifest: ShardManifest =
            serde_json::from_slice(&std::fs::read(published.join("manifest.json")).unwrap())
                .unwrap();
        let row = |child| {
            let bytes = std::fs::read(published.join(format!("pages-{child}.0.bin"))).unwrap();
            transparent_shard::page_row::decode_page_row(&bytes[..fixture::GEOMETRY.page_row_bytes])
                .unwrap()
                .remove(0)
        };
        let head = row(1);
        assert_eq!(head.fragment_count, 2);
        let bytes = std::fs::read(published.join("pages-1.0.bin")).unwrap();
        let fragments: Vec<_> = bytes
            .chunks_exact(fixture::GEOMETRY.page_row_bytes)
            .take(head.fragment_count as usize)
            .map(|row| row.to_vec())
            .collect();
        let count: usize = fragments
            .iter()
            .map(|row| {
                transparent_shard::page_row::decode_page_row(row).unwrap()[0]
                    .events
                    .len()
            })
            .sum();
        assert_eq!(count, 80);
        let mut bad = row(0);
        bad.fragment_count = 3;
        bad.link = RunLink {
            prev_child: 0,
            prev_row: 1,
            older_runs: 1,
        };
        let identities = manifest.child_digests().iter().map(hex::encode).collect();
        let salts = manifest
            .page_children
            .iter()
            .map(|child| {
                transparent_shard::page_tag_salt(
                    manifest.shard_id,
                    child.index,
                    child.end_height,
                    &BlockHash::from_display_hex(&child.terminal_block_hash).unwrap(),
                    child.tag_salt_counter,
                )
            })
            .collect();
        let target = Anchor {
            height: 100,
            hash: fixture::synthetic(100).to_display_hex(),
        };
        Self {
            _dir: dir,
            entry,
            manifest,
            identities,
            salts,
            target,
            script,
            head,
            fragments,
            bad,
            carried,
        }
    }
    fn walker(&self) -> Walker<'_> {
        Walker {
            entry: &self.entry,
            manifest: &self.manifest,
            geometry: fixture::GEOMETRY,
            identities: &self.identities,
            salts: &self.salts,
            target_anchor: &self.target,
        }
    }
    fn scope(&self) -> types::Scope {
        types::Scope {
            source_id: "1".repeat(64),
            scope_id: "2".repeat(64),
            generation: 1,
            anchor: types::Anchor {
                height: 100,
                hash: self.target.hash.clone(),
            },
            scripts: vec![types::Script {
                script: hex::encode(&self.script),
                required_from: 0,
            }],
            accepted_headers: vec![
                types::Anchor {
                    height: 0,
                    hash: transparent_filter::MAINNET_GENESIS_DISPLAY.into(),
                },
                types::Anchor {
                    height: 100,
                    hash: self.target.hash.clone(),
                },
            ],
        }
    }
    fn pass(&self, mixed: bool, event_cap: bool) -> (Pass, SharedStore, Walk, types::Checkpoint) {
        let mut pass = Pass::new(self.scope(), None, Limits::default()).unwrap();
        let mut store = pass.fixture_store();
        store
            .add_scripts(&[ScriptEntry {
                script: self.script.clone(),
                origin: ScriptOrigin::Derived,
                required_from: 0,
            }])
            .unwrap();
        let pending = PendingPages {
            id: None,
            shard_id: 0,
            revision_digest: self.entry.manifest_digest.clone(),
            script: self.script.clone(),
            cursor: RunCursor {
                child: u8::from(mixed),
                row: 0,
                older_runs: Some(u8::from(mixed)),
            },
            carried: if mixed {
                self.carried.clone()
            } else {
                self.head.events[..2].to_vec()
            },
            fragments: if mixed {
                self.fragments.clone()
            } else {
                vec![]
            },
            dummy_debt: None,
            attempts: 0,
            target_anchor: Some(self.target.clone()),
        };
        let events = if event_cap {
            (0..65_536)
                .map(|output| StoredEvent {
                    script: self.script.clone(),
                    event: receive(1, 90, output),
                    shard_id: 0,
                    revision_digest: self.entry.manifest_digest.clone(),
                })
                .collect()
        } else {
            vec![]
        };
        store
            .commit_shard(ShardCommit {
                events,
                pending_upsert: vec![pending],
                ..Default::default()
            })
            .unwrap();
        if !event_cap {
            let keys: Vec<_> = (0..8).map(|n| format!("{n:064x}")).collect();
            for key in &keys {
                store
                    .inner
                    .borrow_mut()
                    .put_filter(key, &"c".repeat(64), false, &[])
                    .unwrap();
            }
            let mut remaining = MAX_STORE_BYTES - encoded(&store).len();
            for key in &keys {
                let n = remaining.min(8 * 1024 * 1024);
                remaining -= n;
                store
                    .inner
                    .borrow_mut()
                    .put_filter(key, &"c".repeat(64), false, &vec![0; n])
                    .unwrap();
            }
            assert_eq!(remaining, 0);
            assert_eq!(encoded(&store).len(), MAX_STORE_BYTES);
        }
        pass.finish_budget().unwrap();
        let Step::Checkpoint { checkpoint } = pass.advance().unwrap() else {
            panic!("baseline checkpoint");
        };
        pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
        let item = store.pending().unwrap().remove(0);
        let walk = Walk {
            item,
            head: mixed.then(|| self.head.clone()),
            walked: vec![],
            done: false,
            dummies: None,
        };
        (pass, store, walk, checkpoint)
    }
    fn assert_terminal(
        &self,
        pass: &mut Pass,
        store: &SharedStore,
        prior: &types::Checkpoint,
        expected_cursor: RunCursor,
    ) {
        assert!(store.policy_limit.get());
        let pending = store.pending().unwrap();
        assert_eq!(pending[0].dummy_debt, Some((0, 2)));
        assert_eq!(pending[0].cursor, expected_cursor);
        let Step::Checkpoint { checkpoint } = pass.advance().unwrap() else {
            panic!("new debt must checkpoint");
        };
        assert!(!checkpoint.complete);
        assert_eq!(checkpoint.records, prior.records);
        assert_eq!(
            checkpoint.previous_checkpoint_id,
            Some(prior.checkpoint_id.clone())
        );
        pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
        assert!(
            matches!(pass.advance().unwrap(),Step::Stopped {status:Status::Incomplete {reason}} if reason=="history_metadata_limit")
        );
        let mut cold = Pass::new(self.scope(), Some(checkpoint), Limits::default()).unwrap();
        assert!(
            matches!(cold.advance().unwrap(),Step::Stopped {status:Status::Incomplete {reason}} if reason=="history_metadata_limit")
        );
        assert_eq!(cold.fixture_store().pending().unwrap()[0], pending[0]);
    }
}
fn encoded(store: &SharedStore) -> Vec<u8> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&*store.inner.borrow())
        .unwrap()
}

#[test]
fn real_walker_failure_preserves_stamped_anchor_debt_at_byte_cap() {
    let fixture = Fixture::new();
    let (mut pass, mut store, mut walk, prior) = fixture.pass(false, false);
    let cursor = walk.item.cursor;
    let failure = fixture
        .walker()
        .advance(
            &mut store,
            &mut walk,
            &[vec![encode_fragment(&fixture.bad).unwrap()]],
            None,
        )
        .unwrap_err();
    assert!(matches!(failure.error, SyncError::Content(_)));
    assert_eq!(failure.dummies, 2);
    fixture
        .walker()
        .fail(&mut store, &mut walk, &mut Failures::default(), failure)
        .expect("real stamped failure must preserve new debt");
    fixture.assert_terminal(&mut pass, &store, &prior, cursor);
}
fn mixed_prefix(event_cap: bool) {
    let fixture = Fixture::new();
    let (mut pass, mut store, mut walk, prior) = fixture.pass(true, event_cap);
    let cursor = walk.item.cursor;
    let before = encoded(&store);
    let failure = fixture
        .walker()
        .advance(
            &mut store,
            &mut walk,
            &[vec![encode_fragment(&fixture.bad).unwrap()]],
            None,
        )
        .unwrap_err();
    assert!(
        store.policy_limit.get(),
        "checked run must hit the real local cap"
    );
    assert!(
        matches!(failure.error, SyncError::Content(_)),
        "growth refusal must retain the known bad-next-head failure"
    );
    assert_eq!(failure.dummies, 2);
    assert_eq!(walk.item.cursor, cursor);
    assert_eq!(encoded(&store), before);
    fixture
        .walker()
        .fail(&mut store, &mut walk, &mut Failures::default(), failure)
        .unwrap();
    fixture.assert_terminal(&mut pass, &store, &prior, cursor);
}
#[test]
fn real_walker_event_cap_retains_bad_next_head_debt() {
    mixed_prefix(true)
}
#[test]
fn real_walker_byte_cap_retains_bad_next_head_debt() {
    mixed_prefix(false)
}
