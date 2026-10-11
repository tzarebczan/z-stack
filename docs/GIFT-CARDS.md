# Optional gift cards

`@z-stack/gift-cards` adds bearer gift cards without changing the ordinary wallet
or requiring an account service. Import its root for the pure v3 codec, amount
helpers, recovery store contract, and coordinator. Import `/browser` only when
opening isolated browser gift wallets. Native applications supply an adapter
around their own engine, durable payment receipts, and encrypted seed storage.

## Vizor link compatibility

The codec reads and writes Vizor's positional JSON `#v3=` format. Current Vizor
shares v3 by default. To accept those links, configure the exact endpoint
`https://link.vizor.cash/payment-links/open` alongside your application's share
endpoint. Mainnet and explicitly configured regtest links follow the Vizor
format; `test` is this package's testnet extension.

Messages are trimmed and limited to 128 grapheme clusters and 512 UTF-8 bytes,
using the runtime's `Intl.Segmenter`. Artwork identifiers are trimmed ASCII
identifiers of at most 64 characters. Labels are trimmed, preserve an explicit
empty label, and fit within the complete 16 KiB link bound. This package retains
the bounded v3 representation directly. Vizor additionally limits its expanded
v2 recovery representation to 16 KiB, so unusually large labels accepted here
may exceed that recovery bound when imported into Vizor.

Older Vizor v1 and v2 links are unsupported. Vizor still reads those versions and
may share v2 for a legacy phrase with alternate whitespace. This package does
not convert or reinterpret such legacy secrets. The codec needs `Intl.Segmenter`
when validating messages, including in standalone Node 22 consumers.

## Install and configure

The published alpha.9 preview predates gift cards. Use a source checkout that
contains this package, then build and pack the optional archive separately.
A future published preview must explicitly include the gift-card archive:

```sh
pnpm build:sdk
pnpm pack:sdk
pnpm pack:gift-cards
node scripts/create-example.mjs browser-wallet /path/to/gift-wallet --with-gift-cards --install
```

The scaffold installs the gift-card API dependency alongside the matching SDK. Add
the product controls using the application integration below. Ordinary templates
still install the SDK without gift cards. See the [package README](../packages/gift-cards/README.md)
for standalone native/pure installation.

Parse only configured origin/path pairs. Application configuration chooses the
light server, network, share endpoint, and storage namespace. A link never chooses
a server or triggers an automatic payment. Production share endpoints require
HTTPS; loopback HTTP is available for local test fixtures.
The coordinator accepts its share endpoint and explicitly supplied
`acceptedEndpoints` only. Vizor's endpoint is never added implicitly.

```ts
import { parseGiftLink, giftFundingAmount } from "@z-stack/gift-cards";

declare const incomingLink: string;
const card = parseGiftLink(incomingLink, {
  network: "testnet",
  endpoints: ["https://wallet.example/gift"],
});
const fundingZat = giftFundingAmount(card);
```

Mainnet and regtest v3 links use Vizor's positional JSON protocol. Testnet is a
network extension. Legacy v1/v2 links are unsupported. The URL fragment contains
original BIP-39 entropy, not the derived spending seed. Rust reconstructs the
English phrase, derives account zero with an empty passphrase, and selects a
shielded Unified Address. The face amount and presentation do not affect keys.

```ts
import { createBrowserGiftBackend } from "@z-stack/gift-cards/browser";

const backend = createBrowserGiftBackend({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev",
  namespace: "my-wallet-gifts",
});
```

Use the SDK's existing [hosting setup](INTEGRATION.md) for production WASM assets
and workers. Each active gift wallet has a separate worker realm and durable,
gift-scoped storage. Main wallet restore, lock, and recovery remain independent.
Close sessions and dispose the backend when the application locks or logs out.
For regtest, configure both upgrade heights to match the owned fixture.

