//! Complete bounded raw-block recovery evidence. Accepted headers are supplied
//! independently; this verifies transaction-effect/txid inclusion, not consensus
//! validity or ZIP244 authorizing-data commitments. Source transactions must
//! never replace wallet-authored pending raw bytes or be reused for broadcast.
use super::{RegtestAcceptedChain, ZNetwork};
use crate::error::{EngineError, Result};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use transparent_wallet::{Acceptance, ChainView};
use zcash_primitives::{block::Block, transaction::TxVersion};
use zcash_protocol::consensus::BranchId;

const MAGIC: &[u8] = b"COFFER-REGTEST-BLOCKS-V1\n";
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_BLOCK_BYTES: usize = 2_000_000;
const MAX_TRANSACTIONS: usize = 65_536;

/// Parsed transaction effects cannot be replaced after accepted-header checks.
/// V5 signatures and proofs are parsed but are not authenticated by this type.
/// No wallet state is modified by constructing this evidence.
pub struct VerifiedRegtestRecoveryBlocks {
    pub(super) blocks: Vec<Block>,
    target_hash: String,
}
impl VerifiedRegtestRecoveryBlocks {
    pub fn target_height(&self) -> u32 {
        self.blocks.len() as u32
    }
    pub fn target_hash(&self) -> &str {
        &self.target_hash
    }
}
fn invalid() -> EngineError {
    EngineError::Message("native_recovery_blocks_invalid".into())
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(EngineError::Message("native_recovery_cancelled".into()))
    } else {
        Ok(())
    }
}
fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    if bytes.len() < count {
        return Err(invalid());
    }
    let (head, tail) = bytes.split_at(count);
    *bytes = tail;
    Ok(head)
}
fn u32_le(bytes: &mut &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take(bytes, 4)?.try_into().map_err(|_| invalid())?,
    ))
}
// Bound the parser's allocation before Block::read reads its transaction vector.
fn compact_size(bytes: &mut &[u8]) -> Result<u64> {
    let first = take(bytes, 1)?[0];
    match first {
        0..=252 => Ok(u64::from(first)),
        253 => {
            let n = u16::from_le_bytes(take(bytes, 2)?.try_into().map_err(|_| invalid())?);
            if n < 253 {
                return Err(invalid());
            }
            Ok(u64::from(n))
        }
        254 => {
            let n = u32_le(bytes)?;
            if n <= u16::MAX.into() {
                return Err(invalid());
            }
            Ok(u64::from(n))
        }
        255 => {
            let n = u64::from_le_bytes(take(bytes, 8)?.try_into().map_err(|_| invalid())?);
            if n <= u32::MAX.into() {
                return Err(invalid());
            }
            Ok(n)
        }
    }
}
fn transaction_count(raw: &[u8]) -> Result<usize> {
    let mut bytes = raw;
    take(&mut bytes, 140)?;
    let solution_length = compact_size(&mut bytes)?;
    if solution_length > bytes.len() as u64 {
        return Err(invalid());
    }
    take(&mut bytes, solution_length as usize)?;
    let count = compact_size(&mut bytes)?;
    if count == 0 || count > MAX_TRANSACTIONS as u64 || count > bytes.len() as u64 {
        return Err(invalid());
    }
    Ok(count as usize)
}
fn transaction_root(mut nodes: Vec<[u8; 32]>) -> Result<[u8; 32]> {
    if nodes.is_empty() {
        return Err(invalid());
    }
    while nodes.len() > 1 {
        // Detect duplicated siblings before adding the legitimate odd tail.
        if nodes.chunks_exact(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid());
        }
        if nodes.len() % 2 != 0 {
            nodes.push(*nodes.last().ok_or_else(invalid)?);
        }
        nodes = nodes
            .chunks_exact(2)
            .map(|pair| {
                let mut hash = Sha256::new();
                hash.update(pair[0]);
                hash.update(pair[1]);
                Sha256::digest(hash.finalize()).into()
            })
            .collect();
    }
    Ok(nodes[0])
}

