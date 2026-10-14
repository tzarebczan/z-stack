mod fixture;
use axum::http::Request;
use base64::{engine::general_purpose::STANDARD, Engine};
use fixture::{publish, synthetic, InProcess, ShardSpec, SEAL};
use transparent_events::{ReceiveEvent, SpendEvent, TransparentEvent, Txid};
use transparent_filter::ScriptBytes;
use z_transparent_pir_v2::*;

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
fn drive(pass: &mut Pass, server: &InProcess) -> Checkpoint {
    let mut latest = None;
    for _ in 0..500 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => {
                let reply = answer(server, operation);
                pass.accept(request_id, reply).unwrap()
            }
            Step::NeedChain { .. } => panic!("fixture provides every independent header"),
            Step::Checkpoint { checkpoint } => {
                assert_eq!(
                    pass.advance().unwrap(),
                    Step::Checkpoint {
                        checkpoint: checkpoint.clone()
                    }
                );
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                latest = Some(checkpoint)
            }
            Step::Stopped {
                status: Status::Complete,
            } => return latest.unwrap(),
            Step::Stopped { status } => panic!("incomplete: {status:?}"),
        }
    }
    panic!("did not converge")
}
#[test]
fn nonzero_receive_spend_unused_and_durable_resume() {
    let dir = tempfile::tempdir().unwrap();
    publish(
        dir.path(),
        &[ShardSpec::first(100, 110, true, events())],
        SEAL,
        synthetic,
        vec![],
    );
    let server = InProcess::serve(dir.path());
    let mut pass = Pass::new(scope(), None, Limits::default()).unwrap();
    let cp = drive(&mut pass, &server);
    assert!(cp.complete);
    assert_eq!(cp.pending_pages, 0);
    assert_eq!(cp.unresolved_spends, 0);
    assert_eq!(cp.coverage.len(), 3);
    assert!(cp.coverage.iter().all(|c| c.through == 110));
    let mut recovered: Vec<_> = cp
        .records
        .iter()
        .map(|r| {
            (
                hex::decode(&r.script).unwrap(),
                TransparentEvent::from_bytes(&hex::decode(&r.event).unwrap()).unwrap(),
            )
        })
        .collect();
    recovered.sort_by_key(|(_, e)| e.height());
    let expected: Vec<_> = events()
        .into_iter()
        .map(|(s, e)| (s.as_slice().to_vec(), e))
        .collect();
    assert_eq!(recovered, expected);
    assert!(cp.rewind_to.is_none());
    let mut saved = cp.clone();
    saved.records.clear();
    let mut next = scope();
    next.generation = 2;
    next.scope_id = "3".repeat(64);
    let mut reopened = Pass::new(next, Some(saved), Limits::default()).unwrap();
    let again = drive(&mut reopened, &server);
    assert_eq!(again.records, cp.records);
    assert_eq!(again.previous_checkpoint_id, Some(cp.checkpoint_id));
    assert!(again.rewind_to.is_none());
}
#[test]
fn unacknowledged_checkpoint_and_consumed_reply_are_fenced() {
    let mut p = Pass::new(scope(), None, Limits::default()).unwrap();
    let Step::Exchange { request_id, .. } = p.advance().unwrap() else {
        panic!()
    };
    assert!(p.accept(request_id + 1, Reply::Unavailable).is_err());
    p.accept(request_id, Reply::Unavailable).unwrap();
    assert!(p.accept(request_id, Reply::Unavailable).is_err());
    let Step::Checkpoint { checkpoint } = p.advance().unwrap() else {
        panic!()
    };
    assert!(p.acknowledge("wrong").is_err());
    assert_eq!(p.advance().unwrap(), Step::Checkpoint { checkpoint });
    p.cancel();
    assert_eq!(
        p.advance().unwrap(),
        Step::Stopped {
            status: Status::Cancelled
        }
    );
}
#[derive(Clone)]
struct OracleTransport(InProcess);
impl transparent_wallet::transport::FilterSource for OracleTransport {
    fn shard_map(&mut self) -> Result<(Vec<u8>, u64), transparent_wallet::transport::BoxError> {
        Ok(raw_reply(&self.0, Operation::Map))
    }
    fn filter(
        &mut self,
        shard_id: u64,
    ) -> Result<(Vec<u8>, u64), transparent_wallet::transport::BoxError> {
        Ok(raw_reply(&self.0, Operation::Filter { shard_id }))
    }
}
impl transparent_wallet::transport::ShardTransport for OracleTransport {
    fn init(&mut self) -> Result<(Vec<u8>, u64), transparent_wallet::transport::BoxError> {
        Ok(raw_reply(&self.0, Operation::Init))
    }
    fn manifest(
        &mut self,
        shard_id: u64,
        revision: &str,
    ) -> Result<(Vec<u8>, u64), transparent_wallet::transport::BoxError> {
        Ok(raw_reply(
            &self.0,
            Operation::Manifest {
                shard_id,
                revision: revision.into(),
            },
        ))
    }
    fn setup(
        &mut self,
        shard_id: u64,
        revision: &str,
        table: transparent_wallet::client::Table,
        segment: u32,
    ) -> Result<(Vec<u8>, u64), transparent_wallet::transport::BoxError> {
        Ok(raw_reply(
            &self.0,
            Operation::Setup {
                shard_id,
                revision: revision.into(),
                table: wire_table(table),
                segment,
            },
        ))
    }
    fn query(
        &mut self,
        shard_id: u64,
        revision: &str,
        table: transparent_wallet::client::Table,
        body: &[u8],
    ) -> Result<Vec<u8>, transparent_wallet::transport::BoxError> {
        Ok(raw_reply(
            &self.0,
            Operation::Query {
                shard_id,
                revision: revision.into(),
                table: wire_table(table),
                body: STANDARD.encode(body),
            },
        )
        .0)
    }
}
fn wire_table(t: transparent_wallet::client::Table) -> Table {
    match t {
        transparent_wallet::client::Table::Directory => Table::Directory,
        transparent_wallet::client::Table::Pages(child) => Table::Pages { child },
    }
}
fn raw_reply(s: &InProcess, o: Operation) -> (Vec<u8>, u64) {
    let Reply::Data { body } = answer(s, o) else {
        panic!()
    };
    let b = STANDARD.decode(body).unwrap();
    let n = b.len() as u64;
    (b, n)
}
#[test]
fn nonzero_async_walker_matches_pinned_upstream_store() {
    use transparent_wallet::{
        MemoryStore, ScriptEntry, ScriptOrigin, StaticChain, StaticScripts, WalletStore,
    };
    let dir = tempfile::tempdir().unwrap();
    let map = publish(
        dir.path(),
        &[ShardSpec::first(100, 110, true, events())],
        SEAL,
        synthetic,
        vec![],
    );
    let server = InProcess::serve(dir.path());
    let geometry = transparent_wallet::parse_init(&raw_reply(&server, Operation::Init).0).unwrap();
    let mut oracle = MemoryStore::new();
    let mut scripts = StaticScripts(
        scope()
            .scripts
            .into_iter()
            .map(|s| ScriptEntry {
                script: hex::decode(s.script).unwrap(),
                origin: ScriptOrigin::Derived,
                required_from: s.required_from,
            })
            .collect(),
    );
    let headers = StaticChain {
        hashes: scope()
            .accepted_headers
            .into_iter()
            .map(|a| (a.height, a.hash))
            .collect(),
    };
    let mut filters = OracleTransport(server.clone());
    let mut transport = filters.clone();
    let result = transparent_wallet::sync_into(
        &mut oracle,
        &map,
        serde_json::to_vec(&map).unwrap().len() as u64,
        &geometry,
        &headers,
        &mut scripts,
        &mut filters,
        &mut transport,
        &transparent_wallet::WorkLimits {
            max_queries: Some(64),
            max_private_bytes: Some(96 * 1024 * 1024),
        },
        &transparent_wallet::Anchor {
            height: 110,
            hash: synthetic(110).to_display_hex(),
        },
    )
    .unwrap();
    assert_eq!(result.completion, transparent_wallet::Completion::Complete);
    let mut p = Pass::new(scope(), None, Limits::default()).unwrap();
    let cp = drive(&mut p, &server);
    let expected: Vec<_> = oracle
        .events()
        .unwrap()
        .into_iter()
        .map(|e| Record {
            script: hex::encode(e.script),
            event: hex::encode(e.event.to_bytes()),
            shard_id: e.shard_id,
            revision: e.revision_digest,
        })
        .collect();
    assert!(
        cp.records == expected,
        "recovered facts differ from the pinned oracle"
    );
    for script in &scope().scripts {
        let held = oracle
            .coverage(&hex::decode(&script.script).unwrap())
            .unwrap();
        let got = cp
            .coverage
            .iter()
            .find(|c| c.script == script.script)
            .unwrap();
        assert_eq!(
            got.through,
            held.iter().map(|r| r.end_height).max().unwrap()
        );
    }
}
#[test]
fn bounded_passes_resume_past_cached_filter_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let shards: Vec<_> = (100..140)
        .map(|h| ShardSpec::first(h, h, true, vec![]))
        .collect();
    publish(dir.path(), &shards, SEAL, synthetic, vec![]);
    let server = InProcess::serve(dir.path());
    let mut request = scope();
    request.anchor = Anchor {
        height: 139,
        hash: synthetic(139).to_display_hex(),
    };
    request.accepted_headers = std::iter::once(Anchor {
        height: 0,
        hash: transparent_filter::MAINNET_GENESIS_DISPLAY.into(),
    })
    .chain((99..140).map(|h| Anchor {
        height: h,
        hash: synthetic(h).to_display_hex(),
    }))
    .collect();
    request.scripts.truncate(1);
    let mut checkpoint = None;
    let mut filters = Vec::new();
    let mut complete = false;
    for _ in 0..20 {
        let mut limits = Limits::default();
        limits.max_exchanges = 6;
        let mut p = Pass::new(request.clone(), checkpoint.take(), limits).unwrap();
        for _ in 0..100 {
            match p.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => {
                    if let Operation::Filter { shard_id } = operation {
                        filters.push(shard_id)
                    }
                    let r = answer(&server, operation);
                    p.accept(request_id, r).unwrap()
                }
                Step::Checkpoint { checkpoint: cp } => {
                    p.acknowledge(&cp.checkpoint_id).unwrap();
                    checkpoint = Some(cp)
                }
                Step::Stopped { status } => {
                    complete = status == Status::Complete;
                    break;
                }
                Step::NeedChain { .. } => panic!("all headers supplied"),
            }
        }
        if complete {
            break;
        }
    }
    assert!(complete, "bounded recovery did not converge");
    assert_eq!(
        filters,
        (0..40).collect::<Vec<_>>(),
        "no prefix is downloaded again"
    );
    assert_eq!(checkpoint.unwrap().coverage[0].through, 139);
}
#[test]
fn page_run_chain_recovers_nonzero_history_through_real_pir() {
    let dir = tempfile::tempdir().unwrap();
    let mut history: Vec<_> = (0..7u8)
        .map(|n| {
            (
                script(1),
                TransparentEvent::Receive(ReceiveEvent {
                    metadata: None,
                    height: 101 + u32::from(n),
                    txid: Txid([40 + n; 32]),
                    transaction_index: 0,
                    output_index: 0,
                    value: 111 + u64::from(n),
                    coinbase: false,
                }),
            )
        })
        .collect();
    history.push((
        script(1),
        TransparentEvent::Spend(SpendEvent {
            metadata: None,
            height: 111,
            spending_txid: Txid([99; 32]),
            transaction_index: 0,
            input_index: 0,
            spent_txid: Txid([40; 32]),
            spent_output_index: 0,
        }),
    ));
    let mut spec = ShardSpec::first(100, 120, true, history.clone());
    spec.page_span = 4;
    publish(dir.path(), &[spec], SEAL, synthetic, vec![]);
    let server = InProcess::serve(dir.path());
    let mut request = scope();
    request.anchor = Anchor {
        height: 120,
        hash: synthetic(120).to_display_hex(),
    };
    request.accepted_headers = std::iter::once(Anchor {
        height: 0,
        hash: transparent_filter::MAINNET_GENESIS_DISPLAY.into(),
    })
    .chain((99..=120).map(|h| Anchor {
        height: h,
        hash: synthetic(h).to_display_hex(),
    }))
    .collect();
    let mut p = Pass::new(request, None, Limits::default()).unwrap();
    let cp = drive(&mut p, &server);
    let mut actual: Vec<_> = cp
        .records
        .iter()
        .map(|r| TransparentEvent::from_bytes(&hex::decode(&r.event).unwrap()).unwrap())
        .collect();
    actual.sort_by_key(TransparentEvent::height);
    assert_eq!(
        actual,
        history.into_iter().map(|(_, e)| e).collect::<Vec<_>>()
    );
    assert!(cp.complete);
    assert_eq!(cp.pending_pages, 0);
}
#[test]
fn malformed_private_content_remains_incomplete_and_failure_cap_survives_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = ShardSpec::first(100, 110, true, events());
    spec.malformed_entries = vec![script(1)];
    publish(dir.path(), &[spec], SEAL, synthetic, vec![]);
    let server = InProcess::serve(dir.path());
    let mut previous = None;
    let mut counts = Vec::new();
    for _ in 0..4 {
        let mut p = Pass::new(scope(), previous.take(), Limits::default()).unwrap();
        let mut queries = 0;
        for _ in 0..100 {
            match p.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => {
                    if matches!(operation, Operation::Query { .. }) {
                        queries += 1
                    }
                    let reply = answer(&server, operation);
                    p.accept(request_id, reply).unwrap()
                }
                Step::Checkpoint { checkpoint } => {
                    assert!(!checkpoint.complete);
                    p.acknowledge(&checkpoint.checkpoint_id).unwrap();
                    previous = Some(checkpoint)
                }
                Step::Stopped {
                    status: Status::Incomplete { reason },
                } => {
                    assert_eq!(reason, "content_failed");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        counts.push(queries);
    }
    assert!(counts[0] > 0);
    assert_eq!(counts[3], 0, "held entries must not be queried again");
    let cp = previous.unwrap();
    assert!(!cp.complete);
    assert!(cp
        .records
        .iter()
        .all(|r| r.script != hex::encode(script(1).as_slice())));
}

#[test]
fn genesis_only_publication_emits_real_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let genesis = transparent_filter::BlockHash::from_display_hex(
        transparent_filter::MAINNET_GENESIS_DISPLAY,
    )
    .unwrap();
    publish(
        dir.path(),
        &[ShardSpec::first(0, 0, true, vec![])],
        SEAL,
        |_| genesis,
        vec![],
    );
    let server = InProcess::serve(dir.path());
    let mut request = scope();
    request.anchor = Anchor {
        height: 0,
        hash: genesis.to_display_hex(),
    };
    request.accepted_headers = vec![request.anchor.clone()];
    request.scripts.truncate(1);
    request.scripts[0].required_from = 0;
    let mut pass = Pass::new(request, None, Limits::default()).unwrap();
    let checkpoint = drive(&mut pass, &server);
    assert!(checkpoint.complete);
    assert_eq!(
        checkpoint.coverage.len(),
        1,
        "genesis coverage must carry its revision"
    );
    assert_eq!(checkpoint.coverage[0].through, 0);
    assert_eq!(checkpoint.coverage[0].revision.len(), 64);
}

