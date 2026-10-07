import { baseOperation } from "./errors";
import { freezeBaseNetwork, type BaseNetwork } from "./network";
import { createEthTransfers } from "./eth";
import { createUsdcTransfers } from "./usdc";

/** Stateless payment preparation. Integrators own unlock, durable exclusion and submission orchestration. */
export function createBaseTransfers(options: BaseNetwork) {
  const network = freezeBaseNetwork(options);
  const { prepareEthTransfer } = createEthTransfers(network);
  const { prepareUsdcTransfer, submitUsdcTransfer } = createUsdcTransfers(network);
  return Object.freeze({
    prepareEth: (...args: Parameters<typeof prepareEthTransfer>) =>
      baseOperation(() => prepareEthTransfer(...args)),
    prepareUsdc: (...args: Parameters<typeof prepareUsdcTransfer>) =>
      baseOperation(() => prepareUsdcTransfer(...args)),
    submitUsdc: (...args: Parameters<typeof submitUsdcTransfer>) =>
      baseOperation(() => submitUsdcTransfer(...args)),
  });
}
export type { PreparedUsdcTransfer } from "./usdc";
export { estimateBaseOperatorFee } from "./operator-fee";
