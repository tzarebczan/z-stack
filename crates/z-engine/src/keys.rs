//! Wasm-safe key derivation: mnemonic → USK/UFVK → unified + transparent addresses.
//!
//! No sqlite, gRPC, or proving. Native wallet and `z-wasm` both call this.

use crate::error::{EngineError, Result};
use crate::Network;
use bip39::Mnemonic;
use serde::{Deserialize, Serialize};
use zcash_address::{ConversionError, TryFromAddress, ZcashAddress};
use zcash_keys::{
    address::Address as ZkAddress,
    encoding::encode_transparent_address_p,
    keys::{
        AddressGenerationError, UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey,
    },
};
use zcash_protocol::consensus::NetworkType;
use zeroize::Zeroize;
use zip32::{AccountId, DiversifierIndex};

/// Well-known BIP-39 vector used as the compose faucet seed (regtest only, not money).
pub const REGTEST_FAUCET_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// Transparent P2PKH for [`REGTEST_FAUCET_MNEMONIC`] account 0 on regtest.
/// Must match `infra/compose/regtest/zebra.toml` `miner_address`.
pub const REGTEST_FAUCET_TRANSPARENT: &str = "tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6";

/// Viewing / receive surface for one ZIP-32 account. Spending key is not retained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountKeys {
    pub network: Network,
    pub account_index: u32,
    pub unified_address: String,
    pub ufvk: String,
    pub transparent_address: Option<String>,
}

/// Parsed address metadata (no spend capability).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAddress {
    pub network: Network,
    pub kind: AddressKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressKind {
    Unified,
    Sapling,
    P2pkh,
    P2sh,
    Tex,
    Sprout,
}

/// Receivers we put on a unified address. No transparent-only set (no t-spend).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UaReceiver {
    Orchard,
    Sapling,
    P2pkh,
}

impl UaReceiver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orchard => "orchard",
            Self::Sapling => "sapling",
            Self::P2pkh => "p2pkh",
        }
    }
}

/// Named receiver sets the apps actually generate.
///
/// * `full` — orchard + sapling + p2pkh (`AllAvailableKeys`). ZODL receive-incl-t.
/// * `orchard` — orchard only.
/// * `shielded` — orchard + sapling (no t).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UaReceiverSet {
    Full,
    Orchard,
    Shielded,
}

impl UaReceiverSet {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Orchard => "orchard",
            Self::Shielded => "shielded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "all" => Some(Self::Full),
            "orchard" => Some(Self::Orchard),
            "shielded" => Some(Self::Shielded),
            _ => None,
        }
    }

    pub fn receivers(self) -> &'static [UaReceiver] {
        match self {
            Self::Full => &[UaReceiver::Orchard, UaReceiver::Sapling, UaReceiver::P2pkh],
            Self::Orchard => &[UaReceiver::Orchard],
            Self::Shielded => &[UaReceiver::Orchard, UaReceiver::Sapling],
        }
    }

    pub fn includes_transparent(self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn request(self) -> UnifiedAddressRequest {
        match self {
            Self::Full => UnifiedAddressRequest::AllAvailableKeys,
            Self::Orchard => UnifiedAddressRequest::ORCHARD,
            Self::Shielded => UnifiedAddressRequest::SHIELDED,
        }
    }
}

/// Decode a UA / t-addr and list the receivers we understand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedAddress {
    pub network: Network,
    pub kind: AddressKind,
    pub receivers: Vec<UaReceiver>,
    pub receiver_set: Option<UaReceiverSet>,
}

pub fn classify_receiver_set(receivers: &[UaReceiver]) -> Option<UaReceiverSet> {
    let orchard = receivers.contains(&UaReceiver::Orchard);
    let sapling = receivers.contains(&UaReceiver::Sapling);
    let p2pkh = receivers.contains(&UaReceiver::P2pkh);
    match (orchard, sapling, p2pkh) {
        (true, true, true) => Some(UaReceiverSet::Full),
        (true, true, false) => Some(UaReceiverSet::Shielded),
        (true, false, false) => Some(UaReceiverSet::Orchard),
        _ => None,
    }
}

