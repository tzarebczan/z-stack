//! Local evidence for history labels. This module never requests payloads or changes routing.
use super::*;
use crate::MemoStatus;
use std::collections::{BTreeMap, BTreeSet};
use zcash_client_backend::data_api::enhance_pir::{EnhancePirSuspension, EnhancePirWork};

pub(super) fn work(
    conn: &rusqlite::Connection,
    network: ZNetwork,
) -> Option<BTreeMap<TxId, MemoStatus>> {
    // Borrow the existing query-only connection: history must not migrate a store to label it.
    let db = WalletDb::from_connection(conn, network, SystemClock, new_rng())
        .with_enhancement_mode(EnhancementMode::PrivateIronwood);
    let Ok(work) = db.transaction_enhancement_work() else {
        return None;
    };
    let mut pending = BTreeSet::new();
    let mut unavailable = BTreeSet::new();
    let mut discovery = BTreeSet::new();
    for item in work {
        match item {
            TransactionEnhancementWork::Public(request) => {
                pending.insert(request.txid());
            }
            TransactionEnhancementWork::Private(EnhancePirWork::Query(request)) => {
                pending.insert(request.request_id().txid());
            }
            TransactionEnhancementWork::Private(EnhancePirWork::Rediscover(request)) => {
                discovery.insert(u32::from(request.height));
            }
            TransactionEnhancementWork::Private(EnhancePirWork::Suspended(
                EnhancePirSuspension::Discovery(failure),
            )) => {
                pending.insert(failure.txid);
            }
            TransactionEnhancementWork::Private(EnhancePirWork::Suspended(
                EnhancePirSuspension::OutgoingNotRecoverable(request),
            )) => {
                unavailable.insert(request.request_id().txid());
            }
        }
    }
    // The typed API intentionally identifies rediscovery by block. Resolve only the durable
    // private discovery jobs in those blocks, without exposing their identities to a host.
    if !discovery.is_empty() {
        let mut statement = conn.prepare("SELECT t.txid FROM ironwood_enhance_discovery_queue q JOIN transactions t ON t.id_tx=q.transaction_id JOIN ironwood_enhance_routing r ON r.transaction_id=t.id_tx WHERE t.mined_height=?1 AND r.route=0").ok()?;
        for height in discovery {
            let rows = statement
                .query_map([height], |row| row.get::<_, [u8; 32]>(0))
                .ok()?;
            for id in rows {
                pending.insert(TxId::from_bytes(id.ok()?));
            }
        }
    }
    let mut statuses: BTreeMap<_, _> = unavailable
        .into_iter()
        .map(|id| (id, MemoStatus::Unavailable))
        .collect();
    statuses.extend(pending.into_iter().map(|id| (id, MemoStatus::Pending)));
    Some(statuses)
}
