import { mnemonicToAccount, type HDAccount } from "viem/accounts";
import { formatUnits, type Address } from "viem";

/** Standard Ethereum BIP-44 account zero. The caller owns unlock and secret lifetime. */
export function deriveBaseAccount(mnemonic: string): HDAccount {
  return mnemonicToAccount(mnemonic, { accountIndex: 0, addressIndex: 0 });
}
/** Local derivation only; never contacts an RPC or account service. */
export function deriveBaseAddress(mnemonic: string): Address {
  return deriveBaseAccount(mnemonic).address;
}
export interface BaseBalance {
  eth: bigint;
  usdc: bigint;
}
/** Decimal strings preserve large balances without lossy Number conversion. */
export function formatBaseBalance(balance: BaseBalance) {
  return { eth: formatUnits(balance.eth, 18), usdc: formatUnits(balance.usdc, 6) };
}
