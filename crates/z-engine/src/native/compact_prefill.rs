//! Bounded compact data derived from transaction effects accepted from a local
//! Mainnet node. No consensus/PoW, ZIP244 authorizing-data, wallet or publication
//! qualification is implied. There is no I/O or checkpoint deserialization.
use super::{
    accepted_blocks::verify_mainnet_block_window, chain_identity, AcceptedMainnetWindow,
    VerifiedMainnetBlockWindow,
};
use crate::{error::Result, EngineError, Network};
use incrementalmerkletree::{frontier::Frontier, Hashable};
use prost::Message;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use zcash_client_backend::proto::compact_formats::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend,
    CompactTx, CompactTxIn, TxOut,
};
use zcash_primitives::{block::Block, transaction::Transaction};

/// V1 includes all transaction indices, Sapling/Orchard/Ironwood fields, and
/// transparent inputs/outputs. Coinbase null inputs are omitted; fees and headers
/// are unset. Sprout has no representation in the pinned compact schema.
pub const MAINNET_COMPACT_WINDOW_POLICY: &str = "z-stack-mainnet-accepted-compact-window-v1";
pub const MAINNET_COMPACT_BLOCK_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const MAINNET_COMPACT_WINDOW_MAX_BYTES: usize = 16 * (MAINNET_COMPACT_BLOCK_MAX_BYTES + 5);

type SaplingFrontier = Frontier<sapling::Node, 32>;
type OrchardFrontier = Frontier<orchard::tree::MerkleHashOrchard, 32>;

