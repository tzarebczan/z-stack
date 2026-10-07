/** One `GetSubtreeRoots` page: at most {@link SUBTREE_ROOTS_PAGE} roots from shard `at`. */
export type SubtreeRootPage = (at: number) => Promise<Array<{ completingHeight: number; rootHash: string }>>;

/** Zaino spends up to a second per subtree root; small pages keep each request short. */
export const SUBTREE_ROOTS_PAGE = 8;
const SUBTREE_ROOTS_AHEAD = 4;
const RETRY_MS = 500;

/**
 * Contiguous roots from shard `start`: first one page, then, once the server
 * shows it honors the page limit, several pages in flight. Stops at the first
 * short page, since the open tip shard has no root. A page that fails twice
 * keeps the prefix already fetched; shards past it are hashed from leaves.
 */
export async function fetchSubtreeRoots(
  page: SubtreeRootPage,
  start: number,
  signal?: AbortSignal,
): Promise<Array<{ completingHeight: number; rootHash: string }>> {
  const fetchPage = async (at: number) => {
    try {
      return await page(at);
    } catch {
      signal?.throwIfAborted();
      await new Promise((resolve) => setTimeout(resolve, RETRY_MS));
      signal?.throwIfAborted();
      return page(at);
    }
  };
  const roots: Array<{ completingHeight: number; rootHash: string }> = [];
  const inflight: Array<ReturnType<typeof fetchPage>> = [];
  let next = start;
  const launch = () => {
    const pending = fetchPage(next);
    pending.catch(() => undefined);
    inflight.push(pending);
    next += SUBTREE_ROOTS_PAGE;
  };
  launch();
  while (inflight.length) {
    let got: Array<{ completingHeight: number; rootHash: string }>;
    try {
      got = await inflight.shift()!;
    } catch {
      signal?.throwIfAborted();
      break;
    }
    roots.push(...got);
    // A pipe without maxEntries answers the first page with every root.
    if (got.length !== SUBTREE_ROOTS_PAGE) break;
    while (inflight.length < SUBTREE_ROOTS_AHEAD) launch();
  }
  return roots;
}
