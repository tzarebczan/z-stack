import assert from "node:assert/strict";
import { test } from "node:test";
import { WalletError } from "@z-stack/core";
import { createBlockPrefetch } from "../src/prefetch.ts";
import { createLightServerRecovery, LightServerUnavailableError } from "../src/light-server-recovery.ts";
import { isTransientLightServerError, TransientLightServerError } from "../src/lwd.ts";
import { setImmediate as turn } from "node:timers/promises";

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

test("light-server recovery waits through a short outage and resets after a successful read", async () => {
  const controller = new AbortController();
  const waiting: number[] = [];
  const recovery = createLightServerRecovery({
    signal: controller.signal, graceMs: 100, retryBaseMs: 1,
    onWaiting: (remaining) => waiting.push(remaining),
  });
  let calls = 0;
  const result = await recovery.run(async () => {
    if (++calls <= 3) throw new TypeError("temporary connection failure");
    return 42;
  });
  assert.equal(result, 42);
  assert.equal(calls, 4);
  assert.equal(waiting.length, 3);
  await recovery.waitAfter(new TypeError("second outage"));
  assert.equal(waiting.length, 4, "a successful read starts a fresh outage window");
});

test("a 15-second light-server restart stays inside the 90-second scan grace", async () => {
  let clock = 0;
  let attempts = 0;
  const waits: number[] = [];
  const recovery = createLightServerRecovery({
    signal: new AbortController().signal,
    graceMs: 90_000,
    retryBaseMs: 1,
    now: () => clock,
    onWaiting: remaining => waits.push(remaining),
  });
  const height = await recovery.run(async () => {
    if (++attempts <= 4) {
      clock = attempts * 5_000;
      throw new TypeError("Zaino is restarting");
    }
    return 3_493_000;
  });
  assert.equal(height, 3_493_000);
  assert.equal(attempts, 5);
  assert.deepEqual(waits, [90_000, 85_000, 80_000, 75_000]);
});

test("light-server recovery does not retry malformed or authorization failures", async () => {
  assert.equal(isTransientLightServerError(new TransientLightServerError("503")), true);
  assert.equal(isTransientLightServerError(new Error("grpc-status: 14")), true);
  assert.equal(isTransientLightServerError(new Error("grpc-web GetBlockRange: HTTP 503")), true);
  assert.equal(isTransientLightServerError(new Error("grpc-web GetBlockRange: HTTP 401")), false);
  assert.equal(isTransientLightServerError(new SyntaxError("invalid compact block")), false);
  const recovery = createLightServerRecovery({ signal: new AbortController().signal, retryBaseMs: 1 });
  let calls = 0;
  await assert.rejects(recovery.run(async () => { calls++; throw new Error("chain mismatch"); }), /chain mismatch/);
  assert.equal(calls, 1);
});

test("light-server recovery is bounded and stops immediately on cancellation", async () => {
  const controller = new AbortController();
  const recovery = createLightServerRecovery({ signal: controller.signal, graceMs: 20, retryBaseMs: 2 });
  let calls = 0;
  await assert.rejects(recovery.run(async () => { calls++; throw new TypeError("offline"); }), (error) => {
    assert.ok(error instanceof LightServerUnavailableError);
    assert.equal(WalletError.fromUnknown(error).code, "transport");
    assert.ok(error.cause instanceof TypeError);
    return true;
  });
  assert.ok(calls >= 2 && calls < 12, `retry count was ${calls}`);
  const cancelled = new AbortController();
  let waiting = 0;
  const recovering = createLightServerRecovery({
    signal: cancelled.signal, graceMs: 1000, retryBaseMs: 100,
    onWaiting: () => { waiting++; cancelled.abort(); },
  });
  let cancelledCalls = 0;
  await assert.rejects(recovering.run(async () => { cancelledCalls++; throw new TypeError("offline"); }), /sync cancelled/);
  assert.equal(waiting, 1);
  assert.equal(cancelledCalls, 1);
});

test("HTTP tip, tree, and roots mark gateway outages retryable but keep bad data permanent", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  let status = 503;
  let calls = 0;
  t.mock.method(globalThis, "fetch", async () => {
    calls++;
    return status !== 200
      ? new Response("upstream restarting", { status, headers: { "content-type": "text/plain" } })
      : new Response("{malformed", { status: 200, headers: { "content-type": "application/json" } });
  });
  const transport = httpLwdTransport("http://127.0.0.1:1239", "mainnet");
  for (const read of [() => transport.tip(), () => transport.treeState!(1), () => transport.subtreeRoots!("orchard")]) {
    await assert.rejects(read(), (error) => isTransientLightServerError(error));
    status = 401;
    await assert.rejects(read(), (error) => !isTransientLightServerError(error));
    status = 503;
  }
  assert.equal(calls, 6);
  status = 200;
  await assert.rejects(transport.tip(), (error) => !isTransientLightServerError(error));
});