/// Encode a UA from a UFVK for one of the supported receiver sets.
pub fn unified_address_for_set(
    ufvk_str: &str,
    network: Network,
    set: UaReceiverSet,
) -> Result<String> {
    let ufvk = UnifiedFullViewingKey::decode(&network, ufvk_str.trim())
        .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;
    let (ua, _) = ufvk
        .default_address(set.request())
        .map_err(|e| EngineError::Message(format!("address: {e:?}")))?;
    Ok(ua.encode(&network))
}

/// What [`unified_address_at`] does when a requested Sapling receiver has no
/// valid diversifier at the chosen index (about half of all indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidDiversifier {
    /// Fail with an `invalid diversifier index` error.
    Reject,
    /// Use the first valid index at or above the chosen one.
    Next,
}

/// A UA and the diversifier index it was derived at. The index is decimal
/// because diversifier indices are 88-bit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressAtIndex {
    pub address: String,
    pub diversifier_index: String,
}

/// Transparent child indices are non-hardened BIP-32 indices.
const TRANSPARENT_INDEX_LIMIT: u128 = 1 << 31;

/// Encode the UA at diversifier index `index`. Pure: it reads no wallet state
/// and advances no address counter. Orchard and Sapling incoming viewing keys
/// detect notes at every diversifier, so scanning finds funds sent to it.
/// A transparent receiver needs `allow_transparent`: transparent scanning only
/// follows the gap limit from the last used index.
pub fn unified_address_at(
    ufvk_str: &str,
    network: Network,
    set: UaReceiverSet,
    index: u128,
    invalid: InvalidDiversifier,
    allow_transparent: bool,
) -> Result<AddressAtIndex> {
    if set.includes_transparent() {
        if !allow_transparent {
            return Err(EngineError::Message(
                "transparent receiver requires explicit opt-in".into(),
            ));
        }
        if index >= TRANSPARENT_INDEX_LIMIT {
            return Err(EngineError::Message(
                "invalid diversifier index: above the transparent child index range".into(),
            ));
        }
    }
    let start = DiversifierIndex::try_from(index).map_err(|_| {
        EngineError::Message("invalid diversifier index: above the 88-bit range".into())
    })?;
    let ufvk = UnifiedFullViewingKey::decode(&network, ufvk_str.trim())
        .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;
    // Require every receiver in the set. The `shielded` request used for the
    // default address allows Sapling, so it silently drops it at invalid indices.
    let request = {
        use zcash_keys::keys::ReceiverRequirement::{Omit, Require};
        let p2pkh = if set.includes_transparent() {
            Require
        } else {
            Omit
        };
        let sapling = if set == UaReceiverSet::Orchard {
            Omit
        } else {
            Require
        };
        UnifiedAddressRequest::custom(Require, sapling, p2pkh)
            .map_err(|e| EngineError::Message(format!("address: {e:?}")))?
    };
    let found = match invalid {
        InvalidDiversifier::Reject => ufvk.address(start, request).map(|ua| (ua, start)),
        InvalidDiversifier::Next => ufvk.find_address(start, request),
    };
    let (ua, at) = found.map_err(|e| match e {
        AddressGenerationError::InvalidSaplingDiversifierIndex(_) => EngineError::Message(
            "invalid diversifier index: no Sapling receiver at this index".into(),
        ),
        AddressGenerationError::DiversifierSpaceExhausted => EngineError::Message(
            "invalid diversifier index: no valid index at or above this one".into(),
        ),
        other => EngineError::Message(format!("address: {other:?}")),
    })?;
    let at = u128::from(at);
    if set.includes_transparent() && at >= TRANSPARENT_INDEX_LIMIT {
        return Err(EngineError::Message(
            "invalid diversifier index: above the transparent child index range".into(),
        ));
    }
    Ok(AddressAtIndex {
        address: ua.encode(&network),
        diversifier_index: at.to_string(),
    })
}

