# Run a mainnet chain service

A full node and light server are operator infrastructure, separate from browser
wallet integration. Public gRPC-Web endpoints or an app-owned `BlockTransport`
can be used without installing either locally.

Choose supported releases from [Zakura](https://github.com/zakura-core/zakura/releases)
and [Zaino](https://github.com/zingolabs/zaino/releases). Verify release signatures,
checksums and network compatibility against their published instructions. Do not
copy a historical snapshot/version pin without checking support expiry.

Keep validator state, the light-server index and wallet data in separate
owner-controlled directories. On Linux, an operator may use
`${XDG_DATA_HOME:-$HOME/.local/share}/z-stack/mainnet` and user service units under
`${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user`. Preserve RPC cookies outside
source control; do not print them in diagnostics or expose validator RPC publicly.

Budget disk space for the selected validator mode, light-server index, snapshot
bootstrap and recovery staging. Confirm that your chosen combination can retrieve
historical blocks, raw transactions and tree state needed by old wallets. A
pruned validator may not supply these through the light server. Monitor disk,
file-cache memory, readiness and supported-release deadlines separately.

Verify the configured network and matching recent tips, then test historical
compact blocks, transaction retrieval and tree state before accepting wallet
traffic. Browser clients require gRPC-Web/CORS or an appropriately restricted
adapter; native gRPC readiness alone does not establish browser compatibility.
The [proxy reference](../scripts/grpc-web-proxy.mjs) is development tooling, not a
public production gateway.

See [node tooling](NODE.md) and [privacy boundaries](SECURITY.md) before operating
a service for other users.
