# Architecture

```
┌─────────────┐              ┌──────────────┐
│ packages/web│              │  z-desktop   │  (GPUI)
│  WASM lab   │              └──────┬───────┘
└──────┬──────┘                     │ native sqlite
       │ keys + scan + history      ▼
       ▼                     ┌──────────────┐
┌─────────────┐              │  z-engine    │◀── Zakura crates
│  z-wasm     │─────────────▶│  web store   │
└──────┬──────┘              │  + sqlite    │
       │ compact blocks      └──────┬───────┘
       │ gRPC-Web or         native gRPC │
       │ GET /lwd/blocks (loopback)      │
       ▼                                 ▼
                         ┌─────────────────────┐
                         │ Zaino / LWD         │
                         └─────────────────────┘
```

Hosted wasm uses CORS gRPC-Web (optional Traefik `grpcweb`). Local wasm uses **`z-wallet pipe`** (`http://127.0.0.1:1239`) or `z-wallet serve` `/lwd/*` as a block pipe only. Desktop talks native gRPC to Zaino and skips the pipe. See [`WEB.md`](WEB.md) and [`NODE.md`](NODE.md).

## Layers

1. **z-engine** — accounts, sync orchestration, shield, PCZT create/prove/sign/broadcast, history. Native: sqlite. WASM: snapshot store + `scan_block`.
2. **z-wasm** — mnemonic / UFVK / UA, compact-block scan, snapshot, `HistoryEntry`. Sapling params stay out; Orchard/Ironwood proving runs in a worker, with an optional Rayon pool.
3. **Apps** — `packages/web` is the SDK lab (WASM-first). GPUI talks native engine.
4. **infra/compose** — **Regtest** is Zebra+Zaino (`pnpm regtest:up`). Mainnet/testnet is bring-your-own Zebra or Zakura + Zaino; `docker-compose.yml` is an operator stub. Traefik is an optional `grpcweb` profile. Tor later.

## Crypto

Consume published `zakura-*` packages with upstream-shaped dependency keys (`orchard = { package = "zakura-orchard", ... }`). See root `Cargo.toml`.

## Browser client internals

`wasm-client.ts` owns the client state and assembles the engine-facing API.
The storage controller handles hydration, generation replacement and durable
checkpoints; the sync controller scans blocks and coordinates optional memo/public
data work; the spending controller owns reservations, proving and submission.
Separate modules own worker actors, origin-wide epochs and bounded snapshot
previews. Cryptographic work stays in Rust.

Controllers receive typed subsets of internal capabilities. State accessors read
the owner closure at use time, rather than capturing a snapshot that could become
stale while another tab saves or a worker restarts. Factory construction does no
wallet work; peer capabilities are resolved only when an operation starts. No
account, registration, backup server or fiat integration enters this graph.