test("gRPC-Web range timeout and unavailable trailer return once to the bounded scan recovery", async (t) => {
  const { grpcWebTransport } = await import("../src/lwd.ts");
  let calls = 0;
  let mode: "timeout" | "unavailable" | "invalid" = "timeout";
  t.mock.method(globalThis, "fetch", async () => {
    calls++;
    if (mode === "timeout") throw new DOMException("range deadline", "TimeoutError");
    const trailer = new TextEncoder().encode(`grpc-status: ${mode === "unavailable" ? 14 : 3}\r\n`);
    const frame = new Uint8Array(5 + trailer.length);
    frame[0] = 0x80;
    new DataView(frame.buffer).setUint32(1, trailer.length, false);
    frame.set(trailer, 5);
    return new Response(frame);
  });
  const transport = grpcWebTransport("https://example.invalid");
  for (mode of ["timeout", "unavailable", "invalid"] as const) {
    calls = 0;
    await assert.rejects(transport.blocks(1, 1000),
      (error) => isTransientLightServerError(error) === (mode !== "invalid"));
    assert.equal(calls, 1, `${mode} must not fan out into recursive range retries`);
  }
});

test("prefetch reserves buffer space for outstanding requests", async () => {
  let started = 0;
  const p = createBlockPrefetch({
    start: 1, tip: 100, batch: 1, prefetch: 8, buffer: 16,
    fetch: async () => { started++; return new Uint8Array(1024); },
  });
  await turn();
  assert.equal(started, 16);
  assert.equal(p.buffered(), 16);
  assert.equal(p.inFlight(), 0);
  await p.next();
  await turn();
  assert.equal(started, 17);
  assert.equal(p.buffered(), 16);
  p.dispose();
  assert.equal(p.buffered(), 0);
});

test("dispose wakes a pending next and aborts outstanding fetches", async () => {
  const signals: AbortSignal[] = [];
  const p = createBlockPrefetch({
    start: 1, tip: 100, batch: 1, prefetch: 4,
    fetch: (_s, _e, signal) => {
      signals.push(signal);
      return new Promise((_resolve, reject) => {
        signal.addEventListener("abort", () => reject(signal.reason), { once: true });
      });
    },
  });
  const waiting = p.next();
  await turn();
  p.dispose();
  await assert.rejects(waiting, /cancelled/);
  await turn();
  assert.equal(signals.length, 4);
  assert.ok(signals.every((s) => s.aborted));
  assert.equal(p.inFlight(), 0);
});

test("external cancellation wakes next even when the transport never settles", async () => {
  const controller = new AbortController();
  const p = createBlockPrefetch({
    start: 1, tip: 2, batch: 1, prefetch: 1, signal: controller.signal,
    fetch: async () => new Promise<Uint8Array>(() => {}),
  });
  const waiting = p.next();
  controller.abort();
  await assert.rejects(waiting, /cancelled/);
  p.dispose();
});

test("synchronous and falsy fetch failures reject without hanging", async () => {
  for (const failure of [new Error("sync fetch failed"), null]) {
    const p = createBlockPrefetch({
      start: 1, tip: 1, batch: 1, prefetch: 1,
      fetch: () => { throw failure; },
    });
    await p.next().then(
      () => assert.fail("fetch should fail"),
      (error) => assert.equal(error, failure),
    );
    p.dispose();
  }
});

test("block transports propagate cancellation without retrying", async (t) => {
  const { httpLwdTransport, grpcWebTransport } = await import("../src/lwd.ts");
  let calls = 0;
  t.mock.method(globalThis, "fetch", async (_url: unknown, init: RequestInit) => {
    calls++;
    const signal = init.signal!;
    return new Promise<Response>((_resolve, reject) => {
      signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    });
  });
  for (const transport of [httpLwdTransport("http://127.0.0.1:1239"), grpcWebTransport("https://example.invalid")]) {
    const controller = new AbortController();
    const reading = transport.blocks(1, 1000, controller.signal);
    controller.abort();
    await assert.rejects(reading, /abort/i);
  }
  assert.equal(calls, 2);
});