/// Receivers present on an encoded address (UA / sapling / t).
pub fn inspect_address(encoded: &str) -> Result<InspectedAddress> {
    let parsed = parse_address(encoded)?;
    let addr = ZcashAddress::try_from_encoded(encoded.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    let zk: ZkAddress = addr
        .convert()
        .map_err(|e| EngineError::Message(format!("address convert: {e}")))?;
    let mut receivers = Vec::new();
    match zk {
        ZkAddress::Unified(ua) => {
            if ua.orchard().is_some() {
                receivers.push(UaReceiver::Orchard);
            }
            if ua.sapling().is_some() {
                receivers.push(UaReceiver::Sapling);
            }
            if ua.transparent().is_some() {
                receivers.push(UaReceiver::P2pkh);
            }
        }
        ZkAddress::Sapling(_) => receivers.push(UaReceiver::Sapling),
        ZkAddress::Transparent(_) => receivers.push(UaReceiver::P2pkh),
        ZkAddress::Tex(_) => {}
    }
    Ok(InspectedAddress {
        network: parsed.network,
        kind: parsed.kind,
        receiver_set: classify_receiver_set(&receivers),
        receivers,
    })
}

/// 24-word English mnemonic from 32 bytes of CSPRNG entropy.
pub fn generate_mnemonic() -> Result<String> {
    let mut entropy = [0u8; 32];
    getrandom::getrandom(&mut entropy).map_err(|e| EngineError::Message(format!("rng: {e}")))?;
    let mnemonic = Mnemonic::from_entropy(&entropy)
        .map_err(|e| EngineError::Message(format!("mnemonic: {e}")))?;
    entropy.zeroize();
    Ok(mnemonic.to_string())
}

/// Original BIP-39 entropy for compact bearer gifts; never the derived seed.
/// Accept only canonical English phrases so serialization cannot change an account.
pub fn mnemonic_to_entropy(words: &str) -> Result<Vec<u8>> {
    let mnemonic = Mnemonic::parse_normalized(words)
        .map_err(|_| EngineError::Message("invalid gift recovery phrase".into()))?;
    if mnemonic.to_string() != words {
        return Err(EngineError::Message("invalid gift recovery phrase".into()));
    }
    Ok(mnemonic.to_entropy())
}

/// Reconstruct a compact gift's English phrase. Errors never contain the input.
pub fn mnemonic_from_entropy(entropy: &[u8]) -> Result<String> {
    Mnemonic::from_entropy(entropy)
        .map(|m| m.to_string())
        .map_err(|_| EngineError::Message("invalid gift entropy".into()))
}

/// Shielded-only Unified Address request: requires BOTH Orchard and Sapling, omits P2PKH.
/// Unlike `UnifiedAddressRequest::SHIELDED` (which only *allows* Sapling and silently drops it
/// at Sapling-invalid diversifier indices like 0), this always derives a full shielded UA
/// matching the same default diversifier index as `AllAvailableKeys`.
pub fn shielded_ua_request() -> UnifiedAddressRequest {
    use zcash_keys::keys::ReceiverRequirement::{Omit, Require};
    UnifiedAddressRequest::custom(Require, Require, Omit)
        .expect("Orchard + Sapling without P2PKH is always a valid UA request")
}

/// Shielded-only Unified Address request tailored to the receivers present in `ufvk`.
/// Requires Orchard and Sapling when both are present, or gracefully falls back to
/// Orchard-only / Sapling-only UFVKs (never embedding P2PKH).
pub fn shielded_ua_request_for_ufvk(ufvk: &UnifiedFullViewingKey) -> Result<UnifiedAddressRequest> {
    use zcash_keys::keys::ReceiverRequirement::{Omit, Require};
    match (ufvk.orchard().is_some(), ufvk.sapling().is_some()) {
        (true, true) => Ok(shielded_ua_request()),
        (true, false) => Ok(UnifiedAddressRequest::ORCHARD),
        (false, true) => UnifiedAddressRequest::custom(Omit, Require, Omit)
            .map_err(|e| EngineError::Message(format!("sapling-only UA request: {e:?}"))),
        (false, false) => Err(EngineError::Message(
            "UFVK has no shielded receivers (Orchard or Sapling required)".into(),
        )),
    }
}

/// Transparent-requiring Unified Address request used ONLY when deriving or rotating
/// standalone external BIP-44 transparent P2PKH addresses. Requiring `p2pkh` instructs
/// `zakura-client-sqlite` to allocate from `TransparentKeyScope::EXTERNAL` (indices
/// `0..gap_limit`) instead of a timestamp-based shielded diversifier index (`>= 2^31`).
pub fn transparent_ua_request() -> UnifiedAddressRequest {
    use zcash_keys::keys::ReceiverRequirement::{Allow, Require};
    UnifiedAddressRequest::unsafe_custom(Allow, Allow, Require)
}

/// Derive UFVK + default shielded-only UA (+ separate default transparent address) from a BIP-39 mnemonic.
/// Default UAs omit transparent receivers (`shielded_ua_request()`) so wallets never
/// downgrade shielded receives to transparent; transparent receives use an explicit standalone `t1` address.
pub fn account_from_mnemonic(
    mnemonic: &str,
    network: Network,
    account_index: u32,
) -> Result<AccountKeys> {
    let mnemonic = Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| EngineError::Message(format!("invalid mnemonic: {e}")))?;
    let seed = mnemonic.to_seed("");
    let account = AccountId::try_from(account_index).unwrap_or(AccountId::ZERO);
    let usk = UnifiedSpendingKey::from_seed(&network, &seed, account)
        .map_err(|e| EngineError::Message(format!("USK: {e:?}")))?;
    let ufvk = usk.to_unified_full_viewing_key();
    let shielded_req = shielded_ua_request_for_ufvk(&ufvk)?;
    let (ua, _) = ufvk
        .default_address(shielded_req)
        .map_err(|e| EngineError::Message(format!("address: {e:?}")))?;
    let transparent_address = ufvk
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .ok()
        .and_then(|(full, _)| {
            full.transparent()
                .map(|t| encode_transparent_address_p(&network, t))
        });
    Ok(AccountKeys {
        network,
        account_index,
        unified_address: ua.encode(&network),
        ufvk: ufvk.encode(&network),
        transparent_address,
    })
}

