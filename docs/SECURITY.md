# Security and privacy

Wallet key derivation, scanning, transaction construction and proofs run in
Rust on the user's device. Local seed encryption uses browser WebCrypto;
passkey ceremonies use WebAuthn and PRF. The SDK does not require a remote
custody service. A web app still trusts the JavaScript and WASM delivered
by its host, its browser, and its dependencies. XSS or a compromised application
can access an unlocked wallet. A passkey does not isolate an unlocked seed from
malicious code running on the same origin.

## Network disclosures

| Operation or setting | Information visible to the provider |
| --- | --- |
| Compact scan | IP address, block ranges, birthday tree height, timing |
| Broadcast | IP address, transaction bytes, timing |
| `memoFetch: "auto"` (opt-in) | IDs of wallet transactions retrieved for memo recovery |
| `memoFetch: "on-demand"` (default) | Same IDs, only when `fetchMemos()` is requested |
| Remote `transparent: true` | Wallet transparent addresses |
| `memoFetch: "shared"`, supported gateway | Public block ranges and their shielded transactions |
| `transparentScan: "compact"`, supported protocol | Public all-pool block ranges; matching is local |

Shared mode requires the server's explicit capability; it must not silently
fall back to transaction-ID retrieval. Standard gRPC-Web light servers do not
necessarily implement that extension. See [the protocol](PUBLIC-RETRIEVAL.md).

SDK chain requests omit ambient cookies and referrers and reject redirects.
A native bridge token is sent only to its configured bridge. Custom transports
own their network policy. These controls do not conceal IP addresses or provide
Tor, a relay, or anonymity from request timing.

## Recovery and locking

The browser snapshot contains viewing material and decrypted wallet history.
Chunking and integrity verification do not encrypt it. Protect the device and
origin accordingly. JavaScript strings cannot be reliably zeroized; wipe byte
buffers where possible and limit the lifetime of secret strings.

- A new wallet returns its phrase once. An imported phrase stays the user's
  recovery phrase; importing does not create a replacement phrase.
- Browser seed vaults protect spending material at rest with a passphrase or
  WebAuthn PRF. They are separate from viewing snapshots.
- Synced passkeys synchronize credentials, not necessarily encrypted vault
  records. Cross-device recovery needs the record, a supported portable vault,
  or the recovery phrase. Confirm a server backup before calling it complete.
- `unlockPolicy: "each-spend"` is the default: unlock explicitly before every
  spend. `"session"` retains the seed only in memory. Reload locks; no plaintext
  seed is persisted and saved data cannot override the application's policy.
- `lock()` drops the seed. `await close()` also cancels work, clears listeners
  and releases ownership. `forget()` deletes the local wallet and remains usable;
  passkeys stay unless explicitly included. Provider logout is independent.

Wallet replacement commits its identity, viewing snapshot and policy together.
Seed vaults and remote backups have separate commits. Keep the prior recovery
phrase or encrypted record until replacement succeeds; prepare a separate record
or revision rather than overwriting the only usable backup. A page kill cannot
run application rollback. The experimental `/engine` inline passkey/passphrase
conveniences also write separately from the snapshot and do not provide atomic
backup replacement. New apps should use `createWallet` and explicit service
adapters with retained prior records.

RP IDs are origin security boundaries. Avoid a shared parent-domain RP ID
unless every relevant subdomain is trusted. Register and unlock from real user
clicks; cancelling a passkey sheet is a normal outcome. The [passkey guide](../packages/passkey/README.md)
describes challenges, user verification, PRF handling, and ciphertext transfer.

## Spending and persistence

The SDK persists a pending seed transaction before broadcasting it. A network
failure may have happened after submission: keep `broadcast_failed` pending
and check its transaction ID before offering another payment. An explicit `broadcast_rejected`
permits releasing the reservation. Hardware signatures are verified in Rust
against the transaction before broadcast.

Cross-tab spending uses origin-wide exclusion and saved-revision checks. A
stale tab cannot overwrite a newer wallet save. A cancelled or failed guarded
restore preserves the previously committed wallet. Quota failures preserve the
last successful checkpoint; they do not make unsaved progress durable.

A native engine bridge controls spending keys. Keep it loopback-only, protect
its token, and do not proxy it to the internet. View-only wallets and hardware
wallets have different spending capabilities; do not treat address visibility
as authority to spend.

## Diagnostics

Browser runtime console diagnostics contain fixed stage labels, not wallet data
or exception payloads. Public errors and events still carry data needed by the
application; do not upload their messages, causes or payloads to telemetry.
Native engine tracing also uses fixed stage labels. This policy does not cover
dependency diagnostics, operating-system logs, intentional CLI output or
application telemetry; review those before deploying your app.

## Application responsibilities

Use HTTPS, restrict scripts on the wallet origin, secure authentication and
backup APIs, and avoid recording addresses, transaction IDs, notes, or recovery
material in analytics. Test logout, storage loss, multi-tab conflicts, provider
outages, uncertain broadcasts, and cross-device recovery in your app. Keep fiat
and swap services behind explicit user decisions; they have separate metadata
and identity requirements.

This guide describes boundaries and safeguards, not an audit certificate.
Report vulnerabilities through [SECURITY.md](../SECURITY.md).
