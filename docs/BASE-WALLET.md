# Optional Base wallet

`@z-stack/base` is separate from `@z-stack/sdk`. It installs viem, not the Zcash
engine, passkeys, Cloudflare services or banking providers. Import it only when
your app enables its Base wallet. There is no registration, background polling,
cloud backup, telemetry or automatic delegation.

## 1. Build or install the optional archive

```sh
pnpm install --frozen-lockfile
pnpm --filter @z-stack/base build
pnpm pack:base
node scripts/create-example.mjs base-wallet /path/to/base-demo --install
```

Or install `artifacts/z-stack-base-0.1.0-alpha.9.tgz` in an existing application.
No npm publication is required. The [Base-only demo](../examples/base-wallet/README.md)
uses Base Sepolia. Zcash examples continue installing only SDK/core/passkey.

## 2. Choose the network and derive an address locally

```ts
import { BASE_SEPOLIA, createBaseWallet, deriveBaseAddress } from "@z-stack/base";

// Supply the phrase only while your app's existing unlock is active.
const address = deriveBaseAddress(unlockedPhrase);
const wallet = createBaseWallet({
  address,
  network: BASE_SEPOLIA,
  rpcUrl: "https://your-public-rpc.example",
});
```

Mainnet uses `BASE_MAINNET`. Both presets specify the chain and [Circle USDC](https://developers.circle.com/stablecoins/usdc-contract-addresses)
contract; the RPC is application-owned. Construction and address derivation
make no network requests. A wallet holds its address and configuration, never
a phrase, key, passkey or signer. Do not put privileged provider credentials in
browser configuration. Supply a server endpoint where credentials are needed.

The account path is `m/44'/60'/0'/0/0`. Your existing Zcash recovery phrase can
produce this Base account, so users need no separate recovery secret. Reusing
one phrase also means compromise affects both wallets. EVM addresses, balances
and transfers are public; swaps can connect a shielded-wallet interaction with
a public Base transaction. Do not send the Zcash phrase or viewing keys to an
RPC, paymaster, bundler or account backend.

## 3. Bind each operation to the selected wallet

```ts
const guard = {
  signal: paymentController.signal,
  assertCurrent() {
    if (selectedWallet !== selectedAtReview || !baseOptIn || sessionLocked)
      throw new Error("Review the payment again.");
  },
};
const balance = await wallet.getBalance(guard);
const review = await wallet.reviewPayment(
  {
    asset: "usdc",
    recipient,
    amount: 1_000_000n, // atomic units: USDC 6, ETH 18
    deadline: Date.now() + 300_000,
  },
  guard,
);
```

Show recipient, asset, amount, network, estimated total fee and signed L2 fee limit before
accepting. The immutable review belongs to this wallet instance. Copies and
reviews from another instance are rejected. The native flow includes OP Stack
L1/L2/operator fees; fee-read failures do not become zero fees. `review.fee` includes an estimate of variable L1/operator charges, which may change before inclusion. Only `review.l2Max` is bound by the signed gas fields. Do not present the combined estimate as “up to” or a maximum total charge.

## 4. Unlock, reserve and send once

```ts
import {
  deriveBaseAccount,
  BaseSubmissionUnknownError,
  BaseSubmissionNotSentError,
  baseErrorMessage,
} from "@z-stack/base";

try {
  const hash = await wallet.sendPayment(review, {
    guard,
    withSpendLock: (run) => sharedAddressLock.run(run),
    withSigner: (run) => appUnlock.withPhrase((phrase) => run(deriveBaseAccount(phrase))),
    assertNoPending: () => pendingPayments.assertNone(wallet.address),
    reserveSubmission: (submission) => pendingPayments.persist(submission),
    releaseSubmission: (submission) => pendingPayments.removeMatching(submission.hash),
  });
  showSubmitted(hash);
} catch (error) {
  if (error instanceof BaseSubmissionUnknownError) showCheckStatus(error.submission.hash);
  else showPaymentError(baseErrorMessage(error));
}
```

The adapter names above represent your application services, not services the
SDK installs. `withSigner` must request a fresh unlock, validate the selected
wallet, run the callback, and release references afterward. JavaScript cannot
promise that garbage-collected strings or viem signer objects are securely
zeroized; avoid retaining them and protect the application's origin against XSS.

`withSpendLock` must serialize every native and sponsored payment for this
chain/address, including other tabs. `assertNoPending` must check persistent
native and sponsored reservations inside that lock. `reserveSubmission` must
finish a durable write before returning, and throw if persistence fails. These storage hooks must not broadcast or disclose the signed bytes; the no-submission guarantee covers only the SDK transport. The
saved record contains the hash, sender, chain, nonce and serialized signed transaction, never a seed. **Signed bytes authorize spending: keep them private until broadcast.** Do not log them, upload them to analytics, or disclose them from a backup API. The SDK checks the signer and
serialized transaction against the review, refreshes fees before signing, then
submits once with application retries disabled. Browsers or proxies can retransmit
the same signed bytes; the unchanged nonce/hash still identifies the same payment. A signed review is consumed even if a later
step fails.

Do not sign a replacement for an unknown submission, silently clear a reservation, or treat a UI
cancellation as proof that no transaction was sent. `BaseSubmissionNotSentError` means the SDK never invoked the broadcast transport and called `releaseSubmission` under the same lock. That callback must remove only the matching hash, read back the durable result, and reject on failure. A release failure leaves the application fail-closed. Check the saved hash against
the selected chain's canonical finalized receipt, then update the record inside
the same lock. A missing receipt stays unresolved. `getBaseTransactionStatus` validates the saved signed record and checks canonical finalized transaction execution; a successful ERC-20 transaction alone does not prove a token transfer. `getBasePaymentStatus` additionally verifies the exact USDC Transfer event against the saved signed calldata (or a plain ETH transfer). Use it before marking a native payment complete, as the demos do. A missing or mismatched token event retains the reservation. `included` means mined but not finalized: wait and check again; do not offer another broadcast. Only `pending` (mempool) and `unknown` can offer explicit same-byte retry. `nonce-consumed` means a finalized transaction consumed the nonce, not that this payment failed. Keep that outcome uncertain and direct the user to their chain activity. After the user explicitly acknowledges that review, an application can archive the record as **unverified** under its spend lock. Retain any payment reference to block automatic repayment; do not mark it paid or failed. A nonce that could still execute must never take this path.

After explicit user confirmation, `rebroadcastBaseTransaction` can send the **identical signed bytes** once, without another signature, nonce or change to signed gas limits. Base’s variable L1/operator fees can change; show that distinction during retry approval. Hold the same spend lock, match the durable pending record, check the current wallet/opt-in, and reconcile finality before retrying. This retry cannot produce a second transaction. Never automatically retry, bump fees, or reinterpret an HTTP error as rejection. Cross-origin and cross-device
coordination need an application-owned durable service; Web Locks and local
storage cover one origin only. The demo shows that narrower browser contract.

`createBaseTransfers({ ...BASE_SEPOLIA, rpcUrl })` requires an explicit RPC and provides the lower-level preparation and one-use USDC
submission primitives. These primitives do not provide an
application lock or pending journal; callers must supply those safeguards. Its mandatory fourth `submitUsdc(account, prepared, stillCurrent, beforeBroadcast)` argument must persist the signed record before the transport is invoked. A failed save or final guard throws `BaseTransactionNotSentError`; the caller owns matching-record cleanup under its lock. A lost, malformed or mismatched RPC acknowledgement throws `BaseTransferSubmissionUnknownError` with the locally computed `hash` and `chainId`. Reconcile that hash instead of retrying the consumed review. The
low-level `signUsdcAuthorization` signs EIP-3009 only; it neither picks a relayer
nor submits a transfer. Save the authorization and maintain pending exclusion
before disclosing it to a provider. The high-level wallet exposes the journaled
transaction flow; sponsored flows use the separate smart-account journal contract. Applications must supply authentication, consent and durable pending-operation guards.

Public native-wallet errors use stable `BaseError.code` values and fixed copy from `baseErrorMessage`. Diagnostic `cause` is non-enumerable and may contain provider credentials or request details; do not render, serialize explicitly, or log it in production. Unrecognized errors receive generic public copy. App lock/pending failures use `payment_blocked`, fresh-unlock failures use `unlock_failed`, and signing failures use `signing_failed`; they are not described as RPC outages. Known application causes can be handled privately, but never display arbitrary cause messages. Unknown-submission errors expose the hash while keeping the full signed record non-enumerable.

## 5. Add your own paymaster explicitly

```ts
import { createBaseSmartAccount, runBaseSponsoredTransfer } from "@z-stack/base/smart-account";

const smart = createBaseSmartAccount(verifiedDeployment);
const policy = smart.freezeSponsorPolicy({
  paymaster: yourPaymasterAddress,
  maxFeePerGas: 1_000_000_000n,
  maxSponsoredCost: 1_000_000_000_000_000n,
});
```

Configure your own paymaster and bundler. The
supported implementation is viem's Simple7702 account with EntryPoint 0.8 on
Base or Base Sepolia. Supply a reviewed delegate and the canonical EntryPoint 0.8 address, their
runtime code hashes and USDC address through `verifiedDeployment`. A deployment
is a trusted wallet configuration, never something a dapp, quote or paymaster
response can choose. Arbitrary smart-account implementations are not supported.

Run `runBaseSponsoredTransfer` inside the same address lock and fresh signer
callback, after checking durable pending operations. Supply the verified RPC
client, `smart.readSmartAccountReadiness` guard, intent, frozen policy, two approval
callbacks, durable journal and provider adapter. The first approval acknowledges
**persistent delegation**; the second approves the final quote's exact operation
hash. The flow saves the authorization before giving it to the provider and saves
the signed operation before submission. The journal must retain unresolved
signatures: an EIP-7702 authorization can be used even if no payment was submitted.

The final guard runs before the durable `submission-unknown` marker. Committing
that marker starts dispatch; the SDK enters the bundler without another guard
or readiness request. Cancellation before dispatch leaves the signed operation
unsent. Once dispatch starts, reconcile its hash even if the page closes or the
local deadline passes. Low-level `submitJournaledOperation` accepts a durable
`beforeSubmit` preparation callback: if preparation rejects before transport,
the same signed handle can be attempted again. It must never broadcast. After
transport is entered, that handle stays consumed even on a lost acknowledgement.

`provider.quote` returns one final unsigned `UserOperation<'0.8'>`. Adapt your
provider's gas-estimation/paymaster methods here, preserving sender, nonce,
calldata and signed authorization. Use `createBaseBundlerTransport({ url: yourBundlerEndpoint })` for a bounded,
credential-free JSON-RPC transport, or supply your own.
`provider.bundler.request` submits the raw
`eth_sendUserOperation` once with transport retries disabled. Do not call a
high-level sender that reprepares or requotes after signing. Never expose
provider secrets through either adapter. Providers may require an app-owned
server policy/authentication layer; that service is outside this package.

The SDK rejects changed recipients, token approvals, batches, ETH charges,
unauthorized paymasters, invalid gas fields, fee-limit increases, alternate nonce
lanes and changed authorization tuples. Provider receipts are hints: use
`verifySponsoredSettlement` to independently inspect canonical finalized RPC
logs for the exact UserOperation and its single USDC transfer.

**Experimental:** the current Simple7702 call has no on-chain payment expiry.
The review deadline limits local signing/submission, not delayed execution.
Delegation persists even if the transfer fails. Explain both before approval.
Code hashes prove identity, not an audit. Complete deployment, provider,
credential tracing and funded sponsorship verification before enabling this in
production.

## Boundaries and validation

| Component                                                                     | Owner                                          |
| ----------------------------------------------------------------------------- | ---------------------------------------------- |
| Derivation, balances, fee preparation, local signing and smart-account checks | `@z-stack/base`                                |
| Zcash scanning, proving and spending                                          | `@z-stack/sdk` / Rust engine                   |
| Unlock, passkeys, backups and account registration                            | Application, optionally other z-stack packages |
| RPC/bundler/paymaster selection, keys, sponsorship policy and quotas          | Application                                    |
| KYC, card UX, fiat accounts and swap providers | Application |
| Pending reservations, receipt reconciliation and cross-device exclusion       | Application using SDK primitives               |

Tests use real viem signatures and controlled RPC/provider responses, including
changed accounts, cancellation, failed durable writes and lost acknowledgements.
Fresh archive consumers build without Zcash packages or WASM. Live funded Base
Sepolia payments, physical device unlocks and a funded production paymaster have
not been verified by those tests. The SDK does not promise an audit or provider
availability.

Primary protocol references: [viem account abstraction](https://viem.sh/account-abstraction),
[ERC-4337](https://eips.ethereum.org/EIPS/eip-4337), and
[EIP-7702](https://eips.ethereum.org/EIPS/eip-7702).

## Run the combined examples

The Vite and Next.js examples support `--with-base`; [example commands](../examples/README.md#add-base-to-an-existing-wallet-example) install the fourth archive explicitly. Their Base panel verifies each unlock against the selected Zcash wallet, derives the standard EVM account from the same phrase, and clears the input before asynchronous work. App controllers serialize Zcash and Base actions; page teardown invalidates pending Base signing. Next.js remounts a fresh, disconnected Base panel on restoration from the back/forward cache; it never restores an unlocked signer. The journal contains signed transaction records for exact retry, never phrases. Those signed bytes are spending authority and stay in application-owned local storage; protect the origin against XSS and never export them to analytics. It survives disconnect/reload, and a finalized canonical receipt must be checked before it is cleared under the same spend lock.

Fresh archive production builds run with `pnpm test:combined:package`. The funded browser test uses the existing local Zcash regtest fixture and an isolated Anvil EVM. On Linux x64, install the optional test tools into a disposable directory:

```sh
npm install --prefix /path/to/fixture-tools --ignore-scripts --no-audit --no-fund @foundry-rs/anvil-linux-amd64@1.7.1 solc@0.8.30
Z_STACK_BASE_FIXTURE_TOOLS=/path/to/fixture-tools Z_STACK_REGTEST_NU6_3=150 pnpm test:combined:browser
# Use --browser=firefox or --browser=webkit with scripts/test-funded-demos.mjs.
# --base-only checks the shared-wallet Base flow without proving new Zcash payments.
```

The fixture installs a minimal transfer token at the configured USDC address and a fixed-fee oracle. Actual EVM execution verifies balances, signed transactions and transfer logs; it does not validate Circle's full contract, realistic OP Stack fee behavior, a live bundler or a funded paymaster. All browser RPC traffic stays on loopback. Fixture tools are development-only; their binaries and contracts are not packaged. The Anvil launcher is currently tested on Linux x64. The harness can retain screenshots and test output; disposable consumer dependencies are removed unless `--keep-consumers` is selected.
