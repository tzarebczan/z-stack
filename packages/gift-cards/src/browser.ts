import { GiftError, GIFT_CONFIRMATIONS } from "./protocol";
import type { GiftBackend, GiftSendOptions, GiftWallet } from "./service";
import type { BrowserGiftOptions, GiftWorkerReply, GiftWorkerRequest, WorkerConfiguration } from "./worker-messages";
export type { BrowserGiftOptions } from "./worker-messages";

/** One isolated JS realm and durable viewing database per active gift. No account/backend service. */
export function createBrowserGiftBackend(options: BrowserGiftOptions): GiftBackend {
  if (!options.namespace.trim() || !options.server.trim()) throw new GiftError("invalid_link");
  const { onProgress, ...configuration } = options;
  const config: WorkerConfiguration = { ...configuration,
    ...(options.wasmBasePath ? { wasmBasePath: new URL(options.wasmBasePath, globalThis.location.href).href } : {}) };
  let disposed = false;
  const sessions = new Set<GiftConnection>();
  const create = () => {
    if (disposed) throw new GiftError("closed");
    const session = new GiftConnection(new Worker(new URL("./gift.worker.ts", import.meta.url), { type: "module" }), onProgress,
      () => sessions.delete(session));
    sessions.add(session);
    return session;
  };
  return {
    async create(network) {
      if (network !== options.network) throw new GiftError("wrong_network");
      const session = create();
      try {
        const reply = await session.call({ id: session.next(), op: "create", config });
        if (reply.kind !== "account") throw new GiftError("conflict");
        return reply.value;
      } finally { await session.close(); }
    },
    async derive(card) {
      const session = create();
      try {
        const reply = await session.call({ id: session.next(), op: "derive", config, card });
        if (reply.kind !== "account") throw new GiftError("conflict");
        return reply.value;
      } finally { await session.close(); }
    },
    async open(card): Promise<GiftWallet> {
      const session = create();
      try {
        const reply = await session.call({ id: session.next(), op: "open", config, card });
        if (reply.kind !== "snapshot") throw new GiftError("conflict");
        return {
          async getWallet() {
            const r = await session.call({ id: session.next(), op: "snapshot" });
            if (r.kind !== "snapshot") throw new GiftError("conflict"); return r.value;
          },
          async sync() {
            const r = await session.call({ id: session.next(), op: "sync" });
            if (r.kind !== "snapshot") throw new GiftError("conflict"); return r.value;
          },
          async history() {
            const r = await session.call({ id: session.next(), op: "history" });
            if (r.kind !== "history") throw new GiftError("conflict"); return r.value;
          },
          async transaction(txid) {
            const r = await session.call({ id: session.next(), op: "transaction", txid });
            if (r.kind !== "transaction") throw new GiftError("conflict"); return r.value;
          },
          async estimateFee(to, amountZec) {
            const r = await session.call({ id: session.next(), op: "fee", to, amountZec });
            if (r.kind !== "fee") throw new GiftError("conflict"); return r.value;
          },
          async send(to, amountZec, sendOptions) {
            if (sendOptions.minConfirmations !== undefined && sendOptions.minConfirmations !== GIFT_CONFIRMATIONS) throw new GiftError("invalid_link");
            const r = await session.call({ id: session.next(), op: "send", to, amountZec, maxFeeZat: sendOptions.maxFeeZat }, sendOptions);
            if (r.kind !== "spend") throw new GiftError("conflict"); return r.value;
          },
          close: () => session.close(),
        };
      } catch (error) { await session.close(); throw error; }
    },
    async dispose() {
      disposed = true;
      await Promise.all([...sessions].map(session => session.close()));
    },
  };
}

type Pending = { resolve: (reply: GiftWorkerReply) => void; reject: (error: unknown) => void; beforeSubmit?: GiftSendOptions["beforeSubmit"] };
class GiftConnection {
  private sequence = 0;
  private stopped = false;
  private closing: Promise<void> | null = null;
  private abort = new AbortController();
  private pending = new Map<number, Pending>();
  private journals = new Set<Promise<void>>();
  constructor(private worker: Worker, onProgress: BrowserGiftOptions["onProgress"], private removed: () => void) {
    worker.onmessage = (event: MessageEvent<GiftWorkerReply>) => {
      const reply = event.data;
      if (reply.kind === "progress") { onProgress?.(reply); return; }
      const waiting = this.pending.get(reply.id);
      if (!waiting) return;
      if (reply.kind === "reserved") {
        const journal = Promise.resolve().then(() => { this.abort.signal.throwIfAborted(); return waiting.beforeSubmit?.({ txid: reply.txid }, this.abort.signal); })
          // A successful atomic journal write remains committed when Close races it.
          .then(() => worker.postMessage({ id: reply.id, op: "reserved-ack", accepted: true }))
          .catch(() => worker.postMessage({ id: reply.id, op: "reserved-ack", accepted: false }));
        this.journals.add(journal);
        void journal.finally(() => this.journals.delete(journal));
        return;
      }
      this.pending.delete(reply.id);
      if (reply.kind === "error") {
        const code = reply.code === "wrong_network" || reply.code === "conflict" || reply.code === "storage" || reply.code === "closed" || reply.code === "review_changed" || reply.code === "broadcast_rejected" ? reply.code
          : reply.code === "insufficient_funds" ? "insufficient_funds"
          : reply.code === "broadcast_failed" || reply.code === "unresolved" ? "unresolved"
          : reply.code === "invalid_mnemonic" || reply.code === "invalid_birthday" || reply.code === "invalid_link" ? "invalid_link" : "connection";
        const error = new GiftError(code);
        waiting.reject(reply.txid ? Object.assign(error, { txid: reply.txid }) : error);
      } else waiting.resolve(reply);
    };
    const fail = () => {
      this.stopped = true;
      this.abort.abort();
      for (const request of this.pending.values()) request.reject(new GiftError("closed"));
      this.pending.clear(); worker.terminate(); this.removed();
    };
    worker.onerror = fail;
    worker.onmessageerror = fail;
  }
  next(): number { return ++this.sequence; }
  call(request: GiftWorkerRequest, options?: GiftSendOptions): Promise<GiftWorkerReply> {
    if (this.stopped && request.op !== "close") return Promise.reject(new GiftError("closed"));
    return new Promise((resolve, reject) => {
      this.pending.set(request.id, { resolve, reject, beforeSubmit: options?.beforeSubmit });
      this.worker.postMessage(request);
    });
  }
  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.stopped = true;
    this.abort.abort();
    this.closing = (async () => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        const closed = this.call({ id: this.next(), op: "close" });
        // Keep the worker alive until every in-flight commit has acknowledged its outcome.
        await Promise.allSettled([...this.journals]);
        await Promise.race([closed, new Promise<void>(resolve => { timer = setTimeout(resolve, 5000); })]);
      } finally {
        if (timer !== undefined) clearTimeout(timer);
        this.worker.terminate();
        for (const request of this.pending.values()) request.reject(new GiftError("closed"));
        this.pending.clear(); this.removed();
      }
    })();
    return this.closing;
  }
}
