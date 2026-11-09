import { WalletError } from "@z-stack/core";
import type { BlockTransport } from "./lwd";

/** Compare public metadata with Rust's transaction schedule, never a server-supplied schedule. */
export async function assertSpendingConsensus(
  transport: BlockTransport,
  network: string,
  scanned: number,
  branchId: ((network: string, height: number) => string) | undefined,
  signal: AbortSignal,
): Promise<void> {
  if (!transport.info) return;
  const info = await transport.info(signal);
  signal.throwIfAborted();
  // Custom transports without branch metadata retain their existing contract.
  if (info.consensusBranchId === undefined) return;
  const height = info.blockHeight;
  if (!branchId || height === undefined || !Number.isSafeInteger(height) || height < 1 ||
      height >= 0xffffffff || !Number.isSafeInteger(scanned) || scanned < 1 || scanned >= 0xffffffff ||
      !/^[a-f0-9]{8}$/i.test(info.consensusBranchId)) {
    throw new WalletError("sync_required", "The wallet cannot verify the node's consensus rules. Update the wallet and check the connection before sending.");
  }
  if (branchId(network, height).toLowerCase() !== info.consensusBranchId.toLowerCase()) {
    throw new WalletError("sync_required", "The wallet and node disagree on the active network upgrade. Update the wallet before sending. Nothing was broadcast.");
  }
  if (branchId(network, scanned + 1) !== branchId(network, height + 1)) {
    throw new WalletError("sync_required", "A network upgrade changed while this payment was prepared. Sync and review it again. Nothing was broadcast.");
  }
}
