//! WebAssembly bindings for `z-engine` keys, compact-block scan, history, and orchard send.
//!
//! Sapling params stay out of this crate. Compact-block **fetch** and broadcast are JS.

#![deny(unsafe_code)]

use wasm_bindgen::prelude::*;
use z_engine::keys::{self, AccountKeys};
use z_engine::{crypto_smoke, LightServer, Network};

/// Rayon thread pool for trial-decrypt / proving. Requires the `multicore` feature
/// and a SharedArrayBuffer (COOP/COEP). Same entry as legacy WebZjs.
#[cfg(feature = "multicore")]
pub use wasm_bindgen_rayon::init_thread_pool;

#[cfg_attr(
    not(all(target_arch = "wasm32", target_feature = "atomics")),
    allow(dead_code)
)]
mod thread_cache;

/// Every Rayon worker allocates while decoding and decrypting compact blocks;
/// std's wasm allocator serializes them on one lock. See `thread_cache`.
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
#[global_allocator]
static ALLOC: thread_cache::ThreadCache<std::alloc::System> =
    thread_cache::ThreadCache::new(std::alloc::System);

#[wasm_bindgen(start)]
pub fn start() {
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
}

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

fn parse_network(network: &str) -> Result<Network, JsValue> {
    Network::parse(network).ok_or_else(|| js_err(format!("unknown network: {network}")))
}

/// 24-word English mnemonic (32 bytes entropy).
#[wasm_bindgen(js_name = generateMnemonic)]
pub fn generate_mnemonic() -> Result<String, JsValue> {
    keys::generate_mnemonic().map_err(js_err)
}

/// UFVK + default unified address (+ transparent receiver) from a mnemonic.
#[wasm_bindgen(js_name = accountFromMnemonic)]
pub fn account_from_mnemonic(
    mnemonic: &str,
    network: &str,
    account_index: u32,
) -> Result<WasmAccount, JsValue> {
    let network = parse_network(network)?;
    let acct = keys::account_from_mnemonic(mnemonic, network, account_index).map_err(js_err)?;
    Ok(WasmAccount::from(acct))
}

/// ZIP-32 seed fingerprint (64 hex digits) of a BIP-39 mnemonic.
#[wasm_bindgen(js_name = seedFingerprint)]
pub fn seed_fingerprint(mnemonic: &str) -> Result<String, JsValue> {
    keys::seed_fingerprint(mnemonic).map_err(js_err)
}

/// Error unless `ufvk` is a valid unified viewing key for `network`.
#[wasm_bindgen(js_name = checkViewingKey)]
pub fn check_viewing_key(network: &str, ufvk: &str) -> Result<(), JsValue> {
    let network = parse_network(network)?;
    keys::account_from_ufvk(ufvk.trim(), network, 0)
        .map(|_| ())
        .map_err(js_err)
}

/// True when `derived` (from a mnemonic) covers every FVK item in `stored`.
#[wasm_bindgen(js_name = ufvkCovers)]
pub fn ufvk_covers(network: &str, derived: &str, stored: &str) -> Result<(), JsValue> {
    let network = parse_network(network)?;
    keys::derived_ufvk_covers(network, derived, stored).map_err(js_err)
}

/// `{ network, kind }` for a UA / t-addr / sapling / TEX string.
#[wasm_bindgen(js_name = parseAddress)]
pub fn parse_address(encoded: &str) -> Result<JsValue, JsValue> {
    let parsed = keys::parse_address(encoded).map_err(js_err)?;
    serde_wasm_bindgen::to_value(&parsed).map_err(js_err)
}

/// `{ network, kind, receivers, receiverSet }` — no wallet required.
#[wasm_bindgen(js_name = inspectAddress)]
pub fn inspect_address(encoded: &str) -> Result<JsValue, JsValue> {
    let inspected = keys::inspect_address(encoded).map_err(js_err)?;
    serde_wasm_bindgen::to_value(&inspected).map_err(js_err)
}

/// UA from a UFVK for `full` / `orchard` / `shielded`. No snapshot write.
#[wasm_bindgen(js_name = unifiedAddressForSet)]
pub fn unified_address_for_set(ufvk: &str, network: &str, set: &str) -> Result<String, JsValue> {
    let network = parse_network(network)?;
    let set = keys::UaReceiverSet::parse(set)
        .ok_or_else(|| js_err(format!("unknown UA receiver set: {set}")))?;
    keys::unified_address_for_set(ufvk, network, set).map_err(js_err)
}

