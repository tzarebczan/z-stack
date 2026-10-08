//! Birthday height from a civil date (YYYY-MM-DD) or the reverse.
//!
//! Target spacing changes at Blossom and NU7. We estimate from *tip* and the system clock
//! so a restore dated "first used this wallet" lands near the right height
//! without a block-index lookup.

use crate::error::{EngineError, Result};
use crate::{Network, SyncStage};
use std::time::{SystemTime, UNIX_EPOCH};
use zcash_protocol::consensus::{NetworkUpgrade, Parameters};

/// Post-Blossom, pre-NU7 target spacing (seconds).
pub const BLOCK_SECONDS: u32 = 75;

/// Extra blocks subtracted when birthday is an approximate date, not an exact height.
pub const DATE_SAFETY_BLOCKS: u32 = 200;

/// Session catch-up this small is “almost at tip”, not a full-chain sync.
pub const NEAR_TIP_BLOCKS: u32 = 512;

/// Compact-block fetch / scan has not advanced for this long → consider a stall.
pub const SYNC_STALL_SECS: u32 = 15;

/// Remaining this small is catch-up, not a dead light server.
/// A 13-block GetBlockRange can sit still for [`SYNC_STALL_SECS`] while Zaino is
/// merely slow; that must not become “check Zaino / the light URL”.
pub const STALL_QUIET_REMAINING: u32 = 32;

/// Last few blocks (verify window / 3-conf + slack). A 6-behind wallet is
/// usable — do not leave a 90% catch-up strip up as if the chain is still far.
pub const QUIET_BEHIND_BLOCKS: u32 = 10;

/// Whether to surface the scary “check Zaino / light URL” stall copy.
///
/// Near tip (`span` ≤ [`NEAR_TIP_BLOCKS`] or `remaining` ≤ [`STALL_QUIET_REMAINING`])
/// stays CatchingUp even if heights freeze. Far from tip: only scream when heights
/// are frozen **and** a light probe failed (`light_reachable == false`).
pub fn light_stall_warning(span: u32, remaining: u32, frozen: bool, light_reachable: bool) -> bool {
    frozen && remaining > STALL_QUIET_REMAINING && span > NEAR_TIP_BLOCKS && !light_reachable
}

/// Historic catch-up overlay stage. Sticky so prefetch does not flip
/// Downloading ↔ Scanning every chunk.
///
/// Once `scanned` has left origin (`scan_moved`), stay **Scanning** (or
/// **CatchingUp** near tip). Download/decrypt/persist overlap; the headline
/// must not bounce just because a 1000-block persist paused the 2s rate window.
pub fn historic_overlay_stage(
    scan_moved: bool,
    _download_ahead: bool,
    remaining: u32,
    scream: bool,
) -> SyncStage {
    if scream {
        return SyncStage::Downloading;
    }
    if !scan_moved {
        return SyncStage::Downloading;
    }
    if remaining <= NEAR_TIP_BLOCKS {
        return SyncStage::CatchingUp;
    }
    SyncStage::Scanning
}

/// One overlay checklist row. Several can be **Active** at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayCheck {
    Done,
    Active,
    Pending,
}

/// Concurrent rows: connect, download, trial-decrypt, persist, memos.
pub fn historic_overlay_checks(
    connected: bool,
    scan_moved: bool,
    download_ahead: bool,
    remaining: u32,
    enhancing: bool,
) -> [OverlayCheck; 5] {
    if enhancing {
        return [
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Active,
        ];
    }
    if !connected {
        return [
            OverlayCheck::Active,
            OverlayCheck::Pending,
            OverlayCheck::Pending,
            OverlayCheck::Pending,
            OverlayCheck::Pending,
        ];
    }
    if remaining == 0 && scan_moved {
        return [
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Done,
            OverlayCheck::Pending,
        ];
    }
    let working = download_ahead || scan_moved;
    [
        OverlayCheck::Done,
        if remaining > 0 {
            OverlayCheck::Active
        } else {
            OverlayCheck::Done
        },
        if working {
            OverlayCheck::Active
        } else {
            OverlayCheck::Pending
        },
        if working {
            OverlayCheck::Active
        } else {
            OverlayCheck::Pending
        },
        OverlayCheck::Pending,
    ]
}

