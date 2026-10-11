import { GiftError, GIFT_CONFIRMATIONS, encodeGiftLink, giftFundingAmount, parseGiftLink, zecAmount,
  type GiftCard, type GiftNetwork } from "./protocol";

export type GiftAccount = { entropy: Uint8Array; address: string; fingerprint: string };
export type GiftTransaction = {
  txid: string; status: string; minedHeight: number | null; receivedZat: number; spentZat: number;
  saplingReceived?: number; orchardReceived?: number; ironwoodReceived?: number; transparentReceived?: number;
  saplingSpent?: number; orchardSpent?: number; ironwoodSpent?: number; transparentSpent?: number;
};
export type GiftWalletSnapshot = {
  network: string; unifiedAddress: string; birthdayHeight: number; scannedHeight?: number;
  /** Authoritative current unspent shielded value with at least six confirmations. */
  matureAvailableZat?: number;
  balance: { saplingAvailable: number; orchardAvailable: number; ironwoodAvailable: number; totalPending?: number };
};
export type GiftSendOptions = {
  maxFeeZat: string;
  /** Claim backends must enforce this count on every selected input; funding omits it. */
  minConfirmations?: number;
  beforeSubmit?: (receipt: { txid: string }, signal: AbortSignal) => Promise<void>;
};
export type GiftSpendResult = { txid?: string; txids?: string[]; unconfirmed?: boolean };
export interface GiftWallet {
  getWallet(): Promise<GiftWalletSnapshot>;
  sync(): Promise<GiftWalletSnapshot>;
  estimateFee(to: string, amountZec: string): Promise<{ feeZat: number }>;
  /** Enforce options.minConfirmations on the actual claim proposal, or reject before signing. */
  send(to: string, amountZec: string, options: GiftSendOptions): Promise<GiftSpendResult>;
  /** Complete history, or the most recent 500 unfiltered rows. A full page fails maturity closed. */
  history(): Promise<GiftTransaction[]>;
  /** Exact txid lookup over retained wallet history; null is authoritative absence. */
  transaction?(txid: string): Promise<GiftTransaction | null>;
  close(): Promise<void>;
}
export interface GiftBackend {
  create(network: GiftNetwork): Promise<GiftAccount>;
  derive(card: GiftCard): Promise<GiftAccount>;
  open(card: GiftCard): Promise<GiftWallet>;
  dispose(): Promise<void>;
}
export type GiftRejectedAttempt =
  | { kind: "funding"; txid: string; feeZat: string }
  | { kind: "redemption"; txid: string; feeZat: string; destination: string };
