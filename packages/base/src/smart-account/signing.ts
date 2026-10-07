import type { BasePublicClient } from "./client";
import type { SmartAccountDeployment } from "./deployment";

import {
  getAddress,
  isAddress,
  isHex,
  recoverTypedDataAddress,
  zeroAddress,
  type Address,
  type Hash,
  type Hex,
  type SignedAuthorization,
} from "viem";

import type { HDAccount } from "viem/accounts";

import { recoverAuthorizationAddress } from "viem/utils";

import {
  formatUserOperationRequest,
  getUserOperationHash,
  getUserOperationTypedData,
  toSimple7702SmartAccount,
  type UserOperation,
} from "viem/account-abstraction";

import { createIntents, type SponsoredTransferIntent } from "./intent";


import { simple7702Owner } from "./owner";

import { createReadiness, type SmartAccountReadiness } from "./readiness";

export interface SigningGuard {
  /** Bind to the app's active wallet session and shared signing lock. */
  assertCurrent(): void;
  now(): number;
  /** Must call readSmartAccountReadiness: freshly verifies pinned code and current chain nonces. */
  recheck(): Promise<SmartAccountReadiness>;
}

export interface CodeConsent {
  readonly owner: string;
  readonly delegate: string;
  readonly chainId: number;
  /** Approval acknowledges that delegation has no expiry and persists if a UserOperation fails. */
  readonly persistentDelegationApproved: true;
}

export interface TransferConsent {
  readonly callData: Hex;
  readonly deadline: number;
  /** Hash of the final unsigned UserOperation, including sponsor data, every gas field and delegate. */
  readonly operationHash: Hash;
  readonly sponsoredFeesApproved: true;
}

export interface SponsorPolicy {
  readonly paymaster: Address;
  readonly maxFeePerGas: bigint;
  /** Maximum total sponsored gas cost in wei; no token fee or ETH fallback is authorized. */
  readonly maxSponsoredCost: bigint;
}

export interface AuthorizationRecord {
  readonly kind: "authorization";
  readonly intent: SponsoredTransferIntent;
  readonly state: SmartAccountReadiness;
  readonly authorization: SignedAuthorization;
}

export interface SignedOperationRecord {
  readonly kind: "signed-operation";
  readonly intent: SponsoredTransferIntent;
  readonly state: SmartAccountReadiness;
  readonly operation: UserOperation<"0.8">;
  readonly expectedHash: Hash;
}

export type SaveRecord = (record: AuthorizationRecord | SignedOperationRecord) => Promise<void>;

// In-memory handles can only be issued after durable saves succeed. No provider receives a signature before that save.
export interface JournaledAuthorization {
  readonly authorization: Readonly<SignedAuthorization>;
}

export interface JournaledOperation {
  readonly expectedHash: Hash;
  readonly request: Readonly<ReturnType<typeof formatUserOperationRequest>>;
}

