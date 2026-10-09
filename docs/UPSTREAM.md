# Upstream dependencies and local patches

## Package aliases and pins

Rust dependency keys retain upstream API paths while Cargo resolves the published
Zakura package. For example, `use orchard::…` resolves `zakura-orchard`, not the
upstream `orchard` crate. Exact crypto pins match the wallet backend; do not mix
Zakura and upstream Orchard implementations in one wallet.

| Rust key | Published package | Pin | Owner |
| --- | --- | --- | --- |
| `orchard` | `zakura-orchard` | `=2.2.0` | workspace |
| `zcash_keys` | `zakura-keys` | `=2.2.0` | workspace |
| `zcash_primitives` | `zakura-primitives` | `=2.2.0` | workspace |
| `zcash_proofs` | `zakura-proofs` | `=2.2.0` | workspace, optional native proving |
| `zcash_client_backend` | `zakura-client-backend` | `0.1.0-rc7` | workspace |
| `zcash_client_sqlite` | `zakura-client-sqlite` | `0.1.0-rc7` | workspace, native |
| `pczt` | `zakura-pczt` | `0.1.0-rc4` | workspace, hardware/native |
| `sapling` | `zakura-sapling-crypto` | `=2.2.0` | z-engine |
| `jubjub` | `zakura-jubjub` | `=2.2.0` | z-engine, patched |
| `bls12_381` | `zakura-bls12-381` | `=2.2.0` | z-engine, patched |
| transitive pasta fields | `zakura-pasta-curves` | `2.2.0` | patched |
| `zcash_protocol` | `zakura-protocol` | `=2.2.0` | z-engine |
| `zcash_address` | `zakura-address` | `=2.2.0` | z-engine |
| `transparent` | `zakura-transparent` | `=2.2.0` | z-engine |