#[test]
fn malformed_page_dummy_debt_survives_budget_and_cold_resume() {
    let dir = tempfile::tempdir().unwrap();
    let history = (0..3u8)
        .map(|n| {
            (
                script(1),
                TransparentEvent::Receive(ReceiveEvent {
                    metadata: None,
                    height: 101 + u32::from(n),
                    txid: Txid([70 + n; 32]),
                    transaction_index: 0,
                    output_index: 0,
                    value: 100,
                    coinbase: false,
                }),
            )
        })
        .collect();
    let mut spec = ShardSpec::first(100, 110, true, history);
    spec.malformed_page_heads = true;
    publish(dir.path(), &[spec], SEAL, synthetic, vec![]);
    let server = InProcess::serve(dir.path());
    let mut request = scope();
    request.scripts.truncate(1);
    let mut previous = None;
    let mut page_counts = Vec::new();
    let mut reasons = Vec::new();
    for budget in [3, 1, 1, 1] {
        let mut limits = Limits::default();
        limits.max_queries = budget;
        let mut pass = Pass::new(request.clone(), previous.take(), limits).unwrap();
        let mut page_queries = 0;
        for _ in 0..100 {
            match pass.advance().unwrap() {
                Step::Exchange {
                    request_id,
                    operation,
                } => {
                    if matches!(
                        operation,
                        Operation::Query {
                            table: Table::Pages { .. },
                            ..
                        }
                    ) {
                        page_queries += 1;
                    }
                    let reply = answer(&server, operation);
                    pass.accept(request_id, reply).unwrap();
                }
                Step::Checkpoint { mut checkpoint } => {
                    assert!(!checkpoint.complete);
                    pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                    checkpoint.records.clear(); // Exact engine persistence boundary.
                    previous = Some(checkpoint);
                }
                Step::Stopped {
                    status: Status::Incomplete { reason },
                } => {
                    reasons.push(reason);
                    break;
                }
                _ => panic!("unexpected fixture step"),
            }
        }
        page_counts.push(page_queries);
    }
    assert_eq!(
        page_counts,
        vec![1, 1, 1, 1],
        "all three owed dummy queries must survive cold budget boundaries before another real head"
    );
    assert_eq!(
        reasons,
        vec![
            "query_budget",
            "query_budget",
            "query_budget",
            "content_failed"
        ],
        "partial dummy rounds must keep debt and remain budget-incomplete until fully paid"
    );
}