type RecordBase = {
  id: string; revision: number; role: "issued" | "received";
  link: string; address: string; amountZat: string;
  rejectedAttempts?: readonly GiftRejectedAttempt[];
};
export type GiftRecord = RecordBase & (
  | { phase: "draft"; fundingTxid?: never; claimTxid?: never; destination?: never; feeZat?: never }
  | { phase: "funding" | "funded"; fundingTxid?: string; claimTxid?: never; destination?: never; feeZat?: string }
  | { phase: "funding-rejected"; fundingTxid: string; claimTxid?: never; destination?: never; feeZat: string }
  | { phase: "redeeming"; fundingTxid?: string; claimTxid?: string; destination: string; feeZat: string }
  | { phase: "redeemed"; fundingTxid?: string; claimTxid: string; destination: string; feeZat: string }
  | { phase: "redemption-rejected"; fundingTxid?: string; claimTxid: string; destination: string; feeZat: string }
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
  availableZat: bigint; matureAvailableZat: bigint; confirmations: number;
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
function definitelyRejected(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "broadcast_rejected";
}
function ensureAttemptRoom(record: GiftRecord): void {
  if ((record.rejectedAttempts?.length ?? 0) >= 256) throw new GiftError("storage");
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
  const endpoints = [options.shareEndpoint, ...(options.acceptedEndpoints ?? [])];
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
    if (!Number.isInteger(scanned) || scanned < 0 || scanned > 0xffffffff) throw new GiftError("conflict");
    // Deduct every active input debit from mature outputs. This lower bound cannot
    // count an old, spent deposit as maturity for younger replacement funds.
    let matureCredits = 0n, debits = 0n;
    const inbound: GiftTransaction[] = [], youngConfirmations: number[] = [];
    const seenTxids = new Set<string>();
    for (const tx of history) {
      if (!/^[0-9a-f]{64}$/i.test(tx.txid) || seenTxids.has(tx.txid.toLowerCase())) throw new GiftError("conflict");
      seenTxids.add(tx.txid.toLowerCase());
      if (!["mined", "pending", "expired"].includes(tx.status)) throw new GiftError("conflict");
      const received = integer(tx.receivedZat);
      const spent = integer(tx.spentZat);
      const poolReceived = integer(tx.saplingReceived ?? 0) + integer(tx.orchardReceived ?? 0) + integer(tx.ironwoodReceived ?? 0);
      const poolSpent = integer(tx.saplingSpent ?? 0) + integer(tx.orchardSpent ?? 0) + integer(tx.ironwoodSpent ?? 0) + integer(tx.transparentSpent ?? 0);
      const transparentReceived = integer(tx.transparentReceived ?? 0);
      const shieldedReceived = poolReceived > 0n ? poolReceived : received > transparentReceived ? received - transparentReceived : 0n;
      if (tx.status !== "expired") debits += spent > poolSpent ? spent : poolSpent;
      if (tx.status !== "mined" || tx.minedHeight === null) continue;
      if (!Number.isInteger(tx.minedHeight) || tx.minedHeight < 1 || tx.minedHeight > scanned) throw new GiftError("conflict");
      if (shieldedReceived === 0n) continue;
      inbound.push(tx);
      const confirmations = scanned - tx.minedHeight + 1;
      if (confirmations >= GIFT_CONFIRMATIONS) matureCredits += shieldedReceived;
      else youngConfirmations.push(confirmations);
    }
    const matureRemainder = snapshot.matureAvailableZat !== undefined ? integer(snapshot.matureAvailableZat)
      : history.length >= 500 || matureCredits < debits ? 0n : matureCredits - debits;
    const matureAvailableZat = matureRemainder < availableZat ? matureRemainder : availableZat;
    const confirmations = matureAvailableZat >= giftFundingAmount(card) ? GIFT_CONFIRMATIONS : Math.max(0, ...youngConfirmations);
    let updated = record;
    if ((record.phase === "funding" || record.phase === "draft") && availableZat >= giftFundingAmount(card) && inbound.length > 0)
      updated = await commit(record, { ...record, phase: "funded" });
    const lookup = async (txid: string) => {
      let transaction = history.find(tx => tx.txid.toLowerCase() === txid.toLowerCase());
      let complete = history.length < 500;
      if (!transaction && wallet.transaction) {
        const exact = await wallet.transaction(txid);
        if (exact && (exact.txid.toLowerCase() !== txid.toLowerCase() || !["mined", "pending", "expired"].includes(exact.status))) throw new GiftError("conflict");
        transaction = exact ?? undefined;
        complete = true;
      }
      if (transaction?.status === "mined" && (transaction.minedHeight === null || !Number.isInteger(transaction.minedHeight) || transaction.minedHeight < 1 || transaction.minedHeight > scanned)) throw new GiftError("conflict");
      return { transaction, complete };
    };
    if (record.phase === "funding-rejected") {
      if (availableZat >= giftFundingAmount(card) && inbound.length > 0) updated = await commit(record, { ...record, phase: "funded" });
      else {
        const { transaction, complete } = await lookup(record.fundingTxid);
        if (transaction?.status === "mined") updated = await commit(record, { ...record, phase: "funded" });
        else if (transaction?.status === "pending") updated = await commit(record, { ...record, phase: "funding" });
        else if (!transaction && !complete) throw new GiftError("unresolved");
      }
    }
    if (record.phase === "redemption-rejected") {
      let observed: Extract<GiftRejectedAttempt, { kind: "redemption" }> | undefined;
      let observedStatus: "mined" | "pending" | undefined;
      let incomplete = false;
      for (const attempt of record.rejectedAttempts ?? []) {
        if (attempt.kind !== "redemption") continue;
        const { transaction, complete } = await lookup(attempt.txid);
        if (transaction?.status === "mined") { observed = attempt; observedStatus = "mined"; break; }
        if (transaction?.status === "pending" && !observed) { observed = attempt; observedStatus = "pending"; }
        if (!transaction && !complete) incomplete = true;
      }
      if (observed) updated = await commit(record, { ...record, phase: observedStatus === "mined" ? "redeemed" : "redeeming", claimTxid: observed.txid,
        destination: observed.destination, feeZat: observed.feeZat });
      else if (incomplete) throw new GiftError("unresolved");
    }
    if ((record.phase === "redeeming" || record.phase === "redeemed") && record.claimTxid) {
      const { transaction, complete } = await lookup(record.claimTxid);
      const mined = transaction?.status === "mined";
      if (record.phase === "redeeming" && mined) updated = await commit(record, { ...record, phase: "redeemed", claimTxid: record.claimTxid });
      if (record.phase === "redeemed" && !mined && (transaction || complete)) updated = await commit(record, { ...record, phase: "redeeming" });
    }
    return { record: updated, card, snapshot, availableZat, matureAvailableZat, confirmations };
  };
  const ready = async (record: GiftRecord, card: GiftCard, wallet: GiftWallet, destination: string) => {
    plainRecipient(destination);
    if (record.phase === "redeeming" || record.phase === "redeemed" || record.phase === "funding") throw new GiftError("unresolved");
    ensureAttemptRoom(record);
    const inspection = await inspectOpen(record, card, wallet);
    if (inspection.record.phase === "redeeming" || inspection.record.phase === "redeemed" || inspection.record.phase === "funding") throw new GiftError("unresolved");
    if (inspection.availableZat >= giftFundingAmount(card) && inspection.matureAvailableZat < giftFundingAmount(card)) throw new GiftError("confirming");
    const feeZat = integer((await wallet.estimateFee(destination, zecAmount(card.amountZat))).feeZat);
    if (inspection.availableZat < card.amountZat + feeZat) throw new GiftError("insufficient_funds");
    if (inspection.confirmations < GIFT_CONFIRMATIONS || inspection.matureAvailableZat < card.amountZat + feeZat) throw new GiftError("confirming");
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
        if ((record.phase !== "draft" && record.phase !== "funding-rejected") || record.role !== "issued") throw new GiftError("unresolved");
        ensureAttemptRoom(record);
        if (record.phase === "funding-rejected") {
          const wallet = await backend.open(card);
          try { record = (await inspectOpen(record, card, wallet)).record; }
          finally { await wallet.close(); }
          if (record.phase !== "funding-rejected") throw new GiftError("unresolved");
        }
        const fee = integer((await source.estimateFee(record.address, zecAmount(giftFundingAmount(card)))).feeZat);
        if (fee > review.maxFeeZat) throw new GiftError("review_changed");
        record = await commit(record, { ...record, phase: "funding", fundingTxid: undefined, feeZat: fee.toString() });
        try {
          const result = await source.send(record.address, zecAmount(giftFundingAmount(card)), {
            maxFeeZat: review.maxFeeZat.toString(),
            beforeSubmit: async ({ txid }, signal) => {
              signal.throwIfAborted();
              if (record.phase !== "funding") throw new GiftError("conflict");
              record = await commit(record, { ...record, fundingTxid: txid });
            },
          });
          const txid = txidOf(result);
          if (record.phase !== "funding") throw new GiftError("conflict");
          record = await commit(record, { ...record, ...(txid ? { fundingTxid: txid } : {}), phase: "funding" });
          return record;
        } catch (error) {
          const txid = receiptTxid(error);
          if (txid && definitelyRejected(error) && record.phase === "funding" && record.fundingTxid === txid && record.feeZat !== undefined) {
            await commit(record, { ...record, phase: "funding-rejected", fundingTxid: txid, feeZat: record.feeZat,
              rejectedAttempts: [...(record.rejectedAttempts ?? []), { kind: "funding", txid, feeZat: record.feeZat }] });
            throw new GiftError("broadcast_rejected");
          }
          if (txid && record.phase === "funding" && record.fundingTxid === undefined) await commit(record, { ...record, fundingTxid: txid });
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
          record = await commit(state.record, { ...state.record, phase: "redeeming", claimTxid: undefined, destination: review.destination, feeZat: state.feeZat.toString() });
          try {
            const result = await wallet.send(review.destination, zecAmount(card.amountZat), {
              minConfirmations: GIFT_CONFIRMATIONS,
              maxFeeZat: review.maxFeeZat.toString(),
              beforeSubmit: async ({ txid }, signal) => {
                signal.throwIfAborted();
                if (record.phase !== "redeeming") throw new GiftError("conflict");
                record = await commit(record, { ...record, claimTxid: txid });
              },
            });
            const txid = txidOf(result);
            if (record.phase !== "redeeming") throw new GiftError("conflict");
            return await commit(record, { ...record, ...(txid ? { claimTxid: txid } : {}) });
          } catch (error) {
            const txid = receiptTxid(error);
            if (txid && definitelyRejected(error) && record.phase === "redeeming" && record.claimTxid === txid) {
              await commit(record, { ...record, phase: "redemption-rejected", claimTxid: txid,
                rejectedAttempts: [...(record.rejectedAttempts ?? []), { kind: "redemption", txid, feeZat: record.feeZat, destination: record.destination }] });
              throw new GiftError("broadcast_rejected");
            }
            if (txid && record.phase === "redeeming" && record.claimTxid === undefined) await commit(record, { ...record, claimTxid: txid });
            throw new GiftError("unresolved");
          }
        } finally { await wallet.close(); }
      });
    },
    async dispose(): Promise<void> { await backend.dispose(); },
  };
}
export type GiftCards = ReturnType<typeof createGiftCards>;
