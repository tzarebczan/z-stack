import { GiftError, GIFT_CONFIRMATIONS, encodeGiftLink, giftFundingAmount, parseGiftLink, zecAmount,
  type GiftCard, type GiftNetwork } from "./protocol";

export type GiftAccount = { entropy: Uint8Array; address: string; fingerprint: string };
export type GiftTransaction = { txid: string; status: string; minedHeight: number | null; receivedZat: number; spentZat: number };
export type GiftWalletSnapshot = {
  network: string; unifiedAddress: string; birthdayHeight: number; scannedHeight?: number;
  balance: { saplingAvailable: number; orchardAvailable: number; ironwoodAvailable: number; totalPending?: number };
};
export type GiftSendOptions = {
  maxFeeZat: string;
  beforeSubmit?: (receipt: { txid: string }, signal: AbortSignal) => Promise<void>;
};
export type GiftSpendResult = { txid?: string; txids?: string[]; unconfirmed?: boolean };
export interface GiftWallet {
  getWallet(): Promise<GiftWalletSnapshot>;
  sync(): Promise<GiftWalletSnapshot>;
  estimateFee(to: string, amountZec: string): Promise<{ feeZat: number }>;
  send(to: string, amountZec: string, options: GiftSendOptions): Promise<GiftSpendResult>;
  history(): Promise<GiftTransaction[]>;
  close(): Promise<void>;
}
export interface GiftBackend {
  create(network: GiftNetwork): Promise<GiftAccount>;
  derive(card: GiftCard): Promise<GiftAccount>;
  open(card: GiftCard): Promise<GiftWallet>;
  dispose(): Promise<void>;
}
type RecordBase = {
  id: string; revision: number; role: "issued" | "received";
  link: string; address: string; amountZat: string;
};
export type GiftRecord = RecordBase & (
  | { phase: "draft"; fundingTxid?: never; claimTxid?: never; destination?: never; feeZat?: never }
  | { phase: "funding" | "funded"; fundingTxid?: string; claimTxid?: never; destination?: never; feeZat?: string }
  | { phase: "redeeming"; fundingTxid?: string; claimTxid?: string; destination: string; feeZat: string }
  | { phase: "redeemed"; fundingTxid?: string; claimTxid: string; destination: string; feeZat: string }
);
/** The application supplies encrypted, committed writes with revision compare-and-swap. */
export interface GiftStore {
  get(id: string): Promise<GiftRecord | null>;
  list(): Promise<GiftRecord[]>;
  save(record: GiftRecord, expectedRevision: number | null): Promise<void>;
  lock<T>(id: string, body: () => Promise<T>): Promise<T>;
}
export interface GiftSource {
  tip(): Promise<number>;
  estimateFee(to: string, amountZec: string): Promise<{ feeZat: number }>;
  /** Must enforce maxFeeZat on the actual proposal and await beforeSubmit before forwarding bytes. */
  send(to: string, amountZec: string, options: GiftSendOptions): Promise<GiftSpendResult>;
}
export type GiftInspection = {
  record: GiftRecord; card: GiftCard; snapshot: GiftWalletSnapshot;
  availableZat: bigint; confirmations: number;
};
export type GiftFundingReview = { record: GiftRecord; card: GiftCard; feeZat: bigint; totalZat: bigint };
export type GiftRedeemReview = { record: GiftRecord; card: GiftCard; destination: string; feeZat: bigint; amountZat: bigint };
export type GiftCardsOptions = {
  network: GiftNetwork; shareEndpoint: string; acceptedEndpoints?: readonly string[];
  backend: GiftBackend; store: GiftStore; source: GiftSource;
};