fn invalid() -> EngineError {
    EngineError::Message("native_compact_window_invalid".into())
}
fn current(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(EngineError::Message(
            "native_compact_window_cancelled".into(),
        ))
    } else {
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opaque prefix state. Construction starts at canonical Mainnet genesis, or
/// comes from a successful compact window. Clone preserves that provenance;
/// arbitrary sizes/frontiers and persisted checkpoints cannot be imported.
#[derive(Clone)]
pub struct MainnetCompactCheckpoint {
    height: u32,
    hash: String,
    sapling: SaplingFrontier,
    orchard: OrchardFrontier,
    ironwood: OrchardFrontier,
}
impl MainnetCompactCheckpoint {
    /// This is empty shielded prefix state, not a compact genesis message.
    pub fn canonical_genesis() -> Self {
        Self {
            height: 0,
            hash: chain_identity::genesis(Network::Mainnet).into(),
            sapling: Frontier::empty(),
            orchard: Frontier::empty(),
            ironwood: Frontier::empty(),
        }
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Canonical display-order block hash.
    pub fn block_hash(&self) -> &str {
        &self.hash
    }
    pub fn tree_sizes(&self) -> [u64; 3] {
        [
            self.sapling.tree_size(),
            self.orchard.tree_size(),
            self.ironwood.tree_size(),
        ]
    }
    pub fn tree_roots(&self) -> [[u8; 32]; 3] {
        [
            self.sapling.root().to_bytes(),
            self.orchard.root().to_bytes(),
            self.ironwood.root().to_bytes(),
        ]
    }
    /// Public state digest, not independent chain or storage authentication.
    pub fn identity(&self) -> String {
        let mut h = Sha256::new();
        h.update(MAINNET_COMPACT_WINDOW_POLICY.as_bytes());
        h.update([0]);
        h.update(chain_identity::genesis(Network::Mainnet).as_bytes());
        h.update([0]);
        h.update(self.height.to_le_bytes());
        h.update(self.hash.as_bytes());
        for (size, root) in self.tree_sizes().into_iter().zip(self.tree_roots()) {
            h.update(size.to_le_bytes());
            h.update(root);
        }
        hex(&h.finalize())
    }
}

/// Complete length-delimited CompactBlock payload and its successor state.
/// No full transactions, partial windows or mutable checkpoint fields escape.
pub struct VerifiedMainnetCompactWindow {
    evidence: VerifiedMainnetBlockWindow,
    predecessor_identity: String,
    payload: Vec<u8>,
    payload_sha256: String,
    successor: MainnetCompactCheckpoint,
}
impl VerifiedMainnetCompactWindow {
    pub fn block_window(&self) -> &VerifiedMainnetBlockWindow {
        &self.evidence
    }
    pub fn derivation_policy(&self) -> &'static str {
        MAINNET_COMPACT_WINDOW_POLICY
    }
    pub fn predecessor_identity(&self) -> &str {
        &self.predecessor_identity
    }
    pub fn encoded_blocks(&self) -> &[u8] {
        &self.payload
    }
    pub fn payload_sha256(&self) -> &str {
        &self.payload_sha256
    }
    pub fn successor_checkpoint(&self) -> &MainnetCompactCheckpoint {
        &self.successor
    }
}

fn checked_append<H: Hashable + Clone>(frontier: &mut Frontier<H, 32>, leaf: H) -> Result<()> {
    u32::try_from(frontier.tree_size())
        .map_err(|_| invalid())?
        .checked_add(1)
        .ok_or_else(invalid)?;
    if !frontier.append(leaf) {
        return Err(invalid());
    }
    Ok(())
}
fn metadata(state: &MainnetCompactCheckpoint) -> Result<ChainMetadata> {
    let [s, o, i] = state.tree_sizes();
    Ok(ChainMetadata {
        sapling_commitment_tree_size: s.try_into().map_err(|_| invalid())?,
        orchard_commitment_tree_size: o.try_into().map_err(|_| invalid())?,
        ironwood_commitment_tree_size: i.try_into().map_err(|_| invalid())?,
    })
}
fn compact_transaction(
    tx: &Transaction,
    index: usize,
    state: &mut MainnetCompactCheckpoint,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<CompactTx> {
    check()?;
    let mut compact = CompactTx {
        index: index.try_into().map_err(|_| invalid())?,
        txid: tx.txid().as_ref().to_vec(),
        ..Default::default()
    };
    if let Some(bundle) = tx.transparent_bundle() {
        if !bundle.is_coinbase() {
            for input in &bundle.vin {
                check()?;
                compact.vin.push(CompactTxIn {
                    prevout_txid: input.prevout().hash().to_vec(),
                    prevout_index: input.prevout().n(),
                });
            }
        }
        for output in &bundle.vout {
            check()?;
            compact.vout.push(TxOut {
                value: output.value().into(),
                script_pub_key: output.script_pubkey().0 .0.clone(),
            });
        }
    }
    if let Some(bundle) = tx.sapling_bundle() {
        for spend in bundle.shielded_spends() {
            check()?;
            compact.spends.push(CompactSaplingSpend::from(spend));
        }
        for output in bundle.shielded_outputs() {
            check()?;
            checked_append(&mut state.sapling, sapling::Node::from_cmu(output.cmu()))?;
            compact.outputs.push(CompactSaplingOutput::from(output));
        }
    }
    for (frontier, target, bundle) in [
        (
            &mut state.orchard,
            &mut compact.actions,
            tx.orchard_bundle(),
        ),
        (
            &mut state.ironwood,
            &mut compact.ironwood_actions,
            tx.ironwood_bundle(),
        ),
    ] {
        if let Some(bundle) = bundle {
            for action in bundle.actions() {
                check()?;
                checked_append(
                    frontier,
                    orchard::tree::MerkleHashOrchard::from_cmx(action.cmx()),
                )?;
                target.push(CompactOrchardAction::from(action));
            }
        }
    }
    check()?;
    Ok(compact)
}

#[derive(Clone, Copy)]
struct Limits {
    block: usize,
    window: usize,
}
const LIMITS: Limits = Limits {
    block: MAINNET_COMPACT_BLOCK_MAX_BYTES,
    window: MAINNET_COMPACT_WINDOW_MAX_BYTES,
};

/// Verify the complete immutable full-block candidate first, then derive one
/// bounded window using cloned prefix frontiers. The predecessor is unchanged on
/// any refusal or cancellation. Exactly sixteen aligned blocks are required;
/// genesis serving, partial tails, checkpoint restore and disk publication are
/// separate contracts. Accepted local-node trust remains explicit.
pub fn derive_mainnet_compact_window(
    candidate: &[u8],
    accepted: &AcceptedMainnetWindow,
    predecessor: &MainnetCompactCheckpoint,
    cancel: &AtomicBool,
) -> Result<VerifiedMainnetCompactWindow> {
    derive(
        candidate,
        accepted,
        predecessor,
        cancel,
        LIMITS,
        &mut || current(cancel),
    )
}
fn derive(
    candidate: &[u8],
    accepted: &AcceptedMainnetWindow,
    predecessor: &MainnetCompactCheckpoint,
    cancel: &AtomicBool,
    limits: Limits,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<VerifiedMainnetCompactWindow> {
    check()?;
    let evidence = verify_mainnet_block_window(candidate, accepted, cancel)?;
    if predecessor.height.checked_add(1) != Some(evidence.start_height())
        || predecessor.hash != evidence.predecessor_hash()
    {
        return Err(invalid());
    }
    metadata(predecessor)?;
    let predecessor_identity = predecessor.identity();
    let mut state = predecessor.clone();
    // The verifier proved exact framing and canonical parsing of these same
    // immutable bytes. Keep its parser and security checks unchanged.
    let mut bytes = &candidate[super::MAINNET_BLOCK_WINDOW_MAGIC.len() + 8..];
    let mut payload = Vec::new();
    for _ in 0..super::MAINNET_BLOCK_WINDOW_WIDTH {
        check()?;
        let length = u32::from_le_bytes(bytes[..4].try_into().map_err(|_| invalid())?) as usize;
        let mut raw = &bytes[4..4 + length];
        let block = Block::read(&mut raw, &Network::Mainnet).map_err(|_| invalid())?;
        bytes = &bytes[4 + length..];
        let mut transactions = Vec::with_capacity(block.vtx().len());
        for (index, tx) in block.vtx().iter().enumerate() {
            transactions.push(compact_transaction(tx, index, &mut state, check)?);
        }
        let compact = CompactBlock {
            height: u32::from(block.claimed_height()).into(),
            hash: block.header().hash().0.to_vec(),
            prev_hash: block.header().prev_block.0.to_vec(),
            time: block.header().time,
            header: vec![],
            vtx: transactions,
            chain_metadata: Some(metadata(&state)?),
        };
        let encoded_len = compact.encoded_len();
        // A uint32-sized protobuf message requires at most five length bytes.
        if encoded_len > limits.block
            || payload
                .len()
                .checked_add(encoded_len)
                .and_then(|n| n.checked_add(5))
                .is_none_or(|n| n > limits.window)
        {
            return Err(invalid());
        }
        check()?;
        compact
            .encode_length_delimited(&mut payload)
            .map_err(|_| invalid())?;
    }
    state.height = evidence.end_height();
    state.hash = evidence.target_hash().into();
    let mut digest = Sha256::new();
    for chunk in payload.chunks(64 * 1024) {
        check()?;
        digest.update(chunk);
    }
    check()?;
    Ok(VerifiedMainnetCompactWindow {
        evidence,
        predecessor_identity,
        payload,
        payload_sha256: hex(&digest.finalize()),
        successor: state,
    })
}

#[cfg(test)]
mod tests;