#[wasm_bindgen(js_name = cryptoSmoke)]
pub fn wasm_crypto_smoke() -> String {
    crypto_smoke().to_string()
}

#[wasm_bindgen]
pub struct WasmAccount {
    network: String,
    account_index: u32,
    unified_address: String,
    ufvk: String,
    transparent_address: Option<String>,
}

impl From<AccountKeys> for WasmAccount {
    fn from(a: AccountKeys) -> Self {
        Self {
            network: a.network.as_str().into(),
            account_index: a.account_index,
            unified_address: a.unified_address,
            ufvk: a.ufvk,
            transparent_address: a.transparent_address,
        }
    }
}

#[wasm_bindgen]
impl WasmAccount {
    #[wasm_bindgen(getter)]
    pub fn network(&self) -> String {
        self.network.clone()
    }

    #[wasm_bindgen(getter, js_name = accountIndex)]
    pub fn account_index(&self) -> u32 {
        self.account_index
    }

    #[wasm_bindgen(getter, js_name = unifiedAddress)]
    pub fn unified_address(&self) -> String {
        self.unified_address.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn ufvk(&self) -> String {
        self.ufvk.clone()
    }

    #[wasm_bindgen(getter, js_name = transparentAddress)]
    pub fn transparent_address(&self) -> Option<String> {
        self.transparent_address.clone()
    }
}

#[wasm_bindgen]
pub struct WasmEngine {
    network: Network,
    server: LightServer,
}

#[wasm_bindgen]
impl WasmEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(network: &str, server_url: Option<String>) -> Result<WasmEngine, JsValue> {
        let network = parse_network(network)?;
        let server = match server_url {
            Some(u) if !u.trim().is_empty() => LightServer::Url(u),
            _ => match network {
                Network::Regtest => LightServer::LocalRegtest,
                // Hosted wasm (later) may use gRPC-Web. Local web uses z-desktop / z-wallet serve.
                _ => LightServer::Url(LightServer::LOCAL_ZAINO_GRPC_WEB.into()),
            },
        };
        Ok(Self { network, server })
    }

    #[wasm_bindgen(js_name = network)]
    pub fn network(&self) -> String {
        self.network.as_str().into()
    }

    #[wasm_bindgen(js_name = serverUrl)]
    pub fn server_url(&self) -> String {
        self.server.as_url()
    }

    #[wasm_bindgen(js_name = cryptoSmoke)]
    pub fn crypto_smoke() -> String {
        crypto_smoke().to_string()
    }

    #[wasm_bindgen(js_name = generateMnemonic)]
    pub fn generate_mnemonic(&self) -> Result<String, JsValue> {
        keys::generate_mnemonic().map_err(js_err)
    }

    #[wasm_bindgen(js_name = accountFromMnemonic)]
    pub fn account_from_mnemonic(
        &self,
        mnemonic: &str,
        account_index: u32,
    ) -> Result<WasmAccount, JsValue> {
        let acct =
            keys::account_from_mnemonic(mnemonic, self.network, account_index).map_err(js_err)?;
        Ok(WasmAccount::from(acct))
    }
}

/// In-tab wallet: keys + compact-block scan + `HistoryEntry`. Fetch stays in JS.
#[wasm_bindgen]
pub struct WasmWallet {
    inner: z_engine::web::WebWallet,
}

#[wasm_bindgen]
impl WasmWallet {
    #[wasm_bindgen(js_name = create)]
    pub fn create(
        network: &str,
        mnemonic: &str,
        birthday: u32,
        account_index: u32,
    ) -> Result<WasmWallet, JsValue> {
        let network = parse_network(network)?;
        let acct = keys::account_from_mnemonic(mnemonic, network, account_index).map_err(js_err)?;
        let inner = z_engine::web::WebWallet::from_account(acct, birthday).map_err(js_err)?;
        Ok(Self { inner })
    }

