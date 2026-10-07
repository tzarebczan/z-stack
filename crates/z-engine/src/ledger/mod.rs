//! Ledger Zcash app (shielded signing, app 3.9.3+) as pure data.
//!
//! The engine turns a PCZT into the ordered APDUs the device needs and turns
//! the device's raw responses back into spend authorization signatures. The
//! transport (WebHID in a browser) only relays bytes, so all parsing and
//! validation stays in Rust.
//!
//! A signing exchange: stream the PCZT (the device shows the transaction and
//! the user approves), then ask for one signature per real Orchard/Ironwood
//! spend. Padding spends are already signed by the wallet.
//!
//! The APDU encoding and the version limits come from Vizor
//! (chainapsis/vizor-wallet, Apache-2.0), which ships Ledger shielded signing.

mod apdu;
mod parse;
mod serializer;

pub use apdu::{
    app_info_command, decode_app_info, decode_ufvk_responses, map_status_word,
    open_zcash_app_command, ufvk_bytes_remaining, ufvk_commands, ApduCommand, DeviceApp,
};

use orchard::ValuePool;
use pczt::roles::signer::SpendAuthSignature;
use serde::Serialize;

use self::apdu::ZCASH_CLA;
use self::parse::{parse_pczt, ParsedPczt};
use self::serializer::{packet_p1, packet_p2, serialize_pczt};

// Ledger Zcash app 3.9.3 limits. Shielded action limits apply per pool.
pub(crate) const MAX_TRANSPARENT_OUTPUTS: usize = 10;
pub(crate) const MAX_SHIELDED_ACTIONS: usize = 32;

/// Oldest app that signs the PCZTs this engine builds.
pub const MIN_SIGNING_APP_VERSION: &str = "3.9.3";
/// Oldest app that shows a memo as a hash (it reset on that path before) and
/// the oldest one new accounts are connected with.
pub const MIN_ACCOUNT_APP_VERSION: &str = "3.9.4";

/// The app name the device reports while the Zcash app is open.
pub const ZCASH_APP_NAME: &str = "Zcash";

pub(crate) const MEMO_HASH_UNSUPPORTED: &str =
    "ledger_memo_hash_unsupported: Update the Ledger Zcash app (3.9.4 or later) to send a memo that is not plain ASCII text";
const APP_OUTDATED: &str = "ledger_app_outdated";
const LEGACY_ORCHARD_RECOVERY_UNSUPPORTED: &str = "ledger_legacy_orchard_recovery_unsupported: The Ledger Zcash app cannot sign a transaction that spends Orchard funds into Ironwood yet.";

/// The account a PCZT must belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedAccount {
    pub coin_type: u32,
    pub account_index: u32,
    pub seed_fingerprint: [u8; 32],
}

/// One signature the plan asks for, in order after the PCZT packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureSlot {
    /// "orchard" or "ironwood".
    pub pool: &'static str,
    pub action_index: usize,
}

/// The whole exchange for one transaction: send `commands` in order and hand
/// every raw response (status word included) back to [`signatures_from_responses`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SigningPlan {
    pub commands: Vec<ApduCommand>,
    /// Index into `commands` of the packet whose response waits for the
    /// user's approval on the device (for a "review on your Ledger" prompt).
    pub review_index: usize,
    pub signatures: Vec<SignatureSlot>,
}

/// A Ledger account's viewing key and the metadata that identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerAccount {
    pub ufvk: String,
    /// 64 hex. The app does not export the ZIP-32 seed fingerprint, so this is
    /// a domain-separated hash of the approved viewing key (as Vizor does).
    pub seed_fingerprint: String,
    pub account_index: u32,
}

/// Stable, non-secret identifier for a Ledger account (stands in for the
/// ZIP-32 seed fingerprint in PCZT derivations).
pub fn account_fingerprint(ufvk: &str, account_index: u32) -> [u8; 32] {
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .personal(b"zstack_LedgerAFP")
        .to_state()
        .update(&account_index.to_be_bytes())
        .update(ufvk.as_bytes())
        .finalize();
    hash.as_bytes().try_into().expect("32-byte hash")
}

/// Decodes a UFVK export and checks the key parses for `network`.
pub fn account_from_ufvk_responses(
    responses: &[Vec<u8>],
    account_index: u32,
    network: crate::Network,
) -> Result<LedgerAccount, String> {
    let ufvk = decode_ufvk_responses(responses)?;
    zcash_keys::keys::UnifiedFullViewingKey::decode(&network, &ufvk).map_err(|e| {
        format!("Ledger returned a viewing key for another network or an invalid one: {e}")
    })?;
    let fp = account_fingerprint(&ufvk, account_index);
    Ok(LedgerAccount {
        seed_fingerprint: fp.iter().map(|b| format!("{b:02x}")).collect(),
        ufvk,
        account_index,
    })
}

