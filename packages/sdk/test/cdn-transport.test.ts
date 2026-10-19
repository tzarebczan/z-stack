import assert from "node:assert/strict";
import { test } from "node:test";
import {
  cdnSegmentTransport,
  sliceSegmentFrames,
} from "../src/cdn-transport.ts";
import type { BlockTransport } from "../src/lwd.ts";

/** Helper to create synthetic length-delimited compact-block frames for testing */
function makeFramedBlocks(startHeight: number, count: number, payloadByte = 0xaa): Uint8Array {
  // Each block will have a payload of 10 bytes: [0x08, height_varint, ...]
  const frames: Uint8Array[] = [];
  for (let i = 0; i < count; i++) {
    const height = startHeight + i;
    const payload = new Uint8Array(8);
    payload[0] = 0x08; // field 1 varint
    payload[1] = height & 0x7f;
    payload.fill(payloadByte, 2);

    const frame = new Uint8Array(4 + payload.length);
    new DataView(frame.buffer).setUint32(0, payload.length, false);
    frame.set(payload, 4);
    frames.push(frame);
  }

  const totalLen = frames.reduce((acc, f) => acc + f.length, 0);
  const out = new Uint8Array(totalLen);
  let offset = 0;
  for (const f of frames) {
    out.set(f, offset);
    offset += f.length;
  }
  return out;
}

test("sliceSegmentFrames extracts the exact sub-range of delimited frames", () => {
  const segStart = 1000;
  const count = 10;
  const segment = makeFramedBlocks(segStart, count);

  // Extract heights 1002..1004 (3 blocks)
  const sliced = sliceSegmentFrames(segment, segStart, 1002, 1004);
  const expected = makeFramedBlocks(1002, 3);
  assert.deepEqual(sliced, expected);
});

test("sliceSegmentFrames returns empty when sub-range is outside the segment", () => {
  const segStart = 1000;
  const count = 10;
  const segment = makeFramedBlocks(segStart, count);

  assert.equal(sliceSegmentFrames(segment, segStart, 1020, 1030).length, 0);
  assert.equal(sliceSegmentFrames(segment, segStart, 900, 950).length, 0);
  assert.equal(sliceSegmentFrames(segment, segStart, 1005, 1002).length, 0);
});

test("sliceSegmentFrames rejects corrupted or truncated segment data", () => {
  const segStart = 1000;
  const segment = makeFramedBlocks(segStart, 5);

  // Truncate length header
  const truncatedHeader = segment.subarray(0, 2);
  assert.throws(() => sliceSegmentFrames(truncatedHeader, segStart, 1000, 1001), /truncated length header/);

  // Truncate frame data (declare 100 bytes, provide only 10)
  const truncatedData = new Uint8Array(10);
  new DataView(truncatedData.buffer).setUint32(0, 100, false);
  assert.throws(() => sliceSegmentFrames(truncatedData, segStart, 1000, 1001), /truncated frame data/);
});

test("cdnSegmentTransport fetches historical blocks from CDN when available", async () => {
  const segment1000 = makeFramedBlocks(1000, 1000);
  const requestedUrls: string[] = [];

  const mockFetch: typeof fetch = async (input) => {
    const url = String(input);
    requestedUrls.push(url);
    if (url.includes("/compact-blocks/mainnet/1000.bin")) {
      return new Response(segment1000, { status: 200 });
    }
    return new Response(null, { status: 404 });
  };

  const liveBlocksCalls: Array<{ start: number; end: number }> = [];
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveBlocksCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100,
    fetch: mockFetch,
  });

  // Query 1000..1999 (full segment)
  const result = await transport.blocks(1000, 1999);
  assert.equal(requestedUrls.length, 1);
  assert.equal(requestedUrls[0], "https://cdn.example.com/compact-blocks/mainnet/1000.bin");
  assert.equal(liveBlocksCalls.length, 0, "No live blocks calls should occur on CDN hit");
  assert.deepEqual(result, segment1000);
});

test("cdnSegmentTransport correctly slices and stitches partial ranges across segments", async () => {
  const segment1000 = makeFramedBlocks(1000, 1000, 0x11);
  const segment2000 = makeFramedBlocks(2000, 1000, 0x22);

  const mockFetch: typeof fetch = async (input) => {
    const url = String(input);
    if (url.includes("/compact-blocks/mainnet/1000.bin")) {
      return new Response(segment1000, { status: 200 });
    }
    if (url.includes("/compact-blocks/mainnet/2000.bin")) {
      return new Response(segment2000, { status: 200 });
    }
    return new Response(null, { status: 404 });
  };

  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => makeFramedBlocks(start, end - start + 1),
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100,
    fetch: mockFetch,
  });

  // Query 1500..2050 (spans segment 1000 and 2000 with partial slices)
  const result = await transport.blocks(1500, 2050);

  const expectedHead = makeFramedBlocks(1500, 500, 0x11);
  const expectedTail = makeFramedBlocks(2000, 51, 0x22);
  const expectedCombined = new Uint8Array(expectedHead.length + expectedTail.length);
  expectedCombined.set(expectedHead, 0);
  expectedCombined.set(expectedTail, expectedHead.length);

  assert.deepEqual(result, expectedCombined);
});

