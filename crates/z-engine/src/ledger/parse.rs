// Copyright 2026 Vizor contributors.
// Modifications Copyright 2026 tzarebczan. Licensed under Apache-2.0.
//! Converts a PCZT into the compact fields the Ledger Zcash app's PCZT APDUs take.
//!
//! Adapted from Vizor (chainapsis/vizor-wallet, Apache-2.0,
//! `rust/src/wallet/ledger/parse.rs`). Changes: transparent inputs and
//! transparent outputs with a BIP-32 derivation are refused (the web wallet
//! neither spends transparent funds from a device nor sends change there).

use ff::PrimeField;
use orchard::{
    bundle::BundleVersion,
    note::{NoteVersion, Rho},
    note_encryption::{IronwoodDomain, OrchardDomain},
    Note, ValuePool,
};
use pczt::{
    roles::verifier::{OrchardError, TransparentError, Verifier},
    Pczt,
};
use zcash_note_encryption::{try_output_recovery_with_pkd_esk, Domain};
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_protocol::consensus::BranchId;
use zcash_script::script::Evaluable;

const V6_TX_VERSION: u32 = 6;

#[derive(Debug, Clone)]
pub(super) struct Global {
    pub tx_version: u32,
    pub version_group_id: u32,
    pub consensus_branch_id: u32,
    pub fallback_lock_time: Option<u32>,
    pub expiry_height: u32,
    pub coin_type: u32,
    pub tx_modifiable: u8,
}

#[derive(Debug, Clone)]
pub(super) struct TransparentOutput {
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(super) struct ShieldedAction {
    pub cv_net: [u8; 32],
    pub nullifier: [u8; 32],
    pub rk: [u8; 32],
    pub spend_recipient: [u8; 43],
    pub spend_value: u64,
    pub spend_rho: [u8; 32],
    pub spend_rseed: [u8; 32],
    pub alpha: [u8; 32],
    pub signing_path: Vec<u32>,
    pub seed_fingerprint: [u8; 32],
    pub cmx: [u8; 32],
    pub ephemeral_key: [u8; 32],
    pub enc_ciphertext: Vec<u8>,
    pub out_ciphertext: Vec<u8>,
    pub recipient: [u8; 43],
    pub value: u64,
    pub rseed: [u8; 32],
    pub rcv: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShieldedDerivation {
    signing_path: Vec<u32>,
    seed_fingerprint: [u8; 32],
}

#[derive(Debug, Clone)]
pub(super) struct ShieldedBundle {
    pub actions: Vec<ShieldedAction>,
    pub flags: u8,
    pub value_balance: i128,
    pub anchor: [u8; 32],
}

#[derive(Debug, Clone)]
pub(super) struct IronwoodAction {
    pub action: ShieldedAction,
    pub note_plaintext_version: u8,
}

#[derive(Debug, Clone)]
pub(super) struct IronwoodBundle {
    pub actions: Vec<IronwoodAction>,
    pub flags: u8,
    pub value_balance: i128,
    pub anchor: [u8; 32],
}

#[derive(Debug, Clone)]
pub(super) struct ParsedPczt {
    pub global: Global,
    pub transparent_outputs: Vec<TransparentOutput>,
    pub orchard_bundle: Option<ShieldedBundle>,
    pub ironwood_bundle: Option<IronwoodBundle>,
    /// Whether any output memo would take the device's memo-hash render path
    /// (apps before 3.9.4 reset on it). The caller that knows the app version decides.
    pub memo_reaches_hash_path: bool,
}

pub(super) fn parse_pczt(bytes: &[u8]) -> Result<ParsedPczt, String> {
    let pczt = Pczt::parse(bytes).map_err(|e| format!("PCZT parse failed: {e:?}"))?;
    if !pczt.sapling().spends().is_empty() || !pczt.sapling().outputs().is_empty() {
        return Err("Ledger signing does not support Sapling spends or outputs".into());
    }
    if !pczt.transparent().inputs().is_empty() {
        return Err("Ledger signing of transparent inputs is not supported yet".into());
    }

    let global = parse_global(&pczt)?;
    let branch = BranchId::try_from(global.consensus_branch_id).map_err(|_| {
        format!(
            "Unrecognized consensus branch id {:#010x}",
            global.consensus_branch_id
        )
    })?;
    let shielded_derivation =
        fallback_shielded_derivation(&pczt, global.tx_version >= V6_TX_VERSION)?;

    let mut transparent_outputs = Vec::new();
    let mut orchard_bundle = None;
    let mut ironwood_bundle = None;
    let mut memo_reaches_hash_path = false;

    let verifier = Verifier::new(pczt)
        .with_transparent::<String, _>(|bundle| {
            for output in bundle.outputs() {
                if !output.bip32_derivation().is_empty() {
                    return Err(TransparentError::Custom(
                        "Ledger signing of a transparent output with a key derivation is not supported".into(),
                    ));
                }
                transparent_outputs.push(TransparentOutput {
                    value: output.value().into_u64(),
                    script_pubkey: output.script_pubkey().to_bytes(),
                });
            }
            Ok(())
        })
        .map_err(map_transparent_error)?;

    let verifier = verifier
        .with_orchard::<String, _>(|bundle| {
            memo_reaches_hash_path |=
                bundle_memo_reaches_hash_path(bundle).map_err(OrchardError::Custom)?;
            orchard_bundle = convert_shielded_bundle(
                bundle,
                branch,
                ValuePool::Orchard,
                shielded_derivation.as_ref(),
            )
            .map_err(OrchardError::Custom)?;
            Ok(())
        })
        .map_err(|e| map_orchard_error("Orchard", e))?;

    if global.tx_version >= V6_TX_VERSION {
        verifier
            .with_ironwood::<String, _>(|bundle| {
                memo_reaches_hash_path |=
                    bundle_memo_reaches_hash_path(bundle).map_err(OrchardError::Custom)?;
                ironwood_bundle =
                    convert_ironwood_bundle(bundle, branch, shielded_derivation.as_ref())
                        .map_err(OrchardError::Custom)?;
                Ok(())
            })
            .map_err(|e| map_orchard_error("Ironwood", e))?;
    }

    Ok(ParsedPczt {
        global,
        transparent_outputs,
        orchard_bundle,
        ironwood_bundle,
        memo_reaches_hash_path,
    })
}

fn parse_global(pczt: &Pczt) -> Result<Global, String> {
    let global = pczt.global();
    let json =
        serde_json::to_value(global).map_err(|e| format!("Serialize PCZT global fields: {e}"))?;
    let read_u32 = |name: &str| {
        json.get(name)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| format!("PCZT global.{name} is missing or invalid"))
    };
    let fallback_lock_time = match json.get("fallback_lock_time") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or("PCZT global.fallback_lock_time is invalid")?,
        ),
    };
    let tx_modifiable = json
        .get("tx_modifiable")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .ok_or("PCZT global.tx_modifiable is missing or invalid")?;
    Ok(Global {
        tx_version: *global.tx_version(),
        version_group_id: *global.version_group_id(),
        consensus_branch_id: *global.consensus_branch_id(),
        fallback_lock_time,
        expiry_height: *global.expiry_height(),
        coin_type: read_u32("coin_type")?,
        tx_modifiable,
    })
}

