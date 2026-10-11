//! Wallet-independent full-block window evidence against caller-accepted mainnet
//! hashes. This checks transaction-effect/txid inclusion, not consensus validity
//! or ZIP244 authorizing-data commitments. It performs no I/O or wallet mutation.
use super::chain_identity;
use crate::{error::Result, EngineError, Network};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use zcash_primitives::block::Block;
use zcash_protocol::consensus::BranchId;

pub const MAINNET_BLOCK_WINDOW_POLICY: &str = "z-stack-mainnet-accepted-block-window-v1";
pub const MAINNET_BLOCK_WINDOW_MAGIC: &[u8] = b"COFFER-MAINNET-BLOCK-WINDOW-V1\n";
pub const MAINNET_BLOCK_WINDOW_WIDTH: u32 = 16;
const MAX_BLOCK_BYTES: usize = 2_000_000;
const MAX_TRANSACTIONS: usize = 65_536;
pub const MAINNET_BLOCK_WINDOW_MAX_BYTES: usize =
    MAINNET_BLOCK_WINDOW_MAGIC.len() + 8 + 16 * (4 + MAX_BLOCK_BYTES);

fn invalid() -> EngineError {
    EngineError::Message("native_block_window_invalid".into())
}
fn current(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(EngineError::Message("native_block_window_cancelled".into()))
    } else {
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash_bytes(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut result = [0; 32];
    for (byte, digits) in result.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(digits).map_err(|_| invalid())?, 16)
            .map_err(|_| invalid())?;
    }
    Ok(result)
}

/// Exactly one predecessor and sixteen contiguous, aligned mainnet block hashes.
/// The caller must obtain these independently of candidate bytes/publication
/// metadata from its accepted node, and recheck that node's snapshot. Construction
/// validates shape and canonical genesis; it does not authenticate the node.
pub struct AcceptedMainnetWindow {
    start: u32,
    hashes: [[u8; 32]; 17],
    source_identity: String,
    accepted_identity: String,
}
impl AcceptedMainnetWindow {
    /// `source_identity` is the caller's lower-case SHA256 context digest, binding
    /// the accepted-node/source policy. Heights and hashes are in display order.
    pub fn from_local_node(
        genesis: &str,
        source_identity: &str,
        headers: Vec<(u32, String)>,
    ) -> Result<Self> {
        if genesis != chain_identity::genesis(Network::Mainnet)
            || headers.len() != 17
            || source_identity.len() != 64
            || !source_identity
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
        let start = headers[1].0;
        if start == 0 || !(start - 1).is_multiple_of(MAINNET_BLOCK_WINDOW_WIDTH) {
            return Err(invalid());
        }
        start.checked_add(15).ok_or_else(invalid)?;
        let mut hashes = [[0; 32]; 17];
        let mut identity = Sha256::new();
        identity.update(MAINNET_BLOCK_WINDOW_POLICY.as_bytes());
        identity.update([0]);
        identity.update(genesis.as_bytes());
        identity.update([0]);
        identity.update(source_identity.as_bytes());
        for (index, (height, hash)) in headers.into_iter().enumerate() {
            if height != start - 1 + index as u32 {
                return Err(invalid());
            }
            hashes[index] = hash_bytes(&hash)?;
            if height == 0 && hashes[index] != hash_bytes(genesis)? {
                return Err(invalid());
            }
            identity.update(height.to_le_bytes());
            identity.update(hashes[index]);
        }
        Ok(Self {
            start,
            hashes,
            source_identity: source_identity.into(),
            accepted_identity: hex(&identity.finalize()),
        })
    }
    pub fn start_height(&self) -> u32 {
        self.start
    }
    pub fn end_height(&self) -> u32 {
        self.start + 15
    }
    pub fn accepted_identity(&self) -> &str {
        &self.accepted_identity
    }
}