test("cdnSegmentTransport falls back to live transport on CDN 404 or network error", async () => {
  const liveCalls: Array<{ start: number; end: number }> = [];

  const mockFetch: typeof fetch = async () => {
    return new Response(null, { status: 404 });
  };

  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1, 0x33);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100,
    fetch: mockFetch,
  });

  const result = await transport.blocks(1000, 1999);
  assert.equal(liveCalls.length, 1);
  assert.deepEqual(liveCalls[0], { start: 1000, end: 1999 });
  assert.deepEqual(result, makeFramedBlocks(1000, 1000, 0x33));

  // Query partial sub-range on 404: only the sub-range should be requested from live transport
  liveCalls.length = 0;
  const partialResult = await transport.blocks(1050, 1070);
  assert.equal(liveCalls.length, 1);
  assert.deepEqual(liveCalls[0], { start: 1050, end: 1070 });
  assert.deepEqual(partialResult, makeFramedBlocks(1050, 21, 0x33));
});

test("cdnSegmentTransport routes live tip blocks directly to live transport without CDN calls", async () => {
  const requestedCdnUrls: string[] = [];
  const liveCalls: Array<{ start: number; end: number }> = [];

  const mockFetch: typeof fetch = async (input) => {
    requestedCdnUrls.push(String(input));
    return new Response(null, { status: 200 });
  };

  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100, // safeTip = 4900
    fetch: mockFetch,
  });

  // Query 4950..5000 (all in tip zone)
  await transport.blocks(4950, 5000);
  assert.equal(requestedCdnUrls.length, 0, "No CDN requests should be made for tip blocks");
  assert.equal(liveCalls.length, 1);
  assert.deepEqual(liveCalls[0], { start: 4950, end: 5000 });
});

test("cdnSegmentTransport splits boundary ranges between historical CDN and live tip", async () => {
  const requestedCdnUrls: string[] = [];
  const liveCalls: Array<{ start: number; end: number }> = [];

  const segment4000 = makeFramedBlocks(4000, 1000, 0x44);

  const mockFetch: typeof fetch = async (input) => {
    requestedCdnUrls.push(String(input));
    return new Response(segment4000, { status: 200 });
  };

  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1, 0x55);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100, // safeTip = 4900
    fetch: mockFetch,
  });

  // Query 4800..4950 (crosses safeTip = 4900)
  const result = await transport.blocks(4800, 4950);

  // Historical: 4800..4900 fetched from CDN (segment 4000 sliced)
  assert.equal(requestedCdnUrls.length, 1);
  assert.ok(requestedCdnUrls[0].includes("/compact-blocks/mainnet/4000.bin"));

  // Live tip: 4901..4950 queried directly from live transport
  assert.equal(liveCalls.length, 1);
  assert.deepEqual(liveCalls[0], { start: 4901, end: 4950 });

  const expectedHist = makeFramedBlocks(4800, 101, 0x44);
  const expectedLive = makeFramedBlocks(4901, 50, 0x55);
  const expected = new Uint8Array(expectedHist.length + expectedLive.length);
  expected.set(expectedHist, 0);
  expected.set(expectedLive, expectedHist.length);

  assert.deepEqual(result, expected);
});

test("cdnSegmentTransport delegates all metadata and spending methods to live transport", async () => {
  let submitCalled = false;
  let treeStateCalled = false;
  let subtreeRootsCalled = false;
  let txCalled = false;
  let infoCalled = false;

  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 1234,
    blocks: async () => new Uint8Array(0),
    submit: async (hex) => {
      submitCalled = true;
      return `txid-${hex}`;
    },
    treeState: async (h) => {
      treeStateCalled = true;
      return { height: h, hash: "abc" };
    },
    subtreeRoots: async () => {
      subtreeRootsCalled = true;
      return [];
    },
    tx: async (txid) => {
      txCalled = true;
      return `raw-${txid}`;
    },
    info: async () => {
      infoCalled = true;
      return { chain: "main", blockHeight: 1234 };
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
  });

  assert.equal(await transport.tip(), 1234);
  assert.equal(await transport.submit?.("deadbeef"), "txid-deadbeef");
  assert.ok(submitCalled);

  await transport.treeState?.(500);
  assert.ok(treeStateCalled);

  await transport.subtreeRoots?.("orchard");
  assert.ok(subtreeRootsCalled);

  await transport.tx?.("1234");
  assert.ok(txCalled);

  const info = await transport.info?.();
  assert.equal(info?.chain, "main");
  assert.ok(infoCalled);
});

test("cdnSegmentTransport retries on transient 503 and succeeds without live fallback", async () => {
  const segment1000 = makeFramedBlocks(1000, 1000);
  let fetchAttempts = 0;

  const mockFetch: typeof fetch = async () => {
    fetchAttempts++;
    if (fetchAttempts === 1) {
      return new Response(null, { status: 503 });
    }
    return new Response(segment1000, { status: 200 });
  };

  let liveCalled = false;
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async () => {
      liveCalled = true;
      return new Uint8Array(0);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    fetch: mockFetch,
    segmentBlocks: 1000,
    reorgDepth: 100,
  });

  const res = await transport.blocks(1000, 1999);
  assert.equal(fetchAttempts, 2);
  assert.equal(liveCalled, false);
  assert.deepEqual(res, segment1000);
});