/// ZIP-32 seed fingerprint of a BIP-39 mnemonic (empty passphrase), as 64 hex
/// digits. Wallets use it to recognise a seed without storing it.
pub fn seed_fingerprint(mnemonic: &str) -> Result<String> {
    let mnemonic = Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| EngineError::Message(format!("invalid mnemonic: {e}")))?;
    let mut seed = mnemonic.to_seed("");
    let fp = match zip32::fingerprint::SeedFingerprint::from_seed(&seed) {
        Some(fp) => fp,
        None => {
            seed.zeroize();
            return Err(EngineError::Message(
                "seed length is not valid for ZIP 32".into(),
            ));
        }
    };
    seed.zeroize();
    Ok(fp.to_bytes().iter().map(|b| format!("{b:02x}")).collect())
}

/// Restore a viewing surface from a UFVK string (no spending key).
/// Default UA is shielded-only (`shielded_ua_request()`).
pub fn account_from_ufvk(
    ufvk_str: &str,
    network: Network,
    account_index: u32,
) -> Result<AccountKeys> {
    let ufvk = UnifiedFullViewingKey::decode(&network, ufvk_str.trim())
        .map_err(|e| EngineError::Message(format!("ufvk: {e}")))?;
    let shielded_req = shielded_ua_request_for_ufvk(&ufvk)?;
    let (ua, _) = ufvk
        .default_address(shielded_req)
        .map_err(|e| EngineError::Message(format!("address: {e:?}")))?;
    let transparent_address = ufvk
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .ok()
        .and_then(|(full, _)| {
            full.transparent()
                .map(|t| encode_transparent_address_p(&network, t))
        });
    Ok(AccountKeys {
        network,
        account_index,
        unified_address: ua.encode(&network),
        ufvk: ufvk.encode(&network),
        transparent_address,
    })
}

/// True when `derived` has every FVK item in `stored` (derived may have extras).
pub fn derived_ufvk_covers(network: Network, derived: &str, stored: &str) -> Result<()> {
    let derived = derived.trim();
    let stored = stored.trim();
    if derived == stored {
        return Ok(());
    }
    let derived_key = UnifiedFullViewingKey::decode(&network, derived)
        .map_err(|e| EngineError::Message(format!("derived ufvk: {e}")))?;
    let stored_key = UnifiedFullViewingKey::decode(&network, stored)
        .map_err(|e| EngineError::Message(format!("stored ufvk: {e}")))?;
    if derived_key.subsumes_ufvk(&stored_key) {
        Ok(())
    } else {
        Err(EngineError::Message(
            "those words do not match this wallet's viewing key".into(),
        ))
    }
}