/// Catch-up bar from *this session’s* origin→tip (not birthday→tip).
///
/// Near tip (span ≤ [`NEAR_TIP_BLOCKS`]) floors at 90% so 54 blocks behind
/// never looks like 4% of a full scan. Far from tip — birthday restore **or**
/// a 108k island→tip gap fill — is 0% until download or scan leaves origin,
/// then download 0–36%, scan 0–50%. Do not floor a huge gap at 90%.
pub fn catch_up_percent(origin: u32, scanned: u32, downloaded: u32, tip: u32) -> f32 {
    catch_up_percent_from(origin, scanned, downloaded, tip, origin)
}

/// Like [`catch_up_percent`]. `birthday` is kept so callers can distinguish
/// gap-fill from a birthday restore; only span size changes the bar.
pub fn catch_up_percent_from(
    origin: u32,
    scanned: u32,
    downloaded: u32,
    tip: u32,
    _birthday: u32,
) -> f32 {
    let span = tip.saturating_sub(origin).max(1);
    let sc_done = scanned.max(origin).saturating_sub(origin).min(span);
    let dl_done = downloaded
        .max(scanned)
        .max(origin)
        .saturating_sub(origin)
        .min(span);
    let pct = if span <= NEAR_TIP_BLOCKS {
        90.0 + 9.0 * (sc_done as f32 / span as f32)
    } else if dl_done == 0 && sc_done == 0 {
        0.0
    } else {
        (36.0 * (dl_done as f32 / span as f32) + 50.0 * (sc_done as f32 / span as f32)).min(99.0)
    };
    pct.min(99.0)
}

/// Full historic overlay (download / trial-decrypt / persist) vs compact “N behind”.
///
/// Hide only when remaining is a near-tip follow-on (`≤ `[`STALL_QUIET_REMAINING`]).
/// Unknown remaining (sync just started) stays visible — a 108k gap must not
/// collapse to the header bar before the first tick.
pub fn historic_overlay_visible(remaining: Option<u32>) -> bool {
    match remaining {
        None => true,
        Some(0) => false,
        Some(left) => left > STALL_QUIET_REMAINING,
    }
}

/// UI guard: if the chain is within [`NEAR_TIP_BLOCKS`], never show a tiny bar.
pub fn display_catch_up_percent(percent: f32, scanned: u32, tip: u32) -> f32 {
    let left = tip.saturating_sub(scanned);
    if tip > 0 && left > 0 && left <= NEAR_TIP_BLOCKS {
        percent.max(90.0).min(99.0)
    } else {
        percent.clamp(0.0, 100.0)
    }
}

/// Rough scan rates for ETA copy (compact blocks / second).
pub fn scan_rate_range(local_light: bool) -> (u32, u32) {
    if local_light {
        (400, 1_800)
    } else {
        (20, 80)
    }
}

/// Live overlay clock. None until scan actually moved (`sc_done > 1`).
/// Gap fill and far-from-tip catch-up stay on “N behind” — a 54k hole at 2 blk/s
/// used to print multi-day ETAs that vanished on the next tick.
pub fn live_scan_eta_secs(
    sc_done: u32,
    remaining: u32,
    origin: u32,
    birthday: u32,
    scan_bps: f32,
) -> Option<u32> {
    if sc_done <= 1 || remaining == 0 || scan_bps <= 1.0 {
        return None;
    }
    if origin > birthday || remaining > NEAR_TIP_BLOCKS {
        return None;
    }
    Some((remaining as f32 / scan_bps).round() as u32).filter(|s| *s >= 1)
}

