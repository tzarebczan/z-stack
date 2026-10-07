const DB_NAME = "z-stack-storage-formats-benchmark";
const STORE = "fixtures";
const SAMPLES = 7;
const FORMATS = ["Uint8Array", "ArrayBuffer", "Blob"] as const;
type Format = typeof FORMATS[number];
type Payload = Uint8Array<ArrayBuffer> | ArrayBuffer | Blob;
type Sample = {
  constructionMs: number;
  synchronousPutMs: number;
  synchronousConstructionAndPutMs: number;
  constructionThroughCommitMs: number;
  idbReadMs: number;
  conversionMs: number;
  fullReadMs: number;
};
const button = document.querySelector<HTMLButtonElement>("#run")!;
const output = document.querySelector<HTMLPreElement>("#results")!;

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
function countRecords(db: IDBDatabase): Promise<number> {
  return new Promise((resolve, reject) => {
    const req = db.transaction(STORE, "readonly").objectStore(STORE).count();
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
function clearFixture(db: IDBDatabase): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).clear();
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error("Benchmark cleanup aborted"));
  });
}
function makePayload(format: Format, bytes: Uint8Array<ArrayBuffer>): Payload {
  if (format === "Blob") return new Blob([bytes], { type: "application/octet-stream" });
  if (format === "ArrayBuffer") return bytes.buffer;
  return bytes;
}

async function sample(db: IDBDatabase, format: Format, bytes: Uint8Array<ArrayBuffer>): Promise<Sample> {
  const before = performance.now();
  const payload = makePayload(format, bytes);
  const constructionMs = performance.now() - before;
  let synchronousPutMs = 0;
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error("Benchmark save aborted"));
    const store = tx.objectStore(STORE);
    const putStart = performance.now();
    store.put(payload, "snapshot:benchmark");
    synchronousPutMs = performance.now() - putStart;
    store.put({ format, key: "snapshot:benchmark", byteLength: bytes.byteLength,
      preview: { network: "regtest", scannedHeight: 10000, unifiedAddress: "benchmark-fixture" } }, "default");
  });
  const constructionThroughCommitMs = performance.now() - before;
  const readStart = performance.now();
  // Complete the read transaction before starting any asynchronous Blob read,
  // exactly as a production reader must to avoid inactive transactions.
  const stored = await new Promise<Payload>((resolve, reject) => {
    const tx = db.transaction(STORE, "readonly");
    let result: Payload;
    tx.oncomplete = () => resolve(result);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error("Benchmark read aborted"));
    const req = tx.objectStore(STORE).get("snapshot:benchmark");
    req.onsuccess = () => { result = req.result as Payload; };
  });
  const idbReadMs = performance.now() - readStart;
  const conversionStart = performance.now();
  const restored = stored instanceof Blob ? new Uint8Array(await stored.arrayBuffer())
    : stored instanceof Uint8Array ? stored : new Uint8Array(stored);
  const conversionMs = performance.now() - conversionStart;
  const fullReadMs = performance.now() - readStart;
  if (restored.byteLength !== bytes.byteLength || restored[0] !== 113
      || restored[Math.floor(bytes.byteLength / 2)] !== 113 || restored.at(-1) !== 113) {
    throw new Error(`${format} payload verification failed`);
  }
  return { constructionMs, synchronousPutMs, synchronousConstructionAndPutMs: constructionMs + synchronousPutMs,
    constructionThroughCommitMs, idbReadMs, conversionMs, fullReadMs };
}
function summarize(samples: Sample[]) {
  const median = (values: number[]) => +[...values].sort((a, b) => a - b)[Math.floor(values.length / 2)].toFixed(3);
  const fields = Object.keys(samples[0]) as Array<keyof Sample>;
  return Object.fromEntries(fields.map((field) => [field, median(samples.map((value) => value[field]))]));
}

button.onclick = async () => {
  button.disabled = true;
  let db: IDBDatabase | null = null;
  let ownsFixture = false;
  const results: unknown[] = [];
  try {
    if (location.hostname !== "127.0.0.1" || location.port !== "15178") {
      throw new Error("Use isolated http://127.0.0.1:15178/test/storage-formats.html");
    }
    db = await openDb();
    if (await countRecords(db)) throw new Error("Benchmark database contains existing records; nothing changed.");
    ownsFixture = true;
    for (const mib of [16, 60]) {
      // Common source allocation/touching is intentionally excluded from all
      // format timings. The production serializer already owns these bytes.
      const bytes = new Uint8Array(mib * 1024 * 1024).fill(113);
      const samples: Record<Format, Sample[]> = { Uint8Array: [], ArrayBuffer: [], Blob: [] };
      for (let round = 0; round < SAMPLES; round++) {
        // Rotate ordering so a single format does not always receive cold or
        // recently warmed storage, and yield for rendering between samples.
        for (let offset = 0; offset < FORMATS.length; offset++) {
          const format = FORMATS[(round + offset) % FORMATS.length];
          samples[format].push(await sample(db, format, bytes));
          output.textContent = JSON.stringify({ status: "running", mib, round: round + 1, format, results }, null, 2);
          await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
        }
      }
      for (const format of FORMATS) results.push({ mib, format, samples: SAMPLES, medians: summarize(samples[format]),
        maxSynchronousConstructionAndPutMs: +Math.max(...samples[format].map((s) => s.synchronousConstructionAndPutMs)).toFixed(3) });
    }
    await clearFixture(db);
    if (await countRecords(db)) throw new Error("Benchmark cleanup verification failed");
    ownsFixture = false;
    output.textContent = JSON.stringify({ status: "passed", userAgent: navigator.userAgent,
      note: "Synthetic touched buffers; source allocation excluded, Blob construction and byte materialization included. Seven samples per format, rotating order.", results }, null, 2);
  } catch (error) {
    output.textContent = JSON.stringify({ status: "failed", message: String(error), results }, null, 2);
  } finally {
    if (db && ownsFixture) await clearFixture(db);
    db?.close();
    button.disabled = false;
  }
};
