import assert from "node:assert/strict";
import { test } from "node:test";
import { fetchSubtreeRoots, SUBTREE_ROOTS_PAGE } from "../src/subtree-roots.ts";

const root = (i: number) => ({ completingHeight: 1000 + i, rootHash: `r${i}` });

/** Server holding complete-shard roots `0..count`, like Zaino behind the pipe. */
function server(count: number, opts: { honorsLimit?: boolean; failAt?: Map<number, number> } = {}) {
  const calls: number[] = [];
  let open = 0;
  let maxOpen = 0;
  const page = async (at: number) => {
    calls.push(at);
    open++;
    maxOpen = Math.max(maxOpen, open);
    await new Promise((r) => setTimeout(r, 1));
    open--;
    const failures = opts.failAt?.get(at) ?? 0;
    if (failures > 0) {
      opts.failAt!.set(at, failures - 1);
      throw new Error("503 light server unavailable");
    }
    const end = opts.honorsLimit === false ? count : Math.min(count, at + SUBTREE_ROOTS_PAGE);
    return Array.from({ length: Math.max(0, end - at) }, (_, k) => root(at + k));
  };
  return { page, calls, maxOpen: () => maxOpen };
}

test("fetches every root from the start shard in pages, several in flight", async () => {
  const s = server(100);
  const roots = await fetchSubtreeRoots(s.page, 37);
  assert.deepEqual(roots, Array.from({ length: 63 }, (_, k) => root(37 + k)));
  assert.equal(s.calls[0], 37);
  assert.ok(s.maxOpen() > 1, "pages after the first overlap");
  assert.ok(s.calls.every((at) => at >= 37), "never asks for shards before the start");
});

test("a start at the tip shard costs one request", async () => {
  const s = server(40);
  assert.deepEqual(await fetchSubtreeRoots(s.page, 40), []);
  assert.deepEqual(s.calls, [40]);
});

test("a pipe that ignores maxEntries is asked once", async () => {
  const s = server(30, { honorsLimit: false });
  const roots = await fetchSubtreeRoots(s.page, 5);
  assert.equal(roots.length, 25);
  assert.deepEqual(s.calls, [5]);
});

test("a transient failure is retried; a persistent one keeps the prefix", async () => {
  const retried = server(20, { failAt: new Map([[8, 1]]) });
  assert.equal((await fetchSubtreeRoots(retried.page, 0)).length, 20);

  const broken = server(40, { failAt: new Map([[16, 2]]) });
  const roots = await fetchSubtreeRoots(broken.page, 0);
  assert.deepEqual(roots, Array.from({ length: 16 }, (_, k) => root(k)), "contiguous prefix only");
});

test("cancellation propagates instead of returning a partial list", async () => {
  const controller = new AbortController();
  const page = async () => {
    controller.abort();
    throw new DOMException("aborted", "AbortError");
  };
  await assert.rejects(fetchSubtreeRoots(page, 0, controller.signal), /abort/i);
});
