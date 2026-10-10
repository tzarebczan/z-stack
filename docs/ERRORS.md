# Errors, retries and lifecycle

Use `WalletError.code` for control flow. Provider message text and `cause` may
contain sensitive data; do not send them to analytics or render them as HTML.
Use fixed application copy or `walletErrorMessage(code)` without a raw fallback.
`WalletError.userMessage()` also returns fixed copy for unknown errors; the raw
message and cause remain available for private debugging.

Before signing, sync and render the returned snapshot, then revalidate the
approved amount, fee and review deadline. A payment that fails validation or
unlocking must still show the confirmations and history found by that sync.
Tie payment cancellation to `wallet.cancelSync()` during this check.

A `WalletError` subclass thrown by `beforeBroadcast` survives spending rollback
and SDK normalization. The examples use an app-owned `ReviewOutdatedError` for
fixed expired-review copy; the SDK does not manage application review deadlines.

| Code | Next action |
| --- | --- |
| `cancelled`, `hardware_cancelled` | Keep the last committed state; let the user retry |
| `sync_required` | Complete the first scan before enabling payment review. A send also needs the wallet within 10 blocks of the tip; sync before proving |
| `seed_locked` | Ask for local reauthentication from a user action |
| `invalid_birthday` | Correct the height/date before clearing secret inputs |
| `rescan_later_birthday` | Use a birthday at or before the current one |
| `rescan_pending` | Sync to confirm or expire the outgoing payment before rescanning |
| `unsupported_payment_uri` | Paste the recipient address; payment links need a separate review UI |
| `busy` | Await the current operation or the previous client's close |
| `closed` | Stop using this handle; await teardown before creating a new one |
| `already_exists` | Load the saved wallet; require deliberate backup/replacement |
| `wallet_changed` | Refresh state and review again; another tab committed first. Its fixed copy names another tab, so use your own error for an expired payment review |
| `transport` | Retain activity, show a connection indicator, retry sync explicitly |
| `storage_full` | Keep site data and recovery backups; free space, then retry saving |
| `broadcast_rejected` | The node explicitly rejected this transaction; review before retrying |
| `broadcast_failed` | Submission may have succeeded; reconcile its receipt before another payment |

```ts
import { WalletError, walletErrorMessage } from "@z-stack/sdk";
try {
  const result = await wallet.send(destination, amount, memo);
  showReceipt(result.txid);
} catch (error) {
  if (error instanceof WalletError && error.code === "broadcast_failed") {
    showUncertainSubmission(error.txid);
    // Query this receipt. Do not auto-send a replacement payment.
  } else if (error instanceof WalletError) {
    showError(walletErrorMessage(error.code));
  } else {
    showError("Action incomplete. Try again after checking the wallet.");
  }
}
```

The functions and wallet in this snippet belong to your app. A timeout,
cancellation, server disconnect, closed client or absent history entry cannot
establish rejection after submission started. `broadcast_failed` retains its
transaction ID even if the original wallet was forgotten or replaced. If no
receipt is available, keep the outcome unresolved; do not infer success/failure.

## Own teardown

Only one browser client owns the page engine. Shared initialization is not a
multi-wallet API. Await `wallet.close()` before another `createWallet`, including
React StrictMode remounts. See the [React example](../examples/react-wallet/README.md).
Closing retains saved state, locks the seed and clears subscriptions.

If saving a pre-broadcast rollback fails, `close()` still clears the session,
retires its worker and releases ownership, then rejects with the storage error.
Reopen and inspect saved state before attempting another spend; a failed commit
does not prove that a durable reservation was removed. Calling `close()` again
returns the same completion and cannot clear a newly opened wallet.

Closing while an external Ledger/Keystone exchange stalls stops waiting for it
and rolls back pre-broadcast reservations before releasing ownership. Late device
results are ignored. The integrating app still owns its device connection and
prompt cleanup; aborting a JavaScript wait cannot dismiss every hardware UI.
A proof already in progress may need to drain before safe reservation cleanup.
Never free a session underneath an outstanding wallet mutation.

Keep balance and history visible during sync. New progress is not empty activity.
Memo retrieval is separate from compact scanning. `cancelSync()` preserves
committed progress. `lock()` removes spending access and keeps viewing data;
it does not encrypt history. Logout from an optional provider, local forget,
remote tombstone and account deletion are separate application actions.

If local storage is unavailable, standalone or owned-wallet forget rejects with
`wallet_db`; it cannot confirm deletion. Keep the recovery backup and retry once
the configured store is accessible. Do not treat locking or an empty UI as proof
that saved wallet/seed/passkey records were deleted.

Malformed recovery words use `invalid_recovery_phrase`; a valid phrase for a
different saved wallet uses `seed_mismatch`. Neither display string includes the
words or the engine's parsing detail. `unknown` remains generic. Shared display
copy is browser-safe; native applications can add their own setup instructions
based on the code rather than parsing `message`.

`wallet.forget({ pending: "reject" })` rejects with `forget_pending` while outgoing
reservations are unresolved. It checks the latest durable wallet under the spend
lock; a competing save before deletion returns `wallet_changed` and leaves all
stored records intact. Reload and inspect the wallet before trying again.
