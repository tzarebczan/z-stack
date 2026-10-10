# Experimental native payment receipts and public-block scanning

These Rust APIs belong to `z_engine::native::NativeWallet` behind the `native`
feature. They are not exposed by the TypeScript native bridge or browser SDK.
Existing `send` and `estimate_fee` keep their previous behavior. Consumers must
serialize wallet operations and hold an exclusive process lock on the wallet.

## Review, sign, submit, recover

`prepare_payment(to, amount_zat, memo)` selects a single shielded payment and
persists its serialized proposal in `ext_native_payments_v1` in the wallet DB.
It returns a `PaymentReceipt` with a random wallet-bound ID, fee and expiry
(Unix seconds). It accepts a plain shielded address, not ZIP-321 multipay.
No network request or signing occurs during preparation.

Display the requested recipient, amount and memo with the returned fee. On
approval, call `sign_payment(auth, id)` with fresh authorization. Signing decodes
the saved proposal; it does not select a replacement set of inputs. A review
expires after 120 seconds, on a changed scanned tip/hash, or when its original
inputs can no longer be used. Expiry during proving rolls the operation back.

The signed transaction, spending reservations and receipt commit in one SQLite
transaction using `synchronous=FULL`. `sign_payment` never broadcasts. A second
signing call for that ID is refused. `submit_payment(id)` reads and sends the
saved transaction bytes; a retry cannot construct another payment.

Persist the ID in the application's send journal **before** signing. If a
process or response is lost, use the offline `payment_receipt(id)`:

| Phase | Meaning and recovery |
| --- | --- |
| `prepared` | No signed transaction committed for this ID. Clear the app's unsigned intent; a new send still needs a new review. Never sign automatically during recovery. |
| `signed` | Signed bytes are durable; submission has not started through this API. Offer explicit submission of those bytes. |
| `unknown` | Submission started, but no positive acknowledgement was saved. Keep the app intent unresolved; offer explicit replay of the same bytes. |
| `accepted` | The server acknowledged the transaction (including a recognized duplicate). This is not mining or final settlement. |
| `mined` | Local wallet history observes the saved transaction in a block. |

Normal native sync can also rebroadcast saved unmined transactions. It cannot
create a new payment, and a `signed` receipt does not prove no other path has
broadcast those bytes. An accepted transaction may later expire or be removed
from a mempool. Continue showing its actual wallet-history status.

Missing/corrupt receipts or saved transaction bytes are storage errors, not an
unknown delivery that can be retried normally. They are never permission to clear an uncertain
intent. Seed restoration does not restore local proposal IDs. Whole-DB rescans
are refused once a signed receipt exists until a migration can preserve signed
bytes and receipts together. Normal sync remains available. Unsigned reviews
older than a day are pruned; total stored receipts are currently capped at
10,000. Proposal data is stored with the same at-rest privacy limits as the
native wallet database.

Qualification must include funded chains, reorgs, disk exhaustion, process loss,
concurrent callers, clock changes and hardware platforms. These experimental
APIs alone do not qualify a consumer for mainnet spending.

## Offline shared regtest blocks

`scan_public_regtest(bytes)` accepts protobuf **varint-delimited** compact blocks
from height 1 through at most 320, with a 128 MiB input limit. It requires a
regtest wallet with birthday 1. This framing differs from the browser engine's
four-byte block framing.

The caller must authenticate the complete publication, digests, network,
freshness and anti-rollback state before calling. The engine checks framing,
heights, parent links and compatibility with already-scanned hashes, then uses
the standard native shielded scanner in one wallet DB transaction. Replaying the
same publication is supported. It performs no network, transparent-address,
transaction-ID, memo-enhancement or rebroadcast requests.

This bounded full-prefix scan is for integration research. It is not consensus
verification, transparent-history coverage, an incremental production scanner,
PIR, OHTTP, or an anonymity guarantee. Reorgs and rollback are refused; they do
not reset saved state automatically. When a consumer checkpoints its publication
after scanning, a failed checkpoint write must retry the same publication, not
assume that the wallet transaction was also rolled back.
