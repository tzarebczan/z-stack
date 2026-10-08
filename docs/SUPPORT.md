# Preview support and validation

The supported integration paths are TypeScript/ESM with Vite or the Next.js
webpack demo, using local browser storage. This is alpha software. No production
support SLA or independent security-audit certification is offered. Use GitHub issues for non-sensitive
integration reports, with package version, browser/OS, runtime mode and a small
empty-wallet reproduction. Never attach seeds, viewing keys, wallet databases,
tokens, memos or raw error causes. Follow [private reporting](../SECURITY.md) for
security findings; the contact is documented there.

| Platform / capability | Evidence | Limits |
| --- | --- | --- |
| Desktop Chromium, Firefox and WebKit engines on Ubuntu | Fresh external archives; real ST/MT two-thread engines, setup checks, IndexedDB contracts, create/restore/sync/reload/close | Playwright Linux engines, not physical Safari/iOS devices; isolation requires require-corp in tested WebKit |
| Vite 6 + TypeScript 5.9 | Standalone, React 18, local-passkey and remote-backup production builds | Additional bundlers beyond the listed integrations remain unverified |
| Next.js 16.3.8 + React 19.3 + webpack | Installed-archive production shell, headers, workers, recovery and navigation gate | Turbopack and physical mobile devices remain unverified; funded demo UI acceptance is on isolated regtest |
| Node 22/24/26 ESM / server import | Public exports under network/storage/worker traps; React renderToString | Not a Next.js, Remix or server deployment test |
| Storage and lifecycle | Transactional storage, quota/abort/CAS/forget and owner regressions; React teardown queue | Browser crash/eviction matrices remain incomplete |
| Passkey vault | Crypto/PRF regressions; real server verification with Chromium virtual authenticator, encrypted readback, account isolation, lost receipt, unsupported PRF, storage-loss recovery and deletion | Physical provider/device and synced cross-device matrix incomplete |
| Native engine | Rust checks/tests; separate loopback bridge client | No native executable release inventory or all-platform certification |
| Ledger hardware | Transport/protocol regression tests | Physical device/firmware combinations remain unverified |
| Funded native and browser engines | Disposable wallets, loopback regtest shielding, send, pending/mined, memo and restart/reload | Browser uses compact transparent scanning and local validator submission fallback; no live mainnet spend or physical hardware certification |
| iOS / Android / constrained-memory devices | Not fully tested for this candidate | Background/resume, eviction, memory and proving limits need device testing |
| Custom assets / hosting / non-Vite framework | Documented deployment requirements; integrity/asset tests | Custom threaded paths and deployment targets beyond the tested Vite/Next webpack builds need separate acceptance |

The engine uses Zakura Common 2.2.0's NU7 schedule: testnet activation is
**4,465,026**; mainnet activation remains unset. Transaction construction,
proof circuit selection and date-based birthdays follow that schedule.
See [consensus and dependency details](UPSTREAM.md#nu7-consensus).
A reachable server or empty-wallet sync does not establish funded compatibility.
Use an isolated NU7-enabled regtest for reproducible acceptance; a public faucet
and light server must follow the same chain before testing a funded round trip.

The repository's CI checks installed archives with real WASM engines and runs
controlled browser and provider fixtures. These checks do not certify a live
provider or every device. Funded regtest acceptance is a separate, opt-in check;
it is not a live mainnet spending test. See [verification commands](RELEASE.md)
for reproducing the checks. A build using typed WASM stubs does not test the engine.

Accounts, Google login, cloud backup, fiat/KYC and gas sponsorship belong
to the integrating app. They are neither required by the SDK nor certified by
its test suite. The local-passkey example uses no backend.

Lock removes spending access. Viewing state, balance and activity can remain
readable on the device; it is not encrypted-history lock. Integrators can hide
those views in their UI, but that does not encrypt persisted viewing data.
