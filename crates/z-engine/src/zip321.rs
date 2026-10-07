//! ZIP-321 payment URIs. Keep `@z-stack/core` `zip321Uri` / `parseZip321` in lockstep.

use crate::parse_zec_to_zatoshis;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Zip321Payment {
    pub address: String,
    pub amount_zec: Option<String>,
    pub memo: Option<String>,
    pub label: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Zip321Request {
    pub payments: Vec<Zip321Payment>,
}

impl Zip321Request {
    pub fn primary(&self) -> Option<&Zip321Payment> {
        self.payments.first()
    }

    pub fn address(&self) -> &str {
        self.primary().map(|p| p.address.as_str()).unwrap_or("")
    }

    pub fn amount_zec(&self) -> Option<&str> {
        self.primary().and_then(|p| p.amount_zec.as_deref())
    }

    pub fn memo(&self) -> Option<&str> {
        self.primary().and_then(|p| p.memo.as_deref())
    }
}

/// `zcash:<address>` plus optional `amount`, `memo` (base64url), `label`, `message`.
pub fn zip321_uri(address: &str, amount_zec: Option<&str>) -> Result<String, String> {
    zip321_uri_full(address, amount_zec, None, None, None)
}

pub fn zip321_uri_full(
    address: &str,
    amount_zec: Option<&str>,
    memo: Option<&str>,
    label: Option<&str>,
    message: Option<&str>,
) -> Result<String, String> {
    let address = address.trim();
    if address.is_empty() {
        return Err("empty address".into());
    }
    if address
        .chars()
        .any(|c| c.is_whitespace() || c == '?' || c == '&' || c == '#')
    {
        return Err("address is not URI-safe".into());
    }
    let mut q = Vec::new();
    if let Some(amount) = amount_zec.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = parse_zec_to_zatoshis(amount)?;
        q.push(format!("amount={amount}"));
    }
    if let Some(m) = memo.map(str::trim).filter(|s| !s.is_empty()) {
        q.push(format!("memo={}", b64url_encode(m.as_bytes())));
    }
    if let Some(l) = label.map(str::trim).filter(|s| !s.is_empty()) {
        q.push(format!("label={}", percent_encode(l)));
    }
    if let Some(m) = message.map(str::trim).filter(|s| !s.is_empty()) {
        q.push(format!("message={}", percent_encode(m)));
    }
    if q.is_empty() {
        Ok(format!("zcash:{address}"))
    } else {
        Ok(format!("zcash:{address}?{}", q.join("&")))
    }
}

pub fn parse_zip321(uri: &str) -> Result<Zip321Request, String> {
    let raw = uri.trim();
    if !raw.to_ascii_lowercase().starts_with("zcash:") {
        return Err("not a zcash: URI".into());
    }
    let rest = &raw["zcash:".len()..];
    let q = rest.find('?');
    let path_addr = (if q.is_none() {
        rest
    } else {
        &rest[..q.unwrap()]
    })
    .trim();
    if path_addr
        .chars()
        .any(|c| c.is_whitespace() || c == '?' || c == '&' || c == '#')
    {
        return Err("address is not URI-safe".into());
    }
    let mut params: BTreeMap<(u32, String), String> = BTreeMap::new();
    if let Some(qi) = q {
        for part in rest[qi + 1..].split('&') {
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (percent_decode(k)?, percent_decode(v)?),
                None => (percent_decode(part)?, String::new()),
            };
            let (idx, name) = zip321_param_key(&k)?;
            if params.insert((idx, name), v).is_some() {
                return Err(format!("duplicate ZIP-321 parameter: {k}"));
            }
        }
    }
    assemble_payments(path_addr.to_string(), params)
}

