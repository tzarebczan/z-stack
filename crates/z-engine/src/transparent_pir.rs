//! Publisher-accepted transparent discovery metadata. Canonical wallet rows own value.
use crate::error::{EngineError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use transparent_events::TransparentEvent;

pub(crate) fn refused(code: &str) -> EngineError {
    EngineError::Message(code.into())
}

pub use z_transparent_pir_v2 as acquisition;
pub use z_transparent_pir_v2::{
    Anchor as TransparentPirAnchor, Checkpoint as TransparentPirCheckpoint,
    Record as TransparentPirRecord, Scope as TransparentPirScope, Script as TransparentPirScript,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectionState {
    pub scope: Option<TransparentPirScope>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub wallet_cursor: Option<u32>,
    #[serde(default)]
    pub bound_source: Option<String>,
    #[serde(default)]
    pub rewind_floor: Option<u32>,
    pub checkpoint_id: Option<String>,
    pub checkpoint_digest: Option<String>,
    #[serde(default)]
    pub projection_invalidated: bool,
    pub continuation: Option<TransparentPirCheckpoint>,
    pub covered_through: u64,
    pub complete: bool,
    pub pending_pages: u64,
    pub unresolved_spends: u64,
    /// Identities and digests only. No second copy of wallet values or balance.
    pub records: BTreeMap<String, (u32, String)>,
    #[serde(default)]
    pub projected_outputs: BTreeSet<String>,
    /// Web funding remains blocked independently of provisional-row deletion ownership.
    /// None is legacy/unknown provenance and requires an explicit canonical rescan.
    #[serde(default)]
    pub unverified_outputs: Option<BTreeSet<String>>,
    #[serde(default)]
    pub projected_spends: BTreeSet<String>,
    #[serde(default)]
    pub projected_transactions: BTreeSet<String>,
    #[serde(skip)]
    pub durable: bool,
}

pub(crate) fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(json: &str) -> Result<T> {
    if json.len() > 128 * 1024 * 1024 {
        return Err(refused("transparent_pir_size_limit"));
    }
    serde_json::from_str(json).map_err(|_| refused("transparent_pir_input_invalid"))
}

impl ProjectionState {
    pub fn begin(
        &mut self,
        source: &str,
        anchor: TransparentPirAnchor,
        mut scripts: Vec<TransparentPirScript>,
    ) -> Result<TransparentPirScope> {
        scripts.sort_by(|a, b| a.script.cmp(&b.script));
        scripts.dedup();
        if !hash(source) || !hash(&anchor.hash) || scripts.is_empty() || scripts.len() > 4096 {
            return Err(refused("transparent_pir_scope_invalid"));
        }
        if self
            .bound_source
            .as_deref()
            .or_else(|| self.scope.as_ref().map(|s| s.source_id.as_str()))
            .is_some_and(|old| old != source)
        {
            return Err(refused("transparent_pir_source_changed"));
        }
        if scripts.iter().any(|s| {
            s.required_from > anchor.height
                || crate::web::from_hex(&s.script)
                    .ok()
                    .is_none_or(|b| b.is_empty() || b.len() > 40)
        }) {
            return Err(refused("transparent_pir_scope_invalid"));
        }
        let scope_id = digest(
            &serde_json::to_vec(&scripts).map_err(|_| refused("transparent_pir_scope_invalid"))?,
        );
        if let Some(old) = &self.scope {
            if old.source_id == source && old.anchor == anchor && old.scope_id == scope_id {
                return Ok(old.clone());
            }
            // A lower target cannot authenticate the saved anchor yet. Keep the
            // private journal until canonical scanning reaches that height again.
            if anchor.height < old.anchor.height {
                return Err(refused("transparent_pir_rewind_required"));
            }
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| refused("transparent_pir_generation_exhausted"))?;
        let same_source = self.bound_source.as_deref() == Some(source)
            || self.scope.as_ref().is_some_and(|s| s.source_id == source);
        if self
            .scope
            .as_ref()
            .is_some_and(|old| old.scope_id != scope_id)
        {
            // Aggregate coverage belongs to this exact script inventory. The
            // continuation retains each old script's independent coverage.
            self.covered_through = 0;
        }
        self.bound_source = Some(source.into());
        self.generation = generation;
        let scope = TransparentPirScope {
            source_id: source.into(),
            generation,
            scope_id,
            accepted_headers: vec![anchor.clone()],
            anchor,
            scripts,
        };
        self.scope = Some(scope.clone());
        if !same_source {
            self.checkpoint_id = None;
            self.checkpoint_digest = None;
            self.continuation = None;
            self.covered_through = 0;
            self.projected_outputs.clear();
            self.projected_spends.clear();
            self.projected_transactions.clear();
        }
        self.complete = false;
        self.durable = false;
        Ok(scope)
    }

    pub fn validate(
        &self,
        checkpoint: &TransparentPirCheckpoint,
    ) -> Result<Vec<(Vec<u8>, TransparentEvent)>> {
        let scope = self
            .scope
            .as_ref()
            .ok_or_else(|| refused("transparent_pir_consent_required"))?;
        if checkpoint.source_id != scope.source_id
            || checkpoint.generation != scope.generation
            || checkpoint.scope_id != scope.scope_id
            || checkpoint.anchor != scope.anchor
        {
            return Err(refused("transparent_pir_context_changed"));
        }
        let bytes = checkpoint_payload(checkpoint)?;
        let replay = self.checkpoint_id.as_ref() == Some(&checkpoint.checkpoint_id);
        if replay {
            if self.checkpoint_digest.as_ref() != Some(&digest(&bytes)) {
                return Err(refused("transparent_pir_checkpoint_conflict"));
            }
            if !self.projection_invalidated {
                return Ok(Vec::new());
            }
        }
        if !hash(&checkpoint.checkpoint_id)
            || (!replay && checkpoint.previous_checkpoint_id != self.checkpoint_id)
            || checkpoint.covered_through < self.covered_through
            || checkpoint.covered_through > scope.anchor.height
            || checkpoint.records.len() > 65_536
            || (checkpoint.complete
                && (checkpoint.covered_through != scope.anchor.height
                    || checkpoint.pending_pages != 0
                    || checkpoint.unresolved_spends != 0))
        {
            return Err(refused("transparent_pir_checkpoint_invalid"));
        }
        if checkpoint.complete
            && scope.scripts.iter().any(|s| {
                !checkpoint.coverage.iter().any(|c| {
                    c.script == s.script
                        && c.required_from <= s.required_from
                        && c.through >= scope.anchor.height
                        && hash(&c.revision)
                })
            })
        {
            return Err(refused("transparent_pir_coverage_incomplete"));
        }
        let mut decoded = Vec::new();
        let mut identities = BTreeMap::new();
        let mut received_totals = BTreeMap::<[u8; 32], u64>::new();
        let mut below_floor_outputs = BTreeSet::<([u8; 32], u32)>::new();
        let mut in_scope_outputs = BTreeSet::<([u8; 32], u32)>::new();
        for record in &checkpoint.records {
            if !hash(&record.revision) {
                return Err(refused("transparent_pir_revision_invalid"));
            }
            let entry = scope
                .scripts
                .iter()
                .find(|s| s.script == record.script)
                .ok_or_else(|| refused("transparent_pir_script_not_owned"))?;
            let script = crate::web::from_hex(&record.script)
                .map_err(|_| refused("transparent_pir_record_invalid"))?;
            let bytes = crate::web::from_hex(&record.event)
                .map_err(|_| refused("transparent_pir_record_invalid"))?;
            let event = TransparentEvent::from_bytes(&bytes)
                .map_err(|_| refused("transparent_pir_record_invalid"))?;
            let height = u64::from(event_height(event));
            if event.to_bytes() != bytes || height > scope.anchor.height {
                return Err(refused("transparent_pir_record_invalid"));
            }
            // The first publication shard overlaps the birthday estimate. Keep
            // that older event out of the wallet scope instead of refusing the
            // checkpoint. A spend of it is outside the scope too, unless this
            // same checkpoint also has the receive inside the scope.
            if height < entry.required_from {
                if let TransparentEvent::Receive(receive) = event {
                    below_floor_outputs.insert((receive.txid.0, receive.output_index));
                }
                continue;
            }
            if let TransparentEvent::Receive(receive) = event {
                in_scope_outputs.insert((receive.txid.0, receive.output_index));
                let total = received_totals.entry(receive.txid.0).or_default();
                *total = total
                    .checked_add(receive.value)
                    .ok_or_else(|| refused("transparent_pir_amount_invalid"))?;
                zcash_protocol::value::Zatoshis::from_u64(*total)
                    .map_err(|_| refused("transparent_pir_amount_invalid"))?;
            }
            let key = event_key(event);
            let fact = (event_height(event), digest(&bytes));
            if self.records.get(&key).is_some_and(|old| *old != fact)
                || identities.insert(key, fact).is_some()
            {
                return Err(refused("transparent_pir_record_conflict"));
            }
            decoded.push((script, event));
        }
        decoded.retain(|(_, event)| {
            let TransparentEvent::Spend(spend) = event else {
                return true;
            };
            let outpoint = (spend.spent_txid.0, spend.spent_output_index);
            !(below_floor_outputs.contains(&outpoint) && !in_scope_outputs.contains(&outpoint))
        });
        decoded.sort_by_key(|(_, e)| match e {
            TransparentEvent::Receive(e) => (e.height, e.transaction_index, 0, e.output_index),
            TransparentEvent::Spend(e) => (e.height, e.transaction_index, 1, e.input_index),
        });
        Ok(decoded)
    }

    pub fn prepare_rewind(&mut self, height: u32) {
        self.records.retain(|_, (h, _)| *h <= height);
        self.covered_through = self.covered_through.min(u64::from(height));
        self.complete = false;
    }

    pub fn stage(
        &mut self,
        checkpoint: &TransparentPirCheckpoint,
        records: &[(Vec<u8>, TransparentEvent)],
    ) -> Result<()> {
        for (_, event) in records {
            self.records.insert(
                event_key(*event),
                (event_height(*event), digest(&event.to_bytes())),
            );
        }
        self.rewind_floor = None;
        self.checkpoint_id = Some(checkpoint.checkpoint_id.clone());
        self.checkpoint_digest = Some(digest(&checkpoint_payload(checkpoint)?));
        let mut saved = checkpoint.clone();
        saved.records.clear();
        self.continuation = Some(saved);
        self.covered_through = checkpoint.covered_through;
        self.complete = checkpoint.complete;
        self.pending_pages = checkpoint.pending_pages;
        self.unresolved_spends = checkpoint.unresolved_spends;
        self.durable = false;
        Ok(())
    }

    pub fn acknowledge(&mut self, id: &str) -> Result<()> {
        if self.checkpoint_id.as_deref() != Some(id) {
            return Err(refused("transparent_pir_checkpoint_changed"));
        }
        self.durable = true;
        Ok(())
    }

    pub fn invalidate_projection(&mut self) {
        if self.bound_source.is_none() && self.scope.is_none() {
            return;
        }
        // The scanner erased canonical rows, not acquisition progress. Keep the
        // continuation and its owed cover queries; replay its records atomically.
        self.projection_invalidated = true;
        self.unverified_outputs = Some(BTreeSet::new());
        self.complete = false;
        self.durable = false;
        self.projected_outputs.clear();
        self.projected_spends.clear();
        self.projected_transactions.clear();
    }

    pub fn rewind(&mut self, height: u32) {
        if self.bound_source.is_none() && self.scope.is_none() {
            return;
        }
        self.rewind_floor = Some(self.rewind_floor.map_or(height, |old| old.min(height)));
        self.projection_invalidated = true;
        self.complete = false;
        self.covered_through = self.covered_through.min(u64::from(height));
        // Canonical rows have their own rewind authority. Acquisition must
        // retain its source evidence and owed cover queries for core revalidation.
        self.durable = false;
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({ "trust":"publisher-provided-confirmed-history", "spendingEnabled":false,
            "completion":if self.complete && !self.projection_invalidated && self.durable { "complete-for-enrolled-scope" } else if self.complete && !self.projection_invalidated { "checkpoint-pending" } else { "incomplete" },
            "scope": self.scope.as_ref().map(|s|serde_json::json!({"sourceId":s.source_id,"scopeId":s.scope_id,"generation":s.generation,"anchor":s.anchor,"scriptCount":s.scripts.len()})), "checkpointId":self.checkpoint_id, "coveredThrough":self.covered_through,
            "pendingPages":self.pending_pages, "unresolvedSpends":self.unresolved_spends })
    }
}

pub(crate) fn event_height(event: TransparentEvent) -> u32 {
    match event {
        TransparentEvent::Receive(e) => e.height,
        TransparentEvent::Spend(e) => e.height,
    }
}
pub(crate) fn event_key(event: TransparentEvent) -> String {
    match event {
        TransparentEvent::Receive(e) => {
            format!("r:{}:{}", crate::web::to_hex(&e.txid.0), e.output_index)
        }
        TransparentEvent::Spend(e) => format!(
            "s:{}:{}:{}:{}",
            crate::web::to_hex(&e.spending_txid.0),
            e.input_index,
            crate::web::to_hex(&e.spent_txid.0),
            e.spent_output_index
        ),
    }
}

fn checkpoint_payload(checkpoint: &TransparentPirCheckpoint) -> Result<Vec<u8>> {
    let mut normalized = checkpoint.clone();
    normalized.previous_checkpoint_id = None;
    serde_json::to_vec(&normalized).map_err(|_| refused("transparent_pir_input_invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(count: usize) -> Vec<TransparentPirScript> {
        (0..count)
            .map(|index| TransparentPirScript {
                script: format!("76a914{index:040x}88ac"),
                required_from: 0,
            })
            .collect()
    }

    #[test]
    fn scope_cap_counts_canonical_inventory_before_rejecting_duplicates() {
        let unique = inventory(2049);
        let mut duplicates = unique.clone();
        duplicates.extend(unique.clone());
        let anchor = TransparentPirAnchor {
            height: 3,
            hash: "03".repeat(32),
        };
        let scope = ProjectionState::default()
            .begin(&"ab".repeat(32), anchor.clone(), duplicates)
            .unwrap();
        let expected = ProjectionState::default()
            .begin(&"ab".repeat(32), anchor, unique)
            .unwrap();
        assert_eq!(scope.scripts.len(), 2049);
        assert_eq!(scope.scope_id, expected.scope_id);
    }

    #[test]
    fn scope_unique_cap_remains_4096_and_refusal_preserves_state() {
        let anchor = TransparentPirAnchor {
            height: 3,
            hash: "03".repeat(32),
        };
        assert_eq!(
            ProjectionState::default()
                .begin(&"ab".repeat(32), anchor.clone(), inventory(4096))
                .unwrap()
                .scripts
                .len(),
            4096
        );
        let mut state = enrolled();
        let before = serde_json::to_vec(&state).unwrap();
        assert!(state
            .begin(&"ab".repeat(32), anchor, inventory(4097))
            .is_err());
        assert!(
            serde_json::to_vec(&state).unwrap() == before,
            "scope refusal must preserve private state"
        );
    }

    #[test]
    fn scope_canonicalization_does_not_merge_distinct_history_floors() {
        let scripts = [0, 1, 1]
            .into_iter()
            .map(|required_from| TransparentPirScript {
                script: "51".into(),
                required_from,
            })
            .collect();
        let scope = ProjectionState::default()
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 3,
                    hash: "03".repeat(32),
                },
                scripts,
            )
            .unwrap();
        assert_eq!(scope.scripts.len(), 2);
        assert_eq!(scope.scripts[0].required_from, 0);
        assert_eq!(scope.scripts[1].required_from, 1);
    }

    fn enrolled() -> ProjectionState {
        let mut state = ProjectionState::default();
        let scope = state
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 3,
                    hash: "03".repeat(32),
                },
                vec![TransparentPirScript {
                    script: "51".into(),
                    required_from: 0,
                }],
            )
            .unwrap();
        let checkpoint = TransparentPirCheckpoint {
            source_id: scope.source_id,
            scope_id: scope.scope_id,
            generation: scope.generation,
            anchor: scope.anchor,
            checkpoint_id: "cd".repeat(32),
            previous_checkpoint_id: None,
            covered_through: 3,
            complete: false,
            pending_pages: 1,
            unresolved_spends: 0,
            records: vec![],
            coverage: vec![],
            rewind_to: None,
            continuation: "opaque-private-journal-and-cover-debt".into(),
        };
        state.stage(&checkpoint, &[]).unwrap();
        state
    }
    #[test]
    fn same_height_scope_change_preserves_private_checkpoint_for_core_revalidation() {
        let mut state = enrolled();
        let saved = state.continuation.clone().unwrap();
        let scope = state
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 3,
                    hash: "83".repeat(32),
                },
                state.scope.as_ref().unwrap().scripts.clone(),
            )
            .unwrap();
        assert_eq!(scope.anchor.hash, "83".repeat(32));
        assert!(
            serde_json::to_vec(&state.continuation).unwrap()
                == serde_json::to_vec(&Some(saved)).unwrap(),
            "fork enrollment must preserve private evidence and debt"
        );
        assert!(!state.complete);
    }
    #[test]
    fn canonical_rewind_preserves_private_journal_and_refuses_lower_enrollment() {
        let mut state = enrolled();
        let saved = serde_json::to_vec(&state.continuation).unwrap();
        state.rewind(1);
        assert!(
            serde_json::to_vec(&state.continuation).unwrap() == saved,
            "canonical rewind must retain private cover debt"
        );
        assert_eq!(state.rewind_floor, Some(1));
        assert!(state
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 1,
                    hash: "01".repeat(32)
                },
                state.scope.as_ref().unwrap().scripts.clone()
            )
            .is_err());
        assert!(
            serde_json::to_vec(&state.continuation).unwrap() == saved,
            "closed lower target must preserve checkpoint"
        );
    }
    #[test]
    fn expanded_scope_frontier_is_incomplete_without_erasing_acquisition() {
        let mut state = enrolled();
        let saved = serde_json::to_vec(&state.continuation).unwrap();
        let mut scripts = state.scope.as_ref().unwrap().scripts.clone();
        scripts.push(TransparentPirScript {
            script: "52".into(),
            required_from: 0,
        });
        let scope = state
            .begin(
                &"ab".repeat(32),
                TransparentPirAnchor {
                    height: 3,
                    hash: "03".repeat(32),
                },
                scripts,
            )
            .unwrap();
        assert_eq!(state.covered_through, 0);
        assert!(
            serde_json::to_vec(&state.continuation).unwrap() == saved,
            "scope growth must preserve acquisition and cover debt"
        );
        let partial = TransparentPirCheckpoint {
            source_id: scope.source_id,
            scope_id: scope.scope_id,
            generation: scope.generation,
            anchor: scope.anchor,
            checkpoint_id: "de".repeat(32),
            previous_checkpoint_id: state.checkpoint_id.clone(),
            covered_through: 0,
            complete: false,
            pending_pages: 1,
            unresolved_spends: 0,
            records: vec![],
            coverage: vec![],
            rewind_to: None,
            continuation: "continued-private-journal-and-cover-debt".into(),
        };
        state.validate(&partial).unwrap();
    }

    #[test]
    fn never_enrolled_rewind_preserves_exact_serialized_state() {
        let mut state = ProjectionState::default();
        let saved = serde_json::to_vec(&state).unwrap();
        state.rewind(1);
        assert_eq!(serde_json::to_vec(&state).unwrap(), saved);
        assert!(state.rewind_floor.is_none());
    }
}
