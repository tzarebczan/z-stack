// Adapted from valargroup/wallet-pir c9262e8431b4caa7cf2e2c55edbf0084c1f2ddb2.
// Copyright (c) 2025-2026 Roman Akhtariev. MIT; see LICENSE-MIT and UPSTREAM.md.
//! What the embedding wallet supplies besides storage.
//!
//! The library does not know the wallet's derivation rules, gap limit or
//! imported keys, and it does not decide which chain is the right one. Both
//! are handed in through these traits, so the ledger this library builds
//! follows the wallet's own policies rather than inventing parallel ones.

use crate::store::{Anchor, ScriptEntry};

/// Whether a block the wallet's coverage rests on is still on its chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceptance {
    Accepted,
    Rejected,
    /// The wallet cannot say: it has no view of this height yet.
    Unknown,
}

/// The wallet's view of the chain. Independent of the shard service by
/// construction, which is what lets a reorg be detected rather than accepted.
pub trait ChainView {
    fn is_accepted(&self, height: u64, hash_display_hex: &str) -> Acceptance;
    fn tip(&self) -> Option<Anchor>;
    /// An independently accepted hash, needed for rollback inside a shard.
    fn hash_at(&self, height: u64) -> Option<String> {
        self.tip().filter(|a| a.height == height).map(|a| a.hash)
    }
}

/// The wallet's script set and its rule for extending it.
pub trait ScriptProvider {
    /// Every script the wallet is currently responsible for, with the height
    /// its coverage must begin at.
    fn scripts(&mut self) -> Vec<ScriptEntry>;
    /// Called with the scripts that showed activity in a sync. Returns any
    /// scripts the wallet's own rules now add — a gap-limit advance — each
    /// with its own required height. Those receive historical discovery
    /// over their whole required range before the sync completes.
    fn on_activity(&mut self, active: &[Vec<u8>]) -> Vec<ScriptEntry>;
}

/// A fixed script set that never advances.
pub struct StaticScripts(pub Vec<ScriptEntry>);

impl ScriptProvider for StaticScripts {
    fn scripts(&mut self) -> Vec<ScriptEntry> {
        self.0.clone()
    }
    fn on_activity(&mut self, _: &[Vec<u8>]) -> Vec<ScriptEntry> {
        Vec::new()
    }
}
