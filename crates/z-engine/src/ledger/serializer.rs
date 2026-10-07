// Copyright 2026 Vizor contributors.
// Modifications Copyright 2026 tzarebczan. Licensed under Apache-2.0.
//! Byte-exact serializer for the Ledger Zcash app's compact PCZT APDU subset.
//!
//! Adapted from Vizor (chainapsis/vizor-wallet, Apache-2.0,
//! `rust/src/wallet/ledger/serializer.rs`). Changes: transparent inputs are not
//! serialized (the web wallet does not spend transparent funds from a device).

use super::parse::{
    Global, IronwoodBundle, ParsedPczt, ShieldedAction, ShieldedBundle, TransparentOutput,
};
use super::{MAX_SHIELDED_ACTIONS, MAX_TRANSPARENT_OUTPUTS, MEMO_HASH_UNSUPPORTED};

pub(super) const MAX_PACKET_SIZE: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandPackets {
    pub instruction: u8,
    pub packets: Vec<Vec<u8>>,
    pub finishes_pczt: bool,
}

pub(super) fn serialize_pczt(
    pczt: &ParsedPczt,
    memo_hash_supported: bool,
) -> Result<Vec<CommandPackets>, String> {
    if !memo_hash_supported && pczt.memo_reaches_hash_path {
        return Err(MEMO_HASH_UNSUPPORTED.into());
    }
    let is_v6 = pczt.global.tx_version >= 6;
    let mut commands = vec![
        CommandPackets {
            instruction: 0x52,
            packets: vec![serialize_header(&pczt.global)],
            finishes_pczt: false,
        },
        // No transparent inputs.
        CommandPackets {
            instruction: 0x53,
            packets: vec![vec![0]],
            finishes_pczt: false,
        },
        CommandPackets {
            instruction: 0x54,
            packets: serialize_transparent_outputs(&pczt.transparent_outputs)?,
            finishes_pczt: false,
        },
        CommandPackets {
            instruction: 0x56,
            packets: serialize_orchard_bundle(pczt.orchard_bundle.as_ref())?,
            finishes_pczt: !is_v6,
        },
    ];
    if is_v6 {
        commands.push(CommandPackets {
            instruction: 0x58,
            packets: serialize_ironwood_bundle(pczt.ironwood_bundle.as_ref())?,
            finishes_pczt: true,
        });
    }
    Ok(commands)
}

fn serialize_header(global: &Global) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(34);
    bytes.extend_from_slice(b"PCZT");
    push_u32_le(&mut bytes, if global.tx_version >= 6 { 2 } else { 1 });
    push_u32_le(&mut bytes, global.tx_version);
    push_u32_le(&mut bytes, global.version_group_id);
    push_u32_le(&mut bytes, global.consensus_branch_id);
    match global.fallback_lock_time {
        Some(value) => {
            bytes.push(1);
            push_u32_le(&mut bytes, value);
        }
        None => bytes.push(0),
    }
    push_u32_le(&mut bytes, global.expiry_height);
    push_u32_le(&mut bytes, global.coin_type);
    bytes.push(global.tx_modifiable);
    bytes
}

fn serialize_transparent_outputs(outputs: &[TransparentOutput]) -> Result<Vec<Vec<u8>>, String> {
    ensure_count(
        "transparent outputs",
        outputs.len(),
        MAX_TRANSPARENT_OUTPUTS,
    )?;
    let mut packets = vec![compact_size(outputs.len())?];
    for output in outputs {
        packets.push(output.value.to_le_bytes().to_vec());
        packets.extend(field_packets(&output.script_pubkey)?);
        // No BIP-32 derivation: the wallet never sends change to its own
        // transparent address (the parser refuses outputs that carry one).
        packets.push(vec![0]);
    }
    Ok(packets)
}

fn serialize_orchard_bundle(bundle: Option<&ShieldedBundle>) -> Result<Vec<Vec<u8>>, String> {
    let Some(bundle) = bundle else {
        return Ok(vec![vec![0]]);
    };
    let actions = bundle.actions.iter().collect::<Vec<_>>();
    serialize_shielded_bundle(
        &actions,
        bundle.flags,
        bundle.value_balance,
        &bundle.anchor,
        |_| None,
    )
}

