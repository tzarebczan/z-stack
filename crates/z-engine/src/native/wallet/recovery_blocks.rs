//! Complete bounded raw-block recovery evidence. Accepted headers are supplied
//! independently; this verifies transaction-effect/txid inclusion, not consensus
//! validity or ZIP244 authorizing-data commitments. Source transactions must
//! never replace wallet-authored pending raw bytes or be reused for broadcast.
use super::{RegtestAcceptedChain, ZNetwork};
use crate::error::{EngineError, Result};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use transparent_wallet::{Acceptance, ChainView};
use zcash_primitives::block::Block;
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
impl VerifiedRegtestRecoveryBlocks {
    pub(super) fn compact_blocks(
        &self,
    ) -> Result<Vec<zcash_client_backend::proto::compact_formats::CompactBlock>> {
        use zcash_client_backend::proto::compact_formats::{
            ChainMetadata, CompactBlock, CompactOrchardAction, CompactSaplingOutput,
            CompactSaplingSpend, CompactTx,
        };
        let mut sizes = [0u32; 3];
        let mut blocks = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            let mut transactions = Vec::with_capacity(block.vtx().len());
            for (index, tx) in block.vtx().iter().enumerate() {
                let mut compact = CompactTx {
                    index: index as u64,
                    txid: tx.txid().as_ref().to_vec(),
                    ..Default::default()
                };
                if let Some(bundle) = tx.sapling_bundle() {
                    compact.spends = bundle
                        .shielded_spends()
                        .iter()
                        .map(|spend| CompactSaplingSpend {
                            nf: spend.nullifier().0.to_vec(),
                        })
                        .collect();
                    compact.outputs = bundle
                        .shielded_outputs()
                        .iter()
                        .map(|output| CompactSaplingOutput {
                            cmu: output.cmu().to_bytes().to_vec(),
                            ephemeral_key: output.ephemeral_key().0.to_vec(),
                            ciphertext: output.enc_ciphertext()[..52].to_vec(),
                        })
                        .collect();
                }
                if let Some(bundle) = tx.orchard_bundle() {
                    compact.actions = bundle
                        .actions()
                        .iter()
                        .map(CompactOrchardAction::from)
                        .collect();
                }
                if let Some(bundle) = tx.ironwood_bundle() {
                    compact.ironwood_actions = bundle
                        .actions()
                        .iter()
                        .map(CompactOrchardAction::from)
                        .collect();
                }
                for (size, count) in sizes.iter_mut().zip([
                    compact.outputs.len(),
                    compact.actions.len(),
                    compact.ironwood_actions.len(),
                ]) {
                    *size = size
                        .checked_add(u32::try_from(count).map_err(|_| invalid())?)
                        .ok_or_else(invalid)?;
                }
                transactions.push(compact);
            }
            blocks.push(CompactBlock {
                height: u64::from(u32::from(block.claimed_height())),
                hash: block.header().hash().0.to_vec(),
                prev_hash: block.header().prev_block.0.to_vec(),
                time: block.header().time,
                header: vec![],
                vtx: transactions,
                chain_metadata: Some(ChainMetadata {
                    sapling_commitment_tree_size: sizes[0],
                    orchard_commitment_tree_size: sizes[1],
                    ironwood_commitment_tree_size: sizes[2],
                }),
            });
        }
        Ok(blocks)
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
/// Sprout payloads remain unsupported by canonical recovery.
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
pub(super) mod tests {
    use super::*;
    use crate::native::RegtestScanSchedule;
    use zcash_primitives::{
        block::{BlockHash, BlockHeaderData},
        transaction::Transaction,
    };

