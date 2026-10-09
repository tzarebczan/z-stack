# App-owned remote backup

A runnable WebAuthn account and encrypted-backup example. It uses the optional
`@z-stack/sdk/services` interfaces; the wallet engine needs none of this.
PRF encrypts the vault locally. The server verifies WebAuthn and stores public
credential metadata, hashed sessions and the encrypted record in SQLite.
No email, Google account, wallet address, viewing key or chain query is required.

## Run

Build and pack the SDK from the repository root (`pnpm build:sdk`, then
`pnpm pack:sdk`). Copy this example outside the checkout, then run the following
commands in that copy. Install paths below refer to the original `artifacts/`
directory; the SDK archive includes its matching core and passkey helpers.

```sh
npm install /path/to/sdk-alpha/z-stack-sdk-0.1.0-alpha.4.tgz
npm run server
```

In another terminal, run `npm run dev` and open `http://localhost:5173`.
Node 22.18+ is required for SQLite. Use localhost, not a LAN hostname or IP.
The server listens on `127.0.0.1:3010`; Vite proxies `/api` to it. Data is saved
in the owner-only `.data/` directory, outside the SDK. Use a disposable phrase.
This example does not create or sync a funded wallet.

The copy generates its own npm lockfile when the archives are installed. The
acceptance suite follows this same fresh-install path; no template lockfile with
machine-specific archive paths is committed.

1. Enter a test phrase. Prepare registration, then create the passkey.
2. Upload the encrypted backup. Completion requires exact-generation readback.
3. Forget the local vault and sign out. Sign in with the same credential,
   retrieve the encrypted record, then unlock it locally.
4. Forgetting the remote backup writes a revisioned tombstone. Account deletion
   removes server credentials, sessions and backups; local copies remain.

A synced passkey alone contains no vault ciphertext. PRF must be available to
protect/unlock this example. Unsupported providers keep the phrase recovery path;
the example does not pretend that a non-PRF passkey encrypts a backup.
Prepare challenges before the next click so the WebAuthn ceremony begins with
user activation. The UI separates local protection, account verification, upload,
retrieval and decryption; each can fail independently.

## Replace the backend

The client providers are in `src/providers.ts`; the record protocol is validated
in `server/record.ts`. The server checks signatures, user verification, RP/origin,
expiring cookie-bound one-use challenges and credential ownership. Session owners
come from verified credentials, never from a requested account ID. SQLite commits
revision comparison and replacement together before acknowledging an upload.
A lost response must be reconciled against the exact attempted record, without
reading a newer revision and blindly overwriting it. Logout, local forget,
remote forget and account deletion have separate meanings.

This loopback-only reference deliberately refuses hosted origins. Before hosting
an adaptation, implement TLS, `Secure` cookies (consider `__Host-` with `Path=/`),
production rate limits, persistent challenge/session policy, account recovery,
credential revocation, deletion/retention and independent security review.
Review your proxy and platform logs too: the SDK cannot suppress them.
The bounded allowlist rejects accidental plaintext fields, but a server cannot
prove arbitrary uploaded bytes are encrypted. Client cryptography remains part
of your trust boundary. SQLite deletion is logical deletion, not a guarantee
that filesystem, WAL, snapshots or provider backups erased every historical byte.

## Verify

`npm test` checks ownership, revisions, tombstones, deletion and HTTP rejection.
`npm run build` checks the independently installed package types and production
bundle. From the SDK root, `pnpm test:packages:browser` also verifies real server
WebAuthn with Chromium's virtual PRF authenticator, checks separate accounts and a lost upload acknowledgement, clears browser
storage, signs in again and recovers the backup. A non-PRF authenticator rejects
local protection without verifying an account or discarding the input phrase. That does not certify physical devices or
synced credential providers.

See [accounts and backups](https://github.com/tzarebczan/z-stack/blob/main/docs/SERVICES.md),
[adapter acceptance](https://github.com/tzarebczan/z-stack/blob/main/docs/ADAPTERS.md) and
[security boundaries](https://github.com/tzarebczan/z-stack/blob/main/docs/SECURITY.md).