fn serialize_ironwood_bundle(bundle: Option<&IronwoodBundle>) -> Result<Vec<Vec<u8>>, String> {
    let Some(bundle) = bundle else {
        return Ok(vec![vec![0]]);
    };
    let actions = bundle
        .actions
        .iter()
        .map(|item| &item.action)
        .collect::<Vec<_>>();
    serialize_shielded_bundle(
        &actions,
        bundle.flags,
        bundle.value_balance,
        &bundle.anchor,
        |index| Some(bundle.actions[index].note_plaintext_version),
    )
}

fn serialize_shielded_bundle(
    actions: &[&ShieldedAction],
    flags: u8,
    value_balance: i128,
    anchor: &[u8; 32],
    note_version: impl Fn(usize) -> Option<u8>,
) -> Result<Vec<Vec<u8>>, String> {
    ensure_count("shielded actions", actions.len(), MAX_SHIELDED_ACTIONS)?;
    let mut packets = vec![compact_size(actions.len())?];
    if actions.is_empty() {
        return Ok(packets);
    }
    for (index, action) in actions.iter().enumerate() {
        serialize_shielded_action(action, note_version(index), &mut packets)?;
    }
    let magnitude = u64::try_from(value_balance.unsigned_abs())
        .map_err(|_| "Shielded value balance exceeds Ledger's u64 range")?;
    let mut trailer = Vec::with_capacity(42);
    trailer.push(flags);
    trailer.extend_from_slice(&magnitude.to_le_bytes());
    trailer.push(u8::from(value_balance.is_negative()));
    trailer.extend_from_slice(anchor);
    packets.push(trailer);
    Ok(packets)
}

fn serialize_shielded_action(
    action: &ShieldedAction,
    note_plaintext_version: Option<u8>,
    packets: &mut Vec<Vec<u8>>,
) -> Result<(), String> {
    let mut spend = Vec::with_capacity(243);
    spend.extend_from_slice(&action.cv_net);
    spend.extend_from_slice(&action.nullifier);
    spend.extend_from_slice(&action.rk);
    spend.extend_from_slice(&action.spend_recipient);
    spend.extend_from_slice(&action.spend_value.to_le_bytes());
    spend.extend_from_slice(&action.spend_rho);
    spend.extend_from_slice(&action.spend_rseed);
    spend.extend_from_slice(&action.alpha);
    packets.push(spend);

    let mut derivation = Vec::with_capacity(33 + action.signing_path.len() * 4);
    derivation.extend_from_slice(&action.seed_fingerprint);
    derivation.extend_from_slice(&pack_derivation_path(&action.signing_path)?);
    packets.push(derivation);

    let mut output = Vec::with_capacity(64);
    output.extend_from_slice(&action.cmx);
    output.extend_from_slice(&action.ephemeral_key);
    packets.push(output);
    packets.extend(field_packets(&action.enc_ciphertext)?);
    packets.extend(field_packets(&action.out_ciphertext)?);

    let mut metadata = Vec::with_capacity(116);
    metadata.extend_from_slice(&action.recipient);
    metadata.extend_from_slice(&action.value.to_le_bytes());
    metadata.extend_from_slice(&action.rseed);
    metadata.extend_from_slice(&action.rcv);
    if let Some(version) = note_plaintext_version {
        metadata.push(version);
    }
    packets.push(metadata);
    Ok(())
}

pub(super) fn pack_derivation_path(path: &[u32]) -> Result<Vec<u8>, String> {
    let path_len = u8::try_from(path.len()).map_err(|_| "Ledger derivation path is too long")?;
    let mut bytes = Vec::with_capacity(1 + path.len() * 4);
    bytes.push(path_len);
    for component in path {
        bytes.extend_from_slice(&component.to_be_bytes());
    }
    ensure_packet_size(&bytes)?;
    Ok(bytes)
}

fn field_packets(field: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut bytes = compact_size(field.len())?;
    bytes.extend_from_slice(field);
    Ok(bytes
        .chunks(MAX_PACKET_SIZE)
        .map(|chunk| chunk.to_vec())
        .collect())
}

fn compact_size(value: usize) -> Result<Vec<u8>, String> {
    if value < 253 {
        return Ok(vec![value as u8]);
    }
    if value <= u16::MAX as usize {
        let mut bytes = vec![253];
        bytes.extend_from_slice(&(value as u16).to_le_bytes());
        return Ok(bytes);
    }
    if value <= u32::MAX as usize {
        let mut bytes = vec![254];
        bytes.extend_from_slice(&(value as u32).to_le_bytes());
        return Ok(bytes);
    }
    let value = u64::try_from(value).map_err(|_| "Length exceeds CompactSize range")?;
    let mut bytes = vec![255];
    bytes.extend_from_slice(&value.to_le_bytes());
    Ok(bytes)
}

