# How the browser example works

## Local wallet lifecycle

Create shows 24 recovery words and waits for acknowledgement before saving.
A blank creation birthday uses the server tip minus 100 blocks. An explicit
height creates offline. Sync needs the configured server in either case.

Restore accepts valid 12, 15, 18, 21 or 24-word BIP39 phrases. Use a height or date
before the first deposit. Automatic restore birthdays are refused because they
could skip funds. Failed restores keep the input; successful ones clear it.
A saved wallet hides Create and Restore. Local removal requires backup confirmation
and refuses pending outgoing payments. It removes this browser's viewing data and
local vault, not on-chain funds. Lock removes spending access while retaining history.

The SDK has one client owner per page. Returning users see loading until storage
is read. On a `wallet_changed` error, the app reloads the committed state, including
another tab's removal. Separate tabs still need their own clients; the SDK's
storage generations prevent one tab from overwriting another's save.

## Sync and receiving

The last balance and activity remain visible during sync. `sync.activity`
distinguishes snapshot loading, active syncing and waiting for the server.
Cancel sync keeps committed progress. The SDK retries transient outages for
90 seconds by default; apps can set `lightServerGraceMs`. This does not replace
the transport's per-request timeout.

Available and confirming funds are separate. Incoming testnet payments require
three confirmations by default. Sync again as new blocks arrive. Activity shows
confirmation counts from the SDK policy. The receive QR encodes the public address
locally with MIT-licensed qrcode-generator; it makes no remote request.

## Payment review and receipts

Review accepts a shielded address, exact decimal amount, optional memo and estimated
fee. **Max** asks `wallet.maxSend(recipient)` for the spendable amount after the
estimated fee. It excludes confirming funds and still needs review; a fee or
balance change can invalidate it. Transparent-only recipients and ZIP-321 links
remain outside this example's reviewed payment path.

Before sending, the app syncs and displays the fresh state, then checks balance,
fee and the five-minute review deadline. It clears the phrase input before awaiting
unlock. Cancel stops the pre-send sync or prevents broadcast after proof cleanup.
An already-running proof may finish first. The phrase unlocks only this spend.

The SDK saves the pending transaction before submission. A lost acknowledgement
keeps its receipt and known transaction ID. Check that transaction before another
payment; the example never retries a new payment blindly. Synced receipts become
confirmed or expired. Pending history also blocks local removal after reload.

Compact scan history describes wallet movement. **Load memos and details** sends
activity transaction IDs to the light server, which can associate them with the
requester. Memos are decrypted locally. Exact fee metadata distinguishes a
fee-only self-send from an external debit. The app does not infer the original
self-send amount from change notes.

## Loading, hosting and identity

`onLoadProgress` shows engine download, verification and startup. `on("runtime")`
updates scanner readiness without polling. See
[engine loading](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#engine-loading)
for engine sizes, compression and single-thread configuration.

The generated app displays the SDK version and verified preview revision.
`sdk-build.json` and `SDK-ARCHIVES.json` record the archive set. Production preview
caches hashed JS/CSS/WASM immutably and negotiates WASM compression. Missing engine
or worker assets never fall back to HTML; configure the same policy on your host.

## Privacy

Recovery words stay local, and state/events do not contain them. Copy phrase is an
explicit action that puts a secret on the system clipboard. Startup and action
failures log fixed codes rather than raw provider payloads. Faucet requests share
the public receiving address and requester IP, never the phrase or viewing key.
The SDK's optional Base, passkey and backend integrations are not required here.