fn bundle_version_for(branch: BranchId, pool: ValuePool) -> Result<BundleVersion, String> {
    bundle_version_for_branch(branch, pool)
        .ok_or_else(|| format!("No {pool:?} bundle version for branch {branch:?}"))
}

/// The account derivation for padding spends, which carry none of their own.
fn fallback_shielded_derivation(
    pczt: &Pczt,
    include_ironwood: bool,
) -> Result<Option<ShieldedDerivation>, String> {
    let find = |bundle: &orchard::pczt::Bundle| {
        bundle.actions().iter().find_map(|action| {
            action
                .spend()
                .zip32_derivation()
                .as_ref()
                .or_else(|| action.output().zip32_derivation().as_ref())
                .map(convert_shielded_derivation)
        })
    };
    let mut shielded = None;
    let verifier = Verifier::new(pczt.clone())
        .with_orchard::<String, _>(|bundle| {
            shielded = find(bundle);
            Ok(())
        })
        .map_err(|e| map_orchard_error("Orchard", e))?;
    if include_ironwood && shielded.is_none() {
        verifier
            .with_ironwood::<String, _>(|bundle| {
                shielded = find(bundle);
                Ok(())
            })
            .map_err(|e| map_orchard_error("Ironwood", e))?;
    }
    Ok(shielded)
}

fn convert_shielded_derivation(derivation: &orchard::pczt::Zip32Derivation) -> ShieldedDerivation {
    ShieldedDerivation {
        signing_path: derivation
            .derivation_path()
            .iter()
            .map(|component| component.index())
            .collect(),
        seed_fingerprint: *derivation.seed_fingerprint(),
    }
}