/// ZIP-321 `paramindex` is `.` followed by 1-9999 without a leading zero.
/// Anything else after a dot is invalid, and rejecting it here keeps a
/// crafted index from sizing the payment list.
fn zip321_param_key(k: &str) -> Result<(u32, String), String> {
    let Some((name, index)) = k.split_once('.') else {
        return Ok((0, k.to_string()));
    };
    let valid = (1..=4).contains(&index.len())
        && !index.starts_with('0')
        && index.bytes().all(|b| b.is_ascii_digit());
    match index.parse::<u32>() {
        Ok(i) if valid => Ok((i, name.to_string())),
        _ => Err(format!("invalid ZIP-321 parameter index: {k}")),
    }
}

/// `zcash:addr0?amount=1&address.1=addr1&amount.1=2&memo.1=...`
pub fn zip321_uri_many(payments: &[Zip321Payment]) -> Result<String, String> {
    if payments.is_empty() {
        return Err("ZIP-321 needs at least one payment".into());
    }
    let first = &payments[0];
    if first.address.trim().is_empty() {
        return Err("empty address".into());
    }
    let mut uri = zip321_uri_full(
        &first.address,
        first.amount_zec.as_deref(),
        first.memo.as_deref(),
        first.label.as_deref(),
        first.message.as_deref(),
    )?;
    for (i, p) in payments.iter().enumerate().skip(1) {
        let addr = p.address.trim();
        if addr.is_empty() {
            return Err(format!("empty address at index {i}"));
        }
        if addr
            .chars()
            .any(|c| c.is_whitespace() || c == '?' || c == '&' || c == '#')
        {
            return Err("address is not URI-safe".into());
        }
        let sep = if uri.contains('?') { "&" } else { "?" };
        uri.push_str(&format!("{sep}address.{i}={addr}"));
        if let Some(amount) = p
            .amount_zec
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let _ = parse_zec_to_zatoshis(amount)?;
            uri.push_str(&format!("&amount.{i}={amount}"));
        }
        if let Some(m) = p.memo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            uri.push_str(&format!("&memo.{i}={}", b64url_encode(m.as_bytes())));
        }
        if let Some(l) = p.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            uri.push_str(&format!("&label.{i}={}", percent_encode(l)));
        }
        if let Some(m) = p
            .message
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            uri.push_str(&format!("&message.{i}={}", percent_encode(m)));
        }
    }
    Ok(uri)
}

fn assemble_payments(
    path_addr: String,
    params: BTreeMap<(u32, String), String>,
) -> Result<Zip321Request, String> {
    let mut by_index: BTreeMap<u32, Zip321Payment> = BTreeMap::new();
    if !path_addr.is_empty() {
        by_index.insert(
            0,
            Zip321Payment {
                address: path_addr,
                ..Zip321Payment::default()
            },
        );
    }
    for ((idx, key), val) in params {
        let name = if let Some(inner) = key.strip_prefix("req-") {
            if !matches!(inner, "address" | "amount" | "memo" | "label" | "message") {
                return Err(format!("unsupported required ZIP-321 parameter: {key}"));
            }
            inner
        } else {
            key.as_str()
        };
        let slot = by_index.entry(idx).or_default();
        match name {
            "address" if !slot.address.is_empty() => {
                return Err("ZIP-321 URI names the same payment address twice".into());
            }
            "address" if !val.is_empty() => slot.address = val,
            "amount" if !val.is_empty() => {
                let _ = parse_zec_to_zatoshis(&val)?;
                slot.amount_zec = Some(val);
            }
            "memo" if !val.is_empty() => {
                let bytes = b64url_decode(&val).unwrap_or_else(|_| val.as_bytes().to_vec());
                slot.memo =
                    Some(String::from_utf8(bytes).map_err(|_| "memo is not UTF-8".to_string())?);
            }
            "label" if !val.is_empty() => slot.label = Some(val),
            "message" if !val.is_empty() => slot.message = Some(val),
            _ => {}
        }
    }
    if !by_index
        .get(&0)
        .map(|p| !p.address.trim().is_empty())
        .unwrap_or(false)
    {
        return Err("ZIP-321 URI has no address".into());
    }
    let max = *by_index.keys().max().unwrap_or(&0);
    let mut payments = Vec::with_capacity(by_index.len());
    for i in 0..=max {
        let Some(p) = by_index.remove(&i) else {
            return Err("ZIP-321 payment indices must be sequential".into());
        };
        if p.address.trim().is_empty() {
            return Err("ZIP-321 payment indices must be sequential".into());
        }
        payments.push(p);
    }
    Ok(Zip321Request { payments })
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(s.len());
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return Err("bad percent-encoding".into());
            }
            let hi = from_hex_nibble(raw[i + 1])?;
            let lo = from_hex_nibble(raw[i + 2])?;
            bytes.push((hi << 4) | lo);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| "percent-decoded text is not UTF-8".into())
}

