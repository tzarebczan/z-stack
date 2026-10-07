//! Shared compact-block scan tuning (WASM snapshot + native sqlite).
//!
//! Keep `@z-stack/core` `syncTuning` / `MAX_SYNC_BLOCKS` in lockstep with these constants.

use zcash_client_backend::proto::compact_formats::{ChainMetadata, CompactBlock};

/// Local Zaino / loopback batch (native `sync` + WASM `/lwd/blocks`).
pub const BATCH_LOCAL: u32 = 4_000;
/// Native `GetBlockRange` chunk. Two thousand heights amortize RPC overhead
/// without making the first apply wait for a full 4,000-height scan batch.
pub const FETCH_CHUNK_LOCAL: u32 = 2_000;
/// Historic sqlite persist width. Decrypt stays on bounded fetch chunks;
/// empty compact rows are skipped and this many heights share one txn + watermark.
pub const HISTORIC_PERSIST_BLOCKS: u32 = 8_000;
/// Start Rayon decrypt once this many compact blocks have arrived on a
/// GetBlockRange stream, instead of waiting for the full RPC body.
pub const STREAM_DECRYPT_BLOCKS: u32 = 256;
/// Public LWD batch (servers often cap GetBlockRange).
pub const BATCH_PUBLIC: u32 = 1_000;
/// How many batches to fetch ahead of the scanner on local Zaino.
/// WASM **gRPC-Web** uses 1000-block ranges with prefetch 8 (`grpcWebPrefetch` in `@z-stack/core`):
/// each unary GetBlockRange must finish inside Zaino's ~120s deadline; parallelism is extra HTTP fetches,
/// not a bigger range. Native local historic uses 2000-block GetBlockRange with
/// `PREFETCH_NATIVE_LOCAL` on two reused HTTP/2 channels (apply each chunk as it
/// arrives; 4000 is scan coalescing, not the first-apply wait). WASM `/lwd`
/// pipe is 4×2000 (`pipePrefetch` in `@z-stack/core`) with a 2× apply buffer so
/// HTTP does not wait on WASM.
pub const PREFETCH_LOCAL: u32 = 4;
/// Desktop/CLI local Zaino: in-flight GetBlockRange while Rayon decrypts k and
/// sqlite persists k-1. Six 2k ranges retain the ~12k-block lead of the former
/// twelve 1k ranges without doubling buffered bytes or node request pressure.
/// Do not copy onto WASM gRPC-Web.
pub const PREFETCH_NATIVE_LOCAL: u32 = 6;
pub const PREFETCH_PUBLIC: u32 = 2;
/// WASM IndexedDB persist every N batches (native sqlite writes per `put_blocks`).
pub const PERSIST_EVERY_LOCAL: u32 = 16;
pub const PERSIST_EVERY_PUBLIC: u32 = 8;
/// Max unscanned birthday→tip gap (~130 days at 75s/block).
pub const MAX_SYNC_BLOCKS: u32 = 150_000;
/// Empty-block tree checkpoints (blocks with commitments always checkpoint).
pub const CHECKPOINT_EVERY: u32 = 32;
/// WASM reorg hash window (native sqlite keeps its own).
pub const HASH_KEEP: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTuning {
    pub batch: u32,
    pub prefetch: u32,
    pub persist_every: u32,
}

/// Same numbers WASM (`createWasmClient`) and native desktop/CLI use.
pub fn sync_tuning(local_light: bool) -> SyncTuning {
    if local_light {
        SyncTuning {
            batch: BATCH_LOCAL,
            prefetch: PREFETCH_LOCAL,
            persist_every: PERSIST_EVERY_LOCAL,
        }
    } else {
        SyncTuning {
            batch: BATCH_PUBLIC,
            prefetch: PREFETCH_PUBLIC,
            persist_every: PERSIST_EVERY_PUBLIC,
        }
    }
}

/// True when the compact block has sapling/orchard/ironwood content to trial-decrypt.
/// Coinbase-only / empty shielded blocks skip `scan_block` on WASM and native selective shard scanning.
pub fn compact_has_shielded(block: &CompactBlock) -> bool {
    block.vtx.iter().any(|tx| {
        !tx.spends.is_empty()
            || !tx.outputs.is_empty()
            || !tx.actions.is_empty()
            || !tx.ironwood_actions.is_empty()
    })
}

/// Sapling / Orchard / Ironwood commitment counts (tree growth, not spends).
pub fn compact_commitment_counts(block: &CompactBlock) -> (u32, u32, u32) {
    let mut sapling = 0u32;
    let mut orchard = 0u32;
    let mut ironwood = 0u32;
    for tx in &block.vtx {
        sapling = sapling.saturating_add(u32::try_from(tx.outputs.len()).unwrap_or(u32::MAX));
        orchard = orchard.saturating_add(u32::try_from(tx.actions.len()).unwrap_or(u32::MAX));
        ironwood =
            ironwood.saturating_add(u32::try_from(tx.ironwood_actions.len()).unwrap_or(u32::MAX));
    }
    (sapling, orchard, ironwood)
}