    #[wasm_bindgen(js_name = fromSnapshot)]
    pub fn from_snapshot(bytes: &[u8]) -> Result<WasmWallet, JsValue> {
        Ok(Self {
            inner: z_engine::web::WebWallet::from_snapshot(bytes).map_err(js_err)?,
        })
    }

    #[wasm_bindgen(js_name = fromUfvk)]
    pub fn from_ufvk(
        network: &str,
        ufvk: &str,
        birthday: u32,
        account_index: u32,
    ) -> Result<WasmWallet, JsValue> {
        let network = parse_network(network)?;
        Ok(Self {
            inner: z_engine::web::WebWallet::from_ufvk(network, ufvk, birthday, account_index)
                .map_err(js_err)?,
        })
    }

    /// A hardware-wallet account (Keystone or Ledger): the device's viewing key
    /// plus what its PCZTs must carry. Spends go through `hardware*`.
    #[wasm_bindgen(js_name = fromHardware)]
    pub fn from_hardware(
        network: &str,
        ufvk: &str,
        birthday: u32,
        device: &str,
        seed_fingerprint: &str,
        account_index: u32,
    ) -> Result<WasmWallet, JsValue> {
        let network = parse_network(network)?;
        let hardware = z_engine::web::HardwareAccount {
            device: device.into(),
            seed_fingerprint: seed_fingerprint.into(),
            account_index,
        };
        Ok(Self {
            inner: z_engine::web::WebWallet::from_hardware(network, ufvk, birthday, hardware)
                .map_err(js_err)?,
        })
    }

    /// `{ device, seedFingerprint, accountIndex }` JSON, or undefined for a seed or view-only wallet.
    #[wasm_bindgen(js_name = hardwareAccount)]
    pub fn hardware_account(&self) -> Option<String> {
        self.inner
            .hardware()
            .map(|h| serde_json::to_string(h).expect("hardware account serializes"))
    }

    /// PCZT for a send (before proofs and signatures). Its notes stay reserved
    /// until it is finalized, released, or expires.
    #[wasm_bindgen(js_name = hardwareCreateSend)]
    pub fn hardware_create_send(
        &mut self,
        to: &str,
        amount_zec: &str,
        memo: Option<String>,
    ) -> Result<Vec<u8>, JsValue> {
        self.inner
            .hardware_create_send(to, amount_zec, memo.as_deref())
            .map_err(js_err)
    }

    /// Adds the Orchard/Ironwood proofs. Needs no wallet state.
    #[wasm_bindgen(js_name = hardwareProve)]
    pub fn hardware_prove(pczt: &[u8]) -> Result<Vec<u8>, JsValue> {
        z_engine::web::hardware_prove(pczt).map_err(js_err)
    }

    /// The redacted copy to hand a device: `"full"`, `"compact"` or `"batch"`.
    #[wasm_bindgen(js_name = hardwareSignerCopy)]
    pub fn hardware_signer_copy(pczt: &[u8], copy: &str) -> Result<Vec<u8>, JsValue> {
        let copy = z_engine::web::SignerCopy::parse(copy).map_err(js_err)?;
        z_engine::web::hardware_signer_copy(pczt, copy).map_err(js_err)
    }

    /// Applies `[{ pool, actionIndex, signature }]` (hex) from a device; each is verified.
    #[wasm_bindgen(js_name = hardwareApplySignatures)]
    pub fn hardware_apply_signatures(
        pczt: &[u8],
        signatures_json: &str,
    ) -> Result<Vec<u8>, JsValue> {
        let sigs: Vec<z_engine::web::DeviceSignature> =
            serde_json::from_str(signatures_json).map_err(js_err)?;
        z_engine::web::hardware_apply_signatures(pczt, &sigs).map_err(js_err)
    }

    /// Merges a device-signed PCZT (Keystone) into the proved one.
    #[wasm_bindgen(js_name = hardwareCombine)]
    pub fn hardware_combine(proved: &[u8], signed: &[u8]) -> Result<Vec<u8>, JsValue> {
        z_engine::web::hardware_combine(proved, signed).map_err(js_err)
    }

