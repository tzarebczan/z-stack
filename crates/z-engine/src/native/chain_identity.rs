//! Canonical network identity, independent of configurable endpoint names.
use crate::{
    error::{EngineError, Result},
    Network,
};

// Zakura chain parameters: checkpoint/list.rs and network/testnet.rs.
pub(super) fn genesis(network: Network) -> &'static str {
    network.genesis_hash()
}

pub(super) fn validate(network: Network, chain: &str, hash: &str) -> Result<()> {
    let name = chain.to_ascii_lowercase();
    let matches = match network {
        Network::Mainnet => matches!(name.as_str(), "main" | "mainnet"),
        Network::Testnet => matches!(name.as_str(), "test" | "testnet"),
        // Zebra reports "test" for regtest; the genesis disambiguates it.
        Network::Regtest => matches!(name.as_str(), "regtest" | "test"),
    };
    if !matches || !hash.eq_ignore_ascii_case(genesis(network)) {
        return Err(EngineError::ChainMismatch {
            wallet: network.as_str().into(),
            server: "submission endpoint identity mismatch".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_genesis_disambiguates_test_and_rejects_loose_names() {
        for network in [Network::Mainnet, Network::Testnet, Network::Regtest] {
            assert!(validate(network, network.as_str(), genesis(network)).is_ok());
            assert!(validate(network, network.as_str(), &"00".repeat(32)).is_err());
        }
        assert!(validate(Network::Regtest, "test", genesis(Network::Regtest)).is_ok());
        assert!(validate(Network::Testnet, "test", genesis(Network::Regtest)).is_err());
        assert!(validate(Network::Regtest, "test", genesis(Network::Testnet)).is_err());
        assert!(validate(Network::Mainnet, "not-mainnet", genesis(Network::Mainnet)).is_err());
    }
}
