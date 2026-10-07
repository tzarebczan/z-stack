# Optional accounts and backups

The wallet works without signing up. Account registration, Google sign-in,
server sessions and remote backups are independent services. Import optional
client primitives and provider types from `@z-stack/sdk/services`; it does not
load the engine, open a database, make requests or choose a server on import.
The same primitives are available separately in `@z-stack/passkey`.

## Responsibilities

| Layer | Owns | Requires a backend? |
| --- | --- | --- |
| `@z-stack/sdk` wallet | Keys, scanning, proofs, local wallet snapshots, chain transport | A configured light server; no account service |
| Local seed protection | Passphrase encryption or passkey PRF, local encrypted seed | No |
| `@z-stack/sdk/services` | WebAuthn client ceremonies, sanitized responses, vaults and storage/provider interfaces | No; only when your app supplies a remote implementation |
| Application account provider | Registration, verification, OAuth, sessions, account deletion | Your choice of platform |
| Application backup store | Authorization, encrypted-record persistence, revisions, retention and deletion | Your choice of storage |
| Fiat and swaps | Provider accounts, consent, KYC, quotes and transfers | Separate provider integration |

Your application owns session endpoints, account authorization and backup
formats. Choose your own server or service, local protection alone, or a
hardware wallet. The light server protocol is separate from the account/backup
API.

## Protect locally first

```ts
import { createPasskeyVault } from "@z-stack/sdk/services";

const vault = createPasskeyVault({
  rpName: "Acme Wallet",
  id: "primary",
  purpose: "zcash-seed",
});

// Call from a user click. No remote account is created.
const registered = await vault.protect(words, {
  userName: "Alice · Travel wallet",
  userDisplayName: "Alice · Travel wallet",
});
```

The default store is IndexedDB. `PasskeyVaultStore` lets an app supply its own
storage. For a remote backup alongside a local vault, keep the local store and
explicitly copy `vault.export()` to your remote store. Do not turn every local
read or unlock into a network dependency. Keep backup consent and status
separate from wallet readiness.

The wallet's `registerPasskeySeed()` helper uses its guarded local seed store.
An independent vault is application-owned: export/import and deletion do not
implicitly modify the wallet's seed store. On unlock, attach the recovered
phrase through the wallet API and wipe the returned buffer when done.

## Add a server account

Implement `PasskeyAccountProvider<Session>` with your own HTTP client and session
representation. Its challenge result includes WebAuthn JSON options and an
opaque `requestId`; the server binds that identifier to a short-lived challenge
and the intended session/account. The interface does not prescribe URL paths,
cookies, bearer tokens, a database schema or hosting.

Fetch options before showing the registration button. Challenges are short-lived
and single-use: discard the prepared handler after an attempt and fetch fresh
options if the click is delayed. From the next click, pass the options to the
client ceremony, then send the sanitized response:

<!-- sdk-example: vault-account-registration -->
```ts
import type { PasskeyAccountProvider, PasskeyVault } from "@z-stack/sdk/services";

// account and vault are your instances; Session is your application's type.
export async function prepareRegistration<Session>(
  account: PasskeyAccountProvider<Session>,
  vault: PasskeyVault,
) {
  const challenge = await account.registrationOptions();
  return async function registerFromClick(words: string) {
    const registered = await vault.protect(words, { server: challenge.options });
    return account.verifyRegistration(challenge.requestId, registered.response);
  };
}
```

This example registers a new vault. For another credential on an existing
vault, use `prepareAddPasskey()` and call its `register()` from a second click.
For account-only registration, no wallet phrase or PRF evaluation is needed:

<!-- sdk-example: account-only-registration -->
```ts
import {
  createCredential, fromBase64Url, registrationJson, resolveRpId,
  type PasskeyAccountProvider,
} from "@z-stack/sdk/services";

export async function prepareAccountRegistration<Session>(
  account: PasskeyAccountProvider<Session>,
  rpId = resolveRpId(),
) {
  if (!rpId) throw new Error("Passkeys require a valid RP ID.");
  const challenge = await account.registrationOptions();
  return async function registerFromClick() {
    const options = challenge.options;
    const created = await createCredential({
      rpId,
      rpName: options.rp.name,
      user: { ...options.user, id: fromBase64Url(options.user.id) },
      extensions: [{ credProps: true }],
      server: options,
    });
    return account.verifyRegistration(
      challenge.requestId, registrationJson(created.credential),
    );
  };
}
```

The client deliberately drops server-provided extension lists. `extensions`
selects the allowed client extensions; do not pass server PRF or largeBlob
requests through. Resident credentials and user verification are always
required, even if the server options request a weaker policy. Configure your
verifier to require user verification too. Pass your configured RP ID explicitly
when it differs from the page hostname; a server/client RP ID mismatch fails.

For sign-in, obtain `authenticationOptions()`, then pass the server options to
`vault.unlock()` and send `unlocked.response` to `verifyAuthentication()`. Wipe
the unlocked bytes in `finally`, including when verification fails. Account-only
sign-in can use the same preparation pattern without unlocking a vault:

<!-- sdk-example: account-only-authentication -->
```ts
import {
  getAssertion, assertionJson, resolveRpId, type PasskeyAccountProvider,
} from "@z-stack/sdk/services";

export async function prepareAccountSignIn<Session>(
  account: PasskeyAccountProvider<Session>,
  rpId = resolveRpId(),
) {
  if (!rpId) throw new Error("Passkeys require a valid RP ID.");
  const challenge = await account.authenticationOptions();
  return async function signInFromClick() {
    const assertion = await getAssertion({
      rpId, extensions: [{}], server: challenge.options,
    });
    return account.verifyAuthentication(
      challenge.requestId, assertionJson(assertion.credential),
    );
  };
}
```