    /// Finalizes a proved, signed PCZT and records it as pending. Returns `{ txid, hex }`.
    #[wasm_bindgen(js_name = hardwareFinalize)]
    pub fn hardware_finalize(&mut self, pczt: &[u8]) -> Result<String, JsValue> {
        let done = self.inner.hardware_finalize(pczt).map_err(js_err)?;
        serde_json::to_string(&done).map_err(js_err)
    }

    /// Releases the notes reserved by unfinished PCZTs (signing was cancelled).
    #[wasm_bindgen(js_name = hardwareReleaseLocks)]
    pub fn hardware_release_locks(&mut self) -> u32 {
        self.inner.hardware_release_locks() as u32
    }

    /// Ledger: `{ commands: [{cla, ins, p1, p2, data}], reviewIndex, signatures }` for this PCZT.
    #[wasm_bindgen(js_name = ledgerSigningPlan)]
    pub fn ledger_signing_plan(&self, pczt: &[u8], app_version: &str) -> Result<String, JsValue> {
        let plan = self
            .inner
            .ledger_signing_plan(pczt, app_version)
            .map_err(js_err)?;
        serde_json::to_string(&plan).map_err(js_err)
    }

    /// Ledger: applies the raw responses (hex, status words included) to a plan for `pczt`.
    #[wasm_bindgen(js_name = ledgerApplyResponses)]
    pub fn ledger_apply_responses(
        &self,
        pczt: &[u8],
        responses_json: &str,
    ) -> Result<Vec<u8>, JsValue> {
        let responses = hex_list(responses_json)?;
        self.inner
            .ledger_apply_responses(pczt, &responses)
            .map_err(js_err)
    }