/// `version >= minimum` for `major.minor.patch` versions; a pre-release
/// (`3.9.4-rc1`) counts as older than its release, and anything unparsable
/// fails closed.
pub fn app_version_at_least(version: &str, minimum: &str) -> bool {
    fn parts(v: &str) -> Option<([u64; 3], bool)> {
        let v = v.trim();
        let (core, pre) = match v.split_once(['-', '+']) {
            Some((core, rest)) => (core, v.as_bytes()[core.len()] == b'-' && !rest.is_empty()),
            None => (v, false),
        };
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let out = [it.next()??, it.next()??, it.next()??];
        it.next().is_none().then_some((out, pre))
    }
    match (parts(version), parts(minimum)) {
        (Some((v, v_pre)), Some((m, _))) => v > m || (v == m && !v_pre),
        _ => false,
    }
}

/// Everything the device needs to sign `pczt`, after checking it belongs to
/// `expected` and that the connected app (`app_version`) can sign it.
pub fn signing_plan(
    pczt: &[u8],
    expected: &ExpectedAccount,
    app_version: &str,
) -> Result<SigningPlan, String> {
    if !app_version_at_least(app_version, MIN_SIGNING_APP_VERSION) {
        return Err(format!(
            "{APP_OUTDATED}: Update the Ledger Zcash app to {MIN_SIGNING_APP_VERSION} or later (found {app_version})"
        ));
    }
    let parsed = parse_pczt(pczt)?;
    validate_account(&parsed, expected)?;
    validate_release_support(&parsed)?;
    let memo_hash_supported = app_version_at_least(app_version, MIN_ACCOUNT_APP_VERSION);
    build_plan(&parsed, memo_hash_supported)
}

/// Checks the device's raw responses to a plan for `pczt` and returns the
/// spend authorization signatures. Apply them with the PCZT Signer, which
/// verifies each against its action.
pub fn signatures_from_responses(
    pczt: &[u8],
    expected: &ExpectedAccount,
    responses: &[Vec<u8>],
) -> Result<Vec<SpendAuthSignature>, String> {
    let parsed = parse_pczt(pczt)?;
    validate_account(&parsed, expected)?;
    let plan = build_plan(&parsed, true)?;
    // Status words first: a transport stops at the device's first error, and
    // that error (a rejection, a locked device) is the one to report.
    let payloads = responses
        .iter()
        .map(|r| apdu::decode_raw_response(r))
        .collect::<Result<Vec<_>, _>>()?;
    if responses.len() != plan.commands.len() {
        return Err(format!(
            "Ledger returned {} APDU response(s); expected {}",
            responses.len(),
            plan.commands.len()
        ));
    }
    let packet_count = plan.commands.len() - plan.signatures.len();
    if let Some(index) = payloads[..packet_count].iter().position(|p| !p.is_empty()) {
        return Err(format!(
            "Ledger PCZT APDU {} returned unexpected response data",
            index + 1
        ));
    }
    plan.signatures
        .iter()
        .zip(&payloads[packet_count..])
        .map(|(slot, payload)| {
            let signature: [u8; 64] = payload.as_slice().try_into().map_err(|_| {
                format!(
                    "Ledger returned a {}-byte spend authorization signature; expected 64",
                    payload.len()
                )
            })?;
            if signature.iter().all(|byte| *byte == 0) {
                return Err("Ledger returned an all-zero spend authorization signature".into());
            }
            let pool = if slot.pool == "ironwood" {
                ValuePool::Ironwood
            } else {
                ValuePool::Orchard
            };
            Ok(SpendAuthSignature::from_parts(
                pool,
                slot.action_index,
                signature,
            ))
        })
        .collect()
}

