//! Fold per-tx aggregates into `HistoryEntry` rows (same columns as `v_transactions`).

use super::store::{TxAgg, WebWallet};
use crate::{HistoryEntry, MemoStatus};
use std::collections::BTreeMap;

pub fn entries(
    aggs: &BTreeMap<String, TxAgg>,
    limit: usize,
    scanned_height: u32,
    status: Option<crate::HistoryStatusFilter>,
    txid: Option<&str>,
) -> Vec<HistoryEntry> {
    let mut rows: Vec<HistoryEntry> = aggs
        .iter()
        .map(|(txid, agg)| {
            // `received_zat` omits change, while per-pool receipts include it.
            // A wallet's balance delta must include change or a fee-only
            // self-transfer appears to lose its entire input note.
            let pool_received = agg
                .transparent_received
                .saturating_add(agg.sapling_received)
                .saturating_add(agg.orchard_received)
                .saturating_add(agg.ironwood_received);
            let pool_spent = agg
                .transparent_spent
                .saturating_add(agg.sapling_spent)
                .saturating_add(agg.orchard_spent)
                .saturating_add(agg.ironwood_spent);
            let account_delta_zat = if pool_received > 0 || pool_spent > 0 {
                pool_received as i64 - pool_spent as i64
            } else {
                agg.received_zat as i64 - agg.spent_zat as i64
            };
            HistoryEntry {
                txid: WebWallet::display_txid(txid),
                mined_height: agg.mined_height,
                expiry_height: agg.expiry_height,
                account_delta_zat,
                spent_zat: agg.spent_zat,
                received_zat: agg.received_zat,
                fee_zat: agg.fee_zat,
                sent_note_count: agg.spent_notes,
                received_note_count: agg.received_notes,
                memo_count: agg.memos.len() as u32,
                has_change: agg.has_change,
                is_shielding: agg.is_shielding,
                expired_unmined: agg.mined_height.is_none()
                    && agg
                        .expiry_height
                        .is_some_and(|e| e > 0 && e <= scanned_height),
                memos: agg.memos.clone(),
                memo_status: memo_status(agg),
                block_time: agg.block_time,
                confirmations: agg
                    .mined_height
                    .map(|h| scanned_height.saturating_sub(h).saturating_add(1)),
                transparent_received: agg.transparent_received,
                transparent_spent: agg.transparent_spent,
                sapling_received: agg.sapling_received,
                sapling_spent: agg.sapling_spent,
                orchard_received: agg.orchard_received,
                orchard_spent: agg.orchard_spent,
                ironwood_received: agg.ironwood_received,
                ironwood_spent: agg.ironwood_spent,
                history_metadata_complete: agg.history_metadata_complete,
                outgoing_shielded_zat: agg.outgoing_shielded_zat,
                transparent_inputs: agg.transparent_inputs.clone(),
                transparent_outputs: agg.transparent_outputs.clone(),
            }
        })
        .collect();
    if let Some(id) = txid.map(str::trim).filter(|s| !s.is_empty()) {
        rows.retain(|e| e.txid.eq_ignore_ascii_case(id));
    }
    if let Some(filter) = status {
        rows.retain(|e| e.matches_status(filter));
    }
    rows.sort_by(|a, b| match (a.mined_height, b.mined_height) {
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (ha, hb) => hb.cmp(&ha).then_with(|| b.txid.cmp(&a.txid)),
    });
    rows.truncate(limit.min(500));
    rows
}

fn memo_status(agg: &TxAgg) -> MemoStatus {
    if !agg.memos.is_empty() {
        MemoStatus::Available
    } else if !agg.has_shielded_activity() {
        MemoStatus::NotApplicable
    } else if !agg.enhancement_complete {
        MemoStatus::Pending
    } else {
        match agg.memo_recovered {
            Some(true) => MemoStatus::Empty,
            Some(false) => MemoStatus::Unavailable,
            None => MemoStatus::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_states_distinguish_unfetched_empty_unrecoverable_and_legacy() {
        let mut agg = TxAgg::default();
        assert_eq!(memo_status(&agg), MemoStatus::NotApplicable);
        agg.received_notes = 1;
        assert_eq!(memo_status(&agg), MemoStatus::Pending);
        agg.enhancement_complete = true;
        assert_eq!(memo_status(&agg), MemoStatus::Unknown);
        agg.memo_recovered = Some(false);
        assert_eq!(memo_status(&agg), MemoStatus::Unavailable);
        agg.memo_recovered = Some(true);
        assert_eq!(memo_status(&agg), MemoStatus::Empty);
        agg.memos.push("hello".into());
        assert_eq!(memo_status(&agg), MemoStatus::Available);
        let restored: TxAgg = serde_json::from_str(&serde_json::to_string(&agg).unwrap()).unwrap();
        assert_eq!(memo_status(&restored), MemoStatus::Available);
        let row = entries(
            &BTreeMap::from([("00".repeat(32), restored)]),
            1,
            10,
            None,
            None,
        )
        .remove(0);
        assert_eq!(row.to_json()["memoStatus"], "available");
    }

    #[test]
    fn change_counts_toward_account_delta_but_not_external_receipt() {
        let mut aggs = BTreeMap::new();
        aggs.insert(
            "01".repeat(32),
            TxAgg {
                mined_height: Some(100),
                spent_zat: 1_641_929,
                received_zat: 0,
                orchard_spent: 1_641_929,
                orchard_received: 1_631_929,
                fee_zat: Some(10_000),
                history_metadata_complete: true,
                ..Default::default()
            },
        );
        let row = entries(&aggs, 1, 100, None, None).remove(0);
        assert_eq!(row.account_delta_zat, -10_000);
        assert_eq!(row.received_zat, 0);
        assert_eq!(row.fee_zat, Some(10_000));
        assert_eq!(row.to_json()["historyMetadataComplete"], true);
        assert_eq!(row.to_json()["transparentInputs"], serde_json::json!([]));
    }
}