test("HTTP block retries discard a truncated body and request identical bounds", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  const calls: Array<{ url: string; signal: AbortSignal | null | undefined; auth: string | null }> = [];
  const controller = new AbortController();
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit) => {
    calls.push({ url, signal: init.signal, auth: new Headers(init.headers).get("authorization") });
    if (calls.length === 1) {
      return new Response(new ReadableStream<Uint8Array>({
        start(stream) {
          stream.enqueue(new Uint8Array([1, 2, 3, 4]));
          // Fail after arrayBuffer has started consuming the response prefix.
          setTimeout(() => stream.error(new TypeError("upstream closed after prefix")), 0);
        },
      }));
    }
    return new Response(new Uint8Array([9, 8, 7]));
  });
  const transport = httpLwdTransport("http://127.0.0.1:1239", "regtest", undefined, undefined, "test-token");
  assert.deepEqual(await transport.blocks(1001, 2000, controller.signal), new Uint8Array([9, 8, 7]));
  assert.equal(calls.length, 2);
  assert.equal(calls[0].url, calls[1].url);
  assert.equal(new URL(calls[1].url).searchParams.get("start"), "1001");
  assert.equal(new URL(calls[1].url).searchParams.get("end"), "2000");
  assert.ok(calls.every((call) => call.auth === "Bearer test-token"));
  // Each request follows the caller's cancellation (combined with the stall watchdog).
  assert.ok(calls.every((call) => call.signal && !call.signal.aborted));
  controller.abort();
  assert.ok(calls.every((call) => call.signal?.aborted));
});

test("HTTP block network retries are bounded and preserve the final failure", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  let calls = 0;
  const failure = new TypeError("persistent upstream truncation");
  t.mock.method(globalThis, "fetch", async () => { calls++; throw failure; });
  await assert.rejects(httpLwdTransport("http://127.0.0.1:1239", "regtest").blocks(1, 1000), (e) => e === failure);
  assert.equal(calls, 3, "one initial request and only two retries");
});

test("HTTP block retries recover gateway failure but do not retry permanent status or valid malformed bytes", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  let calls = 0;
  let status = 503;
  let errorBody: unknown = { error: "upstream unavailable" };
  t.mock.method(globalThis, "fetch", async () => {
    calls++;
    return status === 200 ? new Response(new Uint8Array([255])) : Response.json(errorBody, { status });
  });
  const transport = httpLwdTransport("http://127.0.0.1:1239", "regtest");
  const recovering = transport.blocks(1, 1000);
  await turn();
  status = 200;
  assert.deepEqual(await recovering, new Uint8Array([255]));
  assert.equal(calls, 2);
  for (status of [400, 401, 404, 429, 500]) {
    calls = 0;
    await assert.rejects(transport.blocks(1, 1000));
    assert.equal(calls, 1, `HTTP ${status} must not trigger a retry`);
  }
  status = 404;
  for (errorBody of [null, { error: { bad: "error field" } }]) {
    calls = 0;
    await assert.rejects(transport.blocks(1, 1000));
    assert.equal(calls, 1, "malformed error JSON cannot turn a permanent status into a retryable TypeError");
  }
  status = 200;
  calls = 0;
  assert.deepEqual(await transport.blocks(1, 1000), new Uint8Array([255]));
  assert.equal(calls, 1, "complete response bytes are validated once by Rust, never retried or altered here");
});

test("HTTP block cancellation during retry backoff is immediate and cannot start another request", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  const controller = new AbortController();
  let calls = 0;
  t.mock.method(globalThis, "fetch", async () => { calls++; throw new TypeError("network failed"); });
  const reading = httpLwdTransport("http://127.0.0.1:1239", "regtest").blocks(1, 1000, controller.signal);
  await turn();
  controller.abort();
  await assert.rejects(reading, (error) => error === controller.signal.reason);
  assert.equal(calls, 1);
  await assert.rejects(httpLwdTransport("http://127.0.0.1:1239", "regtest").blocks(1, 1000, controller.signal),
    (error) => error === controller.signal.reason);
  assert.equal(calls, 1, "an already-cancelled range must not call fetch");
});

test("HTTP block abort and timeout failures never retry even without an external signal", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  let calls = 0;
  let failure: DOMException;
  let errorResponse = false;
  t.mock.method(globalThis, "fetch", async () => {
    calls++;
    // Exercise the body-read rejection path, not only the fetch promise.
    return { ok: !errorResponse, status: 503,
      arrayBuffer: async () => { throw failure; }, json: async () => { throw failure; } } as Response;
  });
  const transport = httpLwdTransport("http://127.0.0.1:1239", "regtest");
  for (const name of ["AbortError", "TimeoutError"]) {
    calls = 0;
    failure = new DOMException("request cancelled", name);
    await assert.rejects(transport.blocks(1, 1000), (error) => error === failure);
    assert.equal(calls, 1);
    calls = 0;
    errorResponse = true;
    await assert.rejects(transport.blocks(1, 1000), (error) => error === failure);
    assert.equal(calls, 1, "cancelling an HTTP error body must not turn cancellation into a gateway retry");
    errorResponse = false;
  }
});

