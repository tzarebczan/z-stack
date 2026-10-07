/** JSON-RPC against local regtest Zebra (`LightServer::LOCAL_REGTEST_ZEBRA_RPC`). */

export function regtestRpcUrl(env = process.env) {
  if (env.Z_STACK_ZEBRA_RPC) return env.Z_STACK_ZEBRA_RPC;
  const port = env.Z_STACK_REGTEST_RPC_PORT || "29232";
  if (!/^[0-9]+$/.test(port) || Number(port) < 1 || Number(port) > 65535) {
    throw new Error("Z_STACK_REGTEST_RPC_PORT must be a port from 1 to 65535");
  }
  return `http://127.0.0.1:${Number(port)}`;
}

/** Native Zaino endpoint: URL override wins, otherwise follow the published port. */
export function regtestLwdUrl(env = process.env) {
  if (env.Z_STACK_REGTEST_LWD) return env.Z_STACK_REGTEST_LWD;
  const port = env.Z_STACK_REGTEST_LWD_PORT || "28137";
  if (!/^[0-9]+$/.test(port) || Number(port) < 1 || Number(port) > 65535)
    throw new Error("Z_STACK_REGTEST_LWD_PORT must be a port from 1 to 65535");
  return `http://127.0.0.1:${Number(port)}`;
}

export const ZEBRA_RPC = regtestRpcUrl();

export async function zebraRpc(method, params = []) {
  const res = await fetch(ZEBRA_RPC, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  if (!res.ok) {
    throw new Error(`Zebra RPC HTTP ${res.status}`);
  }
  const body = await res.json();
  if (body.error) {
    throw new Error(`Zebra RPC ${method}: ${JSON.stringify(body.error)}`);
  }
  return body.result;
}

export async function waitForZebra(timeoutMs = 60_000) {
  const start = Date.now();
  let last = "";
  while (Date.now() - start < timeoutMs) {
    try {
      return await zebraRpc("getblockchaininfo");
    } catch (e) {
      last = e instanceof Error ? e.message : String(e);
      await new Promise((r) => setTimeout(r, 1000));
    }
  }
  throw new Error(`Zebra RPC not ready: ${last}`);
}

export async function generate(n) {
  const count = Math.max(1, Number(n) || 1);
  // Zebra accepts generate(nblocks) on regtest.
  return zebraRpc("generate", [count]);
}