#[derive(Debug, Clone, Copy)]
pub struct SyncEta {
    pub blocks: u32,
    pub seconds_fast: u32,
    pub seconds_slow: u32,
}

impl SyncEta {
    pub fn from_span(birthday: u32, tip: u32, local_light: bool) -> Self {
        let blocks = tip.saturating_sub(birthday).max(1);
        let (lo, hi) = scan_rate_range(local_light);
        Self {
            blocks,
            seconds_fast: (blocks / hi).max(1),
            seconds_slow: (blocks / lo).max(1),
        }
    }

    pub fn human(&self) -> String {
        format!(
            "{} blocks · about {}–{}",
            self.blocks,
            fmt_secs(self.seconds_fast),
            fmt_secs(self.seconds_slow)
        )
    }
}

pub fn fmt_secs(secs: u32) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", (secs + 30) / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600 + 30) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

/// Consensus spacing at a height. Mainnet NU7 remains unscheduled upstream.
pub fn block_spacing_seconds(network: Network, height: u32) -> u32 {
    let active = |nu| {
        network
            .activation_height(nu)
            .is_some_and(|h| height >= u32::from(h))
    };
    if active(NetworkUpgrade::Nu7) {
        25
    } else if active(NetworkUpgrade::Blossom) {
        75
    } else {
        150
    }
}

fn spacing_eras(network: Network) -> Vec<(u32, u32)> {
    let mut eras = vec![(1, 150)];
    for (nu, seconds) in [(NetworkUpgrade::Blossom, 75), (NetworkUpgrade::Nu7, 25)] {
        if let Some(h) = network.activation_height(nu) {
            eras.push((u32::from(h).max(1), seconds));
        }
    }
    eras.sort_by_key(|(h, _)| *h);
    eras
}

fn height_before_seconds(network: Network, tip: u32, mut seconds: u64) -> u32 {
    let mut height = tip.max(1);
    for (start, spacing) in spacing_eras(network).into_iter().rev() {
        if height < start {
            continue;
        }
        let blocks = u64::from(height - start + 1);
        let span = blocks * u64::from(spacing);
        if seconds < span {
            return height
                .saturating_sub((seconds / u64::from(spacing)) as u32)
                .max(1);
        }
        seconds -= span;
        height = start.saturating_sub(1);
    }
    1
}

fn seconds_between_heights(network: Network, height: u32, tip: u32) -> u64 {
    let mut cursor = tip;
    let mut seconds = 0;
    for (start, spacing) in spacing_eras(network).into_iter().rev() {
        if cursor < start || cursor <= height {
            continue;
        }
        let lower = height.max(start.saturating_sub(1));
        seconds += u64::from(cursor - lower) * u64::from(spacing);
        cursor = lower;
    }
    seconds
}

/// Mainnet estimate. Use [`height_from_date_for_network`] for other networks.
pub fn height_from_date(ymd: &str, tip_height: u32) -> Result<u32> {
    height_from_date_for_network(ymd, tip_height, Network::Mainnet)
}

/// Approximate date-to-height conversion, using every intervening spacing era.
/// A live tip and exact birthday height are preferable to this wall-clock estimate.
pub fn height_from_date_for_network(ymd: &str, tip_height: u32, network: Network) -> Result<u32> {
    let date_unix = ymd_to_unix(ymd.trim())?;
    let seconds = now_unix().saturating_sub(date_unix).max(0) as u64;
    Ok(height_before_seconds(network, tip_height, seconds))
}

pub fn date_from_height(height: u32, tip_height: u32) -> String {
    date_from_height_for_network(height, tip_height, Network::Mainnet)
}

pub fn date_from_height_for_network(height: u32, tip_height: u32, network: Network) -> String {
    let seconds = seconds_between_heights(network, height, tip_height);
    unix_to_ymd(now_unix().saturating_sub(seconds as i64))
}

