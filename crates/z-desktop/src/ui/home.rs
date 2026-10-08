use crate::shared::theme;
use crate::ui::actions::{NextField, PrevField, SubmitFocused};
use crate::ui::field_input::{FieldEvent, FieldInput};
use crate::ui::qr::qr_block;
use gpui::prelude::*;
use gpui::*;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use z_engine::keys::{inspect_address, unified_address_for_set, InspectedAddress, UaReceiverSet};
use z_engine::native::{
    pick_local_validator, probe_validator, Bridge, NativeWallet, SeedAuth, SeedStore, UnlockPolicy,
};
use z_engine::{
    date_from_height_for_network, describe_light_url, display_anyhow, display_catch_up_percent,
    fmt_secs, format_zatoshis, historic_overlay_checks, historic_overlay_visible,
    is_loopback_light_url, parse_birthday_input_for_network, parse_zec_to_zatoshis, parse_zip321,
    typical_tip, uses_fast_sync, ymd_days_ago, zip321_uri, LightServer, Network, OverlayCheck,
    SyncEta, SyncProgress, SyncStage, NEAR_TIP_BLOCKS, QUIET_BEHIND_BLOCKS, SHIELD_THRESHOLD_ZAT,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Passphrase,
    RestoreWords,
    Ufvk,
    Birthday,
    BirthdayDate,
    Light,
    Rpc,
    SendTo,
    Amount,
    SendMemo,
    SpendSeed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestoreKind {
    Seed,
    Ufvk,
}

/// Compact-block sync needs a worker that can poll tonic while sqlite persist
/// and Rayon wait run `block_in_place`. `current_thread` was the class that
/// froze the overlay between every 1000-block apply.
fn chain_sync_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("z-desktop-sync")
        .build()?)
}

// GPUI can replace the displayed wallet while an old background worker is
// still unwinding. Serialize wallet jobs across sessions until that worker has
// dropped its database connection, even if the new wallet uses the same path.
fn wallet_job_gate() -> &'static StdMutex<()> {
    static GATE: OnceLock<StdMutex<()>> = OnceLock::new();
    GATE.get_or_init(|| StdMutex::new(()))
}

/// A panic in a wallet job took the whole app down: GPUI does not propagate
/// it, and the main thread then aborts polling the dead task. Report it like
/// any failed job; saved scan progress resumes on the next sync.
fn unpanicked<T>(job: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(|panic| {
        let what = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Err(anyhow::anyhow!("wallet job failed unexpectedly: {what}"))
    })
}

/// Auto-shield and Sapling migration run after each sync. Their failures (a
/// broadcast whose outcome is unknown, an unreachable node) used to be
/// dropped, so a shield that never went out was invisible. Note them in the
/// sync status; the next sync tries again.
async fn maintained(w: &NativeWallet, auth: &SeedAuth, skip: bool, message: String) -> String {
    if w.is_view_only() || skip {
        return message;
    }
    match w.maintain(auth).await {
        Ok(_) => message,
        Err(e) => format!("{message} · auto-shield did not finish: {e}"),
    }
}

fn inspect_line(a: &InspectedAddress) -> String {
    let rec = a
        .receivers
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(" + ");
    let rec = if rec.is_empty() { "none" } else { rec.as_str() };
    let set = a
        .receiver_set
        .map(|s| match s {
            UaReceiverSet::Full => "Full (+t receive)",
            UaReceiverSet::Orchard => "Orchard",
            UaReceiverSet::Shielded => "Shielded",
        })
        .unwrap_or("mixed");
    format!(
        "{kind} · {set} · {rec}",
        kind = match a.kind {
            z_engine::keys::AddressKind::Unified => "unified",
            z_engine::keys::AddressKind::Sapling => "sapling",
            z_engine::keys::AddressKind::P2pkh => "p2pkh",
            z_engine::keys::AddressKind::P2sh => "p2sh",
            z_engine::keys::AddressKind::Tex => "tex",
            z_engine::keys::AddressKind::Sprout => "sprout",
        }
    )
}

fn wrap_chars(s: &str, every: usize) -> String {
    if every == 0 || s.chars().count() <= every {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + s.len() / every);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && i % every == 0 {
            out.push('\n');
        }
        out.push(ch);
    }
    out
}

#[derive(Clone, Copy)]
enum BtnKind {
    Ember,
    Patina,
    Ghost,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    None,
    Restore,
    Shield,
    Send,
    Sync,
    ResetScan,
    WipeScan,
    AttachSeed,
}

