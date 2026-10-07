# Compose

## Regtest (automated tests)

Zebra 6.3.0 + Zaino 0.10.0-no-tls. Coinbase pays the faucet t-addr derived from
`z_engine::keys::REGTEST_FAUCET_MNEMONIC`.

```bash
pnpm regtest:up          # compose up, wait RPC, mine height 2 (NU6.2)
pnpm regtest:mine 110    # extra blocks if you need consensus coinbase maturity (still 100 at the node)
Z_STACK_REGTEST=1 cargo test -p z-engine --test regtest -- --ignored --nocapture
pnpm regtest:down
```

| Endpoint | URL |
|----------|-----|
| Zebra JSON-RPC | http://127.0.0.1:29232 (no cookie auth) |
| Zaino gRPC | http://127.0.0.1:28137 (plaintext h2c) |

Without Docker, or to test Ironwood: `pnpm regtest:native:up [--fresh]` runs
local Zakura and Zaino binaries selected with `ZAKURAD` / `ZAINOD` on the same
ports, with NU6.3 (Ironwood) at 150 (`Z_STACK_REGTEST_NU6_3`). Export the same variable for `z-wallet`,
cargo tests and the SDK scripts. Chain and logs: `~/.local/share/z-stack/regtest`.
`pnpm regtest:native:down` stops it.

```bash
pnpm regtest:native:up --fresh
Z_STACK_REGTEST=1 Z_STACK_REGTEST_NU6_3=150 cargo test -p z-engine --features native --test regtest \
  create_sync_shield_send -- --ignored --exact   # Orchard, before height 150
Z_STACK_REGTEST=1 Z_STACK_REGTEST_NU6_3=150 cargo test -p z-engine --features native --test regtest \
  ironwood_turnstile_shield_send -- --ignored --exact
```

Wallet:

```bash
cargo run -p z-engine --features native,cli -- --network regtest --passphrase regtest \
  restore --mnemonic 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about' \
  --birthday 1 --wallet ./wallet-data-regtest
```

Zakura is the operator full-node path (bring-your-own, or the `docker-compose.yml` stub). Regtest uses Zebra
because `generate` + activation-height config is the documented Z3 recipe.

## Bring your own node (mainnet / testnet)

If Zebra or Zakura and Zaino are already running, do **not** compose-up. Start only
the loopback pipe for the WASM lab (desktop talks native gRPC and skips it):

```bash
z-wallet pipe --zaino http://127.0.0.1:8138 --network mainnet \
  --rpc http://127.0.0.1:8232 --bind 127.0.0.1:1239

ZAINO_URL=http://127.0.0.1:8138 Z_NETWORK=mainnet ZAKURA_RPC=http://127.0.0.1:8232 pnpm lwd:pipe
```

`pnpm lwd:pipe` defaults to mainnet Zaino at `127.0.0.1:8138`; override it for your node. See [`docs/NODE.md`](../../docs/NODE.md).

## Operator stub (mainnet/testnet)

`docker-compose.yml` documents Zakura + Zaino (`--profile node`). It is not a
pin-and-run stack (no published ports; pin digests first). Traefik is
`--profile grpcweb` and is not required for desktop or local web. Browsers
cannot speak cleartext HTTP/2, so gRPC-Web on `:1238` is HTTP/1.1 (six Chrome
sockets). For the WASM lab, run **`pnpm lwd:pipe`** on the host instead
(`z-wallet pipe` → `127.0.0.1:1239`, native gRPC to **your** Zaino). Pin digests
before relying on it. See [`docs/NODE.md`](../../docs/NODE.md).

Transparent UTXO scan (`GetAddressUtxos`) is limited to loopback Zaino. Native
remote servers do not receive wallet transparent addresses; use a local node
for native address-based transparent queries.

`sync::run` works on regtest via `LwdChannel` answering Ironwood subtree-root
requests locally. Shield proving works; Zaino→Zebra `SendTransaction` is still
flaky (see `docs/UPSTREAM.md`).
