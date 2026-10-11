import { createWallet, indexedDbWalletStorage, WalletError, deriveAccount, type Wallet } from "@z-stack/sdk";
import { initializeGiftKeys, createGiftAccount, deriveGiftAccount, giftMnemonicFromEntropy } from "@z-stack/sdk/gift-keys";
import { GiftError, validateGiftCard } from "./protocol";
import { giftIdentity } from "./service";
import type { GiftWorkerReply, GiftWorkerRequest, WorkerConfiguration } from "./worker-messages";

const port = self as unknown as {
  postMessage(reply: GiftWorkerReply): void;
  onmessage: ((event: MessageEvent<GiftWorkerRequest>) => void) | null;
};
let wallet: Wallet | null = null;
let giftWords: string | null = null;
let releaseLease: (() => void) | null = null;
let lease: Promise<unknown> | null = null;
let stopped = false;
let queue: Promise<void> = Promise.resolve();
const receipts = new Map<number, { resolve: () => void; reject: () => void }>();
const post = (reply: GiftWorkerReply) => port.postMessage(reply);
function requireWallet(): Wallet {
  if (stopped || !wallet) throw new GiftError("closed");
  return wallet;
}
async function initialize(config: WorkerConfiguration): Promise<void> {
  await initializeGiftKeys({ network: config.network, preferMulticore: false, threads: 1,
    prewarmProveWorker: false, prewarmProvingKey: false, wasmBasePath: config.wasmBasePath,
    regtestNu63Height: config.regtestNu63Height, regtestNu7Height: config.regtestNu7Height });
  if (stopped) throw new GiftError("closed");
}
async function close(): Promise<void> {
  stopped = true;
  for (const receipt of receipts.values()) receipt.reject();
  receipts.clear();
  const active = wallet;
  wallet = null;
  giftWords = null;
  try { active?.lock(); await active?.close(); }
  finally { releaseLease?.(); releaseLease = null; await lease; lease = null; }
}
async function handle(request: GiftWorkerRequest): Promise<void> {
  if (!Number.isSafeInteger(request.id) || request.id < 1) throw new GiftError("invalid_link");
  if (stopped && request.op !== "close") throw new GiftError("closed");
  switch (request.op) {
    case "create": {
      await initialize(request.config);
      post({ kind: "account", id: request.id, value: createGiftAccount(request.config.network) });
      return;
    }
    case "derive": {
      validateGiftCard(request.card);
      if (request.card.network !== request.config.network) throw new GiftError("wrong_network");
      await initialize(request.config);
      post({ kind: "account", id: request.id, value: deriveGiftAccount(request.card.entropy, request.config.network) });
      return;
    }
    case "open": {
      if (wallet) throw new GiftError("conflict");
      validateGiftCard(request.card);
      if (request.card.network !== request.config.network) throw new GiftError("wrong_network");
      await initialize(request.config);
      const account = deriveGiftAccount(request.card.entropy, request.config.network);
      const name = `${request.config.namespace}:${giftIdentity(request.card.network, account.fingerprint)}`;
      if (!navigator.locks) throw new GiftError("storage");
      const held = new Promise<void>(resolve => { releaseLease = resolve; });
      await new Promise<void>((resolve, reject) => {
        lease = navigator.locks.request(`z-stack:gift-wallet:${name}`, { ifAvailable: true }, async lock => {
          if (!lock) { reject(new GiftError("conflict")); return; }
          resolve();
          await held;
        }).catch(reject);
      });
      if (stopped) { releaseLease?.(); throw new GiftError("closed"); }
      const opened = await createWallet({ ...request.config, storage: indexedDbWalletStorage({ name }),
        autoShield: false, transparent: false, transparentScan: "off", memoFetch: "on-demand",
        unlockPolicy: "each-spend", autoSync: false, preferMulticore: false, threads: 1, prewarmProvingKey: false,
        verifyChainTip: true,
        onProgress: progress => post({ kind: "progress", stage: progress.stage, percent: progress.percent }) });
      if (stopped) { await opened.close(); throw new GiftError("closed"); }
      wallet = opened;
      giftWords = giftMnemonicFromEntropy(request.card.entropy);
      const saved = await opened.load();
      requireWallet();
      if (saved) {
        const derived = deriveAccount(giftWords, request.card.network, 0);
        if (saved.network !== request.card.network || saved.ufvk !== derived.ufvk || saved.birthdayHeight !== request.card.birthdayHeight) throw new GiftError("conflict");
      } else await wallet.restore(giftWords, { birthday: request.card.birthdayHeight });
      requireWallet().lock();
      post({ kind: "snapshot", id: request.id, value: await requireWallet().getWallet() });
      return;
    }
    case "snapshot": post({ kind: "snapshot", id: request.id, value: await requireWallet().getWallet() }); return;
    case "sync": post({ kind: "snapshot", id: request.id, value: await requireWallet().sync() }); return;
    case "history": post({ kind: "history", id: request.id, value: await requireWallet().history() }); return;
    case "fee": {
      const inspection = await requireWallet().inspectAddress(request.to);
      if (inspection.network !== (await requireWallet().getWallet()).network || !inspection.receivers?.includes("orchard")) throw new GiftError("invalid_link");
      post({ kind: "fee", id: request.id, value: await requireWallet().estimateFee(request.to, request.amountZec) });
      return;
    }
    case "send": {
      const active = requireWallet();
      if (!giftWords) throw new GiftError("closed");
      try {
        await active.unlock(giftWords);
        const value = await active.send(request.to, request.amountZec, undefined, { maxFeeZat: request.maxFeeZat,
          beforeSubmit: async ({ txid }, signal) => {
            await new Promise<void>((resolve, reject) => {
              const cancel = () => { receipts.delete(request.id); reject(new GiftError("closed")); };
              signal.addEventListener("abort", cancel, { once: true });
              receipts.set(request.id, {
                resolve: () => { signal.removeEventListener("abort", cancel); resolve(); },
                reject: () => { signal.removeEventListener("abort", cancel); reject(new GiftError("storage")); },
              });
              post({ kind: "reserved", id: request.id, txid });
            });
            signal.throwIfAborted();
          } });
        post({ kind: "spend", id: request.id, value: { txid: value.txid, txids: value.txids } });
      } finally { active.lock(); }
      return;
    }
    case "close": await close(); post({ kind: "done", id: request.id }); return;
    case "reserved-ack": return;
  }
}
function fail(id: number, error: unknown) {
  let code = error instanceof GiftError ? error.code : error instanceof WalletError ? error.code : "connection";
  if (error instanceof Error && error.message.includes("approved maximum")) code = "review_changed";
  const txid = error instanceof WalletError ? error.txid : undefined;
  post({ kind: "error", id, code, ...(typeof txid === "string" && /^[0-9a-f]{64}$/i.test(txid) ? { txid } : {}) });
}
port.onmessage = event => {
  const request = event.data;
  if (request.op === "reserved-ack") {
    const receipt = receipts.get(request.id);
    receipts.delete(request.id);
    if (request.accepted) receipt?.resolve(); else receipt?.reject();
    return;
  }
  if (request.op === "close") {
    void close().then(() => post({ kind: "done", id: request.id })).catch(error => fail(request.id, error));
    return;
  }
  queue = queue.then(() => handle(request)).catch(error => fail(request.id, error));
};
