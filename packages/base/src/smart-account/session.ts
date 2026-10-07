import type { BasePublicClient } from "./client";
import type { Hash } from "viem";
import type { HDAccount } from "viem/accounts";
import type { UserOperation } from "viem/account-abstraction";
import type { createBaseSmartAccount } from "./index";
import type { SponsoredTransferIntent } from "./intent";
import type { SmartAccountReadiness } from "./readiness";
import type {
  CodeConsent,
  TransferConsent,
  SigningGuard,
  SponsorPolicy,
  SaveRecord,
  RawBundlerTransport,
} from "./signing";

export interface SponsoredTransferApprovals {
  approveCode(details: {
    owner: string;
    delegate: string;
    chainId: number;
    persistent: true;
  }): Promise<CodeConsent>;
  approveTransfer(details: {
    intent: SponsoredTransferIntent;
    operationHash: Hash;
    policy: Readonly<SponsorPolicy>;
    userPaysUsdc: 0n;
    submissionDeadlineOnly: true;
    delayedExecutionPossible: true;
  }): Promise<TransferConsent>;
}
export interface BaseSponsorProvider {
  /** Return one final unsigned quote. The SDK validates all payment, delegation and fee fields. */
  quote(operation: UserOperation<"0.8">): Promise<UserOperation<"0.8">>;
  /** Zero transport retries; use the raw eth_sendUserOperation method only. */
  bundler: RawBundlerTransport;
}
export interface SponsoredTransferJournal {
  save: SaveRecord;
  /** Persist UNKNOWN before sending; persist SUBMITTED after a matching acknowledgement. */
  setSubmission(value: { hash: Hash; phase: "submission-unknown" | "submitted" }): Promise<void>;
}
/** Run inside the app's shared spend lock and fresh signer callback after durable pending exclusion.
 * Experimental: the app must verify its deployment/provider and disclose persistent code and delayed execution. */
export async function runBaseSponsoredTransfer(parameters: {
  account: ReturnType<typeof createBaseSmartAccount>;
  client: BasePublicClient;
  owner: HDAccount;
  intent: SponsoredTransferIntent;
  state: SmartAccountReadiness;
  guard: SigningGuard;
  policy: Readonly<SponsorPolicy>;
  approvals: SponsoredTransferApprovals;
  provider: BaseSponsorProvider;
  journal: SponsoredTransferJournal;
}) {
  const { account, client, owner, guard, approvals, provider, journal } = parameters;
  const intent = account.freezeTransferIntent(parameters.intent);
  const state = Object.freeze({ ...parameters.state });
  const policy = account.freezeSponsorPolicy(parameters.policy);
  const check = async () => {
    guard.assertCurrent();
    if (guard.now() >= intent.deadline) throw new Error("The reviewed Base transfer expired.");
    account.assertReadinessUnchanged(state, await guard.recheck());
    guard.assertCurrent();
    if (guard.now() >= intent.deadline) throw new Error("The reviewed Base transfer expired.");
  };
  await check();
  const consent = await approvals.approveCode({
    owner: intent.owner,
    delegate: account.deployment.delegate,
    chainId: account.deployment.chainId,
    persistent: true,
  });
  await check();
  const authorization = await account.signJournaledAuthorization({
    owner,
    intent,
    state,
    guard,
    consent,
    save: journal.save,
  });
  const fees = await client.estimateFeesPerGas();
  await check();
  if (
    fees.maxFeePerGas === undefined ||
    fees.maxPriorityFeePerGas === undefined ||
    fees.maxFeePerGas > policy.maxFeePerGas
  )
    throw new Error(
      "The sponsored fee estimate is unavailable. Reconcile the saved authorization.",
    );
  const quote = await provider.quote({
    sender: intent.owner,
    nonce: state.operationNonce,
    callData: intent.callData,
    factory: "0x7702",
    factoryData: "0x",
    authorization: authorization.authorization,
    signature: "0x",
    callGasLimit: 200_000n,
    verificationGasLimit: 300_000n,
    preVerificationGas: 100_000n,
    maxFeePerGas: fees.maxFeePerGas,
    maxPriorityFeePerGas: fees.maxPriorityFeePerGas,
  });
  // Snapshot the provider response before an approval UI can yield or mutate it.
  const prepared = account.validateSponsoredOperation({
    authorization,
    prepared: quote,
    sponsorPolicy: policy,
  });
  const operationHash = account.reviewedOperationHash(prepared);
  await check();
  const transferConsent = await approvals.approveTransfer({
    intent,
    operationHash,
    policy,
    userPaysUsdc: 0n,
    submissionDeadlineOnly: true,
    delayedExecutionPossible: true,
  });
  await check();
  const operation = await account.signJournaledOperation({
    client,
    owner,
    authorization,
    prepared,
    consent: transferConsent,
    sponsorPolicy: policy,
    guard,
    save: journal.save,
  });
  const hash = await account.submitJournaledOperation(
    operation,
    provider.bundler,
    guard,
    async () => {
      await check();
      // This durable marker starts dispatch: no subsequent fallible guard before transport.
      await journal.setSubmission({ hash: operation.expectedHash, phase: "submission-unknown" });
    },
  );
  await journal.setSubmission({ hash, phase: "submitted" });
  return Object.freeze({ operationHash: hash, phase: "submitted" as const });
}
