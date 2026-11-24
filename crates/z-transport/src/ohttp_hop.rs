//! RFC 9458 Oblivious HTTP (`OhttpHop<H>`) composable over ANY inner [`NetworkHop`].
//!
//! Because `OhttpHop` wraps an inner `NetworkHop`, callers compose any combination cleanly:
//! - `OhttpHop::new(DirectHop)`  -> standard OHTTP (Relay sees IP; Gateway sees content)
//! - `OhttpHop::new(Socks5Hop)`  -> Nym/Tor SOCKS5h + OHTTP (Relay sees mixnet/Tor exit!)
//! - `OhttpHop::new(TorHop)`     -> Embedded Arti Tor + OHTTP (Zodl/Vizor-grade!)
//!
//! Supports two wire dialects:
//! - [`OhttpDialect::Rfc9458Bhttp`]: Standard RFC 9458 HPKE + Binary HTTP (`bhttp`),
//!   interoperable with `zodl-ohttp` and any RFC 9458 OHTTP relay + gateway.
//! - [`OhttpDialect::CofferSigned`]: Fixed-bucket padded BHTTP framing compatible with
//!   `coffer-sync-origin` (`/sync-ohttp`, `/wallet-ohttp`, `/transparent-ohttp`, `/ext-ohttp`).

use crate::{HopKind, HopRequest, HopResponse, NetworkHop, TransportError};
use async_trait::async_trait;
use bhttp::{ControlData, Message, Mode};
use ohttp::{ClientRequest, KeyConfig};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

/// Wire framing dialect used inside the HPKE-encrypted OHTTP payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OhttpDialect {
    /// Standard RFC 9458 Binary HTTP (`bhttp`) over HPKE (`zodl-ohttp` and standard gateways).
    Rfc9458Bhttp,
    /// Coffer signed-publisher fixed-bucket padded BHTTP framing.
    CofferSigned { request_frame_bytes: usize },
}

/// Configuration for [`OhttpHop`].
#[derive(Debug, Clone)]
pub struct OhttpHopConfig {
    /// Exact OHTTP relay endpoint URL (e.g. `https://cloud.cofferwallet.com/ext-ohttp`
    /// or `https://relay.example/relay`).
    pub relay_url: String,
    /// Default authority (`host[:port]`) for inner BHTTP requests when `HopRequest.target`
    /// is a path (e.g. `/ext/price/zec-usd` or `/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock`).
    pub default_authority: String,
    /// Default scheme for inner BHTTP requests (`https`).
    pub default_scheme: String,
    /// Validated RFC 9458 OHTTP KeyConfig bytes (single key config or key-config list).
    pub key_config: Vec<u8>,
    pub dialect: OhttpDialect,
    pub label: String,
}

/// Composable RFC 9458 OHTTP hop wrapping any inner [`NetworkHop`].
pub struct OhttpHop<H: NetworkHop> {
    inner: Arc<H>,
    relay_url: String,
    default_authority: String,
    default_scheme: String,
    dialect: OhttpDialect,
    label: String,
    key_config: RwLock<KeyConfig>,
    closed: AtomicBool,
}

fn decode_key_config(bytes: &[u8]) -> Result<KeyConfig, TransportError> {
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(TransportError::Ohttp("key_config_size_invalid"));
    }
    let candidates = match KeyConfig::decode(bytes) {
        Ok(single) => vec![single],
        Err(_) => KeyConfig::decode_list(bytes)
            .map_err(|_| TransportError::Ohttp("key_config_decode_failed"))?,
    };
    for cfg in candidates {
        // Verify this key config actually carries a supported HPKE ciphersuite.
        if ClientRequest::from_config(&mut cfg.clone()).is_ok() {
            return Ok(cfg);
        }
    }
    Err(TransportError::Ohttp("key_config_unsupported_suite"))
}