fn required_shielded_derivation(
    spend_value: u64,
    derivation: Option<&orchard::pczt::Zip32Derivation>,
    fallback: Option<&ShieldedDerivation>,
) -> Result<ShieldedDerivation, String> {
    match derivation {
        Some(derivation) => Ok(convert_shielded_derivation(derivation)),
        None if spend_value == 0 => fallback.cloned().ok_or_else(|| {
            "Shielded padding spend has no account derivation available for Ledger".into()
        }),
        None => Err("Shielded real spend is missing ZIP-32 derivation".into()),
    }
}

fn convert_shielded_bundle(
    bundle: &orchard::pczt::Bundle,
    branch: BranchId,
    pool: ValuePool,
    fallback_derivation: Option<&ShieldedDerivation>,
) -> Result<Option<ShieldedBundle>, String> {
    if bundle.actions().is_empty() {
        return Ok(None);
    }
    let bundle_version = bundle_version_for(branch, pool)?;
    let actions = bundle
        .actions()
        .iter()
        .map(|action| convert_shielded_action(action, fallback_derivation))
        .collect::<Result<Vec<_>, _>>()?;
    let (magnitude, sign) = bundle.value_sum().magnitude_sign();
    let value_balance = if matches!(sign, orchard::value::Sign::Negative) {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    Ok(Some(ShieldedBundle {
        actions,
        flags: bundle
            .flags()
            .to_byte(bundle_version)
            .ok_or_else(|| format!("Bundle flags are invalid for {bundle_version:?}"))?,
        value_balance,
        anchor: bundle.anchor().to_bytes(),
    }))
}

fn convert_ironwood_bundle(
    bundle: &orchard::pczt::Bundle,
    branch: BranchId,
    fallback_derivation: Option<&ShieldedDerivation>,
) -> Result<Option<IronwoodBundle>, String> {
    let Some(shared) =
        convert_shielded_bundle(bundle, branch, ValuePool::Ironwood, fallback_derivation)?
    else {
        return Ok(None);
    };
    let note_plaintext_version =
        match bundle_version_for(branch, ValuePool::Ironwood)?.note_version() {
            NoteVersion::V2 => 0x02,
            NoteVersion::V3 => 0x03,
        };
    Ok(Some(IronwoodBundle {
        actions: shared
            .actions
            .into_iter()
            .map(|action| IronwoodAction {
                action,
                note_plaintext_version,
            })
            .collect(),
        flags: shared.flags,
        value_balance: shared.value_balance,
        anchor: shared.anchor,
    }))
}

fn convert_shielded_action(
    action: &orchard::pczt::Action,
    fallback_derivation: Option<&ShieldedDerivation>,
) -> Result<ShieldedAction, String> {
    let spend = action.spend();
    let output = action.output();
    let spend_value = spend
        .value()
        .map(|value| value.inner())
        .ok_or("Shielded spend is missing its value")?;
    let derivation = required_shielded_derivation(
        spend_value,
        spend.zip32_derivation().as_ref(),
        fallback_derivation,
    )?;
    let encrypted_note = output.encrypted_note();
    Ok(ShieldedAction {
        cv_net: action.cv_net().to_bytes(),
        nullifier: spend.nullifier().to_bytes(),
        rk: spend.rk().into(),
        spend_recipient: spend
            .recipient()
            .map(|recipient| recipient.to_raw_address_bytes())
            .ok_or("Shielded spend is missing its recipient")?,
        spend_value,
        spend_rho: spend
            .rho()
            .map(|rho| rho.to_bytes())
            .ok_or("Shielded spend is missing rho")?,
        spend_rseed: spend
            .rseed()
            .map(|rseed| *rseed.as_bytes())
            .ok_or("Shielded spend is missing rseed")?,
        alpha: spend
            .alpha()
            .map(|alpha| alpha.to_repr())
            .ok_or("Shielded spend is missing alpha")?,
        signing_path: derivation.signing_path,
        seed_fingerprint: derivation.seed_fingerprint,
        cmx: output.cmx().to_bytes(),
        ephemeral_key: encrypted_note.epk_bytes,
        enc_ciphertext: encrypted_note.enc_ciphertext.to_vec(),
        out_ciphertext: encrypted_note.out_ciphertext.to_vec(),
        recipient: output
            .recipient()
            .map(|recipient| recipient.to_raw_address_bytes())
            .ok_or("Shielded output is missing its recipient")?,
        value: output
            .value()
            .map(|value| value.inner())
            .ok_or("Shielded output is missing its value")?,
        rseed: output
            .rseed()
            .map(|rseed| *rseed.as_bytes())
            .ok_or("Shielded output is missing rseed")?,
        rcv: action
            .rcv()
            .as_ref()
            .map(|rcv| rcv.to_bytes())
            .ok_or("Shielded action is missing rcv")?,
    })
}

fn bundle_memo_reaches_hash_path(bundle: &orchard::pczt::Bundle) -> Result<bool, String> {
    for action in bundle.actions() {
        if output_memo_reaches_hash_path(action)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn output_memo_reaches_hash_path(action: &orchard::pczt::Action) -> Result<bool, String> {
    let output = action.output();
    let note = Note::from_parts(
        output
            .recipient()
            .ok_or("Shielded output is missing its recipient")?,
        output
            .value()
            .ok_or("Shielded output is missing its value")?,
        // rho of an action's output is the nullifier of its spend.
        Rho::from_bytes(&action.spend().nullifier().to_bytes())
            .into_option()
            .ok_or("Shielded spend nullifier is not a valid rho")?,
        output.rseed().ok_or("Shielded output is missing rseed")?,
        *output.note_version(),
    )
    .into_option()
    .ok_or("Shielded output note is invalid")?;
    let pk_d = OrchardDomain::get_pk_d(&note);
    let esk = OrchardDomain::derive_esk(&note).ok_or("Shielded output is missing esk")?;
    let recovered = if *output.note_version() == NoteVersion::V3 {
        try_output_recovery_with_pkd_esk(
            &IronwoodDomain::for_pczt_action(action),
            pk_d,
            esk,
            action,
        )
    } else {
        try_output_recovery_with_pkd_esk(&OrchardDomain::for_pczt_action(action), pk_d, esk, action)
    };
    let Some((_, _, memo)) = recovered else {
        // Restricted zero-value outputs can have deliberately random ciphertext.
        return if note.value().inner() == 0 {
            Ok(false)
        } else {
            Err("Could not verify the memo before Ledger signing".into())
        };
    };
    Ok(memo_reaches_ledger_hash_path(&memo))
}

/// Whether the Ledger Zcash app would render `memo` as a hash rather than as
/// text (anything but printable ASCII). Mirrors `memo_display` and
/// `is_displayable_memo_text` in the device app.
fn memo_reaches_ledger_hash_path(memo: &[u8; 512]) -> bool {
    // ZIP-302 "no memo": 0xF6 followed by zeros. The device shows no field.
    if memo[0] == 0xf6 && memo[1..].iter().all(|byte| *byte == 0) {
        return false;
    }
    let Some(len) = memo.iter().rposition(|byte| *byte != 0).map(|i| i + 1) else {
        return false;
    };
    // Any other lead byte above the text range is hashed without inspection.
    if memo[0] > 0xf4 {
        return true;
    }
    match std::str::from_utf8(&memo[..len]) {
        Ok(text) => !text.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
        Err(_) => true,
    }
}

fn map_transparent_error(error: TransparentError<String>) -> String {
    match error {
        TransparentError::Custom(message) => message,
        other => format!("Transparent bundle is invalid: {other:?}"),
    }
}

fn map_orchard_error(pool: &str, error: OrchardError<String>) -> String {
    match error {
        OrchardError::Custom(message) => message,
        other => format!("{pool} bundle is invalid: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::memo_reaches_ledger_hash_path;

    fn memo(text: &[u8]) -> [u8; 512] {
        let mut m = [0u8; 512];
        m[..text.len()].copy_from_slice(text);
        m
    }

    #[test]
    fn only_printable_ascii_memos_are_shown_as_text() {
        let mut none = [0u8; 512];
        none[0] = 0xf6;
        assert!(!memo_reaches_ledger_hash_path(&none));
        assert!(!memo_reaches_ledger_hash_path(&memo(b"thanks for lunch")));
        assert!(memo_reaches_ledger_hash_path(&memo("café".as_bytes())));
        assert!(memo_reaches_ledger_hash_path(&memo(b"line\nbreak")));
        assert!(memo_reaches_ledger_hash_path(&memo(&[0xf5, 1])));
    }
}