    #[wasm_bindgen(js_name = toSnapshot)]
    pub fn to_snapshot(&self) -> Result<Vec<u8>, JsValue> {
        self.inner.to_snapshot().map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyCompactBlock)]
    pub fn apply_compact_block(&mut self, bytes: &[u8]) -> Result<String, JsValue> {
        let d = self.inner.apply_compact_block(bytes).map_err(js_err)?;
        serde_json::to_string(&d).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyCompactBlocks)]
    pub fn apply_compact_blocks(&mut self, blob: &[u8]) -> Result<String, JsValue> {
        let d = self.inner.apply_compact_blocks_blob(blob).map_err(js_err)?;
        serde_json::to_string(&d).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyCompactBlocksSummary)]
    pub fn apply_compact_blocks_summary(&mut self, blob: &[u8]) -> Result<String, JsValue> {
        let d = self
            .inner
            .apply_compact_blocks_summary(blob)
            .map_err(js_err)?;
        serde_json::to_string(&d).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyTransparentBlocks)]
    pub fn apply_transparent_blocks(&mut self, blob: &[u8]) -> Result<u32, JsValue> {
        self.inner.apply_transparent_blocks(blob).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applySharedMemos)]
    pub fn apply_shared_memos(&mut self, json: &str) -> Result<u32, JsValue> {
        self.inner.apply_shared_memos(json).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyTreeState)]
    pub fn apply_tree_state(&mut self, json: &str) -> Result<(), JsValue> {
        self.inner.apply_tree_state_json(json).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applySubtreeRoots)]
    pub fn apply_subtree_roots(&mut self, protocol: &str, json: &str) -> Result<u32, JsValue> {
        self.inner
            .apply_subtree_roots_json(protocol, json)
            .map_err(js_err)
    }

    #[wasm_bindgen(js_name = treesReady)]
    pub fn trees_ready(&self) -> bool {
        self.inner.trees_ready()
    }

    #[wasm_bindgen(js_name = sinsemillaLive)]
    pub fn sinsemilla_live(&self) -> bool {
        self.inner.sinsemilla_live()
    }

    #[wasm_bindgen(js_name = subtreeRootCount)]
    pub fn subtree_root_count(&self, protocol: &str) -> u32 {
        self.inner.subtree_root_count(protocol)
    }

    /// First shard index to request from `GetSubtreeRoots`: the birthday
    /// shard once `applyTreeState` ran, never shard 0 on a fresh restore.
    #[wasm_bindgen(js_name = subtreeRootsStart)]
    pub fn subtree_roots_start(&self, protocol: &str) -> u32 {
        self.inner.subtree_roots_start(protocol)
    }

    #[wasm_bindgen(js_name = recomputePools)]
    pub fn recompute_pools(&mut self) -> Result<(), JsValue> {
        self.inner.finalize_scan_trees().map_err(js_err)
    }

    /// Hash open shards plus marked note leaves. Progress `(hashed, total, message)`.
    #[wasm_bindgen(js_name = recomputePoolsWithTick)]
    pub fn recompute_pools_with_tick(&mut self, cb: &js_sys::Function) -> Result<(), JsValue> {
        self.inner
            .finalize_scan_trees_ticking(|t| {
                let _ = cb.call3(
                    &JsValue::NULL,
                    &JsValue::from_f64(t.hashed as f64),
                    &JsValue::from_f64(t.total as f64),
                    &JsValue::from_str(&t.message),
                );
            })
            .map_err(js_err)
    }

    #[wasm_bindgen(js_name = history)]
    pub fn history(&self, limit: u32) -> String {
        self.inner.history_json(limit as usize).to_string()
    }

    /// `status` is `mined` | `pending` | `expired` (empty = all). `txid` empty = no tx filter.
    /// Filtered before the limit, same cap as `history` (500).
    #[wasm_bindgen(js_name = historyQuery)]
    pub fn history_query(&self, limit: u32, status: &str, txid: &str) -> String {
        let status = if status.trim().is_empty() {
            None
        } else {
            Some(status)
        };
        let txid = if txid.trim().is_empty() {
            None
        } else {
            Some(txid)
        };
        self.inner
            .history_query_json(limit as usize, status, txid)
            .to_string()
    }

    #[wasm_bindgen(js_name = snapshotJson)]
    pub fn snapshot_json(&self, server: &str) -> String {
        self.inner.wallet_snapshot(server).to_string()
    }

    #[wasm_bindgen(js_name = scannedHeight)]
    pub fn scanned_height(&self) -> u32 {
        self.inner.scanned_height()
    }

    #[wasm_bindgen(js_name = birthday)]
    pub fn birthday(&self) -> u32 {
        self.inner.birthday()
    }

    #[wasm_bindgen(js_name = nextHeight)]
    pub fn next_height(&self) -> u32 {
        self.inner.next_height()
    }

    #[wasm_bindgen(js_name = unifiedAddress)]
    pub fn unified_address(&self) -> String {
        self.inner.unified_address().to_string()
    }

    #[wasm_bindgen(js_name = transparentAddress)]
    pub fn transparent_address(&self) -> Option<String> {
        self.inner.transparent_address().map(str::to_string)
    }

    #[wasm_bindgen(js_name = nextUnifiedAddress)]
    pub fn next_unified_address(&mut self) -> Result<String, JsValue> {
        self.inner.next_unified_address().map_err(js_err)
    }

    /// Verify a mnemonic matches this wallet's UFVK and clear view-only.
    #[wasm_bindgen(js_name = attachSeed)]
    pub fn attach_seed(&mut self, mnemonic: &str) -> Result<(), JsValue> {
        self.inner.attach_seed(mnemonic).map_err(js_err)
    }

    #[wasm_bindgen(js_name = proveError)]
    pub fn prove_error() -> String {
        z_engine::web::PROVE_NOT_READY.to_string()
    }

    #[wasm_bindgen(js_name = shieldError)]
    pub fn shield_error() -> String {
        z_engine::web::SHIELD_NOT_READY.to_string()
    }

    /// ZIP-317 fee for a send / ZIP-321 URI. Propose only. Returns `{ feeZat, feeZec }`.
    #[wasm_bindgen(js_name = estimateFee)]
    pub fn estimate_fee(
        &mut self,
        to: &str,
        amount_zec: &str,
        memo: Option<String>,
    ) -> Result<String, JsValue> {
        let fee = self
            .inner
            .estimate_fee(to, amount_zec, memo.as_deref())
            .map_err(js_err)?;
        Ok(z_engine::fee_estimate_json(fee).to_string())
    }

    /// Explicit single transparent swap output, funded from shielded notes.
    #[wasm_bindgen(js_name = estimateTransparentFee)]
    pub fn estimate_transparent_fee(
        &mut self,
        to: &str,
        amount_zec: &str,
    ) -> Result<String, JsValue> {
        let fee = self
            .inner
            .estimate_transparent_fee(to, amount_zec)
            .map_err(js_err)?;
        Ok(z_engine::fee_estimate_json(fee).to_string())
    }

    /// No memo/URI/multipay. The caller must disclose the public address and amount.
    #[wasm_bindgen(js_name = proveTransparentSend)]
    pub fn prove_transparent_send(
        &mut self,
        mnemonic: &str,
        to: &str,
        amount_zec: &str,
        max_fee_zat: Option<String>,
    ) -> Result<String, JsValue> {
        let max_fee = max_fee_zat
            .as_deref()
            .map(|value| {
                if value.is_empty()
                    || value.len() > 16
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(js_err("invalid maximum fee in zatoshis"));
                }
                let limit = value.parse::<u64>().map_err(js_err)?;
                if limit > 2_100_000_000_000_000 {
                    return Err(js_err("invalid maximum fee in zatoshis"));
                }
                Ok(limit)
            })
            .transpose()?;
        let raw = self
            .inner
            .prove_transparent_send(mnemonic, to, amount_zec, max_fee)
            .map_err(js_err)?;
        let txid = self.inner.raw_txid(&raw).map_err(js_err)?;
        Ok(serde_json::json!({ "txid": txid, "hex": z_engine::web::to_hex(&raw) }).to_string())
    }

    /// Max orchard sendable (own UA if `to` is empty). Returns `{ maxSendZat, maxSendZec, feeZat, feeZec }`.
    #[wasm_bindgen(js_name = maxSend)]
    pub fn max_send(&mut self, to: Option<String>) -> Result<String, JsValue> {
        let dest = to.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let (max, fee) = self.inner.max_send(dest).map_err(js_err)?;
        Ok(z_engine::max_send_json(max, fee).to_string())
    }

    /// Orchard-only send. `memo` may be empty. Returns `{ txid, hex }`.
    #[wasm_bindgen(js_name = proveSend)]
    pub fn prove_send(
        &mut self,
        mnemonic: &str,
        to: &str,
        amount_zec: &str,
        memo: Option<String>,
    ) -> Result<String, JsValue> {
        let raw = self
            .inner
            .prove_send(mnemonic, to, amount_zec, memo.as_deref())
            .map_err(js_err)?;
        let txid = self.inner.raw_txid(&raw).map_err(js_err)?;
        let hex = z_engine::web::to_hex(&raw);
        Ok(serde_json::json!({ "txid": txid, "hex": hex }).to_string())
    }

    #[wasm_bindgen(js_name = applyUtxos)]
    pub fn apply_utxos(&mut self, json: &str) -> Result<u32, JsValue> {
        self.inner.apply_utxos_json(json).map_err(js_err)
    }

    #[wasm_bindgen(js_name = applyMempool)]
    pub fn apply_mempool(&mut self, json: &str) -> Result<u32, JsValue> {
        self.inner.apply_mempool_json(json).map_err(js_err)
    }

    #[wasm_bindgen(js_name = enhanceRawTx)]
    pub fn enhance_raw_tx(&mut self, hex: &str) -> Result<u32, JsValue> {
        let raw = z_engine::web::from_hex(hex).map_err(js_err)?;
        self.inner.enhance_raw_tx(&raw).map_err(js_err)
    }

    #[wasm_bindgen(js_name = memoEnhancementTxids)]
    pub fn memo_enhancement_txids(&self, limit: u32) -> String {
        serde_json::to_string(&self.inner.memo_enhancement_txids(limit as usize))
            .expect("transaction id list serializes")
    }

    #[wasm_bindgen(js_name = rewindTo)]
    pub fn rewind_to(&mut self, height: u32) -> Result<u32, JsValue> {
        self.inner.rewind_to_height(height).map_err(js_err)
    }

    /// Rescan an earlier range while retaining keys and current addresses.
    #[wasm_bindgen(js_name = rescanFrom)]
    pub fn rescan_from(&mut self, birthday: u32) -> Result<(), JsValue> {
        self.inner.rescan_from(birthday).map_err(js_err)
    }

    /// Drop scan state (notes/trees/history). Keys and birthday stay. Call sync after.
    #[wasm_bindgen(js_name = resetScan)]
    pub fn reset_scan(&mut self) {
        self.inner.reset_scan();
    }

    /// JSON array of raw hex for our unmined, unexpired sends. Resubmitting the
    /// same bytes is idempotent, so a lost broadcast is delivered after reload.
    #[wasm_bindgen(js_name = pendingRawTxs)]
    pub fn pending_raw_txs(&self) -> Result<String, JsValue> {
        serde_json::to_string(&self.inner.pending_raw_txs()).map_err(js_err)
    }

    #[wasm_bindgen(js_name = abandon)]
    pub fn abandon(&mut self, txid: &str) -> Result<bool, JsValue> {
        self.inner.abandon_unmined(txid).map_err(js_err)
    }

    /// Apply a proved raw tx as the next compact block (pending → mined).
    #[wasm_bindgen(js_name = applyMinedTx)]
    pub fn apply_mined_tx(&mut self, hex: &str, time: u32) -> Result<String, JsValue> {
        let raw = z_engine::web::from_hex(hex).map_err(js_err)?;
        let d = self.inner.apply_mined_raw_tx(&raw, time).map_err(js_err)?;
        serde_json::to_string(&d).map_err(js_err)
    }

    /// Shield transparent UTXOs into Orchard. `thresholdZat` is the minimum to shield.
    /// Mnemonic is required at prove time. Returns `{ txid, hex }`.
    #[wasm_bindgen(js_name = proveShield)]
    pub fn prove_shield(&mut self, mnemonic: &str, threshold_zat: u32) -> Result<String, JsValue> {
        let raw = self
            .inner
            .prove_shield(mnemonic, u64::from(threshold_zat))
            .map_err(js_err)?;
        let txid = self.inner.raw_txid(&raw).map_err(js_err)?;
        let hex = z_engine::web::to_hex(&raw);
        Ok(serde_json::json!({ "txid": txid, "hex": hex }).to_string())
    }

    /// Build the process-wide Orchard proving key (same cache a later prove uses).
    #[wasm_bindgen(js_name = warmOrchardProvingKey)]
    pub fn warm_orchard_proving_key() -> bool {
        z_engine::web::warm_orchard_proving_key()
    }

    #[wasm_bindgen(js_name = orchardProvingKeyReady)]
    pub fn orchard_proving_key_ready() -> bool {
        z_engine::web::orchard_proving_key_ready()
    }

    #[wasm_bindgen(js_name = capabilities)]
    pub fn capabilities() -> String {
        serde_json::json!({
            "keys": true,
            "sync": true,
            "history": true,
            "prove": true,
            "transparentScan": true,
            "transparentOutputs": true,
            "mempool": true,
            "multicore": cfg!(feature = "multicore"),
            "threads": rayon::current_num_threads(),
            "orchardCircuit": true,
            "orchardProvingKeyReady": z_engine::web::orchard_proving_key_ready(),
            "simd": cfg!(target_feature = "simd128"),
        })
        .to_string()
    }
}

