# @z-stack/passkey

**Alpha.** Part of the matching `0.1.0-alpha.4` archive release; not published to npm.

Protect a secret, such as a wallet recovery phrase, with one or more passkeys.
It is client-only and has no runtime dependencies: native WebAuthn, the PRF
extension and WebCrypto.

```ts
import { createPasskeyVault, getPasskeySupport, isPasskeyCancel } from "@z-stack/passkey";

const support = await getPasskeySupport();        // never prompts; safe on page load
const vault = createPasskeyVault({ rpName: "Acme Wallet" });

// "Protect with a passkey" button
try {
  const { passkey } = await vault.protect(mnemonic, { userName: "alice" });
  if (passkey.synced === false) warn("This passkey lives on this device only. Keep your recovery phrase.");
} catch (e) {
  if (!isPasskeyCancel(e)) show(e.message);         // e.code: "prf-unsupported", "already-registered", ...
}

// "Unlock" button
const unlocked = await vault.unlock();
const words = unlocked.text();
unlocked.wipe();
```

## What it does for you

Each item below has broken at least one real wallet.

| Pitfall | What the package does |
|---|---|
| PRF output used directly as an AES key | HKDF-SHA256 with a per-passkey random salt and a purpose label. It uses envelope encryption, so each passkey wraps one data key. |
| `credential.toJSON()` sends the PRF secret to your server | `unlocked.response` / `registered.response` (and `registrationJson` / `assertionJson`) strip `prf.results` and `largeBlob.blob`. |
| `userVerification: "preferred"` on one call and `"required"` on another | Every call uses `"required"`. Authenticators return a different PRF secret with and without UV. |
| PRF missing from `create()` means "unsupported" | If `prf.enabled` is not `false`, it evaluates PRF on a follow-up assertion. Security keys and Windows Hello only return it there. |
| Reading `getClientCapabilities().prf` | The key is `extension:prf`. `getPasskeySupport()` reads the right keys and never sniffs user agents. |
| Orphan passkeys after a failed setup | A credential abandoned mid-setup is hidden with `signalUnknownCredential`. |
| Duplicate passkeys in the chooser | `excludeCredentials` lists the vault's passkeys. A second passkey from the same provider fails with `already-registered`. |
| Autofill request collides with a button | Explicit prompts abort a pending `mediation: "conditional"` request for you. |
| Providers that reject an extension (older password managers) | Retries without the optional extensions. It never silently drops PRF. |
| Two tabs overwrite each other | Stores compare-and-swap on `revision`. The losing tab gets `conflict`. |
| `127.0.0.1` in development | Reports `rp-id` with a hint to use `localhost`. `localhostUrl()` builds the link. |

## Prompts

- **`protect`:** one sheet if the authenticator returns PRF at creation (Chrome with Google Password Manager, Safari 18+ with iCloud Keychain, recent 1Password and Bitwarden). Two otherwise (security keys, Windows Hello). `portable` adds one more.
- **`unlock`:** one sheet.
- **`prepareAddPasskey` / `register`:** unlock with an existing passkey, then register the new one from a second click (two or three sheets). `register()` calls `credentials.create` before it awaits.
- **`addPasskey`:** both steps in one call, for callers that do not need a separate activation for the second sheet.

Call `protect` and `unlock` straight from a click handler. Call `prepareAddPasskey()`, then `prepared.register()` from the next click: the unlock sheet uses up the first gesture. Some browsers drop user activation after an unrelated `await`.

## New devices and backups

Synced passkeys (iCloud Keychain, Google Password Manager, 1Password, Bitwarden) give the same PRF secret on every device. What a new device lacks is the vault record, which holds only ciphertext. There are three ways to get it there:

1. **`portable: true`** writes an encrypted copy into the passkey itself (largeBlob) when the authenticator supports it (security keys, iCloud Keychain). `unlock()` on a device with no record restores from it. It is ignored when the RP ID is a parent domain of the page (`example.com` on `wallet.example.com`): every subdomain could read that copy and request the same PRF output. It is also ignored on `localhost`, because RP IDs have no port and every local server shares it. Subdomains of the page's own hostname can use its RP ID too, so enable it only on a hostname whose subdomains you control. The copy is written after the vault is saved, and cancelling that last prompt keeps the passkey without a copy.
2. **`vault.export()` / `vault.import(record)`** lets you store the record anywhere, including your server. The server sees ciphertext and public metadata, including account names, salts and credential IDs, but no decryption key.
3. **The recovery phrase.** A passkey is a convenience, not a backup. Tell users when `passkey.synced === false`.

## Autofill (conditional mediation)

```html
<input name="username" autocomplete="username webauthn" />
```
```ts
if ((await getPasskeySupport()).conditionalGet) {
  vault.unlock({ mediation: "conditional" }).then(onUnlocked, (e) => { if (!isPasskeyCancel(e)) show(e); });
}
```

## Using your server's challenges

If your backend verifies passkeys (for example with SimpleWebAuthn), pass its JSON options through. One prompt then signs the server's challenge *and* unlocks the secret:

```ts
const options = await fetch("/api/passkeys/authenticate/options").then((r) => r.json());
const unlocked = await vault.unlock({ server: options });
await fetch("/api/passkeys/authenticate/verify", { method: "POST", body: JSON.stringify(unlocked.response) });
```

`server` works the same way for `protect` and `addPasskey`, with creation options.

## Managing passkeys

- **`passkeys()`** lists the vault's passkeys, with provider name, whether each is synced, and created and last-used dates.
- **`renamePasskey()`** sets your own label for a passkey.
- **`removePasskey()`** removes one passkey from the vault.

`removePasskey` and `forget` change this device's record only. They do not call
`signalAllAcceptedCredentials`, because another synced device may still rely on
that passkey. Call `signalAllAcceptedCredentials` yourself only from a source of
truth that knows every device's passkeys, such as your server.

Local removal blocks that credential from reopening or rejoining this record,
including an unlock racing removal in another tab. It cannot erase an existing
portable copy or exported ciphertext: a device without the revocation record
can still restore the old secret. Do not treat removal as revoking access to a
secret the credential could already decrypt.

## Storage

`indexedDbVaultStore()` is the default. You can also use `memoryVaultStore()`, or
implement `PasskeyVaultStore` (`get`, `put` with an expected revision, `delete`)
on any backend.

## Errors

Every error is a `PasskeyError` with a `code`:

- `cancelled`, `aborted`: the user closed the sheet or the call was aborted.
- `unsupported`: the browser can't do passkeys here.
- `rp-id`: the RP ID doesn't match this origin.
- `already-registered`: this provider already has a passkey for the vault.
- `prf-unsupported`: this passkey can't protect secrets.
- `no-vault`: nothing is saved on this device.
- `vault-exists`: something is already saved.
- `wrong-passkey`: the chosen passkey doesn't belong to this vault.
- `decrypt-failed`: the vault didn't open.
- `conflict`: another tab changed the vault.
- `last-passkey`: that is the vault's only passkey.

`isPasskeyCancel(e)` is true for `cancelled` and `aborted`, so you can stay quiet on those.

`forget()` leaves a tombstone rather than deleting the record, so a write that
started before it (another tab's `protect`, a portable restore) fails its
compare-and-swap instead of bringing the vault back.

Custom stores must return tombstones from `get()`, preserve their revisions,
and accept their empty ciphertext fields. `undefined` means no row; `delete()`
is a separate administrative hard-delete. `vault.import()` rejects tombstones
with `no-vault`, including when `replace` is true.
