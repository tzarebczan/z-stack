import type { RawBundlerTransport } from "./signing";
import { freezeBaseNetwork, BASE_SEPOLIA } from "../network";

/** Raw ERC-4337 submission only. No provider credentials, quotation, fallback or retries. */
export function createBaseBundlerTransport(options: {
  url: string;
  request?: typeof fetch;
}): RawBundlerTransport {
  // Reuse the HTTPS/loopback policy without retaining the mutable options object.
  const url = freezeBaseNetwork({ ...BASE_SEPOLIA, rpcUrl: options.url }).rpcUrl!;
  const request = options.request ?? fetch;
  return Object.freeze({
    async request(parameters: Parameters<RawBundlerTransport["request"]>[0]) {
      if (parameters.method !== "eth_sendUserOperation")
        throw new Error("Only raw UserOperation submission is supported.");
      const response = await request(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          method: parameters.method,
          params: parameters.params,
        }),
        credentials: "omit",
        cache: "no-store",
        redirect: "error",
        signal: AbortSignal.timeout(15_000),
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new Error("Bundler acknowledgement unavailable. Reconcile the saved operation hash.");
      }
      const reader = response.body.getReader(),
        chunks: Uint8Array[] = [];
      let size = 0;
      try {
        for (;;) {
          const part = await reader.read();
          if (part.done) break;
          size += part.value.byteLength;
          if (size > 32_768) throw new Error("Bundler response exceeds the size limit.");
          chunks.push(part.value);
        }
      } finally {
        await reader.cancel();
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.byteLength;
      }
      const body = JSON.parse(new TextDecoder().decode(bytes));
      if (
        !body ||
        body.jsonrpc !== "2.0" ||
        body.id !== 1 ||
        body.error ||
        typeof body.result !== "string"
      )
        throw new Error("Bundler acknowledgement invalid. Reconcile the saved operation hash.");
      return body.result;
    },
  });
}
