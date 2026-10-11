//! JSON-RPC helper for a local validator (Zakura / Zebra `sendrawtransaction`).

use crate::error::{EngineError, Result};
use crate::{LightServer, Network};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

mod verified;
pub(super) use verified::VerifiedValidator;

/// Verify the validator's chain and genesis, then broadcast on that connection.
pub fn send_raw_transaction_verified(url: &str, network: Network, raw: &[u8]) -> Result<String> {
    VerifiedValidator::connect(url, network)?.send(raw)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn parse_host_port(url: &str) -> Result<(String, u16)> {
    let url = url.trim();
    let normalized = if url.contains("://") {
        url.to_owned()
    } else {
        format!("http://{url}")
    };
    let uri: http::Uri = normalized
        .parse()
        .map_err(|e| EngineError::Transport(format!("invalid validator rpc URL: {e}")))?;
    if uri.scheme_str() != Some("http") {
        return Err(EngineError::Transport(
            "validator rpc supports HTTP only; HTTPS requires a TLS transport".into(),
        ));
    }
    let authority = uri
        .authority()
        .ok_or_else(|| EngineError::Transport("validator rpc URL needs host:port".into()))?;
    if authority.as_str().contains('@') || uri.query().is_some() || !matches!(uri.path(), "" | "/")
    {
        return Err(EngineError::Transport(
            "validator rpc URL must be a host:port endpoint without credentials, path or query"
                .into(),
        ));
    }
    let port = authority.port_u16().ok_or_else(|| {
        EngineError::Transport("validator rpc URL needs a valid explicit port".into())
    })?;
    // ToSocketAddrs expects an unbracketed IPv6 host, while the HTTP authority
    // retains brackets. Preserve that distinction when constructing Host below.
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.is_empty() {
        return Err(EngineError::Transport(
            "validator rpc URL needs a host".into(),
        ));
    }
    Ok((host.to_owned(), port))
}

fn host_header(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// POST a JSON-RPC method to a Zebra/zcashd HTTP endpoint (no cookie auth).
pub fn jsonrpc(url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    jsonrpc_response(url, method, params, None)
}

const RAW_TRANSACTION_HEX_LIMIT: usize = 2 * 2 * 1024 * 1024;
const RAW_TRANSACTION_RESPONSE_LIMIT: usize = RAW_TRANSACTION_HEX_LIMIT + 64 * 1024;

fn jsonrpc_response(
    url: &str,
    method: &str,
    params: serde_json::Value,
    limit: Option<usize>,
) -> Result<serde_json::Value> {
    let (host, port) = parse_host_port(url)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string();
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| EngineError::Transport(format!("zebra rpc resolve {host}:{port}: {e}")))?
        .next()
        .ok_or_else(|| EngineError::Transport(format!("zebra rpc no addr for {host}:{port}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| EngineError::Transport(format!("zebra rpc connect {host}:{port}: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(120))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(15))).ok();
    let authority = host_header(&host, port);
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| EngineError::Transport(format!("zebra rpc write: {e}")))?;
    if let Some(limit) = limit {
        let mut response = Vec::with_capacity(limit);
        let mut chunk = [0u8; 8192];
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(EngineError::Transport(
                    "getrawtransaction response read timeout".into(),
                ));
            }
            stream.set_read_timeout(Some(remaining)).map_err(|_| {
                EngineError::Transport("getrawtransaction response read failed".into())
            })?;
            let length = stream.read(&mut chunk).map_err(|_| {
                EngineError::Transport("getrawtransaction response read failed".into())
            })?;
            if length == 0 {
                break;
            }
            if length > limit.saturating_sub(response.len()) {
                return Err(EngineError::Transport(
                    "getrawtransaction response limit".into(),
                ));
            }
            response.extend_from_slice(&chunk[..length]);
        }
        let response = String::from_utf8(response)
            .map_err(|_| EngineError::Transport("getrawtransaction response invalid".into()))?;
        let (_, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
            EngineError::Transport("getrawtransaction response body missing".into())
        })?;
        let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|_| {
            EngineError::Transport("getrawtransaction response JSON invalid".into())
        })?;
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(EngineError::BroadcastRejected {
                code: error
                    .get("code")
                    .and_then(|code| code.as_i64())
                    .and_then(|code| i32::try_from(code).ok())
                    .unwrap_or(-1),
                message: "getrawtransaction RPC rejected".into(),
            });
        }
        return value
            .get("result")
            .cloned()
            .ok_or_else(|| EngineError::Transport("getrawtransaction result missing".into()));
    }
    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| EngineError::Transport(format!("zebra rpc read: {e}")))?;
    let json = resp.split("\r\n\r\n").nth(1).ok_or_else(|| {
        EngineError::Transport(format!(
            "zebra rpc no body: {}",
            resp.chars().take(200).collect::<String>()
        ))
    })?;
    let v: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| EngineError::Transport(format!("zebra rpc json: {e}")))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(EngineError::BroadcastRejected {
            code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32,
            message: err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or(&err.to_string())
                .to_string(),
        });
    }
    Ok(v["result"].clone())
}

