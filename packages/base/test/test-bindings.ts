export { accountAbi, usdcTransferAbi, nonceAbi } from "../src/smart-account/index";
import { BASE_MAINNET, createBaseTransfers, estimateBaseOperatorFee } from "../src/index";
import { createBaseSmartAccount, simple7702Owner } from "../src/smart-account/index";
export type {
  SponsoredTransferIntent,
  SmartAccountReadiness,
  SigningGuard,
  SponsorPolicy,
  AuthorizationRecord,
  SaveRecord,
  SignedOperationRecord,
  JournaledOperation,
} from "../src/smart-account/index";
const BASE_SMART_ACCOUNT = Object.freeze({
  enabled: false,
  chainId: 8453,
  delegate: "0xe6Cae83BdE06E4c305530e199D7217f42808555B",
  delegateCodeHash: "0xcc7b633aef4b2543cb8f37522adf1a401f910f0f6b2430c1eecc11f401ccfcf3",
  entryPoint: "0x4337084D9E255Ff0702461CF8895CE9E3b5Ff108",
  entryPointCodeHash: "0x28f989233f4ffb52e4b168fb74df5dfa52fe0f846141774f5abc56c5604d8e46",
  entryPointVersion: "0.8",
  usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
} as const);
export { BASE_SMART_ACCOUNT, simple7702Owner, estimateBaseOperatorFee };
export const delegationCode = `0xef0100${BASE_SMART_ACCOUNT.delegate.slice(2).toLowerCase()}`;
export const {
  prepareEth: prepareBaseEth,
  prepareUsdc: prepareBaseSwap,
  submitUsdc: submitBaseSwap,
} = createBaseTransfers({ ...BASE_MAINNET, rpcUrl: "https://mainnet.base.org" });
export const {
  freezeTransferIntent,
  assertTransferIntent,
  readSmartAccountReadiness,
  assertReadinessUnchanged,
  freezeSponsorPolicy,
  reviewedOperationHash,
  signJournaledAuthorization,
  signJournaledOperation,
  submitJournaledOperation,
  inspectSponsoredSettlement,
  verifySponsoredSettlement,
} = createBaseSmartAccount(BASE_SMART_ACCOUNT);
