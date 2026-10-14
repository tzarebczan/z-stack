//! Executable draft of the next manifest's routing contract.
//! This is deliberately separate from the v6 runtime and wallet wire types.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const REVISION: &str = "ironwood-enhance-pir-v7-draft";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Domain {
    id: u64,
    logical_rows: u64,
    populated_spans: Vec<Span>,
    content_sha256: String,
    setup_sha256: String,
    parameter_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    start: u64,
    end: u64,
}

impl Domain {
    // A session changes with PIR material, not with publication or placement.
    fn session_id(&self) -> Result<String, String> {
        if self.logical_rows == 0 || self.populated_spans.is_empty() {
            return Err("invalid domain geometry".into());
        }
        let mut end = 0;
        for span in &self.populated_spans {
            if span.start < end || span.end <= span.start || span.end > self.logical_rows {
                return Err("invalid populated spans".into());
            }
            end = span.end;
        }
        let mut hash = Sha256::new();
        hash.update(b"enhance-pir/v7-draft/session\0");
        hash.update(self.id.to_le_bytes());
        hash.update(self.logical_rows.to_le_bytes());
        hash.update((self.populated_spans.len() as u64).to_le_bytes());
        for span in &self.populated_spans {
            hash.update(span.start.to_le_bytes());
            hash.update(span.end.to_le_bytes());
        }
        for digest in [&self.content_sha256, &self.setup_sha256] {
            hash.update(decode_digest(digest)?);
        }
        hash.update((self.parameter_id.len() as u64).to_le_bytes());
        hash.update(self.parameter_id.as_bytes());
        Ok(hex::encode(hash.finalize()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Route {
    global_start: u64,
    global_end: u64,
    domain_id: u64,
    local_start: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingView {
    protocol_revision: String,
    revision: u64,
    recovery_epoch: u64,
    anchor_height: u64,
    anchor_block_hash: String,
    total_rows: u64,
    domains: Vec<Domain>,
    routes: Vec<Route>,
}

fn decode_digest(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64
        || value
            .bytes()
            .any(|b| !b.is_ascii_hexdigit() || b.is_ascii_uppercase())
    {
        return Err("digest must be lowercase sha256 hex".into());
    }
    hex::decode(value)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "incorrect digest width".into())
}

impl RoutingView {
    fn validate(&self) -> Result<(), String> {
        if self.protocol_revision != REVISION || self.revision == 0 || self.total_rows == 0 {
            return Err("incompatible or empty routing view".into());
        }
        decode_digest(&self.anchor_block_hash)?;
        let mut domains = BTreeMap::new();
        for domain in &self.domains {
            domain.session_id()?;
            if domains.insert(domain.id, domain).is_some() {
                return Err("duplicate domain".into());
            }
        }
        let mut end = 0;
        let mut local_routes: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
        for route in &self.routes {
            if route.global_start != end || route.global_end <= end {
                return Err("routing gap, overlap or unordered interval".into());
            }
            let domain = domains.get(&route.domain_id).ok_or("unknown domain")?;
            let local_end = route
                .local_start
                .checked_add(route.global_end - end)
                .ok_or("local overflow")?;
            if !domain
                .populated_spans
                .iter()
                .any(|span| route.local_start >= span.start && local_end <= span.end)
            {
                return Err("route exceeds populated domain span".into());
            }
            local_routes
                .entry(route.domain_id)
                .or_default()
                .push((route.local_start, local_end));
            end = route.global_end;
        }
        if end != self.total_rows {
            return Err("incomplete routing".into());
        }
        for spans in local_routes.values_mut() {
            spans.sort_unstable();
            if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
                return Err("two global ranges alias the same local rows".into());
            }
        }
        Ok(())
    }

    fn locate(&self, row: u64) -> Option<(u64, u64)> {
        let route = self
            .routes
            .iter()
            .find(|r| r.global_start <= row && row < r.global_end)?;
        Some((
            route.domain_id,
            route.local_start + row - route.global_start,
        ))
    }

    // A revision mismatch is observable only if the server checks the view ID.
    fn query_is_current(&self, revision: u64, recovery_epoch: u64) -> bool {
        self.revision == revision && self.recovery_epoch == recovery_epoch
    }
}

fn domain(id: u64, logical_rows: u64, populated_rows: u64, content: u8) -> Domain {
    Domain {
        id,
        logical_rows,
        populated_spans: vec![Span {
            start: 0,
            end: populated_rows,
        }],
        content_sha256: format!("{content:02x}").repeat(32),
        setup_sha256: "ab".repeat(32),
        parameter_id: "simplepir-p16-q48-v1".into(),
    }
}

fn composed() -> RoutingView {
    RoutingView {
        protocol_revision: REVISION.into(),
        revision: 7,
        recovery_epoch: 0,
        anchor_height: 100,
        anchor_block_hash: "cd".repeat(32),
        total_rows: 32_768 + 2_048,
        domains: vec![domain(1, 32_768, 32_768, 1), {
            let mut tail = domain(2, 8_192, 2_048, 2);
            tail.populated_spans.push(Span {
                start: 4_096,
                end: 8_192,
            });
            tail
        }],
        routes: vec![
            Route {
                global_start: 0,
                global_end: 28_672,
                domain_id: 1,
                local_start: 0,
            },
            Route {
                global_start: 28_672,
                global_end: 32_768,
                domain_id: 2,
                local_start: 4_096,
            },
            Route {
                global_start: 32_768,
                global_end: 34_816,
                domain_id: 2,
                local_start: 0,
            },
        ],
    }
}

#[test]
fn composed_view_round_trips_and_maps_b_without_moving_its_local_rows() {
    let view: RoutingView =
        serde_json::from_slice(&serde_json::to_vec(&composed()).unwrap()).unwrap();
    view.validate().unwrap();
    assert_eq!(view.locate(28_672), Some((2, 4_096)));
    assert_eq!(view.locate(32_768), Some((2, 0)));
    assert_eq!(view.locate(34_815), Some((2, 2_047)));
    assert_eq!(view.locate(34_816), None);
}

#[test]
fn rejects_gaps_overlaps_bounds_unknown_domains_and_unknown_fields() {
    let mut view = composed();
    view.routes[1].global_start += 1;
    assert!(view.validate().is_err());
    view = composed();
    view.routes[1].global_start -= 1;
    assert!(view.validate().is_err());
    view = composed();
    view.routes[1].local_start = 5_000;
    assert!(view.validate().is_err());
    view = composed();
    view.routes[1].domain_id = 3;
    assert!(view.validate().is_err());
    view = composed();
    view.routes[2].local_start = 2_048; // allocated padding between B and A's suffix
    assert!(view.validate().is_err());
    view = composed();
    view.routes[2].local_start = 4_096; // aliases A's suffix in the same domain
    assert!(view.validate().is_err());
    view = composed();
    view.domains.push(view.domains[0].clone());
    assert!(view.validate().is_err());
    view = composed();
    view.anchor_block_hash = "not-a-digest".into();
    assert!(view.validate().is_err());
    let mut json = serde_json::to_value(composed()).unwrap();
    json["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RoutingView>(json).is_err());
}

#[test]
fn routing_change_preserves_sealed_session_but_fences_stale_query() {
    let old = composed();
    let sealed_session = old.domains[0].session_id().unwrap();
    let mut new = old.clone();
    new.revision += 1;
    new.domains[1] = domain(2, 4_096, 2_048, 3);
    new.routes[1].domain_id = 1;
    new.routes[1].local_start = 28_672;
    new.routes[2].local_start = 0;
    new.validate().unwrap();
    assert_eq!(new.domains[0].session_id().unwrap(), sealed_session);
    assert_ne!(
        old.domains[1].session_id().unwrap(),
        new.domains[1].session_id().unwrap()
    );
    assert!(!new.query_is_current(old.revision, old.recovery_epoch));
    assert_eq!(new.locate(28_672), Some((1, 28_672)));
    assert_eq!(new.locate(32_768), Some((2, 0)));
    new.recovery_epoch += 1;
    assert!(!new.query_is_current(new.revision, old.recovery_epoch));
}

#[test]
fn session_identity_changes_with_content_geometry_setup_or_parameters() {
    let original = domain(4, 4_096, 2_048, 1);
    let id = original.session_id().unwrap();
    let mut changed = original.clone();
    changed.content_sha256 = "ef".repeat(32);
    assert_ne!(id, changed.session_id().unwrap());
    changed = original.clone();
    changed.logical_rows = 8_192;
    assert_ne!(id, changed.session_id().unwrap());
    changed = original.clone();
    changed.setup_sha256 = "ef".repeat(32);
    assert_ne!(id, changed.session_id().unwrap());
    changed = original.clone();
    changed.parameter_id.push('x');
    assert_ne!(id, changed.session_id().unwrap());
}
