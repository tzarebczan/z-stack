import type { WalletSnapshot } from "@z-stack/core";
import { clearSavedSnapshot, readWalletGeneration, peekSavedSnapshot, readSavedSnapshot, saveWalletSnapshot } from "../../sdk/src/snapshot-storage";

const output = document.querySelector<HTMLPreElement>("#results")!;
const button = document.querySelector<HTMLButtonElement>("#run")!;
const preview: WalletSnapshot = {
  network: "regtest", server: "", birthdayHeight: 1, scannedHeight: 10000,
  unifiedAddress: "storage-benchmark-fixture", transparentAddress: null, zip321: "",
  balance: { saplingAvailable: 0, orchardAvailable: 0, ironwoodAvailable: 0,
    transparentAvailable: 0, totalAvailable: 0, saplingZec: "0", orchardZec: "0",
    ironwoodZec: "0", transparentZec: "0", totalZec: "0" },
};
async function timed(work: () => Promise<unknown>): Promise<number> {
  const start = performance.now();
  await work();
  return performance.now() - start;
}
function median(samples: number[]): number {
  return +[...samples].sort((a, b) => a - b)[Math.floor(samples.length / 2)].toFixed(3);
}
button.onclick = async () => {
  button.disabled = true;
  let ownsFixture = false;
  const originalPut = IDBObjectStore.prototype.put;
  const OriginalBlob = Blob;
  let blockingMs = 0;
  try {
    if (location.hostname !== "127.0.0.1" || location.port !== "15176") {
      throw new Error("Use isolated http://127.0.0.1:15176/test/storage-benchmark.html");
    }
    if (await readSavedSnapshot() || await peekSavedSnapshot(() => preview)) {
      throw new Error("Existing wallet data found; choose a fresh browser profile/origin. Nothing changed.");
    }
    ownsFixture = true;
    // Measure the JS-blocking portion separately from asynchronous IDB commit.
    IDBObjectStore.prototype.put = function(value: unknown, key?: IDBValidKey) {
      const before = performance.now();
      const request = originalPut.call(this, value, key);
      if (value instanceof Uint8Array || value instanceof OriginalBlob) blockingMs += performance.now() - before;
      return request;
    };
    window.Blob = new Proxy(OriginalBlob, {
      construct(target, args, newTarget) {
        const before = performance.now();
        const result = Reflect.construct(target, args, newTarget);
        blockingMs += performance.now() - before;
        return result;
      },
    });
    const generation = await readWalletGeneration();
    const results = [];
    for (const mib of [1, 16, 60]) {
      const bytes = new Uint8Array(mib * 1024 * 1024);
      // Touch all pages; IDB still serializes the full payload. No wallet secrets.
      bytes.fill(113);
      const save = [], peek = [], read = [], synchronousStorage = [];
      for (let i = 0; i < 7; i++) {
        blockingMs = 0;
        save.push(await timed(() => saveWalletSnapshot(bytes, preview, () => true, generation)));
        synchronousStorage.push(blockingMs);
        peek.push(await timed(async () => {
          const got = await peekSavedSnapshot(() => { throw new Error("unexpected legacy parse"); });
          if (got?.scannedHeight !== preview.scannedHeight) throw new Error("preview mismatch");
        }));
        read.push(await timed(async () => {
          const got = await readSavedSnapshot();
          if (got?.byteLength !== bytes.byteLength || got[0] !== 113 || got.at(-1) !== 113) {
            throw new Error("payload mismatch");
          }
        }));
      }
      results.push({ mib, samples: 7, saveMs: median(save), synchronousStorageMs: median(synchronousStorage),
        previewMs: median(peek), fullReadMs: median(read) });
      output.textContent = JSON.stringify({ status: "running", results }, null, 2);
    }
    await clearSavedSnapshot();
    if (await readSavedSnapshot() || await peekSavedSnapshot(() => preview)) throw new Error("cleanup failed");
    ownsFixture = false;
    output.textContent = JSON.stringify({ status: "passed", userAgent: navigator.userAgent, results }, null, 2);
  } catch (error) {
    output.textContent = JSON.stringify({ status: "failed", message: String(error) }, null, 2);
  } finally {
    IDBObjectStore.prototype.put = originalPut;
    window.Blob = OriginalBlob;
    if (ownsFixture) await clearSavedSnapshot();
    button.disabled = false;
  }
};