impl WorkKind {
    fn is_chain_scan(self) -> bool {
        matches!(self, Self::Sync | Self::ResetScan | Self::WipeScan)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RescanPrompt {
    None,
    Choose,
    ConfirmWipe,
}

#[derive(Clone)]
struct SuccessNotice {
    title: SharedString,
    txids: Vec<String>,
}

fn explorer_tx_url(network: Network, txid: &str) -> Option<String> {
    let id = txid.trim();
    if id.is_empty() {
        return None;
    }
    match network {
        Network::Mainnet => Some(format!("https://zcashexplorer.app/transactions/{id}")),
        Network::Testnet => Some(format!(
            "https://testnet.zcashexplorer.app/transactions/{id}"
        )),
        Network::Regtest => None,
    }
}

struct HistRow {
    height: SharedString,
    delta: SharedString,
    tx: SharedString,
    memo: SharedString,
}

struct WalletSnap {
    network: Network,
    address: String,
    t_address: String,
    orchard: String,
    transparent: String,
    pending: String,
    total: String,
    history: Vec<HistRow>,
    light_url: String,
    rpc_url: String,
    view_only: bool,
    ufvk: String,
    birthday: u32,
    unlock_policy: UnlockPolicy,
    os_unlock: bool,
}

pub struct HomeView {
    focus_handle: FocusHandle,
    wallet_dir: PathBuf,
    previous_wallet_dir: Option<PathBuf>,
    session_generation: u64,
    snapshot_generation: u64,
    network: Network,
    disk_network: Option<Network>,
    has_wallet: bool,
    wallet_loaded: bool,
    light_ok: Option<bool>,
    status: SharedString,
    address: SharedString,
    t_address: SharedString,
    orchard: SharedString,
    transparent: SharedString,
    pending: SharedString,
    total: SharedString,
    history: Vec<HistRow>,
    mnemonic: Option<SharedString>,
    show_pass: bool,
    pass: Entity<FieldInput>,
    restore: Entity<FieldInput>,
    ufvk: Entity<FieldInput>,
    birthday: Entity<FieldInput>,
    birthday_date: Entity<FieldInput>,
    spend_seed: Entity<FieldInput>,
    restore_kind: RestoreKind,
    os_unlock: bool,
    unlock_policy: UnlockPolicy,
    view_only: bool,
    wallet_ufvk: SharedString,
    birthday_h: u32,
    sync_live: Option<Arc<StdMutex<SyncProgress>>>,
    sync_server: String,
    seed_prompt: bool,
    rescan_prompt: RescanPrompt,
    pending_unlock: Option<WorkKind>,
    spend_confirmed: bool,
    keep_seed_in_os: bool,
    session_seed: Option<String>,
    probed_tip: Option<u32>,
    last_bday_h: String,
    last_bday_d: String,
    light: Entity<FieldInput>,
    rpc: Entity<FieldInput>,
    send_to: Entity<FieldInput>,
    send_amount: Entity<FieldInput>,
    send_memo: Entity<FieldInput>,
    receive_set: UaReceiverSet,
    send_inspect: SharedString,
    send_fee: Option<SharedString>,
    send_quote_key: String,
    send_quote_gen: u64,
    send_max_gen: u64,
    probe_note: SharedString,
    busy: bool,
    catchup_busy: bool,
    sync_cancel: Option<Arc<AtomicBool>>,
    sync_paused: bool,
    catchup_started: bool,
    work: WorkKind,
    work_tick: u32,
    work_ticks_running: bool,
    work_started: Instant,
    success: Option<SuccessNotice>,
    work_error: Option<SharedString>,
    auth_shared: Arc<StdMutex<SeedAuth>>,
    bridge_note: SharedString,
}

impl HomeView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let passphrase = std::env::var("Z_STACK_PASSPHRASE").unwrap_or_default();
        let env_wallet = std::env::var("Z_STACK_WALLET").ok().map(PathBuf::from);
        let lab = if env_wallet.is_none() {
            load_desktop_lab()
        } else {
            None
        };
        let wallet_dir = env_wallet
            .clone()
            .or_else(|| lab.as_ref().map(|l| l.wallet_dir.clone()))
            .unwrap_or_else(|| PathBuf::from("./wallet-data"));
        let previous_wallet_dir = lab.as_ref().and_then(|l| l.previous_wallet_dir.clone());
        let disk = NativeWallet::open(&wallet_dir).ok();
        let disk_network = disk.as_ref().map(|w| w.network());
        let has_wallet = disk.is_some();
        let initial_snap = disk.as_ref().and_then(|w| HomeView::snapshot(w).ok());
        let env_network = std::env::var("Z_STACK_NETWORK")
            .ok()
            .and_then(|value| Network::parse(&value));
        let network = disk_network
            .or(env_network)
            .or_else(|| lab.as_ref().map(|l| l.network))
            .unwrap_or(Network::Regtest);
        let light_url = disk
            .as_ref()
            .map(|w| w.server_url())
            .unwrap_or_else(|| LightServer::local_for_network(network).as_url());
        let rpc_url = disk
            .as_ref()
            .and_then(|w| w.validator_rpc_url())
            .unwrap_or_default();
        let birthday = match network {
            Network::Regtest => "1".into(),
            _ => String::new(),
        };
        let birthday_date = if has_wallet {
            String::new()
        } else {
            std::env::var("Z_STACK_BIRTHDAY_DATE").unwrap_or_default()
        };
        let status = if let Some(net) = disk_network {
            format!(
                "{} wallet already in {}. Open it — Create is for an empty folder.",
                net.as_str(),
                wallet_dir.display()
            )
            .into()
        } else {
            "This folder is empty. Pick a network, Probe Zaino, then Create (or Restore).".into()
        };
        let auth_shared = Arc::new(StdMutex::new(if passphrase.trim().is_empty() {
            SeedAuth::windows_credential()
        } else {
            SeedAuth::passphrase(passphrase.clone())
        }));
        let bridge_note = spawn_web_bridge(wallet_dir.clone(), Arc::clone(&auth_shared));
        let weak = cx.weak_entity();
        let nav: Rc<dyn Fn(FieldEvent, &mut Window, &mut App)> =
            Rc::new(move |ev: FieldEvent, window: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |home, cx| match ev {
                    FieldEvent::Tab { back } => home.tab_fields(back, window, cx),
                    FieldEvent::Submit => home.submit_focused(cx),
                    FieldEvent::Edited => home.on_fields_edited(cx),
                });
            });
        let mk = |cx: &mut Context<Self>,
                  id: &'static str,
                  content: String,
                  placeholder: &str,
                  masked: bool,
                  multiline: bool,
                  nav: &Rc<dyn Fn(FieldEvent, &mut Window, &mut App)>| {
            let nav = Rc::clone(nav);
            cx.new(move |cx| {
                let mut f =
                    FieldInput::new(cx, id, content, placeholder.to_string(), masked, multiline);
                f.set_nav(nav);
                f
            })
        };
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            wallet_dir,
            previous_wallet_dir,
            session_generation: 0,
            snapshot_generation: 0,
            network,
            disk_network,
            has_wallet,
            wallet_loaded: false,
            light_ok: None,
            status,
            address: String::new().into(),
            t_address: String::new().into(),
            orchard: "0".into(),
            transparent: "0".into(),
            pending: "0".into(),
            total: "0".into(),
            history: Vec::new(),
            mnemonic: None,
            show_pass: false,
            pass: mk(
                cx,
                "in-pass",
                passphrase,
                "wallet passphrase",
                true,
                false,
                &nav,
            ),
            restore: mk(
                cx,
                "in-restore",
                String::new(),
                "paste 12 or 24 words",
                false,
                true,
                &nav,
            ),
            ufvk: mk(
                cx,
                "in-ufvk",
                String::new(),
                "uview1… / uviewtest1…",
                false,
                true,
                &nav,
            ),
            birthday: mk(
                cx,
                "in-bday",
                birthday.clone(),
                "height, e.g. 1687104",
                false,
                false,
                &nav,
            ),
            birthday_date: mk(
                cx,
                "in-bday-date",
                birthday_date.clone(),
                "YYYY-MM-DD first used",
                false,
                false,
                &nav,
            ),
            spend_seed: mk(
                cx,
                "in-spend-seed",
                String::new(),
                "12 or 24 words to sign",
                true,
                true,
                &nav,
            ),
            restore_kind: RestoreKind::Seed,
            os_unlock: true,
            unlock_policy: UnlockPolicy::Session,
            view_only: false,
            wallet_ufvk: String::new().into(),
            birthday_h: 1,
            sync_live: None,
            sync_server: light_url.clone(),
            seed_prompt: false,
            rescan_prompt: RescanPrompt::None,
            pending_unlock: None,
            spend_confirmed: false,
            keep_seed_in_os: false,
            session_seed: None,
            probed_tip: None,
            last_bday_h: birthday.clone(),
            last_bday_d: birthday_date,
            light: mk(cx, "in-light", light_url, "zaino gRPC", false, true, &nav),
            rpc: mk(
                cx,
                "in-rpc",
                rpc_url,
                "zakura RPC (optional)",
                false,
                true,
                &nav,
            ),
            send_to: mk(
                cx,
                "in-to",
                String::new(),
                "unified address or zcash: URI",
                false,
                true,
                &nav,
            ),
            send_amount: mk(cx, "in-amt", String::new(), "0.01", false, false, &nav),
            send_memo: mk(
                cx,
                "in-memo",
                String::new(),
                "encrypted to the recipient",
                false,
                true,
                &nav,
            ),
            receive_set: UaReceiverSet::Full,
            send_inspect: SharedString::default(),
            send_fee: None,
            send_quote_key: String::new(),
            send_quote_gen: 0,
            send_max_gen: 0,
            probe_note:
                "Zaino serves compact blocks. Zakura is JSON-RPC. They are not the same process."
                    .into(),
            busy: false,
            catchup_busy: false,
            sync_cancel: None,
            sync_paused: false,
            catchup_started: false,
            work: WorkKind::None,
            work_tick: 0,
            work_ticks_running: false,
            work_started: Instant::now(),
            success: None,
            work_error: None,
            auth_shared,
            bridge_note,
        };
        if let Some(snap) = initial_snap {
            view.apply_balances(snap, cx);
        }
        view.persist_lab();
        view.restore.update(cx, |f, _| f.set_min_h(px(56.0)));
        view.ufvk.update(cx, |f, _| f.set_min_h(px(56.0)));
        view.spend_seed.update(cx, |f, _| f.set_min_h(px(72.0)));
        view.ensure_work_ticks(cx);
        if view.wallet_loaded {
            // A previous restore may have stopped after committing a partial
            // scan. Reopen it and continue without asking for the seed again.
            view.sync_background(cx);
        }
        view
    }

    fn field_text(&self, input: &Entity<FieldInput>, cx: &App) -> String {
        input.read(cx).text()
    }

    fn set_field(
        input: &Entity<FieldInput>,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |f, cx| f.set_text(text, cx));
    }

    fn seed_auth(&self, cx: &App) -> SeedAuth {
        let p = self.field_text(&self.pass, cx);
        let p = p.trim();
        let mut auth = if p.is_empty() {
            if self.os_unlock {
                SeedAuth::windows_credential()
            } else {
                SeedAuth {
                    passphrase: None,
                    windows_credential: false,
                    mnemonic: None,
                    unlock_policy: self.unlock_policy,
                }
            }
        } else {
            SeedAuth::passphrase(p.to_string())
        };
        auth.windows_credential = self.os_unlock || auth.windows_credential;
        auth.unlock_policy = self.unlock_policy;
        auth
    }

    fn eta_tip(&self) -> u32 {
        self.probed_tip.unwrap_or_else(|| typical_tip(self.network))
    }

    fn on_fields_edited(&mut self, cx: &mut Context<Self>) {
        self.publish_auth(cx);
        self.sync_birthday_fields(cx);
        self.apply_zip321_from_to(cx);
        self.quote_send(cx);
        cx.notify();
    }

    fn apply_zip321_from_to(&mut self, cx: &mut Context<Self>) {
        let to = self.field_text(&self.send_to, cx);
        if !to.trim().to_ascii_lowercase().starts_with("zcash:") {
            return;
        }
        let Ok(req) = parse_zip321(to.trim()) else {
            return;
        };
        if req.payments.len() > 1 {
            Self::set_field(&self.send_amount, "", cx);
            Self::set_field(&self.send_memo, "", cx);
            return;
        }
        if let Some(a) = req.amount_zec().map(str::to_string) {
            Self::set_field(&self.send_amount, a, cx);
        }
        if let Some(m) = req.memo().map(str::to_string) {
            Self::set_field(&self.send_memo, m, cx);
        }
    }

    fn inspect_send_to(&self, cx: &App) -> SharedString {
        let to = self.field_text(&self.send_to, cx);
        let dest = to.trim();
        if dest.is_empty() {
            return SharedString::default();
        }
        let encoded = if dest.to_ascii_lowercase().starts_with("zcash:") {
            parse_zip321(dest)
                .ok()
                .map(|r| r.address().to_string())
                .unwrap_or_else(|| dest.to_string())
        } else {
            dest.to_string()
        };
        match inspect_address(&encoded) {
            Ok(a) => inspect_line(&a).into(),
            Err(_) => SharedString::default(),
        }
    }

    fn current_session(&self, generation: u64, wallet: &Path) -> bool {
        self.session_generation == generation && self.wallet_dir == wallet
    }

    fn send_request_key(&self, cx: &App) -> String {
        format!(
            "{}|{}|{}",
            self.field_text(&self.send_to, cx).trim(),
            self.field_text(&self.send_amount, cx).trim(),
            self.field_text(&self.send_memo, cx).trim()
        )
    }

    fn invalidate_send_estimates(&mut self) {
        self.send_quote_gen = self.send_quote_gen.wrapping_add(1);
        self.send_max_gen = self.send_max_gen.wrapping_add(1);
        self.send_quote_key.clear();
        self.send_fee = None;
    }

    fn quote_send(&mut self, cx: &mut Context<Self>) {
        if !self.wallet_loaded {
            self.send_inspect = SharedString::default();
            self.send_fee = None;
            return;
        }
        let to = self.field_text(&self.send_to, cx);
        let amt = self.field_text(&self.send_amount, cx);
        let memo = self.field_text(&self.send_memo, cx);
        let key = self.send_request_key(cx);
        if key == self.send_quote_key {
            return;
        }
        // Empty or invalid edits must invalidate an already-running request too.
        self.invalidate_send_estimates();
        self.send_quote_key = key.clone();
        self.send_inspect = self.inspect_send_to(cx);
        let dest = to.trim().to_string();
        if dest.is_empty() {
            self.send_fee = None;
            return;
        }
        let zip = dest.to_ascii_lowercase().starts_with("zcash:");
        let zat = parse_zec_to_zatoshis(amt.trim()).unwrap_or(0);
        if !zip && zat == 0 {
            self.send_fee = None;
            return;
        }
        let gen = self.send_quote_gen;
        let session = self.session_generation;
        let wallet = self.wallet_dir.clone();
        let dir = wallet.clone();
        let memo_opt = {
            let m = memo.trim();
            if m.is_empty() {
                None
            } else {
                Some(m.to_string())
            }
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let w = NativeWallet::open(&dir)?;
                    w.estimate_fee(&dest, zat, memo_opt.as_deref())
                })
                .await;
            this.update(&mut *cx, |view, cx| {
                if !view.current_session(session, &wallet)
                    || view.send_quote_gen != gen
                    || view.send_request_key(cx) != key
                {
                    return;
                }
                match result {
                    Ok(fee) => {
                        view.send_fee = Some(format!("Fee {} ZEC", format_zatoshis(fee)).into());
                    }
                    Err(e) => {
                        view.send_fee = Some(display_anyhow(&e.into()).into());
                    }
                }
                cx.notify();
            })
            .ok();
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn fill_max_send(&mut self, cx: &mut Context<Self>) {
        if !self.wallet_loaded {
            self.status = "Open the wallet before Max.".into();
            cx.notify();
            return;
        }
        let to = self.field_text(&self.send_to, cx);
        let dest = to.trim().to_string();
        self.invalidate_send_estimates();
        let gen = self.send_max_gen;
        let key = self.send_request_key(cx);
        self.send_quote_key = key.clone();
        let session = self.session_generation;
        let wallet = self.wallet_dir.clone();
        let dir = wallet.clone();
        self.status = "Estimating max send…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let w = NativeWallet::open(&dir)?;
                    let to = if dest.is_empty() { None } else { Some(dest) };
                    w.max_send(to.as_deref())
                })
                .await;
            this.update(&mut *cx, |view, cx| {
                if !view.current_session(session, &wallet)
                    || view.send_max_gen != gen
                    || view.send_request_key(cx) != key
                {
                    return;
                }
                match result {
                    Ok((max, fee)) => {
                        Self::set_field(&view.send_amount, format_zatoshis(max), cx);
                        view.send_fee = Some(
                            format!(
                                "Fee {} ZEC · max {} ZEC",
                                format_zatoshis(fee),
                                format_zatoshis(max)
                            )
                            .into(),
                        );
                        view.send_quote_key.clear();
                        view.status = "Max filled from engine propose.".into();
                    }
                    Err(e) => {
                        view.send_fee = Some(display_anyhow(&e.into()).into());
                        view.status = "Max send failed.".into();
                    }
                }
                cx.notify();
            })
            .ok();
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn displayed_receive_ua(&self) -> String {
        if self.receive_set == UaReceiverSet::Full || self.wallet_ufvk.is_empty() {
            return self.address.to_string();
        }
        unified_address_for_set(self.wallet_ufvk.as_ref(), self.network, self.receive_set)
            .unwrap_or_else(|_| self.address.to_string())
    }

    fn set_receive_set(&mut self, set: UaReceiverSet, cx: &mut Context<Self>) {
        self.receive_set = set;
        cx.notify();
    }

    fn sync_birthday_fields(&mut self, cx: &mut Context<Self>) {
        let tip = self.eta_tip();
        let height_raw = self.field_text(&self.birthday, cx);
        let date_raw = self.field_text(&self.birthday_date, cx);
        if height_raw != self.last_bday_h {
            self.last_bday_h = height_raw.clone();
            if let Ok(h) = parse_birthday_input_for_network(&height_raw, tip, self.network) {
                let date = date_from_height_for_network(h, tip, self.network);
                if date != date_raw {
                    self.last_bday_d = date.clone();
                    Self::set_field(&self.birthday_date, date, cx);
                }
            }
        } else if date_raw != self.last_bday_d {
            self.last_bday_d = date_raw.clone();
            if date_raw.trim().is_empty() {
                return;
            }
            if let Ok(h) = parse_birthday_input_for_network(&date_raw, tip, self.network) {
                let hs = h.to_string();
                if hs != height_raw {
                    self.last_bday_h = hs.clone();
                    Self::set_field(&self.birthday, hs, cx);
                }
            }
        }
    }

    fn set_approx_days(&mut self, days: u32, cx: &mut Context<Self>) {
        let date = ymd_days_ago(days);
        self.last_bday_d.clear();
        Self::set_field(&self.birthday_date, date, cx);
        self.sync_birthday_fields(cx);
        cx.notify();
    }

    fn paste_into(&mut self, input: &Entity<FieldInput>, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.status = "Clipboard is empty.".into();
            cx.notify();
            return;
        };
        let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
        Self::set_field(input, cleaned, cx);
        self.on_fields_edited(cx);
    }

    fn persist_unlock_policy(&mut self, policy: UnlockPolicy, cx: &mut Context<Self>) {
        if self.busy || self.catchup_busy {
            self.status = "Stop the current scan before changing wallet settings.".into();
            cx.notify();
            return;
        }
        self.unlock_policy = policy;
        if self.has_wallet {
            if let Ok(mut w) = NativeWallet::open(&self.wallet_dir) {
                if let Err(e) = w.set_unlock_policy(policy) {
                    self.status = format!("could not save unlock policy: {e}").into();
                }
            }
        }
        if policy == UnlockPolicy::EachSpend {
            self.session_seed = None;
        } else {
            self.try_session_unlock(cx);
        }
        cx.notify();
    }

    fn try_session_unlock(&mut self, cx: &App) {
        if self.session_seed.is_some() || self.unlock_policy == UnlockPolicy::EachSpend {
            return;
        }
        let pass = self.field_text(&self.pass, cx);
        let pass = pass.trim();
        let prefer_os = self.os_unlock || self.unlock_policy == UnlockPolicy::Always;
        if let Ok(words) = SeedStore::new(&self.wallet_dir)
            .load_words(if pass.is_empty() { None } else { Some(pass) }, prefer_os)
        {
            self.session_seed = Some(words);
        }
    }

    fn can_save_seed(&self, cx: &App) -> bool {
        self.os_unlock || !self.field_text(&self.pass, cx).trim().is_empty()
    }

    fn needs_spend_unlock(&self, cx: &App) -> bool {
        if self.spend_confirmed {
            return false;
        }
        if !self.field_text(&self.spend_seed, cx).trim().is_empty() {
            return false;
        }
        if self.view_only && self.session_seed.is_none() {
            return true;
        }
        self.unlock_policy == UnlockPolicy::EachSpend
    }

    fn request_spend_unlock(
        &mut self,
        action: WorkKind,
        reason: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.pending_unlock = Some(action);
        self.seed_prompt = true;
        self.status = reason.into();
        cx.notify();
    }

    fn confirm_pending_unlock(&mut self, cx: &mut Context<Self>) {
        self.seed_prompt = false;
        match self.pending_unlock.take() {
            Some(WorkKind::Shield) => {
                self.spend_confirmed = true;
                self.shield(cx);
            }
            Some(WorkKind::Send) => {
                self.spend_confirmed = true;
                self.send(cx);
            }
            Some(WorkKind::AttachSeed) => self.attach_pasted_seed(cx),
            _ => {
                self.spend_confirmed = false;
                cx.notify();
            }
        }
    }

    fn attach_pasted_seed(&mut self, cx: &mut Context<Self>) {
        let words = self.field_text(&self.spend_seed, cx);
        let words = words.trim().to_string();
        if words.split_whitespace().count() != 12 && words.split_whitespace().count() != 24 {
            self.status = "Paste a 12 or 24 word seed to enable sending.".into();
            self.seed_prompt = true;
            self.pending_unlock = Some(WorkKind::AttachSeed);
            cx.notify();
            return;
        }
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let words_keep = words.clone();
        self.run_blocking(
            cx,
            "attaching seed…",
            move || {
                let mut w = NativeWallet::open(&dir)?;
                w.attach_seed(&words, &auth)?;
                HomeView::snapshot(&w)
            },
            move |view, snap, cx| {
                view.cache_session_seed(&words_keep);
                Self::set_field(&view.spend_seed, "", cx);
                view.apply_snapshot(snap, "Seed attached. This wallet can send.", cx);
            },
        );
    }

    fn cache_session_seed(&mut self, words: &str) {
        if self.unlock_policy == UnlockPolicy::EachSpend {
            return;
        }
        let t = words.trim();
        if !t.is_empty() {
            self.session_seed = Some(t.to_string());
        }
    }

    fn publish_auth(&self, cx: &App) {
        let a = self.seed_auth(cx);
        if let Ok(mut g) = self.auth_shared.lock() {
            *g = a;
        }
    }

    fn auth(&self, cx: &App) -> SeedAuth {
        self.publish_auth(cx);
        let mut a = self.seed_auth(cx);
        a.windows_credential = self.os_unlock;
        a.unlock_policy = self.unlock_policy;
        if self.unlock_policy != UnlockPolicy::EachSpend {
            if let Some(seed) = &self.session_seed {
                a.mnemonic = Some(seed.clone());
            }
        }
        let once = self.field_text(&self.spend_seed, cx);
        if !once.trim().is_empty() {
            a.mnemonic = Some(once);
        }
        a
    }

    fn server(&self, cx: &App) -> LightServer {
        LightServer::parse(&self.field_text(&self.light, cx), self.network)
    }

    fn using_local_light(&self, cx: &App) -> bool {
        let cur = self.field_text(&self.light, cx);
        let cur = cur.trim();
        if cur.is_empty() {
            return true;
        }
        let n = LightServer::parse(cur, self.network).as_url();
        n == LightServer::local_for_network(self.network).as_url()
            || n == LightServer::LOCAL_ZAINO_GRPC
            || n == LightServer::LOCAL_ZAINO_GRPC_MAINNET
    }

    fn apply_network_defaults(&mut self, cx: &mut Context<Self>) {
        let light = LightServer::local_for_network(self.network).as_url();
        Self::set_field(&self.light, light, cx);
        Self::set_field(&self.rpc, "", cx);
        match self.network {
            Network::Regtest => {
                let b = self.field_text(&self.birthday, cx);
                if b.is_empty() || b == "1" {
                    Self::set_field(&self.birthday, "1", cx);
                }
            }
            _ => {
                if self.field_text(&self.birthday, cx) == "1" {
                    Self::set_field(&self.birthday, "", cx);
                }
            }
        }
    }

    fn set_fields_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        for input in [
            self.pass.clone(),
            self.restore.clone(),
            self.ufvk.clone(),
            self.birthday.clone(),
            self.birthday_date.clone(),
            self.spend_seed.clone(),
            self.light.clone(),
            self.rpc.clone(),
            self.send_to.clone(),
            self.send_amount.clone(),
            self.send_memo.clone(),
        ] {
            input.update(cx, |f, cx| f.set_disabled(disabled, cx));
        }
    }

    fn input(&self, field: Field) -> &Entity<FieldInput> {
        match field {
            Field::Passphrase => &self.pass,
            Field::RestoreWords => &self.restore,
            Field::Ufvk => &self.ufvk,
            Field::Birthday => &self.birthday,
            Field::BirthdayDate => &self.birthday_date,
            Field::SpendSeed => &self.spend_seed,
            Field::Light => &self.light,
            Field::Rpc => &self.rpc,
            Field::SendTo => &self.send_to,
            Field::Amount => &self.send_amount,
            Field::SendMemo => &self.send_memo,
        }
    }

    fn next_step(&self) -> SharedString {
        if self.busy {
            return self.status.clone();
        }
        if let Some(left) = self.live_remaining() {
            if left > 0 && (self.catchup_busy || self.work.is_chain_scan()) {
                return format!("{left} behind").into();
            }
        }
        if self.wallet_loaded {
            return format!(
                "Wallet is open in {} ({}). Sync to catch up — or pick another network tab for its own folder.",
                self.wallet_dir.display(),
                self.network.as_str()
            )
            .into();
        }
        if self.has_wallet {
            let net = self.disk_network.unwrap_or(self.network);
            return format!(
                "{} is a {} wallet. Other networks use their own folder (wallet-data-mainnet, …).",
                self.wallet_dir.display(),
                net.as_str()
            )
            .into();
        }
        match self.light_ok {
            Some(true) => format!(
                "Zaino is up on {}. Create a new wallet, or Restore with words and a birthday.",
                self.network.as_str()
            )
            .into(),
            Some(false) => {
                "Zaino is down on this network. Start it, or switch network, before Create.".into()
            }
            None => format!(
                "1. Pick network  2. Probe  3. Create on {}  — or Restore with a birthday height.",
                self.network.as_str()
            )
            .into(),
        }
    }

    fn snapshot(w: &NativeWallet) -> anyhow::Result<WalletSnap> {
        let addr = w.unified_address().unwrap_or_default();
        let t = w.transparent_address()?.unwrap_or_default();
        let b = w.balance().unwrap_or_default();
        Ok(WalletSnap {
            network: w.network(),
            address: addr,
            t_address: t,
            orchard: format_zatoshis(b.orchard_available),
            transparent: format_zatoshis(b.transparent_available),
            pending: format_zatoshis(b.total_pending),
            total: format_zatoshis(b.total_available),
            history: Self::format_history(w),
            light_url: w.server_url(),
            rpc_url: w.validator_rpc_url().unwrap_or_default(),
            view_only: w.is_view_only(),
            ufvk: w.viewing_key().unwrap_or_default(),
            birthday: w.birthday_height(),
            unlock_policy: w.unlock_policy(),
            os_unlock: w.os_unlock(),
        })
    }

    fn format_history(w: &NativeWallet) -> Vec<HistRow> {
        w.history(12)
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                let height = e
                    .mined_height
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| e.status().to_string());
                let tx = if e.txid.len() > 16 {
                    format!("{}…", &e.txid[..16])
                } else {
                    e.txid.clone()
                };
                let memo = if e.memos.is_empty() {
                    String::new()
                } else {
                    e.memos.join(" · ")
                };
                let sign = if e.account_delta_zat > 0 {
                    "+"
                } else if e.account_delta_zat < 0 {
                    "-"
                } else {
                    ""
                };
                HistRow {
                    height: height.into(),
                    delta: format!(
                        "{sign}{} ZEC",
                        format_zatoshis(e.account_delta_zat.unsigned_abs())
                    )
                    .into(),
                    tx: tx.into(),
                    memo: memo.into(),
                }
            })
            .collect()
    }

    fn apply_balances(&mut self, snap: WalletSnap, cx: &mut Context<Self>) {
        self.network = snap.network;
        self.disk_network = Some(snap.network);
        self.has_wallet = true;
        self.wallet_loaded = !snap.address.is_empty();
        self.address = snap.address.into();
        self.t_address = snap.t_address.into();
        self.orchard = snap.orchard.into();
        self.transparent = snap.transparent.into();
        self.pending = snap.pending.into();
        self.total = snap.total.into();
        self.history = snap.history;
        self.view_only = snap.view_only;
        self.wallet_ufvk = snap.ufvk.into();
        self.birthday_h = snap.birthday;
        self.unlock_policy = snap.unlock_policy;
        self.os_unlock = snap.os_unlock;
        let _ = cx;
    }

    fn apply_snapshot(
        &mut self,
        snap: WalletSnap,
        status: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let status = status.into();
        let no_activity_since_birthday = (status.contains("Synced")
            || status.contains("Rescanned"))
            && snap.total == "0"
            && snap.pending == "0"
            && snap.history.is_empty();
        let birthday = snap.birthday;
        Self::set_field(&self.light, snap.light_url.clone(), cx);
        Self::set_field(&self.rpc, snap.rpc_url.clone(), cx);
        self.apply_balances(snap, cx);
        self.status = if no_activity_since_birthday {
            format!(
                "{status} No activity found since birthday {birthday}. If you expect older funds, restore with an earlier first-used date."
            )
            .into()
        } else {
            status
        };
    }

    /// Overlay reads `sync_live`; plate updates when a snapshot lands (`apply_snapshot`).
    fn on_sync_tick(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn run_blocking<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        working: impl Into<SharedString>,
        work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + Send + 'static,
    ) {
        if self.busy {
            return;
        }
        if self.catchup_busy {
            self.work = WorkKind::Sync;
            self.status = "Stop the current scan before another wallet operation.".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.invalidate_send_estimates();
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        let publication = self.snapshot_generation;
        let session = self.session_generation;
        let wallet = self.wallet_dir.clone();
        self.set_fields_disabled(true, cx);
        self.status = working.into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let _wallet_job = wallet_job_gate().lock().unwrap_or_else(|e| e.into_inner());
                    unpanicked(work)
                })
                .await;
            this.update(&mut *cx, |view, cx| {
                if !view.current_session(session, &wallet)
                    || view.snapshot_generation != publication
                {
                    return;
                }
                let was_work = view.work;
                let was_cancelled = view
                    .sync_cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Acquire));
                view.busy = false;
                view.work = WorkKind::None;
                view.sync_live = None;
                view.sync_cancel = None;
                view.set_fields_disabled(false, cx);
                match result {
                    Ok(val) => {
                        if was_cancelled {
                            view.show_stopped_sync(cx);
                        } else {
                            apply(view, val, cx);
                        }
                    }
                    Err(_) if was_cancelled => {
                        view.spend_confirmed = false;
                        view.show_stopped_sync(cx);
                    }
                    Err(e) => {
                        view.spend_confirmed = false;
                        let msg = display_anyhow(&e);
                        view.status = msg.clone().into();
                        if was_work != WorkKind::None {
                            view.work_error = Some(msg.into());
                        }
                        if matches!(was_work, WorkKind::Sync | WorkKind::Restore) {
                            // Restore creates the wallet before its historical
                            // scan. If Zaino disappears mid-scan, publish that
                            // durable wallet so Open/Sync and background retry
                            // remain available after the error is dismissed.
                            view.paint_wallet_now(cx);
                        }
                    }
                }
                if view.wallet_loaded && !was_cancelled {
                    view.ensure_catchup(cx);
                }
                cx.notify();
            })
            .ok();
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.has_wallet {
            self.status = format!(
                "./wallet-data already has a {} wallet. Open it, or delete the folder to create on {}.",
                self.disk_network.unwrap_or(self.network).as_str(),
                self.network.as_str()
            )
            .into();
            cx.notify();
            return;
        }
        if !self.can_save_seed(cx) {
            self.status =
                "Set a passphrase or enable Hello / passkey before Create. No default secret."
                    .into();
            cx.notify();
            return;
        }
        let dir = self.wallet_dir.clone();
        let network = self.network;
        let server = self.server(cx);
        let rpc = self.field_text(&self.rpc, cx).trim().to_string();
        let auth = self.auth(cx);
        let height_raw = self.field_text(&self.birthday, cx);
        let date_raw = self.field_text(&self.birthday_date, cx);
        let birthday: Option<u32> = {
            let raw = if !height_raw.trim().is_empty() {
                height_raw
            } else {
                date_raw
            };
            match raw.trim() {
                "" | "auto" => None,
                s => parse_birthday_input_for_network(s, self.eta_tip(), network).ok(),
            }
        };
        self.begin_work(WorkKind::Sync, cx);
        self.run_blocking(
            cx,
            format!("creating {} wallet…", network.as_str()),
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let (mut w, created) = NativeWallet::create(
                        &dir,
                        network,
                        Some(server),
                        birthday,
                        auth.clone(),
                        0,
                    )
                    .await?;
                    if !rpc.is_empty() {
                        w.set_validator_rpc(Some(rpc))?;
                    }
                    let snap = HomeView::snapshot(&w)?;
                    Ok((created, snap))
                })
            },
            // Show the words as soon as the wallet exists. The first scan used
            // to run inside this job, so an outage or a closed app during it
            // left a wallet that could receive funds but whose words were
            // never shown.
            |view, (created, snap), cx| {
                view.cache_session_seed(&created.mnemonic);
                view.mnemonic = Some(created.mnemonic.into());
                view.wallet_ufvk = created.ufvk.into();
                view.birthday_h = created.birthday_height;
                view.apply_snapshot(
                    snap,
                    format!(
                        "Created on {}, birthday {}. Copy the words and the viewing key. Scanning in the background…",
                        view.network.as_str(),
                        created.birthday_height
                    ),
                    cx,
                );
                view.sync_background(cx);
            },
        );
    }

    fn restore(&mut self, cx: &mut Context<Self>) {
        if self.has_wallet {
            self.status = format!(
                "./wallet-data already has a {} wallet. Open it instead of restoring into the same folder.",
                self.disk_network.unwrap_or(self.network).as_str()
            )
            .into();
            cx.notify();
            return;
        }
        let kind = self.restore_kind;
        let words = self.field_text(&self.restore, cx);
        let words = words.trim().to_string();
        let ufvk = self.field_text(&self.ufvk, cx).trim().to_string();
        if kind == RestoreKind::Seed {
            let n = words.split_whitespace().count();
            if n != 12 && n != 24 {
                self.status = "Paste a 12 or 24 word seed, or switch to viewing key.".into();
                cx.notify();
                return;
            }
        } else if ufvk.len() < 20 {
            self.status = "Paste a unified viewing key (starts with uview).".into();
            cx.notify();
            return;
        }
        if kind == RestoreKind::Seed && !self.can_save_seed(cx) {
            self.status =
                "Set a passphrase or enable Hello / passkey before restoring a seed.".into();
            cx.notify();
            return;
        }
        let dir = self.wallet_dir.clone();
        let network = self.network;
        let server = self.server(cx);
        let rpc = self.field_text(&self.rpc, cx).trim().to_string();
        let auth = self.auth(cx);
        let height_raw = self.field_text(&self.birthday, cx);
        let date_raw = self.field_text(&self.birthday_date, cx);
        let seed_for_cache = words.clone();
        self.begin_work(WorkKind::Restore, cx);
        self.run_blocking(
            cx,
            format!("restoring {} wallet…", network.as_str()),
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let tip_now = NativeWallet::fetch_tip(&server).await.unwrap_or(1);
                    let birthday = if !date_raw.trim().is_empty() {
                        parse_birthday_input_for_network(&date_raw, tip_now, network)?
                    } else if !height_raw.trim().is_empty() {
                        parse_birthday_input_for_network(&height_raw, tip_now, network)?
                    } else {
                        anyhow::bail!("Set birthday height or first-used date (YYYY-MM-DD).");
                    };
                    let (mut w, _ua) = if kind == RestoreKind::Ufvk {
                        NativeWallet::restore_ufvk(&dir, &ufvk, network, Some(server), birthday, 0)
                            .await?
                    } else {
                        NativeWallet::restore(
                            &dir,
                            &words,
                            network,
                            Some(server),
                            birthday,
                            auth.clone(),
                            0,
                        )
                        .await?
                    };
                    if !rpc.is_empty() {
                        w.set_validator_rpc(Some(rpc))?;
                    }
                    let snap = HomeView::snapshot(&w)?;
                    Ok((birthday, snap))
                })
            },
            move |view, (birthday, snap), cx| {
                Self::set_field(&view.restore, "", cx);
                view.mnemonic = None;
                if kind == RestoreKind::Seed {
                    view.cache_session_seed(&seed_for_cache);
                }
                view.apply_snapshot(
                    snap,
                    format!(
                        "Restored on {}, birthday {birthday}. Scanning history in the background…",
                        view.network.as_str()
                    ),
                    cx,
                );
                view.sync_background(cx);
            },
        );
    }

    fn ensure_catchup(&mut self, cx: &mut Context<Self>) {
        if self.catchup_started {
            return;
        }
        self.catchup_started = true;
        let session = self.session_generation;
        let wallet = self.wallet_dir.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_spawn(async {
                    std::thread::sleep(std::time::Duration::from_secs(45));
                })
                .await;
                let keep_running = this.update(&mut *cx, |view, cx| {
                    if !view.current_session(session, &wallet) {
                        return false;
                    }
                    if view.wallet_loaded && !view.busy {
                        view.sync_quiet(cx);
                    }
                    true
                });
                if !matches!(keep_running, Ok(true)) {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.open_and_sync(cx);
    }

    fn sync_quiet(&mut self, cx: &mut Context<Self>) {
        self.sync_background(cx);
    }

    fn paint_wallet_now(&mut self, cx: &mut Context<Self>) -> bool {
        let Ok(w) = NativeWallet::open(&self.wallet_dir) else {
            return false;
        };
        let Ok(snap) = HomeView::snapshot(&w) else {
            return false;
        };
        self.apply_balances(snap, cx);
        self.wallet_loaded
    }

    fn show_stopped_sync(&mut self, cx: &mut Context<Self>) {
        self.status = if self.paint_wallet_now(cx) {
            Self::set_field(&self.restore, "", cx);
            Self::set_field(&self.ufvk, "", cx);
            "Scan stopped. Saved progress will resume on Sync.".into()
        } else {
            "Scan stopped before the wallet was saved. Restore again to continue.".into()
        };
    }

    fn live_sync(&self) -> Option<SyncProgress> {
        self.sync_live
            .as_ref()
            .and_then(|l| l.lock().ok().map(|g| g.clone()))
    }

    fn live_remaining(&self) -> Option<u64> {
        let live = self.live_sync()?;
        let scanned = live.scanned_height?;
        let tip = live.tip_height?;
        Some(tip.saturating_sub(scanned))
    }

    fn shows_blocking_work(&self) -> bool {
        match self.work {
            WorkKind::None => false,
            WorkKind::Restore => true,
            WorkKind::Sync => historic_overlay_visible(
                self.live_remaining()
                    .map(|left| u32::try_from(left).unwrap_or(u32::MAX)),
            ),
            _ => true,
        }
    }

    fn show_sync_strip(&self) -> bool {
        self.catchup_busy && self.sync_live.is_some()
    }

    fn sync_background(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.sync_paused
            || self.rescan_prompt != RescanPrompt::None
            || !self.has_wallet
        {
            return;
        }
        if self.catchup_busy {
            return;
        }
        if self.work == WorkKind::None {
            // The 45 s catch-up must not clear the last send's notice and txid,
            // or a failure the user has not read yet.
            let (success, error) = (self.success.take(), self.work_error.take());
            self.begin_work(WorkKind::Sync, cx);
            self.success = success;
            self.work_error = error;
        }
        self.catchup_busy = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.sync_cancel = Some(Arc::clone(&cancel));
        let cancel_for_ui = Arc::clone(&cancel);
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        let publication = self.snapshot_generation;
        let session = self.session_generation;
        let wallet = self.wallet_dir.clone();
        let live = self.attach_sync_live();
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let server = self.server(cx);
        let skip_maintain = self.unlock_policy == UnlockPolicy::EachSpend;
        self.status = "Catching up in the background…".into();
        self.ensure_catchup(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let _wallet_job = wallet_job_gate().lock().unwrap_or_else(|e| e.into_inner());
                    unpanicked(|| {
                        if cancel.load(Ordering::Acquire) {
                            anyhow::bail!("sync cancelled; saved scan retained");
                        }
                        let rt = chain_sync_runtime()?;
                        rt.block_on(async {
                            let w = NativeWallet::open_with_light(&dir, Some(server))?;
                            let (tip, progress) = w
                                .sync_reported_resilient_cancellable(
                                    Some(live),
                                    Arc::clone(&cancel),
                                )
                                .await?;
                            if cancel.load(Ordering::Acquire) {
                                anyhow::bail!("sync cancelled; saved scan retained");
                            }
                            let message =
                                maintained(&w, &auth, skip_maintain, progress.message).await;
                            let snap = HomeView::snapshot(&w)?;
                            Ok::<_, anyhow::Error>((tip, message, snap))
                        })
                    })
                })
                .await;
            this.update(&mut *cx, |view, cx| {
                if !view.current_session(session, &wallet) {
                    return;
                }
                view.catchup_busy = false;
                view.sync_cancel = None;
                // A later send/probe/wipe owns its own status and snapshot. The
                // old catch-up must not repaint pre-action balances over it.
                if view.snapshot_generation != publication {
                    cx.notify();
                    return;
                }
                view.sync_live = None;
                if view.work == WorkKind::Sync {
                    view.work = WorkKind::None;
                }
                match result {
                    Ok((tip, msg, snap)) => {
                        view.apply_snapshot(snap, format!("Synced {tip}: {msg}"), cx);
                    }
                    Err(e) => {
                        view.status = if cancel_for_ui.load(Ordering::Acquire) {
                            "Scan stopped. Saved progress will resume on Sync.".into()
                        } else {
                            format!("Catch-up: {}", display_anyhow(&e)).into()
                        };
                    }
                }
                cx.notify();
            })
            .ok();
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn stop_sync(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.sync_cancel {
            cancel.store(true, Ordering::Release);
            self.sync_paused = true;
            self.status = "Stopping scan after the current database batch…".into();
            cx.notify();
        }
    }

    fn open_and_sync(&mut self, cx: &mut Context<Self>) {
        self.sync_paused = false;
        self.try_session_unlock(cx);
        self.paint_wallet_now(cx);
        if self.wallet_loaded {
            if self.work == WorkKind::None {
                self.begin_work(WorkKind::Sync, cx);
            }
            if self.catchup_busy {
                cx.notify();
            } else {
                self.sync_background(cx);
            }
            return;
        }
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let server = self.server(cx);
        let skip_maintain = self.unlock_policy == UnlockPolicy::EachSpend;
        self.begin_work(WorkKind::Sync, cx);
        let live = self.attach_sync_live();
        let cancel = self.attach_sync_cancel();
        self.run_blocking(
            cx,
            "opening and syncing…",
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let mut w = NativeWallet::open_with_light(&dir, Some(server))?;
                    let _ = w.ensure_ufvk();
                    let (tip, progress) = w
                        .sync_reported_resilient_cancellable(Some(live), Arc::clone(&cancel))
                        .await?;
                    if cancel.load(Ordering::Acquire) {
                        anyhow::bail!("sync cancelled; saved scan retained");
                    }
                    let message = maintained(&w, &auth, skip_maintain, progress.message).await;
                    let snap = HomeView::snapshot(&w)?;
                    Ok((tip, message, snap))
                })
            },
            |view, (tip, msg, snap), cx| {
                view.apply_snapshot(snap, format!("Synced {tip}: {msg}"), cx);
            },
        );
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.sync_paused = false;
        if !self.has_wallet {
            self.status = "Create or Restore a wallet before Sync.".into();
            cx.notify();
            return;
        }
        self.paint_wallet_now(cx);
        if self.wallet_loaded {
            self.begin_work(WorkKind::Sync, cx);
            if self.catchup_busy {
                cx.notify();
            } else {
                self.sync_background(cx);
            }
            return;
        }
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let server = self.server(cx);
        let skip_maintain = self.unlock_policy == UnlockPolicy::EachSpend;
        self.begin_work(WorkKind::Sync, cx);
        let live = self.attach_sync_live();
        let cancel = self.attach_sync_cancel();
        self.run_blocking(
            cx,
            "syncing…",
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let w = NativeWallet::open_with_light(&dir, Some(server))?;
                    let (tip, progress) = w
                        .sync_reported_resilient_cancellable(Some(live), Arc::clone(&cancel))
                        .await?;
                    if cancel.load(Ordering::Acquire) {
                        anyhow::bail!("sync cancelled; saved scan retained");
                    }
                    let message = maintained(&w, &auth, skip_maintain, progress.message).await;
                    let snap = HomeView::snapshot(&w)?;
                    Ok((tip, message, snap))
                })
            },
            |view, (tip, msg, snap), cx| {
                view.apply_snapshot(snap, format!("Synced {tip}: {msg}"), cx);
            },
        );
    }

    fn request_rescan(&mut self, cx: &mut Context<Self>) {
        if !self.has_wallet {
            self.status = "Open a wallet before rescanning.".into();
            cx.notify();
            return;
        }
        if self.busy || self.catchup_busy {
            self.status = "Wait for the current sync to finish, then rescan.".into();
            cx.notify();
            return;
        }
        self.rescan_prompt = RescanPrompt::Choose;
        cx.notify();
    }

    fn rescan_gap(&mut self, cx: &mut Context<Self>) {
        self.rescan_prompt = RescanPrompt::None;
        if !self.has_wallet {
            self.status = "Open a wallet before rescanning.".into();
            cx.notify();
            return;
        }
        if self.busy || self.catchup_busy {
            self.status = "Wait for the current sync to finish, then rescan.".into();
            cx.notify();
            return;
        }
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let server = self.server(cx);
        let skip_maintain = self.unlock_policy == UnlockPolicy::EachSpend;
        self.begin_work(WorkKind::ResetScan, cx);
        let live = self.attach_sync_live();
        let cancel = self.attach_sync_cancel();
        self.run_blocking(
            cx,
            "rewinding to the last filled island, then syncing the gap…",
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let w = NativeWallet::open_with_light(&dir, Some(server))?;
                    w.rewind_scan_to_gap().await?;
                    let (tip, progress) = w
                        .sync_reported_resilient_cancellable(Some(live), Arc::clone(&cancel))
                        .await?;
                    if cancel.load(Ordering::Acquire) {
                        anyhow::bail!("sync cancelled; saved scan retained");
                    }
                    let message = maintained(&w, &auth, skip_maintain, progress.message).await;
                    let snap = HomeView::snapshot(&w)?;
                    Ok((tip, message, snap))
                })
            },
            |view, (tip, msg, snap), cx| {
                view.apply_snapshot(snap, format!("Rescanned to {tip}: {msg}"), cx);
            },
        );
    }

    fn wipe_scan_from_birthday(&mut self, cx: &mut Context<Self>) {
        self.rescan_prompt = RescanPrompt::None;
        if !self.has_wallet {
            self.status = "Open a wallet before wiping scan.".into();
            cx.notify();
            return;
        }
        if self.busy || self.catchup_busy {
            self.status = "Wait for the current sync to finish, then wipe scan.".into();
            cx.notify();
            return;
        }
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let server = self.server(cx);
        let skip_maintain = self.unlock_policy == UnlockPolicy::EachSpend;
        self.begin_work(WorkKind::WipeScan, cx);
        let live = self.attach_sync_live();
        let cancel = self.attach_sync_cancel();
        self.run_blocking(
            cx,
            "clearing notes and scan cache, then syncing from birthday…",
            move || {
                let rt = chain_sync_runtime()?;
                rt.block_on(async {
                    let w = NativeWallet::open_with_light(&dir, Some(server))?;
                    w.reset_scan().await?;
                    let (tip, progress) = w
                        .sync_reported_resilient_cancellable(Some(live), Arc::clone(&cancel))
                        .await?;
                    if cancel.load(Ordering::Acquire) {
                        anyhow::bail!("sync cancelled; saved scan retained");
                    }
                    let message = maintained(&w, &auth, skip_maintain, progress.message).await;
                    let snap = HomeView::snapshot(&w)?;
                    Ok((tip, message, snap))
                })
            },
            |view, (tip, msg, snap), cx| {
                view.apply_snapshot(snap, format!("Wiped scan; synced to {tip}: {msg}"), cx);
            },
        );
    }

    fn begin_work(&mut self, kind: WorkKind, cx: &mut Context<Self>) {
        self.work = kind;
        self.work_tick = 0;
        self.work_started = Instant::now();
        self.success = None;
        self.work_error = None;
        if kind.is_chain_scan() {
            self.sync_server = self.field_text(&self.light, cx);
        }
        self.ensure_work_ticks(cx);
        cx.notify();
    }

    fn attach_sync_live(&mut self) -> Arc<StdMutex<SyncProgress>> {
        let live = Arc::new(StdMutex::new(SyncProgress {
            stage: SyncStage::Connecting,
            percent: 0.0,
            message: "connecting".into(),
            ..Default::default()
        }));
        self.sync_live = Some(live.clone());
        live
    }

    fn attach_sync_cancel(&mut self) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.sync_cancel = Some(Arc::clone(&cancel));
        self.sync_paused = false;
        cancel
    }

    fn set_success(&mut self, title: impl Into<SharedString>, txids: Vec<String>) {
        if txids.is_empty() {
            return;
        }
        self.work_error = None;
        self.success = Some(SuccessNotice {
            title: title.into(),
            txids,
        });
    }

    fn ensure_work_ticks(&mut self, cx: &mut Context<Self>) {
        if self.work_ticks_running {
            return;
        }
        self.work_ticks_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_spawn(async {
                    std::thread::sleep(std::time::Duration::from_millis(90));
                })
                .await;
                let keep = this.update(&mut *cx, |view, cx| {
                    view.work_tick = view.work_tick.saturating_add(1);
                    view.on_sync_tick(cx);
                    if view.work == WorkKind::None && view.sync_live.is_none() {
                        view.work_ticks_running = false;
                        false
                    } else {
                        true
                    }
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    }

    fn work_elapsed_ms(&self) -> u128 {
        self.work_started.elapsed().as_millis()
    }

    fn work_step(&self) -> usize {
        let ms = self.work_elapsed_ms();
        match self.work {
            WorkKind::None | WorkKind::Restore | WorkKind::AttachSeed => 0,
            WorkKind::Shield => match ms {
                0..=1400 => 0,
                1401..=2800 => 1,
                2801..=5200 => 2,
                _ => 3,
            },
            WorkKind::Send => match ms {
                0..=1400 => 0,
                1401..=4200 => 1,
                _ => 2,
            },
            WorkKind::Sync | WorkKind::ResetScan | WorkKind::WipeScan => {
                let pct = self
                    .sync_live
                    .as_ref()
                    .and_then(|l| l.lock().ok().map(|g| g.percent))
                    .unwrap_or(0.0);
                if pct < 8.0 {
                    0
                } else if pct < 92.0 {
                    1
                } else {
                    2
                }
            }
        }
    }

    fn shield(&mut self, cx: &mut Context<Self>) {
        if !self.wallet_loaded {
            self.status = "Open the wallet before Shield.".into();
            cx.notify();
            return;
        }
        self.try_session_unlock(cx);
        if self.needs_spend_unlock(cx) {
            self.request_spend_unlock(
                WorkKind::Shield,
                if self.view_only {
                    "View-only wallet. Paste the seed or confirm Hello to shield."
                } else {
                    "Unlock to shield: Hello / passphrase / seed."
                },
                cx,
            );
            return;
        }
        self.begin_work(WorkKind::Shield, cx);
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        self.run_blocking(
            cx,
            "proving shield (can take a minute)…",
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async {
                    let w = NativeWallet::open(&dir)?;
                    let txids = w.shield(&auth, SHIELD_THRESHOLD_ZAT).await?;
                    let snap = HomeView::snapshot(&w)?;
                    Ok((txids, snap))
                })
            },
            |view, (txids, snap), cx| {
                if view.unlock_policy == UnlockPolicy::EachSpend {
                    view.spend_confirmed = false;
                    Self::set_field(&view.spend_seed, "", cx);
                }
                view.set_success("Shield forged", txids.clone());
                view.apply_snapshot(snap, format!("Shielded {}", txids.join(", ")), cx);
            },
        );
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        if !self.wallet_loaded {
            self.status = "Open the wallet before Send.".into();
            cx.notify();
            return;
        }
        self.try_session_unlock(cx);
        if self.needs_spend_unlock(cx) {
            self.request_spend_unlock(
                WorkKind::Send,
                if self.view_only {
                    "View-only wallet. Paste the seed or confirm Hello to sign this send."
                } else {
                    "Unlock to sign: Hello / passphrase / seed."
                },
                cx,
            );
            return;
        }
        let pasted = self.field_text(&self.spend_seed, cx);
        let pasted = pasted.trim().to_string();
        let to = self.field_text(&self.send_to, cx);
        let to = to.trim().to_string();
        if to.is_empty() {
            self.status = "Paste a unified address first.".into();
            cx.notify();
            return;
        }
        let amount_raw = self.field_text(&self.send_amount, cx);
        let zip = to.to_ascii_lowercase().starts_with("zcash:");
        let amount = if zip && amount_raw.trim().is_empty() {
            0
        } else {
            match parse_zec_to_zatoshis(&amount_raw) {
                Ok(v) if v > 0 || zip => v,
                Ok(_) => {
                    self.status = "Amount must be greater than 0.".into();
                    cx.notify();
                    return;
                }
                Err(e) => {
                    self.status = format!("amount: {e}").into();
                    cx.notify();
                    return;
                }
            }
        };
        let memo = self.field_text(&self.send_memo, cx);
        let memo = memo.trim().to_string();
        let memo_opt = if memo.is_empty() { None } else { Some(memo) };
        self.begin_work(WorkKind::Send, cx);
        let dir = self.wallet_dir.clone();
        let auth = self.auth(cx);
        let attach = self.keep_seed_in_os && !pasted.is_empty();
        let attach_words = pasted.clone();
        if !pasted.is_empty() {
            self.cache_session_seed(&pasted);
        }
        self.run_blocking(
            cx,
            "proving send (can take a minute)…",
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async {
                    let mut w = NativeWallet::open(&dir)?;
                    let txids = w.send(&auth, &to, amount, memo_opt.as_deref()).await?;
                    let persist_err = if attach && !attach_words.trim().is_empty() {
                        w.attach_seed(&attach_words, &auth)
                            .err()
                            .map(|e| e.to_string())
                    } else {
                        None
                    };
                    let snap = HomeView::snapshot(&w)?;
                    Ok((txids, snap, persist_err))
                })
            },
            |view, (txids, snap, persist_err), cx| {
                Self::set_field(&view.send_memo, "", cx);
                if view.unlock_policy == UnlockPolicy::EachSpend {
                    view.spend_confirmed = false;
                    Self::set_field(&view.spend_seed, "", cx);
                }
                view.seed_prompt = false;
                view.set_success("Payment sealed", txids.clone());
                let mut status = format!("Sent {}", txids.join(", "));
                if let Some(e) = persist_err {
                    status.push_str(&format!(" · seed was not saved: {e}"));
                }
                view.apply_snapshot(snap, status, cx);
            },
        );
    }

    fn rotate_address(&mut self, cx: &mut Context<Self>) {
        let dir = self.wallet_dir.clone();
        self.run_blocking(
            cx,
            "new address…",
            move || {
                let w = NativeWallet::open(&dir)?;
                let ua = w.next_unified_address()?;
                let mut snap = HomeView::snapshot(&w)?;
                snap.address = ua;
                Ok(snap)
            },
            |view, snap, cx| {
                view.apply_snapshot(snap, "Rotated receive address", cx);
            },
        );
    }

    fn persist_lab(&self) {
        // A wallet pinned by Z_STACK_WALLET (scratch or test runs) must not
        // replace the saved choice the next plain launch reopens.
        if Self::env_wallet_pinned() {
            return;
        }
        persist_desktop_lab(
            self.network,
            &self.wallet_dir,
            self.previous_wallet_dir.as_deref(),
        );
    }

    fn env_wallet_pinned() -> bool {
        std::env::var("Z_STACK_WALLET").is_ok()
    }

    fn bridge_wallet_pinned() -> bool {
        std::env::var("Z_STACK_BRIDGE").is_ok_and(|bind| bind != "off" && bind != "0")
    }

    fn clear_wallet_switch_inputs(&mut self, cx: &mut Context<Self>) {
        Self::set_field(&self.pass, "", cx);
        Self::set_field(&self.send_to, "", cx);
        Self::set_field(&self.send_amount, "", cx);
        Self::set_field(&self.send_memo, "", cx);
        Self::set_field(&self.birthday, "", cx);
        Self::set_field(&self.birthday_date, "", cx);
        self.last_bday_h.clear();
        self.last_bday_d.clear();
        self.show_pass = false;
        self.publish_auth(cx);
    }

    fn switch_wallet_blocked(&mut self, cx: &mut Context<Self>) -> bool {
        let reason = if Self::env_wallet_pinned() {
            Some("Z_STACK_WALLET pins this folder. Relaunch without that override to switch wallets.")
        } else if Self::bridge_wallet_pinned() {
            Some("The desktop web bridge pins this folder. Relaunch with Z_STACK_BRIDGE=off to switch wallets.")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.status = reason.into();
            cx.notify();
            return true;
        }
        false
    }

    fn log_out_for_new_seed(&mut self, cx: &mut Context<Self>) {
        if self.switch_wallet_blocked(cx) {
            return;
        }
        if self.mnemonic.is_some() {
            self.status = "Copy and dismiss the newly created seed before logging out.".into();
            cx.notify();
            return;
        }
        let old = self.wallet_dir.clone();
        let new = match reserve_wallet_profile(&old, self.network) {
            Ok(dir) => dir,
            Err(e) => {
                self.status = format!("Could not prepare a new wallet folder: {e}").into();
                cx.notify();
                return;
            }
        };
        self.previous_wallet_dir = Some(old.clone());
        self.adopt_folder(new.clone(), self.network, cx);
        self.unlock_policy = UnlockPolicy::Session;
        self.os_unlock = true;
        self.clear_wallet_switch_inputs(cx);
        self.status = format!(
            "Logged out. {} is intact. Restore the other seed in {} using a birthday before its first transaction.",
            old.display(),
            new.display()
        )
        .into();
        cx.notify();
    }

    fn reopen_previous_wallet(&mut self, cx: &mut Context<Self>) {
        if self.switch_wallet_blocked(cx) {
            return;
        }
        let Some(previous) = self.previous_wallet_dir.clone() else {
            return;
        };
        if wallet_network(&previous) != Some(self.network) {
            self.status =
                format!("Previous wallet is unavailable in {}.", previous.display()).into();
            cx.notify();
            return;
        }
        let current = self.wallet_dir.clone();
        self.previous_wallet_dir = Some(current);
        self.adopt_folder(previous, self.network, cx);
        self.clear_wallet_switch_inputs(cx);
    }

    fn reset_wallet_session(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.sync_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.session_generation = self.session_generation.wrapping_add(1);
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        self.invalidate_send_estimates();
        self.sync_live = None;
        self.work = WorkKind::None;
        self.seed_prompt = false;
        self.rescan_prompt = RescanPrompt::None;
        self.pending_unlock = None;
        self.spend_confirmed = false;
        self.catchup_started = false;
        self.catchup_busy = false;
        self.sync_paused = false;
        self.wallet_loaded = false;
        self.has_wallet = false;
        self.disk_network = None;
        self.session_seed = None;
        self.mnemonic = None;
        self.view_only = false;
        self.wallet_ufvk = "".into();
        self.history.clear();
        self.address = "".into();
        self.t_address = "".into();
        self.orchard = "0".into();
        self.transparent = "0".into();
        self.pending = "0".into();
        self.total = "0".into();
        self.light_ok = None;
        self.probed_tip = None;
        self.success = None;
        self.work_error = None;
        self.send_inspect = SharedString::default();
        self.send_fee = None;
        self.send_quote_key.clear();
        Self::set_field(&self.restore, "", cx);
        Self::set_field(&self.ufvk, "", cx);
        Self::set_field(&self.spend_seed, "", cx);
    }

    fn adopt_folder(&mut self, dir: PathBuf, network: Network, cx: &mut Context<Self>) {
        let _ = std::fs::create_dir_all(&dir);
        self.wallet_dir = dir;
        self.reset_wallet_session(cx);
        let disk = NativeWallet::open(&self.wallet_dir).ok();
        self.disk_network = disk.as_ref().map(|w| w.network());
        self.has_wallet = disk.is_some();
        if let Some(disk_net) = self.disk_network {
            if disk_net != network {
                self.network = disk_net;
                self.status = format!(
                    "{} already holds a {} wallet, not {}.",
                    self.wallet_dir.display(),
                    disk_net.as_str(),
                    network.as_str()
                )
                .into();
                if let Some(Ok(snap)) = disk.as_ref().map(HomeView::snapshot) {
                    self.apply_balances(snap, cx);
                }
            } else {
                self.network = disk_net;
                if let Some(w) = disk.as_ref() {
                    Self::set_field(&self.light, w.server_url(), cx);
                    Self::set_field(&self.rpc, w.validator_rpc_url().unwrap_or_default(), cx);
                    if let Ok(snap) = HomeView::snapshot(w) {
                        self.apply_balances(snap, cx);
                    }
                }
                self.status = format!(
                    "Using {} ({}) — Sync when you want a catch-up.",
                    self.wallet_dir.display(),
                    disk_net.as_str()
                )
                .into();
            }
        } else {
            self.network = network;
            self.apply_network_defaults(cx);
            self.probe_note = format!(
                "Empty folder {} for {}. Probe Zaino, then Create or Restore.",
                self.wallet_dir.display(),
                network.as_str()
            )
            .into();
            self.status = format!(
                "Network is {}. Folder {}. Probe Zaino, then Create — or Restore with a birthday.",
                network.as_str(),
                self.wallet_dir.display()
            )
            .into();
        }
        self.persist_lab();
        cx.notify();
    }

    fn set_network(&mut self, network: Network, cx: &mut Context<Self>) {
        if self.busy {
            self.status = "Wait for the current job to finish, then switch network.".into();
            cx.notify();
            return;
        }
        if Self::env_wallet_pinned() {
            if let Some(disk) = self.disk_network {
                if disk != network {
                    self.status = format!(
                        "Z_STACK_WALLET pins {} to {}. Unset it, or point it at an empty folder, to use {}.",
                        self.wallet_dir.display(),
                        disk.as_str(),
                        network.as_str()
                    )
                    .into();
                    cx.notify();
                    return;
                }
            }
            if self.network != network {
                self.network = network;
                self.apply_network_defaults(cx);
            }
            self.persist_lab();
            cx.notify();
            return;
        }
        let dest = dir_for_network(network);
        if dest != self.wallet_dir || self.disk_network.is_some_and(|d| d != network) {
            self.adopt_folder(dest, network, cx);
            return;
        }
        if self.network == network {
            return;
        }
        self.network = network;
        self.apply_network_defaults(cx);
        self.light_ok = None;
        self.probe_note = format!(
            "Endpoints set for {}. Probe before you create.",
            network.as_str()
        )
        .into();
        self.status = format!(
            "Network is {}. Probe Zaino, then Create — or Restore with a birthday.",
            network.as_str()
        )
        .into();
        self.persist_lab();
        cx.notify();
    }

    fn cycle_light(&mut self, cx: &mut Context<Self>) {
        let cur = LightServer::parse(&self.field_text(&self.light, cx), self.network).as_url();
        let next = match self.network {
            Network::Mainnet => {
                let lab = LightServer::LOCAL_ZAINO_GRPC_MAINNET;
                let public = LightServer::for_network(Network::Mainnet).as_url();
                if cur == lab {
                    public
                } else {
                    lab.to_string()
                }
            }
            _ => {
                let local = LightServer::local_for_network(self.network).as_url();
                let public = LightServer::for_network(self.network).as_url();
                if self.using_local_light(cx) {
                    public
                } else {
                    local
                }
            }
        };
        Self::set_field(&self.light, next, cx);
        self.light_ok = None;
        cx.notify();
    }

    fn ensure_local(&mut self, cx: &mut Context<Self>) {
        if !self.using_local_light(cx) {
            self.cycle_light(cx);
        }
        let net = self.network;
        let launcher = launcher_bin();
        self.run_blocking(
            cx,
            format!("ensuring local Zaino on {}…", net.as_str()),
            move || {
                let net_s = net.as_str().to_string();
                if launcher.exists() {
                    let ready = std::process::Command::new(&launcher)
                        .args(["ready", "--network", &net_s])
                        .output();
                    if let Ok(out) = ready {
                        if out.status.success() {
                            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
                        }
                    }
                    let up = std::process::Command::new(&launcher)
                        .args(["up", "--network", &net_s, "--wait-secs", "8"])
                        .output()?;
                    let stdout = String::from_utf8_lossy(&up.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&up.stderr).trim().to_string();
                    let mut msg = stdout;
                    if !stderr.is_empty() {
                        if !msg.is_empty() {
                            msg.push('\n');
                        }
                        msg.push_str(&stderr);
                    }
                    if !up.status.success() {
                        msg.push_str("\nStart Zaino + Zakura, or: z-node-launcher up --network ");
                        msg.push_str(&net_s);
                        msg.push_str(" --docker");
                    }
                    return Ok(msg);
                }
                Ok(format!(
                    "z-node-launcher not next to this binary. Probe local ports, or cargo run -p z-node-launcher -- ready --network {net_s}"
                ))
            },
            |view, msg, cx| {
                view.probe_note = msg.into();
                view.probe(cx);
            },
        );
    }

    fn probe(&mut self, cx: &mut Context<Self>) {
        let network = self.network;
        let light = self.server(cx);
        let rpc = self.field_text(&self.rpc, cx).trim().to_string();
        self.run_blocking(
            cx,
            format!("probing {} Zaino and validator…", network.as_str()),
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async {
                    let light_p = NativeWallet::probe_light(&light).await;
                    let val = if rpc.is_empty() {
                        pick_local_validator(network).unwrap_or_else(|| {
                            probe_validator(LightServer::local_validator_rpc(network))
                        })
                    } else {
                        probe_validator(&rpc)
                    };
                    Ok((light_p, val))
                })
            },
            |view, (light_p, val), cx| {
                view.light_ok = Some(light_p.ok);
                if val.ok && view.field_text(&view.rpc, cx).trim().is_empty() {
                    Self::set_field(&view.rpc, val.url.clone(), cx);
                }
                if let Some(tip) = light_p.tip {
                    view.probed_tip = Some(tip as u32);
                    if view.field_text(&view.birthday, cx).trim().is_empty()
                        && view.network != Network::Regtest
                    {
                        let h = (tip as u32).saturating_sub(100).max(1);
                        view.last_bday_h.clear();
                        Self::set_field(&view.birthday, h.to_string(), cx);
                        view.sync_birthday_fields(cx);
                    }
                }
                let loopback = is_loopback_light_url(&light_p.url);
                let tscan = if light_p.t_scan {
                    "t-scan on"
                } else if uses_fast_sync(&light_p.url) && !loopback {
                    "t-scan off (loopback only) · dedicated Zaino, no bridge token"
                } else {
                    "t-scan off (public LWD)"
                };
                view.probe_note = {
                    let mut note = format!(
                        "Zaino {} · chain {} · tip {:?} · {tscan}\nZakura {} · chain {} · height {:?} · {}",
                        if light_p.ok { "ok" } else { "down" },
                        if light_p.chain.is_empty() { "?" } else { &light_p.chain },
                        light_p.tip,
                        if val.ok { "ok" } else { "down" },
                        if val.chain.is_empty() { "?" } else { &val.chain },
                        val.height,
                        val.subversion.as_deref().unwrap_or(val.error.as_deref().unwrap_or("")),
                    );
                    let chain_l = light_p.chain.to_lowercase();
                    let mismatch = light_p.ok
                        && match view.network {
                            Network::Mainnet => !chain_l.contains("main"),
                            Network::Testnet => {
                                !(chain_l.contains("test") && !chain_l.contains("regtest"))
                            }
                            Network::Regtest => {
                                !(chain_l.contains("regtest") || chain_l.contains("test"))
                            }
                        };
                    if mismatch {
                        note.push_str(&format!(
                            "\nchain mismatch: this Zaino is '{}' not {}",
                            light_p.chain,
                            view.network.as_str()
                        ));
                    }
                    if view.network == Network::Mainnet && light_p.url.contains(":8137") {
                        note.push_str(&format!(
                            "\nverify this endpoint's network; the default mainnet Zaino endpoint is {}",
                            LightServer::LOCAL_ZAINO_GRPC_MAINNET
                        ));
                    }
                    note.into()
                };
                view.status = if light_p.ok {
                    format!("{} light {}  tip {:?}", view.network.as_str(), light_p.url, light_p.tip)
                        .into()
                } else {
                    format!(
                        "Zaino not reachable at {} — {}",
                        light_p.url,
                        light_p.error.as_deref().unwrap_or("error")
                    )
                    .into()
                };
            },
        );
    }

    fn visible_fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Passphrase, Field::Light, Field::Rpc];
        if !self.has_wallet {
            if self.restore_kind == RestoreKind::Ufvk {
                fields.push(Field::Ufvk);
            } else {
                fields.push(Field::RestoreWords);
            }
            fields.push(Field::Birthday);
            fields.push(Field::BirthdayDate);
        }
        if self.seed_prompt {
            fields.push(Field::SpendSeed);
        }
        if self.wallet_loaded {
            fields.push(Field::SendTo);
            fields.push(Field::SendMemo);
            fields.push(Field::Amount);
        }
        fields
    }

    fn tab_fields(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.visible_fields();
        if fields.is_empty() {
            return;
        }
        let current = fields
            .iter()
            .position(|&f| self.input(f).read(cx).is_focused(window));
        let next = match current {
            None => {
                if back {
                    *fields.last().unwrap()
                } else {
                    fields[0]
                }
            }
            Some(i) if back => fields[(i + fields.len() - 1) % fields.len()],
            Some(i) => fields[(i + 1) % fields.len()],
        };
        let handle = self.input(next).read(cx).handle();
        window.focus(&handle);
        cx.notify();
    }

    fn submit_focused(&mut self, cx: &mut Context<Self>) {
        if !self.has_wallet {
            self.restore(cx);
            return;
        }
        if self.wallet_loaded {
            self.send(cx);
        }
    }

    fn overlay_up(&self) -> bool {
        self.busy
            || self.success.is_some()
            || self.work_error.is_some()
            || self.rescan_prompt != RescanPrompt::None
            || self.seed_prompt
    }

    fn on_next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_up() {
            self.tab_fields(false, window, cx);
        }
    }

    fn on_prev_field(&mut self, _: &PrevField, window: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_up() {
            self.tab_fields(true, window, cx);
        }
    }

    fn on_submit_focused(
        &mut self,
        _: &SubmitFocused,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.overlay_up() {
            self.submit_focused(cx);
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if self.rescan_prompt != RescanPrompt::None {
                self.rescan_prompt = RescanPrompt::None;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if self.success.take().is_some() || self.work_error.take().is_some() {
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        let _ = event;
    }

    fn dim_overlay(id: &'static str) -> Stateful<Div> {
        div()
            .id(id)
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0.09, 0.42, 0.05, 0.88))
    }

    fn work_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::dim_overlay("work-overlay").child(self.work_panel(cx))
    }

    fn sync_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.live_sync();
        let scanned = live.as_ref().and_then(|p| p.scanned_height).unwrap_or(0);
        let tip = live.as_ref().and_then(|p| p.tip_height).unwrap_or(0);
        let left = tip.saturating_sub(scanned);
        let pct = live
            .as_ref()
            .map(|p| display_catch_up_percent(p.percent, scanned as u32, tip as u32))
            .unwrap_or(90.0);
        let stalled = live
            .as_ref()
            .is_some_and(|p| p.message.contains("not moving"));
        let line = if stalled {
            live.as_ref()
                .map(|p| p.message.clone())
                .unwrap_or_else(|| "catching up".into())
        } else if left > 0 && left <= u64::from(NEAR_TIP_BLOCKS) {
            format!("{left} behind")
        } else if scanned > 0 && tip > 0 {
            format!("{pct:.0}% of catch-up  ·  {left} behind  ·  scanned {scanned} / {tip}")
        } else {
            live.as_ref()
                .map(|p| p.message.clone())
                .unwrap_or_else(|| "catching up".into())
        };
        div()
            .id("sync-strip")
            .absolute()
            .top(px(52.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .bg(theme::card_bg())
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if stalled {
                        theme::primary_color()
                    } else {
                        theme::line()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::plate())
                                    .child(line),
                            )
                            .child(
                                div()
                                    .id("stop-background-sync")
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.0))
                                    .bg(theme::field_bg())
                                    .text_color(theme::text_color())
                                    .cursor_pointer()
                                    .child("Stop scan")
                                    .on_click(cx.listener(|view, _event, _window, cx| {
                                        view.stop_sync(cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .h(px(6.0))
                            .w_full()
                            .bg(theme::field_bg())
                            .rounded(px(3.0))
                            .child(
                                div()
                                    .h_full()
                                    .w(px((pct / 100.0 * 436.0).max(8.0)))
                                    .bg(theme::primary_color())
                                    .rounded(px(3.0)),
                            ),
                    ),
            )
    }

    fn success_overlay(&self, notice: &SuccessNotice, cx: &mut Context<Self>) -> impl IntoElement {
        Self::dim_overlay("success-overlay").child(
            div().child(self.success_panel(notice, cx)).with_animation(
                "success-pop",
                Animation::new(Duration::from_millis(380)).with_easing(ease_in_out),
                |d, delta| d.opacity(0.35 + 0.65 * delta),
            ),
        )
    }

    fn error_overlay(&self, err: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        Self::dim_overlay("error-overlay").child(
            div()
                .w(px(460.0))
                .flex()
                .flex_col()
                .gap_3()
                .p_8()
                .bg(theme::card_bg())
                .rounded(px(12.0))
                .border_2()
                .border_color(theme::warn())
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::warn())
                        .child("Did not land"),
                )
                .child(div().text_color(theme::text_color()).child(err))
                .child(
                    div()
                        .id("dismiss-error")
                        .px_3()
                        .py_1()
                        .bg(theme::ink_on_plate())
                        .text_color(theme::plate())
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .child("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.work_error = None;
                            cx.notify();
                        })),
                ),
        )
    }

    fn rescan_choose_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::dim_overlay("rescan-choose").child(
            div()
                .w(px(520.0))
                .flex()
                .flex_col()
                .gap_3()
                .p_8()
                .bg(theme::card_bg())
                .rounded(px(12.0))
                .border_2()
                .border_color(theme::plate())
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::plate())
                        .child("Rescan"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child(
                            "Scan the gap fills island→tip (keeps notes already scanned). Wipe scan clears notes and the cache, then syncs from birthday. Keys and seed stay.",
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .flex_wrap()
                        .child(self.btn(
                            "confirm-gap",
                            "Scan the gap",
                            BtnKind::Ember,
                            true,
                            cx,
                            |v, cx| v.rescan_gap(cx),
                        ))
                        .child(self.btn(
                            "choose-wipe",
                            "Wipe scan & resync from birthday",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| {
                                v.rescan_prompt = RescanPrompt::ConfirmWipe;
                                cx.notify();
                            },
                        ))
                        .child(self.btn(
                            "cancel-rescan",
                            "Cancel",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| {
                                v.rescan_prompt = RescanPrompt::None;
                                cx.notify();
                            },
                        )),
                ),
        )
    }

    fn rescan_wipe_confirm_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::dim_overlay("rescan-wipe").child(
            div()
                .w(px(520.0))
                .flex()
                .flex_col()
                .gap_3()
                .p_8()
                .bg(theme::card_bg())
                .rounded(px(12.0))
                .border_2()
                .border_color(theme::plate())
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::plate())
                        .child("Wipe scan & resync from birthday"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child("Clear notes and scan cache? Sync starts from birthday."),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .flex_wrap()
                        .child(self.btn(
                            "confirm-wipe",
                            "Wipe scan & resync",
                            BtnKind::Ember,
                            true,
                            cx,
                            |v, cx| v.wipe_scan_from_birthday(cx),
                        ))
                        .child(self.btn(
                            "back-rescan",
                            "Back",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| {
                                v.rescan_prompt = RescanPrompt::Choose;
                                cx.notify();
                            },
                        ))
                        .child(self.btn(
                            "cancel-wipe",
                            "Cancel",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| {
                                v.rescan_prompt = RescanPrompt::None;
                                cx.notify();
                            },
                        )),
                ),
        )
    }

    fn seed_prompt_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shielding = self.pending_unlock == Some(WorkKind::Shield);
        let attaching = self.pending_unlock == Some(WorkKind::AttachSeed);
        Self::dim_overlay("seed-prompt").child(
            div()
                .w(px(520.0))
                .flex()
                .flex_col()
                .gap_3()
                .p_8()
                .bg(theme::card_bg())
                .rounded(px(12.0))
                .border_2()
                .border_color(theme::plate())
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::plate())
                        .child(if attaching {
                            "Attach seed"
                        } else if shielding {
                            "Unlock to shield"
                        } else {
                            "Unlock to sign"
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child("Paste the 12 or 24 words, or confirm Hello. This prompt is required every time you chose Ask each time."),
                )
                .child(self.labeled("Seed", &self.spend_seed))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .child(self.btn("paste-spend", "Paste", BtnKind::Ghost, true, cx, |v, cx| {
                            let input = v.spend_seed.clone();
                            v.paste_into(&input, cx);
                        }))
                        .child(self.pick_chip(
                            "keep-os",
                            if cfg!(windows) {
                                "Also save in Hello"
                            } else {
                                "Also save in OS keychain"
                            },
                            self.keep_seed_in_os,
                            cx,
                            |v, cx| {
                                v.keep_seed_in_os = !v.keep_seed_in_os;
                                cx.notify();
                            },
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .flex_wrap()
                        .child(self.btn(
                            "sign-now",
                            if attaching {
                                "Attach seed"
                            } else if shielding {
                                "Confirm shield"
                            } else {
                                "Confirm send"
                            },
                            BtnKind::Ember,
                            true,
                            cx,
                            |v, cx| v.confirm_pending_unlock(cx),
                        ))
                        .when(self.os_unlock, |d| {
                            d.child(self.btn(
                                "hello-now",
                                if cfg!(windows) {
                                    "Use saved OS seed"
                                } else {
                                    "Use saved OS seed"
                                },
                                BtnKind::Patina,
                                true,
                                cx,
                                |v, cx| v.confirm_pending_unlock(cx),
                            ))
                        })
                        .child(self.btn("cancel-sign", "Cancel", BtnKind::Ghost, true, cx, |v, cx| {
                            v.seed_prompt = false;
                            v.pending_unlock = None;
                            v.spend_confirmed = false;
                            cx.notify();
                        })),
                ),
        )
    }

    fn work_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, steps): (SharedString, &[&str]) = match self.work {
            WorkKind::Shield => (
                "Forging the shield".into(),
                &[
                    "Gathering transparent notes",
                    "Folding them through the turnstile",
                    "Proving the circuit",
                    "Broadcasting",
                ],
            ),
            WorkKind::Send => (
                "Sealing the payment".into(),
                &[
                    "Building the payment",
                    "Proving the circuit",
                    "Broadcasting",
                ],
            ),
            WorkKind::Restore => ("Restoring wallet".into(), &[]),
            WorkKind::Sync | WorkKind::ResetScan | WorkKind::WipeScan => (
                "Catching up".into(),
                &[
                    "Connecting to Zaino",
                    "Downloading compact blocks",
                    "Trial-decrypting notes",
                    "Writing the wallet",
                    "Reading memos",
                ],
            ),
            WorkKind::None | WorkKind::AttachSeed => ("Working".into(), &[]),
        };
        let live = self
            .sync_live
            .as_ref()
            .and_then(|l| l.lock().ok().map(|g| g.clone()));
        let chain_checks = live.as_ref().map(|p| {
            let scanned = p.scanned_height.unwrap_or(0);
            let tip = p.tip_height.unwrap_or(0);
            let dload = p.downloaded_height.unwrap_or(scanned);
            let scan_moved = p.decrypt_active
                || p.persist_active
                || matches!(p.stage, SyncStage::Scanning | SyncStage::CatchingUp);
            historic_overlay_checks(
                !matches!(p.stage, SyncStage::Connecting | SyncStage::Idle),
                scan_moved,
                dload > scanned || p.download_active,
                tip.saturating_sub(scanned) as u32,
                matches!(p.stage, SyncStage::Enhancing),
            )
        });
        let active = if self.work.is_chain_scan() {
            0
        } else {
            self.work_step()
        };
        let t = self.work_started.elapsed().as_secs_f32();
        let bar_w = if let Some(p) = &live {
            let scanned = p.scanned_height.unwrap_or(0) as u32;
            let tip = p.tip_height.unwrap_or(0) as u32;
            let dload = p.downloaded_height.unwrap_or(0) as u32;
            let pct = display_catch_up_percent(p.percent, scanned, tip);
            if pct < 0.5 && dload <= scanned {
                0.0
            } else {
                (pct / 100.0 * 404.0).max(12.0)
            }
        } else {
            (48.0 + t * 42.0).min(404.0)
        };
        let orbit = (0..6u32).map(|i| {
            let a = t * 0.55 + i as f32 * (std::f32::consts::TAU / 6.0);
            let r = 36.0;
            let x = 42.0 + a.cos() * r;
            let y = 42.0 + a.sin() * r;
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(8.0))
                .bg(if i % 2 == 0 {
                    theme::primary_color()
                } else {
                    theme::plate()
                })
                .opacity(0.7)
        });
        div()
            .relative()
            .w(px(480.0))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_4()
            .p_8()
            .bg(theme::card_bg())
            .rounded(px(14.0))
            .border_2()
            .border_color(theme::primary_color())
            .shadow(vec![BoxShadow {
                color: hsla(0.07, 0.85, 0.42, 0.28),
                offset: point(px(0.0), px(10.0)),
                blur_radius: px(28.0),
                spread_radius: px(0.0),
            }])
            .child(
                div()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .relative()
                            .w(px(96.0))
                            .h(px(96.0))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(22.0))
                                    .top(px(22.0))
                                    .w(px(52.0))
                                    .h(px(52.0))
                                    .rounded(px(26.0))
                                    .border_2()
                                    .border_color(theme::plate())
                                    .bg(hsla(0.08, 0.55, 0.22, 1.0)),
                            )
                            .children(orbit),
                    ),
            )
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::plate())
                    .child(title),
            )
            .when(self.work.is_chain_scan() && self.sync_cancel.is_some(), |d| {
                d.child(
                    div()
                        .id("stop-foreground-sync")
                        .px_3()
                        .py_2()
                        .rounded(px(4.0))
                        .bg(theme::field_bg())
                        .text_color(theme::text_color())
                        .cursor_pointer()
                        .child("Stop scan and keep saved progress")
                        .on_click(cx.listener(|view, _event, _window, cx| {
                            view.stop_sync(cx);
                        })),
                )
            })
            .when(
                self.work.is_chain_scan() && !self.sync_server.is_empty(),
                |d| {
                    d.child(
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(describe_light_url(&self.sync_server)),
                    )
                },
            )
            .child(
                div()
                    .h(px(10.0))
                    .w_full()
                    .bg(theme::field_bg())
                    .rounded(px(5.0))
                    .child(
                        div()
                            .h_full()
                            .w(px(bar_w))
                            .bg(theme::primary_color())
                            .rounded(px(5.0)),
                    ),
            )
            .children(steps.iter().enumerate().map(|(i, label)| {
                let state = chain_checks
                    .as_ref()
                    .and_then(|c| c.get(i).copied())
                    .unwrap_or_else(|| {
                        if i < active {
                            OverlayCheck::Done
                        } else if i == active {
                            OverlayCheck::Active
                        } else {
                            OverlayCheck::Pending
                        }
                    });
                let (mark, color) = match state {
                    OverlayCheck::Done => ("●", theme::ok()),
                    OverlayCheck::Active => ("◉", theme::primary_color()),
                    OverlayCheck::Pending => ("○", theme::muted()),
                };
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(color).child(mark))
                    .child(div().text_color(color).child(*label))
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(
                        live.as_ref()
                            .map(|p| p.message.clone())
                            .unwrap_or_else(|| {
                                if self.work == WorkKind::Sync {
                                    "Download, trial-decrypt, and persist overlap on local Zaino."
                                        .into()
                                } else if self.work == WorkKind::Restore {
                                    "Preparing the account and birthday; historical sync starts automatically next."
                                        .into()
                                } else if self.work == WorkKind::WipeScan {
                                    "Keys and birthday stay. Sync starts from birthday.".into()
                                } else if self.work == WorkKind::ResetScan {
                                    "Rewinding to the last filled island, then scanning to tip."
                                        .into()
                                } else {
                                    "Stay on this screen. Proving can take a minute.".into()
                                }
                            }),
                    ),
            )
            .when_some(live.as_ref().and_then(|p| {
                let scanned = p.scanned_height?;
                let tip = p.tip_height?;
                let pct = display_catch_up_percent(p.percent, scanned as u32, tip as u32);
                Some((
                    pct,
                    scanned,
                    p.downloaded_height,
                    tip,
                    p.eta_secs,
                    p.blocks_per_sec,
                    p.download_blocks_per_sec,
                    p.message.clone(),
                    p.stage,
                ))
            }), |d, (pct, scanned, downloaded, tip, eta, rate, down_rate, msg, stage)| {
                let left = tip.saturating_sub(scanned);
                let stalled = msg.contains("not moving");
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::plate())
                                .child({
                                    if left > 0 && left <= u64::from(QUIET_BEHIND_BLOCKS) {
                                        format!("{left} behind")
                                    } else if left > 0 && left <= u64::from(NEAR_TIP_BLOCKS) {
                                        format!("{left} behind  ·  scanned {scanned} / {tip}")
                                    } else if pct < 0.5
                                        && downloaded.unwrap_or(scanned) <= scanned
                                        && left > u64::from(NEAR_TIP_BLOCKS)
                                    {
                                        format!("starting  ·  scanned {scanned} / {tip}")
                                    } else {
                                        match downloaded {
                                            Some(dload) if dload > scanned => {
                                                format!("{pct:.0}% of catch-up  ·  down {dload}  ·  scan {scanned} / {tip}")
                                            }
                                            _ => {
                                                format!("{pct:.0}% of catch-up  ·  scanned {scanned} / {tip}")
                                            }
                                        }
                                    }
                                }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(if stalled {
                                    theme::primary_color()
                                } else {
                                    theme::ok()
                                })
                                .child({
                                    if stalled {
                                        msg
                                    } else {
                                    let mut parts = Vec::new();
                                    if left <= u64::from(NEAR_TIP_BLOCKS) {
                                        if let Some(s) = eta {
                                            parts.push(format!("about {} left", fmt_secs(s)));
                                        }
                                    }
                                    if let Some(r) = down_rate.filter(|r| *r >= 1.0) {
                                        parts.push(format!("{r:.0} down blk/s"));
                                    }
                                    if let Some(r) = rate.filter(|r| *r >= 1.0) {
                                        parts.push(format!("{r:.0} scan blk/s"));
                                    }
                                    if parts.is_empty() {
                                        match stage {
                                            SyncStage::Connecting => {
                                                "fetching merkle frontiers — not compact blocks yet"
                                                    .into()
                                            }
                                            SyncStage::Enhancing => "reading memos".into(),
                                            _ => "downloading · trial-decrypt · persist".into(),
                                        }
                                    } else {
                                        parts.join("  ·  ")
                                    }
                                    }
                                }),
                        ),
                )
            })
    }

    fn success_panel(&self, notice: &SuccessNotice, cx: &mut Context<Self>) -> impl IntoElement {
        let network = self.network;
        div()
            .w(px(480.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_8()
            .bg(theme::plate())
            .rounded(px(14.0))
            .shadow(vec![BoxShadow {
                color: hsla(0.09, 0.4, 0.08, 0.45),
                offset: point(px(0.0), px(12.0)),
                blur_radius: px(28.0),
                spread_radius: px(0.0),
            }])
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::ink_on_plate())
                    .child(notice.title.clone()),
            )
            .children(notice.txids.iter().map(|txid| {
                let short = if txid.len() > 20 {
                    format!("{}…{}", &txid[..12], &txid[txid.len() - 8..])
                } else {
                    txid.clone()
                };
                let full = txid.clone();
                let url = explorer_tx_url(network, txid);
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::ink_on_plate())
                            .child(format!("txid {short}")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child({
                                let full = full.clone();
                                div()
                                    .id(SharedString::from(format!("copy-tx-{full}")))
                                    .px_3()
                                    .py_1()
                                    .bg(theme::field_bg())
                                    .text_color(theme::text_color())
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .child("Copy txid")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_text(&full, "txid", cx);
                                    }))
                            })
                            .when_some(url, |d, url| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("open-tx-{full}")))
                                        .px_3()
                                        .py_1()
                                        .bg(theme::primary_color())
                                        .text_color(theme::text_color())
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .child("Open explorer")
                                        .on_click(cx.listener(move |_this, _, _, cx| {
                                            cx.open_url(&url);
                                        })),
                                )
                            }),
                    )
            }))
            .child(
                div()
                    .id("dismiss-success")
                    .px_3()
                    .py_1()
                    .bg(theme::ink_on_plate())
                    .text_color(theme::plate())
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .child("Dismiss")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.success = None;
                        cx.notify();
                    })),
            )
    }

    fn copy_text(&mut self, text: &str, what: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.status = format!("copied {what}").into();
        cx.notify();
    }

    fn btn(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        kind: BtnKind,
        enabled: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let disabled = self.busy || self.catchup_busy || !enabled;
        let (bg, fg) = match kind {
            BtnKind::Ember => (theme::primary_color(), theme::text_color()),
            BtnKind::Patina => (theme::secondary_color(), theme::text_color()),
            BtnKind::Ghost => (theme::field_bg(), theme::text_color()),
        };
        div()
            .id(id)
            .px_4()
            .py_2()
            .bg(bg)
            .text_color(fg)
            .rounded(px(4.0))
            .cursor_pointer()
            .opacity(if disabled { 0.45 } else { 1.0 })
            .child(label.into())
            .on_click(cx.listener(move |this, _e, _window, cx| {
                if !this.busy && !this.catchup_busy && enabled {
                    handler(this, cx);
                }
            }))
    }

    fn network_chip(
        &self,
        id: &'static str,
        network: Network,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.network == network;
        let env_locked = Self::env_wallet_pinned()
            && self.disk_network.is_some()
            && self.disk_network != Some(network);
        let bg = if selected {
            theme::primary_color()
        } else {
            theme::field_bg()
        };
        let border = if selected {
            theme::plate()
        } else {
            theme::line()
        };
        div()
            .id(id)
            .flex_shrink_0()
            .occlude()
            .px_4()
            .py_2()
            .min_w(px(92.0))
            .bg(bg)
            .text_color(theme::text_color())
            .border_1()
            .border_color(border)
            .rounded(px(4.0))
            .cursor_pointer()
            .opacity(if env_locked { 0.45 } else { 1.0 })
            .child(network.as_str())
            .on_click(cx.listener(move |this, _e, _window, cx| {
                this.set_network(network, cx);
            }))
    }

    fn pick_chip(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_3()
            .py_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .bg(if on {
                theme::primary_color()
            } else {
                theme::field_bg()
            })
            .text_color(theme::text_color())
            .border_1()
            .border_color(if on { theme::plate() } else { theme::line() })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
    }

    fn start_card(&self, create_label: String, cx: &mut Context<Self>) -> impl IntoElement {
        let seed_mode = self.restore_kind == RestoreKind::Seed;
        let tip = self.eta_tip();
        let height_raw = self.field_text(&self.birthday, cx);
        let date_raw = self.field_text(&self.birthday_date, cx);
        let raw = if !height_raw.trim().is_empty() {
            height_raw.clone()
        } else {
            date_raw.clone()
        };
        let parsed = parse_birthday_input_for_network(&raw, tip, self.network).ok();
        let eta = parsed.map(|b| {
            let fast = uses_fast_sync(&self.field_text(&self.light, cx));
            SyncEta::from_span(b, tip, fast)
        });
        let words_n = self
            .field_text(&self.restore, cx)
            .split_whitespace()
            .count();
        let ufvk_n = self.field_text(&self.ufvk, cx).trim().len();
        div()
            .p_5()
            .bg(theme::card_bg())
            .rounded(px(10.0))
            .border_1()
            .border_color(theme::line())
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_3()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::plate())
                    .child(format!("Start on {}", self.network.as_str())),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.pick_chip(
                        "rk-seed",
                        "Seed words",
                        seed_mode,
                        cx,
                        |v, cx| {
                            v.restore_kind = RestoreKind::Seed;
                            cx.notify();
                        },
                    ))
                    .child(self.pick_chip(
                        "rk-ufvk",
                        "Viewing key",
                        !seed_mode,
                        cx,
                        |v, cx| {
                            v.restore_kind = RestoreKind::Ufvk;
                            cx.notify();
                        },
                    )),
            )
            .when(seed_mode, |d| {
                d.child(self.labeled("Seed (12 or 24 words)", &self.restore))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(self.btn(
                                "paste-seed",
                                "Paste words",
                                BtnKind::Ghost,
                                true,
                                cx,
                                |v, cx| {
                                    let input = v.restore.clone();
                                    v.paste_into(&input, cx);
                                },
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if words_n == 12 || words_n == 24 {
                                        theme::ok()
                                    } else {
                                        theme::muted()
                                    })
                                    .child(if words_n == 0 {
                                        "Ctrl+V · newlines become spaces".into()
                                    } else if words_n == 12 || words_n == 24 {
                                        format!("{words_n} words — ready")
                                    } else {
                                        format!("{words_n} words — need 12 or 24")
                                    }),
                            ),
                    )
            })
            .when(!seed_mode, |d| {
                d.child(self.labeled("Unified viewing key", &self.ufvk))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(self.btn(
                                "paste-ufvk",
                                "Paste viewing key",
                                BtnKind::Ghost,
                                true,
                                cx,
                                |v, cx| {
                                    let input = v.ufvk.clone();
                                    v.paste_into(&input, cx);
                                },
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::muted())
                                    .child(if ufvk_n == 0 {
                                        "uview1… / uviewtest1…  ·  scan only until you paste a seed"
                                            .into()
                                    } else {
                                        format!("{ufvk_n} chars")
                                    }),
                            ),
                    )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child("Birthday — exact height, or about when you first used the wallet"),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.pick_chip("ago-7", "~1 week", false, cx, |v, cx| {
                        v.set_approx_days(7, cx)
                    }))
                    .child(self.pick_chip("ago-30", "~1 month", false, cx, |v, cx| {
                        v.set_approx_days(30, cx)
                    }))
                    .child(self.pick_chip("ago-365", "~1 year", false, cx, |v, cx| {
                        v.set_approx_days(365, cx)
                    }))
                    .child(self.pick_chip("ago-tip", "Near tip", false, cx, |v, cx| {
                        let h = v.eta_tip().saturating_sub(100).max(1);
                        v.last_bday_h.clear();
                        Self::set_field(&v.birthday, h.to_string(), cx);
                        v.sync_birthday_fields(cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.labeled("Exact height", &self.birthday)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.labeled("First-used date", &self.birthday_date)),
                    ),
            )
            .when_some(eta, |d, e| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(theme::ok())
                        .child(format!(
                            "Scan from {}  ·  tip {}  ·  {}",
                            parsed.unwrap_or(1),
                            tip,
                            e.human()
                        )),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child("Create uses tip−100 if both are empty. Restore needs height or date. Height and date stay in sync."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.pick_chip(
                        "un-session",
                        UnlockPolicy::Session.label(),
                        self.unlock_policy == UnlockPolicy::Session,
                        cx,
                        |v, cx| v.persist_unlock_policy(UnlockPolicy::Session, cx),
                    ))
                    .child(self.pick_chip(
                        "un-each",
                        UnlockPolicy::EachSpend.label(),
                        self.unlock_policy == UnlockPolicy::EachSpend,
                        cx,
                        |v, cx| v.persist_unlock_policy(UnlockPolicy::EachSpend, cx),
                    ))
                    .child(self.pick_chip(
                        "un-always",
                        UnlockPolicy::Always.label(),
                        self.unlock_policy == UnlockPolicy::Always,
                        cx,
                        |v, cx| v.persist_unlock_policy(UnlockPolicy::Always, cx),
                    ))
                    .child(self.pick_chip(
                        "os-unlock",
                        if cfg!(windows) {
                            "Hello / passkey"
                        } else if cfg!(target_os = "macos") {
                            "Keychain / passkey"
                        } else {
                            "System keyring"
                        },
                        self.os_unlock,
                        cx,
                        |v, cx| {
                            v.os_unlock = !v.os_unlock;
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.btn("create", create_label, BtnKind::Ember, true, cx, |v, cx| {
                        v.create(cx)
                    }))
                    .child(self.btn(
                        "restore",
                        if seed_mode { "Restore seed" } else { "Restore view-only" },
                        BtnKind::Patina,
                        true,
                        cx,
                        |v, cx| v.restore(cx),
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(format!(
                        "{}  ·  seed in {} (OS passkey, not a website login). UFVK + birthday stay in the wallet folder.",
                        self.unlock_policy.hint(),
                        SeedStore::os_unlock_label()
                    )),
            )
    }

    fn labeled(&self, label: &'static str, input: &Entity<FieldInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_shrink_0()
            .min_w(px(0.0))
            .w_full()
            .child(div().text_sm().text_color(theme::muted()).child(label))
            .child(input.clone())
    }

    fn header(
        &self,
        folder_line: String,
        next: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .px_6()
            .pt_5()
            .pb_3()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::plate())
                            .child("z-stack"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(if self.view_only {
                                "Shielded orchard · view-only until you paste a seed"
                            } else if self.session_seed.is_some() {
                                "Shielded orchard · seed unlocked this session"
                            } else {
                                "Shielded orchard · spend from here"
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .gap_2()
                            .child(self.network_chip("net-regtest", Network::Regtest, cx))
                            .child(self.network_chip("net-testnet", Network::Testnet, cx))
                            .child(self.network_chip("net-mainnet", Network::Mainnet, cx)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .min_w(px(0.0))
                            .flex_1()
                            .text_color(theme::muted())
                            .child(folder_line),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .bg(theme::well())
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme::line())
                    .text_sm()
                    .text_color(theme::plate())
                    .child(next),
            )
    }

    fn ledger_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("ledger")
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .p_5()
                    .bg(theme::card_bg())
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme::line())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(if self.view_only {
                                "Available · view-only"
                            } else {
                                "Available"
                            }),
                    )
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::plate())
                            .child(format!("{} ZEC", self.total)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(format!("orchard {}", self.orchard))
                            .child(format!("transparent {}", self.transparent))
                            .child(format!("pending {}", self.pending)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .flex_wrap()
                            .mt_2()
                            .child(self.btn(
                                "open",
                                if self.wallet_loaded { "Reload" } else { "Open wallet" },
                                BtnKind::Ghost,
                                true,
                                cx,
                                |v, cx| v.refresh(cx),
                            ))
                            .child(self.btn("sync", "Sync", BtnKind::Patina, true, cx, |v, cx| {
                                v.sync(cx)
                            }))
                            .child(self.btn(
                                "rescan",
                                "Rescan",
                                BtnKind::Ghost,
                                true,
                                cx,
                                |v, cx| v.request_rescan(cx),
                            ))
                            .child(self.btn("shield", "Shield", BtnKind::Ember, true, cx, |v, cx| {
                                v.shield(cx)
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .flex_wrap()
                            .child(self.btn(
                                "log-out-new-seed",
                                "Log out / new seed",
                                BtnKind::Ghost,
                                true,
                                cx,
                                |v, cx| v.log_out_for_new_seed(cx),
                            ))
                            .when(self.previous_wallet_dir.is_some(), |d| {
                                d.child(self.btn(
                                    "reopen-previous-wallet",
                                    "Previous wallet",
                                    BtnKind::Ghost,
                                    true,
                                    cx,
                                    |v, cx| v.reopen_previous_wallet(cx),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .flex_wrap()
                            .child(self.pick_chip(
                                "un-session-open",
                                UnlockPolicy::Session.label(),
                                self.unlock_policy == UnlockPolicy::Session,
                                cx,
                                |v, cx| v.persist_unlock_policy(UnlockPolicy::Session, cx),
                            ))
                            .child(self.pick_chip(
                                "un-each-open",
                                UnlockPolicy::EachSpend.label(),
                                self.unlock_policy == UnlockPolicy::EachSpend,
                                cx,
                                |v, cx| v.persist_unlock_policy(UnlockPolicy::EachSpend, cx),
                            ))
                            .child(self.pick_chip(
                                "un-always-open",
                                UnlockPolicy::Always.label(),
                                self.unlock_policy == UnlockPolicy::Always,
                                cx,
                                |v, cx| v.persist_unlock_policy(UnlockPolicy::Always, cx),
                            ))
                            .when(self.view_only, |d| {
                                d.child(self.btn(
                                    "unlock-view",
                                    "Paste seed to send",
                                    BtnKind::Ghost,
                                    true,
                                    cx,
                                    |v, cx| {
                                        v.request_spend_unlock(
                                            WorkKind::AttachSeed,
                                            "Paste the seed to enable sending. This does not send.",
                                            cx,
                                        );
                                    },
                                ))
                            })
                            .when(self.session_seed.is_some() && !self.view_only, |d| {
                                d.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::ok())
                                        .child("unlocked this session"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(self.unlock_policy.hint()),
                    ),
            )
            .when(!self.address.is_empty(), |d| {
                d.child(
                    div()
                        .p_4()
                        .bg(theme::well())
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme::line())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme::muted())
                                .child("Receive"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(self.pick_chip(
                                    "ua-full",
                                    "Full (+t)",
                                    self.receive_set == UaReceiverSet::Full,
                                    cx,
                                    |v, cx| v.set_receive_set(UaReceiverSet::Full, cx),
                                ))
                                .child(self.pick_chip(
                                    "ua-orchard",
                                    "Orchard",
                                    self.receive_set == UaReceiverSet::Orchard,
                                    cx,
                                    |v, cx| v.set_receive_set(UaReceiverSet::Orchard, cx),
                                ))
                                .child(self.pick_chip(
                                    "ua-shielded",
                                    "Shielded",
                                    self.receive_set == UaReceiverSet::Shielded,
                                    cx,
                                    |v, cx| v.set_receive_set(UaReceiverSet::Shielded, cx),
                                )),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme::muted())
                                .child(
                                    "UA can include transparent for receive. Spend is shielded only.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_4()
                                .items_start()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::plate())
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .child(wrap_chars(&self.displayed_receive_ua(), 44)),
                                )
                                .child(qr_block(
                                    &zip321_uri(&self.displayed_receive_ua(), None).unwrap_or_else(
                                        |_| self.displayed_receive_ua(),
                                    ),
                                    3.0,
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(self.btn("copy-ua", "Copy UA", BtnKind::Ghost, true, cx, |v, cx| {
                                    let addr = v.displayed_receive_ua();
                                    v.copy_text(&addr, "UA", cx);
                                }))
                                .child(self.btn("next-ua", "Next UA", BtnKind::Ghost, true, cx, |v, cx| {
                                    v.rotate_address(cx)
                                }))
                                .child(self.btn(
                                    "copy-zip321",
                                    "Copy ZIP-321",
                                    BtnKind::Ghost,
                                    true,
                                    cx,
                                    |v, cx| {
                                        let addr = v.displayed_receive_ua();
                                        match zip321_uri(&addr, None) {
                                            Ok(uri) => v.copy_text(&uri, "ZIP-321", cx),
                                            Err(e) => {
                                                v.status = e.into();
                                                cx.notify();
                                            }
                                        }
                                    },
                                )),
                        )
                        .when(
                            !self.t_address.is_empty() && self.receive_set.includes_transparent(),
                            |d| {
                                d.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::muted())
                                        .child(format!("Transparent edge {}", self.t_address)),
                                )
                            },
                        ),
                )
            })
            .when(!self.history.is_empty(), |d| {
                d.child(
                    div()
                        .p_4()
                        .bg(theme::well())
                        .rounded(px(8.0))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme::muted())
                                .child("Tape"),
                        )
                        .children(self.history.iter().map(|row| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .pb_2()
                                .border_b_1()
                                .border_color(theme::line())
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap_3()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::muted())
                                                .child(row.height.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::plate())
                                                .child(row.delta.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::muted())
                                                .child(row.tx.clone()),
                                        ),
                                )
                                .when(!row.memo.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_sm()
                                            .text_color(theme::ok())
                                            .child(format!("“{}”", row.memo)),
                                    )
                                })
                        })),
                )
            })
            .when_some(self.mnemonic.clone(), |d, words| {
                d.child(
                    div()
                        .p_5()
                        .bg(theme::plate())
                        .rounded(px(8.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::ink_on_plate())
                                .child("Write this down. It is not stored in the app."),
                        )
                        .child(div().text_color(theme::ink_on_plate()).child(words.clone()))
                        .when(!self.wallet_ufvk.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_sm()
                                    .text_color(theme::ink_on_plate())
                                    .child(format!(
                                        "Birthday {} · viewing key saved. Copy it for a watch-only restore.",
                                        self.birthday_h
                                    )),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::ink_on_plate())
                                    .child(wrap_chars(&self.wallet_ufvk, 44)),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .child(self.btn("copy-seed", "Copy words", BtnKind::Ghost, true, cx, |v, cx| {
                                    if let Some(m) = v.mnemonic.clone() {
                                        v.copy_text(&m, "mnemonic", cx);
                                    }
                                }))
                                .child(self.btn(
                                    "dismiss-seed",
                                    "I copied the words",
                                    BtnKind::Ember,
                                    true,
                                    cx,
                                    |v, cx| {
                                        v.mnemonic = None;
                                        cx.notify();
                                    },
                                ))
                                .when(!self.wallet_ufvk.is_empty(), |d| {
                                    d.child(self.btn(
                                        "copy-ufvk",
                                        "Copy viewing key",
                                        BtnKind::Ghost,
                                        true,
                                        cx,
                                        |v, cx| {
                                            let u = v.wallet_ufvk.to_string();
                                            v.copy_text(&u, "UFVK", cx);
                                        },
                                    ))
                                }),
                        ),
                )
            })
    }

    fn forge_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(400.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .p_5()
                    .bg(theme::card_bg())
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme::primary_color())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::plate())
                            .child("Send shielded"),
                    )
                    .child(self.labeled("To", &self.send_to))
                    .when(!self.send_inspect.is_empty(), |d| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(theme::muted())
                                .child(self.send_inspect.clone()),
                        )
                    })
                    .child(self.labeled("Memo", &self.send_memo))
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .items_end()
                            .child(div().w(px(140.0)).child(self.labeled("ZEC", &self.send_amount)))
                            .child(self.btn("max-send", "Max", BtnKind::Ghost, true, cx, |v, cx| {
                                v.fill_max_send(cx)
                            }))
                            .child(self.btn("send", "Send", BtnKind::Ember, true, cx, |v, cx| {
                                v.send(cx)
                            })),
                    )
                    .when_some(self.send_fee.clone(), |d, fee| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(theme::ok())
                                .child(fee),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(if self.view_only {
                                "Watching only. Paste the seed when you need to send — it is not stored unless you keep Hello on."
                            } else {
                                "From orchard. Fee is ZIP-317 from the engine, not a pad."
                            }),
                    ),
            )
    }

    fn workshop_column(
        &self,
        can_create: bool,
        can_operate: bool,
        create_label: String,
        light_toggle: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("workshop")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .when(can_create, |d| d.child(self.start_card(create_label, cx)))
            .when(self.previous_wallet_dir.is_some(), |d| {
                d.child(
                    div()
                        .p_5()
                        .bg(theme::card_bg())
                        .rounded(px(10.0))
                        .child(self.btn(
                            "reopen-previous-empty",
                            "Open previous wallet",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| v.reopen_previous_wallet(cx),
                        )),
                )
            })
            .when(can_operate, |d| {
                d.child(
                    div()
                        .p_5()
                        .bg(theme::card_bg())
                        .rounded(px(10.0))
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .child(self.btn(
                            "open-empty",
                            if self.wallet_loaded {
                                "Reload"
                            } else {
                                "Open wallet"
                            },
                            BtnKind::Ember,
                            true,
                            cx,
                            |v, cx| v.refresh(cx),
                        ))
                        .child(self.btn(
                            "log-out-unloaded",
                            "Log out / new seed",
                            BtnKind::Ghost,
                            true,
                            cx,
                            |v, cx| v.log_out_for_new_seed(cx),
                        )),
                )
            })
            .child(self.servers_card(light_toggle, cx))
    }

    fn servers_card(&self, light_toggle: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_4()
            .bg(theme::card_bg())
            .rounded(px(8.0))
            .border_1()
            .border_color(theme::line())
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child("Workshop — Zaino compact blocks, Zakura RPC"),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.labeled("Zaino", &self.light)),
                    )
                    .child(self.btn(
                        "cycle-light",
                        light_toggle,
                        BtnKind::Ghost,
                        true,
                        cx,
                        |v, cx| v.cycle_light(cx),
                    )),
            )
            .child(self.labeled("Zakura", &self.rpc))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        self.btn("probe", "Probe", BtnKind::Patina, true, cx, |v, cx| {
                            v.probe(cx)
                        }),
                    )
                    .child(self.btn(
                        "ensure-local",
                        "Ensure local Zaino",
                        BtnKind::Ghost,
                        true,
                        cx,
                        |v, cx| v.ensure_local(cx),
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(if self.light_ok == Some(false) {
                        theme::warn()
                    } else if self.light_ok == Some(true) {
                        theme::ok()
                    } else {
                        theme::muted()
                    })
                    .child(self.probe_note.clone()),
            )
    }

    fn footer(
        &self,
        light_toggle: &'static str,
        rpc_text: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let _ = (light_toggle, rpc_text);
        div()
            .px_6()
            .pb_4()
            .flex()
            .gap_3()
            .items_end()
            .child(
                div()
                    .w(px(280.0))
                    .child(self.labeled("Passphrase", &self.pass)),
            )
            .child(self.btn(
                "toggle-pass",
                if self.show_pass { "Hide" } else { "Show" },
                BtnKind::Ghost,
                true,
                cx,
                |v, cx| {
                    v.show_pass = !v.show_pass;
                    let masked = !v.show_pass;
                    v.pass.update(cx, |f, cx| f.set_masked(masked, cx));
                },
            ))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(self.bridge_note.clone()),
            )
    }
}

