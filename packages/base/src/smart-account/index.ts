import { freezeSmartAccountDeployment, type SmartAccountDeployment } from "./deployment";
import { createIntents } from "./intent";
import { createReadiness } from "./readiness";
import { createSigning } from "./signing";
import { createSettlement } from "./settlement";

/** No provider, paymaster, enabled flag or deployment is selected by the SDK. */
export function createBaseSmartAccount(deployment: SmartAccountDeployment) {
  const manifest = freezeSmartAccountDeployment(deployment);
  return Object.freeze({
    deployment: manifest,
    ...createIntents(manifest),
    ...createReadiness(manifest),
    ...createSigning(manifest),
    ...createSettlement(manifest),
  });
}
export type { SmartAccountDeployment } from "./deployment";
export type { SponsoredTransferIntent } from "./intent";
export type { SmartAccountReadiness } from "./readiness";
export type {
  SigningGuard,
  CodeConsent,
  TransferConsent,
  SponsorPolicy,
  AuthorizationRecord,
  SignedOperationRecord,
  SaveRecord,
  JournaledAuthorization,
  JournaledOperation,
  RawBundlerTransport,
} from "./signing";
export type { ExpectedSponsoredSettlement, SponsoredSettlement } from "./settlement";
export { accountAbi, usdcTransferAbi, nonceAbi } from "./abis";
export { simple7702Owner } from "./owner";
export { runBaseSponsoredTransfer } from "./session";
export type {
  SponsoredTransferApprovals,
  BaseSponsorProvider,
  SponsoredTransferJournal,
} from "./session";
export { createBaseBundlerTransport } from "./bundler";
export type { BasePublicClient } from "./client";