/// Fill `chain_metadata` on every height so persist can hand selective shard scanning a sequential
/// span. Zaino sometimes omits it (which forced a single-thread `scan_block`) or
/// leaves an unused pool at protobuf default `0`. A `0` must not clobber the
/// running size — `put_blocks` treats `from_state.tree_size + commitments ==
/// final_tree_size` as sequentiality, and a default-0 pool fails that check.
pub fn ensure_compact_chain_metadata(
    blocks: &mut [CompactBlock],
    sapling0: u32,
    orchard0: u32,
    ironwood0: u32,
) {
    let mut sapling = sapling0;
    let mut orchard = orchard0;
    let mut ironwood = ironwood0;
    for block in blocks {
        let (ds, dorch, di) = compact_commitment_counts(block);
        sapling = sapling.saturating_add(ds);
        orchard = orchard.saturating_add(dorch);
        ironwood = ironwood.saturating_add(di);
        if let Some(m) = &block.chain_metadata {
            if m.sapling_commitment_tree_size > 0 {
                sapling = m.sapling_commitment_tree_size;
            }
            if m.orchard_commitment_tree_size > 0 {
                orchard = m.orchard_commitment_tree_size;
            }
            if m.ironwood_commitment_tree_size > 0 {
                ironwood = m.ironwood_commitment_tree_size;
            }
        }
        block.chain_metadata = Some(ChainMetadata {
            sapling_commitment_tree_size: sapling,
            orchard_commitment_tree_size: orchard,
            ironwood_commitment_tree_size: ironwood,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_client_backend::proto::compact_formats::{CompactOrchardAction, CompactTx};

    #[test]
    fn empty_compact_is_not_shielded() {
        let b = CompactBlock::default();
        assert!(!compact_has_shielded(&b));
        let mut with_coinbase = CompactBlock::default();
        with_coinbase.vtx.push(CompactTx::default());
        assert!(!compact_has_shielded(&with_coinbase));
    }

    #[test]
    fn orchard_action_is_shielded() {
        let mut tx = CompactTx::default();
        tx.actions.push(CompactOrchardAction::default());
        let mut b = CompactBlock::default();
        b.vtx.push(tx);
        assert!(compact_has_shielded(&b));
        assert_eq!(compact_commitment_counts(&b), (0, 1, 0));
    }

    #[test]
    fn fills_missing_chain_metadata_from_tree_sizes() {
        let mut tx = CompactTx::default();
        tx.actions.push(CompactOrchardAction::default());
        let mut b = CompactBlock::default();
        b.height = 10;
        b.vtx.push(tx);
        let mut blocks = vec![b];
        ensure_compact_chain_metadata(&mut blocks, 3, 9, 0);
        let m = blocks[0].chain_metadata.expect("filled");
        assert_eq!(m.sapling_commitment_tree_size, 3);
        assert_eq!(m.orchard_commitment_tree_size, 10);
        assert_eq!(m.ironwood_commitment_tree_size, 0);
    }

    #[test]
    fn default_zero_pool_does_not_clobber_running_tree_size() {
        let mut tx = CompactTx::default();
        tx.actions.push(CompactOrchardAction::default());
        let mut b = CompactBlock::default();
        b.height = 10;
        b.vtx.push(tx);
        b.chain_metadata = Some(ChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 10,
            ironwood_commitment_tree_size: 0,
        });
        let mut blocks = vec![b];
        ensure_compact_chain_metadata(&mut blocks, 3_000, 9, 0);
        let m = blocks[0].chain_metadata.expect("merged");
        assert_eq!(
            m.sapling_commitment_tree_size, 3_000,
            "protobuf default 0 must not wipe the running sapling size"
        );
        assert_eq!(m.orchard_commitment_tree_size, 10);
        assert_eq!(m.ironwood_commitment_tree_size, 0);
    }

    #[test]
    fn local_tuning_is_the_fast_path() {
        let t = sync_tuning(true);
        assert_eq!(t.batch, BATCH_LOCAL);
        assert_eq!(t.prefetch, PREFETCH_LOCAL);
        assert!(PREFETCH_NATIVE_LOCAL > PREFETCH_LOCAL);
        assert_eq!(HISTORIC_PERSIST_BLOCKS, 8_000);
        assert!(HISTORIC_PERSIST_BLOCKS >= BATCH_LOCAL);
        let p = sync_tuning(false);
        assert_eq!(p.batch, BATCH_PUBLIC);
        assert!(t.batch > p.batch);
    }
}