The browser worker enables the SDK's opt-in `verifyChainTip` check. After a block
has been scanned, each sync adds one public compact block range request to compare
the saved overlap hash with the configured server, including unchanged or shorter
tips. A reorg rewinds the scan before inspection can use mined history. Failed
verification leaves settlement unresolved. It does not query a gift address or
replace validator consensus verification. See [the SDK contract](SDK.md#verify-the-saved-chain-tip).

## Application integration

The application owns authorization and encrypted recovery storage. Supply an
unlocked vault or encryption capability without giving the gift coordinator the
main wallet's phrase. Initial browser funding requires a software source wallet;
the SDK's fee-capped send currently rejects hardware sources. A gift can still be
redeemed into a hardware or viewing-only destination wallet because the gift
account signs the payment. The initial browser adapter requires a same-network
Unified Address with an Orchard receiver for redemption. Native adapters can
apply their own supported shielded receiver policy. Native sources must preserve
their own immutable review and actual fee limit when adapting the payment receipt API.

Save the gift secret durably before reviewing or funding it. Review displays the
face value, claim reserve, actual funding fee, destination, and total debit. Fresh
authorization applies to the explicit confirmation action. Store the payment
attempt before signing and preserve its identity on submission errors. A node
acknowledgement means submitted, not mined.

Before redemption, inspect and sync the isolated gift wallet. Validate a plain
shielded destination on the same network, require six scanned funding
confirmations, and use the engine's actual fee for the exact face amount. If the
reserve is inadequate, keep the gift recoverable and show that it needs more
confirmed funds. Do not reduce the reviewed amount silently or sweep unrelated
top-ups.

Maturity applies to the currently unspent value, including the actual claim fee.
The browser adapter queries the engine's current unspent balance with
`minConfirmations: 6` and uses the same policy for fee estimation and the actual
claim proposal. Its snapshot exposes `matureAvailableZat`; the coordinator caps
that value by the current shielded balance. A spent historical deposit cannot
qualify a younger refill. Change and self-send outputs need their own six
confirmations.

Other adapters may provide the same authoritative snapshot field only if their
engine enforces this policy on the claim. Otherwise the coordinator subtracts
all active input debits from mature shielded credits and caps that lower bound
by the current available balance. Such adapters must return complete history or
the latest 500 unfiltered rows. A full page without authoritative maturity fails
closed; adapters without per-pool change evidence may defer redemption
conservatively. Every custom claim backend must also enforce
`GiftSendOptions.minConfirmations` on its actual selected inputs, or reject before
signing. The coordinator passes six for redemption and omits this override for
source funding. A mature lower bound alone cannot constrain a backend's greedy
input selection. Exact `transaction(txid)` lookup keeps older mined claims from
being mistaken for a reorg when they leave the recent page. Without that lookup,
a truncated page cannot downgrade a saved redeemed claim or authorize retry of
a missing rejected funding or claim attempt.

Status comes from positive chain evidence and the saved local attempt. A zero
balance or missing history does not prove another holder redeemed the gift.
On reload or lost replies, inspect the existing wallet and receipt. Any replay
uses retained signed bytes; do not create another funding payment or claim while
its outcome is unknown. New SDK snapshots retain locally signed bytes for mined
claims within the 2,048-block retention horizon. Sync only rebroadcasts them after
a reorg makes the claim unmined and while it remains unexpired. Legacy snapshots,
pruned receipts, and expired claims may lack replayable bytes; retain the claim ID
and keep the outcome unresolved rather than constructing another claim.
The initial release retains gift wallet state and secrets instead of automatically
deleting them after a mined receipt.

A definitive SDK `broadcast_rejected` verdict permits retry only when its txid
matches the current committed receipt. The record enters `funding-rejected` or
`redemption-rejected` and keeps that attempt in `rejectedAttempts`, including its
fee and, for a claim, destination. Funding retries require a fresh fee review and
authorization; redemption retries repeat sync, maturity, destination and actual
fee checks. Positive mined evidence overrides a prior rejection and blocks a
replacement payment. Unknown errors, missing or mismatched txids, and failed
rejection-record writes remain unresolved. Up to 256 rejected attempts are
retained; a full journal stops before constructing another attempt.

The `beforeSubmit` callback checks cancellation before its durable journal write.
After the write commits, it resolves without a second cancellation check. A
committed receipt must retain its signed reservation even if the review or session
closes before submission; cancellation can prevent forwarding bytes without
discarding their recovery evidence.

Native integrations keep renderer commands typed. Trusted runtime code chooses
gift roots, transport endpoints, and account fingerprints. Serialize source and
gift operations through their owning mutation gates, retain the existing mainnet
spend gate, and apply the same network/endpoint consent to gift sync. Reuse native
[prepare/sign/submit/receipt APIs](NATIVE-PAYMENTS.md) instead of a second signing
or broadcast path.

## Secret handling and verification

A share link is a spending credential. Its creator may retain recovery access,
and any holder may spend it. Sharing does not revoke that access. Copy/export/QR
actions require an explicit application action. Remove an incoming fragment from
browser history before analytics initialize; keep pending intake in memory until
the vault unlocks. Parser and worker errors must not include the secret input.

`pnpm test:gift-cards` checks protocol, amount, recovery, and coordination behavior.
`pnpm test:gift-cards:package` installs the real archive in fresh pure and browser
consumers, verifies server-safe root import without the SDK, and builds the emitted
worker path with the matching SDK. `pnpm test:gift-cards:browser` runs the packaged
worker with real production WASM in Chromium, Firefox, and WebKit. These checks
do not replace funded claim/reclaim, reorg, quota, process-loss, or mainnet
qualification. See [release verification](RELEASE.md).

### Browser confirmation policy

The optional SDK policy accepts an integer from 1 through 4,294,967,295:

```ts
import type { Wallet } from "@z-stack/sdk";

declare const wallet: Wallet;
declare const recipient: string;

const mature = await wallet.balance({ minConfirmations: 6 });
const fee = await wallet.estimateFee(recipient, "0.01", undefined, { minConfirmations: 6 });
await wallet.send(recipient, "0.01", undefined, { minConfirmations: 6, maxFeeZat: String(fee.feeZat) });
```

Omitting the policy preserves the engine default. The policy applies to current
unspent notes, fee selection and actual software send inputs; it does not alter
the stored wallet's default confirmation settings. Browser hardware and native
HTTP adapters reject an explicit policy until they support its enforcement.
Older WASM without the confirmation-aware binding also rejects it.