fn hex_list(json: &str) -> Result<Vec<Vec<u8>>, JsValue> {
    let list: Vec<String> = serde_json::from_str(json).map_err(js_err)?;
    list.iter()
        .map(|h| z_engine::web::from_hex(h).map_err(js_err))
        .collect()
}

fn apdu_json(c: &z_engine::ledger::ApduCommand) -> String {
    serde_json::to_string(c).expect("APDU serializes")
}

/// Ledger: `{ minSigning, minAccount }` app versions.
#[wasm_bindgen(js_name = ledgerAppVersions)]
pub fn ledger_app_versions() -> String {
    serde_json::json!({
        "minSigning": z_engine::ledger::MIN_SIGNING_APP_VERSION,
        "minAccount": z_engine::ledger::MIN_ACCOUNT_APP_VERSION,
        "appName": z_engine::ledger::ZCASH_APP_NAME,
    })
    .to_string()
}

/// Ledger: `version >= minimum` (pre-releases count as older; unparsable fails closed).
#[wasm_bindgen(js_name = ledgerAppVersionAtLeast)]
pub fn ledger_app_version_at_least(version: &str, minimum: &str) -> bool {
    z_engine::ledger::app_version_at_least(version, minimum)
}

/// Ledger: the APDU that reads the running app's name and version.
#[wasm_bindgen(js_name = ledgerAppInfoCommand)]
pub fn ledger_app_info_command() -> String {
    apdu_json(&z_engine::ledger::app_info_command())
}