fn validate_relay_url(raw: &str) -> Result<String, TransportError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 || trimmed.chars().any(char::is_control) {
        return Err(TransportError::InvalidRequest("ohttp_relay_url_invalid"));
    }
    if let Some(rest) = trimmed.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() || host.contains('@') {
            return Err(TransportError::InvalidRequest("ohttp_relay_url_invalid"));
        }
        return Ok(trimmed.to_owned());
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return Err(TransportError::InvalidRequest("ohttp_relay_url_invalid"));
        }
        let is_loopback = authority.starts_with("127.0.0.1:")
            || authority == "127.0.0.1"
            || authority.starts_with("[::1]:")
            || authority == "[::1]"
            || authority.starts_with("localhost:")
            || authority == "localhost";
        if is_loopback {
            return Ok(trimmed.to_owned());
        }
    }
    Err(TransportError::InvalidRequest("ohttp_relay_url_invalid"))
}

impl<H: NetworkHop> OhttpHop<H> {
    pub fn new(config: OhttpHopConfig, inner: Arc<H>) -> Result<Self, TransportError> {
        let relay_url = validate_relay_url(&config.relay_url)?;
        if config.default_authority.trim().is_empty() {
            return Err(TransportError::InvalidRequest("ohttp_authority_missing"));
        }
        let key_config = decode_key_config(&config.key_config)?;
        let label = if config.label.trim().is_empty() {
            format!("OHTTP via {}", inner.label())
        } else {
            config.label
        };
        Ok(Self {
            inner,
            relay_url,
            default_authority: config.default_authority.trim().to_owned(),
            default_scheme: if config.default_scheme.trim().is_empty() {
                "https".into()
            } else {
                config.default_scheme.trim().to_owned()
            },
            dialect: config.dialect,
            label,
            key_config: RwLock::new(key_config),
            closed: AtomicBool::new(false),
        })
    }

    /// Hot-rotate the gateway HPKE key configuration after verifying a fresh signed key document.
    pub fn update_key_config(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let parsed = decode_key_config(bytes)?;
        let mut guard = self
            .key_config
            .write()
            .map_err(|_| TransportError::Ohttp("key_lock_poisoned"))?;
        *guard = parsed;
        Ok(())
    }

    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    pub fn inner(&self) -> &Arc<H> {
        &self.inner
    }

    fn encode_bhttp(&self, req: &HopRequest) -> Result<Vec<u8>, TransportError> {
        let (scheme, authority, path) = if req.target.starts_with('/') {
            (
                self.default_scheme.as_bytes().to_vec(),
                self.default_authority.as_bytes().to_vec(),
                req.target.as_bytes().to_vec(),
            )
        } else {
            let (scheme_str, rest) = req
                .target
                .split_once("://")
                .ok_or(TransportError::InvalidRequest("ohttp_target_invalid"))?;
            let (auth_str, path_str) = match rest.find('/') {
                Some(idx) => (&rest[..idx], &rest[idx..]),
                None => (rest, "/"),
            };
            (
                scheme_str.as_bytes().to_vec(),
                auth_str.as_bytes().to_vec(),
                path_str.as_bytes().to_vec(),
            )
        };
        let mut msg = Message::request(req.method.as_bytes().to_vec(), scheme, authority, path);
        for (name, value) in &req.headers {
            // Strip ambient identity headers defensively even inside OHTTP.
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "cookie" | "origin" | "referer" | "x-forwarded-for" | "cf-connecting-ip"
            ) {
                continue;
            }
            msg.put_header(lower.into_bytes(), value.as_bytes().to_vec());
        }
        if !req.body.is_empty() {
            msg.write_content(&req.body);
        }
        let mut plain = Vec::new();
        msg.write_bhttp(Mode::KnownLength, &mut plain)
            .map_err(|_| TransportError::Ohttp("bhttp_encode_failed"))?;