export interface RawBundlerTransport {
  /** Inject a transport configured with zero retries. Do not use high-level sendUserOperation. */
  request(parameters: {
    method: "eth_sendUserOperation";
    params: readonly [JournaledOperation["request"], Address];
  }): Promise<unknown>;
}
export function createSigning(manifest: Readonly<SmartAccountDeployment>) {
  const { assertTransferIntent, freezeTransferIntent } = createIntents(manifest);
  const { assertReadinessUnchanged } = createReadiness(manifest);

  /** Policy is deliberately required, with no default or provider-selected paymaster. */
  function freezeSponsorPolicy(policy: SponsorPolicy): Readonly<SponsorPolicy> {
    if (
      !isAddress(policy.paymaster) ||
      getAddress(policy.paymaster) === zeroAddress ||
      typeof policy.maxFeePerGas !== "bigint" ||
      policy.maxFeePerGas <= 0n ||
      policy.maxFeePerGas >= 1n << 128n ||
      typeof policy.maxSponsoredCost !== "bigint" ||
      policy.maxSponsoredCost <= 0n ||
      policy.maxSponsoredCost >= 1n << 256n
    )
      throw new Error("Invalid explicit Base sponsor policy.");
    return Object.freeze({
      paymaster: getAddress(policy.paymaster),
      maxFeePerGas: policy.maxFeePerGas,
      maxSponsoredCost: policy.maxSponsoredCost,
    });
  }

  /** Use the final quote's hash in transfer consent before any UserOperation signature. */
  function reviewedOperationHash(operation: UserOperation<"0.8">): Hash {
    return getUserOperationHash({
      chainId: manifest.chainId,
      entryPointAddress: manifest.entryPoint,
      entryPointVersion: "0.8",
      userOperation: operation,
    });
  }

  const authorizations = new WeakMap<JournaledAuthorization, AuthorizationRecord>();

  const signedAuthorizations = new WeakSet<JournaledAuthorization>();

  const operations = new WeakMap<JournaledOperation, SignedOperationRecord>();

  const submitted = new WeakSet<JournaledOperation>();
  const submitting = new WeakSet<JournaledOperation>();

  async function check(
    guard: SigningGuard,
    intent: SponsoredTransferIntent,
    state: SmartAccountReadiness,
  ) {
    guard.assertCurrent();
    if (guard.now() >= intent.deadline) throw new Error("The reviewed Base transfer expired.");
    const current = await guard.recheck();
    guard.assertCurrent();
    if (guard.now() >= intent.deadline) throw new Error("The reviewed Base transfer expired.");
    assertReadinessUnchanged(state, current);
  }

  function assertCodeConsent(consent: CodeConsent, owner: string) {
    if (
      consent.persistentDelegationApproved !== true ||
      consent.chainId !== manifest.chainId ||
      getAddress(consent.owner) !== getAddress(owner) ||
      getAddress(consent.delegate) !== getAddress(manifest.delegate)
    )
      throw new Error("Approve the pinned persistent Base delegation first.");
  }

  async function assertAuthorization(
    authorization: SignedAuthorization,
    state: SmartAccountReadiness,
  ) {
    if (
      authorization.chainId !== manifest.chainId ||
      authorization.nonce !== state.authorizationNonce ||
      getAddress(authorization.address) !== getAddress(manifest.delegate) ||
      getAddress(await recoverAuthorizationAddress({ authorization })) !== getAddress(state.owner)
    )
      throw new Error("Base authorization signer, nonce, chain or implementation mismatch.");
  }

  /** Signing this tuple enables persistent wallet code even if the later transfer fails. Never call during preparation. */
  async function signJournaledAuthorization(parameters: {
    owner: HDAccount;
    intent: SponsoredTransferIntent;
    state: SmartAccountReadiness;
    consent: CodeConsent;
    guard: SigningGuard;
    save: SaveRecord;
  }): Promise<JournaledAuthorization> {
    const { guard, save, owner } = parameters;
    const intent = freezeTransferIntent(parameters.intent);
    assertTransferIntent(parameters.intent, intent.callData);
    const state = Object.freeze({ ...parameters.state });
    if (getAddress(owner.address) !== intent.owner || getAddress(state.owner) !== intent.owner)
      throw new Error("The Base signer is not the reviewed wallet.");
    assertCodeConsent(parameters.consent, owner.address);
    const signer = simple7702Owner(owner);
    await check(guard, intent, state);
    assertCodeConsent(parameters.consent, owner.address);
    const authorization = Object.freeze(
      await signer.signAuthorization({
        chainId: manifest.chainId,
        address: manifest.delegate,
        nonce: state.authorizationNonce,
      }),
    );
    await assertAuthorization(authorization, state);
    await check(guard, intent, state);
    const record: AuthorizationRecord = Object.freeze({
      kind: "authorization",
      intent,
      state,
      authorization,
    });
    await save(record);
    await check(guard, intent, state);
    const handle = Object.freeze({ authorization });
    authorizations.set(handle, record);
    return handle;
  }

  const operationKeys = new Set([
    "sender",
    "nonce",
    "callData",
    "factory",
    "factoryData",
    "authorization",
    "signature",
    "callGasLimit",
    "verificationGasLimit",
    "preVerificationGas",
    "maxFeePerGas",
    "maxPriorityFeePerGas",
    "paymaster",
    "paymasterData",
    "paymasterVerificationGasLimit",
    "paymasterPostOpGasLimit",
  ]);

  function assertOperation(
    operation: UserOperation<"0.8">,
    record: AuthorizationRecord,
    policy: SponsorPolicy,
  ) {
    if (Object.keys(operation).some((key) => !operationKeys.has(key)))
      throw new Error("Unexpected sponsored operation field.");
    if (
      getAddress(operation.sender) !== record.intent.owner ||
      operation.nonce !== record.state.operationNonce ||
      operation.factory !== "0x7702" ||
      operation.factoryData !== "0x" ||
      operation.signature !== "0x"
    )
      throw new Error("The provider changed the reviewed Base operation.");
    assertTransferIntent(record.intent, operation.callData);
    if (
      !operation.paymaster ||
      !isAddress(operation.paymaster) ||
      getAddress(operation.paymaster) === zeroAddress ||
      !operation.paymasterData ||
      !isHex(operation.paymasterData, { strict: true }) ||
      operation.paymasterData.length > 8194
    )
      throw new Error("A gas sponsor is required for this Base transfer.");
    for (const value of [
      operation.callGasLimit,
      operation.verificationGasLimit,
      operation.preVerificationGas,
      operation.maxFeePerGas,
      operation.paymasterVerificationGasLimit,
      operation.paymasterPostOpGasLimit,
    ])
      if (typeof value !== "bigint" || value <= 0n || value >= 1n << 128n)
        throw new Error("Invalid sponsored Base gas fields.");
    if (
      typeof operation.maxPriorityFeePerGas !== "bigint" ||
      operation.maxPriorityFeePerGas < 0n ||
      operation.maxPriorityFeePerGas > operation.maxFeePerGas
    )
      throw new Error("Invalid sponsored Base gas fee.");
    if (getAddress(operation.paymaster) !== policy.paymaster)
      throw new Error("The paymaster is not allowed by the Base sponsor policy.");
    if (operation.maxFeePerGas > policy.maxFeePerGas)
      throw new Error("The Base sponsored gas fee exceeds policy.");
    const totalGas =
      operation.callGasLimit +
      operation.verificationGasLimit +
      operation.preVerificationGas +
      (operation.paymasterVerificationGasLimit ?? 0n) +
      (operation.paymasterPostOpGasLimit ?? 0n);
    if (totalGas * operation.maxFeePerGas > policy.maxSponsoredCost)
      throw new Error("The total Base sponsored cost exceeds policy.");
    // The provider may supply an identical tuple, but cannot replace any signed component.
    if (
      !operation.authorization ||
      operation.authorization.address !== record.authorization.address ||
      operation.authorization.chainId !== record.authorization.chainId ||
      operation.authorization.nonce !== record.authorization.nonce ||
      operation.authorization.r !== record.authorization.r ||
      operation.authorization.s !== record.authorization.s ||
      operation.authorization.yParity !== record.authorization.yParity
    )
      throw new Error("The provider changed the saved Base authorization.");
  }

  /** Validate and snapshot a final quote before presenting it for transfer approval. */
  function validateSponsoredOperation(parameters: {
    authorization: JournaledAuthorization;
    prepared: UserOperation<"0.8">;
    sponsorPolicy: Readonly<SponsorPolicy>;
  }): Readonly<UserOperation<"0.8">> {
    const record = authorizations.get(parameters.authorization);
    if (!record) throw new Error("Save the Base authorization before reviewing its quote.");
    const policy = freezeSponsorPolicy(parameters.sponsorPolicy);
    const operation = Object.freeze({
      ...parameters.prepared,
      authorization:
        parameters.prepared.authorization &&
        Object.freeze({ ...parameters.prepared.authorization }),
    });
    assertOperation(operation, record, policy);
    return operation;
  }

  /** Consume one exact, final sponsor response. Preparation is outside this primitive; it cannot reprepare after signing. */
  async function signJournaledOperation(parameters: {
    client: BasePublicClient;
    owner: HDAccount;
    authorization: JournaledAuthorization;
    prepared: UserOperation<"0.8">;
    consent: TransferConsent;
    sponsorPolicy: Readonly<SponsorPolicy>;
    guard: SigningGuard;
    save: SaveRecord;
  }): Promise<JournaledOperation> {
    const record = authorizations.get(parameters.authorization);
    if (!record)
      throw new Error("Save the Base authorization before preparing a sponsored operation.");
    const { owner, guard, save } = parameters;
    if (!parameters.sponsorPolicy || !Object.isFrozen(parameters.sponsorPolicy))
      throw new Error("An explicit frozen Base sponsor policy is required.");
    const policy = freezeSponsorPolicy(parameters.sponsorPolicy);
    const consent = Object.freeze({ ...parameters.consent });
    const operation = Object.freeze({
      ...parameters.prepared,
      authorization:
        parameters.prepared.authorization &&
        Object.freeze({ ...parameters.prepared.authorization }),
    });
    if (getAddress(owner.address) !== record.intent.owner)
      throw new Error("The Base signer changed.");
    assertOperation(operation, record, policy);
    const approvedHash = reviewedOperationHash(operation);
    const approve = () => {
      if (
        consent.sponsoredFeesApproved !== true ||
        consent.callData !== record.intent.callData ||
        consent.deadline !== record.intent.deadline ||
        consent.operationHash !== approvedHash
      )
        throw new Error("Confirm the exact reviewed sponsored USDC transfer first.");
    };
    approve();
    await check(guard, record.intent, record.state);
    await assertAuthorization(record.authorization, record.state);
    const account = await toSimple7702SmartAccount({
      client: parameters.client,
      owner: simple7702Owner(owner),
      entryPoint: "0.8",
      implementation: manifest.delegate,
      getNonce: async (parameters) => {
        if (parameters?.key !== 0n) throw new Error("Only the fixed Base nonce lane is supported.");
        return record.state.operationNonce;
      },
    });
    await check(guard, record.intent, record.state);
    approve();
    if (signedAuthorizations.has(parameters.authorization))
      throw new Error("This authorization already has a signed operation. Reconcile its journal.");
    signedAuthorizations.add(parameters.authorization);
    const signature = await account.signUserOperation({ ...operation, chainId: manifest.chainId });
    const signed = Object.freeze({ ...operation, signature });
    const recovered = await recoverTypedDataAddress({
      ...getUserOperationTypedData({
        chainId: manifest.chainId,
        entryPointAddress: manifest.entryPoint,
        userOperation: signed,
      }),
      signature,
    });
    if (getAddress(recovered) !== record.intent.owner)
      throw new Error("The UserOperation signature is not from the reviewed wallet.");
    const expectedHash = reviewedOperationHash(signed);
    if (expectedHash !== approvedHash)
      throw new Error("The signed Base operation changed the approved quote.");
    await check(guard, record.intent, record.state);
    const signedRecord: SignedOperationRecord = Object.freeze({
      kind: "signed-operation",
      intent: record.intent,
      state: record.state,
      operation: signed,
      expectedHash,
    });
    await save(signedRecord);
    await check(guard, record.intent, record.state);
    const formatted = formatUserOperationRequest(signed);
    if (formatted.eip7702Auth) Object.freeze(formatted.eip7702Auth);
    const handle = Object.freeze({ expectedHash, request: Object.freeze(formatted) });
    operations.set(handle, signedRecord);
    return handle;
  }

  /** One attempt only. Lost acknowledgements retain the saved expectedHash for receipt reconciliation. */
  async function submitJournaledOperation(
    handle: JournaledOperation,
    transport: RawBundlerTransport,
    guard: SigningGuard,
    /** Guarded durable preparation only. Failure leaves this same signed handle retryable. */
    beforeSubmit?: () => Promise<void>,
  ): Promise<Hash> {
    const record = operations.get(handle);
    if (!record || submitted.has(handle) || submitting.has(handle))
      throw new Error(
        "This Base operation is absent or has already been submitted. Reconcile its saved hash.",
      );
    submitting.add(handle);
    try {
      await check(guard, record.intent, record.state);
      await beforeSubmit?.();
      // No fallible guard/await between consuming the handle and entering transport.
      submitted.add(handle);
      const result = await transport.request({
        method: "eth_sendUserOperation",
        params: Object.freeze([handle.request, manifest.entryPoint]),
      });
      if (typeof result !== "string" || result.toLowerCase() !== handle.expectedHash.toLowerCase())
        throw new Error("Bundler hash mismatch. Reconcile the saved UserOperation hash.");
      return handle.expectedHash;
    } finally { submitting.delete(handle); }
  }
  return {
    freezeSponsorPolicy,
    reviewedOperationHash,
    validateSponsoredOperation,
    signJournaledAuthorization,
    signJournaledOperation,
    submitJournaledOperation,
  };
}