/// Ledger: the APDU that opens the Zcash app from the dashboard.
#[wasm_bindgen(js_name = ledgerOpenAppCommand)]
pub fn ledger_open_app_command() -> String {
    apdu_json(&z_engine::ledger::open_zcash_app_command())
}

/// Ledger: `{ name, version }` from the app-info response (hex).
#[wasm_bindgen(js_name = ledgerDecodeAppInfo)]
pub fn ledger_decode_app_info(response_hex: &str) -> Result<String, JsValue> {
    let raw = z_engine::web::from_hex(response_hex).map_err(js_err)?;
    let app = z_engine::ledger::decode_app_info(&raw).map_err(js_err)?;
    serde_json::to_string(&app).map_err(js_err)
}

/// Ledger: `{ first, continuation }` APDUs for the UFVK export.
#[wasm_bindgen(js_name = ledgerUfvkCommands)]
pub fn ledger_ufvk_commands(account_index: u32) -> Result<String, JsValue> {
    let (first, continuation) = z_engine::ledger::ufvk_commands(account_index).map_err(js_err)?;
    Ok(serde_json::json!({ "first": first, "continuation": continuation }).to_string())
}

/// Ledger: bytes the UFVK export still expects after these responses (0 = done).
#[wasm_bindgen(js_name = ledgerUfvkBytesRemaining)]
pub fn ledger_ufvk_bytes_remaining(responses_json: &str) -> Result<u32, JsValue> {
    let n = z_engine::ledger::ufvk_bytes_remaining(&hex_list(responses_json)?).map_err(js_err)?;
    Ok(n as u32)
}