function integer(value: number): bigint {
  if (!Number.isSafeInteger(value) || value < 0 || value > 2_100_000_000_000_000) throw new GiftError("conflict");
  return BigInt(value);
}
function txidOf(value: unknown): string | undefined {
  if (typeof value !== "object" || value === null || !("txid" in value) || typeof value.txid !== "string") return undefined;
  return /^[0-9a-f]{64}$/i.test(value.txid) ? value.txid.toLowerCase() : undefined;
}
function receiptTxid(error: unknown): string | undefined {
  const direct = txidOf(error);
  if (direct) return direct;
  if (typeof error === "object" && error !== null && "details" in error) return txidOf(error.details);
  return undefined;
}
function plainRecipient(destination: string): void {
  if (!destination || destination !== destination.trim() || /[\s:?&#]/.test(destination)) throw new GiftError("invalid_link");
}

/** Amount/presentation/birthday never select a second spend namespace for the same account. */
export function giftIdentity(network: GiftNetwork, fingerprint: string): string {
  if (!/^[0-9a-f]{64}$/.test(fingerprint)) throw new GiftError("invalid_link");
  return `${network}:${fingerprint}`;
}

export function createGiftCards(options: GiftCardsOptions) {
  const { backend, source, store } = options;
  const endpoints = [options.shareEndpoint, ...(options.acceptedEndpoints ?? []), "https://link.vizor.cash/payment-links/open"];
  const parse = (link: string) => parseGiftLink(link, { endpoints, network: options.network });
  const get = async (id: string) => {
    const record = await store.get(id);
    if (!record) throw new GiftError("not_found");
    const card = parse(record.link);
    const account = await backend.derive(card);
    if (record.amountZat !== card.amountZat.toString() || giftIdentity(card.network, account.fingerprint) !== record.id || account.address !== record.address) throw new GiftError("conflict");
    return { record, card };
  };
  const commit = async (record: GiftRecord, next: GiftRecord) => {
    const updated = { ...next, revision: record.revision + 1 };
    await store.save(updated, record.revision);
    return updated;
  };
  const derive = async (card: GiftCard) => {
    const account = await backend.derive(card);
    return { account, id: giftIdentity(card.network, account.fingerprint) };
  };
  const inspectOpen = async (record: GiftRecord, card: GiftCard, wallet: GiftWallet): Promise<GiftInspection> => {
    const snapshot = await wallet.sync();
    // Expected address is the configured shielded profile; snapshots may use a full UA.
    const account = await backend.derive(card);
    if (giftIdentity(card.network, account.fingerprint) !== record.id || account.address !== record.address || snapshot.network !== card.network)
      throw new GiftError("conflict");
    const availableZat = integer(snapshot.balance.orchardAvailable) + integer(snapshot.balance.saplingAvailable) + integer(snapshot.balance.ironwoodAvailable);
    const history = await wallet.history();
    const scanned = snapshot.scannedHeight ?? 0;
    // Only proved inbound history contributes to maturity; a zero balance proves nothing.
    const inbound = history.filter(tx => tx.status === "mined" && tx.minedHeight !== null && tx.receivedZat > 0 && tx.spentZat === 0);
    const matured = inbound.filter(tx => scanned - (tx.minedHeight ?? scanned) + 1 >= GIFT_CONFIRMATIONS);
    const maturedZat = matured.reduce((sum, tx) => sum + integer(tx.receivedZat), 0n);
    const confirmations = maturedZat >= giftFundingAmount(card) ? GIFT_CONFIRMATIONS : Math.max(0,
      ...inbound.map(tx => Math.min(GIFT_CONFIRMATIONS - 1, scanned - (tx.minedHeight ?? scanned) + 1)));
    let updated = record;
    if ((record.phase === "funding" || record.phase === "draft") && availableZat >= giftFundingAmount(card) && inbound.length > 0)
      updated = await commit(record, { ...record, phase: "funded" });
    if (record.phase === "redeeming" && record.claimTxid && history.some(tx => tx.txid === record.claimTxid && tx.status === "mined" && tx.minedHeight !== null && tx.minedHeight <= scanned))
      updated = await commit(record, { ...record, phase: "redeemed", claimTxid: record.claimTxid });
    if (record.phase === "redeemed" && !history.some(tx => tx.txid === record.claimTxid && tx.status === "mined" && tx.minedHeight !== null && tx.minedHeight <= scanned))
      updated = await commit(record, { ...record, phase: "redeeming" });
    return { record: updated, card, snapshot, availableZat, confirmations };
  };
  const ready = async (record: GiftRecord, card: GiftCard, wallet: GiftWallet, destination: string) => {
    plainRecipient(destination);
    if (record.phase === "redeeming" || record.phase === "redeemed" || record.phase === "funding") throw new GiftError("unresolved");
    const inspection = await inspectOpen(record, card, wallet);
    if (inspection.confirmations < GIFT_CONFIRMATIONS) throw new GiftError("confirming");
    const feeZat = integer((await wallet.estimateFee(destination, zecAmount(card.amountZat))).feeZat);
    if (inspection.availableZat < card.amountZat + feeZat) throw new GiftError("insufficient_funds");
    return { ...inspection, feeZat };
  };

  return {
    async list(): Promise<GiftRecord[]> { return store.list(); },
    async prepare(input: { amountZat: bigint; label?: string; message?: string }): Promise<GiftFundingReview> {
      giftFundingAmount(input);
      const tip = await source.tip();
      if (!Number.isInteger(tip) || tip < 1 || tip > 0xffffffff) throw new GiftError("connection");
      const account = await backend.create(options.network);
      const card: GiftCard = { ...input, network: options.network, entropy: account.entropy, birthdayHeight: Math.max(1, tip - 10) };
      const id = giftIdentity(card.network, account.fingerprint);
      const record: GiftRecord = { id, revision: 0, role: "issued", link: encodeGiftLink(card, options.shareEndpoint),
        address: account.address, amountZat: card.amountZat.toString(), phase: "draft" };
      await store.save(record, null);
      const feeZat = integer((await source.estimateFee(record.address, zecAmount(giftFundingAmount(card)))).feeZat);
      return { record, card, feeZat, totalZat: giftFundingAmount(card) + feeZat };
    },
    async import(link: string): Promise<GiftRecord> {
      const card = parse(link);
      const { account, id } = await derive(card);
      return store.lock(id, async () => {
        const saved = await store.get(id);
        if (saved) {
          if (encodeGiftLink(parse(saved.link), options.shareEndpoint) !== encodeGiftLink(card, options.shareEndpoint)) throw new GiftError("conflict");
          return saved;
        }
        const record: GiftRecord = { id, revision: 0, role: "received", link: encodeGiftLink(card, options.shareEndpoint),
          address: account.address, amountZat: card.amountZat.toString(), phase: "draft" };
        await store.save(record, null);
        return record;
      });
    },
    async fund(id: string, review: { maxFeeZat: bigint }): Promise<GiftRecord> {
      return store.lock(id, async () => {
        let { record, card } = await get(id);
        if (record.phase !== "draft" || record.role !== "issued") throw new GiftError("unresolved");
        const fee = integer((await source.estimateFee(record.address, zecAmount(giftFundingAmount(card)))).feeZat);
        if (fee > review.maxFeeZat) throw new GiftError("review_changed");
        record = await commit(record, { ...record, phase: "funding", feeZat: fee.toString() });
        try {
          const result = await source.send(record.address, zecAmount(giftFundingAmount(card)), {
            maxFeeZat: review.maxFeeZat.toString(),
            beforeSubmit: async ({ txid }, signal) => {
              signal.throwIfAborted();
              if (record.phase !== "funding") throw new GiftError("conflict");
              record = await commit(record, { ...record, fundingTxid: txid });
              signal.throwIfAborted();
            },
          });
          const txid = txidOf(result);
          if (record.phase !== "funding") throw new GiftError("conflict");
          record = await commit(record, { ...record, ...(txid ? { fundingTxid: txid } : {}), phase: "funding" });
          return record;
        } catch (error) {
          const txid = receiptTxid(error);
          if (txid && record.phase === "funding" && record.fundingTxid !== txid) await commit(record, { ...record, fundingTxid: txid });
          throw new GiftError("unresolved");
        }
      });
    },
    async inspect(id: string): Promise<GiftInspection> {
      return store.lock(id, async () => {
        const { record, card } = await get(id);
        const wallet = await backend.open(card);
        try { return await inspectOpen(record, card, wallet); }
        finally { await wallet.close(); }
      });
    },
    async prepareRedeem(id: string, destination: string): Promise<GiftRedeemReview> {
      return store.lock(id, async () => {
        const { record, card } = await get(id);
        const wallet = await backend.open(card);
        try {
          const state = await ready(record, card, wallet, destination);
          return { record: state.record, card, destination, feeZat: state.feeZat, amountZat: card.amountZat };
        } finally { await wallet.close(); }
      });
    },
    async redeem(id: string, review: { destination: string; maxFeeZat: bigint }): Promise<GiftRecord> {
      return store.lock(id, async () => {
        let { record, card } = await get(id);
        const wallet = await backend.open(card);
        try {
          const state = await ready(record, card, wallet, review.destination);
          if (state.feeZat > review.maxFeeZat) throw new GiftError("review_changed");
          record = await commit(state.record, { ...state.record, phase: "redeeming", destination: review.destination, feeZat: state.feeZat.toString() });
          try {
            const result = await wallet.send(review.destination, zecAmount(card.amountZat), {
              maxFeeZat: review.maxFeeZat.toString(),
              beforeSubmit: async ({ txid }, signal) => {
                signal.throwIfAborted();
                if (record.phase !== "redeeming") throw new GiftError("conflict");
                record = await commit(record, { ...record, claimTxid: txid });
                signal.throwIfAborted();
              },
            });
            const txid = txidOf(result);
            if (record.phase !== "redeeming") throw new GiftError("conflict");
            return await commit(record, { ...record, ...(txid ? { claimTxid: txid } : {}) });
          } catch (error) {
            const txid = receiptTxid(error);
            if (txid && record.phase === "redeeming" && record.claimTxid !== txid) await commit(record, { ...record, claimTxid: txid });
            throw new GiftError("unresolved");
          }
        } finally { await wallet.close(); }
      });
    },
    async dispose(): Promise<void> { await backend.dispose(); },
  };
}
export type GiftCards = ReturnType<typeof createGiftCards>;
