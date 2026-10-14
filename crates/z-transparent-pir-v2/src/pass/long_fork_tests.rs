#[path = "../../tests/fixture.rs"]
mod fixture;
use super::*;
use axum::http::Request;
use base64::{engine::general_purpose::STANDARD, Engine};
use fixture::{publish, synthetic, InProcess, ShardSpec, SEAL};
use transparent_events::{ReceiveEvent, SpendEvent, TransparentEvent, Txid};
use transparent_filter::ScriptBytes;

fn script(tag: u8) -> ScriptBytes {
    let mut s = vec![0x76, 0xa9, 0x14];
    s.extend([tag; 20]);
    s.extend([0x88, 0xac]);
    ScriptBytes::new(s)
}
fn events() -> Vec<(ScriptBytes, TransparentEvent)> {
    vec![
        (
            script(1),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 101,
                txid: Txid([7; 32]),
                transaction_index: 0,
                output_index: 0,
                value: 50000,
                coinbase: false,
            }),
        ),
        (
            script(1),
            TransparentEvent::Spend(SpendEvent {
                metadata: None,
                height: 102,
                spending_txid: Txid([8; 32]),
                transaction_index: 0,
                input_index: 0,
                spent_txid: Txid([7; 32]),
                spent_output_index: 0,
            }),
        ),
        (
            script(2),
            TransparentEvent::Receive(ReceiveEvent {
                metadata: None,
                height: 103,
                txid: Txid([9; 32]),
                transaction_index: 0,
                output_index: 0,
                value: 17000,
                coinbase: false,
            }),
        ),
    ]
}
fn scope() -> Scope {
    Scope {
        source_id: "1".repeat(64),
        generation: 1,
        scope_id: "2".repeat(64),
        anchor: Anchor {
            height: 110,
            hash: synthetic(110).to_display_hex(),
        },
        scripts: (1..=3)
            .map(|i| Script {
                script: hex::encode(script(i).as_slice()),
                required_from: 100,
            })
            .collect(),
        accepted_headers: std::iter::once(Anchor {
            height: 0,
            hash: transparent_filter::MAINNET_GENESIS_DISPLAY.into(),
        })
        .chain((99..=110).map(|h| Anchor {
            height: h,
            hash: synthetic(h).to_display_hex(),
        }))
        .collect(),
    }
}
fn path(table: Table) -> String {
    match table {
        Table::Directory => "directory".into(),
        Table::Pages { child } => format!("pages-{child}"),
    }
}
fn answer(server: &InProcess, operation: Operation) -> Reply {
    let (method, url, body) = match operation {
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
            table,
            segment,
        } => (
            "GET",
            format!(
                "/v2/shards/{shard_id}/revisions/{revision}/setup/{}/{segment}",
                path(table)
            ),
            vec![],
        ),
        Operation::Query {
            shard_id,
            revision,
            table,
            body,
        } => (
            "POST",
            format!(
                "/v2/shards/{shard_id}/revisions/{revision}/query/{}",
                path(table)
            ),
            STANDARD.decode(body).unwrap(),
        ),
    };
    let response = server.answer(
        Request::builder()
            .method(method)
            .uri(url)
            .header("content-type", "application/octet-stream")
            .body(body)
            .unwrap(),
    );
    assert_eq!(
        response.status(),
        200,
        "fixture reply {}",
        String::from_utf8_lossy(response.body())
    );
    Reply::Data {
        body: STANDARD.encode(response.body()),
    }
}