pub fn parse_birthday_input(raw: &str, tip_height: u32) -> Result<u32> {
    parse_birthday_input_for_network(raw, tip_height, Network::Mainnet)
}

pub fn parse_birthday_input_for_network(
    raw: &str,
    tip_height: u32,
    network: Network,
) -> Result<u32> {
    let s = raw.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("auto") {
        return Ok(tip_height.saturating_sub(100).max(1));
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s
            .parse::<u32>()
            .map_err(|_| EngineError::Message("birthday height is not a number".into()))
            .map(|h| h.max(1));
    }
    let h = height_from_date_for_network(s, tip_height, network)?;
    // Keep the same time margin across spacing changes (200 × 75 seconds).
    Ok(height_before_seconds(
        network,
        h,
        u64::from(DATE_SAFETY_BLOCKS * BLOCK_SECONDS),
    ))
}

/// Fallback tip when Probe has not run yet (used for ETA / date↔height).
pub fn typical_tip(network: Network) -> u32 {
    match network {
        Network::Mainnet => 3_470_000,
        Network::Testnet => 4_470_000,
        Network::Regtest => 200,
    }
}

/// Civil date `days` before now (YYYY-MM-DD).
pub fn ymd_days_ago(days: u32) -> String {
    let unix = now_unix().saturating_sub(i64::from(days) * 86_400);
    unix_to_ymd(unix)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ymd_to_unix(s: &str) -> Result<i64> {
    let mut parts = s.split('-');
    let y: i32 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| EngineError::Message("date must be YYYY-MM-DD".into()))?;
    let m: u32 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| EngineError::Message("date must be YYYY-MM-DD".into()))?;
    let d: u32 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| EngineError::Message("date must be YYYY-MM-DD".into()))?;
    if parts.next().is_some() || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(EngineError::Message("date must be YYYY-MM-DD".into()));
    }
    let _ = Network::Mainnet;
    Ok(days_from_civil(y, m as i32, d as i32) * 86400)
}

fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn unix_to_ymd(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant civil_from_days / days_from_civil (Unix epoch).
fn days_from_civil(mut y: i32, m: i32, d: i32) -> i64 {
    y -= i32::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + i64::from(doy);
    i64::from(era) * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i32::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nu7_spacing_and_boundary_estimates() {
        let activation = 4_465_026;
        assert_eq!(block_spacing_seconds(Network::Testnet, activation - 1), 75);
        assert_eq!(block_spacing_seconds(Network::Testnet, activation), 25);
        assert_eq!(block_spacing_seconds(Network::Mainnet, u32::MAX), 75);
        assert_eq!(
            seconds_between_heights(Network::Testnet, activation - 2, activation + 2),
            150
        );
        assert_eq!(
            height_before_seconds(Network::Testnet, activation + 2, 150),
            activation - 2
        );
        assert_eq!(
            height_before_seconds(Network::Testnet, activation + 2, 74),
            activation
        );
        assert_eq!(
            height_before_seconds(Network::Testnet, activation + 2, 75),
            activation - 1
        );
        assert_eq!(
            height_before_seconds(Network::Testnet, activation + 1_000, 15_000),
            activation + 400
        );
        assert_eq!(height_before_seconds(Network::Testnet, 10, u64::MAX), 1);
        assert_eq!(seconds_between_heights(Network::Testnet, 100, 90), 0);
    }

    #[test]
    fn ymd_roundtrip() {
        let unix = ymd_to_unix("2024-06-15").unwrap();
        assert_eq!(unix_to_ymd(unix), "2024-06-15");
    }

    #[test]
    fn height_in_the_past_is_below_tip() {
        let h = height_from_date("2020-01-01", 3_000_000).unwrap();
        assert!(h < 3_000_000);
        assert!(h > 1);
    }

    #[test]
    fn parse_digits_or_date() {
        assert_eq!(parse_birthday_input("1687104", 3_000_000).unwrap(), 1687104);
        let h = parse_birthday_input("2022-05-31", 3_000_000).unwrap();
        assert!(h < 3_000_000);
    }

    #[test]
    fn rejects_impossible_civil_dates() {
        assert!(ymd_to_unix("2024-02-31").is_err());
        assert!(ymd_to_unix("2023-02-29").is_err());
        assert!(ymd_to_unix("2024-04-31").is_err());
        assert!(ymd_to_unix("2024-02-29").is_ok());
        assert!(parse_birthday_input("2024-02-31", 3_000_000).is_err());
    }

    #[test]
    fn date_restore_is_below_exact_conversion() {
        let exact = height_from_date("2022-05-31", 3_000_000).unwrap();
        let parsed = parse_birthday_input("2022-05-31", 3_000_000).unwrap();
        assert_eq!(parsed, exact.saturating_sub(DATE_SAFETY_BLOCKS).max(1));
    }

    #[test]
    fn days_ago_is_iso_date() {
        let s = ymd_days_ago(0);
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        let earlier = ymd_days_ago(400);
        assert!(earlier < s);
    }

    #[test]
    fn near_tip_catch_up_is_not_four_percent() {
        let origin = 3_472_263;
        let tip = 3_472_317;
        let pct = catch_up_percent(origin, origin, origin, tip);
        assert!(
            pct >= 90.0,
            "54 behind must not look like a 4% full-chain bar, got {pct}"
        );
        assert_eq!(display_catch_up_percent(4.0, origin, tip), 90.0);
        let done = catch_up_percent(origin, tip, tip, tip);
        assert!(done >= 98.0);
    }

    #[test]
    fn far_from_tip_idle_is_zero_not_connecting_floor() {
        let pct = catch_up_percent(1, 1, 1, 3_000_000);
        assert!(
            (pct - 0.0).abs() < 0.01,
            "birthday-scale idle must not fake 4%, got {pct}"
        );
    }

    #[test]
    fn thirteen_behind_is_not_a_dead_zaino() {
        assert!(!light_stall_warning(13, 13, true, false));
        assert!(!light_stall_warning(54, 13, true, false));
        assert!(!light_stall_warning(10_000, 13, true, false));
        assert!(!light_stall_warning(10_000, 100, true, true));
        assert!(!light_stall_warning(10_000, 100, false, false));
        assert!(light_stall_warning(10_000, 100, true, false));
        assert!(!light_stall_warning(NEAR_TIP_BLOCKS, 100, true, false));
        assert!(!light_stall_warning(
            NEAR_TIP_BLOCKS + 1,
            STALL_QUIET_REMAINING,
            true,
            false
        ));
    }

    #[test]
    fn six_behind_is_quiet() {
        assert!(QUIET_BEHIND_BLOCKS >= 6);
        assert!(QUIET_BEHIND_BLOCKS < STALL_QUIET_REMAINING);
    }

    #[test]
    fn live_eta_waits_for_movement_and_skips_gap_fill() {
        assert_eq!(
            live_scan_eta_secs(0, 54_749, 3_418_128, 3_335_466, 400.0),
            None
        );
        assert_eq!(
            live_scan_eta_secs(1, 54_749, 3_418_128, 3_335_466, 400.0),
            None
        );
        assert_eq!(
            live_scan_eta_secs(120, 54_749, 3_418_128, 3_335_466, 400.0),
            None,
            "54k gap fill must not print a clock"
        );
        assert_eq!(live_scan_eta_secs(10, 40, 3_472_900, 3_472_900, 0.5), None);
        assert_eq!(
            live_scan_eta_secs(10, 40, 3_472_900, 3_472_900, 10.0),
            Some(4)
        );
    }

    #[test]
    fn island_gap_fill_is_honest_session_percent() {
        let birthday = 3_335_466;
        let origin = 3_418_128;
        let scanned = 3_418_238;
        let tip = 3_472_935;
        let idle = catch_up_percent_from(origin, origin, origin, tip, birthday);
        assert!(
            idle < 0.5,
            "54k island→tip must not open at the 90% near-tip floor, got {idle}"
        );
        let pct = catch_up_percent_from(origin, scanned, origin, tip, birthday);
        assert!(
            pct > idle && pct < 10.0,
            "54k gap after 110 scanned is session percent, got {pct}"
        );
        let from_birthday = catch_up_percent_from(birthday, birthday, birthday, tip, birthday);
        assert!(
            (from_birthday - 0.0).abs() < 0.01,
            "true birthday restore must show 0% until heights move, got {from_birthday}"
        );
        let first_height = catch_up_percent_from(birthday, birthday, birthday + 1, tip, birthday);
        assert!(
            first_height > from_birthday,
            "one downloaded height above birthday must leave 0%, got {first_height}"
        );
        let first_batch =
            catch_up_percent_from(birthday, birthday + 4000, birthday + 8000, tip, birthday);
        assert!(
            first_batch > 0.5 && first_batch < 10.0,
            "birthday catch-up must move off 0% once this-session heights move, got {first_batch}"
        );
        assert_eq!(
            live_scan_eta_secs(4000, tip - birthday - 4000, birthday, birthday, 400.0),
            None,
            "far-from-tip birthday restore must not print a clock ETA"
        );
    }

    #[test]
    fn overlay_hides_only_near_tip_remainder() {
        assert!(historic_overlay_visible(None));
        assert!(historic_overlay_visible(Some(108_222)));
        assert!(historic_overlay_visible(Some(513)));
        assert!(historic_overlay_visible(Some(33)));
        assert!(!historic_overlay_visible(Some(STALL_QUIET_REMAINING)));
        assert!(!historic_overlay_visible(Some(QUIET_BEHIND_BLOCKS)));
        assert!(!historic_overlay_visible(Some(0)));
    }

    #[test]
    fn one_oh_eight_k_behind_is_not_ninety_percent() {
        let birthday = 3_335_466;
        let origin = 3_371_443;
        let tip = 3_479_665;
        let pct = catch_up_percent_from(origin, origin, origin, tip, birthday);
        assert!(
            pct < 0.5,
            "108k remaining must not paint a 90% bar, got {pct}"
        );
        assert!(
            (display_catch_up_percent(pct, origin, tip) - pct).abs() < 0.01,
            "display guard must not floor a 108k gap to 90%"
        );
    }

    #[test]
    fn overlay_stays_scanning_when_prefetch_is_ahead() {
        let left = 100_000;
        assert_eq!(
            historic_overlay_stage(false, false, left, false),
            crate::SyncStage::Downloading
        );
        assert_eq!(
            historic_overlay_stage(false, true, left, false),
            crate::SyncStage::Downloading
        );
        assert_eq!(
            historic_overlay_stage(true, true, left, false),
            crate::SyncStage::Scanning,
            "apply bottleneck (down ahead) must stay Scanning"
        );
        assert_eq!(
            historic_overlay_stage(true, true, left, false),
            historic_overlay_stage(true, true, left, false),
            "must not flip every 1000 just because prefetch is ahead"
        );
        assert_eq!(
            historic_overlay_stage(true, false, left, false),
            crate::SyncStage::Scanning,
            "once scan left origin, stay Scanning even if a persist pause catches download"
        );
        assert_eq!(
            historic_overlay_checks(true, true, true, left, false),
            [
                OverlayCheck::Done,
                OverlayCheck::Active,
                OverlayCheck::Active,
                OverlayCheck::Active,
                OverlayCheck::Pending,
            ],
            "download + decrypt + persist must all be active during historic catch-up"
        );
        assert_eq!(
            historic_overlay_stage(true, true, 40, false),
            crate::SyncStage::CatchingUp
        );
        assert_eq!(
            historic_overlay_stage(true, true, left, true),
            crate::SyncStage::Downloading
        );
    }
}
