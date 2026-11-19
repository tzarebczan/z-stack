# Optional Solana wallet

`@z-stack/solana` derives the Solana account from the same existing BIP-39 phrase
used by the Zcash wallet. It uses hardened SLIP-0010 path `m/44'/501'/0'/0'`
and an empty BIP-39 passphrase, matching Coffer's existing recovery policy.
It never creates a second phrase, registers an account or selects a remote RPC.

The app owns fresh unlock, wallet/profile identity, public-RPC consent, RPC and
Jupiter credentials, sponsorship, durable spending reservations and UI review.
Call `withSolanaSigner` only after fresh authentication; do not retain its signer.
The callback's owned key bytes are cleared when it settles. JavaScript cannot
guarantee complete memory erasure. Hardware and viewing-only wallets need a
separate supported signer integration.

```ts
import {
  deriveSolanaAddress, withSolanaSigner, createSolanaRpcClient,
  createSolanaWallet, signPreparedSolanaSwap,
} from '@z-stack/solana';

const address = deriveSolanaAddress(existingPhrase);
const rpc = createSolanaRpcClient(explicitlySelectedReadRpc);
const wallet = createSolanaWallet({ address, rpc });
const balances = await wallet.getBalances(sessionGuard);
// Obtain prepared from a saved, reviewed Jupiter V2 build. The signing method
// repeats instruction, lookup-table, mint and simulated-effect checks locally.
const signed = await signPreparedSolanaSwap(prepared, {
  rpc,
  guard: sessionGuard,
  withSigner: run => withSolanaSigner(freshlyUnlockedPhrase, run),
});
// Persist a spending reservation before handing signed bytes to a submitter.
```

`SOLANA_USDC` and `SOLANA_CYPH` pin mint, token program and decimals. CYPH uses
Token2022 and has issuer-controlled extensions, including a permanent delegate.
This package does not make token holdings private. Balances expose exact atomic
amounts and current scaled-UI metadata; the swap validator refuses active hooks,
paused mints and non-unit scaled amounts.

`prepareJupiterSwap({ build, intent, rpc })` accepts raw `/swap/v2/build`
instructions. It supports current exact-in `route_v2` and
`shared_accounts_route_v2`, decoded against Jupiter's on-chain Anchor IDL at
`C88XWfp26heEmDkmfSzeXP7Fd7GQJ2j9dDTUsyiZbUTa` (read November 14, 2026).
It checks owner, both ATAs, mints/programs, encoded amounts/slippage/platform fee,
zero positive-slippage fee, safe setup/cleanup instructions, required signers,
fresh resolved lookup tables, current mint properties, sponsor/priority ceilings
and simulated changes to every writable account. Existing token authorities and
extensions must remain unchanged. Unrelated owner and sponsor token debits fail.
Unsupported instructions fail explicitly.

Sponsored swaps support token-to-token routes. Native SOL input/output is refused
when payer and owner differ: a temporary wrapped-SOL close must not turn sponsored
rent into an owner payout. The sponsor is forbidden from every Jupiter swap account,
including read-only remaining accounts. Sponsored token swaps must leave the owner's
native SOL balance unchanged.

Every fresh review includes `sponsorCostCeilingLamports`: the exact compiled-message
fee plus the selected RPC's rent-exempt minimum for every distinct possible sponsored
ATA creation. This charges for idempotent creates even when those ATAs currently exist,
and includes Token2022's immutable-owner and mint-required account extensions. The
total must fit `maxSponsorLamports`; the simulated sponsor debit must also fit the
computed ceiling. Fresh signing recomputes the cost and refuses a changed review.
The read RPC must expose `getMinimumBalanceForRentExemption` for these bounded sizes.

The cost field is optional only to keep older saved records readable. Sponsor
co-signing refuses records without a positive ceiling; prepare and review a fresh
quote instead. The app must still enforce per-account and global budgets, minimum
trade value and fee economics. Sponsored output ATAs can later be closed by their
owner; static rent accounting bounds this cost, rather than preventing that action.

The prepared review includes raw build data and exact immutable message bytes.
Client signing re-runs these checks through the app's explicitly selected RPC.
RPC answers and the pinned Jupiter program remain trust boundaries; simulation
is not a cryptographic execution attestation. Current mint authorities can change
mint policy, and the program can be upgraded independently.

`validateSignedSolanaSwap` and `coSignSolanaSwap` are server building blocks:
use only the prepared record previously saved by the server. Never accept a
caller's prepared review as sponsor authorization. Co-signing requires the valid
owner signature and preserves exact reviewed message bytes. The app must also
bind requests to owner identity, enforce sponsor budgets and reserve funds before
broadcast. Sponsorship has no automatic user-paid fallback.

`verifySolanaReceive` verifies a finalized transaction's actual recipient delta.
The caller must bind its signature to the provider's authenticated payout record.
`getSolanaTransactionStatus` returns `expired-unresolved` when a missing historical
status outlives the blockhash. That observation does not authorize a replacement.

Jupiter reference: [Build](https://developers.jup.ag/docs/swap/build) and
[explicit integrator payer](https://developers.jup.ag/docs/swap/advanced/gasless).
Fees are explicit `feeBps`, `feeAccount` and `feeMint` review fields. Jupiter's
platform fee collection and this package's configured integrator fee are public.

`proveSolanaNotIncluded` checks an expired submission against its original
historical window, including after a reload days later. Preparation separately
records a conservative expiry bound from the selected RPC's latest known hash,
after verifying that the transaction's original hash is valid. This bound never
replaces the signed blockhash or extends Jupiter's signing eligibility.

Recovery requires an invalid original blockhash at a fresh finalized context,
full archival coverage for at most 300 historical slots after preparation, every
produced block's signatures and a continuous parent/hash chain. The final covered
header must have a block height beyond the saved RPC expiry bound and must match
a second read. Missing or changed headers, pruned history, an inclusion or a
window that does not reach the bound remains unknown. The selected RPC is a trust
boundary; this observation is not a cryptographic light-client proof. A missing
transaction or elapsed deadline alone never proves non-inclusion.

## Solana Privacy Ring receipts

Use `verifySolanaRingDeposit` to verify a finalized, proofless deposit of one SPL
token into the default Solana mainnet Ring. Supply the signature from the
reviewed provider execution, its expected Solana depositor, the recipient's
public address and owner hash resolved from the Ring registry, the token and
the minimum amount in atomic units. Supply a trusted RPC that returns the
`result` values for `getGenesisHash` and raw JSON `getTransaction`, with
transaction version 1 enabled.

A finalized receipt confirms the deposit into the Ring. The recipient syncs
its private balance separately. The deposit exposes the depositor, recipient,
token and amount. The default Ring encrypts asset and amount for later Ring
transfers, while wallet addresses remain visible.
[Helius deposit guide](https://www.helius.dev/docs/privacy/guides/deposit)
