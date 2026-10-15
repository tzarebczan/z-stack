# Threat model: z-stack

## What this project does and where untrusted input enters

z-stack is a Zcash wallet engine (Rust, compiled native and to WebAssembly) plus a
TypeScript SDK for web apps. Key derivation, trial decryption, note tracking,
commitment trees, transaction building, and proving all run on the user's
device. The servers it talks to are not trusted with keys, but the wallet still
consumes a lot of data from them. Treat all of the following as attacker-controlled:

- **Light server / chain data.** Compact blocks, tree states, subtree roots and
  full transactions from lightwalletd/Zaino (native gRPC in
  `crates/z-engine/src/native/{lwd,source,pipeline,tree_state}.rs`; gRPC-Web in
  `packages/sdk/src/lwd.ts`, `transport-bounds.ts`; length-framed compact-block blobs in
  `crates/z-engine/src/web/framed.rs`). A malicious or MITM'd server can send
  malformed protobuf, oversized frames, inconsistent heights/hashes, bad
  subtree roots, and crafted ciphertexts that hit trial decryption and memo
  decoding.
- **Selective shard scanning / tree reconstruction** (`crates/z-engine/src/offload.rs`,
  `native/selective_scan.rs`, `native/verify.rs`, `web/live_trees.rs`). Server-supplied
  roots and frontiers decide which witnesses the wallet believes are valid.
- **PIR services.** Transparent PIR (`crates/z-transparent-pir-v2`, esp. `sync.rs`,
  `pass.rs`, `serialization.rs`) and Enhance PIR (`crates/z-enhance-pir`) parse
  manifests, materials and replies from a remote server (size limits in
  `z-enhance-pir/src/lib.rs`).
- **Public retrieval / memo fetch** (`web/public_data.rs`, `web/private_enhance.rs`,
  `packages/sdk/src/public-data.ts`).
- **Wallet snapshots and browser storage** (`web/store.rs`, `WasmWallet.fromSnapshot`,
  `packages/sdk/src/{snapshot-chunks,snapshot-storage,storage,integrity}.ts`). Another
  tab, extension, or a restored backup file can hand us arbitrary bytes.
- **User/app strings passed into WASM** (`crates/z-wasm/src/lib.rs`): mnemonics, gift
  mnemonics/entropy, UFVKs, unified/transparent addresses (`parseAddress`,
  `inspectAddress`, `checkViewingKey`, `ufvkCovers`), and ZIP-321 payment URIs
  (`crates/z-engine/src/zip321.rs`, mirrored in `packages/core`).
- **Hardware wallet path.** PCZT → Ledger APDU conversion and responses
  (`crates/z-engine/src/ledger/{parse,apdu,serializer}.rs`, `web/hardware.rs`); signatures
  returned by the device are verified before broadcast.
- **Local native bridge** (`crates/z-engine/src/native/bridge.rs`). Loopback HTTP server
  driven by a web page; protected by an Origin allowlist and a per-start bearer token
  (also accepted as `?token=`). Any web page in the user's browser can reach it.
- **Passkey / seed vault** (`packages/passkey`, `packages/sdk/src/seed-vault.ts`):
  WebAuthn PRF and passphrase-encrypted records, which may come back from an app's
  remote backup service.

## Components that matter most / least

Most: anything that can leak or corrupt spending keys or seeds; scan/decrypt and
tree code fed by the light server; transaction building and the funds/notes the
wallet thinks it can spend; the native bridge auth; snapshot and vault decoding;
PIR reply parsing.

Least / out of scope:
- `vendor/` (patched upstream Zakura/PIR crates). Report upstream bugs only if
  reachable through z-stack.
- `crates/z-desktop` (GPUI shell), `examples/`, `infra/`, `scripts/`: demos and tooling.
- `crates/z-node-launcher`: local dev helper for regtest nodes.
- XSS in example apps, and "a compromised origin can read an unlocked wallet". That is
  documented in `docs/SECURITY.md` and not something the SDK claims to prevent.
- Network metadata the docs already disclose (IP, block ranges, txids on memo fetch).

## How to exercise it

- `cargo test -p z-engine --features native,cli,native-pir,private-enhance,transparent-pir --lib`
  (unit tests for scan, store, offload, zip321, keys, ledger, bridge).
- `cargo test -p z-transparent-pir-v2 --lib -- --test-threads=1`, `cargo test -p z-enhance-pir --lib`.
- `crates/z-engine/src/bin/z_wallet.rs` is the CLI (`z-wallet serve` runs the bridge).
- TS tests: `node --conditions=@z-stack/source --import tsx --test packages/sdk/test/*.test.ts`.
- Regtest tests need a node and are not runnable offline.

## How you rate severity

- **Critical:** seed/spending-key disclosure or extraction (including via the bridge
  without the token, or through logs/snapshots); a server or input able to make the
  wallet sign or send funds the user did not approve; bypass of hardware signature
  verification.
- **High:** memory unsafety reachable from server data or WASM inputs; a malicious
  server making the wallet accept wrong balances, fake notes, or bad witnesses that lead
  to stuck/lost funds; viewing key or decrypted history leaking off-device beyond what
  docs disclose; vault decryption without the passphrase/PRF.
- **Medium:** remote DoS of sync (panics, unbounded memory/CPU from crafted
  blocks/PIR replies/snapshots), privacy leaks beyond the documented table.
- **Low:** local-only crashes from obviously malformed caller input, cosmetic issues.

## Anything to leave alone

- Mainnet/testnet endpoints. Don't hit public servers; use unit tests.
- Known upstream Zcash protocol limitations (e.g. light-server trust in compact block
  completeness) unless z-stack makes them worse.
- Alpha status: "not audited" is known.
