# Local node tooling

A validator and light server are optional infrastructure. Browser apps can use
a configured gRPC-Web provider without running either. To host your own chain
service, run Zebra or Zakura with Zaino and verify their network and recent tips.
See [mainnet operations](MAINNET-NODE.md) for deployment considerations.

## Choose the right endpoint

| Client | Chain connection |
| --- | --- |
| Native engine and desktop | Native gRPC to Zaino/lightwalletd |
| Browser wallet | gRPC-Web with appropriate TLS/CORS, or an app-owned transport |
| Local browser diagnostic app | Loopback block pipe, or native wallet bridge |

Validator JSON-RPC and light-server gRPC are separate endpoints. A block pipe
only transports chain data; a wallet bridge also controls a native wallet.
Keep both development interfaces bound to loopback. Never expose a wallet bridge,
its token or validator RPC through a public proxy.

The native `--server local` defaults are mainnet `127.0.0.1:8138`, testnet
`127.0.0.1:8137` and regtest `127.0.0.1:28137`. These are z-stack defaults,
not universal node port conventions. Supply your actual endpoint explicitly and
probe its network before creating a wallet.

## Local browser block pipe

With your validator and Zaino already running:

```sh
z-wallet pipe --zaino http://127.0.0.1:8138 --network mainnet \
  --rpc http://127.0.0.1:8232 --bind 127.0.0.1:1239
```

The repository helper accepts the same configuration:

```sh
ZAINO_URL=http://127.0.0.1:8138 Z_NETWORK=mainnet ZAKURA_RPC=http://127.0.0.1:8232 pnpm lwd:pipe
```

Without overrides it targets mainnet Zaino at `127.0.0.1:8138`. If `--rpc`
is omitted, the pipe probes loopback validators and enables validator-backed
mempool/submission methods only when one is available. Point the diagnostic app's
Zaino-pipe transport at `http://127.0.0.1:1239`; native clients connect directly
to Zaino instead. A local JSON wallet bridge uses `127.0.0.1:8787`.

## Disposable regtest

The [compose fixture](../infra/compose/README.md) uses Zebra 6.3.0 and
Zaino 0.10.0, with validator RPC at `127.0.0.1:29232` and light-server gRPC at
`127.0.0.1:28137`. It mines NU6.2 at height 2 and activates NU6.3 at 1,000,000:

```sh
pnpm regtest:up
pnpm test:regtest
pnpm regtest:down
```

To test Ironwood without Docker, set `ZAKURAD` and `ZAINOD` to local binaries
and use `pnpm regtest:native:up`. Without explicit paths, the launcher looks for
versioned binaries under `~/.local/share/z-stack/mainnet/bin`.
NU6.3 defaults to height 150. Match each
client's `Z_STACK_REGTEST_NU6_3` to the actual fixture. Chain data and logs use
`Z_STACK_REGTEST_DIR`, defaulting to `~/.local/share/z-stack/regtest`.
`--fresh` deletes that fixture chain; use it only for disposable test data.
Stop it with `pnpm regtest:native:down`.

NU7 requires a compatible validator and light server; the compose fixture above
does not support it. See the optional [NU7 fixture](../infra/nu7/README.md) and
[acceptance steps](RELEASE.md#nu7-acceptance).

SDK/browser fixtures share `Z_STACK_REGTEST_LWD_PORT` (default 28137). An explicit
`Z_STACK_REGTEST_LWD` URL takes precedence. Never run competing funded tests
against the same faucet. See [package verification](RELEASE.md).

## Node launcher and snapshots

`z-node-launcher` can probe node readiness and inspect snapshot metadata:

```sh
cargo run -p z-node-launcher -- ready --network testnet
cargo run -p z-node-launcher -- snapshot --mode archive
```

The mainnet/testnet compose file is an operator stub, not a configured production
deployment. Select compatible releases, pin image digests, configure storage and
secure endpoints before using it. Snapshot metadata comes from Zakura's published
inventory; verify checksums and stop the validator before replacing its state.
Budget space for extraction and the light-server index. A pruned node may not
serve historical raw transactions or tree state required by old wallets.

## Privacy and deep sync

Native address-based transparent queries are loopback-only. The browser block
pipe also requires its upstream light server to be loopback before exposing
address queries. A loopback forwarder that contacts a remote provider does not
make those addresses private. SDK remote address lookup requires explicit
permission; compact transparent scanning is a separate capability.
See [network disclosures](SECURITY.md) and [public retrieval](PUBLIC-RETRIEVAL.md).

Tor/SOCKS transport is not implemented by z-stack. Local chain services avoid
remote provider requests but do not provide anonymity for the node's own traffic.
A native wallet's first remote scan is capped at 150,000 blocks unless
`Z_STACK_ALLOW_DEEP_SYNC=1`; existing native wallets can catch up over longer
gaps. Browser deep-sync policy is configured separately. See [sync](SYNC.md).
