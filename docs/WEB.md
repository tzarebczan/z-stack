# Browser engine internals

For application setup, use [the integration guide](INTEGRATION.md). This document
explains how the browser runtime implements the wallet API and which invariants
must survive engine changes.

## Runtime layout

`@z-stack/sdk` calls `z-wasm`, which wraps `z-engine`. Cryptography and signature
validation stay in Rust. TypeScript moves compact blocks, coordinates workers,
commits snapshots, and exposes events. `packages/web` is a diagnostic consumer;
it is not a required app shell or backend.

There are two production binaries:

- Single-thread WASM: key/UI helpers and a fallback scanner/prover.
- Threaded WASM: atomics and wasm-bindgen-rayon, selected when isolation and
  SharedArrayBuffer are available. Worker count is bounded and configurable.

The page starts key/UI bindings and a scan worker in parallel. The initial
runtime report can be single-thread while the scanner initializes; read
`wallet.runtime` after hydration/sync for the active mode. Scanner replacement
uses generation ownership so a retired worker cannot publish state or runtime
capabilities. Failed threaded initialization falls back to verified ST artifacts.

The scan and prove workers are ES modules. Nested Rayon helpers must remain
emitted modules too. Keep SDK asset URLs relative to `import.meta.url`; the Vite
plugin prevents dependency pre-bundling from invalidating them. Custom artifact
paths are resolved once against the document base, shared by both workers, and
select ST rather than guessing a threaded companion binary.

## Scanning

The browser speaks binary gRPC-Web to a CORS-enabled light server, or uses a
configured HTTP block pipe. It cannot speak native HTTP/2 gRPC directly. Native
desktop clients use the engine's native transport instead.

Rust trial-decrypts compact outputs locally. The shared selective-shard scanner
uses verified subtree roots to avoid hashing completed interior shards with no
wallet notes. Birthday, marked, tip, and recent spend-anchor boundaries are
retained. Per-height metadata and chain-link checks remain necessary even for
empty blocks. Sparse storage must not invent a dense frontier or derive a
checkpoint from a later downloaded root. See [sync invariants](SYNC.md).

Initial catch-up prefetches bounded ranges and reports progress without clearing
saved history. Follow-on sync yields and avoids eager work intended for a long
restore. Subtree roots are refreshed for existing wallets so settled leaves can
be pruned without resetting the wallet. Enhancement may continue at an unchanged
tip after a failed memo or transparent range request.

## Storage and concurrency

The Rust `zstk1` snapshot is distinct from the browser's persistence envelope.
It contains viewing material, notes, tree state, scan coverage, pending
transactions, and history. It excludes the spending phrase.

IndexedDB stores larger snapshots in content-defined chunks of at most 32 KiB,
with bounded index pages and an atomic manifest/preview commit. Byte-identical
chunks are reused and unreachable chunks retired in the same transaction.
This avoids repeated replacement of a large browser value. Chunking does not
add encryption. Failed quota writes keep the last committed wallet.

Checkpoint writes can skip serialization when the current revision has already
committed, but still validate wallet identity and revision. Restore and spend
commits always perform their required atomic save. Hydration may migrate or
repair data, so the next sync can still need one new save.

Generation guards isolate replacement operations, stale callbacks, and tab
conflicts. Guarded view-only restore can prepare application state before its
first commit, but must not publish a session before the SDK confirms the commit.
Cancellation before commit preserves the previous wallet; cancellation after
commit does not undo it. See [lifecycle hooks](SDK.md).

## Spending and hardware

Rust constructs and proves transactions. Orchard/Ironwood proofs use in-process
circuits; Sapling proving parameter files are separate and must never be embedded
in these binaries. The proving worker can initialize early, but expensive key
prewarming waits until a funded sync so it does not compete with initial catch-up.

A seed send reserves and saves its transaction before network submission. Unknown
submission outcomes retain pending state. Explicit node rejection releases the
reservation. Origin-wide spending exclusion and revision checks prevent two tabs
from spending the same notes or replacing newer state.

Hardware accounts use PCZTs. Apps supply QR/WebHID transport; Rust verifies every
returned signature against the transaction. Hardware seed attachment is refused.
Transport cancellation before broadcast releases reservations. Unsupported pools,
networks, firmware, or transparent hardware inputs produce explicit errors rather
than silently switching signing methods. See [hardware API](SDK.md#hardware-wallets)
and [third-party provenance](THIRD_PARTY.md).

## Network and key boundaries

Compact scanning does not send viewing keys to a server. Providers still see
IP addresses, ranges, timing, and broadcasts. Default memo enhancement can send
wallet transaction IDs. Remote transparent-address lookups require explicit
opt-in. Public all-pool and memo ranges depend on a separately supported gateway
protocol; they never silently fall back to selective lookups.

Spending seeds use session memory and optional encrypted vaults. Passkey PRF,
challenges, user verification, and record encryption belong in `@z-stack/passkey`.
The SDK's seed adapter does not implement application login or an off-device
backup service. See [security and privacy](SECURITY.md) and
[the passkey guide](../packages/passkey/README.md).

## Source and distribution

Repository consumers can select `@z-stack/source`; preview archives expose only
built JS and declarations. Both binary variants, integrity manifests, and worker
helpers are required when packing. A source fingerprint prevents packing a
binary left over from earlier Rust changes. Browser integrity verification binds
the binary to its manifest; neither mechanism establishes trust in a compromised
host or replaces a security review.

Run `pnpm test:packages --browser` for installed-archive create/restore/sync/reload
in ST and MT. Run funded regtest and separate consumer integration checks before release;
compilation with typed fixture bindings is insufficient for wallet verification.
