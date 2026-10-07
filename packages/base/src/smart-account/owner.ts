import type { HDAccount, PrivateKeyAccount } from "viem/accounts";

/** viem requires PrivateKeyAccount structurally; forward the existing HD signer, never extract its key. */
export function simple7702Owner(account: HDAccount): PrivateKeyAccount {
  const signAuthorization = account.signAuthorization;
  if (!signAuthorization)
    throw new Error("The selected signer cannot sign EIP-7702 authorizations.");
  return Object.freeze({
    address: account.address,
    publicKey: account.publicKey,
    source: "privateKey",
    type: "local",
    sign: (parameters) => account.sign(parameters),
    signAuthorization: (parameters) => signAuthorization(parameters),
    signMessage: (parameters) => account.signMessage(parameters),
    signTypedData: (parameters) => account.signTypedData(parameters),
    signTransaction: (transaction, options) => account.signTransaction(transaction, options),
  });
}