fn page_session(
    server: &InProcess,
    request: Scope,
    previous: Option<Checkpoint>,
    limits: Limits,
    stop: &str,
) -> (Checkpoint, usize, Status) {
    let mut pass = Pass::new(request, previous, limits).unwrap();
    let mut latest = None;
    let mut pages = 0;
    for _ in 0..100 {
        match pass.advance().unwrap() {
            Step::Exchange {
                request_id,
                operation,
            } => {
                let is_page = matches!(
                    operation,
                    Operation::Query {
                        table: Table::Pages { .. },
                        ..
                    }
                );
                pages += usize::from(is_page);
                let reply = if is_page
                    && ((stop == "overload" && pages == 2)
                        || (stop == "partial_overload" && pages == 3))
                {
                    Reply::Overloaded {
                        retry_after: Some(1),
                    }
                } else {
                    answer(server, operation)
                };
                pass.accept(request_id, reply).unwrap();
            }
            Step::Checkpoint { mut checkpoint } => {
                assert!(!checkpoint.complete);
                pass.acknowledge(&checkpoint.checkpoint_id).unwrap();
                checkpoint.records.clear();
                if stop == "cancel" && pages == 1 {
                    // Native/Web may drop the pass after this durable ack,
                    // before dispatching the pending dummy exchange.
                    pass.cancel();
                    return (checkpoint, pages, Status::Cancelled);
                }
                latest = Some(checkpoint);
            }
            Step::Stopped { status } => return (latest.unwrap(), pages, status),
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("page session did not stop");
}

#[test]
fn malformed_page_debt_precedes_real_queries_after_cancel_overload_and_anchor_advance() {
    let dir = tempfile::tempdir().unwrap();
    let history = (0..3u8)
        .map(|n| {
            (
                script(1),
                TransparentEvent::Receive(ReceiveEvent {
                    metadata: None,
                    height: 101 + u32::from(n),
                    txid: Txid([80 + n; 32]),
                    transaction_index: 0,
                    output_index: 0,
                    value: 100,
                    coinbase: false,
                }),
            )
        })
        .collect();
    let mut spec = ShardSpec::first(100, 120, true, history);
    spec.malformed_page_heads = true;
    publish(dir.path(), &[spec], SEAL, synthetic, vec![]);
    let server = InProcess::serve(dir.path());
    let mut original = scope();
    original.scripts.truncate(1);
    let (_, uninterrupted, status) =
        page_session(&server, original.clone(), None, Limits::default(), "");
    assert_eq!(
        uninterrupted, 4,
        "one genuine head followed by its three dummy queries"
    );
    assert_eq!(
        status,
        Status::Incomplete {
            reason: "content_failed".into()
        }
    );
    for stop in ["cancel", "overload", "partial_overload"] {
        let (first, attempted, status) =
            page_session(&server, original.clone(), None, Limits::default(), stop);
        if stop == "cancel" {
            assert_eq!(attempted, 1);
            assert_eq!(status, Status::Cancelled);
        } else {
            assert_eq!(
                status,
                Status::Incomplete {
                    reason: "overloaded".into()
                }
            );
        }
        let mut request = original.clone();
        // A new block/expanded scope must not erase a debt under the same revision.
        request.anchor = Anchor {
            height: 111,
            hash: synthetic(111).to_display_hex(),
        };
        request.accepted_headers.push(request.anchor.clone());
        request.generation += 1;
        request.scope_id = "4".repeat(64);
        let mut previous = Some(first);
        let mut paid = Vec::new();
        for phase in 0..3 {
            let mut limits = Limits::default();
            limits.max_queries = 1;
            let (checkpoint, queries, status) =
                page_session(&server, request.clone(), previous.take(), limits, "");
            let reason = if phase < 2 {
                "query_budget"
            } else {
                "content_failed"
            };
            assert_eq!(
                status,
                Status::Incomplete {
                    reason: reason.into()
                },
                "a partial dummy round cannot forgive the unpaid suffix"
            );
            paid.push(queries);
            previous = Some(checkpoint);
        }
        assert_eq!(
            paid,
            vec![1, 1, 1],
            "{stop}: persisted debt must be paid before retrying a malformed head"
        );
        let mut limits = Limits::default();
        limits.max_queries = 1;
        let (_, queries, status) = page_session(&server, request, previous, limits, "");
        assert_eq!(
            queries, 1,
            "the next pass may retry the real head only after paying the debt"
        );
        assert_eq!(
            status,
            Status::Incomplete {
                reason: "query_budget".into()
            }
        );
    }
}
