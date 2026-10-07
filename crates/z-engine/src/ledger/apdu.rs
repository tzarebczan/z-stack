// Copyright 2026 Vizor contributors.
// Modifications Copyright 2026 tzarebczan. Licensed under Apache-2.0.
//! APDUs for the Ledger Zcash app: device management, UFVK export, status words.
//!
//! Adapted from Vizor (chainapsis/vizor-wallet, Apache-2.0,
//! `rust/src/wallet/ledger/{apdu,transport}.rs`). Changes: commands are plain
//! data for a browser transport (WebHID) instead of a desktop HID session.

use serde::Serialize;

use super::serializer::pack_derivation_path;

pub(crate) const ZCASH_CLA: u8 = 0xe0;
const BOLOS_CLA: u8 = 0xb0;
const GET_APP_AND_VERSION: u8 = 0x01;
const OPEN_APP: u8 = 0xd8;
const GET_VK: u8 = 0x50;
const GET_VK_FIRST: u8 = 0x00;
const GET_VK_CONTINUE: u8 = 0x80;
const GET_VK_UFVK: u8 = 0x00;
const RESPONSE_OK: u16 = 0x9000;
const UFVK_RESPONSE_LIMIT: usize = 8 * 1024;

/// One command for the device. The transport frames it and returns the raw
/// response, status word included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApduCommand {
    pub cla: u8,
    pub ins: u8,
    pub p1: u8,
    pub p2: u8,
    /// At most 255 bytes, hex.
    #[serde(serialize_with = "hex_bytes")]
    pub data: Vec<u8>,
}

fn hex_bytes<S: serde::Serializer>(data: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&data.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// The running app and its version (dashboard included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceApp {
    pub name: String,
    pub version: String,
}

pub fn app_info_command() -> ApduCommand {
    ApduCommand {
        cla: BOLOS_CLA,
        ins: GET_APP_AND_VERSION,
        p1: 0,
        p2: 0,
        data: Vec::new(),
    }
}

/// Opens the Zcash app from the dashboard. The device may drop the
/// connection while it switches apps.
pub fn open_zcash_app_command() -> ApduCommand {
    ApduCommand {
        cla: ZCASH_CLA,
        ins: OPEN_APP,
        p1: 0,
        p2: 0,
        data: b"Zcash".to_vec(),
    }
}

pub fn decode_app_info(response: &[u8]) -> Result<DeviceApp, String> {
    let response = decode_raw_response(response)?;
    let mut cursor = 0usize;
    let format = take_byte(&response, &mut cursor, "format")?;
    if format != 1 {
        return Err(format!(
            "Ledger returned unsupported app-info format {format}"
        ));
    }
    let name = take_length_prefixed_string(&response, &mut cursor, "app name")?;
    let version = take_length_prefixed_string(&response, &mut cursor, "app version")?;
    if cursor < response.len() {
        let flags_len = take_byte(&response, &mut cursor, "flags length")? as usize;
        if cursor.checked_add(flags_len) != Some(response.len()) {
            return Err("Ledger app-info response has malformed flags".into());
        }
    }
    Ok(DeviceApp { name, version })
}

/// UFVK export for `m/32'/133'/account'` (and the transparent `m/44'/133'/account'`):
/// the first command, then the continuation to repeat until the declared
/// length has arrived.
pub fn ufvk_commands(account_index: u32) -> Result<(ApduCommand, ApduCommand), String> {
    if account_index >= 0x8000_0000 {
        return Err("Ledger account index must be below 2^31".into());
    }
    let account = 0x8000_0000 | account_index;
    let mut request = pack_derivation_path(&[0x8000_0020, 0x8000_0085, account])?;
    request.extend_from_slice(&pack_derivation_path(&[0x8000_002c, 0x8000_0085, account])?);
    Ok((
        ApduCommand {
            cla: ZCASH_CLA,
            ins: GET_VK,
            p1: GET_VK_FIRST,
            p2: GET_VK_UFVK,
            data: request,
        },
        ApduCommand {
            cla: ZCASH_CLA,
            ins: GET_VK,
            p1: GET_VK_CONTINUE,
            p2: GET_VK_UFVK,
            data: Vec::new(),
        },
    ))
}

