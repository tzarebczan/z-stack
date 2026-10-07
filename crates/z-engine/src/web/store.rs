//! Versioned in-memory wallet snapshot (IndexedDB blob on the JS side).

use super::history;
use super::live_trees::{self, StoredLiveTrees};
use super::public_data::ScanCoverage;
use super::{from_hex, to_hex, SNAPSHOT_MAGIC};
use crate::error::{EngineError, Result};
use crate::keys::AccountKeys;
use crate::offload::{self, KeptLeaf, Offload, OffloadOutput};
use crate::{format_zatoshis, zip321_uri, Balance, HistoryEntry, Network};
use incrementalmerkletree::{Address as MerkleAddress, Marking, Position, Retention};
use orchard::note::{Note as OrchardNote, RandomSeed, Rho};
use orchard::note::{NoteVersion, Nullifier as OrchardNullifier};
use orchard::tree::MerkleHashOrchard;
use orchard::value::NoteValue;
use orchard::Address as OrchardAddress;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};
use shardtree::store::memory::MemoryShardStore;
use shardtree::store::ShardStore;
use shardtree::ShardTree;
use std::collections::{BTreeMap, BTreeSet};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::data_api::{BlockMetadata, ORCHARD_SHARD_HEIGHT, SAPLING_SHARD_HEIGHT};
use zcash_client_backend::proto::service::TreeState;
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedFullViewingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::merkle_tree::HashSer;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};
use zcash_protocol::{ShieldedPool, TxId};
use zip32::{DiversifierIndex, Scope};

// Per-block checkpoints only need the inclusive recent anchor window. Rewind
// rebuilds older retained history from the unchanged snapshot leaves/metadata.
pub(crate) const TREE_CHECKPOINTS: usize = offload::TIP_CHECKPOINT_BLOCKS as usize + 1;
pub(crate) use crate::scan::{CHECKPOINT_EVERY, HASH_KEEP};
/// JSON snapshot layout. Magic stays `zstk1`; this field distinguishes selective shard scanning + tree-size marks.
const SNAPSHOT_VERSION: u32 = 2;
fn snapshot_version_legacy() -> u32 {
    1
}

pub(crate) type SaplingTree = ShardTree<
    MemoryShardStore<sapling::Node, BlockHeight>,
    { SAPLING_SHARD_HEIGHT * 2 },
    SAPLING_SHARD_HEIGHT,
>;
pub(crate) type OrchardTree = ShardTree<
    MemoryShardStore<MerkleHashOrchard, BlockHeight>,
    { ORCHARD_SHARD_HEIGHT * 2 },
    ORCHARD_SHARD_HEIGHT,
>;

pub(crate) fn empty_sapling_tree() -> SaplingTree {
    ShardTree::new(MemoryShardStore::empty(), TREE_CHECKPOINTS)
}

pub(crate) fn empty_orchard_tree() -> OrchardTree {
    ShardTree::new(MemoryShardStore::empty(), TREE_CHECKPOINTS)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedNote {
    pub pool: String,
    pub txid: String,
    pub output_index: u32,
    pub value_zat: u64,
    pub nf: String,
    pub is_change: bool,
    pub spent: bool,
    pub spent_in: Option<String>,
    pub spent_height: Option<u32>,
    pub mined_height: u32,
}

/// Full orchard/ironwood note needed to spend. Not shown in history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredNote {
    pub id: u32,
    pub pool: String,
    pub txid: String,
    pub output_index: u16,
    pub position: u64,
    pub mined_height: u32,
    /// 0 = external, 1 = internal
    pub scope: u8,
    pub recipient: String,
    pub value_zat: u64,
    pub rho: String,
    pub rseed: String,
    pub version: u8,
    pub nf: String,
    pub is_change: bool,
    pub spent: bool,
    #[serde(default)]
    pub spent_in: Option<String>,
}

/// An output reserved by an in-flight proposal (a PCZT waiting on a hardware
/// signer). Input selection skips it until it expires or its owner unlocks it.
/// Kept in memory only (see `from_snapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredLock {
    /// Wire-order txid hex, as `StoredNote::txid`.
    pub txid: String,
    pub pool: String,
    pub index: u32,
    /// Lock owner, 32 bytes hex.
    pub owner: String,
    /// Last block height at which the lock holds.
    pub expiry: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TreeLeaf {
    pub hash: String,
    /// 0 ephemeral, 1 marked, 2 checkpoint, 3 checkpoint+marked
    pub kind: u8,
    #[serde(default)]
    pub height: u32,
    /// Note-commitment tree position. 0 on pre-selective-scan snapshots (infer dense).
    #[serde(default)]
    pub position: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct BirthdayTrees {
    height: u32,
    hash: String,
    sapling: String,
    orchard: String,
    ironwood: String,
}

/// Index `i` is shard `i`. Roots are fetched from the birthday shard, so
/// earlier indexes hold placeholders with an empty `root_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSubtreeRoot {
    completing_height: u64,
    root_hash: String,
}

impl StoredSubtreeRoot {
    fn placeholder() -> Self {
        Self {
            completing_height: 0,
            root_hash: String::new(),
        }
    }

    fn is_placeholder(&self) -> bool {
        self.root_hash.is_empty()
    }
}

