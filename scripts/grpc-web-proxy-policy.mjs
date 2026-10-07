// This is a protocol boundary, not IP anonymity or enclave attestation.
const service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";
const shieldedMethods = new Set([
  "GetLatestBlock", "GetBlock", "GetBlockRange", "GetTreeState",
  "GetSubtreeRoots", "GetLightdInfo", "GetTransaction", "SendTransaction",
]);
const transparentMethods = new Set([
  "GetAddressUtxos", "GetAddressUtxosStream", "GetTaddressTxids",
  "GetTaddressTransactions", "GetTaddressBalance", "GetTaddressBalanceStream",
]);

export function proxyPolicy(upstream, allowTransparent) {
  const url = new URL(upstream);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password ||
      url.pathname !== "/" || url.search || url.hash) {
    throw new Error("Upstream must be an HTTP(S) origin without credentials, path or query");
  }
  const local = url.hostname === "localhost" || url.hostname === "[::1]" ||
    /^127\.\d+\.\d+\.\d+$/.test(url.hostname);
  const transparent = allowTransparent ?? local;
  return {
    origin: url.origin,
    allows(path) {
      if (!path?.startsWith(service)) return false;
      const method = path.slice(service.length);
      return shieldedMethods.has(method) || (transparent && transparentMethods.has(method));
    },
  };
}