/// WASM proving has no Sapling params — destinations must have an Orchard receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendDestPolicy {
    /// Unified (with a shielded receiver) or Sapling. Native path.
    Shielded,
    /// Unified with an Orchard receiver only. WASM path.
    OrchardOnly,
}

/// Reject transparent / TEX / Sprout / transparent-only UA. Product policy: shielded send only.
pub fn assert_shielded_send_dest(encoded: &str, policy: SendDestPolicy) -> Result<ParsedAddress> {
    let parsed = parse_address(encoded)?;
    let addr = ZcashAddress::try_from_encoded(encoded.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    let zk: ZkAddress = addr
        .convert()
        .map_err(|e| EngineError::Message(format!("address convert: {e}")))?;
    match zk {
        ZkAddress::Unified(ua) => {
            let orchard = ua.orchard().is_some();
            let sapling = ua.sapling().is_some();
            if !orchard && !sapling {
                return Err(EngineError::Message(
                    "destination unified address has no shielded receiver".into(),
                ));
            }
            if policy == SendDestPolicy::OrchardOnly && !orchard {
                return Err(EngineError::Message(
                    "wasm send is orchard-only; sapling destinations need the desktop wallet"
                        .into(),
                ));
            }
            Ok(parsed)
        }
        ZkAddress::Sapling(_) => {
            if policy == SendDestPolicy::OrchardOnly {
                return Err(EngineError::Message(
                    "wasm send is orchard-only; sapling destinations need the desktop wallet"
                        .into(),
                ));
            }
            Ok(parsed)
        }
        ZkAddress::Transparent(_) | ZkAddress::Tex(_) => Err(EngineError::Message(
            "transparent send is not supported; shield first and send to a unified address".into(),
        )),
    }
}

/// Parse a UA / t-addr / sapling / TEX string.
pub fn parse_address(encoded: &str) -> Result<ParsedAddress> {
    let addr = ZcashAddress::try_from_encoded(encoded.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    addr.convert::<ParsedAddress>()
        .map_err(|e| EngineError::Message(format!("address convert: {e}")))
}

/// Transparent P2PKH encoded from a unified address, if present.
pub fn transparent_from_unified(encoded_ua: &str, network: Network) -> Result<Option<String>> {
    let addr = ZcashAddress::try_from_encoded(encoded_ua.trim())
        .map_err(|e| EngineError::Message(format!("invalid address: {e}")))?;
    let zk: ZkAddress = addr
        .convert()
        .map_err(|e| EngineError::Message(format!("address convert: {e}")))?;
    match zk {
        ZkAddress::Unified(ua) => Ok(ua
            .transparent()
            .map(|t| encode_transparent_address_p(&network, t))),
        ZkAddress::Transparent(t) => Ok(Some(encode_transparent_address_p(&network, &t))),
        _ => Ok(None),
    }
}

fn network_from_type(net: NetworkType) -> Network {
    match net {
        NetworkType::Main => Network::Mainnet,
        NetworkType::Test => Network::Testnet,
        NetworkType::Regtest => Network::Regtest,
    }
}

impl TryFromAddress for ParsedAddress {
    type Error = core::convert::Infallible;

    fn try_from_sprout(
        net: NetworkType,
        _data: [u8; 64],
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::Sprout,
        })
    }

    fn try_from_sapling(
        net: NetworkType,
        _data: [u8; 43],
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::Sapling,
        })
    }

    fn try_from_unified(
        net: NetworkType,
        _data: zcash_address::unified::Address,
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::Unified,
        })
    }

    fn try_from_transparent_p2pkh(
        net: NetworkType,
        _data: [u8; 20],
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::P2pkh,
        })
    }

    fn try_from_transparent_p2sh(
        net: NetworkType,
        _data: [u8; 20],
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::P2sh,
        })
    }

    fn try_from_tex(
        net: NetworkType,
        _data: [u8; 20],
    ) -> std::result::Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from_type(net),
            kind: AddressKind::Tex,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_gift_entropy_preserves_account_and_rejects_noncanonical_words() {
        let zero = [0u8; 16];
        let words = mnemonic_from_entropy(&zero).unwrap();
        assert_eq!(words, REGTEST_FAUCET_MNEMONIC);
        assert_eq!(mnemonic_to_entropy(&words).unwrap(), zero);
        for len in [16, 20, 24, 28, 32] {
            let entropy = vec![7u8; len];
            let phrase = mnemonic_from_entropy(&entropy).unwrap();
            assert_eq!(mnemonic_to_entropy(&phrase).unwrap(), entropy);
            assert_eq!(
                seed_fingerprint(&phrase).unwrap(),
                seed_fingerprint(&mnemonic_from_entropy(&entropy).unwrap()).unwrap()
            );
        }
        for invalid in [
            format!(" {words}"),
            words.replace(' ', "  "),
            words.to_uppercase(),
            "private invalid words".into(),
        ] {
            let error = mnemonic_to_entropy(&invalid).unwrap_err().to_string();
            assert!(!error.contains(&invalid));
        }
        for len in [0, 15, 17, 31, 33, 64] {
            assert!(mnemonic_from_entropy(&vec![0; len]).is_err());
        }
    }

    #[test]
    fn seed_fingerprint_matches_zip32() {
        // BLAKE2b-256(personal "Zcash_HD_Seed_FP", [64] || bip39_seed), computed independently.
        let m = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        assert_eq!(
            seed_fingerprint(m).expect("fingerprint"),
            "21ed3d7882c7e37fe012b54a6408048048cb09782d4b2938617da793ccd27815"
        );
        assert_eq!(
            seed_fingerprint(&format!("  {m} ")).unwrap(),
            seed_fingerprint(m).unwrap()
        );
        assert!(seed_fingerprint("not a mnemonic").is_err());
    }

    #[test]
    fn mnemonic_roundtrip_and_regtest_ua() {
        let m = generate_mnemonic().expect("mnemonic");
        assert_eq!(m.split_whitespace().count(), 24);
        let acct = account_from_mnemonic(&m, Network::Regtest, 0).expect("account");
        assert!(acct.unified_address.starts_with("uregtest1"));
        assert!(acct.ufvk.starts_with("uviewregtest1"));
        let t = acct.transparent_address.expect("t-addr");
        assert!(t.starts_with("tm"), "regtest t-addr {t}");
        let parsed = parse_address(&acct.unified_address).expect("parse ua");
        assert_eq!(parsed.kind, AddressKind::Unified);
        assert_eq!(parsed.network, Network::Regtest);
    }

    #[test]
    fn faucet_mnemonic_is_stable() {
        let acct =
            account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).expect("faucet");
        assert!(acct.unified_address.starts_with("uregtest1"));
        let t = acct.transparent_address.expect("faucet t-addr");
        assert_eq!(t, REGTEST_FAUCET_TRANSPARENT);
        // Default UAs are shielded-only (no embedded transparent receiver).
        assert_eq!(
            transparent_from_unified(&acct.unified_address, Network::Regtest).unwrap(),
            None
        );
        let view = account_from_ufvk(&acct.ufvk, Network::Regtest, 0).unwrap();
        assert_eq!(view.unified_address, acct.unified_address);
        let t_err = assert_shielded_send_dest(&t, SendDestPolicy::OrchardOnly).unwrap_err();
        assert!(t_err
            .to_string()
            .contains("transparent send is not supported"));
        assert!(
            assert_shielded_send_dest(&acct.unified_address, SendDestPolicy::OrchardOnly).is_ok()
        );
    }

    #[test]
    fn derived_ufvk_covers_same_seed() {
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        derived_ufvk_covers(Network::Regtest, &acct.ufvk, &acct.ufvk).unwrap();
        let other = generate_mnemonic().unwrap();
        let other_acct = account_from_mnemonic(&other, Network::Regtest, 0).unwrap();
        assert!(derived_ufvk_covers(Network::Regtest, &other_acct.ufvk, &acct.ufvk).is_err());
    }

    #[test]
    fn ua_receiver_sets_and_inspect() {
        assert_eq!(UaReceiverSet::parse("all"), Some(UaReceiverSet::Full));
        let _ = UaReceiverSet::Orchard.request();
        assert!(UaReceiverSet::Full.includes_transparent());
        assert!(!UaReceiverSet::Orchard.includes_transparent());

        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let default_ua = inspect_address(&acct.unified_address).unwrap();
        assert_eq!(default_ua.kind, AddressKind::Unified);
        assert_eq!(default_ua.receiver_set, Some(UaReceiverSet::Shielded));
        assert!(default_ua.receivers.contains(&UaReceiver::Orchard));
        assert!(!default_ua.receivers.contains(&UaReceiver::P2pkh));

        let full_ua =
            unified_address_for_set(&acct.ufvk, Network::Regtest, UaReceiverSet::Full).unwrap();
        let full = inspect_address(&full_ua).unwrap();
        assert_eq!(full.receiver_set, Some(UaReceiverSet::Full));
        assert!(full.receivers.contains(&UaReceiver::P2pkh));

        let orchard_only =
            unified_address_for_set(&acct.ufvk, Network::Regtest, UaReceiverSet::Orchard).unwrap();
        let orch = inspect_address(&orchard_only).unwrap();
        assert_eq!(orch.receiver_set, Some(UaReceiverSet::Orchard));
        assert_eq!(orch.receivers, vec![UaReceiver::Orchard]);

        let t = inspect_address(REGTEST_FAUCET_TRANSPARENT).unwrap();
        assert_eq!(t.kind, AddressKind::P2pkh);
        assert_eq!(t.receiver_set, None);
    }

    #[test]
    fn unified_address_at_chosen_indices() {
        use InvalidDiversifier::{Next, Reject};
        let acct = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
        let at = |set, index, invalid| {
            unified_address_at(&acct.ufvk, Network::Regtest, set, index, invalid, false)
        };
        let orchard = UaReceiverSet::Orchard;

        // Deterministic, index 0 is the default, and indices give distinct addresses.
        let zero = at(orchard, 0, Reject).unwrap();
        assert_eq!(zero, at(orchard, 0, Reject).unwrap());
        assert_eq!(
            zero.address,
            unified_address_for_set(&acct.ufvk, Network::Regtest, orchard).unwrap()
        );
        assert_eq!(zero.diversifier_index, "0");
        let mut seen = std::collections::BTreeSet::new();
        for index in [0u128, 1, 2, 1000, u32::MAX as u128 + 1, (1 << 88) - 1] {
            let found = at(orchard, index, Reject).unwrap();
            assert_eq!(found.diversifier_index, index.to_string());
            assert_eq!(
                inspect_address(&found.address).unwrap().receiver_set,
                Some(UaReceiverSet::Orchard)
            );
            assert!(seen.insert(found.address));
        }
        assert!(at(orchard, 1 << 88, Reject)
            .unwrap_err()
            .to_string()
            .starts_with("invalid diversifier index"));

        // Sapling: some indices are invalid. Reject errors; Next returns the next valid one.
        // Every requested receiver is present; index 0 matches the default when it is valid.
        let shielded = UaReceiverSet::Shielded;
        let first = at(shielded, 0, Next).unwrap();
        assert_eq!(
            inspect_address(&first.address).unwrap().receiver_set,
            Some(UaReceiverSet::Shielded)
        );
        if first.diversifier_index == "0" {
            assert_eq!(
                first.address,
                unified_address_for_set(&acct.ufvk, Network::Regtest, shielded).unwrap()
            );
        }
        let invalid = (0..64u128)
            .find(|&i| at(shielded, i, Reject).is_err())
            .expect("a Sapling-invalid index below 64");
        let err = at(shielded, invalid, Reject).unwrap_err();
        assert_eq!(
            crate::error::classify_wallet_error_code(&err.to_string()),
            "invalid_diversifier_index"
        );
        let next = at(shielded, invalid, Next).unwrap();
        let next_index: u128 = next.diversifier_index.parse().unwrap();
        assert!(next_index > invalid);
        assert_eq!(next, at(shielded, next_index, Reject).unwrap());

        // Transparent receivers need an explicit opt-in and a non-hardened index.
        let full = UaReceiverSet::Full;
        assert!(at(full, 0, Reject).is_err());
        let opted =
            |index| unified_address_at(&acct.ufvk, Network::Regtest, full, index, Next, true);
        let first = opted(0).unwrap();
        assert_eq!(
            first.address,
            unified_address_for_set(&acct.ufvk, Network::Regtest, UaReceiverSet::Full).unwrap()
        );
        assert!(opted(1 << 31).is_err());
    }
}