test("prefetch refills when any range finishes, not only the head", async () => {
  const started: number[] = [];
  const fetch = (s: number, _e: number) => {
    started.push(s);
    const ms = s === 1 ? 80 : 8;
    return new Promise<Uint8Array>((resolve) => {
      setTimeout(() => resolve(new Uint8Array([1])), ms);
    });
  };
  const p = createBlockPrefetch({
    start: 1,
    tip: 30,
    batch: 1,
    prefetch: 4,
    fetch,
  });
  await delay(30);
  p.dispose();
  assert.ok(
    started.includes(5),
    `later ranges should start while the slow head is still in flight; started ${started.join(",")}`,
  );
  assert.ok(started.length >= 7, `expected refill past the first window, started ${started.join(",")}`);
});

test("prefetch yields ranges in height order when later fetches finish first", async () => {
  const fetch = (s: number, e: number) => {
    const ms = s === 1 ? 40 : 5;
    return new Promise<Uint8Array>((resolve) => {
      setTimeout(() => resolve(new Uint8Array([s, e])), ms);
    });
  };
  const p = createBlockPrefetch({
    start: 1,
    tip: 6,
    batch: 1,
    prefetch: 4,
    fetch,
  });
  const got: number[] = [];
  for (;;) {
    const job = await p.next();
    if (!job) break;
    got.push(job.start);
  }
  p.dispose();
  assert.deepEqual(got, [1, 2, 3, 4, 5, 6]);
});

test("prefetch keeps HTTP going while apply has not consumed", async () => {
  const started: number[] = [];
  const fetch = (s: number, _e: number) => {
    started.push(s);
    return Promise.resolve(new Uint8Array([s & 255]));
  };
  const p = createBlockPrefetch({
    start: 1,
    tip: 20,
    batch: 1,
    prefetch: 4,
    buffer: 8,
    fetch,
  });
  await delay(20);
  assert.ok(
    started.length >= 8,
    `apply lag must not stall the next HTTP start; started ${started.join(",")}`,
  );
  p.dispose();
});

test("prefetch starts HTTP during a delayed seed, before next()", async () => {
  const started: number[] = [];
  const fetch = (s: number, _e: number) => {
    started.push(s);
    return new Promise<Uint8Array>((resolve) => {
      setTimeout(() => resolve(new Uint8Array([s & 255])), 80);
    });
  };
  const p = createBlockPrefetch({
    start: 3_335_466,
    tip: 3_479_009,
    batch: 1_000,
    prefetch: 8,
    buffer: 16,
    fetch,
  });
  await delay(20);
  assert.equal(started.length, 8, `seed overlap should already have 8 GETs; started ${started.join(",")}`);
  assert.equal(started[0], 3_335_466);
  p.dispose();
});

test("prefetch 8 starts eight HTTP ranges before any apply", async () => {
  let inFlight = 0;
  let max = 0;
  const started: number[] = [];
  const fetch = (s: number, e: number) => {
    started.push(s);
    inFlight += 1;
    max = Math.max(max, inFlight);
    return new Promise<Uint8Array>((resolve) => {
      setTimeout(() => {
        inFlight -= 1;
        resolve(new Uint8Array([s & 255, e & 255]));
      }, 40);
    });
  };
  const p = createBlockPrefetch({
    start: 1,
    tip: 64_000,
    batch: 8_000,
    prefetch: 8,
    fetch,
  });
  await delay(15);
  p.dispose();
  assert.equal(max, 8, `expected 8 in-flight GETs, max ${max}, started ${started.join(",")}`);
  assert.equal(started.length, 8);
});

test("consumeDelimitedStream pages a chunked body", async () => {
  const { consumeDelimitedStream } = await import("../src/lwd.ts");
  const frame = (n: number) => {
    const payload = new Uint8Array([n]);
    const out = new Uint8Array(4 + payload.length);
    new DataView(out.buffer).setUint32(0, payload.length);
    out.set(payload, 4);
    return out;
  };
  const bytes = new Uint8Array([...frame(1), ...frame(2), ...frame(3)]);
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      c.enqueue(bytes.subarray(0, 5));
      c.enqueue(bytes.subarray(5));
      c.close();
    },
  });
  const pages: number[] = [];
  await consumeDelimitedStream(stream, 2, async (blob, n) => {
    pages.push(n, blob.byteLength);
  });
  assert.deepEqual(pages, [2, 10, 1, 5]);
});

test("blockStream rejects a short or overlong range instead of reporting success", async (t) => {
  const { httpLwdTransport } = await import("../src/lwd.ts");
  // One length-delimited fixture block; framing alone suffices to count the page.
  const block = new Uint8Array([0, 0, 0, 1, 7]);
  t.mock.method(globalThis, "fetch", async () => new Response(block));
  const transport = httpLwdTransport("http://127.0.0.1:1239", "regtest");
  await assert.rejects(transport.blockStream!(1, 2, 10, async () => {}), /incomplete range/);
});