/// Bytes the UFVK export still expects after `responses` (0 when complete).
pub fn ufvk_bytes_remaining(responses: &[Vec<u8>]) -> Result<usize, String> {
    let chunks = responses
        .iter()
        .map(|r| decode_raw_response(r))
        .collect::<Result<Vec<_>, _>>()?;
    let first = chunks.first().ok_or("Ledger UFVK response is missing")?;
    if first.len() < 2 {
        return Err("Ledger UFVK response is missing its length prefix".into());
    }
    let expected = 2 + u16::from_be_bytes([first[0], first[1]]) as usize;
    if expected > UFVK_RESPONSE_LIMIT {
        return Err("Ledger UFVK response declares an unreasonable length".into());
    }
    Ok(expected.saturating_sub(chunks.iter().map(Vec::len).sum()))
}

pub fn decode_ufvk_responses(responses: &[Vec<u8>]) -> Result<String, String> {
    if responses.is_empty() {
        return Err("Ledger UFVK response is missing".into());
    }
    let chunks = responses
        .iter()
        .map(|r| decode_raw_response(r))
        .collect::<Result<Vec<_>, _>>()?;
    decode_ufvk_chunks(&chunks)
}

fn decode_ufvk_chunks(chunks: &[Vec<u8>]) -> Result<String, String> {
    let mut response = chunks.first().cloned().unwrap_or_default();
    if response.len() < 2 {
        return Err("Ledger UFVK response is missing its length prefix".into());
    }
    let key_len = u16::from_be_bytes([response[0], response[1]]) as usize;
    let expected_len = 2 + key_len;
    if expected_len > UFVK_RESPONSE_LIMIT {
        return Err(format!(
            "Ledger UFVK response declares an unreasonable length: {key_len} bytes"
        ));
    }
    for chunk in chunks.iter().skip(1) {
        if response.len() >= expected_len {
            return Err("Ledger UFVK response contains trailing chunks".into());
        }
        if chunk.is_empty() {
            return Err("Ledger UFVK response ended before the declared length".into());
        }
        response.extend_from_slice(chunk);
    }
    if response.len() < expected_len {
        return Err("Ledger UFVK response ended before the declared length".into());
    }
    if response.len() != expected_len {
        return Err("Ledger UFVK response contains trailing bytes".into());
    }
    String::from_utf8(response[2..].to_vec())
        .map_err(|_| "Ledger UFVK response is not valid UTF-8".into())
}

/// Strips and checks the status word.
pub(crate) fn decode_raw_response(response: &[u8]) -> Result<Vec<u8>, String> {
    if response.len() < 2 {
        return Err("Ledger APDU response was too short to contain a status word".into());
    }
    let status = u16::from_be_bytes([response[response.len() - 2], response[response.len() - 1]]);
    if status != RESPONSE_OK {
        return Err(map_status_word(status));
    }
    Ok(response[..response.len() - 2].to_vec())
}

const STATUS_PREFIX: &str = "ledger_status_";

/// Every device status error starts with a stable `ledger_status_xxxx: `
/// prefix so callers classify by code; the text after it is for people.
pub fn map_status_word(status: u16) -> String {
    format!("{STATUS_PREFIX}{status:04x}: {}", status_word_text(status))
}

fn status_word_text(status: u16) -> String {
    match status {
        // The Zcash app aliases 0x6982 to both SecurityStatusNotSatisfied and
        // NothingReceived; both mean the device must be unlocked again.
        0x5515 | 0x6982 | 0x5303 => {
            "Ledger device is locked; unlock it and reopen the Zcash app".into()
        }
        0x63c0 => "A wrong Ledger PIN was entered; unlock your Ledger".into(),
        0x5501 => "Ledger request was rejected on the device".into(),
        0x6985 => "Ledger request was rejected or the PCZT was not finalized".into(),
        0x5502 => "Ledger device PIN is not set".into(),
        0x5223 => "Ledger device returned an internal error".into(),
        0x6601 => "Ledger device is busy switching apps; retry shortly".into(),
        0x6700 => "Ledger Zcash app rejected the command length".into(),
        0x670a => "Ledger app-open request did not include an app name".into(),
        0x6807 => "The Zcash app is not installed on this Ledger".into(),
        0x6901 => "Ledger display is busy starting a review; retry shortly".into(),
        0x6a80 => "Ledger rejected the PCZT data or key path".into(),
        0x6a84 => "Ledger ran out of memory for this transaction; try a smaller amount".into(),
        0x6b00 => "Ledger Zcash app rejected the command parameters".into(),
        0x6e00 => "Ledger device does not support this command class".into(),
        0x6d00 => "The running Ledger app does not support this command".into(),
        0x6f00 => "Ledger Zcash app reported a technical problem".into(),
        0x6f01 => "Ledger Zcash app could not parse the transaction version".into(),
        0x6f02 => "Ledger Zcash app could not parse the transaction".into(),
        0x6f03 => "Ledger random number generator failed".into(),
        0x6faa => "Ledger Zcash app halted; close and reopen the app".into(),
        0xb007 => "Ledger Zcash app is in the wrong state; close and reopen the app".into(),
        _ => format!("Ledger Zcash app returned status 0x{status:04x}"),
    }
}