Only successful server verification establishes a remote session. Local
registration or a successful PRF unlock does not authenticate API requests.
A failed server verification can leave a valid local vault; show account setup
as incomplete and allow retry without discarding the user's local recovery.
Google/OIDC providers can be integrated independently; do not derive spending
keys from an email address, account ID or OAuth token.

## Persist encrypted backups

`PasskeyVaultRecord` contains encrypted secret material plus public metadata,
including RP ID, credential IDs, vault purpose and the chosen account name.
It does not contain the phrase, decrypted data key or PRF output. The server
still sees metadata and request timing; avoid unnecessary identifying labels.
The consumer application's recovery-key envelopes are a different format and are not interpreted
by this module.

A remote `PasskeyVaultStore` must implement this contract:

- `get(id)` returns the validated stored row, including a `forgotten: true`
  tombstone. `undefined` means no row at all. Do not turn a tombstone into
  absence: its revision prevents older writes from recreating a forgotten vault.
  Treat authentication, authorization, connectivity and parsing failures as
  errors. A missing backup is not a completed backup.
- `put(record, expectedRevision)` atomically compares the stored revision,
  rejects a mismatch with `PasskeyError("conflict")`, and resolves only after
  durable persistence. `undefined` means create only if absent, not overwrite.
- `forget()` writes a tombstone with `put` and compare-and-swap; it never calls
  `delete`. Validators must allow its empty `userId`, `userName`, `data.iv`,
  `data.ct` and passkey list (and empty RP ID if no vault existed), while still
  validating format, ID, purpose, revision, timestamps and size.
- `delete(id)` is an authenticated administrative hard-delete, scoped to the
  owner. It is separate from `forget()`, removes the revision barrier, and does
  not revoke portable copies. Retention and revocation are app responsibilities.
- Every operation honors cancellation and is bound to the authenticated owner.
  The server derives ownership from the session, never a client-supplied user ID.

Keep the revision observed on download or your last successful backup as the
next write's expected revision. Fetching the latest revision just to overwrite
it defeats conflict protection. Resolve conflicts explicitly; never silently
replace newer recovery material from another device.

Mark backup complete only after the remote store confirms the write. Preserve
the previous usable backup while a replacement is pending, and retain a local
recovery route if the server fails. On recovery, authenticate, retrieve and
validate the record, then call `vault.import(record)`. Import rejects a different
vault ID, RP ID or purpose and refuses to replace an existing live vault unless
explicitly requested. A tombstone is not a recovery backup: import rejects it
with `no-vault`, including when replacement is requested. Your remote adapter
must validate the complete record schema and bound its size before it reaches
import. Credential revocation and
revision checks must also survive device loss and server-side rollback.

## Server security and lifecycle

Use a maintained server-side WebAuthn verifier. Check the challenge, exact
origin, RP ID, signature, user verification and credential ownership. Enforce
single use and expiration, rate-limit ceremonies, and account for synced
credentials when evaluating counters. Verify OAuth state and redirect URIs,
and protect cookie-authenticated writes against CSRF.

Send only `registered.response`, `unlocked.response`, `registrationJson()` or
`assertionJson()` to a verifier. Raw credential serialization can include PRF
outputs or largeBlob bytes. Never upload seeds, unlocked buffers or decrypted
wallet snapshots as part of registration or backup.

`wallet.close()`, `wallet.forget()` and provider `logout()` have different
scopes. Closing locks and releases the engine; forgetting removes local data
while keeping the client usable; logout
invalidates a server session. Your application orchestrates these actions,
cancels in-flight account/backup work, and prevents late results from attaching
to a different logged-in user. Remote deletion and passkey removal require
separate explicit actions. No background upload, OAuth login or account
registration is started by wallet construction or sync.

Local encrypted-seed and passkey deletion rejects if storage is unavailable.
Keep deletion pending and offer a retry; success must mean the local records
were removed. Removing a local passkey record does not remove its credential
from the user's password manager.

## Connect your own unlock method

The public wallet accepts `WalletUnlocker`, which returns a seed for local Rust
verification. It does not prescribe a login provider, an API URL or backup format.
The wallet passes a cancellation signal and the local viewing key for secret
selection. Keep that viewing key local; a provider should not upload it implicitly.

<!-- sdk-example: custom-wallet-unlock -->
```ts
import { createWallet, indexedDbWalletStorage, type WalletUnlocker } from "@z-stack/sdk";

export function openWallet(unlocker: WalletUnlocker) {
  return createWallet({
    network: "testnet",
    server: "https://zcash-testnet.chainsafe.dev",
    storage: indexedDbWalletStorage({ name: "my-app-wallet" }),
    unlocker,
  });
}
// From the user's spend action: await wallet.unlock(); await wallet.send(...).
```

`passkeyUnlocker(vault)` adapts an independently configured local passkey vault.
For strict user-activation paths, start its prepared ceremony from the button
handler and pass the result promise to `wallet.unlock(...)`. Do not claim that
awaiting a network or IndexedDB read preserves activation on every browser.
Failed/cancelled unlocks never authorize a spend. Lock, close and replacement
invalidate late unlock results; an unlocked seed is never a server session.

`WalletStorage` is a separate local transactional boundary; see [storage](STORAGE.md).
An encrypted remote vault store remains optional and application-owned. Never
mark backup complete from local encryption or a queued HTTP request alone:
retain the recovery key, await durable server acknowledgement, and verify the
saved record can be retrieved. A timeout after a write is an unconfirmed backup,
not proof the server rejected it. Keep the prior confirmed backup until a new
record is confirmed. No SDK helper turns local and remote writes into an atomic
transaction.

See [security and privacy](SECURITY.md) and the [passkey reference](../packages/passkey/README.md).
