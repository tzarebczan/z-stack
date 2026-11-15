# Swap integration

Swap routing is an application adapter concern. The SDK provides shielded sends,
shielding, address inspection, fee estimation, and guarded transparent deposit
primitives. It does not ship a quote service, NEAR Intents integration, fiat
account, or card provider.

`@z-stack/swaps` supplies the optional durable two-leg CYPH route controller.
Provider-specific NEAR and Jupiter services remain application adapters behind
the wallet boundary. The optional `@z-stack/solana` sidecar derives the matching
Solana account and validates/signs its reviewed operations. Both have matching
preview archives; see the [Solana package](../packages/solana/README.md) and
[two-leg coordinator](../packages/swaps/README.md) for their integration contracts.

Require explicit user approval of asset, network, destination, amount, fees,
refund address, and quote expiry. Treat an uncertain wallet broadcast as pending
before retrying any cross-chain leg. Persist progress without persisting keys.

Swap services can correlate deposits, withdrawals, and timing. Prefer shielded
endpoints when the route supports them. Never send a seed or viewing key to a
quote provider. Transparent SDK primitives belong behind an approved adapter;
they are not the default wallet Send flow.

The browser `sendTransparent(to, amountZec, options)` method accepts the same
`beforeSubmit(receipt, signal)` journal hook as a shielded send. It runs after
the Rust wallet's signed bytes and input reservation are durable, and before
the transport receives the transaction. A rejected hook prevents submission.
After a successful journal commit, cancellation retains the saved signed bytes
for explicit reconciliation and recovery.

Check `SDK_TRANSPORT_CAPABILITIES.transparentSendBeforeSubmit === true` before
relying on this hook with an installed archive. A caller-provided Private Cloud
transport with verified signed-submit authority can carry a reviewed transparent
output. This does not authorize transparent inputs or alter the default
shielded-only Send policy. The same Rust-derived transaction ID is passed to
the transport for either output type.