test("cdnSegmentTransport does not query CDN for segments extending beyond chain tip", async () => {
  const requestedCdnUrls: string[] = [];
  const mockFetch: typeof fetch = async (input) => {
    requestedCdnUrls.push(String(input));
    return new Response(null, { status: 503 });
  };

  const liveCalls: Array<{ start: number; end: number }> = [];
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 3522050,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };

  const transport = cdnSegmentTransport({
    cdnUrl: "https://cloud.cofferwallet.com",
    network: "mainnet",
    live: mockLive,
    fetch: mockFetch,
    segmentBlocks: 1000,
    reorgDepth: 100,
  });

  // Query 3522000..3522020 (belongs to segment 3522000 which ends at 3522999 > tip 3522050)
  await transport.blocks(3522000, 3522020);

  assert.equal(requestedCdnUrls.length, 0, "Must not query CDN for segments extending beyond tip");
  assert.equal(liveCalls.length, 1);
  assert.deepEqual(liveCalls[0], { start: 3522000, end: 3522020 });
});

test("cdnSegmentTransport falls back when a 200 segment is short or torn", async () => {
  const liveCalls: Array<{ start: number; end: number }> = [];
  const oneFrame = makeFramedBlocks(1000, 1);
  let calls = 0;
  const mockFetch: typeof fetch = async () => {
    calls++;
    // Full-range request gets one frame. Partial request gets a torn frame.
    if (calls === 1) return new Response(oneFrame, { status: 200 });
    const torn = new Uint8Array(10);
    new DataView(torn.buffer).setUint32(0, 100, false);
    return new Response(torn, { status: 200 });
  };
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1, 0x11);
    },
  };
  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 1000,
    reorgDepth: 100,
    fetch: mockFetch,
  });

  const full = await transport.blocks(1000, 1999);
  assert.deepEqual(liveCalls, [{ start: 1000, end: 1999 }]);
  assert.deepEqual(full, makeFramedBlocks(1000, 1000, 0x11));

  liveCalls.length = 0;
  const partial = await transport.blocks(1002, 1004);
  assert.deepEqual(liveCalls, [{ start: 1002, end: 1004 }]);
  assert.deepEqual(partial, makeFramedBlocks(1002, 3, 0x11));
});

test("cdnSegmentTransport rejects an oversized CDN body before using it", async () => {
  let sent = 0;
  const chunk = new Uint8Array(8 * 1024 * 1024);
  const mockFetch: typeof fetch = async () => new Response(
    new ReadableStream({
      pull(controller) {
        if (sent > 64 * 1024 * 1024) throw new Error("read past the cap");
        controller.enqueue(chunk);
        sent += chunk.byteLength;
      },
    }),
    { status: 200 },
  );
  const liveCalls: Array<{ start: number; end: number }> = [];
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };
  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 4,
    reorgDepth: 0,
    fetch: mockFetch,
  });
  await transport.blocks(0, 1);
  // One extra chunk can be queued before cancel. The body must still stop.
  assert.ok(sent < 64 * 1024 * 1024, `read ${sent} bytes`);
  assert.deepEqual(liveCalls, [{ start: 0, end: 1 }]);
});

test("cdnSegmentTransport refuses a declared oversized content-length", async () => {
  const mockFetch: typeof fetch = async () => new Response(new Uint8Array(4), {
    status: 200,
    headers: { "content-length": String(33 * 1024 * 1024) },
  });
  const liveCalls: Array<{ start: number; end: number }> = [];
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 5000,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };
  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 4,
    reorgDepth: 0,
    fetch: mockFetch,
  });
  await transport.blocks(0, 1);
  assert.deepEqual(liveCalls, [{ start: 0, end: 1 }]);
});

test("cdnSegmentTransport falls back when a CDN body never ends", async () => {
  const mockFetch: typeof fetch = async (_input, init) => {
    const signal = init?.signal ?? undefined;
    const body = new ReadableStream({
      pull(controller) {
        if (signal?.aborted) controller.error(signal.reason);
      },
    });
    return new Response(body, { status: 200 });
  };
  const liveCalls: Array<{ start: number; end: number }> = [];
  const mockLive: BlockTransport = {
    kind: "mock-live",
    label: "Mock Live",
    tip: async () => 50,
    blocks: async (start, end) => {
      liveCalls.push({ start, end });
      return makeFramedBlocks(start, end - start + 1);
    },
  };
  const transport = cdnSegmentTransport({
    cdnUrl: "https://cdn.example.com",
    network: "mainnet",
    live: mockLive,
    segmentBlocks: 4,
    reorgDepth: 0,
    segmentTimeoutMs: 30,
    fetch: mockFetch,
  });
  const started = Date.now();
  await transport.blocks(0, 1);
  assert.ok(Date.now() - started < 2_000);
  assert.deepEqual(liveCalls, [{ start: 0, end: 1 }]);
});

