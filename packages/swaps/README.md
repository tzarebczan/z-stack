# Durable two-leg CYPH trades

`@z-stack/swaps` coordinates an explicit recoverable sequential route:
external asset → NEAR Intents → owned Solana USDC/SOL → Jupiter → CYPH.
It is optional and does not change the Zcash wallet or own any keys.

`createTwoLegController` takes an owner namespace, atomic `TwoLegRouteStore` and
provider adapters. The owner binds profile, existing wallet fingerprint and
Solana address. All amounts are exact atomic decimal strings.

1. `create({ id, near })` saves the reviewed deposit instructions.
2. A local funder calls `claimFunding` before signing/sending, then `recordFunding`.
   External deposits may instead be made from their original wallet.
3. `resume` refreshes provider status. A provider success moves only to
   `awaiting-settlement-proof`.
4. `acceptIntermediate` calls the app's `verifyIntermediate` adapter. The adapter
   must bind the authenticated provider payout signature to a finalized actual
   recipient amount; `verifySolanaReceive` is the Solana verification helper.
5. `saveJupiterReview` binds the fresh prepared quote to that settled asset/amount
   and the same owner. The user approves and signs the second leg separately.
6. `executeJupiter` durably saves `jupiter-submission-unknown` and exact signed bytes
   before invoking the submitter. A timeout retains the reservation.
7. `resume` only reconciles. It never signs, re-quotes or submits. An explicit
   `retryJupiter` submits the same saved bytes. A finalized matching transaction
   completes the route; a finalized on-chain failure retains intermediate funds
   in `intermediate-recoverable`. Missing/expired status stays unresolved. A separate `not-sent` observation may
   release only a provider-identified quote that the authenticated backend has
   durably and irrevocably canceled before sponsor signing; it must match the
   saved provider quote ID and reviewed message.

Completed, failed and non-inclusion observations must include the fully signed transaction.
A `not-included` result additionally requires the complete bounded historical
block-signature coverage returned by `proveSolanaNotIncluded`, ending beyond the
saved conservative RPC expiry bound. Reloading days later inspects the original
validity window. Unsupported, changed or pruned history keeps the route unresolved.
The controller checks its owner/payer signatures, exact message and canonical
signature against the saved review before accepting the outcome.

`createWebStorageTwoLegRouteStore` uses supplied Web Locks and storage; use a
wallet-profile-scoped storage adapter. Store failures stop spending. Late
acknowledgements update the captured original owner namespace. Applications must
not delete unresolved records or clear reservations merely because a screen,
request or login timed out. Keep source-wallet and Solana-wide spending locks
around unrelated spending paths as well.

Portable Zod schemas are exported for route, owner, NEAR leg and settlement
records. Persisted records contain public transaction/quote data and signed
transaction authorization bytes, never phrases, private keys or login tokens.
The route is not atomic: if Jupiter is unavailable, funds remain as the shown
intermediate Solana asset. The operator and public networks can observe addresses,
amounts, timing, fees and Solana token activity.

Pending-deposit polling preserves an unfunded route's funding action. The
controller allows at most `MAX_JUPITER_ATTEMPTS` (20) second-leg attempts. Call
`assertJupiterAttemptCapacity(route)` before fresh unlock when resuming a saved
review; review, execute and submit/retry boundaries also enforce the limit.
Intermediate funds remain in the wallet when the limit is reached. Finalized
history is never truncated, and one legacy in-flight overflow can still record
its final receipt.

## Aurora Intents Connect catalog

`getAuroraRingAssetSupport` checks the live bridge-in catalog for native Zcash
and canonical USDC on Solana. Wrapped ZEC on NEAR or Solana cannot meet the
native source requirement. When both assets are listed, qualify a quote that
binds the Ring deposit instruction and the expected recipient, token, minimum
and refund before committing a payment. Use `verifySolanaRingDeposit` from
`@z-stack/solana` to check its finalized deposit receipt.
[Aurora's Ring demo](https://github.com/aurora-is-near/intents-rings-demo)