        if let OhttpDialect::CofferSigned {
            request_frame_bytes,
        } = self.dialect
        {
            // Match coffer-cloud-client's exact padded BHTTP wire format (no length prefix;
            // canonical zero padding to fixed bucket size).
            if plain.len() > request_frame_bytes {
                return Err(TransportError::InvalidRequest("ohttp_request_too_large"));
            }
            plain.resize(request_frame_bytes, 0);
            return Ok(plain);
        }
        Ok(plain)
    }

    fn decode_bhttp(&self, raw: &[u8]) -> Result<HopResponse, TransportError> {
        let mut cursor = Cursor::new(raw);
        let msg = Message::read_bhttp(&mut cursor)
            .map_err(|_| TransportError::Ohttp("bhttp_decode_failed"))?;
        let consumed = cursor.position() as usize;
        let trailing = raw.get(consumed..).unwrap_or(&[]);
        match self.dialect {
            OhttpDialect::Rfc9458Bhttp => {
                if !trailing.is_empty() && !trailing.iter().all(|b| *b == 0) {
                    return Err(TransportError::Ohttp("bhttp_trailing_garbage"));
                }
            }
            OhttpDialect::CofferSigned { .. } => {
                // Canonical Coffer padded BHTTP requires all trailing padding bytes to be zero.
                if !trailing.iter().all(|b| *b == 0) {
                    return Err(TransportError::Ohttp("bhttp_non_zero_padding"));
                }
            }
        }
        let status = match msg.control() {
            ControlData::Response(code) => u16::from(*code),
            _ => return Err(TransportError::Ohttp("bhttp_expected_response")),
        };
        let headers = msg
            .header()
            .fields()
            .iter()
            .filter_map(|f| {
                let k = std::str::from_utf8(f.name()).ok()?.to_owned();
                let v = std::str::from_utf8(f.value()).ok()?.to_owned();
                Some((k, v))
            })
            .collect();
        Ok(HopResponse {
            status,
            headers,
            body: msg.content().to_vec(),
        })
    }
}