#[test]
fn long_fork_observations_survive_budget_and_elapsed_cold_restarts() {
    let old_dir = tempfile::tempdir().unwrap();
    let shards: Vec<_> = std::iter::once(ShardSpec::first(100, 110, true, events()))
        .chain((111..=245).map(|h| ShardSpec::first(h, h, true, vec![])))
        .collect();
    publish(old_dir.path(), &shards, SEAL, synthetic, vec![]);
    let old = InProcess::serve(old_dir.path());
    let mut request = scope();
    request.anchor = Anchor {
        height: 245,
        hash: synthetic(245).to_display_hex(),
    };
    request
        .accepted_headers
        .extend((111..=245).map(|height| Anchor {
            height,
            hash: synthetic(height).to_display_hex(),
        }));
    let mut previous = None;
    for _ in 0..12 {
        let mut p = Pass::new(request.clone(), previous.clone(), Limits::default()).unwrap();
        for _ in 0..4000 {
            match p.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => p.accept(request_id, answer(&old, operation)).unwrap(),
                Step::NeedChain { .. } => {
                    panic!("old fixture independently supplies its accepted chain")
                }
                Step::Checkpoint { mut checkpoint } => {
                    p.acknowledge(&checkpoint.checkpoint_id).unwrap();
                    checkpoint.records.clear();
                    previous = Some(checkpoint);
                }
                Step::Stopped { .. } => break,
            }
        }
        if previous.as_ref().is_some_and(|cp: &Checkpoint| cp.complete) {
            break;
        }
    }
    let old_cp = previous.unwrap();
    assert!(old_cp.complete, "old genuine acquisition must finish");
    drop(old);
    let fork = |h| {
        if h >= 105 {
            synthetic(h + 1000)
        } else {
            synthetic(h)
        }
    };
    let fork_dir = tempfile::tempdir().unwrap();
    publish(
        fork_dir.path(),
        &[ShardSpec::first(100, 245, true, events())],
        SEAL,
        fork,
        vec![],
    );
    let server = InProcess::serve(fork_dir.path());
    request.anchor.hash = fork(245).to_display_hex();
    request.accepted_headers.retain(|a| a.height == 0);
    request.accepted_headers.push(request.anchor.clone());
    let advanced_dir = tempfile::tempdir().unwrap();
    publish(
        advanced_dir.path(),
        &[ShardSpec::first(100, 247, true, events())],
        SEAL,
        fork,
        vec![],
    );
    let advanced_server = InProcess::serve(advanced_dir.path());
    let second_dir = tempfile::tempdir().unwrap();
    publish(
        second_dir.path(),
        &[ShardSpec::first(100, 247, true, events())],
        SEAL,
        |h| {
            if h >= 105 {
                synthetic(h + 2000)
            } else {
                synthetic(h)
            }
        },
        vec![],
    );
    let second_server = InProcess::serve(second_dir.path());
    for scenario in ["stable", "advancing_reorg"] {
        let mut previous = Some(old_cp.clone());
        let mut previously_read = std::collections::BTreeSet::new();
        let mut complete = false;
        for round in 0..12 {
            let epoch = usize::from(scenario == "advancing_reorg" && round >= 3);
            let target = if scenario == "stable" {
                245
            } else {
                (245 + round).min(247)
            };
            let actual = |height| {
                if height >= 105 {
                    synthetic(height + if epoch == 0 { 1000 } else { 2000 })
                } else {
                    synthetic(height)
                }
            };
            request.anchor = Anchor {
                height: target,
                hash: actual(target).to_display_hex(),
            };
            request.accepted_headers.retain(|a| a.height == 0);
            request.accepted_headers.push(request.anchor.clone());
            let current_server = if scenario == "stable" {
                &server
            } else if epoch == 0 {
                &advanced_server
            } else {
                &second_server
            };
            let mut p = Pass::new(request.clone(), previous.clone(), Limits::default()).unwrap();
            let initial_live = p.driver.chain.borrow().len();
            let mut reads = 0;
            let mut mismatches = 0;
            let mut refetched = 0;
            let mut walker = false;
            let mut rewound = false;
            for _ in 0..4000 {
                match p.advance().unwrap() {
                    Step::Exchange {
                        request_id,
                        operation,
                    } => {
                        walker |= !matches!(operation, Operation::Init | Operation::Map);
                        p.accept(request_id, answer(current_server, operation))
                            .unwrap();
                    }
                    Step::NeedChain { height, hash } => {
                        if round == 0 && reads == 12 {
                            p.finish_budget().unwrap();
                            continue;
                        }
                        reads += 1;
                        let actual = actual(height).to_display_hex();
                        mismatches += usize::from(actual != hash);
                        refetched += usize::from(!previously_read.insert(height));
                        p.accept_chain(height, actual).unwrap();
                    }
                    Step::Checkpoint { mut checkpoint } => {
                        rewound |= checkpoint.rewind_to.is_some();
                        walker |= checkpoint.rewind_to.is_some();
                        if !walker {
                            assert_eq!(
                                checkpoint.records.len(),
                                3,
                                "header-only checkpoints preserve genuine old receive/spend facts"
                            );
                            assert!(checkpoint.rewind_to.is_none());
                        }
                        p.acknowledge(&checkpoint.checkpoint_id).unwrap();
                        checkpoint.records.clear();
                        previous = Some(checkpoint);
                    }
                    Step::Stopped { status } => {
                        complete = status == Status::Complete;
                        break;
                    }
                }
            }
            eprintln!(
                "LONG_FORK_CENSUS {}",
                serde_json::json!({"scenario":scenario,"fork_epoch":epoch,"round":round,"exit":if round==0{"elapsed"}else{"chain_budget"},"reads":reads,"mismatches":mismatches,"refetched":refetched,"initial_live":initial_live,"live":p.driver.chain.borrow().len(),"durable":p.driver.chain.borrow().len(),"walker_reached":walker,"rewind":rewound,"complete":complete})
            );
            if complete {
                break;
            }
        }
        assert!(
            complete,
            "bounded cold passes must reach verified long-fork rollback: {scenario}"
        );
    }
}
