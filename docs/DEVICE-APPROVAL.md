# Device approval by QR

`@z-stack/passkey/device-approval` is an optional, dependency-free WebCrypto
sealed-box transfer. It carries a wallet recovery phrase or a unified full
viewing key between two browser sessions. It does not authenticate an account,
export a wallet, import a profile, perform Zcash cryptography or use a server.
The passkey vault and wallet engine are not included in this entry's bundle.

## Integrate the transfer

The receiving device creates a request in memory. Display its encoded string as
a QR, or let the user copy it directly to the source device. Never put request
or response strings in URLs, analytics, logs or persistent browser storage.

```ts
import {
  createDeviceApprovalRequest,
  parseDeviceApprovalRequest,
  sealDeviceApproval,
  parseDeviceApprovalResponse,
} from "@z-stack/passkey/device-approval";

const receiver = await createDeviceApprovalRequest({
  origin: location.origin,
  mode: "wallet", // or "view-only"; the source cannot change this mode
  ttlMs: 300_000,
});
showRequestQr(receiver.request, receiver.details.expiresAt);
```

On the source device, parse the scanned request with that device's own
`location.origin`. Display the requested access and expiry. Require fresh local
reauthentication and explicit approval to export from the selected wallet.
Only then obtain the payload from the wallet SDK and seal it:

```ts
const details = await parseDeviceApprovalRequest({ request, origin: location.origin });
// App: show details.mode, then require fresh authentication and export approval.
const response = await sealDeviceApproval({ request, origin: location.origin, payload });
const comparison = await parseDeviceApprovalResponse({
  request, response, origin: location.origin,
});
showResponseQr(response, comparison.verificationCode);
```

On the receiver, inspect the response with `parseDeviceApprovalResponse` using
`receiver.request`. Display the resulting comparison code on both devices and
require the user to compare it physically before continuing. This code binds
this request and this encrypted response, including the sender's ephemeral
public key. It does not identify the source account or attest its ownership.
A request-only comparison code is also available in `receiver.details`; it
cannot replace comparing the response code.

After confirmation, call `receiver.open(response)` once. Validate the resulting
mnemonic or full UFVK, network, birthday and wallet identity with the actual
wallet engine. Require explicit receiver confirmation and a new local password
before importing into a new local profile. Do not replace existing profiles,
transfer the source's password/passkey credential, rotate its backup or submit
transactions as a side effect. Hardware and view-only sources must refuse a
wallet-mode export and ask the receiver to start a new view-only request.

Call `receiver.dispose()` on cancellation, navigation, lock, logout or profile
change. A successful open consumes the request; replay is refused. A malformed,
wrong-device or tampered response leaves an unexpired request usable, so a bad
camera scan does not destroy the flow. Concurrent opens are refused. Disposal
or expiry during an asynchronous open prevents returning plaintext.

## Payloads and validation

`DeviceApprovalPayload` has two strict modes:

- `wallet`: `{kind:'wallet', mnemonic, fingerprint, network, birthdayHeight,
  label?}`. The fingerprint is 64 lowercase hex characters. The app must verify
  it using the wallet engine's seed fingerprint; this module only bounds the
  mnemonic syntax and fields.
- `view-only`: `{kind:'view-only', ufvk, fingerprint, network, birthdayHeight,
  label?}`. The fingerprint may be the literal `view-only` sentinel or the full
  UFVK identity from `deviceApprovalViewingKeyFingerprint({ufvk,network})`.
  This identity hashes the complete key and network with a purpose label; it is
  not a ZIP32 seed fingerprint. The module verifies that full-key hash when
  supplied. The engine must still validate and import the full UFVK.

Network is `mainnet`, `testnet` or `regtest`; birthday is an unsigned 32-bit
height. Labels are limited to 120 UTF-8 bytes without control characters. The
module rejects unknown fields, mixed secret/viewing-key payloads, access-mode
changes, noncanonical encodings and unsupported versions. Request JSON is at
most 2 KiB; decrypted payload JSON is at most 8 KiB; response JSON is at most
14,000 bytes. These are transport limits, not a promise that one QR can carry
every UFVK. Keep copy/paste available when the QR encoder cannot fit a response.

Only exact canonical HTTPS origins are accepted, with HTTP allowed for
`localhost`, `127.0.0.1` and `[::1]` development. Origins include the port.
Requests expire within five minutes of creation; sources allow at most thirty
seconds of positive clock skew. `now: () => number` is an injectable clock for
tests, not an authorization override for production.

## Cryptographic construction and limits

Each receiver creates a fresh P-256 ECDH key pair and a random 256-bit session
nonce. Its private key is nonextractable and held only by the receiver closure.
Each source response creates another fresh nonextractable P-256 key pair.
ECDH output enters HKDF-SHA256, using SHA-256 of the complete canonical request
as salt and a versioned purpose string plus the request and sender public key
as context. The derived nonextractable AES-256 key encrypts the canonical
payload with AES-GCM, a fresh random 96-bit IV and a 128-bit authentication tag.
A separate purpose string, the complete request and sender public key form the
additional authenticated data. The response includes the request hash, sender
public key, IV and ciphertext. The comparison code contains 128 bits of
SHA-256 of that complete canonical response.

These are standard [WebCrypto primitives](https://www.w3.org/TR/2017/REC-WebCryptoAPI-20170126/),
used directly to keep this optional entry dependency-free. This construction is
not HPKE and has not received an independent cryptographic audit. It provides
no sender identity authentication by itself: physical scanning and comparing
the response code are part of the protocol. Both browsers, the displayed QR,
and their execution environments must be trusted. Browser malware or a
compromised same-origin app can read the wallet before encryption or after
decryption. This flow does not revoke access already held by the source.

Temporary plaintext byte buffers and ECDH output are overwritten in `finally`
blocks. Disposal drops references to retained private keys. JavaScript strings,
native CryptoKeys, browser internals and garbage collection cannot be reliably
physically erased; the API makes no such promise. Keep returned payloads only
as long as the local import needs them. No credential, private key or symmetric
encryption key is serialized in either QR.

## Verify the installed entry

```sh
pnpm build:packages
node --conditions=@z-stack/source --import tsx --test packages/passkey/test/device-approval.test.ts
pnpm test:device-approval:package --browser
```

The installed-archive test uses the same real WebCrypto contract in Node,
Chromium, Firefox and WebKit. CI runs each browser separately. The contract
covers wrong sessions/recipients, origin and mode binding, canonical bounds,
tampering, expiry, successful-open replay, concurrent opens, invalid-scan retry
and disposal during decryption. Node additionally verifies nonextractability
and overwriting of temporary cryptographic buffers.