/// Authenticated transaction effects for one accepted window. Parsed transactions
/// are intentionally unavailable for wallet replay, signing or broadcast.
pub struct VerifiedMainnetBlockWindow {
    start: u32,
    predecessor_hash: String,
    target_hash: String,
    source_identity: String,
    accepted_identity: String,
    candidate_sha256: String,
}
impl VerifiedMainnetBlockWindow {
    pub fn start_height(&self) -> u32 {
        self.start
    }
    pub fn end_height(&self) -> u32 {
        self.start + 15
    }
    pub fn predecessor_hash(&self) -> &str {
        &self.predecessor_hash
    }
    pub fn target_hash(&self) -> &str {
        &self.target_hash
    }
    pub fn source_identity(&self) -> &str {
        &self.source_identity
    }
    pub fn accepted_identity(&self) -> &str {
        &self.accepted_identity
    }
    pub fn candidate_sha256(&self) -> &str {
        &self.candidate_sha256
    }
    pub fn parser_policy(&self) -> &'static str {
        MAINNET_BLOCK_WINDOW_POLICY
    }
}

fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    let head = bytes.get(..count).ok_or_else(invalid)?;
    *bytes = &bytes[count..];
    Ok(head)
}
fn u32_le(bytes: &mut &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take(bytes, 4)?.try_into().map_err(|_| invalid())?,
    ))
}
fn compact_size(bytes: &mut &[u8]) -> Result<u64> {
    let first = take(bytes, 1)?[0];
    let (value, minimum) = match first {
        0..=252 => return Ok(first.into()),
        253 => (
            u16::from_le_bytes(take(bytes, 2)?.try_into().map_err(|_| invalid())?) as u64,
            253,
        ),
        254 => (u32_le(bytes)? as u64, u16::MAX as u64 + 1),
        255 => (
            u64::from_le_bytes(take(bytes, 8)?.try_into().map_err(|_| invalid())?),
            u32::MAX as u64 + 1,
        ),
    };
    if value < minimum {
        return Err(invalid());
    }
    Ok(value)
}
// Preflight the solution and transaction counts before parsing. The pinned nested
// readers enforce CompactSize limits and fail on incomplete element reads.
fn transaction_count(raw: &[u8]) -> Result<()> {
    let mut bytes = raw;
    take(&mut bytes, 140)?;
    let solution = compact_size(&mut bytes)?;
    if solution > bytes.len() as u64 {
        return Err(invalid());
    }
    take(&mut bytes, solution as usize)?;
    let count = compact_size(&mut bytes)?;
    if count == 0 || count > MAX_TRANSACTIONS as u64 || count > bytes.len() as u64 {
        return Err(invalid());
    }
    Ok(())
}
fn transaction_root(mut nodes: Vec<[u8; 32]>, cancel: &AtomicBool) -> Result<[u8; 32]> {
    if nodes.is_empty() {
        return Err(invalid());
    }
    while nodes.len() > 1 {
        current(cancel)?;
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

/// V1 frame: magic, u32 LE start/count=16, then sixteen u32 LE length/raw blocks.
/// All accepted hashes belong to the independently supplied mainnet snapshot.
/// One block is parsed at a time; no parsed window or wallet state is retained.
pub fn verify_mainnet_block_window(
    candidate: &[u8],
    accepted: &AcceptedMainnetWindow,
    cancel: &AtomicBool,
) -> Result<VerifiedMainnetBlockWindow> {
    current(cancel)?;
    let mut bytes = candidate;
    if bytes.len() > MAINNET_BLOCK_WINDOW_MAX_BYTES
        || take(&mut bytes, MAINNET_BLOCK_WINDOW_MAGIC.len())? != MAINNET_BLOCK_WINDOW_MAGIC
        || u32_le(&mut bytes)? != accepted.start
        || u32_le(&mut bytes)? != MAINNET_BLOCK_WINDOW_WIDTH
    {
        return Err(invalid());
    }
    for index in 1..=16 {
        current(cancel)?;
        let length = u32_le(&mut bytes)? as usize;
        if length == 0 || length > MAX_BLOCK_BYTES {
            return Err(invalid());
        }
        let raw = take(&mut bytes, length)?;
        transaction_count(raw)?;
        let mut remaining = raw;
        let block = Block::read(&mut remaining, &Network::Mainnet).map_err(|_| invalid())?;
        let height = accepted.start - 1 + index as u32;
        let display_hash = |hash: [u8; 32]| {
            let mut result = hash;
            result.reverse();
            result
        };
        if !remaining.is_empty()
            || u32::from(block.claimed_height()) != height
            || display_hash(block.header().prev_block.0) != accepted.hashes[index - 1]
            || display_hash(block.header().hash().0) != accepted.hashes[index]
        {
            return Err(invalid());
        }
        let branch = BranchId::for_height(&Network::Mainnet, height.into());
        let mut txids = Vec::with_capacity(block.vtx().len());
        for tx in block.vtx() {
            current(cancel)?;
            if tx.consensus_branch_id() != branch || !tx.version().valid_in_branch(branch) {
                return Err(invalid());
            }
            txids.push(*tx.txid().as_ref());
        }
        if transaction_root(txids, cancel)? != block.header().merkle_root {
            return Err(invalid());
        }
        let mut canonical = Vec::with_capacity(raw.len());
        block.write(&mut canonical).map_err(|_| invalid())?;
        if canonical != raw {
            return Err(invalid());
        }
    }
    if !bytes.is_empty() {
        return Err(invalid());
    }
    let mut digest = Sha256::new();
    for chunk in candidate.chunks(64 * 1024) {
        current(cancel)?;
        digest.update(chunk);
    }
    current(cancel)?;
    Ok(VerifiedMainnetBlockWindow {
        start: accepted.start,
        predecessor_hash: hex(&accepted.hashes[0]),
        target_hash: hex(&accepted.hashes[16]),
        source_identity: accepted.source_identity.clone(),
        accepted_identity: accepted.accepted_identity.clone(),
        candidate_sha256: hex(&digest.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use transparent::{
        address::Script,
        bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut},
    };
    use zcash_primitives::{
        block::{BlockHash, BlockHeaderData},
        transaction::{Authorized, Transaction, TransactionData, TxVersion},
    };
    use zcash_protocol::{
        consensus::{NetworkUpgrade, Parameters},
        value::Zatoshis,
    };

    const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    struct Fixture {
        start: u32,
        headers: Vec<(u32, String)>,
        blocks: Vec<Vec<u8>>,
    }
    impl Fixture {
        fn accepted(&self) -> AcceptedMainnetWindow {
            AcceptedMainnetWindow::from_local_node(
                chain_identity::genesis(Network::Mainnet),
                SOURCE,
                self.headers.clone(),
            )
            .unwrap()
        }
        fn frame(&self) -> Vec<u8> {
            let mut bytes = MAINNET_BLOCK_WINDOW_MAGIC.to_vec();
            bytes.extend(self.start.to_le_bytes());
            bytes.extend(16u32.to_le_bytes());
            for block in &self.blocks {
                bytes.extend((block.len() as u32).to_le_bytes());
                bytes.extend(block);
            }
            bytes
        }
        fn verify(&self) -> Result<VerifiedMainnetBlockWindow> {
            verify_mainnet_block_window(&self.frame(), &self.accepted(), &AtomicBool::new(false))
        }
    }
    fn tx(height: u32, coinbase: bool, branch: BranchId, version: TxVersion) -> Transaction {
        let mut number = height.to_le_bytes().to_vec();
        while number.last() == Some(&0) {
            number.pop();
        }
        if number.last().is_some_and(|b| b & 0x80 != 0) {
            number.push(0);
        }
        let mut sig = vec![number.len() as u8];
        sig.extend(number);
        let bundle = Some(Bundle {
            vin: vec![TxIn::from_parts(
                OutPoint::new(
                    if coinbase { [0; 32] } else { [9; 32] },
                    if coinbase { u32::MAX } else { 0 },
                ),
                Script(zcash_script::script::Code(sig)),
                u32::MAX,
            )],
            vout: vec![TxOut::new(
                Zatoshis::from_u64(50_000 + height as u64).unwrap(),
                Script(zcash_script::script::Code(vec![0x51])),
            )],
            authorization: TransparentAuthorized,
        });
        if version == TxVersion::V6 {
            TransactionData::<Authorized>::from_parts_v6(
                branch,
                0,
                0.into(),
                bundle,
                None,
                None,
                None,
            )
            .freeze()
            .unwrap()
        } else {
            TransactionData::<Authorized>::from_parts(
                version,
                branch,
                0,
                0.into(),
                bundle,
                None,
                None,
                None,
            )
            .freeze()
            .unwrap()
        }
    }
    fn fixture(start: u32) -> Fixture {
        // Native Mainnet parsing and branch schedules, with synthetic headers and
        // transparent transactions. These are not valid consensus/PoW fixtures.
        let mut fixture = Fixture {
            start,
            headers: vec![(start - 1, "11".repeat(32))],
            blocks: vec![],
        };
        let mut parent = [0x11; 32];
        for height in start..start + 16 {
            let branch = BranchId::for_height(&Network::Mainnet, height.into());
            let version = match branch {
                BranchId::Sprout => TxVersion::Sprout(1),
                BranchId::Overwinter => TxVersion::V3,
                BranchId::Sapling | BranchId::Blossom | BranchId::Heartwood | BranchId::Canopy => {
                    TxVersion::V4
                }
                BranchId::Nu5 | BranchId::Nu6 | BranchId::Nu6_1 | BranchId::Nu6_2 => TxVersion::V5,
                BranchId::Nu6_3 | BranchId::Nu7 => TxVersion::V6,
            };
            let coinbase = tx(height, true, branch, version);
            let payment = tx(height, false, branch, version);
            let root = transaction_root(
                vec![*coinbase.txid().as_ref(), *payment.txid().as_ref()],
                &AtomicBool::new(false),
            )
            .unwrap();
            let header = BlockHeaderData {
                version: 4,
                prev_block: BlockHash(parent),
                merkle_root: root,
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
            raw.push(2);
            coinbase.write(&mut raw).unwrap();
            payment.write(&mut raw).unwrap();
            fixture.headers.push((height, header.hash().to_string()));
            fixture.blocks.push(raw);
            parent = header.hash().0;
        }
        fixture
    }
    fn current_fixture() -> Fixture {
        fixture(3_428_145)
    }
    fn refused(fixture: &Fixture) {
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn mainnet_window_authenticates_all_sixteen_blocks_and_binds_source_and_bytes() {
        let fixture = current_fixture();
        let frame = fixture.frame();
        let accepted = fixture.accepted();
        let verified = fixture.verify().unwrap();
        assert_eq!(verified.start_height(), fixture.start);
        assert_eq!(verified.end_height(), fixture.start + 15);
        assert_eq!(verified.predecessor_hash(), fixture.headers[0].1);
        assert_eq!(verified.target_hash(), fixture.headers[16].1);
        assert_eq!(verified.source_identity(), SOURCE);
        assert_eq!(verified.parser_policy(), MAINNET_BLOCK_WINDOW_POLICY);
        assert_eq!(verified.accepted_identity(), accepted.accepted_identity());
        assert_eq!(verified.candidate_sha256(), hex(&Sha256::digest(frame)));
        let other = AcceptedMainnetWindow::from_local_node(
            chain_identity::genesis(Network::Mainnet),
            &"bb".repeat(32),
            fixture.headers.clone(),
        )
        .unwrap();
        assert_ne!(accepted.accepted_identity(), other.accepted_identity());
        let mut upper = fixture.headers.clone();
        for (_, hash) in &mut upper {
            *hash = hash.to_uppercase();
        }
        assert_eq!(
            accepted.accepted_identity(),
            AcceptedMainnetWindow::from_local_node(
                chain_identity::genesis(Network::Mainnet),
                SOURCE,
                upper
            )
            .unwrap()
            .accepted_identity()
        );
    }
    #[test]
    fn mainnet_window_uses_actual_mainnet_upgrade_boundaries() {
        for upgrade in [
            NetworkUpgrade::Nu5,
            NetworkUpgrade::Nu6_3,
            NetworkUpgrade::Nu7,
        ] {
            if let Some(height) = Network::Mainnet.activation_height(upgrade) {
                let height = u32::from(height);
                let start = (height - 1) / 16 * 16 + 1;
                let fixture = fixture(start);
                assert!(fixture.verify().is_ok(), "{upgrade}");
                let branch_before = BranchId::for_height(&Network::Mainnet, (height - 1).into());
                let branch_after = BranchId::for_height(&Network::Mainnet, height.into());
                assert_ne!(branch_before, branch_after);
            }
        }
        assert!(fixture(1_046_401).verify().is_ok()); // V4 before NU5.
    }
    #[test]
    fn mainnet_window_constructor_refuses_wrong_network_shape_alignment_and_hashes() {
        let fixture = current_fixture();
        let genesis = chain_identity::genesis(Network::Mainnet);
        for wrong in [
            chain_identity::genesis(Network::Testnet),
            chain_identity::genesis(Network::Regtest),
            "mainnet",
        ] {
            assert!(
                AcceptedMainnetWindow::from_local_node(wrong, SOURCE, fixture.headers.clone())
                    .is_err()
            );
        }
        for source in ["", "aa", &"AB".repeat(32), &"gg".repeat(32)] {
            assert!(AcceptedMainnetWindow::from_local_node(
                genesis,
                source,
                fixture.headers.clone()
            )
            .is_err());
        }
        for count in [0, 16, 18] {
            let mut headers = fixture.headers.clone();
            headers.resize(count, (0, "00".repeat(32)));
            assert!(AcceptedMainnetWindow::from_local_node(genesis, SOURCE, headers).is_err());
        }
        for changed in [0, 1, 16] {
            let mut headers = fixture.headers.clone();
            headers[changed].0 += 1;
            assert!(AcceptedMainnetWindow::from_local_node(genesis, SOURCE, headers).is_err());
        }
        for start in [0, 2, u32::MAX - 14] {
            let headers = (0..17)
                .map(|i| (start.saturating_sub(1).saturating_add(i), "11".repeat(32)))
                .collect();
            assert!(AcceptedMainnetWindow::from_local_node(genesis, SOURCE, headers).is_err());
        }
        for hash in ["00".repeat(31), "g0".repeat(32)] {
            let mut headers = fixture.headers.clone();
            headers[8].1 = hash;
            assert!(AcceptedMainnetWindow::from_local_node(genesis, SOURCE, headers).is_err());
        }
        let headers = (0..17).map(|i| (i, "11".repeat(32))).collect();
        assert!(AcceptedMainnetWindow::from_local_node(genesis, SOURCE, headers).is_err());
    }
    #[test]
    fn mainnet_window_refuses_bad_framing_counts_lengths_and_cancellation() {
        let fixture = current_fixture();
        let accepted = fixture.accepted();
        let frame = fixture.frame();
        for length in [
            0,
            MAINNET_BLOCK_WINDOW_MAGIC.len() - 1,
            MAINNET_BLOCK_WINDOW_MAGIC.len() + 7,
            frame.len() - 1,
        ] {
            assert!(verify_mainnet_block_window(
                &frame[..length],
                &accepted,
                &AtomicBool::new(false)
            )
            .is_err());
        }
        let mut extra = frame.clone();
        extra.push(0);
        assert!(verify_mainnet_block_window(&extra, &accepted, &AtomicBool::new(false)).is_err());
        for (offset, value) in [
            (MAINNET_BLOCK_WINDOW_MAGIC.len(), fixture.start + 1),
            (MAINNET_BLOCK_WINDOW_MAGIC.len() + 4, 15),
            (MAINNET_BLOCK_WINDOW_MAGIC.len() + 4, 17),
            (MAINNET_BLOCK_WINDOW_MAGIC.len() + 8, 0),
            (
                MAINNET_BLOCK_WINDOW_MAGIC.len() + 8,
                MAX_BLOCK_BYTES as u32 + 1,
            ),
        ] {
            let mut bad = frame.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(verify_mainnet_block_window(&bad, &accepted, &AtomicBool::new(false)).is_err());
        }
        let oversized = vec![0; MAINNET_BLOCK_WINDOW_MAX_BYTES + 1];
        assert!(
            verify_mainnet_block_window(&oversized, &accepted, &AtomicBool::new(false)).is_err()
        );
        assert!(
            matches!(verify_mainnet_block_window(&frame,&accepted,&AtomicBool::new(true)),Err(EngineError::Message(code)) if code=="native_block_window_cancelled")
        );
    }
    #[test]
    fn mainnet_window_refuses_hash_link_merkle_height_and_branch_substitution() {
        let mut fixture = current_fixture();
        fixture.headers[8].1 = "22".repeat(32);
        refused(&fixture);
        let mut fixture = current_fixture();
        fixture.headers[0].1 = "22".repeat(32);
        refused(&fixture);
        let mut fixture = current_fixture();
        fixture.blocks.swap(1, 2);
        refused(&fixture);
        // Changes to a header are independently reflected in the accepted hash,
        // so these checks exercise linkage and txid inclusion rather than merely
        // the outer accepted-hash comparison.
        for offset in [4, 36] {
            let mut fixture = current_fixture();
            fixture.blocks[0][offset] ^= 1;
            let header =
                zcash_primitives::block::BlockHeader::read(&fixture.blocks[0][..]).unwrap();
            fixture.headers[1].1 = header.hash().to_string();
            refused(&fixture);
        }
        let mut fixture = current_fixture();
        let mut parsed = &fixture.blocks[0][..];
        let block = Block::read(&mut parsed, &Network::Mainnet).unwrap();
        let header = block.header();
        let mut bad = vec![];
        header.write(&mut bad).unwrap();
        bad.push(2);
        let branch = BranchId::for_height(&Network::Mainnet, fixture.start.into());
        tx(fixture.start + 1, true, branch, TxVersion::V6)
            .write(&mut bad)
            .unwrap();
        block.vtx()[1].write(&mut bad).unwrap();
        fixture.blocks[0] = bad;
        refused(&fixture);
        let mut fixture = current_fixture();
        let branch_offset = 141 + 1 + 8; // Empty solution, two txs, V6 header/group.
        fixture.blocks[0][branch_offset..branch_offset + 4]
            .copy_from_slice(&u32::from(BranchId::Nu5).to_le_bytes());
        refused(&fixture);
    }
    #[test]
    fn mainnet_window_preflights_noncanonical_and_unbounded_vector_counts() {
        let fixture = current_fixture();
        let original = &fixture.blocks[0];
        for count in [
            vec![0],
            vec![253, 1, 0],
            vec![254, 0, 0, 1, 0],
            vec![255, 255, 255, 255, 255, 255, 255, 255, 255],
        ] {
            let mut raw = original[..141].to_vec();
            raw.extend(count);
            raw.extend(&original[142..]);
            assert!(transaction_count(&raw).is_err());
        }
        let mut raw = original[..140].to_vec();
        raw.extend([255; 9]);
        raw.extend(&original[141..]);
        assert!(transaction_count(&raw).is_err());
        let mut fixture = current_fixture();
        // The coinbase transparent input vector follows the V6 branch/lock/expiry
        // header. A huge declared vector must fail the actual transaction parser.
        let input_count_offset = 142 + 20;
        fixture.blocks[0].splice(input_count_offset..input_count_offset + 1, [255; 9]);
        refused(&fixture);
        assert!(transaction_root(vec![[1; 32], [1; 32]], &AtomicBool::new(false)).is_err());
        assert!(transaction_root(
            vec![[1; 32], [2; 32], [3; 32], [3; 32]],
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(transaction_root(vec![[1; 32], [2; 32], [3; 32]], &AtomicBool::new(false)).is_ok());
    }
}