fn build_plan(parsed: &ParsedPczt, memo_hash_supported: bool) -> Result<SigningPlan, String> {
    let mut commands = Vec::new();
    let mut review_index = 0;
    for command in serialize_pczt(parsed, memo_hash_supported)? {
        let total = command.packets.len();
        if total == 0 {
            return Err("Ledger PCZT command has no packets".into());
        }
        for (index, data) in command.packets.into_iter().enumerate() {
            if command.finishes_pczt && index + 1 == total {
                review_index = commands.len();
            }
            commands.push(ApduCommand {
                cla: ZCASH_CLA,
                ins: command.instruction,
                p1: packet_p1(index, total),
                p2: packet_p2(index, total, command.finishes_pczt),
                data,
            });
        }
    }

    // Only real spends: padding spends (value 0) carry the wallet's signature.
    let mut signatures = Vec::new();
    if let Some(bundle) = &parsed.orchard_bundle {
        signatures.extend(
            bundle
                .actions
                .iter()
                .enumerate()
                .filter(|(_, a)| a.spend_value != 0)
                .map(|(action_index, _)| SignatureSlot {
                    pool: "orchard",
                    action_index,
                }),
        );
    }
    if let Some(bundle) = &parsed.ironwood_bundle {
        signatures.extend(
            bundle
                .actions
                .iter()
                .enumerate()
                .filter(|(_, a)| a.action.spend_value != 0)
                .map(|(action_index, _)| SignatureSlot {
                    pool: "ironwood",
                    action_index,
                }),
        );
    }
    if signatures.is_empty() {
        return Err("PCZT has no real Orchard or Ironwood spends for Ledger to sign".into());
    }
    for slot in &signatures {
        commands.push(ApduCommand {
            cla: ZCASH_CLA,
            ins: if slot.pool == "ironwood" { 0x59 } else { 0x57 },
            p1: 0,
            p2: u8::try_from(slot.action_index)
                .map_err(|_| "Ledger signing index exceeds the APDU range")?,
            data: Vec::new(),
        });
    }
    Ok(SigningPlan {
        commands,
        review_index,
        signatures,
    })
}

fn validate_account(parsed: &ParsedPczt, expected: &ExpectedAccount) -> Result<(), String> {
    if parsed.global.coin_type != expected.coin_type {
        return Err(format!(
            "Ledger PCZT coin type {} does not match expected coin type {}",
            parsed.global.coin_type, expected.coin_type
        ));
    }
    const HARDENED: u32 = 0x8000_0000;
    let path = [
        HARDENED | 32,
        HARDENED | expected.coin_type,
        HARDENED | expected.account_index,
    ];
    let actions = parsed
        .orchard_bundle
        .iter()
        .flat_map(|b| b.actions.iter().map(|a| ("Orchard", a)))
        .chain(
            parsed
                .ironwood_bundle
                .iter()
                .flat_map(|b| b.actions.iter().map(|a| ("Ironwood", &a.action))),
        );
    for (index, (pool, action)) in actions.enumerate() {
        if action.signing_path != path {
            return Err(format!(
                "Ledger {pool} action {index} derivation path does not belong to account {}",
                expected.account_index
            ));
        }
        if action.seed_fingerprint != expected.seed_fingerprint {
            return Err(format!(
                "Ledger {pool} action {index} seed fingerprint does not match this account"
            ));
        }
    }
    Ok(())
}

/// The app mis-signs a transaction that spends Orchard notes into Ironwood
/// outputs (known defect, tracked by Vizor), so refuse that shape.
fn validate_release_support(parsed: &ParsedPczt) -> Result<(), String> {
    let has_orchard_spend = parsed
        .orchard_bundle
        .as_ref()
        .is_some_and(|b| b.actions.iter().any(|a| a.spend_value != 0));
    let has_ironwood_output = parsed
        .ironwood_bundle
        .as_ref()
        .is_some_and(|b| b.actions.iter().any(|a| a.action.value != 0));
    if has_orchard_spend && has_ironwood_output {
        Err(LEGACY_ORCHARD_RECOVERY_UNSUPPORTED.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_versions_compare_numerically_and_fail_closed() {
        assert!(app_version_at_least("3.9.4", "3.9.4"));
        assert!(app_version_at_least("3.10.0", "3.9.4"));
        assert!(app_version_at_least("4.0.0", "3.9.3"));
        assert!(!app_version_at_least("3.9.3", "3.9.4"));
        assert!(!app_version_at_least("3.9.4-rc1", "3.9.4"));
        assert!(app_version_at_least("3.9.5-rc1", "3.9.4"));
        assert!(!app_version_at_least("", "3.9.3"));
        assert!(!app_version_at_least("3.9", "3.9.3"));
        assert!(!app_version_at_least("3.9.x", "3.9.3"));
    }

    #[test]
    fn account_fingerprints_are_per_key_and_account() {
        let a = account_fingerprint("uview1abc", 0);
        assert_ne!(a, account_fingerprint("uview1abc", 1));
        assert_ne!(a, account_fingerprint("uview1abd", 0));
        assert_eq!(a, account_fingerprint("uview1abc", 0));
    }
}
