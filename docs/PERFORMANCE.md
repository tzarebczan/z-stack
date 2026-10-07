# Performance tuning

Sync and proving costs depend on the wallet birthday, note history, browser,
available memory and chain provider. Measure your application's workload rather
than relying on timings from another wallet or device.

## Browser runtime

The SDK provides single-threaded and threaded WASM engines. Threaded mode needs
SharedArrayBuffer and cross-origin isolation; use the single-threaded fallback
when those requirements are unavailable. See [hosting requirements](INTEGRATION.md)
and [setup diagnostics](DIAGNOSTICS.md). Test production asset paths and headers,
not only the development server. Physical mobile memory and background/resume
behavior remain incompletely tested; see [support](SUPPORT.md).

Ordinary updates of 32 compact blocks or fewer decrypt on the scan worker without
waking Rayon. Larger batches use parallel scanning when available. This does not
resize the worker pool; download, finalization and persistence still consume work.
Keep previously saved history visible while sync updates it, and use wallet
events for progress rather than replacing activity with a loading screen.

## Proving preparation

`createWallet({ prewarmProvingKey: false })` disables automatic proving-key
preparation after funded scans. `wallet.setPrewarmProvingKey(enabled)` changes
future preparation; it cannot interrupt a WASM build already running. Preparation
is enabled by default, and concurrent requests share one job per worker. A send
can still prepare its required proving key on demand. Disabling warm-up trades
background work for additional latency on the first payment.

With a threaded scan worker, proving can reuse that worker's built trees and
Rayon pool. Single-threaded scanning uses the separate prove worker. Keep proving
off the UI thread and show its progress; do not initiate a second payment because
a proof takes longer than expected.

## Persistence and memory

Snapshots retain valid commitment trees so reload can avoid rebuilding them.
Checkpoint writes coalesce and reuse unchanged chunks. An unchanged session
revision can skip serialization while still checking the saved wallet's identity
and revision. Quota errors preserve the last committed snapshot; unsaved progress
is not durable. See [storage](STORAGE.md) and [sync design](SYNC.md).

Large history and snapshot hydration can still require substantial memory.
Use the saved preview for initial rendering, avoid redundant full-history copies
in your UI, and test storage eviction and tab navigation on your target devices.

## Measure without collecting wallet data

Measure cold startup, initial sync, a caught-up block update, save/reload and the
first proof separately. Record the source revision, engine hashes, browser/OS,
runtime mode, thread count, provider and fixture size. Compare identical disposable
fixtures, run repeated samples without concurrent builds, and distinguish CPU,
network, storage and end-to-end latency. Do not log phrases, viewing keys, decrypted
history, transaction IDs or raw errors to obtain timings.

Field arithmetic patches are gated to wasm32 and include reproducible baseline
hashes and tests. See [the dependency ledger](UPSTREAM.md). Validate both WASM
variants and native arithmetic before changing them; benchmark results are not
hardware-independent speed guarantees.