    fn fixture() -> (Vec<u8>, RegtestAcceptedChain) {
        fixture_with_script(&[0x51])
    }
    pub(in crate::native::wallet) fn fixture_with_script(
        script: &[u8],
    ) -> (Vec<u8>, RegtestAcceptedChain) {
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
        tx.push(script.len() as u8);
        tx.extend(script);
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
    pub(in crate::native::wallet) fn fixture_prefix(
        script: &[u8],
        count: u32,
    ) -> (Vec<u8>, RegtestAcceptedChain) {
        fixture_prefix_with_transaction(script, count, None)
    }
    fn fixture_prefix_with_transaction(
        script: &[u8],
        count: u32,
        extra: Option<Transaction>,
    ) -> (Vec<u8>, RegtestAcceptedChain) {
        use transparent::{
            address::Script,
            bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut},
        };
        use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
        use zcash_protocol::value::Zatoshis;
        let genesis = "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";
        let mut parent = crate::web::from_hex(genesis).unwrap();
        parent.reverse();
        let mut headers = vec![(0, genesis.to_string())];
        let mut envelope = MAGIC.to_vec();
        envelope.extend(1u32.to_le_bytes());
        envelope.extend(count.to_le_bytes());
        for height in 1..=count {
            let mut height_bytes = height.to_le_bytes().to_vec();
            while height_bytes.last() == Some(&0) {
                height_bytes.pop();
            }
            if height_bytes.last().is_some_and(|b| b & 0x80 != 0) {
                height_bytes.push(0);
            }
            let mut sig = vec![height_bytes.len() as u8];
            sig.extend(height_bytes);
            let transparent = Some(Bundle {
                vin: vec![TxIn::from_parts(
                    OutPoint::new([0; 32], u32::MAX),
                    Script(zcash_script::script::Code(sig)),
                    u32::MAX,
                )],
                vout: vec![TxOut::new(
                    Zatoshis::from_u64(50_000 + u64::from(height)).unwrap(),
                    Script(zcash_script::script::Code(script.to_vec())),
                )],
                authorization: TransparentAuthorized,
            });
            let branch = BranchId::for_height(&ZNetwork::Regtest, height.into());
            let tx = if branch == BranchId::Canopy {
                TransactionData::<Authorized>::from_parts(
                    TxVersion::V4,
                    branch,
                    0,
                    0.into(),
                    transparent,
                    None,
                    None,
                    None,
                )
                .freeze()
                .unwrap()
            } else if height < crate::regtest_nu6_3_height() {
                TransactionData::<Authorized>::from_parts(
                    TxVersion::V5,
                    branch,
                    0,
                    0.into(),
                    transparent,
                    None,
                    None,
                    None,
                )
                .freeze()
                .unwrap()
            } else {
                TransactionData::<Authorized>::from_parts_v6(
                    branch,
                    0,
                    0.into(),
                    transparent,
                    None,
                    None,
                    None,
                )
                .freeze()
                .unwrap()
            };
            let header = BlockHeaderData {
                version: 4,
                prev_block: BlockHash::from_slice(&parent),
                merkle_root: transaction_root(if height == count {
                    extra.as_ref().map_or_else(
                        || vec![*tx.txid().as_ref()],
                        |other| vec![*tx.txid().as_ref(), *other.txid().as_ref()],
                    )
                } else {
                    vec![*tx.txid().as_ref()]
                })
                .unwrap(),
                final_sapling_root: [0; 32],
                time: height,
                bits: 0,
                nonce: [0; 32],
                solution: vec![],
            }
            .freeze()
            .unwrap();
            let mut raw = vec![];
            header.write(&mut raw).unwrap();
            raw.push(if height == count && extra.is_some() {
                2
            } else {
                1
            });
            tx.write(&mut raw).unwrap();
            if height == count {
                if let Some(other) = &extra {
                    other.write(&mut raw).unwrap();
                }
            }
            headers.push((height, header.hash().to_string()));
            parent = header.hash().0.to_vec();
            envelope.extend((raw.len() as u32).to_le_bytes());
            envelope.extend(raw);
        }
        (
            envelope,
            RegtestAcceptedChain::from_local_node(
                RegtestScanSchedule {
                    nu6_3_height: crate::regtest_nu6_3_height(),
                    nu7_height: crate::regtest_nu7_height(),
                },
                headers,
            )
            .unwrap(),
        )
    }
    #[test]
    #[ignore = "requires Z_STACK_REGTEST_NU6_3=150 bounded fixture schedule"]
    fn v6_all_pools_compact_effects_come_from_same_full_transaction() {
        use orchard::{
            note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
            note_encryption::{
                IronwoodDomain, IronwoodNoteEncryption, OrchardDomain, OrchardNoteEncryption,
            },
            value::NoteValue,
            Note,
        };
        use zcash_note_encryption::Domain;
        use zcash_primitives::transaction::{Authorized, TransactionData};
        let (ufvk, _) = crate::native::wallet::tests::fixture_account();
        let fvk = ufvk.orchard().unwrap();
        let mut expected = vec![];
        let empty = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            0.into(),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        let mut raw = vec![];
        empty.write(&mut raw).unwrap();
        raw.truncate(raw.len() - 2);
        for ironwood in [false, true] {
            let rho = Rho::from_bytes(&[0; 32]).unwrap();
            let rseed = RandomSeed::from_bytes([7; 32], &rho).unwrap();
            let note = Note::from_parts(
                fvk.address_at(0u32, zip32::Scope::External),
                NoteValue::from_raw(50_000),
                rho,
                rseed,
                if ironwood {
                    NoteVersion::V3
                } else {
                    NoteVersion::V2
                },
            )
            .unwrap();
            let cmx = ExtractedNoteCommitment::from(note.commitment()).to_bytes();
            let (epk, ciphertext) = if ironwood {
                let enc = IronwoodNoteEncryption::new(None, note, [0; 512]);
                (
                    IronwoodDomain::epk_bytes(enc.epk()).0,
                    enc.encrypt_note_plaintext(),
                )
            } else {
                let enc = OrchardNoteEncryption::new(None, note, [0; 512]);
                (
                    OrchardDomain::epk_bytes(enc.epk()).0,
                    enc.encrypt_note_plaintext(),
                )
            };
            expected.push((cmx, epk, ciphertext[..52].to_vec()));
            raw.push(1);
            raw.extend([0; 32]);
            raw.extend([0; 32]);
            raw.extend(epk);
            raw.extend(cmx);
            raw.extend(epk);
            raw.extend(ciphertext);
            raw.extend([0; 80]);
            raw.push(3);
            raw.extend(0i64.to_le_bytes());
            raw.extend([0; 32]);
            let proof_len = orchard::Proof::expected_proof_size(1);
            raw.push(253);
            raw.extend((proof_len as u16).to_le_bytes());
            raw.extend(vec![0; proof_len]);
            raw.extend([0; 128]);
        }
        // Dummy auth bytes isolate native parsing/effect conversion; this test
        // does not claim signatures/proofs or synthetic headers are valid chain.
        let tx = Transaction::read(&raw[..], BranchId::Nu6_3).unwrap();
        let sapling_sample = Transaction::read(
            &include_bytes!("fixtures/zakura-public-sapling/transaction.bin")[..],
            BranchId::Canopy,
        )
        .unwrap();
        let sapling = sapling_sample.sapling_bundle().unwrap().clone();
        let expected_sapling = sapling
            .shielded_outputs()
            .iter()
            .map(|output| {
                (
                    output.cmu().to_bytes(),
                    output.ephemeral_key().0,
                    output.enc_ciphertext()[..52].to_vec(),
                )
            })
            .collect::<Vec<_>>();
        assert!(!expected_sapling.is_empty());
        let expected_spends = sapling
            .shielded_spends()
            .iter()
            .map(|spend| spend.nullifier().0)
            .collect::<Vec<_>>();
        let tx = TransactionData::<Authorized>::from_parts_v6(
            BranchId::Nu6_3,
            0,
            0.into(),
            None,
            Some(sapling),
            tx.orchard_bundle().cloned(),
            tx.ironwood_bundle().cloned(),
        )
        .freeze()
        .unwrap();
        assert!(
            crate::regtest_nu6_3_height() <= 2048,
            "bounded fixture schedule required"
        );
        let (bytes, chain) =
            fixture_prefix_with_transaction(&[0x51], crate::regtest_nu6_3_height(), Some(tx));
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        let compact = evidence.compact_blocks().unwrap();
        let last = compact.last().unwrap();
        let tx = &last.vtx[1];
        for (actions, (cmx, epk, ciphertext)) in [&tx.actions, &tx.ironwood_actions]
            .into_iter()
            .zip(expected)
        {
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].cmx, cmx);
            assert_eq!(actions[0].ephemeral_key, epk);
            assert_eq!(actions[0].ciphertext, ciphertext);
        }
        assert_eq!(tx.spends.len(), expected_spends.len());
        for (spend, nf) in tx.spends.iter().zip(expected_spends) {
            assert_eq!(spend.nf, nf);
        }
        assert_eq!(tx.outputs.len(), expected_sapling.len());
        for (output, (cmu, epk, ciphertext)) in tx.outputs.iter().zip(expected_sapling) {
            assert_eq!(output.cmu, cmu);
            assert_eq!(output.ephemeral_key, epk);
            assert_eq!(output.ciphertext, ciphertext);
        }
        let metadata = last.chain_metadata.as_ref().unwrap();
        assert_eq!(
            metadata.sapling_commitment_tree_size,
            tx.outputs.len() as u32
        );
        assert_eq!(metadata.orchard_commitment_tree_size, 1);
        assert_eq!(metadata.ironwood_commitment_tree_size, 1);
    }
    #[test]
    #[ignore = "requires Z_STACK_REGTEST_NU6_3=150 bounded fixture schedule"]
    fn v6_prefix_and_compact_metadata_follow_actual_native_transactions() {
        let target = crate::regtest_nu6_3_height();
        assert!(target <= 2048, "bounded fixture schedule required");
        let (bytes, chain) = fixture_prefix(&[0x51], target);
        let evidence =
            verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            evidence.blocks.last().unwrap().vtx()[0].version(),
            zcash_primitives::transaction::TxVersion::V6
        );
        let compact = evidence.compact_blocks().unwrap();
        assert_eq!(compact.len(), target as usize);
        assert_eq!(
            compact.last().unwrap().vtx[0].txid,
            evidence.blocks.last().unwrap().vtx()[0].txid().as_ref()
        );
        assert_eq!(
            compact
                .last()
                .unwrap()
                .chain_metadata
                .as_ref()
                .unwrap()
                .ironwood_commitment_tree_size,
            0
        );
        let wrong = zcash_primitives::transaction::TransactionData::<
            zcash_primitives::transaction::Authorized,
        >::from_parts_v6(BranchId::Nu7, 0, 0.into(), None, None, None, None)
        .freeze()
        .unwrap();
        let (bytes, chain) = fixture_prefix_with_transaction(&[0x51], target, Some(wrong));
        assert!(verify_regtest_recovery_blocks(&bytes, &chain, &AtomicBool::new(false)).is_err());
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
    fn nested_transaction_vector_counts_fail_without_unbounded_reserve() {
        let (bytes, chain) = fixture();
        // Header(140 + empty solution), vtx count, V4 header precede vin.
        let raw_start = MAGIC.len() + 12;
        let vin = raw_start + 141 + 1 + 8;
        let mut malformed = bytes[..vin].to_vec();
        malformed.push(255);
        malformed.extend(u64::MAX.to_le_bytes());
        malformed.extend([0; 4]);
        let raw_length = (malformed.len() - raw_start) as u32;
        malformed[MAGIC.len() + 8..raw_start].copy_from_slice(&raw_length.to_le_bytes());
        // Pinned Array readers collect io::Result element-by-element and stop on
        // the first read error; the claimed count never reserves that capacity.
        assert!(
            verify_regtest_recovery_blocks(&malformed, &chain, &AtomicBool::new(false)).is_err()
        );
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