impl Drop for HomeView {
    fn drop(&mut self) {
        if let Some(cancel) = self.sync_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }
}

const DESKTOP_LAB_FILE: &str = "z-stack-desktop.json";

struct DesktopLab {
    network: Network,
    wallet_dir: PathBuf,
    previous_wallet_dir: Option<PathBuf>,
}

fn load_desktop_lab() -> Option<DesktopLab> {
    let raw = std::fs::read_to_string(DESKTOP_LAB_FILE).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let network = Network::parse(v.get("network")?.as_str()?)?;
    let wallet_dir = PathBuf::from(v.get("walletDir")?.as_str()?);
    if wallet_dir.as_os_str().is_empty() {
        return None;
    }
    Some(DesktopLab {
        network,
        wallet_dir,
        previous_wallet_dir: v
            .get("previousWalletDir")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
    })
}

fn persist_desktop_lab(network: Network, dir: &Path, previous: Option<&Path>) {
    let dir_s = dir.to_string_lossy().replace('\\', "/");
    let body = serde_json::json!({
        "network": network.as_str(),
        "walletDir": dir_s,
        "previousWalletDir": previous
            .filter(|path| *path != dir)
            .map(|path| path.to_string_lossy().replace('\\', "/")),
    });
    if let Ok(text) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(DESKTOP_LAB_FILE, format!("{text}\n"));
    }
}