#[async_trait]
impl<H: NetworkHop + 'static> NetworkHop for OhttpHop<H> {
    fn kind(&self) -> HopKind {
        HopKind::Ohttp
    }
    fn label(&self) -> &str {
        &self.label
    }
    async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let plain = self.encode_bhttp(&req)?;
        let mut key_cfg = self
            .key_config
            .read()
            .map_err(|_| TransportError::Ohttp("key_lock_poisoned"))?
            .clone();
        let client_req = ClientRequest::from_config(&mut key_cfg)
            .map_err(|_| TransportError::Ohttp("hpke_client_init_failed"))?;
        let (enc_req, client_resp) = client_req
            .encapsulate(&plain)
            .map_err(|_| TransportError::Ohttp("hpke_encapsulate_failed"))?;

        let mut outer = HopRequest::post(
            self.relay_url.clone(),
            "message/ohttp-req",
            enc_req,
            req.role,
        )
        .with_header("accept", "message/ohttp-res")
        .with_max_response_bytes(req.max_response_bytes.saturating_add(4096))
        .with_timeout_ms(req.timeout_ms);
        // Never allow an inner transport to replay an already-sealed HPKE envelope;
        // outer retries must re-enter OhttpHop::exchange and reseal with fresh entropy.
        outer.idempotent = false;

        let outer_resp = self.inner.exchange(outer).await?;
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        if outer_resp.status != 200 {
            if (400..500).contains(&outer_resp.status) {
                return Err(TransportError::FailClosed("ohttp_relay_client_error"));
            }
            return Err(TransportError::Unavailable("ohttp_relay_non_200"));
        }
        let dec_resp = client_resp
            .decapsulate(&outer_resp.body)
            .map_err(|_| TransportError::Ohttp("hpke_decapsulate_failed"))?;
        self.decode_bhttp(&dec_resp)
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.inner.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HopRole;
    use ohttp::{
        hpke::{Aead, Kdf, Kem},
        Server, SymmetricSuite,
    };
    use std::sync::Mutex;

    struct InProcessOhttpGateway {
        server: Server,
        saw_inner_target: Mutex<Option<String>>,
        saw_outer_role: Mutex<Option<HopRole>>,
        coffer_framed: bool,
    }

    #[async_trait]
    impl NetworkHop for InProcessOhttpGateway {
        fn kind(&self) -> HopKind {
            HopKind::Tor
        }
        fn label(&self) -> &str {
            "mock-tor-to-ohttp-gateway"
        }
        async fn exchange(&self, req: HopRequest) -> Result<HopResponse, TransportError> {
            *self.saw_outer_role.lock().unwrap() = Some(req.role);
            assert_eq!(req.header_val("content-type"), Some("message/ohttp-req"));
            let (plain, server_resp) = self.server.decapsulate(&req.body).unwrap();
            let mut cur = Cursor::new(plain.as_slice());
            let inner = Message::read_bhttp(&mut cur).unwrap();
            if self.coffer_framed {
                let consumed = cur.position() as usize;
                assert!(plain[consumed..].iter().all(|b| *b == 0));
            }
            if let ControlData::Request { path, .. } = inner.control() {
                let path = std::str::from_utf8(path).unwrap().to_owned();
                *self.saw_inner_target.lock().unwrap() = Some(path);
            }
            // Verify ambient headers were stripped.
            assert!(inner
                .header()
                .fields()
                .iter()
                .all(|f| f.name() != b"cookie" && f.name() != b"x-forwarded-for"));

            let mut reply = Message::response(bhttp::StatusCode::try_from(200u16).unwrap());
            reply.put_header(b"content-type".to_vec(), b"application/json".to_vec());
            reply.write_content(b"{\"ok\":true}");
            let mut reply_bytes = Vec::new();
            reply
                .write_bhttp(Mode::KnownLength, &mut reply_bytes)
                .unwrap();
            let payload = if self.coffer_framed {
                reply_bytes.resize(1024, 0);
                reply_bytes
            } else {
                reply_bytes
            };
            let enc = server_resp.encapsulate(&payload).unwrap();
            Ok(HopResponse {
                status: 200,
                headers: vec![("content-type".into(), "message/ohttp-res".into())],
                body: enc,
            })
        }
    }

    impl HopRequest {
        fn header_val(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    #[tokio::test]
    async fn ohttp_hop_composes_over_tor_or_nym_and_roundtrips_both_dialects() {
        for dialect in [
            OhttpDialect::Rfc9458Bhttp,
            OhttpDialect::CofferSigned {
                request_frame_bytes: 1024,
            },
        ] {
            let key = KeyConfig::new(
                1,
                Kem::X25519Sha256,
                vec![SymmetricSuite::new(Kdf::HkdfSha256, Aead::Aes128Gcm)],
            )
            .unwrap();
            let key_bytes = key.encode().unwrap();
            let gw = Arc::new(InProcessOhttpGateway {
                server: Server::new(key).unwrap(),
                saw_inner_target: Mutex::new(None),
                saw_outer_role: Mutex::new(None),
                coffer_framed: matches!(dialect, OhttpDialect::CofferSigned { .. }),
            });
            let hop = OhttpHop::new(
                OhttpHopConfig {
                    relay_url: "https://relay.example/relay".into(),
                    default_authority: "gateway.example".into(),
                    default_scheme: "https".into(),
                    key_config: key_bytes,
                    dialect,
                    label: "Tor+OHTTP".into(),
                },
                gw.clone(),
            )
            .unwrap();

            let req = HopRequest::get("/v1/enhance/init", HopRole::Pir)
                .with_header("cookie", "should-be-stripped")
                .with_header("x-forwarded-for", "1.2.3.4");
            let resp = hop.exchange(req).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body, b"{\"ok\":true}");
            assert_eq!(
                gw.saw_inner_target.lock().unwrap().as_deref(),
                Some("/v1/enhance/init")
            );
            assert_eq!(*gw.saw_outer_role.lock().unwrap(), Some(HopRole::Pir));
        }
    }
}
