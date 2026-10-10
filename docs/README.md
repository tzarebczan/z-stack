# Documentation

Start with the [walkthrough](GETTING-STARTED.md) to build and run a wallet outside
the SDK checkout. The [support matrix](SUPPORT.md) lists tested paths and limits.

## Integrate a wallet

- [Browser integration](INTEGRATION.md): installation, lifecycle and hosting.
- [Wallet API](SDK.md): methods, events, storage, hardware and native clients.
- [Native payment receipts](NATIVE-PAYMENTS.md): experimental Rust approval, recovery and offline regtest scanning.
- [Examples](../examples/README.md): TypeScript, React, Next.js, local passkeys,
  remote backups and optional Base wallets.
- [Diagnostics](DIAGNOSTICS.md), [adapter checks](ADAPTERS.md) and
  [error handling](ERRORS.md): troubleshoot setup and implement recovery.
- [Storage](STORAGE.md): transactional persistence and wallet ownership.
- [Accounts and backups](SERVICES.md): optional services supplied by your app.
- [Passkey vaults](../packages/passkey/README.md): PRF and encrypted records.
- [Base wallet](BASE-WALLET.md) and [swaps](SWAPS.md): optional integrations.
- [Security and privacy](SECURITY.md): keys, metadata, locking and broadcasts.
- [Public range retrieval](PUBLIC-RETRIEVAL.md): optional gateway extensions.

## Build and contribute

- [Contributing](../CONTRIBUTING.md): toolchains and change verification.
- [Package builds and verification](RELEASE.md): archives, bundles and test commands.
- [Architecture](ARCHITECTURE.md), [browser engine](WEB.md) and [sync](SYNC.md).
- [Performance](PERFORMANCE.md): runtime tuning and measurement guidance.
- [Node tooling](NODE.md) and [mainnet services](MAINNET-NODE.md).
- [Upstream dependencies](UPSTREAM.md): pins, aliases and reproducible patches.
- [Licensing](LICENSING.md) and [third-party provenance](THIRD_PARTY.md).

## API reference

Run `pnpm docs:api` to generate a searchable reference under `docs/api/` from
exported TypeScript symbols. Experimental `/lab` helpers are excluded. The
preview bundle includes this reference for offline use. Preview it locally:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory docs/api
```