fn wallet_network(dir: &Path) -> Option<Network> {
    NativeWallet::open(dir).ok().map(|w| w.network())
}

/// Reserve a fresh sibling directory. Never reuse an existing path: it may
/// hold wallet data even when its database cannot currently be opened.
fn reserve_wallet_profile(current: &Path, network: Network) -> anyhow::Result<PathBuf> {
    let parent = current
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for index in 2..=1000 {
        let candidate = parent.join(format!("{}-profile-{index}", network.as_str()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("all 999 profile names are occupied in {}", parent.display())
}

fn named_dir_for(network: Network) -> PathBuf {
    PathBuf::from(format!("./wallet-data-{}", network.as_str()))
}

fn dir_for_network(network: Network) -> PathBuf {
    let named = named_dir_for(network);
    if wallet_network(&named) == Some(network) {
        return named;
    }
    let legacy = PathBuf::from("./wallet-data");
    if wallet_network(&legacy) == Some(network) {
        return legacy;
    }
    named
}

fn launcher_bin() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sib = exe.with_file_name(if cfg!(windows) {
            "z-node-launcher.exe"
        } else {
            "z-node-launcher"
        });
        if sib.exists() {
            return sib;
        }
    }
    PathBuf::from(if cfg!(windows) {
        "target/debug/z-node-launcher.exe"
    } else {
        "target/debug/z-node-launcher"
    })
}

fn probe_local_bridge(bind: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(addr) = bind.parse::<std::net::SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(400)));
    let _ = stream.write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf);
    body.contains("native-bridge") || body.contains("\"ok\":true")
}

