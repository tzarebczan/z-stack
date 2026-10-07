# Swap integration

Swap routing is an application adapter concern. The SDK provides shielded sends,
shielding, address inspection, fee estimation, and guarded transparent deposit
primitives. It does not ship a quote service, NEAR Intents integration, fiat
account, or card provider.

`packages/swaps` contains an experimental adapter interface only. It is not
included in the preview archives. Implement provider-specific adapters in your
application behind the wallet boundary.

Require explicit user approval of asset, network, destination, amount, fees,
refund address, and quote expiry. Treat an uncertain wallet broadcast as pending
before retrying any cross-chain leg. Persist progress without persisting keys.

Swap services can correlate deposits, withdrawals, and timing. Prefer shielded
endpoints when the route supports them. Never send a seed or viewing key to a
quote provider. Transparent SDK primitives belong behind an approved adapter;
they are not the default wallet Send flow.