/// Shards `from..roots.len()` have server roots.
fn root_range(roots: &[StoredSubtreeRoot]) -> (u64, u64) {
    let from = roots
        .iter()
        .position(|r| !r.is_placeholder())
        .unwrap_or(roots.len());
    (from as u64, roots.len() as u64)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredUtxo {
    #[serde(default)]
    pub coinbase: bool,
    pub txid: String,
    pub index: u32,
    pub script: String,
    pub value_zat: u64,
    pub height: u32,
    pub address: String,
    pub spent: bool,
    #[serde(default)]
    pub spent_in: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxAgg {
    pub mined_height: Option<u32>,
    pub received_zat: u64,
    pub spent_zat: u64,
    pub received_notes: u32,
    pub spent_notes: u32,
    pub has_change: bool,
    #[serde(default)]
    pub is_shielding: bool,
    #[serde(default)]
    pub fee_zat: Option<u64>,
    #[serde(default)]
    pub expiry_height: Option<u32>,
    #[serde(default)]
    pub memos: Vec<String>,
    /// Full transaction processed: decrypted outputs (including blanks), or a
    /// mined spend without incoming outputs or recoverable outgoing ciphertext.
    #[serde(default)]
    pub enhancement_complete: bool,
    /// Whether full-transaction decryption recovered any shielded output.
    /// None preserves the uncertainty in snapshots written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo_recovered: Option<bool>,
    /// Raw public transaction metadata is kept separately so older snapshots
    /// with completed memo enhancement can be upgraded without a rescan.
    #[serde(default)]
    pub history_metadata_complete: bool,
    #[serde(default)]
    pub outgoing_shielded_zat: u64,
    #[serde(default)]
    pub transparent_inputs: Vec<HistoryOutpoint>,
    #[serde(default)]
    pub transparent_outputs: Vec<HistoryOutput>,
    #[serde(default)]
    pub block_time: Option<u32>,
    #[serde(default)]
    pub transparent_received: u64,
    #[serde(default)]
    pub transparent_spent: u64,
    #[serde(default)]
    pub sapling_received: u64,
    #[serde(default)]
    pub sapling_spent: u64,
    #[serde(default)]
    pub orchard_received: u64,
    #[serde(default)]
    pub orchard_spent: u64,
    #[serde(default)]
    pub ironwood_received: u64,
    #[serde(default)]
    pub ironwood_spent: u64,
}

impl TxAgg {
    pub(crate) fn has_shielded_activity(&self) -> bool {
        self.is_shielding
            || self.sapling_received > 0
            || self.sapling_spent > 0
            || self.orchard_received > 0
            || self.orchard_spent > 0
            || self.ironwood_received > 0
            || self.ironwood_spent > 0
            || (self.transparent_received == 0
                && self.transparent_spent == 0
                && (self.received_notes > 0 || self.spent_notes > 0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryOutpoint {
    pub txid: String,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryOutput {
    pub index: u32,
    pub value_zat: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PriorMeta {
    pub(crate) height: u32,
    pub(crate) hash: String,
    pub(crate) sapling_tree_size: u32,
    pub(crate) orchard_tree_size: u32,
    pub(crate) ironwood_tree_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TreeSizeMark {
    height: u32,
    sapling: u64,
    orchard: u64,
    ironwood: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    magic: String,
    network: String,
    account_index: u32,
    ufvk: String,
    /// Near the front: a header peek of a large snapshot must see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hardware: Option<HardwareAccount>,
    unified_address: String,
    transparent_address: Option<String>,
    birthday: u32,
    scanned_height: u32,
    /// Note-derived totals at the front of the JSON so a 512KB header peek can
    /// paint ZEC without waiting for Sinsemilla. Ignored on hydrate (notes win).
    #[serde(default)]
    orchard_available: u64,
    #[serde(default)]
    orchard_pending: u64,
    #[serde(default)]
    total_available: u64,
    #[serde(default)]
    total_pending: u64,
    next_diversifier: u32,
    prior: Option<PriorMeta>,
    notes: Vec<TrackedNote>,
    txs: BTreeMap<String, TxAgg>,
    #[serde(default)]
    spend_notes: Vec<StoredNote>,
    #[serde(default)]
    sapling_leaves: Vec<TreeLeaf>,
    #[serde(default)]
    orchard_leaves: Vec<TreeLeaf>,
    #[serde(default)]
    ironwood_leaves: Vec<TreeLeaf>,
    #[serde(default)]
    utxos: Vec<StoredUtxo>,
    #[serde(default)]
    transparent_scan: Option<ScanCoverage>,
    #[serde(default)]
    transparent_scan_required: bool,
    #[serde(default)]
    memo_scan: Option<ScanCoverage>,
    #[serde(default)]
    block_hashes: BTreeMap<u32, String>,
    #[serde(default)]
    view_only: bool,
    #[serde(default)]
    recent_recipients: Vec<String>,
    #[serde(default)]
    birthday_trees: Option<BirthdayTrees>,
    #[serde(default)]
    sapling_roots: Vec<StoredSubtreeRoot>,
    #[serde(default)]
    orchard_roots: Vec<StoredSubtreeRoot>,
    #[serde(default)]
    ironwood_roots: Vec<StoredSubtreeRoot>,
    #[serde(default)]
    sapling_base: u64,
    #[serde(default)]
    orchard_base: u64,
    #[serde(default)]
    ironwood_base: u64,
    #[serde(default)]
    sapling_next: u64,
    #[serde(default)]
    orchard_next: u64,
    #[serde(default)]
    ironwood_next: u64,
    #[serde(default)]
    trees_ready: bool,
    #[serde(default = "snapshot_version_legacy")]
    version: u32,
    #[serde(default)]
    tree_sizes: Vec<TreeSizeMark>,
    /// Raw bytes of our unmined, unexpired sends (txid hex → tx hex), so a
    /// broadcast whose outcome was unknown can be resubmitted after a reload.
    #[serde(default)]
    pending_txs: BTreeMap<String, String>,
    /// Hashed trees, saved once finalize has built them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live_trees: Option<StoredLiveTrees>,
}

/// Serialize directly from the scan worker's wallet. The owned `Snapshot` above
/// is only the hydration format; constructing it to save used to clone every
/// retained leaf, note, and transaction before allocating the JSON bytes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRef<'a> {
    magic: &'a str,
    network: &'a str,
    account_index: u32,
    ufvk: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware: &'a Option<HardwareAccount>,
    unified_address: &'a str,
    transparent_address: &'a Option<String>,
    birthday: u32,
    scanned_height: u32,
    orchard_available: u64,
    orchard_pending: u64,
    total_available: u64,
    total_pending: u64,
    next_diversifier: u32,
    prior: &'a Option<PriorMeta>,
    notes: &'a [TrackedNote],
    txs: &'a BTreeMap<String, TxAgg>,
    spend_notes: &'a [StoredNote],
    sapling_leaves: SnapshotLeaves<'a>,
    orchard_leaves: SnapshotLeaves<'a>,
    ironwood_leaves: SnapshotLeaves<'a>,
    utxos: &'a [StoredUtxo],
    transparent_scan: &'a Option<ScanCoverage>,
    transparent_scan_required: bool,
    memo_scan: &'a Option<ScanCoverage>,
    block_hashes: &'a BTreeMap<u32, String>,
    view_only: bool,
    recent_recipients: &'a [String],
    birthday_trees: &'a Option<BirthdayTrees>,
    sapling_roots: &'a [StoredSubtreeRoot],
    orchard_roots: &'a [StoredSubtreeRoot],
    ironwood_roots: &'a [StoredSubtreeRoot],
    sapling_base: u64,
    orchard_base: u64,
    ironwood_base: u64,
    sapling_next: u64,
    orchard_next: u64,
    ironwood_next: u64,
    trees_ready: bool,
    version: u32,
    tree_sizes: &'a [TreeSizeMark],
    pending_txs: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_trees: Option<StoredLiveTrees>,
}

struct SnapshotLeaves<'a> {
    retained: &'a [TreeLeaf],
    /// Only the bounded open shard needs a temporary representation.
    buffered: Vec<KeptLeaf>,
}

impl Serialize for SnapshotLeaves<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.retained.len() + self.buffered.len()))?;
        for leaf in self.retained {
            seq.serialize_element(leaf)?;
        }
        for leaf in &self.buffered {
            seq.serialize_element(&kept_to_tree_leaf(leaf.clone()))?;
        }
        seq.end()
    }
}

/// Appends normally have strictly increasing hashed positions. Remember the
/// validated prefix so an empty/new blob never walks the retained history.
#[derive(Default)]
struct LeafValidation {
    len: usize,
    last_position: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TreeFinalizeTick {
    pub done: bool,
    pub hashed: u64,
    pub total: u64,
    pub message: String,
}

const SAPLING_LIVE: u8 = 1;
const ORCHARD_LIVE: u8 = 2;
const IRONWOOD_LIVE: u8 = 4;

/// A hardware signer's account (Keystone, Ledger). The wallet holds only the
/// UFVK; the device holds the keys. The seed fingerprint and account index go
/// into each PCZT's ZIP-32 derivations, which the device checks before signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareAccount {
    /// "keystone" or "ledger".
    pub device: String,
    /// 64 hex digits. Keystone exports its ZIP-32 seed fingerprint; Ledger does
    /// not, so a Ledger account uses `ledger::account_fingerprint` of its
    /// viewing key instead.
    pub seed_fingerprint: String,
    pub account_index: u32,
}

pub struct WebWallet {
    pub(crate) network: Network,
    pub(crate) account_index: u32,
    ufvk: String,
    unified_address: String,
    transparent_address: Option<String>,
    birthday: u32,
    scanned_height: u32,
    pub(crate) next_diversifier: u32,
    pub(crate) prior: Option<PriorMeta>,
    notes: Vec<TrackedNote>,
    txs: BTreeMap<String, TxAgg>,
    pub(crate) spend_notes: Vec<StoredNote>,
    pub(crate) sapling_leaves: Vec<TreeLeaf>,
    pub(crate) orchard_leaves: Vec<TreeLeaf>,
    pub(crate) ironwood_leaves: Vec<TreeLeaf>,
    pub(crate) sapling_tree: SaplingTree,
    pub(crate) orchard_tree: OrchardTree,
    pub(crate) ironwood_tree: OrchardTree,
    pub(crate) next_note_id: u32,
    pub(crate) pending_txs: BTreeMap<TxId, Vec<u8>>,
    pub(crate) utxos: Vec<StoredUtxo>,
    pub(crate) transparent_scan: Option<ScanCoverage>,
    pub(crate) transparent_scan_required: bool,
    pub(crate) memo_scan: Option<ScanCoverage>,
    pub(crate) block_hashes: BTreeMap<u32, String>,
    view_only: bool,
    pub(crate) hardware: Option<HardwareAccount>,
    pub(crate) output_locks: Vec<StoredLock>,
    recent_recipients: Vec<String>,
    birthday_trees: Option<BirthdayTrees>,
    sapling_roots: Vec<StoredSubtreeRoot>,
    orchard_roots: Vec<StoredSubtreeRoot>,
    ironwood_roots: Vec<StoredSubtreeRoot>,
    sapling_base: u64,
    orchard_base: u64,
    ironwood_base: u64,
    sapling_next: u64,
    orchard_next: u64,
    ironwood_next: u64,
    trees_ready: bool,
    tree_sizes: Vec<TreeSizeMark>,
    /// All pools needed for wallet witnesses match scanned commitments. Cold /
    /// rescan starts false; unused pools may remain deferred after finalize.
    pub(crate) sinsemilla_live: bool,
    /// Ephemeral mask of pools actually built by a successful finalize/rebuild.
    /// Note-free pools retain commitments and cursors without hashing catch-up.
    live_pools: u8,
    /// Transient: pools a transaction being built needs besides used ones.
    spend_pools: u8,
    offload: Option<Offload>,
    leaf_validation: [LeafValidation; 3],
    /// A failed mutating scan/tree operation must never replace the last good
    /// persisted snapshot. Reload, rewind, or reset is required to recover.
    scan_error: Option<String>,
    /// Last step-back after a block did not extend our tip: (kept height,
    /// step). A repeat mismatch just above the kept height doubles the step.
    /// In memory only; a reload restarts the search with the first step.
    pub(crate) reorg_probe: Option<(u32, u32)>,
    pub(crate) transparent_scripts: Option<(u32, Vec<(Vec<u8>, String)>)>,
    pub(crate) cached_scan_keys: Option<std::sync::Arc<super::scan::ScanKeys>>,
}

impl WebWallet {
    pub fn from_account(acct: AccountKeys, birthday: u32) -> Result<Self> {
        if birthday == 0 {
            return Err(EngineError::Message("birthday must be >= 1".into()));
        }
        let network = acct.network;
        let ufvk = UnifiedFullViewingKey::decode(&network, &acct.ufvk)
            .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;
        let (_ua, found) = ufvk
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| EngineError::Message(format!("default address: {e:?}")))?;
        let mut next = found;
        let _ = next.increment();
        Ok(Self {
            network,
            account_index: acct.account_index,
            ufvk: acct.ufvk,
            unified_address: acct.unified_address,
            transparent_address: acct.transparent_address,
            birthday,
            scanned_height: birthday.saturating_sub(1),
            next_diversifier: diversifier_u32(next),
            prior: None,
            notes: Vec::new(),
            txs: BTreeMap::new(),
            spend_notes: Vec::new(),
            sapling_leaves: Vec::new(),
            orchard_leaves: Vec::new(),
            ironwood_leaves: Vec::new(),
            sapling_tree: empty_sapling_tree(),
            orchard_tree: empty_orchard_tree(),
            ironwood_tree: empty_orchard_tree(),
            next_note_id: 1,
            pending_txs: BTreeMap::new(),
            utxos: Vec::new(),
            transparent_scan: None,
            transparent_scan_required: false,
            memo_scan: None,
            block_hashes: BTreeMap::new(),
            view_only: false,
            hardware: None,
            output_locks: Vec::new(),
            recent_recipients: Vec::new(),
            birthday_trees: None,
            sapling_roots: Vec::new(),
            orchard_roots: Vec::new(),
            ironwood_roots: Vec::new(),
            sapling_base: 0,
            orchard_base: 0,
            ironwood_base: 0,
            sapling_next: 0,
            orchard_next: 0,
            ironwood_next: 0,
            trees_ready: birthday <= 1,
            tree_sizes: Vec::new(),
            sinsemilla_live: false,
            live_pools: 0,
            spend_pools: 0,
            offload: None,
            leaf_validation: Default::default(),
            scan_error: None,
            reorg_probe: None,
            cached_scan_keys: None,
            transparent_scripts: None,
        })
    }

    /// Watch-only wallet from a UFVK. Scan and history work; prove fails until a matching mnemonic is used.
    pub fn from_ufvk(
        network: Network,
        ufvk: &str,
        birthday: u32,
        account_index: u32,
    ) -> Result<Self> {
        let acct = crate::keys::account_from_ufvk(ufvk, network, account_index)?;
        let mut w = Self::from_account(acct, birthday)?;
        w.view_only = true;
        Ok(w)
    }

    pub fn view_only(&self) -> bool {
        self.view_only
    }

    /// Wallet for a hardware signer's account: its UFVK plus the derivation
    /// the device signs with. Spends go through PCZTs (see `hardware.rs`).
    pub fn from_hardware(
        network: Network,
        ufvk: &str,
        birthday: u32,
        hardware: HardwareAccount,
    ) -> Result<Self> {
        let fp = super::from_hex(&hardware.seed_fingerprint)
            .map_err(|e| EngineError::Message(format!("seed fingerprint: {e}")))?;
        if fp.len() != 32 {
            return Err(EngineError::Message(
                "seed fingerprint must be 32 bytes (64 hex digits)".into(),
            ));
        }
        let device = hardware.device.trim().to_ascii_lowercase();
        match device.as_str() {
            "keystone" => {}
            #[cfg(feature = "hardware")]
            "ledger" => {
                let expected =
                    crate::ledger::account_fingerprint(ufvk.trim(), hardware.account_index);
                if fp != expected {
                    return Err(EngineError::Message(
                        "Ledger seed fingerprint does not match this viewing key (use the one the export returned)".into(),
                    ));
                }
            }
            other => {
                return Err(EngineError::Message(format!(
                    "unknown hardware wallet \"{other}\" (keystone, ledger)"
                )))
            }
        }
        let mut w = Self::from_ufvk(network, ufvk, birthday, hardware.account_index)?;
        w.hardware = Some(HardwareAccount {
            device,
            seed_fingerprint: hardware.seed_fingerprint.to_ascii_lowercase(),
            account_index: hardware.account_index,
        });
        Ok(w)
    }

    pub fn hardware(&self) -> Option<&HardwareAccount> {
        self.hardware.as_ref()
    }

    /// True when birthday frontiers are installed (or birthday is genesis).
    pub fn trees_ready(&self) -> bool {
        self.trees_ready
    }

    /// Trees needed for wallet witnesses match scanned leaves. Follow-on blocks
    /// hash those pools live; unused pools keep deferring until their first note.
    pub fn sinsemilla_live(&self) -> bool {
        self.sinsemilla_live
    }

    /// Compact-block tree sizes at the current scan frontier (birthday GetTreeState or last apply).
    pub(crate) fn frontier_tree_sizes(&self) -> (u32, u32, u32) {
        if let Some(p) = &self.prior {
            return (
                p.sapling_tree_size,
                p.orchard_tree_size,
                p.ironwood_tree_size,
            );
        }
        (
            u32::try_from(self.sapling_next).unwrap_or(u32::MAX),
            u32::try_from(self.orchard_next).unwrap_or(u32::MAX),
            u32::try_from(self.ironwood_next).unwrap_or(u32::MAX),
        )
    }

    /// Complete shards already stored for `sapling` / `orchard` / `ironwood`.
    pub fn subtree_root_count(&self, protocol: &str) -> u32 {
        match protocol {
            "sapling" => self.sapling_roots.len() as u32,
            "orchard" => self.orchard_roots.len() as u32,
            "ironwood" => self.ironwood_roots.len() as u32,
            _ => 0,
        }
    }

    /// First shard whose root this wallet still needs: the stored count, or
    /// the birthday shard once the birthday frontier is installed. Shards
    /// before the birthday never need hashing, and the light server spends
    /// about a second per root, so requesting from shard 0 cannot finish.
    pub fn subtree_roots_start(&self, protocol: &str) -> u32 {
        let (have, base) = match protocol {
            "sapling" => (self.sapling_roots.len() as u64, self.sapling_base),
            "orchard" => (self.orchard_roots.len() as u64, self.orchard_base),
            "ironwood" => (self.ironwood_roots.len() as u64, self.ironwood_base),
            _ => return 0,
        };
        u32::try_from(have.max(offload::shard_of(base))).unwrap_or(u32::MAX)
    }

    /// Matching mnemonic turns a view-only snapshot into a spending wallet (session).
    pub fn attach_seed(&mut self, mnemonic: &str) -> Result<()> {
        if self.hardware.is_some() {
            return Err(EngineError::Message(
                "this is a hardware-wallet account: spends are signed on the device, not with a phrase".into(),
            ));
        }
        let acct = crate::keys::account_from_mnemonic(mnemonic, self.network, self.account_index)?;
        crate::keys::derived_ufvk_covers(self.network, &acct.ufvk, &self.ufvk).map_err(|e| {
            EngineError::Message(format!("{e} (account index {})", self.account_index))
        })?;
        self.view_only = false;
        Ok(())
    }

    /// Rebuild per-pool spent/received from notes + UTXOs. Source of truth for the classifier;
    /// do not persist a derived `type`.
    pub fn recompute_pool_fields(&mut self) {
        let skip_pending: std::collections::HashSet<String> = self
            .txs
            .iter()
            .filter(|(_, a)| a.mined_height.is_none() && (a.spent_notes > 0 || a.is_shielding))
            .map(|(k, _)| k.clone())
            .collect();
        let note_txids: std::collections::HashSet<_> =
            self.notes.iter().map(|note| note.txid.as_str()).collect();
        for (txid, agg) in self.txs.iter_mut() {
            if skip_pending.contains(txid) {
                continue;
            }
            agg.transparent_received = 0;
            agg.transparent_spent = 0;
            // A decrypted mempool receipt has no compact-note row yet. Its
            // pool totals are known from the full transaction and must survive
            // refresh/reload until mined notes become the source of truth.
            let pending_receipt = agg.mined_height.is_none()
                && agg.enhancement_complete
                && !note_txids.contains(txid.as_str());
            if !pending_receipt {
                agg.sapling_received = 0;
                agg.orchard_received = 0;
                agg.ironwood_received = 0;
            }
            agg.sapling_spent = 0;
            agg.orchard_spent = 0;
            agg.ironwood_spent = 0;
        }
        fn bump_recv(agg: &mut TxAgg, pool: &str, v: u64) {
            match pool {
                "sapling" => agg.sapling_received = agg.sapling_received.saturating_add(v),
                "ironwood" => agg.ironwood_received = agg.ironwood_received.saturating_add(v),
                "transparent" => {
                    agg.transparent_received = agg.transparent_received.saturating_add(v)
                }
                _ => agg.orchard_received = agg.orchard_received.saturating_add(v),
            }
        }
        fn bump_spent(agg: &mut TxAgg, pool: &str, v: u64) {
            match pool {
                "sapling" => agg.sapling_spent = agg.sapling_spent.saturating_add(v),
                "ironwood" => agg.ironwood_spent = agg.ironwood_spent.saturating_add(v),
                "transparent" => agg.transparent_spent = agg.transparent_spent.saturating_add(v),
                _ => agg.orchard_spent = agg.orchard_spent.saturating_add(v),
            }
        }
        let notes = self.notes.clone();
        for n in &notes {
            if skip_pending.contains(&n.txid) {
                continue;
            }
            let agg = self.txs.entry(n.txid.clone()).or_default();
            if agg.mined_height.is_none() && n.mined_height > 0 {
                agg.mined_height = Some(n.mined_height);
            }
            bump_recv(agg, &n.pool, n.value_zat);
            if n.spent {
                if let Some(spent_in) = n.spent_in.as_ref() {
                    if skip_pending.contains(spent_in) {
                        continue;
                    }
                    let spent_txid = spent_in.clone();
                    let pool = n.pool.clone();
                    let v = n.value_zat;
                    let height = n.spent_height;
                    let s = self.txs.entry(spent_txid).or_default();
                    bump_spent(s, &pool, v);
                    if s.mined_height.is_none() {
                        s.mined_height = height;
                    }
                }
            }
        }
        let utxos = self.utxos.clone();
        for u in &utxos {
            let agg = self.txs.entry(u.txid.clone()).or_default();
            if agg.mined_height.is_none() && u.height > 0 {
                agg.mined_height = Some(u.height);
            }
            bump_recv(agg, "transparent", u.value_zat);
            agg.received_zat = agg.received_zat.max(
                agg.transparent_received
                    .saturating_add(agg.sapling_received)
                    .saturating_add(agg.orchard_received)
                    .saturating_add(agg.ironwood_received),
            );
            if agg.received_notes == 0 {
                agg.received_notes = 1;
            }
            if u.spent {
                if let Some(spent_in) = u.spent_in.as_ref() {
                    // A pending spender keeps its totals from construction;
                    // adding again on every recompute grew its "sent" amount.
                    if skip_pending.contains(spent_in) {
                        continue;
                    }
                    let spent_txid = spent_in.clone();
                    let v = u.value_zat;
                    let s = self.txs.entry(spent_txid).or_default();
                    bump_spent(s, "transparent", v);
                    s.is_shielding = s.is_shielding
                        || s.orchard_received + s.sapling_received + s.ironwood_received > 0;
                }
            }
        }
    }

    pub fn record_recipient(&mut self, to: &str) {
        let to = to.trim();
        if to.is_empty() {
            return;
        }
        self.recent_recipients.retain(|a| a != to);
        self.recent_recipients.insert(0, to.to_string());
        self.recent_recipients.truncate(8);
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn ufvk(&self) -> &str {
        &self.ufvk
    }

    pub fn unified_address(&self) -> &str {
        &self.unified_address
    }

    pub fn transparent_address(&self) -> Option<&str> {
        self.transparent_address.as_deref()
    }

    pub fn birthday(&self) -> u32 {
        self.birthday
    }

    pub fn scanned_height(&self) -> u32 {
        self.scanned_height
    }

    pub fn account_index(&self) -> u32 {
        self.account_index
    }

    pub fn next_height(&self) -> u32 {
        self.scanned_height.saturating_add(1)
    }

    pub fn decode_ufvk(&self) -> Result<UnifiedFullViewingKey> {
        UnifiedFullViewingKey::decode(&self.network, &self.ufvk)
            .map_err(|e| EngineError::Message(format!("ufvk: {e}")))
    }

    pub fn next_unified_address(&mut self) -> Result<String> {
        let ufvk = self.decode_ufvk()?;
        let mut j = DiversifierIndex::from(self.next_diversifier);
        for _ in 0..32 {
            let (ua, found) = ufvk
                .find_address(j, UnifiedAddressRequest::AllAvailableKeys)
                .map_err(|e| EngineError::Message(format!("next address: {e:?}")))?;
            let encoded = ua.encode(&self.network);
            let mut next = found;
            if next.increment().is_err() {
                return Err(EngineError::Message("diversifier space exhausted".into()));
            }
            j = next;
            self.next_diversifier = diversifier_u32(next);
            if encoded != self.unified_address {
                self.unified_address = encoded;
                self.transparent_address = ua
                    .transparent()
                    .map(|t| zcash_keys::encoding::encode_transparent_address_p(&self.network, t));
                return Ok(self.unified_address.clone());
            }
        }
        Err(EngineError::Message(
            "could not find a new unified address".into(),
        ))
    }

    pub fn to_snapshot(&self) -> Result<Vec<u8>> {
        self.ensure_scan_healthy()?;
        let b = self.balance();
        let snap = SnapshotRef {
            magic: SNAPSHOT_MAGIC,
            network: self.network.as_str(),
            account_index: self.account_index,
            ufvk: &self.ufvk,
            unified_address: &self.unified_address,
            transparent_address: &self.transparent_address,
            birthday: self.birthday,
            scanned_height: self.scanned_height,
            orchard_available: b.orchard_available,
            orchard_pending: b.orchard_pending,
            total_available: b.total_available,
            total_pending: b.total_pending,
            next_diversifier: self.next_diversifier,
            prior: &self.prior,
            notes: &self.notes,
            txs: &self.txs,
            spend_notes: &self.spend_notes,
            sapling_leaves: SnapshotLeaves {
                retained: &self.sapling_leaves,
                buffered: self
                    .offload
                    .as_ref()
                    .map(|o| o.sapling.buffer_kept())
                    .unwrap_or_default(),
            },
            orchard_leaves: SnapshotLeaves {
                retained: &self.orchard_leaves,
                buffered: self
                    .offload
                    .as_ref()
                    .map(|o| o.orchard.buffer_kept())
                    .unwrap_or_default(),
            },
            ironwood_leaves: SnapshotLeaves {
                retained: &self.ironwood_leaves,
                buffered: self
                    .offload
                    .as_ref()
                    .map(|o| o.ironwood.buffer_kept())
                    .unwrap_or_default(),
            },
            utxos: &self.utxos,
            transparent_scan: &self.transparent_scan,
            transparent_scan_required: self.transparent_scan_required,
            memo_scan: &self.memo_scan,
            block_hashes: &self.block_hashes,
            view_only: self.view_only,
            hardware: &self.hardware,
            recent_recipients: &self.recent_recipients,
            birthday_trees: &self.birthday_trees,
            sapling_roots: &self.sapling_roots,
            orchard_roots: &self.orchard_roots,
            ironwood_roots: &self.ironwood_roots,
            sapling_base: self.sapling_base,
            orchard_base: self.orchard_base,
            ironwood_base: self.ironwood_base,
            sapling_next: self.sapling_next,
            orchard_next: self.orchard_next,
            ironwood_next: self.ironwood_next,
            trees_ready: self.trees_ready,
            version: SNAPSHOT_VERSION,
            tree_sizes: &self.tree_sizes,
            pending_txs: self
                .unmined_pending_txs()
                .map(|(key, raw)| (key, to_hex(raw)))
                .collect(),
            live_trees: self.stored_live_trees()?,
        };
        serde_json::to_vec(&snap).map_err(|e| EngineError::Message(format!("snapshot: {e}")))
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let mut snap: Snapshot = serde_json::from_slice(bytes)
            .map_err(|e| EngineError::Message(format!("snapshot parse: {e}")))?;
        if snap.magic != SNAPSHOT_MAGIC {
            return Err(EngineError::Message(format!(
                "unknown snapshot magic {}",
                snap.magic
            )));
        }
        if snap.version > SNAPSHOT_VERSION {
            return Err(EngineError::Message(format!(
                "unsupported snapshot version {}",
                snap.version
            )));
        }
        let legacy_positions = snap.version < 2;
        let live_trees = snap.live_trees.take();
        let network = Network::parse(&snap.network)
            .ok_or_else(|| EngineError::InvalidNetwork(snap.network.clone()))?;
        let mut w = Self {
            network,
            account_index: snap.account_index,
            ufvk: snap.ufvk,
            unified_address: snap.unified_address,
            transparent_address: snap.transparent_address,
            birthday: snap.birthday,
            scanned_height: snap.scanned_height,
            next_diversifier: snap.next_diversifier,
            prior: snap.prior,
            notes: snap.notes,
            txs: snap.txs,
            spend_notes: snap.spend_notes,
            sapling_leaves: snap.sapling_leaves,
            orchard_leaves: snap.orchard_leaves,
            ironwood_leaves: snap.ironwood_leaves,
            sapling_tree: empty_sapling_tree(),
            orchard_tree: empty_orchard_tree(),
            ironwood_tree: empty_orchard_tree(),
            next_note_id: 1,
            pending_txs: snap
                .pending_txs
                .iter()
                .filter_map(|(key, raw)| {
                    Some((Self::txid_from_hex(key).ok()?, super::from_hex(raw).ok()?))
                })
                .collect(),
            utxos: snap.utxos,
            transparent_scan: snap.transparent_scan,
            transparent_scan_required: snap.transparent_scan_required,
            memo_scan: snap.memo_scan,
            block_hashes: snap.block_hashes,
            view_only: snap.view_only,
            hardware: snap.hardware,
            // Reservations belong to PCZTs in the page's memory, which a
            // reload drops; persisting them would only strand the notes.
            output_locks: Vec::new(),
            recent_recipients: snap.recent_recipients,
            birthday_trees: snap.birthday_trees,
            sapling_roots: snap.sapling_roots,
            orchard_roots: snap.orchard_roots,
            ironwood_roots: snap.ironwood_roots,
            sapling_base: snap.sapling_base,
            orchard_base: snap.orchard_base,
            ironwood_base: snap.ironwood_base,
            sapling_next: if snap.sapling_next > 0 {
                snap.sapling_next
            } else {
                snap.sapling_base
            },
            orchard_next: if snap.orchard_next > 0 {
                snap.orchard_next
            } else {
                snap.orchard_base
            },
            ironwood_next: if snap.ironwood_next > 0 {
                snap.ironwood_next
            } else {
                snap.ironwood_base
            },
            trees_ready: snap.trees_ready || snap.birthday <= 1,
            tree_sizes: snap.tree_sizes,
            sinsemilla_live: false,
            live_pools: 0,
            spend_pools: 0,
            offload: None,
            leaf_validation: Default::default(),
            scan_error: None,
            reorg_probe: None,
            cached_scan_keys: None,
            transparent_scripts: None,
        };
        for coverage in [&w.transparent_scan, &w.memo_scan].into_iter().flatten() {
            if coverage.height < w.birthday
                || coverage.height > w.scanned_height
                || coverage.hash.len() != 64
                || !coverage.hash.bytes().all(|b| b.is_ascii_hexdigit())
                || w.hash_at(coverage.height)
                    .is_some_and(|hash| hash != coverage.hash)
            {
                return Err(EngineError::Message(
                    "invalid public scan coverage in snapshot".into(),
                ));
            }
        }
        if legacy_positions {
            w.sapling_next = migrate_legacy_leaf_positions(
                &mut w.sapling_leaves,
                w.sapling_base,
                w.sapling_next,
            );
            w.orchard_next = migrate_legacy_leaf_positions(
                &mut w.orchard_leaves,
                w.orchard_base,
                w.orchard_next,
            );
            w.ironwood_next = migrate_legacy_leaf_positions(
                &mut w.ironwood_leaves,
                w.ironwood_base,
                w.ironwood_next,
            );
        }
        w.assert_consistent_leaf_hashes()?;
        w.repair_tree_cursors();
        w.reconcile_mined_spends();
        w.prune_settled_shards();
        w.next_note_id = w
            .spend_notes
            .iter()
            .map(|n| n.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        // Do not replay shardtrees here — 60MB snapshots spent tens of seconds
        // in rebuild_trees while the overlay said Restoring snapshot. Saved
        // hashed trees load instead; without them catch-up hashes live
        // (selective shard scanning) and finalize_scan_trees / prove rebuilds.
        if let Some(live) = live_trees {
            w.restore_live_trees(&live);
        }
        w.recompute_pool_fields();
        Ok(w)
    }

    fn stored_live_trees(&self) -> Result<Option<StoredLiveTrees>> {
        if !self.sinsemilla_live {
            return Ok(None);
        }
        Ok(Some(StoredLiveTrees {
            pools: self.live_pools,
            scanned_height: self.scanned_height,
            sapling_next: self.sapling_next,
            orchard_next: self.orchard_next,
            ironwood_next: self.ironwood_next,
            sapling: live_trees::save_tree(&self.sapling_tree)?,
            orchard: live_trees::save_tree(&self.orchard_tree)?,
            ironwood: live_trees::save_tree(&self.ironwood_tree)?,
        }))
    }

    /// Use saved hashed trees when they belong to this scan position. Anything
    /// else leaves the wallet to re-hash at its next finalize, as before.
    fn restore_live_trees(&mut self, live: &StoredLiveTrees) -> bool {
        if live.scanned_height != self.scanned_height
            || live.sapling_next != self.sapling_next
            || live.orchard_next != self.orchard_next
            || live.ironwood_next != self.ironwood_next
        {
            return false;
        }
        let loaded = (|| -> Result<(SaplingTree, OrchardTree, OrchardTree)> {
            Ok((
                live_trees::load_tree(&live.sapling, TREE_CHECKPOINTS)?,
                live_trees::load_tree(&live.orchard, TREE_CHECKPOINTS)?,
                live_trees::load_tree(&live.ironwood, TREE_CHECKPOINTS)?,
            ))
        })();
        let Ok((sapling, orchard, ironwood)) = loaded else {
            return false;
        };
        // Every live pool must answer a root at the scanned height.
        let at = BlockHeight::from_u32(self.scanned_height);
        let answers = |bit: u8, root: bool| live.pools & bit == 0 || root;
        if !(answers(
            SAPLING_LIVE,
            matches!(sapling.root_at_checkpoint_id(&at), Ok(Some(_))),
        ) && answers(
            ORCHARD_LIVE,
            matches!(orchard.root_at_checkpoint_id(&at), Ok(Some(_))),
        ) && answers(
            IRONWOOD_LIVE,
            matches!(ironwood.root_at_checkpoint_id(&at), Ok(Some(_))),
        )) {
            return false;
        }
        self.sapling_tree = sapling;
        self.orchard_tree = orchard;
        self.ironwood_tree = ironwood;
        self.live_pools = live.pools;
        self.sinsemilla_live = true;
        true
    }

    pub fn history(&self, limit: usize) -> Vec<HistoryEntry> {
        self.history_matching(limit, None, None)
    }

    /// Filter completion before limiting, so older transactions eventually get
    /// enhanced after the newest page has been processed.
    pub fn memo_enhancement_txids(&self, limit: usize) -> Vec<String> {
        let mut pending: Vec<_> = self
            .txs
            .iter()
            .filter(|(_, agg)| {
                (!agg.enhancement_complete || !agg.history_metadata_complete)
                    && agg.has_shielded_activity()
            })
            .collect();
        pending.sort_by(|(a_id, a), (b_id, b)| {
            b.mined_height
                .unwrap_or(u32::MAX)
                .cmp(&a.mined_height.unwrap_or(u32::MAX))
                .then_with(|| b_id.cmp(a_id))
        });
        pending
            .into_iter()
            .take(limit.min(500))
            .map(|(txid, _)| Self::display_txid(txid))
            .collect()
    }

    pub fn history_matching(
        &self,
        limit: usize,
        status: Option<crate::HistoryStatusFilter>,
        txid: Option<&str>,
    ) -> Vec<HistoryEntry> {
        history::entries(&self.txs, limit, self.scanned_height, status, txid)
    }

    pub fn history_json(&self, limit: usize) -> serde_json::Value {
        serde_json::Value::Array(self.history(limit).iter().map(|e| e.to_json()).collect())
    }

    pub fn history_query_json(
        &self,
        limit: usize,
        status: Option<&str>,
        txid: Option<&str>,
    ) -> serde_json::Value {
        let status = status.and_then(crate::HistoryStatusFilter::parse);
        serde_json::Value::Array(
            self.history_matching(limit, status, txid)
                .iter()
                .map(|e| e.to_json())
                .collect(),
        )
    }

    pub fn balance(&self) -> Balance {
        use zcash_client_backend::data_api::wallet::TargetHeight;
        let policy = crate::confirmations_policy(self.network);
        let target = TargetHeight::from(self.scanned_height.saturating_add(1));
        let mut sapling = 0u64;
        let mut orchard = 0u64;
        let mut ironwood = 0u64;
        let mut t = 0u64;
        let mut sapling_p = 0u64;
        let mut orchard_p = 0u64;
        let mut ironwood_p = 0u64;
        let mut t_p = 0u64;
        let add = |avail: &mut u64, pend: &mut u64, v: u64, ready: bool| {
            if ready {
                *avail = avail.saturating_add(v);
            } else {
                *pend = pend.saturating_add(v);
            }
        };
        for u in &self.utxos {
            if u.spent {
                continue;
            }
            add(
                &mut t,
                &mut t_p,
                u.value_zat,
                self.utxo_ready(u, target, &policy),
            );
        }
        for n in &self.notes {
            if n.spent {
                continue;
            }
            let ready = self.note_ready(n, target, &policy);
            match n.pool.as_str() {
                "sapling" => add(&mut sapling, &mut sapling_p, n.value_zat, ready),
                "ironwood" => add(&mut ironwood, &mut ironwood_p, n.value_zat, ready),
                _ => add(&mut orchard, &mut orchard_p, n.value_zat, ready),
            }
        }
        // Unmined sends/shields: change isn't in `notes` until the compact block is scanned.
        for (txid, agg) in &self.txs {
            if agg.mined_height.is_some() || agg.received_zat == 0 || self.expired_unmined(agg) {
                continue;
            }
            if self.notes.iter().any(|n| n.txid == *txid) {
                continue;
            }
            sapling_p = sapling_p.saturating_add(agg.sapling_received);
            orchard_p = orchard_p.saturating_add(agg.orchard_received);
            ironwood_p = ironwood_p.saturating_add(agg.ironwood_received);
            // Old snapshots did not classify unmined change by pool. Preserve
            // their Orchard fallback only for the still-unclassified amount;
            // transparent UTXOs have already contributed above.
            let classified = agg
                .sapling_received
                .saturating_add(agg.orchard_received)
                .saturating_add(agg.ironwood_received)
                .saturating_add(agg.transparent_received);
            orchard_p = orchard_p.saturating_add(agg.received_zat.saturating_sub(classified));
        }
        let total_available = sapling
            .saturating_add(orchard)
            .saturating_add(ironwood)
            .saturating_add(t);
        let total_pending = sapling_p
            .saturating_add(orchard_p)
            .saturating_add(ironwood_p)
            .saturating_add(t_p);
        Balance {
            sapling_available: sapling,
            orchard_available: orchard,
            ironwood_available: ironwood,
            transparent_available: t,
            total_available,
            sapling_pending: sapling_p,
            orchard_pending: orchard_p,
            ironwood_pending: ironwood_p,
            transparent_pending: t_p,
            total_pending,
        }
    }

    fn note_ready(
        &self,
        n: &TrackedNote,
        target: zcash_client_backend::data_api::wallet::TargetHeight,
        policy: &zcash_client_backend::data_api::wallet::ConfirmationsPolicy,
    ) -> bool {
        let need = if n.is_change {
            u32::from(policy.trusted())
        } else {
            u32::from(policy.untrusted())
        };
        n.mined_height.saturating_add(need) <= u32::from(target)
    }

    pub(crate) fn utxo_ready(
        &self,
        u: &StoredUtxo,
        target: zcash_client_backend::data_api::wallet::TargetHeight,
        policy: &zcash_client_backend::data_api::wallet::ConfirmationsPolicy,
    ) -> bool {
        if self.transparent_scan_required
            && self
                .transparent_scan
                .as_ref()
                .is_none_or(|s| s.height != self.scanned_height)
        {
            return false;
        }
        if u.coinbase {
            return u.height > 0 && u.height.saturating_add(100) <= u32::from(target);
        }
        #[cfg(feature = "transparent-inputs")]
        if policy.allow_zero_conf_shielding() {
            return true;
        }
        let need = u32::from(policy.untrusted());
        let tip = u32::from(target);
        tip > u.height && tip.saturating_sub(u.height) >= need
    }

    pub fn wallet_snapshot(&self, server: &str) -> serde_json::Value {
        let b = self.balance();
        let zip = zip321_uri(&self.unified_address, None).unwrap_or_default();
        let policy = crate::confirmations_policy(self.network);
        #[cfg(feature = "transparent-inputs")]
        let zero_conf_shield = policy.allow_zero_conf_shielding();
        #[cfg(not(feature = "transparent-inputs"))]
        let zero_conf_shield = false;
        serde_json::json!({
            "network": self.network.as_str(),
            "server": server,
            "birthdayHeight": self.birthday,
            "unifiedAddress": self.unified_address,
            "transparentAddress": self.transparent_address,
            "zip321": zip,
            "scannedHeight": self.scanned_height,
            "transparentScanHeight": self.transparent_scan.as_ref().map(|s| s.height),
            "transparentScanComplete": self.transparent_scan.as_ref().is_some_and(|s| s.height == self.scanned_height),
            "memoScanHeight": self.memo_scan.as_ref().map(|s| s.height),
            "treesReady": self.trees_ready,
            // Witnesses exist AND there is an unspent orchard/ironwood note. Empty
            // catch-up (no owned notes) stays false even when treesReady is true.
            "spendReady": self.scan_error.is_none() && self.trees_ready
                && self.spend_notes.iter().any(|n| !n.spent && n.pool != "sapling"),
            // A hardware wallet spends through its device, so it is not view-only.
            "viewOnly": self.view_only && self.hardware.is_none(),
            "hardware": self.hardware,
            "ufvk": self.ufvk,
            "recentRecipients": self.recent_recipients,
            "maxSendZat": crate::max_send_zat(b.orchard_available + b.ironwood_available),
            "maxSendZec": format_zatoshis(crate::max_send_zat(
                b.orchard_available + b.ironwood_available
            )),
            "confirmations": {
                "trusted": u32::from(policy.trusted()),
                "untrusted": u32::from(policy.untrusted()),
                "zeroConfShield": zero_conf_shield,
            },
            "transactions": self.history(40).iter().map(|e| e.to_json()).collect::<Vec<_>>(),
            "balance": {
                "saplingAvailable": b.sapling_available,
                "orchardAvailable": b.orchard_available,
                "ironwoodAvailable": b.ironwood_available,
                "transparentAvailable": b.transparent_available,
                "totalAvailable": b.total_available,
                "saplingPending": b.sapling_pending,
                "orchardPending": b.orchard_pending,
                "ironwoodPending": b.ironwood_pending,
                "transparentPending": b.transparent_pending,
                "totalPending": b.total_pending,
                "saplingZec": format_zatoshis(b.sapling_available),
                "orchardZec": format_zatoshis(b.orchard_available),
                "ironwoodZec": format_zatoshis(b.ironwood_available),
                "transparentZec": format_zatoshis(b.transparent_available),
                "totalZec": format_zatoshis(b.total_available),
                "pendingZec": format_zatoshis(b.total_pending),
                "orchardPendingZec": format_zatoshis(b.orchard_pending),
                "transparentPendingZec": format_zatoshis(b.transparent_pending),
            }
        })
    }

    pub(crate) fn notes(&self) -> &[TrackedNote] {
        &self.notes
    }

    pub(crate) fn txs(&self) -> &BTreeMap<String, TxAgg> {
        &self.txs
    }

    pub(crate) fn notes_mut(&mut self) -> &mut Vec<TrackedNote> {
        &mut self.notes
    }

    /// Finalize omits unused pools. The first owned output in such a pool needs
    /// its previously retained siblings rebuilt before it can have a witness.
    pub(crate) fn prepare_received_pool(&mut self, pool: &str) {
        let bit = match pool {
            "sapling" => SAPLING_LIVE,
            "orchard" => ORCHARD_LIVE,
            "ironwood" => IRONWOOD_LIVE,
            _ => 0,
        };
        if self.sinsemilla_live && self.live_pools & bit == 0 {
            self.invalidate_live_trees();
        }
    }

    fn invalidate_live_trees(&mut self) {
        self.sinsemilla_live = false;
        self.live_pools = 0;
    }

    fn used_pools(&self) -> u8 {
        [
            (SAPLING_LIVE, "sapling"),
            (ORCHARD_LIVE, "orchard"),
            (IRONWOOD_LIVE, "ironwood"),
        ]
        .into_iter()
        .filter(|(_, pool)| self.pool_needed(pool))
        .fold(0, |mask, (bit, _)| mask | bit)
    }

    /// The pool that receives payments to Orchard receivers and shielded change
    /// at the next height: Ironwood once NU6.3 is active, Orchard before. The
    /// builder needs its root at the anchor even without a note of ours there.
    fn output_pool(&self) -> &'static str {
        let next = BlockHeight::from_u32(self.scanned_height.saturating_add(1));
        if self.network.is_nu_active(NetworkUpgrade::Nu6_3, next) {
            "ironwood"
        } else {
            "orchard"
        }
    }

    /// Trees built, checkpointed and required at the spend anchor: pools with
    /// our notes, plus the output pool while a transaction is being built.
    fn pool_needed(&self, pool: &str) -> bool {
        let (bit, leaves) = match pool {
            "sapling" => (SAPLING_LIVE, &self.sapling_leaves),
            "orchard" => (ORCHARD_LIVE, &self.orchard_leaves),
            _ => (IRONWOOD_LIVE, &self.ironwood_leaves),
        };
        self.spend_pools & bit != 0 || pool_used(leaves, &self.notes, &self.spend_notes, pool)
    }

    /// Finalize before building a transaction. Scanning keeps deferring pools
    /// without our notes; a spend also needs the output pool's anchor root.
    pub(crate) fn finalize_for_spend(&mut self) -> Result<()> {
        self.spend_pools = match self.output_pool() {
            "ironwood" => IRONWOOD_LIVE,
            _ => ORCHARD_LIVE,
        };
        let result = self.finalize_scan_trees();
        self.spend_pools = 0;
        result
    }

    pub(crate) fn txs_mut(&mut self) -> &mut BTreeMap<String, TxAgg> {
        &mut self.txs
    }

    pub(crate) fn set_scanned(&mut self, height: u32, prior: PriorMeta) -> Result<()> {
        self.scanned_height = height;
        self.block_hashes.insert(height, prior.hash.clone());
        while self.block_hashes.len() > HASH_KEEP {
            if let Some(k) = self.block_hashes.keys().next().copied() {
                self.block_hashes.remove(&k);
            } else {
                break;
            }
        }
        self.prior = Some(prior);
        self.record_tree_size(height);
        self.release_expired_spends();
        if let Some(offload) = &mut self.offload {
            // Keep the authentic buffered boundary needed by this height,
            // including empty blocks, before a later feed can drop its shard.
            // The leaf's mined height remains unchanged for snapshot rewind.
            let height = BlockHeight::from_u32(height);
            offload.sapling.retain_scanned_boundary(height);
            offload.orchard.retain_scanned_boundary(height);
            offload.ironwood.retain_scanned_boundary(height);
        }
        // Empty heights are real anchors too. Retain their exact boundary
        // before the next append can compact it into a larger terminal node.
        if self.sinsemilla_live {
            let height = BlockHeight::from_u32(height);
            if self.live_pools & SAPLING_LIVE != 0 {
                checkpoint_required(&mut self.sapling_tree, height, self.sapling_next)?;
            }
            if self.live_pools & ORCHARD_LIVE != 0 {
                checkpoint_required(&mut self.orchard_tree, height, self.orchard_next)?;
            }
            if self.live_pools & IRONWOOD_LIVE != 0 {
                checkpoint_required(&mut self.ironwood_tree, height, self.ironwood_next)?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_scan_window(&mut self, last_height: u32) {
        if self.ensure_offload() {
            self.offload
                .as_mut()
                .expect("just ensured")
                .retain_checkpoints_from(offload::keep_from_height(last_height));
        }
    }

    fn recent_boundaries(&self, pool: &str) -> BTreeMap<u64, BlockHeight> {
        let floor = u32::from(offload::keep_from_height(self.scanned_height));
        self.tree_sizes
            .iter()
            .filter(|mark| mark.height >= floor && mark.height <= self.scanned_height)
            .filter_map(|mark| {
                let size = match pool {
                    "sapling" => mark.sapling,
                    "orchard" => mark.orchard,
                    _ => mark.ironwood,
                };
                size.checked_sub(1)
                    .map(|position| (position, BlockHeight::from_u32(mark.height)))
            })
            .collect()
    }

    pub(crate) fn spend_anchor_height(&self, maximum: BlockHeight) -> Option<BlockHeight> {
        if !self.sinsemilla_live || self.scan_error.is_some() {
            return None;
        }
        let used = self.live_pools;
        let floor = u32::from(offload::keep_from_height(self.scanned_height)).max(self.birthday);
        (floor..=u32::from(maximum)).rev().find_map(|height| {
            let h = BlockHeight::from_u32(height);
            let available = (used & SAPLING_LIVE == 0
                || self
                    .sapling_tree
                    .root_at_checkpoint_id(&h)
                    .ok()
                    .flatten()
                    .is_some())
                && (used & ORCHARD_LIVE == 0
                    || self
                        .orchard_tree
                        .root_at_checkpoint_id(&h)
                        .ok()
                        .flatten()
                        .is_some())
                && (used & IRONWOOD_LIVE == 0
                    || self
                        .ironwood_tree
                        .root_at_checkpoint_id(&h)
                        .ok()
                        .flatten()
                        .is_some());
            // Shielding needs a target/confirmation height but no shielded
            // input root. Keep unused nonempty pools deferred in that case.
            let known = used != 0
                || height == self.scanned_height
                || self.tree_sizes.iter().any(|m| m.height == height);
            (available && known).then_some(h)
        })
    }

    fn record_tree_size(&mut self, height: u32) {
        let mark = TreeSizeMark {
            height,
            sapling: self.sapling_next,
            orchard: self.orchard_next,
            ironwood: self.ironwood_next,
        };
        if let Some(last) = self.tree_sizes.last_mut() {
            if last.height == height {
                *last = mark;
                return;
            }
        }
        self.tree_sizes.push(mark);
        let extra = self.tree_sizes.len().saturating_sub(HASH_KEEP);
        if extra > 0 {
            self.tree_sizes.drain(..extra);
        }
    }

    fn absorb_offload_buffer(&mut self) {
        let Some(off) = self.offload.take() else {
            return;
        };
        self.sapling_leaves
            .extend(off.sapling.buffer_kept().into_iter().map(kept_to_tree_leaf));
        self.orchard_leaves
            .extend(off.orchard.buffer_kept().into_iter().map(kept_to_tree_leaf));
        self.ironwood_leaves.extend(
            off.ironwood
                .buffer_kept()
                .into_iter()
                .map(kept_to_tree_leaf),
        );
    }

    /// A complete shard's leaves only witness its unspent notes and let a
    /// rewind land inside it. Rewinds stop at the oldest saved block hash or
    /// restart from the birthday, so once the shard's subtree root is known,
    /// every leaf is older than that hash and every note in it was spent below
    /// it, the root alone stands in for the shard. Otherwise a restore carries
    /// shards of long-spent notes and shards that completed near a blob's end
    /// in every snapshot, and finalize hashes the spent-note ones.
    pub(crate) fn prune_settled_shards(&mut self) {
        let Some(&floor) = self.block_hashes.keys().next() else {
            return;
        };
        let notes = &self.notes;
        // Sapling notes carry no position, so a marked leaf is matched by pool
        // and height: every note of that pool mined there must be settled.
        let settled_at = |pool: &str, height: u32| {
            let mut found = false;
            for n in notes
                .iter()
                .filter(|n| n.pool == pool && n.mined_height == height)
            {
                if !(n.spent && n.spent_height.is_some_and(|h| h < floor)) {
                    return false;
                }
                found = true;
            }
            found
        };
        let pools = [
            (
                settled_shards(
                    &self.sapling_leaves,
                    &self.sapling_roots,
                    self.sapling_next,
                    floor,
                    |h| settled_at("sapling", h),
                ),
                0,
            ),
            (
                settled_shards(
                    &self.orchard_leaves,
                    &self.orchard_roots,
                    self.orchard_next,
                    floor,
                    |h| settled_at("orchard", h),
                ),
                1,
            ),
            (
                settled_shards(
                    &self.ironwood_leaves,
                    &self.ironwood_roots,
                    self.ironwood_next,
                    floor,
                    |h| settled_at("ironwood", h),
                ),
                2,
            ),
        ];
        for (shards, i) in pools {
            if shards.is_empty() {
                continue;
            }
            let leaves = match i {
                0 => &mut self.sapling_leaves,
                1 => &mut self.orchard_leaves,
                _ => &mut self.ironwood_leaves,
            };
            let before = leaves.len();
            leaves
                .retain(|l| l.hash.is_empty() || !shards.contains(&offload::shard_of(l.position)));
            // Removal keeps order, so a fully validated prefix stays validated.
            let validation = &mut self.leaf_validation[i];
            if validation.len == before {
                validation.len = leaves.len();
            } else {
                *validation = LeafValidation::default();
            }
        }
    }

    fn selective_scan_sparse(&self) -> bool {
        offload::is_sparse_leaves(
            hashed_leaf_count(&self.sapling_leaves),
            self.sapling_base,
            self.sapling_next,
        ) || offload::is_sparse_leaves(
            hashed_leaf_count(&self.orchard_leaves),
            self.orchard_base,
            self.orchard_next,
        ) || offload::is_sparse_leaves(
            hashed_leaf_count(&self.ironwood_leaves),
            self.ironwood_base,
            self.ironwood_next,
        )
    }

    fn rewind_keep_height(&self, requested: u32, birthday_bound: u32) -> u32 {
        if requested < self.birthday {
            return birthday_bound;
        }
        if self.block_hashes.contains_key(&requested) {
            return requested;
        }
        self.block_hashes
            .keys()
            .rev()
            .copied()
            .find(|&h| h <= requested && h >= self.birthday)
            .unwrap_or(birthday_bound)
    }

    /// GetTreeState / `install_frontiers` used to clobber `*_next` back to the
    /// birthday frontier after a long scan. Later compact apply and prove
    /// then hashed tip commitments on top of birthday leaves (level-0 conflict).
    fn repair_tree_cursors(&mut self) {
        if let Some(m) = self.tree_sizes.last() {
            self.sapling_next = self.sapling_next.max(m.sapling);
            self.orchard_next = self.orchard_next.max(m.orchard);
            self.ironwood_next = self.ironwood_next.max(m.ironwood);
        }
        if let Some(p) = max_hashed_position(&self.sapling_leaves) {
            self.sapling_next = self.sapling_next.max(p.saturating_add(1));
        }
        if let Some(p) = max_hashed_position(&self.orchard_leaves) {
            self.orchard_next = self.orchard_next.max(p.saturating_add(1));
        }
        if let Some(p) = max_hashed_position(&self.ironwood_leaves) {
            self.ironwood_next = self.ironwood_next.max(p.saturating_add(1));
        }
    }

    pub(crate) fn assert_consistent_leaf_hashes(&mut self) -> Result<()> {
        let result = (|| {
            self.leaf_validation[0].validate(&mut self.sapling_leaves, "sapling")?;
            self.leaf_validation[1].validate(&mut self.orchard_leaves, "orchard")?;
            self.leaf_validation[2].validate(&mut self.ironwood_leaves, "ironwood")?;
            Ok(())
        })();
        self.finish_scan_update(result)
    }

    pub(crate) fn ensure_scan_healthy(&self) -> Result<()> {
        match &self.scan_error {
            Some(error) => Err(EngineError::Message(format!(
                "scan state is invalid after: {error}; reload the last saved wallet or wipe scan and resync"
            ))),
            None => Ok(()),
        }
    }

    pub(crate) fn finish_scan_update<T>(&mut self, result: Result<T>) -> Result<T> {
        // A reorg is reported only after a completed rewind, before anything
        // of the rejected block was applied: the wallet is consistent at the
        // restart height. A fork below every saved hash poisons explicitly.
        if let Err(error) = &result {
            if !matches!(error, EngineError::Reorg { .. }) {
                self.scan_error = Some(error.to_string());
                self.invalidate_live_trees();
            }
        }
        result
    }

    /// Poison the session after a fork deeper than the saved block hashes.
    /// The wording avoids "reorg": the sync treats that as a restart request,
    /// and this state can only be left by reloading or wiping the scan.
    pub(crate) fn fail_close_deep_fork(&mut self, height: u32) {
        self.scan_error = Some(format!(
            "chain fork at {height} is below every saved block hash"
        ));
        self.invalidate_live_trees();
    }

    /// Only a pre-scan wallet should take tree sizes from GetTreeState.
    fn should_adopt_frontier_sizes(&self) -> bool {
        self.scanned_height < self.birthday
            && hashed_leaf_count(&self.sapling_leaves) == 0
            && hashed_leaf_count(&self.orchard_leaves) == 0
            && hashed_leaf_count(&self.ironwood_leaves) == 0
    }

    fn restore_next_after_rewind(&mut self, keep: u32) {
        if keep < self.birthday {
            self.sapling_next = self.sapling_base;
            self.orchard_next = self.orchard_base;
            self.ironwood_next = self.ironwood_base;
            self.tree_sizes.clear();
            return;
        }
        self.tree_sizes.retain(|m| m.height <= keep);
        if let Some(m) = self.tree_sizes.last() {
            self.sapling_next = m.sapling;
            self.orchard_next = m.orchard;
            self.ironwood_next = m.ironwood;
            return;
        }
        if self.selective_scan_sparse() {
            self.sapling_next = self.sapling_base;
            self.orchard_next = self.orchard_base;
            self.ironwood_next = self.ironwood_base;
            return;
        }
        self.sapling_next = self.sapling_base + hashed_leaf_count(&self.sapling_leaves);
        self.orchard_next = self.orchard_base + hashed_leaf_count(&self.orchard_leaves);
        self.ironwood_next = self.ironwood_base + hashed_leaf_count(&self.ironwood_leaves);
    }

    pub(crate) fn hash_at(&self, height: u32) -> Option<&str> {
        self.block_hashes.get(&height).map(String::as_str)
    }

    /// Drop scanned state above `max_height` (inclusive keep). Trees rebuild from remaining leaves.
    pub fn rewind_to_height(&mut self, max_height: u32) -> Result<u32> {
        // A successful rewind discards the failed suffix and reconstructs every
        // tree. Only this and reset may recover without reloading a snapshot.
        self.scan_error = None;
        self.invalidate_live_trees();
        let result = self.rewind_to_height_inner(max_height);
        self.finish_scan_update(result)
    }

    fn rewind_to_height_inner(&mut self, max_height: u32) -> Result<u32> {
        let requested = max_height.min(self.scanned_height);
        let birthday_bound = self.birthday.saturating_sub(1);
        self.absorb_offload_buffer();
        // Nearest stored hash at or below the request. HASH_KEEP is ~2k blocks —
        // fail-closing to birthday from a missing mark used to drop ~100k and
        // persist that as a new snapshot.
        let keep = self.rewind_keep_height(requested, birthday_bound);
        // Our own sends mined above `keep` left this chain but may still be
        // in the mempool and mine again. Like SQLite wallets, un-mine them:
        // they stay pending and keep their inputs until expiry releases them.
        // Without a known expiry nothing could release those inputs, so other
        // spends above `keep` are forgotten and rediscovered by the rescan.
        let unmined_sends: BTreeSet<String> = self
            .txs
            .iter()
            .filter(|(_, agg)| {
                agg.mined_height.is_some_and(|h| h > keep)
                    && agg.expiry_height.is_some_and(|expiry| expiry > 0)
                    && (agg.spent_notes > 0 || agg.is_shielding)
            })
            .map(|(txid, _)| txid.clone())
            .collect();
        let stays_pending =
            |txid: &Option<String>| txid.as_ref().is_some_and(|t| unmined_sends.contains(t));
        self.notes.retain(|n| n.mined_height <= keep);
        for n in &mut self.notes {
            if n.spent_height.is_some_and(|h| h > keep) {
                n.spent_height = None;
                if !stays_pending(&n.spent_in) {
                    n.spent = false;
                    n.spent_in = None;
                }
            }
        }
        self.spend_notes.retain(|n| n.mined_height <= keep);
        for n in &mut self.spend_notes {
            if n.spent && !stays_pending(&n.spent_in) {
                if let Some(txid) = &n.spent_in {
                    let mined = self.txs.get(txid).and_then(|t| t.mined_height);
                    if mined.is_some_and(|h| h > keep) {
                        n.spent = false;
                        n.spent_in = None;
                    }
                }
            }
        }
        self.utxos.retain(|u| u.height == 0 || u.height <= keep);
        for u in &mut self.utxos {
            if u.spent && !stays_pending(&u.spent_in) {
                if let Some(txid) = &u.spent_in {
                    let mined = self.txs.get(txid).and_then(|t| t.mined_height);
                    if mined.is_none_or(|h| h > keep) && mined.is_some() {
                        u.spent = false;
                        u.spent_in = None;
                    }
                }
            }
        }
        for txid in &unmined_sends {
            if let Some(agg) = self.txs.get_mut(txid) {
                agg.mined_height = None;
                agg.block_time = None;
            }
        }
        self.txs
            .retain(|_, agg| agg.mined_height.map(|h| h <= keep).unwrap_or(true));
        self.sapling_leaves.retain(|l| l.height <= keep);
        self.orchard_leaves.retain(|l| l.height <= keep);
        self.ironwood_leaves.retain(|l| l.height <= keep);
        self.leaf_validation = Default::default();
        self.restore_next_after_rewind(keep);
        self.repair_tree_cursors();
        self.block_hashes.retain(|&h, _| h <= keep);
        let txs = &self.txs;
        self.pending_txs.retain(|id, _| {
            txs.get(&to_hex(id.as_ref()))
                .is_some_and(|agg| agg.mined_height.is_none())
        });
        if keep < self.birthday {
            self.scanned_height = birthday_bound;
            self.prior = None;
        } else if let Some(hash) = self.block_hashes.get(&keep).cloned() {
            self.scanned_height = keep;
            let sapling_sz = self.sapling_next as u32;
            let orchard_sz = self.orchard_next as u32;
            let ironwood_sz = self.ironwood_next as u32;
            self.prior = Some(PriorMeta {
                height: keep,
                hash,
                sapling_tree_size: sapling_sz,
                orchard_tree_size: orchard_sz,
                ironwood_tree_size: ironwood_sz,
            });
        } else {
            self.scanned_height = birthday_bound;
            self.prior = None;
            self.restore_next_after_rewind(birthday_bound);
        }
        self.rewind_public_coverage();
        self.rebuild_trees()?;
        Ok(self.scanned_height)
    }

    /// Start from an earlier birthday without changing account identity. Build the
    /// replacement first, so validation failure leaves the current wallet intact.
    pub fn rescan_from(&mut self, birthday: u32) -> Result<()> {
        if birthday == 0 {
            return Err(EngineError::Message("invalid birthday height".into()));
        }
        if birthday > self.birthday {
            return Err(EngineError::Message("rescan later birthday".into()));
        }
        if self.unmined_pending_txs().next().is_some() {
            return Err(EngineError::Message("rescan pending payment".into()));
        }
        let mut fresh = Self::from_ufvk(self.network, &self.ufvk, birthday, self.account_index)?;
        fresh.unified_address = self.unified_address.clone();
        fresh.transparent_address = self.transparent_address.clone();
        fresh.next_diversifier = self.next_diversifier;
        fresh.view_only = self.view_only;
        fresh.hardware = self.hardware.clone();
        fresh.recent_recipients = self.recent_recipients.clone();
        fresh.transparent_scan_required = self.transparent_scan_required;
        *self = fresh;
        Ok(())
    }

    /// Drop notes, trees, UTXOs, and history. Keep UFVK, birthday, and addresses.
    /// Next `sync` trial-decrypts from birthday.
    pub fn reset_scan(&mut self) {
        self.scan_error = None;
        self.invalidate_live_trees();
        self.leaf_validation = Default::default();
        self.scanned_height = self.birthday.saturating_sub(1);
        self.prior = None;
        self.notes.clear();
        self.txs.clear();
        self.spend_notes.clear();
        self.sapling_leaves.clear();
        self.orchard_leaves.clear();
        self.ironwood_leaves.clear();
        self.sapling_tree = empty_sapling_tree();
        self.orchard_tree = empty_orchard_tree();
        self.ironwood_tree = empty_orchard_tree();
        self.sapling_next = self.sapling_base;
        self.orchard_next = self.orchard_base;
        self.ironwood_next = self.ironwood_base;
        self.next_note_id = 1;
        self.pending_txs.clear();
        self.utxos.clear();
        self.transparent_scan = None;
        self.memo_scan = None;
        self.block_hashes.clear();
        self.tree_sizes.clear();
        self.offload = None;
        let _ = self.rebuild_trees();
        // Empty rebuilt trees are not a historic Merkle; next sync should defer.
        self.invalidate_live_trees();
    }

    /// Undo a send/shield that never mined (broadcast failed or we never submitted).
    /// Unmined, with a nonzero expiry height the scanned chain has passed: it
    /// can no longer be mined (zero never expires).
    /// Raw bytes of our sends still waiting to be mined, keyed like `txs`.
    fn unmined_pending_txs(&self) -> impl Iterator<Item = (String, &Vec<u8>)> + '_ {
        self.pending_txs.iter().filter_map(|(id, raw)| {
            let key = to_hex(id.as_ref());
            let agg = self.txs.get(&key)?;
            (agg.mined_height.is_none() && !self.expired_unmined(agg)).then_some((key, raw))
        })
    }

    /// Hex of every unmined, unexpired send, for an idempotent rebroadcast.
    pub fn pending_raw_txs(&self) -> Vec<String> {
        self.unmined_pending_txs()
            .map(|(_, raw)| to_hex(raw))
            .collect()
    }

    fn expired_unmined(&self, agg: &TxAgg) -> bool {
        agg.mined_height.is_none()
            && agg
                .expiry_height
                .is_some_and(|expiry| expiry > 0 && expiry <= self.scanned_height)
    }

    /// Return inputs reserved by our transactions that expired unmined, as
    /// SQLite wallets do. Otherwise a rejected or evicted send locks its notes
    /// forever. The history row stays and shows the transaction as expired.
    fn release_expired_spends(&mut self) {
        let expired: Vec<String> = self
            .txs
            .iter()
            .filter(|(_, agg)| self.expired_unmined(agg))
            .map(|(txid, _)| txid.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        // Spent on chain by another transaction: never spendable again.
        let mined_spent: BTreeSet<String> = self
            .notes
            .iter()
            .filter(|n| n.spent_height.is_some() && !n.nf.is_empty())
            .map(|n| n.nf.clone())
            .collect();
        for txid in expired {
            let id = Some(txid.as_str());
            for n in &mut self.notes {
                if n.spent_in.as_deref() == id && n.spent_height.is_none() {
                    n.spent = false;
                    n.spent_in = None;
                }
            }
            for n in &mut self.spend_notes {
                if n.spent_in.as_deref() == id && !mined_spent.contains(&n.nf) {
                    n.spent = false;
                    n.spent_in = None;
                }
            }
            for u in &mut self.utxos {
                if u.spent_in.as_deref() == id {
                    u.spent = false;
                    u.spent_in = None;
                }
            }
            if let Ok(tx) = Self::txid_from_hex(&txid) {
                self.pending_txs.remove(&tx);
            }
        }
    }

    pub fn abandon_unmined(&mut self, txid: &str) -> Result<bool> {
        // Public IDs use node/explorer display order; existing snapshots and
        // spend references keep wire order. Convert exactly once: a fallback
        // lookup of the public string could abandon a different transaction.
        let id = TxId::from_hex(txid.trim())
            .ok_or_else(|| EngineError::Message("txid must be 64 hexadecimal digits".into()))?;
        let wire = to_hex(id.as_ref());
        let txid = wire.as_str();
        let Some(agg) = self.txs.get(txid) else {
            return Ok(false);
        };
        if agg.mined_height.is_some() {
            return Err(EngineError::Message(
                "cannot abandon a mined transaction".into(),
            ));
        }
        for n in &mut self.notes {
            if n.spent_in.as_deref() == Some(txid) {
                n.spent = false;
                n.spent_in = None;
                n.spent_height = None;
            }
        }
        for n in &mut self.spend_notes {
            if n.spent_in.as_deref() == Some(txid) {
                n.spent = false;
                n.spent_in = None;
            }
        }
        for u in &mut self.utxos {
            if u.spent_in.as_deref() == Some(txid) {
                u.spent = false;
                u.spent_in = None;
            }
        }
        if let Ok(id) = Self::txid_from_hex(txid) {
            self.pending_txs.remove(&id);
        }
        self.txs.remove(txid);
        Ok(true)
    }

    pub(crate) fn prior_metadata(&self) -> Result<Option<BlockMetadata>> {
        let Some(p) = &self.prior else {
            return Ok(None);
        };
        let hash_bytes = from_hex(&p.hash).map_err(EngineError::Message)?;
        if hash_bytes.len() != 32 {
            return Err(EngineError::Message("prior hash must be 32 bytes".into()));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);
        Ok(Some(BlockMetadata::from_parts(
            BlockHeight::from_u32(p.height),
            BlockHash(hash),
            Some(p.sapling_tree_size),
            Some(p.orchard_tree_size),
            Some(p.ironwood_tree_size),
        )))
    }

    pub(crate) fn make_prior(
        height: u32,
        hash: BlockHash,
        sapling: u32,
        orchard: u32,
        ironwood: u32,
    ) -> PriorMeta {
        PriorMeta {
            height,
            hash: to_hex(&hash.0),
            sapling_tree_size: sapling,
            orchard_tree_size: orchard,
            ironwood_tree_size: ironwood,
        }
    }

    pub(crate) fn push_spend_note(
        &mut self,
        pool: ShieldedPool,
        txid: &str,
        output_index: u16,
        position: Position,
        mined_height: u32,
        scope: Option<Scope>,
        note: &OrchardNote,
        nf: Option<&OrchardNullifier>,
        is_change: bool,
    ) -> Result<()> {
        let id = self.next_note_id;
        self.next_note_id = self.next_note_id.saturating_add(1);
        let pool_s = match pool {
            ShieldedPool::Ironwood => "ironwood",
            ShieldedPool::Sapling => "sapling",
            _ => "orchard",
        };
        self.spend_notes.push(StoredNote {
            id,
            pool: pool_s.into(),
            txid: txid.into(),
            output_index,
            position: u64::from(position),
            mined_height,
            scope: match scope {
                Some(Scope::Internal) => 1,
                _ => 0,
            },
            recipient: to_hex(&note.recipient().to_raw_address_bytes()),
            value_zat: note.value().inner(),
            rho: to_hex(&note.rho().to_bytes()),
            rseed: to_hex(note.rseed().as_bytes()),
            version: match note.version() {
                NoteVersion::V3 => 3,
                NoteVersion::V2 => 2,
            },
            nf: nf.map(|n| to_hex(&n.to_bytes())).unwrap_or_default(),
            is_change,
            spent: false,
            spent_in: None,
        });
        Ok(())
    }

    /// Record a spend the scanned chain mined. It replaces any reservation
    /// by one of our unmined transactions: when that one expires, releasing
    /// its inputs must not make this note spendable again.
    pub(crate) fn mark_spend_nf(&mut self, nf_hex: &str, spent_in: Option<&str>) {
        for n in &mut self.spend_notes {
            if n.nf == nf_hex {
                n.spent = true;
                if let Some(spent_in) = spent_in {
                    n.spent_in = Some(spent_in.to_string());
                }
            }
        }
    }

    /// Snapshots written before [`Self::mark_spend_nf`] kept the reserving
    /// transaction, so an expiry could release a note the chain had spent.
    fn reconcile_mined_spends(&mut self) {
        let mined: BTreeMap<String, Option<String>> = self
            .notes
            .iter()
            .filter(|n| n.spent_height.is_some() && !n.nf.is_empty())
            .map(|n| (n.nf.clone(), n.spent_in.clone()))
            .collect();
        if mined.is_empty() {
            return;
        }
        for n in &mut self.spend_notes {
            if let Some(spender) = mined.get(&n.nf) {
                n.spent = true;
                if spender.is_some() {
                    n.spent_in = spender.clone();
                }
            }
        }
    }

    fn ensure_offload(&mut self) -> bool {
        if self.offload.is_some() {
            return true;
        }
        let s = root_range(&self.sapling_roots);
        let o = root_range(&self.orchard_roots);
        let i = root_range(&self.ironwood_roots);
        if s.1 + o.1 + i.1 == 0 {
            return false;
        }
        let mut off = Offload::new(0, 0, 0, OffloadOutput::EncodedLeaves);
        off.sapling.roots_available(s.0, s.1);
        off.orchard.roots_available(o.0, o.1);
        off.ironwood.roots_available(i.0, i.1);
        self.offload = Some(off);
        true
    }

    pub(crate) fn append_sapling(
        &mut self,
        height: u32,
        commitments: &[(sapling::Node, Retention<BlockHeight>)],
    ) -> Result<()> {
        let defer = !self.sinsemilla_live || self.live_pools & SAPLING_LIVE == 0;
        if defer && self.ensure_offload() {
            let off = self.offload.as_mut().expect("just ensured");
            off.sapling.feed(self.sapling_next, commitments, height);
            self.sapling_next = self.sapling_next.saturating_add(commitments.len() as u64);
            self.sapling_leaves
                .extend(off.sapling.drain_kept().into_iter().map(kept_to_tree_leaf));
            return Ok(());
        }
        Self::append_sapling_into(
            &mut self.sapling_tree,
            &mut self.sapling_leaves,
            &mut self.sapling_next,
            height,
            commitments,
            defer,
        )
    }

    pub(crate) fn append_orchard(
        &mut self,
        height: u32,
        commitments: &[(MerkleHashOrchard, Retention<BlockHeight>)],
    ) -> Result<()> {
        let defer = !self.sinsemilla_live || self.live_pools & ORCHARD_LIVE == 0;
        if defer && self.ensure_offload() {
            let off = self.offload.as_mut().expect("just ensured");
            off.orchard.feed(self.orchard_next, commitments, height);
            self.orchard_next = self.orchard_next.saturating_add(commitments.len() as u64);
            self.orchard_leaves
                .extend(off.orchard.drain_kept().into_iter().map(kept_to_tree_leaf));
            return Ok(());
        }
        Self::append_orchard_into(
            &mut self.orchard_tree,
            &mut self.orchard_leaves,
            &mut self.orchard_next,
            height,
            commitments,
            defer,
        )
    }

    pub(crate) fn append_ironwood(
        &mut self,
        height: u32,
        commitments: &[(MerkleHashOrchard, Retention<BlockHeight>)],
    ) -> Result<()> {
        let defer = !self.sinsemilla_live || self.live_pools & IRONWOOD_LIVE == 0;
        if defer && self.ensure_offload() {
            let off = self.offload.as_mut().expect("just ensured");
            off.ironwood.feed(self.ironwood_next, commitments, height);
            self.ironwood_next = self.ironwood_next.saturating_add(commitments.len() as u64);
            self.ironwood_leaves
                .extend(off.ironwood.drain_kept().into_iter().map(kept_to_tree_leaf));
            return Ok(());
        }
        Self::append_orchard_into(
            &mut self.ironwood_tree,
            &mut self.ironwood_leaves,
            &mut self.ironwood_next,
            height,
            commitments,
            defer,
        )
    }

    fn append_sapling_into(
        tree: &mut SaplingTree,
        leaves: &mut Vec<TreeLeaf>,
        next: &mut u64,
        height: u32,
        commitments: &[(sapling::Node, Retention<BlockHeight>)],
        defer: bool,
    ) -> Result<()> {
        if !commitments.is_empty() {
            if !defer {
                tree.batch_insert(Position::from(*next), commitments.iter().cloned())
                    .map_err(|e| EngineError::Message(format!("sapling tree: {e}")))?;
            }
            for (h, r) in commitments {
                leaves.push(leaf_from_ret(&h.to_bytes(), r, height, *next));
                *next = next.saturating_add(1);
            }
        }
        if should_checkpoint(leaves, height, commitments.is_empty()) {
            if !defer {
                // An omitted unused pool may not have its scanned prefix yet.
                // Never substitute a future seeded shard for that prefix.
                checkpoint_tree_at_size(tree, BlockHeight::from_u32(height), *next)?;
            }
            if commitments.is_empty() {
                leaves.push(TreeLeaf {
                    hash: String::new(),
                    kind: 2,
                    height,
                    position: *next,
                });
            }
        }
        Ok(())
    }

    fn append_orchard_into(
        tree: &mut OrchardTree,
        leaves: &mut Vec<TreeLeaf>,
        next: &mut u64,
        height: u32,
        commitments: &[(MerkleHashOrchard, Retention<BlockHeight>)],
        defer: bool,
    ) -> Result<()> {
        if !commitments.is_empty() {
            if !defer {
                tree.batch_insert(Position::from(*next), commitments.iter().cloned())
                    .map_err(|e| EngineError::Message(format!("orchard tree: {e}")))?;
            }
            for (h, r) in commitments {
                leaves.push(leaf_from_ret(&h.to_bytes(), r, height, *next));
                *next = next.saturating_add(1);
            }
        }
        if should_checkpoint(leaves, height, commitments.is_empty()) {
            if !defer {
                checkpoint_tree_at_size(tree, BlockHeight::from_u32(height), *next)?;
            }
            if commitments.is_empty() {
                leaves.push(TreeLeaf {
                    hash: String::new(),
                    kind: 2,
                    height,
                    position: *next,
                });
            }
        }
        Ok(())
    }

    pub fn rebuild_trees(&mut self) -> Result<()> {
        self.ensure_scan_healthy()?;
        let result = self.rebuild_trees_inner();
        self.finish_scan_update(result)
    }

    fn rebuild_trees_inner(&mut self) -> Result<()> {
        self.invalidate_live_trees();
        self.assert_consistent_leaf_hashes()?;
        let sap_next = self.sapling_next;
        let orch_next = self.orchard_next;
        let iron_next = self.ironwood_next;
        let sap_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.sapling_leaves),
            self.sapling_base,
            sap_next,
        );
        let orch_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.orchard_leaves),
            self.orchard_base,
            orch_next,
        );
        let iron_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.ironwood_leaves),
            self.ironwood_base,
            iron_next,
        );
        let selective_scan = sap_sparse || orch_sparse || iron_sparse;
        let built_pools = self.used_pools();
        let sap_used = built_pools & SAPLING_LIVE != 0;
        let orch_used = built_pools & ORCHARD_LIVE != 0;
        let iron_used = built_pools & IRONWOOD_LIVE != 0;
        self.sapling_tree = empty_sapling_tree();
        self.orchard_tree = empty_orchard_tree();
        self.ironwood_tree = empty_orchard_tree();
        let sap_boundaries = self.recent_boundaries("sapling");
        let orch_boundaries = self.recent_boundaries("orchard");
        let iron_boundaries = self.recent_boundaries("ironwood");
        let sap_skip = shards_to_hash_retained(
            &self.sapling_leaves,
            self.sapling_roots.len() as u64,
            sap_used,
            &sap_boundaries,
        );
        let orch_skip = shards_to_hash_retained(
            &self.orchard_leaves,
            self.orchard_roots.len() as u64,
            orch_used,
            &orch_boundaries,
        );
        let iron_skip = shards_to_hash_retained(
            &self.ironwood_leaves,
            self.ironwood_roots.len() as u64,
            iron_used,
            &iron_boundaries,
        );
        put_pool_roots(
            &mut self.sapling_tree,
            &self.sapling_roots,
            0,
            SAPLING_SHARD_HEIGHT,
            &sap_skip,
        )?;
        put_pool_roots(
            &mut self.orchard_tree,
            &self.orchard_roots,
            0,
            ORCHARD_SHARD_HEIGHT,
            &orch_skip,
        )?;
        put_pool_roots(
            &mut self.ironwood_tree,
            &self.ironwood_roots,
            0,
            ORCHARD_SHARD_HEIGHT,
            &iron_skip,
        )?;
        if selective_scan {
            if sap_used {
                replay_selective_scan(
                    &mut self.sapling_tree,
                    &self.sapling_leaves,
                    |leaf| retained_boundary_leaf(leaf, sapling_from_leaf, &sap_boundaries),
                    self.sapling_base,
                    self.sapling_roots.len() as u64,
                    self.scanned_height,
                    true,
                )?;
            }
            if orch_used {
                replay_selective_scan(
                    &mut self.orchard_tree,
                    &self.orchard_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &orch_boundaries),
                    self.orchard_base,
                    self.orchard_roots.len() as u64,
                    self.scanned_height,
                    true,
                )?;
            }
            if iron_used {
                replay_selective_scan(
                    &mut self.ironwood_tree,
                    &self.ironwood_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &iron_boundaries),
                    self.ironwood_base,
                    self.ironwood_roots.len() as u64,
                    self.scanned_height,
                    true,
                )?;
            }
        }
        if let Some(bt) = self.birthday_trees.clone() {
            self.install_frontiers(&bt)?;
        }
        if selective_scan {
            if sap_used {
                replay_selective_scan(
                    &mut self.sapling_tree,
                    &self.sapling_leaves,
                    |leaf| retained_boundary_leaf(leaf, sapling_from_leaf, &sap_boundaries),
                    self.sapling_base,
                    self.sapling_roots.len() as u64,
                    self.scanned_height,
                    false,
                )?;
            }
            if orch_used {
                replay_selective_scan(
                    &mut self.orchard_tree,
                    &self.orchard_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &orch_boundaries),
                    self.orchard_base,
                    self.orchard_roots.len() as u64,
                    self.scanned_height,
                    false,
                )?;
            }
            if iron_used {
                replay_selective_scan(
                    &mut self.ironwood_tree,
                    &self.ironwood_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &iron_boundaries),
                    self.ironwood_base,
                    self.ironwood_roots.len() as u64,
                    self.scanned_height,
                    false,
                )?;
            }
            self.sapling_next = sap_next;
            self.orchard_next = orch_next;
            self.ironwood_next = iron_next;
            if sap_used {
                verify_marked_shard_roots(
                    &self.sapling_tree,
                    &self.sapling_roots,
                    &self.sapling_leaves,
                    "sapling",
                    sap_next,
                )?;
            }
            if orch_used {
                verify_marked_shard_roots(
                    &self.orchard_tree,
                    &self.orchard_roots,
                    &self.orchard_leaves,
                    "orchard",
                    orch_next,
                )?;
            }
            if iron_used {
                verify_marked_shard_roots(
                    &self.ironwood_tree,
                    &self.ironwood_roots,
                    &self.ironwood_leaves,
                    "ironwood",
                    iron_next,
                )?;
            }
        } else {
            let keep_from = offload::keep_from_height(self.scanned_height);
            if sap_used {
                replay_tree(
                    &mut self.sapling_tree,
                    &self.sapling_leaves,
                    |leaf| retained_boundary_leaf(leaf, sapling_from_leaf, &sap_boundaries),
                    "sapling",
                    self.sapling_base,
                    keep_from,
                )?;
            }
            if orch_used {
                replay_tree(
                    &mut self.orchard_tree,
                    &self.orchard_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &orch_boundaries),
                    "orchard",
                    self.orchard_base,
                    keep_from,
                )?;
            }
            if iron_used {
                replay_tree(
                    &mut self.ironwood_tree,
                    &self.ironwood_leaves,
                    |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &iron_boundaries),
                    "ironwood",
                    self.ironwood_base,
                    keep_from,
                )?;
            }
            self.sapling_next = self.sapling_base + hashed_leaf_count(&self.sapling_leaves);
            self.orchard_next = self.orchard_base + hashed_leaf_count(&self.orchard_leaves);
            self.ironwood_next = self.ironwood_base + hashed_leaf_count(&self.ironwood_leaves);
        }
        self.checkpoint_live_trees()?;
        self.live_pools = built_pools;
        self.sinsemilla_live = true;
        Ok(())
    }

    /// After a cold/rescan apply: flush selective shard scanning, hash kept shards once.
    /// No-op when incremental catch-up already hashed live.
    ///
    /// Reload (`from_snapshot` skips shardtree replay) must **not** rehash every
    /// completed unmarked shard. Seeded `GetSubtreeRoots` already sit in the cap.
    /// Finalize hashes the open tip shard plus any shard with a marked (note) leaf
    /// so spends keep witnesses. Balance is note-derived and does not wait for this.
    pub fn finalize_scan_trees(&mut self) -> Result<()> {
        self.finalize_scan_trees_ticking(|_| {})
    }

    pub fn finalize_scan_trees_ticking(
        &mut self,
        tick: impl FnMut(TreeFinalizeTick),
    ) -> Result<()> {
        self.ensure_scan_healthy()?;
        let result = self.finalize_scan_trees_inner(tick);
        self.finish_scan_update(result)
    }

    fn finalize_scan_trees_inner(&mut self, mut tick: impl FnMut(TreeFinalizeTick)) -> Result<()> {
        if let Some(mut off) = self.offload.take() {
            off.flush();
            self.sapling_leaves
                .extend(off.sapling.drain_kept().into_iter().map(kept_to_tree_leaf));
            self.orchard_leaves
                .extend(off.orchard.drain_kept().into_iter().map(kept_to_tree_leaf));
            self.ironwood_leaves
                .extend(off.ironwood.drain_kept().into_iter().map(kept_to_tree_leaf));
        }
        self.prune_settled_shards();
        self.assert_consistent_leaf_hashes()?;
        // A newly needed pool (for example Ironwood from NU6.3 on) was left
        // unbuilt by the previous finalize.
        if self.sinsemilla_live && self.used_pools() & !self.live_pools != 0 {
            self.invalidate_live_trees();
        }
        if self.sinsemilla_live {
            self.recompute_pool_fields();
            tick(TreeFinalizeTick {
                done: true,
                hashed: 0,
                total: 0,
                message: "trees already live".into(),
            });
            return Ok(());
        }
        self.repair_tree_cursors();
        let sap_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.sapling_leaves),
            self.sapling_base,
            self.sapling_next,
        );
        let orch_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.orchard_leaves),
            self.orchard_base,
            self.orchard_next,
        );
        let iron_sparse = offload::is_sparse_leaves(
            hashed_leaf_count(&self.ironwood_leaves),
            self.ironwood_base,
            self.ironwood_next,
        );
        let selective_scan = sap_sparse || orch_sparse || iron_sparse;
        if selective_scan {
            // Sparse catch-up: seed GetSubtreeRoots caps (or empty) and hash only
            // note-bearing + open shards. Never replay unused sapling/ironwood or
            // walk every leaf because completed==0.
            tick(TreeFinalizeTick {
                done: false,
                hashed: 0,
                total: 1,
                message: "seeding subtree roots".into(),
            });
            self.seed_tree_caps()?;
            self.hash_incomplete_shards_ticking(&mut tick)?;
            self.checkpoint_live_trees()?;
            self.live_pools = self.used_pools();
            self.sinsemilla_live = true;
        } else {
            tick(TreeFinalizeTick {
                done: false,
                hashed: 0,
                total: 1,
                message: "rebuilding commitment trees".into(),
            });
            self.rebuild_trees()?;
        }
        self.recompute_pool_fields();
        tick(TreeFinalizeTick {
            done: true,
            hashed: 1,
            total: 1,
            message: "commitment trees ready".into(),
        });
        Ok(())
    }

    fn seed_tree_caps(&mut self) -> Result<()> {
        let sap_used = self.pool_needed("sapling");
        let orch_used = self.pool_needed("orchard");
        let iron_used = self.pool_needed("ironwood");
        let sap_boundaries = self.recent_boundaries("sapling");
        let orch_boundaries = self.recent_boundaries("orchard");
        let iron_boundaries = self.recent_boundaries("ironwood");
        let sap_skip = shards_to_hash_retained(
            &self.sapling_leaves,
            self.sapling_roots.len() as u64,
            sap_used,
            &sap_boundaries,
        );
        let orch_skip = shards_to_hash_retained(
            &self.orchard_leaves,
            self.orchard_roots.len() as u64,
            orch_used,
            &orch_boundaries,
        );
        let iron_skip = shards_to_hash_retained(
            &self.ironwood_leaves,
            self.ironwood_roots.len() as u64,
            iron_used,
            &iron_boundaries,
        );
        self.sapling_tree = empty_sapling_tree();
        self.orchard_tree = empty_orchard_tree();
        self.ironwood_tree = empty_orchard_tree();
        put_pool_roots(
            &mut self.sapling_tree,
            &self.sapling_roots,
            0,
            SAPLING_SHARD_HEIGHT,
            &sap_skip,
        )?;
        put_pool_roots(
            &mut self.orchard_tree,
            &self.orchard_roots,
            0,
            ORCHARD_SHARD_HEIGHT,
            &orch_skip,
        )?;
        put_pool_roots(
            &mut self.ironwood_tree,
            &self.ironwood_roots,
            0,
            ORCHARD_SHARD_HEIGHT,
            &iron_skip,
        )?;
        if let Some(bt) = self.birthday_trees.clone() {
            self.install_frontiers(&bt)?;
        }
        Ok(())
    }

    fn checkpoint_live_trees(&mut self) -> Result<()> {
        let floor = u32::from(offload::keep_from_height(self.scanned_height));
        for (used, size, pool) in [
            (self.pool_needed("sapling"), self.sapling_next, "sapling"),
            (self.pool_needed("orchard"), self.orchard_next, "orchard"),
            (self.pool_needed("ironwood"), self.ironwood_next, "ironwood"),
        ] {
            // Finalize intentionally omits unused nonempty pools. A later first
            // receipt requests their rebuild before they can supply witnesses.
            if !used && size > 0 {
                continue;
            }
            let mut checkpoints: Vec<_> = self
                .tree_sizes
                .iter()
                .filter(|m| m.height >= floor && m.height < self.scanned_height)
                .map(|m| {
                    (
                        BlockHeight::from_u32(m.height),
                        match pool {
                            "sapling" => m.sapling,
                            "orchard" => m.orchard,
                            _ => m.ironwood,
                        },
                    )
                })
                .collect();
            checkpoints.push((BlockHeight::from_u32(self.scanned_height), size));
            match pool {
                "sapling" => checkpoint_recent_prefixes(&mut self.sapling_tree, &checkpoints)?,
                "orchard" => checkpoint_recent_prefixes(&mut self.orchard_tree, &checkpoints)?,
                _ => checkpoint_recent_prefixes(&mut self.ironwood_tree, &checkpoints)?,
            }
        }
        Ok(())
    }

    fn hash_incomplete_shards_ticking(
        &mut self,
        tick: &mut impl FnMut(TreeFinalizeTick),
    ) -> Result<()> {
        let keep_from = offload::keep_from_height(self.scanned_height);
        let sap_used = self.pool_needed("sapling");
        let orch_used = self.pool_needed("orchard");
        let iron_used = self.pool_needed("ironwood");
        let sap_boundaries = self.recent_boundaries("sapling");
        let orch_boundaries = self.recent_boundaries("orchard");
        let iron_boundaries = self.recent_boundaries("ironwood");
        let sap_shards = shards_to_hash_retained(
            &self.sapling_leaves,
            self.sapling_roots.len() as u64,
            sap_used,
            &sap_boundaries,
        );
        let orch_shards = shards_to_hash_retained(
            &self.orchard_leaves,
            self.orchard_roots.len() as u64,
            orch_used,
            &orch_boundaries,
        );
        let iron_shards = shards_to_hash_retained(
            &self.ironwood_leaves,
            self.ironwood_roots.len() as u64,
            iron_used,
            &iron_boundaries,
        );
        let total = count_leaves_in_shards(&self.sapling_leaves, &sap_shards)
            .saturating_add(count_leaves_in_shards(&self.orchard_leaves, &orch_shards))
            .saturating_add(count_leaves_in_shards(&self.ironwood_leaves, &iron_shards))
            .max(1);
        let mut hashed = 0u64;
        tick(TreeFinalizeTick {
            done: false,
            hashed,
            total,
            message: if orch_used && !sap_used && !iron_used {
                "hashing orchard commitment trees".into()
            } else {
                "hashing commitment trees".into()
            },
        });
        if sap_used {
            hash_incomplete_pool(
                &mut self.sapling_tree,
                &self.sapling_leaves,
                |leaf| retained_boundary_leaf(leaf, sapling_from_leaf, &sap_boundaries),
                &sap_shards,
                keep_from,
                "sapling",
                self.sapling_base,
                &mut hashed,
                total,
                tick,
            )?;
        }
        if orch_used {
            hash_incomplete_pool(
                &mut self.orchard_tree,
                &self.orchard_leaves,
                |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &orch_boundaries),
                &orch_shards,
                keep_from,
                "orchard",
                self.orchard_base,
                &mut hashed,
                total,
                tick,
            )?;
        }
        if iron_used {
            hash_incomplete_pool(
                &mut self.ironwood_tree,
                &self.ironwood_leaves,
                |leaf| retained_boundary_leaf(leaf, orchard_from_leaf, &iron_boundaries),
                &iron_shards,
                keep_from,
                "ironwood",
                self.ironwood_base,
                &mut hashed,
                total,
                tick,
            )?;
        }
        Ok(())
    }

    /// Import GetTreeState JSON (`{height,hash,saplingTree,orchardTree,ironwoodTree}`).
    pub fn apply_tree_state_json(&mut self, json: &str) -> Result<()> {
        self.ensure_scan_healthy()?;
        let result = self.apply_tree_state_json_inner(json);
        self.finish_scan_update(result)
    }

    fn apply_tree_state_json_inner(&mut self, json: &str) -> Result<()> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("tree state json: {e}")))?;
        let str_field = |k: &str, alt: &str| {
            v.get(k)
                .or_else(|| v.get(alt))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let bt = BirthdayTrees {
            height: v.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            hash: str_field("hash", "hash"),
            sapling: str_field("saplingTree", "sapling_tree"),
            orchard: str_field("orchardTree", "orchard_tree"),
            ironwood: str_field("ironwoodTree", "ironwood_tree"),
        };
        self.install_frontiers(&bt)?;
        self.birthday_trees = Some(bt);
        self.trees_ready = true;
        Ok(())
    }

    fn install_frontiers(&mut self, bt: &BirthdayTrees) -> Result<()> {
        let ts = TreeState {
            network: String::new(),
            height: u64::from(bt.height),
            hash: bt.hash.clone(),
            time: 0,
            sapling_tree: bt.sapling.clone(),
            orchard_tree: bt.orchard.clone(),
            ironwood_tree: bt.ironwood.clone(),
        };
        let state: ChainState = ts
            .to_chain_state()
            .map_err(|e| EngineError::Message(format!("tree state: {e}")))?;
        let h = state.block_height();
        let sap = state.final_sapling_tree();
        self.sapling_tree
            .insert_frontier(
                sap.clone(),
                Retention::Checkpoint {
                    id: h,
                    marking: Marking::Reference,
                },
            )
            .map_err(|e| EngineError::Message(format!("sapling frontier: {e}")))?;
        let orch = state.final_orchard_tree();
        self.orchard_tree
            .insert_frontier(
                orch.clone(),
                Retention::Checkpoint {
                    id: h,
                    marking: Marking::Reference,
                },
            )
            .map_err(|e| EngineError::Message(format!("orchard frontier: {e}")))?;
        let iron = state.final_ironwood_tree();
        self.ironwood_tree
            .insert_frontier(
                iron.clone(),
                Retention::Checkpoint {
                    id: h,
                    marking: Marking::Reference,
                },
            )
            .map_err(|e| EngineError::Message(format!("ironwood frontier: {e}")))?;
        if self.should_adopt_frontier_sizes() {
            self.sapling_base = sap.tree_size();
            self.sapling_next = self.sapling_base;
            self.orchard_base = orch.tree_size();
            self.orchard_next = self.orchard_base;
            self.ironwood_base = iron.tree_size();
            self.ironwood_next = self.ironwood_base;
        }
        Ok(())
    }

    /// Import `{roots:[{completingHeight,rootHash}]}` for sapling|orchard|ironwood.
    pub fn apply_subtree_roots_json(&mut self, protocol: &str, json: &str) -> Result<u32> {
        self.ensure_scan_healthy()?;
        let result = self.apply_subtree_roots_json_inner(protocol, json);
        self.finish_scan_update(result)
    }

    fn apply_subtree_roots_json_inner(&mut self, protocol: &str, json: &str) -> Result<u32> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("subtree roots json: {e}")))?;
        let arr = v
            .get("roots")
            .and_then(|x| x.as_array())
            .ok_or_else(|| EngineError::Message("roots array required".into()))?;
        let mut roots = Vec::with_capacity(arr.len());
        for item in arr {
            let height = item
                .get("completingHeight")
                .or_else(|| item.get("completing_height"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let hash = item
                .get("rootHash")
                .or_else(|| item.get("root_hash"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if hash.is_empty() {
                continue;
            }
            roots.push(StoredSubtreeRoot {
                completing_height: height,
                root_hash: hash,
            });
        }
        let start = v
            .get("startIndex")
            .or_else(|| v.get("start_index"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize;
        let n = roots.len() as u32;
        if n == 0 {
            return Ok(0);
        }
        let end = start as u64 + u64::from(n);
        let place = |stored: &mut Vec<StoredSubtreeRoot>, roots: Vec<StoredSubtreeRoot>| {
            stored.truncate(start);
            stored.resize(start, StoredSubtreeRoot::placeholder());
            stored.extend(roots);
        };
        match protocol {
            "sapling" => {
                put_pool_roots(
                    &mut self.sapling_tree,
                    &roots,
                    start as u64,
                    SAPLING_SHARD_HEIGHT,
                    &shards_with_hashed_leaves(&self.sapling_leaves),
                )?;
                place(&mut self.sapling_roots, roots);
                if let Some(off) = self.offload.as_mut() {
                    off.sapling.roots_available(start as u64, end);
                }
            }
            "orchard" => {
                put_pool_roots(
                    &mut self.orchard_tree,
                    &roots,
                    start as u64,
                    ORCHARD_SHARD_HEIGHT,
                    &shards_with_hashed_leaves(&self.orchard_leaves),
                )?;
                place(&mut self.orchard_roots, roots);
                if let Some(off) = self.offload.as_mut() {
                    off.orchard.roots_available(start as u64, end);
                }
            }
            "ironwood" => {
                put_pool_roots(
                    &mut self.ironwood_tree,
                    &roots,
                    start as u64,
                    ORCHARD_SHARD_HEIGHT,
                    &shards_with_hashed_leaves(&self.ironwood_leaves),
                )?;
                place(&mut self.ironwood_roots, roots);
                if let Some(off) = self.offload.as_mut() {
                    off.ironwood.roots_available(start as u64, end);
                }
            }
            other => {
                return Err(EngineError::Message(format!(
                    "unknown shielded protocol {other}"
                )))
            }
        }
        self.prune_settled_shards();
        Ok(n)
    }

    pub(crate) fn decode_spend_note(n: &StoredNote) -> Result<OrchardNote> {
        let rec = bytes32_n(&n.recipient, 43)?;
        let mut rec43 = [0u8; 43];
        rec43.copy_from_slice(&rec);
        let recipient = Option::from(OrchardAddress::from_raw_address_bytes(&rec43))
            .ok_or_else(|| EngineError::Message("orchard address".into()))?;
        let rho_b = bytes32(&n.rho)?;
        let rho = Option::from(Rho::from_bytes(&rho_b))
            .ok_or_else(|| EngineError::Message("rho".into()))?;
        let rseed_b = bytes32(&n.rseed)?;
        let rseed = Option::from(RandomSeed::from_bytes(rseed_b, &rho))
            .ok_or_else(|| EngineError::Message("rseed".into()))?;
        let version = if n.version == 3 {
            NoteVersion::V3
        } else {
            NoteVersion::V2
        };
        Option::from(OrchardNote::from_parts(
            recipient,
            NoteValue::from_raw(n.value_zat),
            rho,
            rseed,
            version,
        ))
        .ok_or_else(|| EngineError::Message("orchard note".into()))
    }

    /// Check the live trees against `GetTreeState` JSON for a checkpointed
    /// height: each built pool's root must match, and every unspent Orchard or
    /// Ironwood note must have a witness that hashes to that root. Returns the
    /// number of witnesses checked. `examples/web_replay.rs` runs this against
    /// mainnet data.
    pub fn verify_trees_against(&self, tree_state_json: &str) -> Result<usize> {
        let v: serde_json::Value = serde_json::from_str(tree_state_json)
            .map_err(|e| EngineError::Message(format!("tree state json: {e}")))?;
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let height = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
        let state = TreeState {
            network: String::new(),
            height,
            hash: field("hash"),
            time: 0,
            sapling_tree: field("saplingTree"),
            orchard_tree: field("orchardTree"),
            ironwood_tree: field("ironwoodTree"),
        }
        .to_chain_state()
        .map_err(|e| EngineError::Message(format!("tree state: {e}")))?;
        let id = BlockHeight::from_u32(height as u32);
        let mismatch =
            |pool: &str| EngineError::Message(format!("{pool} root differs at {height}"));
        if self.live_pools & SAPLING_LIVE != 0 {
            let built = self.sapling_tree.root_at_checkpoint_id(&id).ok().flatten();
            if built != Some(state.final_sapling_tree().root()) {
                return Err(mismatch("sapling"));
            }
        }
        let orchard_root = state.final_orchard_tree().root();
        let ironwood_root = state.final_ironwood_tree().root();
        for (bit, tree, want, pool) in [
            (ORCHARD_LIVE, &self.orchard_tree, orchard_root, "orchard"),
            (
                IRONWOOD_LIVE,
                &self.ironwood_tree,
                ironwood_root,
                "ironwood",
            ),
        ] {
            if self.live_pools & bit != 0
                && tree.root_at_checkpoint_id(&id).ok().flatten() != Some(want)
            {
                return Err(mismatch(pool));
            }
        }
        let mut checked = 0;
        for n in self.spend_notes.iter().filter(|n| !n.spent) {
            let (tree, root) = if n.pool == "ironwood" {
                (&self.ironwood_tree, ironwood_root)
            } else {
                (&self.orchard_tree, orchard_root)
            };
            let leaf =
                MerkleHashOrchard::from_cmx(&Self::decode_spend_note(n)?.commitment().into());
            let witness = tree
                .witness_at_checkpoint_id(Position::from(n.position), &id)
                .map_err(|e| EngineError::Message(format!("witness {}: {e}", n.position)))?;
            if !witness.is_some_and(|w| w.root(leaf) == root) {
                return Err(EngineError::Message(format!(
                    "{} note at {} has no valid witness at {height}",
                    n.pool, n.position
                )));
            }
            checked += 1;
        }
        Ok(checked)
    }

    /// Snapshot keys, note references and bridge UTXOs use wire-order bytes.
    /// Keep that legacy representation internal; public history uses Display.
    pub(crate) fn txid_from_hex(s: &str) -> Result<TxId> {
        let b = bytes32(s)?;
        Ok(TxId::from_bytes(b))
    }

    pub(super) fn display_txid(wire_hex: &str) -> String {
        Self::txid_from_hex(wire_hex)
            .map(|id| id.to_string())
            .unwrap_or_else(|_| wire_hex.to_owned())
    }

    pub(crate) fn store_pending_tx(&mut self, tx: &Transaction) -> Result<()> {
        let mut data = Vec::new();
        tx.write(&mut data)
            .map_err(|e| EngineError::Message(format!("serialize tx: {e}")))?;
        self.pending_txs.insert(tx.txid(), data);
        Ok(())
    }

    pub fn apply_utxos_json(&mut self, json: &str) -> Result<u32> {
        // The loopback GetAddressUtxos transport exposes the protocol's wire
        // txid bytes as hex, matching StoredUtxo and transparent OutPoint. Do
        // not apply public history's display-order conversion to this input.
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| EngineError::Message(format!("utxos json: {e}")))?;
        let arr = v
            .get("utxos")
            .and_then(|x| x.as_array())
            .ok_or_else(|| EngineError::Message("utxos array required".into()))?;
        let pending_spent: std::collections::HashMap<(String, u32), Option<String>> = self
            .utxos
            .iter()
            .filter(|u| u.spent)
            .map(|u| ((u.txid.clone(), u.index), u.spent_in.clone()))
            .collect();
        let mut next = Vec::with_capacity(arr.len());
        for item in arr {
            let txid = item
                .get("txid")
                .and_then(|x| x.as_str())
                .ok_or_else(|| EngineError::Message("utxo.txid".into()))?;
            let index = item
                .get("index")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| EngineError::Message("utxo.index".into()))?
                as u32;
            let spent_in = pending_spent
                .get(&(txid.to_string(), index))
                .cloned()
                .flatten();
            let spent =
                spent_in.is_some() || pending_spent.contains_key(&(txid.to_string(), index));
            next.push(StoredUtxo {
                coinbase: item
                    .get("coinbase")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                txid: txid.to_string(),
                index,
                script: item
                    .get("script")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                value_zat: item.get("valueZat").and_then(|x| x.as_u64()).unwrap_or(0),
                height: item.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                address: item
                    .get("address")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| self.transparent_address.clone())
                    .unwrap_or_default(),
                spent,
                spent_in,
            });
        }
        let n = next.len() as u32;
        // The server lists only unspent outputs. Keep ours that a known wallet
        // transaction spent, so its history still shows the transparent input
        // (a mined shield would otherwise read as a plain receipt).
        let listed: std::collections::HashSet<(String, u32)> =
            next.iter().map(|u| (u.txid.clone(), u.index)).collect();
        let kept: Vec<StoredUtxo> = self
            .utxos
            .iter()
            .filter(|u| {
                u.spent
                    && u.spent_in
                        .as_ref()
                        .is_some_and(|t| self.txs.contains_key(t))
                    && !listed.contains(&(u.txid.clone(), u.index))
            })
            .cloned()
            .collect();
        next.extend(kept);
        self.utxos = next;
        // Explicit compatibility address lookup replaces compact coverage. A
        // future switch back must replay spends missed in this mode.
        self.transparent_scan_required = false;
        self.transparent_scan = None;
        self.recompute_pool_fields();
        Ok(n)
    }
}

fn should_checkpoint(leaves: &[TreeLeaf], height: u32, empty: bool) -> bool {
    if !empty {
        return true;
    }
    match leaves.last() {
        None => true,
        Some(l) => height.saturating_sub(l.height) >= CHECKPOINT_EVERY,
    }
}

fn put_pool_roots<H, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    roots: &[StoredSubtreeRoot],
    start_index: u64,
    shard_height: u8,
    skip: &BTreeSet<u64>,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq + HashSer,
{
    for (i, r) in roots.iter().enumerate() {
        let idx = start_index + i as u64;
        if r.is_placeholder() || skip.contains(&idx) {
            continue;
        }
        let bytes = from_hex(&r.root_hash).map_err(EngineError::Message)?;
        let hash = H::read(&bytes[..])
            .map_err(|e| EngineError::Message(format!("subtree root hash: {e}")))?;
        let addr = MerkleAddress::from_parts(shard_height.into(), idx);
        if let Ok(Some(existing)) = tree.store().get_shard(addr) {
            let stored = if existing.root().is_leaf() {
                existing.root().leaf_value().cloned()
            } else {
                existing
                    .root()
                    .annotation()
                    .and_then(|ann| ann.as_deref().cloned())
            };
            if let Some(got) = stored {
                if got != hash {
                    return Err(tree_conflict(
                        "subtree",
                        format!("shard {idx} cap disagrees with an already-stored root"),
                    ));
                }
                continue;
            }
            if existing.root().has_computable_root() {
                continue;
            }
        }
        tree.insert(addr, hash).map_err(|e| {
            let msg = format!("{e}");
            if msg.contains("Inserted root conflicts") {
                tree_conflict("subtree", format!("shard {idx} insert: {e}"))
            } else {
                EngineError::Message(format!("subtree root insert: {e}"))
            }
        })?;
    }
    Ok(())
}

/// Checkpoint the scanned prefix, independently of future downloaded roots.
/// `false` means the pool's intentionally omitted prefix is not in this tree.
fn checkpoint_tree_at_size<H, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    height: BlockHeight,
    size: u64,
) -> Result<bool>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
{
    use shardtree::{store::Checkpoint, LocatedPrunableTree, RetentionFlags, Tree};
    let expected = size.checked_sub(1).map(Position::from);
    let existing = tree
        .store()
        .get_checkpoint(&height)
        .expect("infallible memory store");
    if existing
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.position() != expected)
    {
        return Err(tree_conflict(
            "checkpoint",
            format!("height {height} disagrees with scanned tree size {size}"),
        ));
    }
    let checkpoint_map = match (existing.is_none(), expected) {
        (true, Some(position)) => BTreeMap::from([(height, position)]),
        _ => BTreeMap::new(),
    };
    let boundary = if let Some(position) = expected {
        let Some((address, hash, flags)) =
            offload::stored_checkpoint_boundary(tree, position).expect("infallible memory store")
        else {
            return Ok(false);
        };
        LocatedPrunableTree::from_parts(
            address,
            Tree::leaf((hash, flags | RetentionFlags::CHECKPOINT)),
        )
        .expect("existing terminal node has a valid address")
    } else {
        if existing.is_none() {
            tree.store_mut()
                .add_checkpoint(height, Checkpoint::tree_empty())
                .expect("infallible memory store");
        }
        LocatedPrunableTree::empty(MerkleAddress::from_parts(SHARD.into(), 0))
    };
    tree.insert_tree(boundary, checkpoint_map)
        .map_err(|e| tree_conflict("checkpoint", format!("height {height}: {e}")))?;
    Ok(true)
}