fn from_hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err("bad percent-encoding".into()),
    }
}

fn b64url_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(T[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < data.len() {
            out.push(T[(b2 & 63) as usize] as char);
        }
        i += 3;
    }
    out
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut clean = s.replace('+', "-").replace('/', "_");
    clean.retain(|c| c != '=');
    let mut vals = Vec::with_capacity(clean.len());
    for c in clean.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err("invalid base64url memo".into()),
        };
        vals.push(v);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < vals.len() {
        let v0 = vals[i];
        let v1 = vals.get(i + 1).copied().unwrap_or(0);
        out.push((v0 << 2) | (v1 >> 4));
        if i + 2 < vals.len() {
            let v2 = vals[i + 2];
            out.push((v1 << 4) | (v2 >> 2));
            if i + 3 < vals.len() {
                out.push((v2 << 6) | vals[i + 3]);
            }
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_amount_and_memo() {
        let uri = zip321_uri_full(
            "uregtest1abc",
            Some("0.0005"),
            Some("hello"),
            Some("pay me"),
            None,
        )
        .unwrap();
        assert!(uri.starts_with("zcash:uregtest1abc?"));
        assert!(uri.contains("amount=0.0005"));
        assert!(uri.contains("memo="));
        let p = parse_zip321(&uri).unwrap();
        assert_eq!(p.address(), "uregtest1abc");
        assert_eq!(p.amount_zec(), Some("0.0005"));
        assert_eq!(p.memo(), Some("hello"));
        assert_eq!(p.primary().unwrap().label.as_deref(), Some("pay me"));
        assert_eq!(p.payments.len(), 1);
    }

    #[test]
    fn multipay_roundtrip() {
        let uri = zip321_uri_many(&[
            Zip321Payment {
                address: "uregtest1aaa".into(),
                amount_zec: Some("0.1".into()),
                memo: Some("one".into()),
                ..Zip321Payment::default()
            },
            Zip321Payment {
                address: "uregtest1bbb".into(),
                amount_zec: Some("0.2".into()),
                memo: Some("two".into()),
                ..Zip321Payment::default()
            },
        ])
        .unwrap();
        assert!(uri.starts_with("zcash:uregtest1aaa?"));
        assert!(uri.contains("address.1=uregtest1bbb"));
        assert!(uri.contains("amount.1=0.2"));
        let p = parse_zip321(&uri).unwrap();
        assert_eq!(p.payments.len(), 2);
        assert_eq!(p.payments[0].address, "uregtest1aaa");
        assert_eq!(p.payments[0].amount_zec.as_deref(), Some("0.1"));
        assert_eq!(p.payments[0].memo.as_deref(), Some("one"));
        assert_eq!(p.payments[1].address, "uregtest1bbb");
        assert_eq!(p.payments[1].amount_zec.as_deref(), Some("0.2"));
        assert_eq!(p.payments[1].memo.as_deref(), Some("two"));
    }

    #[test]
    fn query_address_without_path() {
        let p = parse_zip321("zcash:?address=uregtest1abc&amount=1").unwrap();
        assert_eq!(p.payments.len(), 1);
        assert_eq!(p.address(), "uregtest1abc");
        assert_eq!(p.amount_zec(), Some("1"));
    }

    #[test]
    fn rejects_index_gaps_and_unknown_req() {
        assert!(parse_zip321("zcash:uregtest1aaa?address.2=uregtest1bbb").is_err());
        assert!(parse_zip321("zcash:uregtest1aaa?req-expiry=1").is_err());
        assert!(parse_zip321("zcash:?amount=1").is_err());
        assert!(zip321_uri_many(&[]).is_err());
    }

    #[test]
    fn rejects_malformed_indices_without_sizing_by_them() {
        // Pasted into the desktop To field, these used to size a payment
        // vector by the index (about 500 GB) before any validation.
        for uri in [
            "zcash:uregtest1aaa?amount.4000000000=1",
            "zcash:uregtest1aaa?amount.4294967295=1",
            "zcash:uregtest1aaa?address.10000=uregtest1bbb",
            "zcash:uregtest1aaa?address.01=uregtest1bbb",
            "zcash:uregtest1aaa?address.0=uregtest1bbb",
            "zcash:uregtest1aaa?amount.=1",
            "zcash:uregtest1aaa?amount.1x=1",
        ] {
            assert!(parse_zip321(uri).is_err(), "{uri}");
        }
        let max =
            parse_zip321("zcash:uregtest1aaa?address.9999=uregtest1bbb&address.1=uregtest1ccc");
        assert!(max.unwrap_err().contains("sequential"));
    }

    #[test]
    fn rejects_duplicate_parameters_instead_of_overriding() {
        for uri in [
            "zcash:uregtest1aaa?amount=1&amount=2",
            "zcash:uregtest1aaa?address=uregtest1bbb",
            "zcash:uregtest1aaa?address.1=uregtest1bbb&req-address.1=uregtest1ccc",
            "zcash:uregtest1aaa?address.1=uregtest1bbb&address.1=uregtest1ccc",
        ] {
            assert!(parse_zip321(uri).is_err(), "{uri}");
        }
        let one = parse_zip321("zcash:?address=uregtest1bbb&amount=1").unwrap();
        assert_eq!(one.payments.len(), 1);
        assert_eq!(one.payments[0].address, "uregtest1bbb");
    }

    #[test]
    fn three_payments_roundtrip() {
        let uri = zip321_uri_many(&[
            Zip321Payment {
                address: "uregtest1aaa".into(),
                amount_zec: Some("0.1".into()),
                ..Zip321Payment::default()
            },
            Zip321Payment {
                address: "uregtest1bbb".into(),
                amount_zec: Some("0.2".into()),
                ..Zip321Payment::default()
            },
            Zip321Payment {
                address: "uregtest1ccc".into(),
                amount_zec: Some("0.3".into()),
                memo: Some("three".into()),
                ..Zip321Payment::default()
            },
        ])
        .unwrap();
        assert!(uri.contains("address.1=uregtest1bbb"));
        assert!(uri.contains("address.2=uregtest1ccc"));
        let p = parse_zip321(&uri).unwrap();
        assert_eq!(p.payments.len(), 3);
        assert_eq!(p.payments[2].memo.as_deref(), Some("three"));
    }

    #[test]
    fn plus_in_label_is_literal() {
        let uri = zip321_uri_full("uregtest1abc", None, None, Some("a+b"), None).unwrap();
        assert!(uri.contains("a%2Bb") || uri.contains("a+b"));
        let p = parse_zip321(&uri).unwrap();
        assert_eq!(p.primary().unwrap().label.as_deref(), Some("a+b"));
        let plus = parse_zip321("zcash:uregtest1abc?label=a+b").unwrap();
        assert_eq!(plus.primary().unwrap().label.as_deref(), Some("a+b"));
    }
}
