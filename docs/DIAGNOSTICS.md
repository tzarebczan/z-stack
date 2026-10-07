# Check an integration

Import `checkWalletSetup` from `@z-stack/sdk/diagnostics` in browser client code.
Importing it does not start workers, open storage, contact a server or initialize
the wallet.

The entry point imports only cancellation helpers and read-only ownership
metadata. It does not import wallet initialization, storage adapters, passkeys
or the engine. The installed-package gate checks this dependency graph.

Calling it without options checks capabilities only:

```ts
import { checkWalletSetup } from "@z-stack/sdk/diagnostics";
const report = await checkWalletSetup();
```

Each check returns a fixed `code`, `status` and `message`. Reports contain no
URLs, addresses, viewing keys, transaction IDs, provider responses or error
causes. Nothing is logged, collected or uploaded. Your app can display the report
or let the user deliberately include it in an integration issue.

Explicitly request deployment probes from a diagnostics button:

```ts
const report = await checkWalletSetup({
  assets: true,
  worker: true,
  timeoutMs: 60_000,
  signal: controller.signal,
});
```

`assets: true` downloads and hashes the selected bundled ST/MT engine and checks
the integrity manifest and WASM content type. Requests omit credentials and
referrers and refuse redirects. This can download tens of megabytes. Custom
single-thread assets use `{ wasmUrl, integrityUrl }`. Integrity detects mismatched
files; it does not authenticate a compromised host that replaced both files.
`worker: true` starts and terminates a tiny module worker to test URL/CSP support;
it does not initialize the scanner or prover.

An optional `server(signal)` callback is app-owned. Return only
`{ reachable, sharedMemos }` from a public chain capability probe, and honor
cancellation. No server is selected automatically. Do not send wallet identifiers
from this callback. Shared memo capability is never inferred from a successful
ordinary gRPC-Web request.

`ok` means no failed check; warnings and skipped probes remain in the report.
IndexedDB presence does not verify quota, persistence or durability. An active
page owner is a warning; it does not inspect another tab's lease. Isolation may
be unavailable while the single-thread engine works. The whole check has a
bounded deadline; a stalled provider cannot keep it pending indefinitely.

Use [adapter acceptance](ADAPTERS.md) for storage semantics and the
[release checks](RELEASE.md) for actual engine/browser verification.
