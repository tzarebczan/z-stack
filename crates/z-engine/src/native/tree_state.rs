//! Bounded native GetTreeState compatibility for servers whose historical
//! height selector fails while their exact block-hash selector works.
use super::lwd::LwdClient;
use crate::{
    error::{EngineError, Result},
    Network,
};
use std::time::Duration;
use zcash_client_backend::proto::service::{BlockId, TreeState};

const RPC_BUDGET: Duration = Duration::from_secs(15);
const TOTAL_BUDGET: Duration = Duration::from_secs(45);
// Compact blocks include protobuf metadata and pool transaction data. Bound
// both response types at the transport; tree frontiers are much smaller.
const MESSAGE_BYTES: usize = 4 * 1024 * 1024;

fn transport(reason: &'static str) -> EngineError {
    EngineError::Transport(reason.into())
}

fn validate(
    state: &TreeState,
    height: u32,
    expected: Option<Network>,
    hash: Option<&[u8]>,
) -> Result<()> {
    let network = match expected {
        Some(Network::Mainnet) => matches!(state.network.as_str(), "main" | "mainnet"),
        Some(Network::Testnet) => matches!(state.network.as_str(), "test" | "testnet"),
        // Existing Zebra-compatible regtest servers use "test" here.
        Some(Network::Regtest) => matches!(state.network.as_str(), "test" | "regtest"),
        None => matches!(
            state.network.as_str(),
            "main" | "mainnet" | "test" | "testnet" | "regtest"
        ),
    };
    if !network || state.height != u64::from(height) {
        return Err(transport("GetTreeState identity mismatch"));
    }
    if state.hash.len() != 64 {
        return Err(transport("GetTreeState malformed block hash"));
    }
    let mut bytes = crate::web::from_hex(&state.hash)
        .map_err(|_| transport("GetTreeState malformed block hash"))?;
    if bytes.len() != 32 {
        return Err(transport("GetTreeState malformed block hash"));
    }
    // TreeState uses display order; GetBlock and BlockId use internal order.
    bytes.reverse();
    if hash.is_some_and(|hash| bytes != hash) {
        return Err(transport("GetTreeState block hash mismatch"));
    }
    state
        .to_chain_state()
        .map_err(|_| transport("GetTreeState malformed frontier"))?;
    Ok(())
}

pub(super) async fn fetch(
    client: &mut LwdClient,
    height: u32,
    expected: Option<Network>,
) -> Result<TreeState> {
    fetch_with_budgets(client, height, expected, RPC_BUDGET, TOTAL_BUDGET).await
}

async fn fetch_with_budgets(
    client: &mut LwdClient,
    height: u32,
    expected: Option<Network>,
    rpc_budget: Duration,
    total_budget: Duration,
) -> Result<TreeState> {
    // Clone preserves the selected channel and leaves other RPC limits alone.
    let mut client = client.clone().max_decoding_message_size(MESSAGE_BYTES);
    tokio::time::timeout(total_budget, async {
        let selector = BlockId {
            height: u64::from(height),
            hash: vec![],
        };
        match tokio::time::timeout(rpc_budget, client.get_tree_state(selector.clone())).await {
            Ok(Ok(response)) => {
                let state = response.into_inner();
                validate(&state, height, expected, None)?;
                return Ok(state);
            }
            Ok(Err(error)) if super::lwd::remote_tree_status(&error) => {}
            Err(_) => {}
            // Local decoding/framing/connection failures and explicit refusals
            // are not evidence of the remote historical selector failure.
            Ok(Err(_)) => return Err(transport("GetTreeState height request refused")),
        }
        let block = tokio::time::timeout(rpc_budget, client.get_block(selector))
            .await
            .map_err(|_| transport("GetBlock tree-state lookup timed out"))?
            .map_err(|_| transport("GetBlock tree-state lookup failed"))?
            .into_inner();
        if block.height != u64::from(height) || block.hash.len() != 32 {
            return Err(transport("GetBlock tree-state identity mismatch"));
        }
        let state = tokio::time::timeout(
            rpc_budget,
            client.get_tree_state(BlockId {
                height: 0,
                hash: block.hash.clone(),
            }),
        )
        .await
        .map_err(|_| transport("GetTreeState hash lookup timed out"))?
        .map_err(|_| transport("GetTreeState hash lookup failed"))?
        .into_inner();
        validate(&state, height, expected, Some(&block.hash))?;
        Ok(state)
    })
    .await
    .map_err(|_| transport("GetTreeState lookup deadline exceeded"))?
}

#[cfg(test)]
mod tests;