fn checkpoint_required<H, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    height: BlockHeight,
    size: u64,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
{
    if !checkpoint_tree_at_size(tree, height, size)? {
        return Err(tree_conflict(
            "checkpoint",
            format!("height {height} has no retained boundary at tree size {size}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn checkpoint_complete_prefix<H, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    height: BlockHeight,
    size: u64,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
{
    // Validate all witness inputs once at finalize, rather than rehashing the
    // complete root for every historical checkpoint during replay.
    tree.root(
        ShardTree::<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>::root_addr(),
        Position::from(size),
    )
    .map_err(|e| tree_conflict("checkpoint", format!("height {height} prefix {size}: {e}")))?;
    checkpoint_required(tree, height, size)
}

fn checkpoint_recent_prefixes<H, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    checkpoints: &[(BlockHeight, u64)],
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq,
{
    let mut checked = BTreeSet::new();
    for &(height, size) in checkpoints {
        let is_tip = checkpoints.last() == Some(&(height, size));
        if !checked.contains(&size) {
            let covered = tree.root(
                ShardTree::<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>::root_addr(),
                Position::from(size),
            );
            if let Err(error) = covered {
                // Older snapshots may already have compacted this historical
                // boundary. Never invent its hash or return it as an anchor.
                if !is_tip {
                    continue;
                }
                return Err(tree_conflict(
                    "checkpoint",
                    format!("height {height} prefix {size}: {error}"),
                ));
            }
            checked.insert(size);
        }
        if !checkpoint_tree_at_size(tree, height, size)? && is_tip {
            return Err(tree_conflict(
                "checkpoint",
                format!("height {height} has no retained boundary at tree size {size}"),
            ));
        }
    }
    Ok(())
}

fn retained_boundary_leaf<H>(
    leaf: &TreeLeaf,
    parse: impl FnOnce(&TreeLeaf) -> Result<(H, Retention<BlockHeight>)>,
    boundaries: &BTreeMap<u64, BlockHeight>,
) -> Result<(H, Retention<BlockHeight>)> {
    let (hash, retention) = parse(leaf)?;
    let retention = boundaries
        .get(&leaf.position)
        .map(|height| offload::checkpoint_retention(&retention, *height))
        .unwrap_or(retention);
    Ok((hash, retention))
}

/// Rebuild a dense pool (every leaf since `start` retained). Only the last
/// `TIP_CHECKPOINT_BLOCKS` keep checkpoints: the tree holds no more than that,
/// and checkpointing every block evicted one per block, which with serial
/// hashing made this fallback take minutes in the browser. The caller adds the
/// recent empty-block marks through `checkpoint_live_trees`.
fn replay_tree<H, F, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    leaves: &[TreeLeaf],
    parse: F,
    pool: &str,
    start: u64,
    keep_from: BlockHeight,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq + Send + Sync,
    F: Fn(&TreeLeaf) -> Result<(H, Retention<BlockHeight>)>,
{
    let mut run = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        if !leaf.hash.is_empty() {
            run.push(parse(leaf)?);
        }
    }
    offload::insert_leaf_run(tree, start, &run, false, keep_from)
        .map_err(|e| EngineError::Message(format!("rebuild {pool}: {e}")))?;
    if let Some(last) = leaves.last() {
        checkpoint_required(
            tree,
            BlockHeight::from_u32(last.height),
            start + run.len() as u64,
        )?;
    }
    Ok(())
}

fn hashed_leaf_count(leaves: &[TreeLeaf]) -> u64 {
    leaves.iter().filter(|l| !l.hash.is_empty()).count() as u64
}

fn max_hashed_position(leaves: &[TreeLeaf]) -> Option<u64> {
    leaves
        .iter()
        .filter(|l| !l.hash.is_empty())
        .map(|l| l.position)
        .max()
}

fn shards_with_hashed_leaves(leaves: &[TreeLeaf]) -> BTreeSet<u64> {
    leaves
        .iter()
        .filter(|l| !l.hash.is_empty())
        .map(|l| offload::shard_of(l.position))
        .collect()
}

const TREE_CONFLICT_HINT: &str =
    "Wipe scan & resync (or rewind to last good mark). Do not keep scanning or send until trees rebuild.";

fn tree_conflict(pool: &str, detail: impl core::fmt::Display) -> EngineError {
    EngineError::Message(format!(
        "selective-scan tree conflict ({pool}): {detail}. {TREE_CONFLICT_HINT}"
    ))
}

/// Drop same-hash duplicates. Disagreeing hashes at one leaf fail before insert_tree.
fn coalesce_hashed_leaves(leaves: &mut Vec<TreeLeaf>, pool: &str) -> Result<()> {
    // Validate by reference first. Draining here used to discard the entire
    // retained tree on the first conflict, which a pagehide save could publish.
    let mut seen: BTreeMap<u64, &str> = BTreeMap::new();
    for leaf in leaves.iter().filter(|leaf| !leaf.hash.is_empty()) {
        if seen
            .insert(leaf.position, &leaf.hash)
            .is_some_and(|hash| hash != leaf.hash)
        {
            return Err(tree_conflict(
                pool,
                format!(
                    "two hashes at leaf {} (shard {})",
                    leaf.position,
                    offload::shard_of(leaf.position)
                ),
            ));
        }
    }
    drop(seen);
    let mut best: BTreeMap<u64, TreeLeaf> = BTreeMap::new();
    let mut rest = Vec::new();
    for leaf in leaves.drain(..) {
        if leaf.hash.is_empty() {
            rest.push(leaf);
            continue;
        }
        if let Some(existing) = best.get_mut(&leaf.position) {
            // Marking and checkpoint retention are independent bits. Keep the
            // checkpoint's height when a later duplicate only adds marking.
            if existing.kind & 2 == 0 && leaf.kind & 2 != 0 {
                existing.height = leaf.height;
            }
            existing.kind |= leaf.kind;
        } else {
            best.insert(leaf.position, leaf);
        }
    }
    rest.extend(best.into_values());
    *leaves = rest;
    Ok(())
}

impl LeafValidation {
    fn validate(&mut self, leaves: &mut Vec<TreeLeaf>, pool: &str) -> Result<()> {
        // Rewind/reset explicitly clears this cursor; defensively handle an
        // externally shortened vector as well.
        if self.len > leaves.len() {
            *self = Self::default();
        }
        let mut last = self.last_position;
        for leaf in &leaves[self.len..] {
            if leaf.hash.is_empty() {
                continue;
            }
            if last.is_some_and(|position| leaf.position <= position) {
                // Imports/replayed overlaps are rare. Preserve duplicate-hash
                // validation and marked retention without a permanent index
                // proportional to the entire retained history.
                coalesce_hashed_leaves(leaves, pool)?;
                self.len = leaves.len();
                self.last_position = max_hashed_position(leaves);
                return Ok(());
            }
            last = Some(leaf.position);
        }
        self.len = leaves.len();
        self.last_position = last;
        Ok(())
    }
}

/// Pre-selective-scan snapshots stored hashed leaves without positions (`serde` default 0)
/// and omitted `*_next` (falls back to birthday base). That looks sparse to selective shard scanning.
fn migrate_legacy_leaf_positions(leaves: &mut [TreeLeaf], base: u64, next: u64) -> u64 {
    let mut hashed = 0u64;
    let mut all_zero = true;
    let mut any = false;
    for l in leaves.iter() {
        if l.hash.is_empty() {
            continue;
        }
        any = true;
        hashed = hashed.saturating_add(1);
        if l.position != 0 {
            all_zero = false;
        }
    }
    if !any || !all_zero {
        return next;
    }
    let mut i = 0u64;
    for l in leaves.iter_mut() {
        if l.hash.is_empty() {
            continue;
        }
        l.position = base.saturating_add(i);
        i = i.saturating_add(1);
    }
    base.saturating_add(hashed)
}

fn kept_to_tree_leaf(k: KeptLeaf) -> TreeLeaf {
    TreeLeaf {
        hash: to_hex(&k.hash),
        kind: k.kind,
        height: k.height,
        position: k.position,
    }
}

fn replay_selective_scan<H, F, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    leaves: &[TreeLeaf],
    parse: F,
    base: u64,
    completed: u64,
    scanned: u32,
    historic_only: bool,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq + Send + Sync,
    F: Fn(&TreeLeaf) -> Result<(H, Retention<BlockHeight>)>,
{
    let mut entries = Vec::new();
    for leaf in leaves {
        if leaf.hash.is_empty() || leaf.position < base {
            continue;
        }
        let (h, r) = parse(leaf)?;
        entries.push((leaf.position, h, r));
    }
    entries.sort_by_key(|e| e.0);
    let first_shard = offload::shard_of(base);
    let keep_from = offload::keep_from_height(scanned);
    let mut i = 0;
    while i < entries.len() {
        let start_pos = entries[i].0;
        let mut end = i + 1;
        while end < entries.len() && entries[end].0 == entries[end - 1].0 + 1 {
            end += 1;
        }
        let shard = offload::shard_of(start_pos);
        let historic = offload::is_historic_shard(shard, first_shard, completed);
        if historic == historic_only {
            let run: Vec<(H, Retention<BlockHeight>)> = entries[i..end]
                .iter()
                .map(|(_, h, r)| (h.clone(), r.clone()))
                .collect();
            let keeps_recent = run.iter().any(|(_, retention)| {
                matches!(retention, Retention::Checkpoint { id, .. } if *id >= keep_from)
            });
            offload::insert_leaf_run(tree, start_pos, &run, historic && !keeps_recent, keep_from)?;
        }
        i = end;
    }
    Ok(())
}

/// A pool needs its tree only to witness notes we can still spend. Spent notes
/// alone leave it unbuilt; a note arriving later, or a pending spend expiring,
/// makes the next finalize build it from the retained leaves.
fn pool_used(leaves: &[TreeLeaf], notes: &[TrackedNote], spend: &[StoredNote], pool: &str) -> bool {
    notes.iter().any(|n| n.pool == pool && !n.spent)
        || spend.iter().any(|n| n.pool == pool && !n.spent)
        || leaves
            .iter()
            .any(|l| !l.hash.is_empty() && (l.kind == 1 || l.kind == 3))
}

/// Complete, root-covered shards whose every leaf is below `floor` and whose
/// marked leaves are all `settled` (see `WebWallet::prune_settled_shards`).
fn settled_shards(
    leaves: &[TreeLeaf],
    roots: &[StoredSubtreeRoot],
    next: u64,
    floor: u32,
    settled: impl Fn(u32) -> bool,
) -> BTreeSet<u64> {
    let complete = next >> offload::SHARD_HEIGHT;
    let mut ok = BTreeSet::new();
    let mut blocked = BTreeSet::new();
    // Leaves are position-ordered, so decide a shard once per run.
    let mut cur: Option<(u64, bool)> = None;
    let mut close = |cur: Option<(u64, bool)>| {
        if let Some((shard, fine)) = cur {
            if fine {
                ok.insert(shard);
            } else {
                blocked.insert(shard);
            }
        }
    };
    for leaf in leaves {
        if leaf.hash.is_empty() {
            continue;
        }
        let shard = offload::shard_of(leaf.position);
        let fine = match cur {
            Some((s, fine)) if s == shard => fine,
            _ => {
                close(cur);
                shard < complete
                    && roots
                        .get(shard as usize)
                        .is_some_and(|r| !r.is_placeholder())
            }
        };
        let fine = fine && leaf.height < floor && (leaf.kind & 1 == 0 || settled(leaf.height));
        cur = Some((shard, fine));
    }
    close(cur);
    ok.retain(|s| !blocked.contains(s));
    ok
}

/// Note-bearing shards, plus the open tip shard when GetSubtreeRoots marked a boundary.
/// `completed == 0` must **not** mean "hash every leaf from genesis".
fn shards_to_hash(leaves: &[TreeLeaf], completed: u64, used: bool) -> BTreeSet<u64> {
    let mut shards = BTreeSet::new();
    if !used {
        return shards;
    }
    for leaf in leaves {
        if leaf.hash.is_empty() {
            continue;
        }
        let shard = offload::shard_of(leaf.position);
        if leaf.kind == 1 || leaf.kind == 3 {
            shards.insert(shard);
        } else if completed > 0 && shard >= completed {
            shards.insert(shard);
        }
    }
    shards
}

fn shards_to_hash_retained(
    leaves: &[TreeLeaf],
    completed: u64,
    used: bool,
    boundaries: &BTreeMap<u64, BlockHeight>,
) -> BTreeSet<u64> {
    let mut shards = shards_to_hash(leaves, completed, used);
    if used {
        // A complete seeded root supplies a whole-shard boundary directly.
        // Interior boundaries require the actual retained leaves, when present.
        for leaf in leaves {
            if !leaf.hash.is_empty()
                && boundaries.contains_key(&leaf.position)
                && (leaf.position + 1) % offload::SHARD_SIZE != 0
            {
                shards.insert(offload::shard_of(leaf.position));
            }
        }
    }
    shards
}

fn leaf_needs_hash(leaf: &TreeLeaf, shards: &BTreeSet<u64>) -> bool {
    if leaf.hash.is_empty() {
        return false;
    }
    shards.contains(&offload::shard_of(leaf.position))
}

fn count_leaves_in_shards(leaves: &[TreeLeaf], shards: &BTreeSet<u64>) -> u64 {
    leaves.iter().filter(|l| leaf_needs_hash(l, shards)).count() as u64
}

fn hash_incomplete_pool<H, F, const DEPTH: u8, const SHARD: u8>(
    tree: &mut ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    leaves: &[TreeLeaf],
    parse: F,
    shards: &BTreeSet<u64>,
    keep_from: BlockHeight,
    pool: &str,
    skip_before: u64,
    hashed: &mut u64,
    total: u64,
    tick: &mut impl FnMut(TreeFinalizeTick),
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq + Send + Sync,
    F: Fn(&TreeLeaf) -> Result<(H, Retention<BlockHeight>)>,
{
    let mut entries = Vec::new();
    for leaf in leaves {
        if !leaf_needs_hash(leaf, shards) {
            continue;
        }
        if leaf.position < skip_before {
            continue;
        }
        let (h, r) = parse(leaf)?;
        entries.push((leaf.position, h, r));
    }
    entries.sort_by_key(|e| e.0);
    let mut i = 0;
    while i < entries.len() {
        let start_pos = entries[i].0;
        let mut end = i + 1;
        while end < entries.len() && entries[end].0 == entries[end - 1].0 + 1 {
            end += 1;
        }
        let run: Vec<(H, Retention<BlockHeight>)> = entries[i..end]
            .iter()
            .map(|(_, h, r)| (h.clone(), r.clone()))
            .collect();
        for (pos, offset, len) in offload::aligned_batches(start_pos, run.len()) {
            offload::insert_leaf_run(tree, pos, &run[offset..offset + len], false, keep_from)?;
            *hashed = hashed.saturating_add(len as u64);
            tick(TreeFinalizeTick {
                done: false,
                hashed: *hashed,
                total,
                message: format!("hashing {pool} commitment trees"),
            });
        }
        i = end;
    }
    Ok(())
}

fn verify_marked_shard_roots<H, const DEPTH: u8, const SHARD: u8>(
    tree: &ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD>,
    roots: &[StoredSubtreeRoot],
    leaves: &[TreeLeaf],
    pool: &str,
    scanned_size: u64,
) -> Result<()>
where
    H: incrementalmerkletree::Hashable + Clone + PartialEq + HashSer,
{
    let mut shards = BTreeMap::new();
    for leaf in leaves {
        if leaf.hash.is_empty() || (leaf.kind != 1 && leaf.kind != 3) {
            continue;
        }
        let shard = offload::shard_of(leaf.position);
        // Downloaded roots may cover future blocks. A partial scanned shard
        // cannot be compared to its complete future root after rewind.
        if (shard as usize) < roots.len()
            && !roots[shard as usize].is_placeholder()
            && (shard + 1) * (1u64 << SHARD) <= scanned_size
        {
            shards.insert(shard, &roots[shard as usize].root_hash);
        }
    }
    for (shard, want) in shards {
        let addr = MerkleAddress::from_parts(SHARD.into(), shard);
        let got = tree.root(addr, Position::from(u64::MAX)).map_err(|e| {
            EngineError::Message(format!("{pool} selective-scan shard {shard}: {e}"))
        })?;
        let mut buf = Vec::with_capacity(32);
        got.write(&mut buf)
            .map_err(|e| EngineError::Message(format!("{pool} selective-scan encode: {e}")))?;
        let got_hex = to_hex(&buf);
        let want_n = want.trim().trim_start_matches("0x").to_ascii_lowercase();
        if got_hex != want_n {
            return Err(EngineError::Message(format!(
                "{pool} selective-scan shard {shard} root mismatch (built {got_hex}, seeded {want_n}); rescan"
            )));
        }
    }
    Ok(())
}

fn leaf_from_ret(
    hash: &[u8; 32],
    r: &Retention<BlockHeight>,
    height: u32,
    position: u64,
) -> TreeLeaf {
    let (kind, height) = match r {
        Retention::Ephemeral => (0, height),
        Retention::Marked => (1, height),
        Retention::Checkpoint {
            id,
            marking: Marking::None,
        } => (2, u32::from(*id)),
        Retention::Checkpoint { id, marking: _ } => (3, u32::from(*id)),
        Retention::Reference => (0, height),
    };
    TreeLeaf {
        hash: to_hex(hash),
        kind,
        height,
        position,
    }
}

fn ret_from_leaf(leaf: &TreeLeaf) -> Retention<BlockHeight> {
    match leaf.kind {
        1 => Retention::Marked,
        2 => Retention::Checkpoint {
            id: BlockHeight::from_u32(leaf.height),
            marking: Marking::None,
        },
        3 => Retention::Checkpoint {
            id: BlockHeight::from_u32(leaf.height),
            marking: Marking::Marked,
        },
        _ => Retention::Ephemeral,
    }
}

fn sapling_from_leaf(leaf: &TreeLeaf) -> Result<(sapling::Node, Retention<BlockHeight>)> {
    let b = bytes32(&leaf.hash)?;
    let node = Option::from(sapling::Node::from_bytes(b))
        .ok_or_else(|| EngineError::Message("sapling node".into()))?;
    Ok((node, ret_from_leaf(leaf)))
}

fn orchard_from_leaf(leaf: &TreeLeaf) -> Result<(MerkleHashOrchard, Retention<BlockHeight>)> {
    let b = bytes32(&leaf.hash)?;
    let node = Option::from(MerkleHashOrchard::from_bytes(&b))
        .ok_or_else(|| EngineError::Message("orchard node".into()))?;
    Ok((node, ret_from_leaf(leaf)))
}

fn bytes32(s: &str) -> Result<[u8; 32]> {
    let v = from_hex(s).map_err(EngineError::Message)?;
    v.try_into()
        .map_err(|_| EngineError::Message(format!("expected 32 bytes, got {}", s.len() / 2)))
}

fn bytes32_n(s: &str, n: usize) -> Result<Vec<u8>> {
    let v = from_hex(s).map_err(EngineError::Message)?;
    if v.len() != n {
        return Err(EngineError::Message(format!(
            "expected {n} bytes, got {}",
            v.len()
        )));
    }
    Ok(v)
}

fn diversifier_u32(j: DiversifierIndex) -> u32 {
    let b = j.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod performance_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC};
    use crate::offload::{self, SHARD_SIZE};
    use incrementalmerkletree::{Hashable, Level};
    use prost::Message;
    use zcash_client_backend::proto::compact_formats::{ChainMetadata, CompactBlock};

    fn empty_block(height: u64, prev_hash: Vec<u8>) -> CompactBlock {
        let mut hash = vec![0u8; 32];
        hash[0] = height as u8;
        CompactBlock {
            height,
            hash,
            prev_hash,
            time: 1,
            header: vec![],
            vtx: vec![],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            }),
        }
    }

    fn wallet() -> WebWallet {
        let acct =
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, crate::Network::Regtest, 0).unwrap();
        WebWallet::from_account(acct, 1).unwrap()
    }

    #[test]
    fn rescan_preserves_identity_and_reinitializes_birthday_trees() {
        let acct =
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, crate::Network::Regtest, 0).unwrap();
        let mut w = WebWallet::from_account(acct, 100).unwrap();
        w.next_unified_address().unwrap();
        let address = w.unified_address.clone();
        let next = w.next_diversifier;
        w.orchard_base = 900;
        w.orchard_next = 901;
        w.scanned_height = 200;
        w.rescan_from(50).unwrap();
        assert_eq!(w.unified_address, address);
        assert_eq!(w.next_diversifier, next);
        assert!(!w.view_only);
        assert_eq!(w.birthday, 50);
        assert_eq!(w.scanned_height, 49);
        assert_eq!(w.orchard_base, 0);
        assert!(!w.trees_ready);
        assert!(w.birthday_trees.is_none());
        let bytes = w.to_snapshot().unwrap();
        assert!(w.rescan_from(0).is_err());
        assert!(w.rescan_from(51).is_err());
        assert_eq!(w.to_snapshot().unwrap(), bytes);
    }

    #[test]
    fn rescan_keeps_view_only_and_hardware_account_metadata() {
        let mut w = wallet();
        w.view_only = true;
        w.hardware = Some(HardwareAccount {
            device: "keystone".into(),
            seed_fingerprint: "07".repeat(32),
            account_index: 0,
        });
        w.rescan_from(1).unwrap();
        assert!(w.view_only);
        let hardware = w.hardware.as_ref().unwrap();
        assert_eq!(hardware.device, "keystone");
        assert_eq!(hardware.seed_fingerprint, "07".repeat(32));
        assert_eq!(hardware.account_index, 0);
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert!(restored.view_only);
        assert_eq!(restored.hardware.unwrap().seed_fingerprint, "07".repeat(32));
    }

    #[test]
    fn rescan_refuses_unmined_payment_until_expiry() {
        let mut w = wallet();
        let id = zcash_primitives::transaction::TxId::from_bytes([7; 32]);
        let key = to_hex(id.as_ref());
        w.pending_txs.insert(id, vec![1, 2, 3]);
        w.txs.insert(
            key.clone(),
            TxAgg {
                expiry_height: Some(40),
                ..Default::default()
            },
        );
        let before = w.to_snapshot().unwrap();
        assert!(w
            .rescan_from(1)
            .unwrap_err()
            .to_string()
            .contains("rescan pending payment"));
        assert_eq!(w.to_snapshot().unwrap(), before);
        w.scanned_height = 40;
        w.rescan_from(1).unwrap();
        assert!(w.pending_txs.is_empty());
    }

    #[test]
    fn migrate_legacy_assigns_dense_positions() {
        let mut leaves = vec![
            TreeLeaf {
                hash: "11".repeat(32),
                kind: 1,
                height: 10,
                position: 0,
            },
            TreeLeaf {
                hash: String::new(),
                kind: 2,
                height: 10,
                position: 0,
            },
            TreeLeaf {
                hash: "22".repeat(32),
                kind: 0,
                height: 11,
                position: 0,
            },
        ];
        let next = migrate_legacy_leaf_positions(&mut leaves, 5, 5);
        assert_eq!(next, 7);
        assert_eq!(leaves[0].position, 5);
        assert_eq!(leaves[2].position, 6);
        assert!(
            !offload::is_sparse_leaves(2, 5, next),
            "legacy dense must not look like selective shard scanning"
        );
    }

    #[test]
    fn migrate_legacy_leaves_selective_scan_positions_alone() {
        let mut leaves = vec![TreeLeaf {
            hash: "11".repeat(32),
            kind: 1,
            height: 10,
            position: 70_000,
        }];
        let next = migrate_legacy_leaf_positions(&mut leaves, 0, 80_000);
        assert_eq!(next, 80_000);
        assert_eq!(leaves[0].position, 70_000);
    }

    #[test]
    fn rewind_below_birthday_resets_next_to_base() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.sapling_next = SHARD_SIZE * 3;
        w.tree_sizes.push(TreeSizeMark {
            height: 1,
            sapling: SHARD_SIZE * 3,
            orchard: 0,
            ironwood: 0,
        });
        let h = w.rewind_to_height(0).unwrap();
        assert_eq!(h, 0);
        assert_eq!(w.sapling_next, w.sapling_base);
        assert!(w.tree_sizes.is_empty());
    }

    #[test]
    fn from_snapshot_migrates_pre_selective_scan_zstk1_leaves() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        let hash = to_hex(&sapling::Node::empty_leaf().to_bytes());
        w.sapling_leaves.push(TreeLeaf {
            hash,
            kind: 1,
            height: 1,
            position: 0,
        });
        let mut v: serde_json::Value = serde_json::from_slice(&w.to_snapshot().unwrap()).unwrap();
        v["saplingNext"] = serde_json::json!(0);
        v["version"] = serde_json::json!(1);
        if let Some(leaves) = v["saplingLeaves"].as_array_mut() {
            for leaf in leaves {
                leaf["position"] = serde_json::json!(0);
            }
        }
        let restored = WebWallet::from_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        let hashed = restored
            .sapling_leaves
            .iter()
            .filter(|l| !l.hash.is_empty())
            .collect::<Vec<_>>();
        assert!(!hashed.is_empty());
        assert_eq!(hashed[0].position, restored.sapling_base);
        assert_eq!(
            restored.sapling_next,
            restored.sapling_base + hashed.len() as u64
        );
        assert!(!offload::is_sparse_leaves(
            hashed.len() as u64,
            restored.sapling_base,
            restored.sapling_next
        ));
    }

    #[test]
    fn rewind_restores_next_from_size_mark_not_hashed_count() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        let b3 = empty_block(3, b2.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.apply_compact_block(&b3.encode_to_vec()).unwrap();
        w.sapling_next = 70_010;
        w.tree_sizes = vec![
            TreeSizeMark {
                height: 1,
                sapling: 10,
                orchard: 0,
                ironwood: 0,
            },
            TreeSizeMark {
                height: 2,
                sapling: 70_000,
                orchard: 0,
                ironwood: 0,
            },
            TreeSizeMark {
                height: 3,
                sapling: 70_010,
                orchard: 0,
                ironwood: 0,
            },
        ];
        w.rewind_to_height(2).unwrap();
        assert_eq!(w.scanned_height(), 2);
        assert_eq!(w.sapling_next, 70_000);
        assert_eq!(w.tree_sizes.last().map(|m| m.height), Some(2));
    }

    #[test]
    fn rewind_without_hash_resets_to_birthday() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.block_hashes.clear();
        w.sapling_next = 70_000;
        let h = w.rewind_to_height(2).unwrap();
        assert_eq!(h, 0);
        assert_eq!(w.sapling_next, w.sapling_base);
        assert!(w.notes().is_empty());
    }

    #[test]
    fn rewind_sparse_without_marks_keeps_hash_height() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.tree_sizes.clear();
        w.sapling_next = 70_000;
        let h = w.rewind_to_height(2).unwrap();
        assert_eq!(h, 2, "hash at 2 must not fail-close to birthday");
        assert_eq!(w.scanned_height(), 2);
    }

    #[test]
    fn rewind_missing_height_uses_nearest_hash() {
        let mut w = wallet();
        let b1 = empty_block(1, vec![0u8; 32]);
        let b2 = empty_block(2, b1.hash.clone());
        w.apply_compact_block(&b1.encode_to_vec()).unwrap();
        w.apply_compact_block(&b2.encode_to_vec()).unwrap();
        w.block_hashes.remove(&2);
        let h = w.rewind_to_height(2).unwrap();
        assert_eq!(h, 1, "walk back to height 1, not birthday");
        assert_eq!(w.scanned_height(), 1);
    }

    #[test]
    fn apply_subtree_roots_incremental_appends() {
        let mut w = wallet();
        let h0 = "11".repeat(32);
        let h1 = "22".repeat(32);
        let n0 = w
            .apply_subtree_roots_json(
                "sapling",
                &format!(r#"{{"roots":[{{"completingHeight":100,"rootHash":"{h0}"}}]}}"#),
            )
            .unwrap();
        assert_eq!(n0, 1);
        assert_eq!(w.subtree_root_count("sapling"), 1);
        let n1 = w
            .apply_subtree_roots_json(
                "sapling",
                &format!(
                    r#"{{"startIndex":1,"roots":[{{"completingHeight":200,"rootHash":"{h1}"}}]}}"#
                ),
            )
            .unwrap();
        assert_eq!(n1, 1);
        assert_eq!(w.subtree_root_count("sapling"), 2);
        assert_eq!(w.sapling_roots[0].root_hash, h0);
        assert_eq!(w.sapling_roots[1].root_hash, h1);
    }

    #[test]
    fn roots_from_birthday_shard_drop_interior_unmarked_shards() {
        let mut w = wallet();
        let base = 3 * SHARD_SIZE + 100;
        w.orchard_base = base;
        w.orchard_next = base;
        assert_eq!(
            w.subtree_roots_start("orchard"),
            3,
            "birthday shard, not shard 0"
        );
        let leaf = MerkleHashOrchard::empty_leaf();
        let hex = to_hex(&leaf.to_bytes());
        w.apply_subtree_roots_json(
            "orchard",
            &format!(
                r#"{{"startIndex":3,"roots":[{{"completingHeight":100,"rootHash":"{hex}"}},{{"completingHeight":200,"rootHash":"{hex}"}}]}}"#
            ),
        )
        .unwrap();
        assert_eq!(w.subtree_root_count("orchard"), 5);
        assert!(w.orchard_roots[..3]
            .iter()
            .all(StoredSubtreeRoot::is_placeholder));
        assert_eq!(w.subtree_roots_start("orchard"), 5);
        // Placeholders are skipped, never parsed as roots.
        w.seed_tree_caps().unwrap();

        // Birthday shard 3 is kept, shard 4 is complete, unmarked and has a
        // root, so it is dropped; shard 5 is still open.
        let fill = |n: u64| vec![(leaf, Retention::Ephemeral); n as usize];
        w.append_orchard(10, &fill(4 * SHARD_SIZE - base)).unwrap();
        w.append_orchard(20, &fill(SHARD_SIZE)).unwrap();
        w.append_orchard(30, &fill(5)).unwrap();
        assert_eq!(w.offload.as_ref().unwrap().dropped_shards(), 1);
        assert!(w
            .orchard_leaves
            .iter()
            .all(|l| offload::shard_of(l.position) == 3));
        assert_eq!(w.orchard_next, 5 * SHARD_SIZE + 5);
    }

    #[test]
    fn settled_shards_are_pruned_once_below_the_rewind_window() {
        let mut w = wallet();
        let hex = to_hex(&MerkleHashOrchard::empty_leaf().to_bytes());
        let root = format!(r#"{{"completingHeight":100,"rootHash":"{hex}"}}"#);
        w.apply_subtree_roots_json(
            "orchard",
            &format!(r#"{{"startIndex":0,"roots":[{root},{root},{root},{root},{root}]}}"#),
        )
        .unwrap();
        let floor = 1_000;
        w.block_hashes = (floor..floor + 3).map(|h| (h, "00".into())).collect();
        w.scanned_height = floor + 2;
        w.orchard_base = 0;
        w.orchard_next = 5 * SHARD_SIZE + 1;
        let leaf = |position: u64, kind: u8, height: u32| TreeLeaf {
            hash: hex.clone(),
            kind,
            height,
            position,
        };
        let s = SHARD_SIZE;
        w.orchard_leaves = vec![
            leaf(7, 1, 10),            // note spent below the window: pruned
            leaf(s + 7, 3, 20),        // unspent note
            leaf(2 * s + 7, 1, 30),    // spent inside the window
            leaf(3 * s + 7, 0, 40),    // no notes, but a rewind may land
            leaf(3 * s + 8, 2, floor), // on this checkpoint
            leaf(4 * s + 7, 0, 50),    // no notes, below the window: pruned
            leaf(5 * s, 0, floor + 1), // open tip shard
        ];
        let note = |mined: u32, spent_height: Option<u32>| TrackedNote {
            pool: "orchard".into(),
            txid: format!("{mined:064x}"),
            output_index: 0,
            value_zat: 1,
            nf: String::new(),
            is_change: false,
            spent: spent_height.is_some(),
            spent_in: spent_height.map(|h| format!("{h:064x}")),
            spent_height,
            mined_height: mined,
        };
        w.notes = vec![
            note(10, Some(15)),
            note(20, None),
            note(30, Some(floor + 1)),
        ];
        w.assert_consistent_leaf_hashes().unwrap();

        w.prune_settled_shards();
        let shards: Vec<u64> = w
            .orchard_leaves
            .iter()
            .map(|l| offload::shard_of(l.position))
            .collect();
        assert_eq!(shards, [1, 2, 3, 3, 5]);
        w.assert_consistent_leaf_hashes().unwrap();

        // Unknown roots keep every shard, even a settled one.
        let mut unrooted = wallet();
        unrooted.block_hashes = w.block_hashes.clone();
        unrooted.notes = w.notes.clone();
        unrooted.orchard_next = w.orchard_next;
        unrooted.orchard_leaves = vec![leaf(7, 1, 10), leaf(5 * s, 0, floor + 1)];
        unrooted.prune_settled_shards();
        assert_eq!(unrooted.orchard_leaves.len(), 2);
        // A wallet that originally scanned without server roots is repaired
        // in place when roots become available, including at the same tip.
        unrooted
            .apply_subtree_roots_json("orchard", &format!(r#"{{"roots":[{root}]}}"#))
            .unwrap();
        assert_eq!(unrooted.orchard_leaves.len(), 1);
        assert_eq!(unrooted.orchard_leaves[0].position, 5 * s);
    }

    #[test]
    fn hydrate_finalize_skips_complete_shards() {
        let mut w = wallet();
        let h0 = to_hex(&sapling::Node::empty_leaf().to_bytes());
        w.apply_subtree_roots_json(
            "sapling",
            &format!(r#"{{"roots":[{{"completingHeight":100,"rootHash":"{h0}"}}]}}"#),
        )
        .unwrap();
        w.sapling_next = SHARD_SIZE + 2;
        w.sapling_base = 0;
        w.sapling_leaves = vec![
            TreeLeaf {
                hash: h0.clone(),
                kind: 0,
                height: 10,
                position: 0,
            },
            TreeLeaf {
                hash: h0,
                kind: 0,
                height: 20,
                position: SHARD_SIZE,
            },
        ];
        w.scanned_height = 20;
        let bytes = w.to_snapshot().unwrap();
        let mut restored = WebWallet::from_snapshot(&bytes).unwrap();
        assert!(
            !restored.sinsemilla_live,
            "hydrate must not replay shardtrees"
        );
        assert!(offload::is_sparse_leaves(
            hashed_leaf_count(&restored.sapling_leaves),
            restored.sapling_base,
            restored.sapling_next
        ));
        restored.finalize_scan_trees().unwrap();
        assert!(restored.sinsemilla_live);
        restored.finalize_scan_trees().unwrap();
        assert!(restored.sinsemilla_live);
    }

    #[test]
    fn hydrate_paints_note_balance_before_sinsemilla() {
        let mut w = wallet();
        w.scanned_height = 3_472_882;
        w.notes.push(TrackedNote {
            pool: "orchard".into(),
            txid: "aa".into(),
            output_index: 0,
            value_zat: 701_929,
            nf: String::new(),
            is_change: false,
            spent: false,
            spent_in: None,
            spent_height: None,
            mined_height: 3_424_719,
        });
        w.recompute_pool_fields();
        assert_eq!(w.balance().orchard_available, 701_929);
        assert_eq!(w.balance().total_pending, 0);
        let bytes = w.to_snapshot().unwrap();
        let restored = WebWallet::from_snapshot(&bytes).unwrap();
        assert!(
            !restored.sinsemilla_live,
            "hydrate must not wait on shard hashing for ZEC"
        );
        assert_eq!(restored.balance().orchard_available, 701_929);
        assert_eq!(restored.balance().total_available, 701_929);
        assert_eq!(restored.balance().total_pending, 0);
        assert_eq!(
            restored.wallet_snapshot("wasm")["balance"]["orchardAvailable"],
            701_929
        );
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        assert!(
            head.contains("\"orchardAvailable\":701929"),
            "note totals must sit in the snapshot header for a 512KB peek"
        );
        assert!(
            head.find("orchardAvailable").unwrap() < head.find("\"notes\"").unwrap(),
            "header totals must precede the notes array"
        );
    }

    #[test]
    fn finalize_skips_unused_sapling_and_ironwood() {
        let mut w = wallet();
        let sap = to_hex(&sapling::Node::empty_leaf().to_bytes());
        let orch = to_hex(&MerkleHashOrchard::empty_leaf().to_bytes());
        w.sapling_leaves = (0..2_048)
            .map(|i| TreeLeaf {
                hash: sap.clone(),
                kind: 0,
                height: 10,
                position: i,
            })
            .collect();
        w.sapling_next = 4_096; // An unused sparse prefix needs no witness.
        w.ironwood_leaves = (0..2_048)
            .map(|i| TreeLeaf {
                hash: orch.clone(),
                kind: 0,
                height: 10,
                position: i,
            })
            .collect();
        w.ironwood_next = 2_048;
        w.orchard_leaves = vec![TreeLeaf {
            hash: orch,
            kind: 1,
            height: 10,
            position: 0,
        }];
        w.orchard_next = 1;
        w.notes.push(TrackedNote {
            pool: "orchard".into(),
            txid: "aa".into(),
            output_index: 0,
            value_zat: 1,
            nf: String::new(),
            is_change: false,
            spent: false,
            spent_in: None,
            spent_height: None,
            mined_height: 10,
        });
        w.scanned_height = 20;
        let mut msgs = Vec::new();
        w.finalize_scan_trees_ticking(|t| msgs.push(t.message.clone()))
            .unwrap();
        assert!(w.sinsemilla_live);
        assert!(
            msgs.iter()
                .all(|m| !m.contains("sapling") && !m.contains("ironwood")),
            "unused pools must not appear in overlay ticks: {msgs:?}"
        );
        assert!(
            shards_to_hash(&w.sapling_leaves, 0, false).is_empty(),
            "unused sapling with completed=0 must not hash the frontier"
        );
    }

    #[test]
    fn finalize_without_roots_hashes_only_note_shards() {
        let mut leaves: Vec<TreeLeaf> = (0..64)
            .map(|i| TreeLeaf {
                hash: "11".repeat(32),
                kind: 0,
                height: 10,
                position: SHARD_SIZE + i,
            })
            .collect();
        leaves.push(TreeLeaf {
            hash: "22".repeat(32),
            kind: 1,
            height: 10,
            position: 50,
        });
        let shards = shards_to_hash(&leaves, 0, true);
        assert_eq!(
            shards.len(),
            1,
            "completed=0 must not treat every shard as open"
        );
        assert!(shards.contains(&0));
        assert_eq!(count_leaves_in_shards(&leaves, &shards), 1);
        assert!(shards_to_hash(&leaves[..64], 0, false).is_empty());
    }

    #[test]
    fn finalize_disagrees_on_duplicate_leaf() {
        let mut w = wallet();
        let h0 = to_hex(&sapling::Node::empty_leaf().to_bytes());
        let h1 = to_hex(&sapling::Node::empty_root(Level::from(1)).to_bytes());
        w.sapling_leaves = vec![
            TreeLeaf {
                hash: h0,
                kind: 1,
                height: 10,
                position: 100,
            },
            TreeLeaf {
                hash: h1,
                kind: 0,
                height: 11,
                position: 100,
            },
        ];
        w.sapling_next = 200;
        w.scanned_height = 20;
        let err = w.finalize_scan_trees().unwrap_err().to_string();
        assert!(err.contains("selective-scan tree conflict"), "{err}");
        assert!(err.contains("100"), "{err}");
        assert!(err.contains("Wipe scan"), "{err}");
        assert_eq!(
            w.sapling_leaves.len(),
            2,
            "conflict must not drain retained leaves"
        );
        assert!(
            w.to_snapshot().is_err(),
            "failed rebuild must not be persisted"
        );
        assert!(w.finalize_scan_trees().is_err());
        w.reset_scan();
        assert!(w.to_snapshot().is_ok(), "reset recovers a failed scan");
    }

    #[test]
    fn hydrate_repairs_clobbered_next() {
        let mut w = wallet();
        w.orchard_next = 50_081_105;
        w.orchard_base = 50_081_105;
        w.scanned_height = 20;
        w.tree_sizes.push(TreeSizeMark {
            height: 20,
            sapling: 0,
            orchard: 50_200_000,
            ironwood: 0,
        });
        w.orchard_leaves.push(TreeLeaf {
            hash: to_hex(&MerkleHashOrchard::empty_leaf().to_bytes()),
            kind: 0,
            height: 20,
            position: 50_199_999,
        });
        let restored = WebWallet::from_snapshot(&w.to_snapshot().unwrap()).unwrap();
        assert_eq!(restored.orchard_next, 50_200_000);
    }

    #[test]
    fn apply_subtree_roots_skips_matching_cap() {
        let mut w = wallet();
        let h0 = "11".repeat(32);
        let json = format!(r#"{{"roots":[{{"completingHeight":100,"rootHash":"{h0}"}}]}}"#);
        assert_eq!(w.apply_subtree_roots_json("sapling", &json).unwrap(), 1);
        assert_eq!(w.apply_subtree_roots_json("sapling", &json).unwrap(), 1);
        assert_eq!(w.subtree_root_count("sapling"), 1);
    }

    #[test]
    fn apply_subtree_roots_fails_on_disagreement() {
        let mut w = wallet();
        let h0 = "11".repeat(32);
        let h1 = "22".repeat(32);
        w.apply_subtree_roots_json(
            "sapling",
            &format!(r#"{{"roots":[{{"completingHeight":100,"rootHash":"{h0}"}}]}}"#),
        )
        .unwrap();
        let err = w
            .apply_subtree_roots_json(
                "sapling",
                &format!(r#"{{"roots":[{{"completingHeight":100,"rootHash":"{h1}"}}]}}"#),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("selective-scan tree conflict") || err.contains("Inserted root conflicts"),
            "{err}"
        );
        assert!(err.contains("Wipe scan") || err.contains("rescan"), "{err}");
        assert!(
            w.to_snapshot().is_err(),
            "failed root import must not be persisted"
        );
    }

    #[test]
    fn finalize_does_not_reset_next_after_scan() {
        let mut w = wallet();
        w.scanned_height = 20;
        w.orchard_next = 50_200_000;
        w.orchard_leaves.push(TreeLeaf {
            hash: to_hex(&MerkleHashOrchard::empty_leaf().to_bytes()),
            kind: 0,
            height: 20,
            position: 50_199_999,
        });
        assert!(!w.should_adopt_frontier_sizes());
        let next = w.orchard_next;
        w.finalize_scan_trees().unwrap();
        assert_eq!(w.orchard_next, next);
    }
}