fn ensure_count(label: &str, count: usize, maximum: usize) -> Result<(), String> {
    if count > maximum {
        Err(format!(
            "ledger_capacity: Ledger supports at most {maximum} {label}; found {count}"
        ))
    } else {
        Ok(())
    }
}

fn ensure_packet_size(packet: &[u8]) -> Result<(), String> {
    if packet.len() > MAX_PACKET_SIZE {
        Err(format!(
            "Ledger APDU payload exceeds {MAX_PACKET_SIZE} bytes: {}",
            packet.len()
        ))
    } else {
        Ok(())
    }
}

pub(super) fn packet_p1(index: usize, total: usize) -> u8 {
    if index == 0 {
        0x00
    } else if index == total - 1 {
        0x01
    } else {
        0x80
    }
}

pub(super) fn packet_p2(index: usize, total: usize, finishes_pczt: bool) -> u8 {
    if finishes_pczt && index == total - 1 {
        0x01
    } else {
        0x00
    }
}

fn push_u32_le(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(tx_version: u32) -> Global {
        Global {
            tx_version,
            version_group_id: 0x26a7_270a,
            consensus_branch_id: 0xc2d6_d0b4,
            fallback_lock_time: None,
            expiry_height: 0,
            coin_type: 133,
            tx_modifiable: 0,
        }
    }

    #[test]
    fn header_selects_pczt_version_from_transaction_version() {
        assert_eq!(&serialize_header(&global(5))[..8], b"PCZT\x01\x00\x00\x00");
        assert_eq!(&serialize_header(&global(6))[..8], b"PCZT\x02\x00\x00\x00");
    }

    #[test]
    fn v6_command_order_includes_empty_ironwood_bundle() {
        let commands = serialize_pczt(
            &ParsedPczt {
                global: global(6),
                transparent_outputs: vec![],
                orchard_bundle: None,
                ironwood_bundle: None,
                memo_reaches_hash_path: false,
            },
            true,
        )
        .unwrap();
        assert_eq!(
            commands.iter().map(|c| c.instruction).collect::<Vec<_>>(),
            vec![0x52, 0x53, 0x54, 0x56, 0x58]
        );
        assert_eq!(commands.last().unwrap().packets, vec![vec![0]]);
        assert!(commands.last().unwrap().finishes_pczt);
    }

    #[test]
    fn memos_the_app_would_hash_need_a_new_enough_app() {
        let pczt = ParsedPczt {
            global: global(5),
            transparent_outputs: vec![],
            orchard_bundle: None,
            ironwood_bundle: None,
            memo_reaches_hash_path: true,
        };
        assert_eq!(
            serialize_pczt(&pczt, false).unwrap_err(),
            MEMO_HASH_UNSUPPORTED
        );
        assert!(serialize_pczt(&pczt, true).is_ok());
    }

    #[test]
    fn compact_size_matches_zcash_encoding() {
        assert_eq!(compact_size(252).unwrap(), vec![0xfc]);
        assert_eq!(compact_size(580).unwrap(), vec![0xfd, 0x44, 0x02]);
    }

    #[test]
    fn large_fields_are_split_at_apdu_limit() {
        let packets = field_packets(&vec![0x11; 580]).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].len(), 255);
        assert_eq!(&packets[0][..3], &[0xfd, 0x44, 0x02]);
        assert_eq!(packets.iter().map(Vec::len).sum::<usize>(), 583);
    }

    #[test]
    fn derivation_paths_use_big_endian_components() {
        let path = pack_derivation_path(&[0x8000_0020, 0x8000_0085, 0x8000_0000]).unwrap();
        assert_eq!(path, [3, 0x80, 0, 0, 0x20, 0x80, 0, 0, 0x85, 0x80, 0, 0, 0]);
    }

    #[test]
    fn packet_framing_matches_ledger_contract() {
        assert_eq!(packet_p1(0, 1), 0x00);
        assert_eq!(packet_p1(1, 3), 0x80);
        assert_eq!(packet_p1(2, 3), 0x01);
        assert_eq!(packet_p2(2, 3, true), 0x01);
        assert_eq!(packet_p2(1, 3, true), 0x00);
    }
}