fn spawn_web_bridge(dir: PathBuf, auth: Arc<StdMutex<SeedAuth>>) -> SharedString {
    let bind = std::env::var("Z_STACK_BRIDGE").unwrap_or_else(|_| "off".into());
    if bind == "off" || bind == "0" {
        return "Web bridge off. Set Z_STACK_BRIDGE=127.0.0.1:8787 to enable (token required)."
            .into();
    }
    if probe_local_bridge(&bind) {
        return format!("Web lab already on http://{bind} — reusing, this window still works.")
            .into();
    }
    let bind_thread = bind.clone();
    let bridge = Bridge::from_shared(dir, auth);
    let token_note = bridge.token().to_string();
    let _ = std::thread::Builder::new()
        .name("z-stack-web-bridge".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("web bridge runtime: {e}");
                    return;
                }
            };
            if let Err(e) = rt.block_on(bridge.serve(&bind_thread)) {
                let msg = format!("{e:#}");
                let busy = msg.contains("10048")
                    || msg.to_ascii_lowercase().contains("already in use")
                    || msg.to_ascii_lowercase().contains("only one usage");
                if !busy {
                    eprintln!("web bridge: {msg}");
                }
            }
        });
    format!("Web lab: http://{bind}  token {token_note}  (Authorization: Bearer)").into()
}