/// Submit a raw transaction via Zebra `sendrawtransaction`.
pub fn send_raw_transaction(url: &str, raw: &[u8]) -> Result<String> {
    let result = jsonrpc(url, "sendrawtransaction", serde_json::json!([to_hex(raw)]))?;
    result
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| EngineError::Transport(format!("sendrawtransaction: {result}")))
}

/// Mine `n` blocks on a regtest validator (`generate`).
pub fn generate_blocks(url: &str, n: u32) -> Result<serde_json::Value> {
    jsonrpc(url, "generate", serde_json::json!([n.max(1)]))
}

/// Mempool txids from Zebra `getrawmempool`.
pub fn get_raw_mempool(url: &str) -> Result<Vec<String>> {
    let result = jsonrpc(url, "getrawmempool", serde_json::json!([]))?;
    Ok(result
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Raw transaction hex from Zebra `getrawtransaction`.
pub fn get_raw_transaction_hex(url: &str, txid: &str) -> Result<String> {
    let result = jsonrpc_response(
        url,
        "getrawtransaction",
        serde_json::json!([txid, 0]),
        Some(RAW_TRANSACTION_RESPONSE_LIMIT),
    )?;
    bounded_transaction_hex(result.as_str())
}

/// Verbose `getrawtransaction`: raw hex plus the mined height, which is absent
/// (or not positive) while the transaction is only in the mempool.
pub fn get_raw_transaction_verbose(url: &str, txid: &str) -> Result<(String, Option<u32>)> {
    let result = jsonrpc_response(
        url,
        "getrawtransaction",
        serde_json::json!([txid, 1]),
        Some(RAW_TRANSACTION_RESPONSE_LIMIT),
    )?;
    let hex = bounded_transaction_hex(result.get("hex").and_then(|h| h.as_str()))?;
    let height = result
        .get("height")
        .and_then(|h| h.as_i64())
        .filter(|h| *h > 0)
        .and_then(|h| u32::try_from(h).ok());
    Ok((hex, height))
}

fn bounded_transaction_hex(hex: Option<&str>) -> Result<String> {
    let hex =
        hex.ok_or_else(|| EngineError::Transport("getrawtransaction result invalid".into()))?;
    if hex.is_empty()
        || hex.len() > RAW_TRANSACTION_HEX_LIMIT
        || hex.len() % 2 != 0
        || !hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(EngineError::Transport(
            "getrawtransaction hex invalid".into(),
        ));
    }
    Ok(hex.to_owned())
}

/// The node's definitive "no such mempool or main chain transaction".
/// The request never left this machine (no address, or the connection was
/// refused or unreachable), so the node cannot have received it.
pub fn never_sent(error: &EngineError) -> bool {
    matches!(error, EngineError::Transport(message)
        if ["zebra rpc resolve ", "zebra rpc no addr ", "zebra rpc connect "]
            .iter()
            .any(|prefix| message.starts_with(prefix)))
}

pub fn is_unknown_transaction(error: &EngineError) -> bool {
    matches!(error, EngineError::BroadcastRejected { code: -5, .. })
}

/// Up to `max` mempool txs as `{txid, hex}` for WASM trial-decrypt.
pub fn fetch_mempool_txs(url: &str, max: usize) -> Result<Vec<serde_json::Value>> {
    let ids = get_raw_mempool(url)?;
    let mut out = Vec::new();
    for id in ids.into_iter().take(max.max(1).min(64)) {
        match get_raw_transaction_hex(url, &id) {
            Ok(hex) => out.push(serde_json::json!({ "txid": id, "hex": hex })),
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// Result of probing a validator JSON-RPC endpoint (Zakura / Zebra / zcashd).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorProbe {
    pub ok: bool,
    pub url: String,
    pub chain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subversion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `getblockchaininfo` + optional `getnetworkinfo`. Does not require cookie auth.
pub fn probe_validator(url: &str) -> ValidatorProbe {
    let url = url.trim().trim_end_matches('/').to_string();
    match jsonrpc(&url, "getblockchaininfo", serde_json::json!([])) {
        Ok(info) => {
            let chain = info
                .get("chain")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let height = info
                .get("blocks")
                .and_then(|b| b.as_u64())
                .and_then(|n| u32::try_from(n).ok());
            let subversion = jsonrpc(&url, "getnetworkinfo", serde_json::json!([]))
                .ok()
                .and_then(|n| {
                    n.get("subversion")
                        .and_then(|s| s.as_str())
                        .map(str::to_string)
                });
            ValidatorProbe {
                ok: true,
                url,
                chain,
                height,
                subversion,
                error: None,
            }
        }
        Err(e) => ValidatorProbe {
            ok: false,
            url,
            chain: String::new(),
            height: None,
            subversion: None,
            error: Some(e.to_string()),
        },
    }
}

fn chain_matches(network: Network, chain: &str) -> bool {
    let c = chain.to_lowercase();
    match network {
        Network::Mainnet => c.contains("main"),
        Network::Testnet => c.contains("test") && !c.contains("regtest"),
        Network::Regtest => c.contains("regtest") || c == "test",
    }
}

/// Probe loopback RPC candidates. Prefers Zakura subversion, then higher height.
pub fn pick_local_validator(network: Network) -> Option<ValidatorProbe> {
    let mut best: Option<(i64, ValidatorProbe)> = None;
    for url in LightServer::validator_rpc_candidates(network) {
        let p = probe_validator(url);
        if !p.ok || !chain_matches(network, &p.chain) {
            continue;
        }
        let mut score = i64::from(p.height.unwrap_or(0));
        if p.subversion
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("zakura")
        {
            score += 1_000_000_000;
        }
        match &best {
            Some((s, _)) if *s >= score => {}
            _ => best = Some((score, p)),
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_response_peer(response: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let length = socket.read(&mut chunk).unwrap();
                assert!(length > 0);
                request.extend_from_slice(&chunk[..length]);
                assert!(request.len() <= 4096);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let body = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= end + 4 + body {
                        break;
                    }
                }
            }
            // Overflow refusal intentionally closes the socket before all bytes
            // have arrived. Never print response bytes or private fixture errors.
            let _ = socket.write_all(&response);
        });
        (url, peer)
    }

    #[test]
    fn raw_transaction_socket_response_is_bounded_for_hex_and_verbose() {
        for verbose in [false, true] {
            let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            response.extend(vec![b'x'; RAW_TRANSACTION_RESPONSE_LIMIT + 1]);
            let (url, peer) = raw_response_peer(response);
            let error = if verbose {
                get_raw_transaction_verbose(&url, "disposable-fixture-id").map(|_| ())
            } else {
                get_raw_transaction_hex(&url, "disposable-fixture-id").map(|_| ())
            }
            .unwrap_err();
            peer.join().unwrap();
            assert!(
                matches!(error, EngineError::Transport(message) if message == "getrawtransaction response limit")
            );
        }
    }

    #[test]
    fn bounded_raw_response_errors_never_include_response_contents_and_preserve_misses() {
        for body in [
            "{private-fixture-details",
            r#"{"result":{"private":"private-fixture-details"},"error":null}"#,
            r#"{"result":"private-fixture-details","error":null}"#,
            r#"{"error":{"code":-5,"message":"private-fixture-details"}}"#,
            r#"{"private":"private-fixture-details"}"#,
        ] {
            for verbose in [false, true] {
                let response =
                    format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}").into_bytes();
                let (url, peer) = raw_response_peer(response);
                let error = if verbose {
                    get_raw_transaction_verbose(&url, "disposable-fixture-id").map(|_| ())
                } else {
                    get_raw_transaction_hex(&url, "disposable-fixture-id").map(|_| ())
                }
                .unwrap_err();
                peer.join().unwrap();
                assert!(!error.to_string().contains("private-fixture-details"));
                if body.contains("-5") {
                    assert!(is_unknown_transaction(&error));
                }
            }
        }
        let (url, peer) = raw_response_peer(b"private-fixture-details".to_vec());
        let error = get_raw_transaction_hex(&url, "disposable-fixture-id").unwrap_err();
        peer.join().unwrap();
        assert!(
            matches!(error, EngineError::Transport(message) if message == "getrawtransaction response body missing")
        );
        let (url,peer) = raw_response_peer(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"result\":{\"hex\":\"00ab\",\"height\":404},\"error\":null}".to_vec());
        let result = get_raw_transaction_verbose(&url, "disposable-fixture-id").unwrap();
        peer.join().unwrap();
        assert_eq!(result, ("00ab".into(), Some(404)));
    }

    #[test]
    fn only_a_request_that_never_left_counts_as_not_sent() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let refused =
            send_raw_transaction(&format!("http://127.0.0.1:{port}"), &[1, 2]).unwrap_err();
        assert!(never_sent(&refused), "{refused}");
        // Accepted, then dropped without an answer: the node may hold it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = std::io::Read::read(&mut stream, &mut buf);
        });
        let lost = send_raw_transaction(&url, &[1, 2]).unwrap_err();
        peer.join().unwrap();
        assert!(!never_sent(&lost), "{lost}");
    }

    #[test]
    fn hex_encode() {
        assert_eq!(to_hex(&[0xde, 0xad]), "dead");
    }

    #[test]
    fn parse_url() {
        for (url, host, port, authority) in [
            (
                "http://127.0.0.1:29232",
                "127.0.0.1",
                29232,
                "127.0.0.1:29232",
            ),
            (" localhost:28232/ ", "localhost", 28232, "localhost:28232"),
            ("http://[::1]:8232/", "::1", 8232, "[::1]:8232"),
            (
                "[2001:db8::1]:8232",
                "2001:db8::1",
                8232,
                "[2001:db8::1]:8232",
            ),
        ] {
            assert_eq!(parse_host_port(url).unwrap(), (host.into(), port), "{url}");
            assert_eq!(host_header(host, port), authority);
        }
        for url in [
            "https://127.0.0.1:8232",
            "ftp://127.0.0.1:8232",
            "http://localhost",
            "http://localhost:65536",
            "http://user:pass@localhost:8232",
            "http://localhost:8232/rpc",
            "http://localhost:8232/?query=1",
            "http://:8232",
        ] {
            assert!(
                parse_host_port(url).is_err(),
                "accepted unsupported URL {url}"
            );
        }
        assert!(parse_host_port("https://127.0.0.1:8232")
            .unwrap_err()
            .to_string()
            .contains("HTTPS requires a TLS transport"));
    }

    #[test]
    fn ipv6_loopback_rpc_uses_bracketed_http_authority() {
        let listener = match std::net::TcpListener::bind("[::1]:0") {
            Ok(listener) => listener,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("IPv6 loopback listener: {error}"),
        };
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(peer) => break peer,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    other => panic!("RPC did not reach IPv6 loopback: {other:?}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .map(|length| length.parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.contains(&format!("\r\nHost: {addr}\r\n")));
            let body = r#"{"result":123,"error":null,"id":1}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let result = jsonrpc(
            &format!("http://{addr}"),
            "getblockcount",
            serde_json::json!([]),
        );
        peer.join().unwrap();
        assert_eq!(result.unwrap(), serde_json::json!(123));
    }

    #[test]
    fn probe_validator_unreachable_is_not_ok() {
        let p = probe_validator("http://127.0.0.1:1");
        assert!(!p.ok);
        assert!(p.error.is_some());
    }
}