`Cargo.lock` fixes resolved versions for this application workspace. The manifest
is the compatibility policy; the lockfile is the full dependency graph. Review
both on a bump. Source projects: [Zakura Common](https://github.com/zakura-core/common)
and [wallet libraries](https://github.com/zakura-core/wallet-libraries).

## NU7 consensus

The pinned Common 2.2.0 schedule activates NU7 on testnet at **4,465,026**,
with branch ID `0x77190ad9`. Mainnet NU7 is unscheduled in this release.
The engine delegates branch selection and transaction formats to these crates.
Do not replace protocol, address or transparent aliases with upstream crates:
similarly named types from another release can carry a different schedule.

[Zakura 1.6.0](https://github.com/zakura-core/zakura/releases/tag/v1.6.0)
implements the corresponding validator rules. NU7 uses a 25-second target
spacing; date-based birthday helpers account for both sides of activation and
retain the same time safety margin. Dates remain approximate; use an exact
birthday height when available.

NSM fee recycling is a validator/coinbase rule. The wallet pays its quoted
transaction fee once; there is no additional wallet-side NSM charge or deposit.
The SDK continues using the wallet libraries' conventional fee rule; a lower
validator admission threshold is not a new wallet fee policy.

The optional [NU7 light-server fixture](../infra/nu7/README.md) adapts pinned
Zaino source to published Zakura node crates for isolated acceptance. It is
separate from the SDK dependency graph and is not a production server release.

## Build and integration boundaries

`z-engine` defaults to native SQLite/networking. `z-wasm` disables those defaults
and enables browser-safe scan, transaction, proving, and hardware features.
Threaded WASM adds `multicore`; Sapling proving parameter files remain separate.
The SDK archive includes prebuilt engines, so browser integrators do not need Cargo
or local patches. Direct Rust consumers are a separate integration path.

Cargo reads `[patch.crates-io]` from the **top-level** workspace. Depending on
`z-engine` from another workspace does not inherit these patches. A direct Rust
consumer must copy the three root patch entries with paths to this checkout's
`vendor/zakura/`, or deliberately use and verify published implementations.
See [Cargo's override rules](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html).
These crates are not currently a registry distribution from z-stack.

## wasm32 field optimizations

| Package | Changed code | Purpose |
| --- | --- | --- |
| `zakura-pasta-curves` | `arithmetic/fields.rs`, `fields/{fp,fq,portable,wasm32}.rs`, `fields.rs` | wasm32 multiply-accumulate and 29-bit Montgomery field backend |
| `zakura-bls12-381` | `util.rs`, `scalar.rs`, `wasm32.rs`, `lib.rs` | same optimization for the scalar field |
| `zakura-jubjub` | `util.rs` | wasm32 multiply-accumulate |

Baseline: published **2.2.0** archives from
`zakura-core/common` at `b6cf4cefe38fdccc6d2b0dc868f7d6ef01683413`.
Original patch revision: `05110ed3d95f0d005256348fc7652719a3b52b4c`.
All three retain MIT/Apache-2.0 licenses and copyright notices. Arithmetic
selection is gated on `target_arch = "wasm32"`; native production code is unchanged.
The WASM backend is also exercised natively by its test module.

The complete source is under `vendor/zakura/`. Standalone diffs and per-file
baseline hashes are under `patches/zakura/`. `pnpm check:patches` reverses each
patch in a temporary copy, compares every baseline file to the recorded hash,
applies the patch forward, and compares the resulting files byte-for-byte to
the untouched vendor tree. It uses matching Git patch context without whitespace
ignoring or three-way fallback. This offline gate catches inconsistent tree,
patch and manifest edits; it does not download crates.io or independently
establish that a mutually edited manifest/tree/patch is the published source.

To also verify separately obtained published archives:

```sh
pnpm check:patches --archives /path/to/cached-crates
```

The directory must contain `<package>-<version>.crate` for each manifest entry.
The checker compares the compressed archive's SHA-256 to the recorded digest
and its regular-file contents to the reversed baseline. It parses the archive
in memory without extracting paths. Obtain the archives through the manifest's
`https://static.crates.io/...` URLs and review archive digests against trusted
crates.io metadata when changing the baseline. The manifest and cache are trust
inputs, not signatures or an independent authenticity certificate.

No separate fork checkout or submodule is needed. Vendored sources and patch
manifests are the build inputs.

For an upstream contribution, start from the manifest's baseline. The diffs
apply to extracted crate archives with `git apply` from their parent directory;
adjust path prefixes to the upstream repository layout before proposing them.
Include native arithmetic tests, single-thread WASM and threaded WASM wallet
regressions, and [performance measurements](PERFORMANCE.md). Do not claim a
hardware-independent speedup from one benchmark. Remove a local patch only
when a compatible upstream release includes it and passes those same checks.

Future customization should first use a `z-engine` wrapper. If upstream changes
are unavoidable, keep the baseline, patch, license, tests, and removal criterion
in this ledger. Regenerate the manifest and diff together; never edit the
vendored tree without updating its reproducible patch.

## Known gaps (workarounds in z-engine)

| Gap | Workaround |
|-----|------------|
| Older Zaino `GetSubtreeRoots` protocols lack `ironwood = 2`. Ironwood subtree support depends on the server version and network. | Native sync requests Ironwood roots except on regtest. An InvalidArgument answer still becomes an empty stream in `LwdChannel`. `Z_STACK_SKIP_IRONWOOD_SUBTREES=1\|0` overrides this. The web pipe decides from its `--network` too, and the WASM client requests Ironwood roots except on regtest. |
| Zaino 0.10.0 `GetSubtreeRoots` calls `getblock` (verbosity 1) once per root, one at a time, under a 30 s × 4 stream deadline. On an archive node that is 0.2–1.5 s per historical root, so a request for every root from shard 0 on mainnet never completes. | Native requests roots from the scan-start shard, in small concurrent pages delivered in order, and waits at most 2 s once the scan is done. Worth an upstream Zaino fix: only the completing height and root are needed, and the validator's `z_getsubtreesbyindex` already returns both in milliseconds. |
| Zaino 0.8/0.10 regtest `SendTransaction` on local fixtures (`InternalServerError: error receiving data from backing node`) | On regtest, `NativeWallet` submits via Zebra `sendrawtransaction` (`LightServer::zebra_rpc_url`). The funded browser fixture uses the same guarded validator fallback; LWD submit remains the path for remote servers. |
| `zakura-client-sqlite` 0.1.0-rc7 `FsBlockDb` lacks `BlockCache` | `FsBlockCache` in `z-engine` (`wallet-data/blocks/*.pb`) |
| sqlite orchard feature omits `pczt/io-finalizer` needed by migration | We enable `pczt` features ourselves for unification |
| Common packages share the **2.2.0** release | Wallet backend/sqlite rc7 and PCZT rc4 use the same Common family. Native enhancement and status queues are separate; private-routed work is never sent through a public transport. |
| `zakura-client-memory` is crates.io-reserved `0.0.0` | WASM uses `z-engine::web` snapshot + `scan_block` + in-memory shardtrees / `WalletWrite`. Do **not** take crates.io `zcash_client_memory` (upstream orchard). Swap the store when Zakura publishes the memory backend. |

Native selective shard scanning uses public `zakura-client-backend::data_api::ll::wallet::put_blocks_rows` (notes/txs) then hashes only birthday, marked, and tip shards. Not a fork.