/// Ledger: `{ ufvk, seedFingerprint, accountIndex }` from the UFVK export responses.
#[wasm_bindgen(js_name = ledgerAccountFromResponses)]
pub fn ledger_account_from_responses(
    responses_json: &str,
    account_index: u32,
    network: &str,
) -> Result<String, JsValue> {
    let network = parse_network(network)?;
    let account = z_engine::ledger::account_from_ufvk_responses(
        &hex_list(responses_json)?,
        account_index,
        network,
    )
    .map_err(js_err)?;
    serde_json::to_string(&account).map_err(js_err)
}

/// Signs a PCZT's spends with a recovery phrase, the way a device would.
/// Off unless the crate is built with `--features simulator`. Production wasm
/// must not turn a hardware account's seed into a spend path.
#[cfg(feature = "simulator")]
#[wasm_bindgen(js_name = simulateDeviceSigning)]
pub fn simulate_device_signing(
    pczt: &[u8],
    mnemonic: &str,
    network: &str,
    account_index: u32,
) -> Result<Vec<u8>, JsValue> {
    let network = parse_network(network)?;
    z_engine::web::sign_pczt_with_mnemonic(pczt, mnemonic, network, account_index).map_err(js_err)
}

/// Rayon pool size (1 until `initThreadPool` runs on a multicore build).
#[wasm_bindgen(js_name = threadCount)]
pub fn thread_count() -> u32 {
    rayon::current_num_threads() as u32
}

/// Regtest NU6.3 (Ironwood) height for a validator that activates it early.
/// Call before opening a regtest wallet.
#[wasm_bindgen(js_name = setRegtestNu63Height)]
pub fn set_regtest_nu6_3_height(height: u32) {
    z_engine::set_regtest_nu6_3_height(height);
}

/// Optional regtest NU7 activation; call before opening any regtest wallet.
#[wasm_bindgen(js_name = setRegtestNu7Height)]
pub fn set_regtest_nu7_height(height: u32) {
    z_engine::set_regtest_nu7_height(height);
}

/// Process-wide Orchard proving-key warm (no wallet required).
#[wasm_bindgen(js_name = warmOrchardProvingKey)]
pub fn warm_orchard_proving_key() -> bool {
    z_engine::web::warm_orchard_proving_key()
}

#[wasm_bindgen(js_name = orchardProvingKeyReady)]
pub fn orchard_proving_key_ready() -> bool {
    z_engine::web::orchard_proving_key_ready()
}