impl Focusable for HomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.work != WorkKind::None || self.sync_live.is_some() {
            window.request_animation_frame();
        }
        let light_placeholder = LightServer::local_for_network(self.network).as_url();
        let _ = light_placeholder;
        let light_toggle = {
            let cur = LightServer::parse(&self.field_text(&self.light, cx), self.network).as_url();
            if self.network == Network::Mainnet && cur == LightServer::LOCAL_ZAINO_GRPC {
                "Use :8138 Zaino"
            } else if self.using_local_light(cx) {
                "Use public LWD"
            } else {
                "Use local Zaino"
            }
        };
        let create_label = format!("Create on {}", self.network.as_str());
        let folder_line = if let Some(disk) = self.disk_network {
            format!(
                "{} · {}{}",
                self.wallet_dir.display(),
                disk.as_str(),
                if self.wallet_loaded {
                    " · open"
                } else {
                    " · on disk"
                }
            )
        } else {
            format!(
                "{} · empty · {}",
                self.wallet_dir.display(),
                self.network.as_str()
            )
        };
        let next = self.next_step();
        let can_create = !self.has_wallet;
        let can_operate = self.has_wallet;
        let can_spend = self.wallet_loaded;
        let rpc_text = self.field_text(&self.rpc, cx);

        div()
            .id("home")
            .track_focus(&self.focus_handle)
            .key_context("Home")
            .on_action(cx.listener(Self::on_next_field))
            .on_action(cx.listener(Self::on_prev_field))
            .on_action(cx.listener(Self::on_submit_focused))
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .relative()
            .size_full()
            .bg(theme::bg_color())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(self.header(folder_line, next, cx))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h(px(0.0))
                            .px_6()
                            .pb_3()
                            .gap_4()
                            .when(can_spend, |d| {
                                d.child(self.ledger_column(cx)).child(self.forge_column(cx))
                            })
                            .when(!can_spend, |d| {
                                d.child(self.workshop_column(
                                    can_create,
                                    can_operate,
                                    create_label,
                                    light_toggle,
                                    cx,
                                ))
                            }),
                    )
                    .child(self.footer(light_toggle, rpc_text, cx)),
            )
            .when(self.shows_blocking_work(), |d| {
                d.child(self.work_overlay(cx))
            })
            .when(self.show_sync_strip(), |d| d.child(self.sync_strip(cx)))
            .when_some(self.success.clone(), |d, notice| {
                d.child(self.success_overlay(&notice, cx))
            })
            .when_some(self.work_error.clone(), |d, err| {
                d.child(self.error_overlay(err, cx))
            })
            .when(self.seed_prompt, |d| d.child(self.seed_prompt_overlay(cx)))
            .when(self.rescan_prompt == RescanPrompt::Choose, |d| {
                d.child(self.rescan_choose_overlay(cx))
            })
            .when(self.rescan_prompt == RescanPrompt::ConfirmWipe, |d| {
                d.child(self.rescan_wipe_confirm_overlay(cx))
            })
    }
}

#[cfg(test)]
mod job_tests {
    use super::unpanicked;

    #[test]
    fn a_panicking_wallet_job_becomes_an_error_instead_of_ending_the_app() {
        let error = unpanicked::<()>(|| panic!("tree conflict at 3400000")).unwrap_err();
        assert!(error.to_string().contains("tree conflict at 3400000"));
        let owned = unpanicked::<()>(|| panic!("{}", String::from("owned message"))).unwrap_err();
        assert!(owned.to_string().contains("owned message"));
        assert_eq!(unpanicked(|| Ok(7)).unwrap(), 7);
    }
}
