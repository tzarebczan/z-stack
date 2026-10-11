//! A validator session whose chain identity is checked before broadcast.

use super::{host_header, parse_host_port, to_hex};
use crate::error::{EngineError, Result};
use crate::Network;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const HEADER_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = 64 * 1024;
const FRAMING_LIMIT: usize = 64 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// The verified socket is retained across the durable receipt transition.
/// No method on this session resolves or reconnects the configured endpoint.
pub(in crate::native) struct VerifiedValidator {
    socket: BufReader<TcpStream>,
    authority: String,
    next_id: u64,
    deadline: Instant,
    sent: bool,
}

fn transport(message: &str) -> EngineError {
    EngineError::Transport(format!("validator verified rpc {message}"))
}

impl VerifiedValidator {
    pub(in crate::native) fn connect(url: &str, network: Network) -> Result<Self> {
        let (host, port) = parse_host_port(url)?;
        let addr = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| EngineError::Transport(format!("zebra rpc resolve {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| {
                EngineError::Transport(format!("zebra rpc no addr for {host}:{port}"))
            })?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .map_err(|e| EngineError::Transport(format!("zebra rpc connect {host}:{port}: {e}")))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .map_err(|_| transport("write timeout setup failed"))?;
        let mut connection = Self {
            socket: BufReader::new(stream),
            authority: host_header(&host, port),
            next_id: 1,
            deadline: Instant::now() + RESPONSE_TIMEOUT,
            sent: false,
        };
        let info = connection.exchange("getblockchaininfo", serde_json::json!([]), true)?;
        let chain = info
            .get("chain")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| transport("chain missing"))?;
        let genesis = connection.exchange("getblockhash", serde_json::json!([0]), true)?;
        let genesis = genesis
            .as_str()
            .ok_or_else(|| transport("genesis missing"))?;
        crate::native::chain_identity::validate(network, chain, genesis)?;
        Ok(connection)
    }

    pub(in crate::native) fn send(&mut self, raw: &[u8]) -> Result<String> {
        if self.sent {
            return Err(transport("session already submitted"));
        }
        self.sent = true;
        let result = self.exchange(
            "sendrawtransaction",
            serde_json::json!([to_hex(raw)]),
            false,
        )?;
        let txid = result
            .as_str()
            .filter(|txid| txid.len() == 64 && txid.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| transport("transaction id invalid"))?;
        Ok(txid.to_owned())
    }

    fn exchange(
        &mut self,
        method: &str,
        params: serde_json::Value,
        require_persistent: bool,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
        .to_string();
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            self.authority,
            body.len()
        );
        self.socket
            .get_mut()
            .write_all(request.as_bytes())
            .map_err(|_| transport("request write failed"))?;
        self.deadline = Instant::now() + RESPONSE_TIMEOUT;
        let (success_status, body) = self.read_response(require_persistent)?;
        let reply: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| transport("response JSON invalid"))?;
        if reply.get("id").and_then(serde_json::Value::as_u64) != Some(id)
            || !reply.is_object()
            || reply
                .get("jsonrpc")
                .is_some_and(|version| version.as_str() != Some("2.0"))
        {
            return Err(transport("response envelope invalid"));
        }
        if let Some(error) = reply.get("error").filter(|error| !error.is_null()) {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .ok_or_else(|| transport("response error invalid"))?;
            let remote = EngineError::BroadcastRejected {
                code,
                message: error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            };
            let already_known = method == "sendrawtransaction"
                && crate::native::wallet::already_known_transaction(&remote);
            return Err(EngineError::BroadcastRejected {
                code,
                message: if already_known {
                    "transaction already exists in mempool".into()
                } else {
                    format!("{method} RPC rejected")
                },
            });
        }
        if !success_status {
            return Err(transport("response HTTP status invalid"));
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| transport("response result missing"))
    }

    fn set_read_deadline(&self) -> Result<()> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(transport("response read timeout"));
        }
        self.socket
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|_| transport("read timeout setup failed"))
    }

    fn read_line(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            self.set_read_deadline()?;
            let available = self
                .socket
                .fill_buf()
                .map_err(|_| transport("response read failed"))?;
            if available.is_empty() {
                return Err(transport("response truncated"));
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if count > limit.saturating_sub(line.len()) {
                return Err(transport("response framing limit"));
            }
            line.extend_from_slice(&available[..count]);
            self.socket.consume(count);
            if line.last() == Some(&b'\n') {
                if !line.ends_with(b"\r\n") {
                    return Err(transport("response line invalid"));
                }
                return Ok(line);
            }
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            self.set_read_deadline()?;
            let count = self
                .socket
                .read(remaining)
                .map_err(|_| transport("response read failed"))?;
            if count == 0 {
                return Err(transport("response truncated"));
            }
            remaining = &mut remaining[count..];
        }
        Ok(())
    }

    fn read_response(&mut self, require_persistent: bool) -> Result<(bool, Vec<u8>)> {
        let mut head = Vec::new();
        loop {
            let line = self.read_line(HEADER_LIMIT.saturating_sub(head.len()))?;
            let finished = line == b"\r\n";
            head.extend(line);
            if finished {
                break;
            }
        }
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&head) {
            Ok(httparse::Status::Complete(length)) if length == head.len() => {}
            _ => return Err(transport("response headers invalid")),
        }
        if response.version != Some(1) || !matches!(response.code, Some(200 | 400 | 404 | 500)) {
            return Err(transport("response HTTP status invalid"));
        }
        let success_status = response.code == Some(200);
        let mut length = None;
        let mut chunked = false;
        for header in response.headers {
            let value = std::str::from_utf8(header.value)
                .map_err(|_| transport("response header invalid"))?
                .trim();
            if header.name.eq_ignore_ascii_case("content-length") {
                // Reject ambiguous framing, including duplicate identical lengths.
                if length.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(transport("response content length invalid"));
                }
                length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| transport("response content length invalid"))?,
                );
            } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(transport("response transfer encoding invalid"));
                }
                chunked = true;
            } else if header.name.eq_ignore_ascii_case("connection")
                && require_persistent
                && value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close"))
            {
                return Err(transport("response closed verified session"));
            } else if header.name.eq_ignore_ascii_case("content-encoding")
                && !value.eq_ignore_ascii_case("identity")
            {
                return Err(transport("response content encoding unsupported"));
            }
        }
        if chunked && length.is_some() {
            return Err(transport("response framing ambiguous"));
        }
        if chunked {
            self.read_chunks().map(|body| (success_status, body))
        } else if let Some(length) = length {
            if length > BODY_LIMIT {
                return Err(transport("response body limit"));
            }
            let mut body = vec![0; length];
            self.read_exact(&mut body)?;
            Ok((success_status, body))
        } else {
            Err(transport("response framing missing"))
        }
    }

    fn read_chunks(&mut self) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        let mut framing = 0;
        loop {
            let line = self.read_line(FRAMING_LIMIT.saturating_sub(framing))?;
            framing += line.len();
            if !line.first().is_some_and(u8::is_ascii_hexdigit) {
                return Err(transport("response chunk size invalid"));
            }
            let size = match httparse::parse_chunk_size(&line) {
                Ok(httparse::Status::Complete((length, size))) if length == line.len() => {
                    usize::try_from(size).map_err(|_| transport("response chunk size invalid"))?
                }
                _ => return Err(transport("response chunk size invalid")),
            };
            if size > BODY_LIMIT.saturating_sub(body.len()) {
                return Err(transport("response body limit"));
            }
            if size == 0 {
                let mut trailers = Vec::new();
                loop {
                    let line = self.read_line(FRAMING_LIMIT.saturating_sub(framing))?;
                    framing += line.len();
                    let finished = line == b"\r\n";
                    trailers.extend(line);
                    if finished {
                        break;
                    }
                }
                let mut trailer_headers = [httparse::EMPTY_HEADER; 64];
                match httparse::parse_headers(&trailers, &mut trailer_headers) {
                    Ok(httparse::Status::Complete((length, headers)))
                        if length == trailers.len()
                            && headers.iter().all(|header| {
                                !["content-length", "transfer-encoding", "connection"]
                                    .iter()
                                    .any(|name| header.name.eq_ignore_ascii_case(name))
                            }) => {}
                    _ => return Err(transport("response trailers invalid")),
                }
                return Ok(body);
            }
            let start = body.len();
            body.resize(start + size, 0);
            self.read_exact(&mut body[start..])?;
            let mut end = [0; 2];
            self.read_exact(&mut end)?;
            framing += end.len();
            if end != *b"\r\n" || framing > FRAMING_LIMIT {
                return Err(transport("response chunk ending invalid"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    const REGTEST_GENESIS: &str =
        "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";
    const TESTNET_GENESIS: &str =
        "05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38";
    const TXID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn content_length(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn reply(id: u64, result: Value) -> Vec<u8> {
        content_length(&json!({"id": id, "result": result, "error": null}).to_string())
    }

    fn chunked_reply(id: u64, result: Value) -> Vec<u8> {
        let body = json!({"id": id, "result": result, "error": null}).to_string();
        let (first, second) = body.split_at(body.len() / 2);
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x};fixture=true\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\nX-Fixture: done\r\n\r\n",
            first.len(), second.len()
        )
        .into_bytes()
    }

    fn read_request(socket: &mut BufReader<TcpStream>) -> Option<Value> {
        let mut first = String::new();
        let count = match socket.read_line(&mut first) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return None,
            Err(error) => panic!("fixture request read failed: {error}"),
        };
        if count == 0 {
            return None;
        }
        assert_eq!(first, "POST / HTTP/1.1\r\n");
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(socket.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.unwrap();
        assert!(length < 4096);
        let mut body = vec![0; length];
        socket.read_exact(&mut body).unwrap();
        Some(serde_json::from_slice(&body).unwrap())
    }

    /// A single accept proves all requests use the initially verified socket.
    fn peer(responses: Vec<Vec<u8>>) -> (String, JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = BufReader::new(stream);
            let mut requests = Vec::new();
            for response in responses {
                let Some(request) = read_request(&mut socket) else {
                    break;
                };
                requests.push(request);
                if socket.get_mut().write_all(&response).is_err() {
                    return requests;
                }
            }
            // A refused probe must drop the socket without submitting bytes.
            if let Some(request) = read_request(&mut socket) {
                requests.push(request);
            }
            requests
        });
        (url, handle)
    }

    fn assert_refused(responses: Vec<Vec<u8>>) -> Vec<Value> {
        let (url, handle) = peer(responses);
        let error = super::super::send_raw_transaction_verified(
            &url,
            Network::Regtest,
            &[0xfa, 0xce, 0xb0, 0x0c],
        )
        .unwrap_err();
        assert!(!super::super::never_sent(&error));
        assert!(!error.to_string().contains("private-fixture-details"));
        let requests = handle.join().unwrap();
        assert!(requests.iter().all(|request| {
            request["method"] != "sendrawtransaction" && !request.to_string().contains("faceb00c")
        }));
        requests
    }

    #[test]
    fn verified_validator_refuses_wrong_chain_or_genesis_before_send() {
        for (chain, genesis) in [
            ("main", REGTEST_GENESIS),
            ("regtest-private-fixture-details", REGTEST_GENESIS),
            ("regtest", TESTNET_GENESIS),
            ("test", TESTNET_GENESIS),
            ("regtest", "private-fixture-details"),
        ] {
            let requests = assert_refused(vec![
                reply(1, json!({"chain": chain})),
                reply(2, json!(genesis)),
            ]);
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["method"], "getblockchaininfo");
            assert_eq!(requests[1]["method"], "getblockhash");
            assert_eq!(requests[1]["params"], json!([0]));
        }
    }

    #[test]
    fn verified_validator_uses_one_socket_for_ordered_probes_and_send() {
        for chain in ["regtest", "test"] {
            for chunked in [false, true] {
                let make_reply = if chunked { chunked_reply } else { reply };
                let (url, handle) = peer(vec![
                    make_reply(1, json!({"chain": chain})),
                    make_reply(2, json!(REGTEST_GENESIS)),
                    make_reply(3, json!(TXID)),
                ]);
                let txid = super::super::send_raw_transaction_verified(
                    &url,
                    Network::Regtest,
                    &[0xfa, 0xce, 0xb0, 0x0c],
                )
                .unwrap();
                assert_eq!(txid, TXID);
                let requests = handle.join().unwrap();
                assert_eq!(requests.len(), 3);
                assert_eq!(requests[0]["method"], "getblockchaininfo");
                assert_eq!(requests[1]["method"], "getblockhash");
                assert_eq!(requests[2]["method"], "sendrawtransaction");
                assert_eq!(requests[2]["params"], json!(["faceb00c"]));
                assert_eq!(requests[2]["id"], json!(3));
            }
        }
    }

    #[test]
    fn verified_validator_refuses_malformed_probe_envelopes() {
        for response in [
            content_length("{private-fixture-details"),
            content_length(r#"{"id":99,"result":{"chain":"regtest"},"error":null}"#),
            content_length(r#"{"id":1,"result":{"chain":123},"error":null}"#),
            content_length(r#"{"id":1,"jsonrpc":"1.0","result":{"chain":"regtest"}}"#),
            content_length(r#"{"id":1,"error":{"code":-1,"message":"private-fixture-details"}}"#),
            reply(1, json!({})),
        ] {
            assert_eq!(assert_refused(vec![response]).len(), 1);
        }
        for genesis in [json!(null), json!(17), json!({"hash": REGTEST_GENESIS})] {
            assert_eq!(
                assert_refused(vec![
                    reply(1, json!({"chain":"regtest"})),
                    reply(2, genesis)
                ])
                .len(),
                2
            );
        }
    }

    #[test]
    fn verified_validator_refuses_ambiguous_or_unbounded_http_framing() {
        for response in [
            b"HTTP/1.1 200 OK\r\n\r\nprivate-fixture-details".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec(),
            b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 500 Failure\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                BODY_LIMIT + 1
            )
            .into_bytes(),
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                BODY_LIMIT + 1
            )
            .into_bytes(),
            format!(
                "HTTP/1.1 200 OK\r\nX-Fixture: {}\r\n\r\n",
                "x".repeat(HEADER_LIMIT)
            )
            .into_bytes(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 2\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx!!".to_vec(),
        ] {
            assert_eq!(assert_refused(vec![response]).len(), 1);
        }
    }

    #[test]
    fn verified_validator_never_reconnects_after_probe_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = BufReader::new(stream);
            assert_eq!(
                read_request(&mut socket).unwrap()["method"],
                "getblockchaininfo"
            );
            socket
                .get_mut()
                .write_all(&reply(1, json!({"chain":"regtest"})))
                .unwrap();
            assert_eq!(read_request(&mut socket).unwrap()["method"], "getblockhash");
            // The genesis response is truncated and the listener stays alive.
            socket
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{}")
                .unwrap();
            drop(socket);
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let error = super::super::send_raw_transaction_verified(&url, Network::Regtest, &[1, 2])
            .unwrap_err();
        assert!(!super::super::never_sent(&error));
        let listener = handle.join().unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn verified_validator_reports_connect_refusal_as_never_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error =
            super::super::send_raw_transaction_verified(&url, Network::Regtest, &[1]).unwrap_err();
        assert!(super::super::never_sent(&error));
    }

    #[test]
    fn verified_validator_preserves_duplicate_acknowledgements_without_error_payloads() {
        for (message, already_known) in [
            ("txn-already-in-mempool: private-fixture-details", true),
            (
                "transaction dropped because it is already queued for download: private-fixture-details",
                true,
            ),
            ("nullifier already known: private-fixture-details", false),
            (
                "conflicts with a transaction already in the mempool: private-fixture-details",
                false,
            ),
        ] {
            for status in [200, 500] {
                let body = json!({"id":3,"result":null,"error":{"code":-26,"message":message}})
                    .to_string();
                let error_response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .into_bytes();
                let (url, handle) = peer(vec![
                    reply(1, json!({"chain":"regtest"})),
                    reply(2, json!(REGTEST_GENESIS)),
                    error_response,
                ]);
                let error =
                    super::super::send_raw_transaction_verified(&url, Network::Regtest, &[1])
                        .unwrap_err();
                assert!(matches!(
                    &error,
                    EngineError::BroadcastRejected { code: -26, .. }
                ));
                assert_eq!(
                    crate::native::wallet::already_known_transaction(&error),
                    already_known
                );
                assert!(!error.to_string().contains("private-fixture-details"));
                assert_eq!(handle.join().unwrap().len(), 3);
            }
        }
    }
}