fn take_byte(response: &[u8], cursor: &mut usize, field: &str) -> Result<u8, String> {
    let value = response
        .get(*cursor)
        .copied()
        .ok_or_else(|| format!("Ledger app-info response is missing {field}"))?;
    *cursor += 1;
    Ok(value)
}

fn take_length_prefixed_string(
    response: &[u8],
    cursor: &mut usize,
    field: &str,
) -> Result<String, String> {
    let length = take_byte(response, cursor, &format!("{field} length"))? as usize;
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| format!("Ledger {field} length overflowed"))?;
    let bytes = response
        .get(*cursor..end)
        .ok_or_else(|| format!("Ledger app-info response truncated {field}"))?;
    *cursor = end;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| format!("Ledger {field} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn ufvk_plan_matches_the_zcash_app_protocol() {
        let (first, continuation) = ufvk_commands(7).unwrap();
        assert_eq!(
            (first.cla, first.ins, first.p1, first.p2),
            (0xe0, 0x50, 0, 0)
        );
        assert_eq!(
            first.data,
            hex("03800000208000008580000007038000002c8000008580000007")
        );
        assert_eq!(
            (
                continuation.cla,
                continuation.ins,
                continuation.p1,
                continuation.p2
            ),
            (0xe0, 0x50, 0x80, 0)
        );
        assert!(continuation.data.is_empty());
        assert!(ufvk_commands(0x8000_0000).is_err());
    }

    #[test]
    fn ufvk_responses_are_status_checked_and_reassembled() {
        let responses = vec![
            vec![0, 5, b'u', b'v', 0x90, 0],
            vec![b'i', b'e', b'w', 0x90, 0],
        ];
        assert_eq!(ufvk_bytes_remaining(&responses[..1]).unwrap(), 3);
        assert_eq!(ufvk_bytes_remaining(&responses).unwrap(), 0);
        assert_eq!(decode_ufvk_responses(&responses).unwrap(), "uview");
        assert!(decode_ufvk_responses(&[vec![0x69, 0x85]])
            .unwrap_err()
            .contains("rejected"));
        assert!(decode_ufvk_responses(&[vec![0, 5, b'u', 0x90, 0]])
            .unwrap_err()
            .contains("before the declared length"));
        assert!(
            decode_ufvk_responses(&[vec![0, 1, b'u', 0x90, 0], vec![b'x', 0x90, 0]])
                .unwrap_err()
                .contains("trailing")
        );
    }

    #[test]
    fn app_info_decodes_dashboard_and_running_app() {
        let mut app = vec![1, 5];
        app.extend_from_slice(b"Zcash");
        app.push(5);
        app.extend_from_slice(b"3.9.4");
        app.extend_from_slice(&[1, 0, 0x90, 0]);
        assert_eq!(
            decode_app_info(&app).unwrap(),
            DeviceApp {
                name: "Zcash".into(),
                version: "3.9.4".into()
            }
        );
        assert!(decode_app_info(&[1, 5, b'Z', 0x90, 0]).is_err());
        assert!(decode_app_info(&[0x55, 0x15])
            .unwrap_err()
            .starts_with("ledger_status_5515: "));
    }

    #[test]
    fn device_management_apdus_match_ledger_protocol() {
        assert_eq!(
            app_info_command(),
            ApduCommand {
                cla: 0xb0,
                ins: 0x01,
                p1: 0,
                p2: 0,
                data: vec![]
            }
        );
        assert_eq!(open_zcash_app_command().data, b"Zcash");
        assert_eq!(
            (open_zcash_app_command().cla, open_zcash_app_command().ins),
            (0xe0, 0xd8)
        );
    }

    #[test]
    fn every_status_word_error_carries_its_code_prefix() {
        for status in [0x5515, 0x6982, 0x6985, 0x6a80, 0x6a84, 0x6400] {
            assert!(map_status_word(status).starts_with(&format!("ledger_status_{status:04x}: ")));
        }
    }
}