/// Verify the V1 complete prefix: magic, LE start=1/count, then LE length/raw
/// blocks. Hash provenance belongs to the accepted chain constructor's caller.
/// The current profile refuses Sprout payloads, V6 and Ironwood rather than
/// claiming downstream compact conversion supports them.
pub fn verify_regtest_recovery_blocks(
    mut bytes: &[u8],
    chain: &RegtestAcceptedChain,
    cancel: &AtomicBool,
) -> Result<VerifiedRegtestRecoveryBlocks> {
    cancelled(cancel)?;
    if bytes.len() > MAX_BYTES || take(&mut bytes, MAGIC.len())? != MAGIC {
        return Err(invalid());
    }
    let start = u32_le(&mut bytes)?;
    let count = u32_le(&mut bytes)?;
    if start != 1 || count == 0 || count > 2048 || count != chain.target_height() {
        return Err(invalid());
    }
    let mut previous = chain.hash_at(0).ok_or_else(invalid)?;
    let mut blocks = Vec::with_capacity(count as usize);
    for height in 1..=count {
        cancelled(cancel)?;
        let length = u32_le(&mut bytes)? as usize;
        if length == 0 || length > MAX_BLOCK_BYTES {
            return Err(invalid());
        }
        let raw = take(&mut bytes, length)?;
        transaction_count(raw)?;
        let mut remaining = raw;
        let block = Block::read(&mut remaining, &ZNetwork::Regtest).map_err(|_| invalid())?;
        let mut canonical = Vec::with_capacity(raw.len());
        block.write(&mut canonical).map_err(|_| invalid())?;
        if canonical != raw {
            return Err(invalid());
        }
        if !remaining.is_empty()
            || u32::from(block.claimed_height()) != height
            || block.header().prev_block.to_string() != previous
            || chain.is_accepted(u64::from(height), &block.header().hash().to_string())
                != Acceptance::Accepted
        {
            return Err(invalid());
        }
        let branch = BranchId::for_height(&ZNetwork::Regtest, height.into());
        for tx in block.vtx() {
            cancelled(cancel)?;
            if tx.consensus_branch_id() != branch
                || !tx.version().valid_in_branch(branch)
                || tx.version() == TxVersion::V6
                || tx.ironwood_bundle().is_some()
                || tx.sprout_bundle().is_some()
            {
                return Err(invalid());
            }
        }
        let root = transaction_root(block.vtx().iter().map(|tx| *tx.txid().as_ref()).collect())?;
        if root != block.header().merkle_root {
            return Err(invalid());
        }
        previous = block.header().hash().to_string();
        blocks.push(block);
    }
    if !bytes.is_empty() {
        return Err(invalid());
    }
    cancelled(cancel)?;
    Ok(VerifiedRegtestRecoveryBlocks {
        blocks,
        target_hash: previous,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::RegtestScanSchedule;
    use zcash_primitives::{
        block::{BlockHash, BlockHeaderData},
        transaction::Transaction,
    };

    fn fixture() -> (Vec<u8>, RegtestAcceptedChain) {
        let genesis = "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";
        let mut hash = crate::web::from_hex(genesis).unwrap();
        hash.reverse();
        let mut tx = Vec::new();
        tx.extend(0x8000_0004u32.to_le_bytes());
        tx.extend(0x892f_2085u32.to_le_bytes());
        tx.push(1);
        tx.extend([0; 32]);
        tx.extend(u32::MAX.to_le_bytes());
        tx.extend([2, 1, 1]);
        tx.extend(u32::MAX.to_le_bytes());
        tx.push(1);
        tx.extend(50_000u64.to_le_bytes());
        tx.extend([1, 0x51]);
        tx.extend(0u32.to_le_bytes());
        tx.extend(0u32.to_le_bytes());
        tx.extend(0i64.to_le_bytes());
        tx.extend([0, 0, 0]);
        let parsed = Transaction::read(&tx[..], BranchId::Canopy).unwrap();
        let header = BlockHeaderData {
            version: 4,
            prev_block: BlockHash::from_slice(&hash),
            merkle_root: *parsed.txid().as_ref(),
            final_sapling_root: [0; 32],
            time: 1,
            bits: 0,
            nonce: [0; 32],
            solution: vec![],
        }
        .freeze()
        .unwrap();
        let mut raw = Vec::new();
        header.write(&mut raw).unwrap();
        raw.push(1);
        raw.extend(tx);
        let chain = RegtestAcceptedChain::from_local_node(
            RegtestScanSchedule {
                nu6_3_height: crate::regtest_nu6_3_height(),
                nu7_height: crate::regtest_nu7_height(),
            },
            vec![(0, genesis.into()), (1, header.hash().to_string())],
        )
        .unwrap();
        let mut envelope = MAGIC.to_vec();
        envelope.extend(1u32.to_le_bytes());
        envelope.extend(1u32.to_le_bytes());
        envelope.extend((raw.len() as u32).to_le_bytes());
        envelope.extend(raw);
        (envelope, chain)
    }
    #[test]
    fn accepted_complete_fixture_is_verified_without_wallet_state() {
        let (bytes, chain) = fixture();
        let verified =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        assert_eq!(verified.target_height(), 1);
        assert_eq!(verified.target_hash(), chain.target_hash());
        assert_eq!(verified.blocks[0].vtx().len(), 1);
    }
    #[test]
    fn truncation_trailers_and_framing_fail_closed() {
        let (bytes, chain) = fixture();
        for end in 0..bytes.len() {
            assert!(
                verify_regtest_recovery_blocks(&bytes[..end], &chain, &AtomicBool::new(false))
                    .is_err()
            );
        }
        let mut trailer = bytes.clone();
        trailer.push(0);
        assert!(verify_regtest_recovery_blocks(&trailer, &chain, &AtomicBool::new(false)).is_err());
        for offset in [0, MAGIC.len(), MAGIC.len() + 4, MAGIC.len() + 8] {
            let mut changed = bytes.clone();
            changed[offset] ^= 1;
            assert!(
                verify_regtest_recovery_blocks(&changed, &chain, &AtomicBool::new(false)).is_err()
            );
        }
    }
    #[test]
    fn header_and_transaction_substitution_and_cancellation_refused() {
        let (bytes, chain) = fixture();
        for offset in [MAGIC.len() + 12 + 4, bytes.len() - 5] {
            let mut changed = bytes.clone();
            changed[offset] ^= 1;
            assert!(
                verify_regtest_recovery_blocks(&changed, &chain, &AtomicBool::new(false)).is_err()
            );
        }
        assert!(verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(true)).is_err());
        let mut changed = bytes.clone();
        // Per-block trailing byte, with envelope framing adjusted to include it.
        let length_offset = MAGIC.len() + 8;
        let length = u32::from_le_bytes(
            changed[length_offset..length_offset + 4]
                .try_into()
                .unwrap(),
        );
        changed[length_offset..length_offset + 4].copy_from_slice(&(length + 1).to_le_bytes());
        changed.push(0);
        assert!(verify_regtest_recovery_blocks(&changed, &chain, &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn mutated_merkle_trees_and_unbounded_counts_refused() {
        assert!(transaction_root(vec![[1; 32], [1; 32]]).is_err());
        assert!(transaction_root(vec![]).is_err());
        let odd = transaction_root(vec![[1; 32], [2; 32], [3; 32]]).unwrap();
        let mutation = transaction_root(vec![[1; 32], [2; 32], [3; 32], [3; 32]]);
        assert!(mutation.is_err());
        assert_ne!(odd, [0; 32]);
        let mut raw = vec![0; 140];
        raw.push(255);
        raw.extend(u64::MAX.to_le_bytes());
        assert!(transaction_count(&raw).is_err());
        let mut raw = vec![0; 140];
        raw.extend([0, 254]);
        raw.extend(100_000u32.to_le_bytes());
        assert!(transaction_count(&raw).is_err());
    }
}
