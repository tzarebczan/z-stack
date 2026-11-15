import { z } from 'zod';
import {
  AtomicAmountSchema, SolanaAddressSchema, PreparedSolanaSwapSchema, SOLANA_CYPH,
  SOLANA_USDC, SOL_MINT, validateSignedSolanaSwap, solanaTransactionSignature, type PreparedSolanaSwap, type SolanaNonInclusionProof,
} from '@z-stack/solana';

export const RouteOwnerSchema = z.object({ profileId: z.string().min(1).max(128), walletFingerprint: z.string().min(1).max(128), solanaAddress: SolanaAddressSchema }).strict();
export type RouteOwner = z.infer<typeof RouteOwnerSchema>;
export const NearLegSchema = z.object({
  providerReference: z.string().min(1).max(256), sourceAssetId: z.string().min(1).max(256), sourceAmount: z.string().regex(/^[1-9][0-9]{0,38}$/).refine(value => BigInt(value) < (1n << 128n)),
  depositAddress: z.string().min(1).max(512), depositMemo: z.string().max(512).nullable(), refundAddress: z.string().min(1).max(512),
  destinationMint: z.enum([SOLANA_USDC.mint, SOL_MINT]), destinationOwner: SolanaAddressSchema,
  minimumDestinationAmount: AtomicAmountSchema, expiresAt: z.number().int().positive(),
}).strict();
export type NearLeg = z.infer<typeof NearLegSchema>;
export const IntermediateSettlementSchema = z.object({
  providerReference: z.string().min(1).max(256), signature: z.string().min(32).max(128),
  owner: SolanaAddressSchema, mint: z.enum([SOLANA_USDC.mint, SOL_MINT]),
  amount: AtomicAmountSchema.refine(value => BigInt(value) > 0n), slot: z.number().int().nonnegative(), finality: z.literal('finalized'),
}).strict();
export type IntermediateSettlement = z.infer<typeof IntermediateSettlementSchema>;
const nearObservationSchema = z.object({ status: z.enum(['pending', 'processing', 'success', 'refunded', 'failed']), checkedAt: z.number().int().positive() }).strict();
export type NearObservation = z.infer<typeof nearObservationSchema>;
const sourceFundingSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('external') }).strict(),
  z.object({ kind: z.literal('claimed'), claimId: z.string().min(1), transactionId: z.string().min(1).nullable() }).strict(),
]);
const stateSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('awaiting-source-funding') }).strict(),
  z.object({ kind: z.literal('source-funding-unknown'), claimId: z.string().min(1) }).strict(),
  z.object({ kind: z.literal('near-processing') }).strict(),
  z.object({ kind: z.literal('awaiting-settlement-proof') }).strict(),
  z.object({ kind: z.literal('intermediate-ready'), settlement: IntermediateSettlementSchema }).strict(),
  z.object({ kind: z.literal('awaiting-jupiter-review'), settlement: IntermediateSettlementSchema, prepared: PreparedSolanaSwapSchema }).strict(),
  z.object({ kind: z.literal('jupiter-submission-unknown'), settlement: IntermediateSettlementSchema, prepared: PreparedSolanaSwapSchema, signedTransaction: z.string().max(1800), signature: z.string().nullable() }).strict(),
  z.object({ kind: z.literal('jupiter-processing'), settlement: IntermediateSettlementSchema, prepared: PreparedSolanaSwapSchema, signedTransaction: z.string().max(1800), signature: z.string() }).strict(),
  z.object({ kind: z.literal('completed'), settlement: IntermediateSettlementSchema, signature: z.string(), outputAmount: AtomicAmountSchema }).strict(),
  z.object({ kind: z.literal('refunded') }).strict(),
  z.object({ kind: z.literal('source-failed') }).strict(),
  z.object({ kind: z.literal('intermediate-recoverable'), settlement: IntermediateSettlementSchema, reason: z.string().min(1).max(256) }).strict(),
]);
const jupiterAttemptSchema = z.discriminatedUnion('outcome', [
  z.object({ outcome: z.literal('failed'), prepared: PreparedSolanaSwapSchema, signedTransaction: z.string().max(1800), signature: z.string(), reason: z.string(), observedAt: z.number().int().positive() }).strict(),
  z.object({ outcome: z.literal('completed'), prepared: PreparedSolanaSwapSchema, signedTransaction: z.string().max(1800), signature: z.string(), outputAmount: AtomicAmountSchema, observedAt: z.number().int().positive() }).strict(),
]);
export const TwoLegRouteSchema = z.object({
  version: z.literal(1), id: z.string().min(1).max(128), owner: RouteOwnerSchema,
  atomicity: z.literal('recoverable-sequential'), targetMint: z.literal(SOLANA_CYPH.mint),
  near: NearLegSchema, sourceFunding: sourceFundingSchema, nearObservation: nearObservationSchema.nullable(),
  state: stateSchema, jupiterAttempts: z.array(jupiterAttemptSchema).max(20).default([]), revision: z.number().int().nonnegative(), createdAt: z.number().int().positive(), updatedAt: z.number().int().positive(),
}).strict().refine(route => route.owner.solanaAddress === route.near.destinationOwner, 'Wrong route destination owner');
export type TwoLegRoute = z.infer<typeof TwoLegRouteSchema>;
export type TwoLegRouteState = TwoLegRoute['state'];
export type RouteMutation<T> = { routes: TwoLegRoute[]; result: T };
/** Atomic read-modify-write across every tab/process sharing this owner namespace. Fail closed on persistence failure. */
export interface TwoLegRouteStore {
  transaction<T>(owner: RouteOwner, change: (routes: TwoLegRoute[]) => RouteMutation<T>): Promise<T>;
}
const ownerKey = (owner: RouteOwner) => JSON.stringify([owner.profileId, owner.walletFingerprint, owner.solanaAddress]);
function freezeRoute(route: TwoLegRoute): TwoLegRoute { return structuredClone(TwoLegRouteSchema.parse(route)); }
function parseRoutes(raw: unknown, owner: RouteOwner): TwoLegRoute[] {
  const routes = z.array(TwoLegRouteSchema).max(100).parse(raw);
  if (routes.some(route => ownerKey(route.owner) !== ownerKey(owner)) || new Set(routes.map(route => route.id)).size !== routes.length) throw new Error('Saved routes belong to another wallet or contain duplicate identities.');
  return routes;
}
export function createMemoryTwoLegRouteStore(): TwoLegRouteStore {
  const data = new Map<string, TwoLegRoute[]>(); let tail = Promise.resolve();
  return {
    transaction<T>(owner: RouteOwner, change: (routes: TwoLegRoute[]) => RouteMutation<T>): Promise<T> {
      const run = tail.then(() => {
        const key = ownerKey(RouteOwnerSchema.parse(owner));
        const result = change(structuredClone(data.get(key) ?? []));
        data.set(key, structuredClone(parseRoutes(result.routes, owner)));
        return result.result;
      });
      tail = run.then(() => undefined, () => undefined); return run;
    },
  };
}
export function createWebStorageTwoLegRouteStore(options: { storage: Pick<Storage, 'getItem' | 'setItem'>; locks: Pick<LockManager, 'request'>; namespace: string }): TwoLegRouteStore {
  return {
    async transaction<T>(owner: RouteOwner, change: (routes: TwoLegRoute[]) => RouteMutation<T>): Promise<T> {
      const key = options.namespace + ':' + ownerKey(RouteOwnerSchema.parse(owner));
      return options.locks.request(key, { mode: 'exclusive' }, () => {
        const raw = options.storage.getItem(key);
        if (raw && raw.length > 1024 * 1024) throw new Error('Saved routes are too large.');
        const mutation = change(parseRoutes(raw ? JSON.parse(raw) : [], owner));
        const serialized = JSON.stringify(parseRoutes(mutation.routes, owner));
        if (serialized.length > 1024 * 1024) throw new Error('Saved routes are too large.');
        options.storage.setItem(key, serialized); return mutation.result;
      });
    },
  };
}
export type JupiterSubmission = { signature: string };
export type JupiterObservation = { kind: 'pending' | 'unknown' } | { kind: 'not-included'; signature: string; transaction: string; proof: SolanaNonInclusionProof } | { kind: 'not-sent'; providerQuoteId: string; reviewedMessage: string; reason: string } | { kind: 'failed'; reason: string; signature: string; transaction: string; finality: 'finalized' } | { kind: 'completed'; signature: string; transaction: string; outputAmount: string; owner: string; mint: string; finality: 'finalized' };
export interface TwoLegRouteAdapters {
  /** Must verify actual finalized recipient balance changes from the destination transaction, not a provider status string. */
  verifyIntermediate(route: TwoLegRoute, evidence: unknown): Promise<IntermediateSettlement>;
  /** Submit or look up only this immutable reviewed transaction. A timeout is unknown. */
  submitJupiter(route: TwoLegRoute, prepared: PreparedSolanaSwap, signedTransaction: string): Promise<JupiterSubmission>;
  refreshNear(route: TwoLegRoute): Promise<NearObservation>;
  reconcileJupiter(route: TwoLegRoute): Promise<JupiterObservation>;
}
export function createTwoLegController(options: { owner: RouteOwner; store: TwoLegRouteStore; adapters: TwoLegRouteAdapters; now?: () => number; assertCurrent?: () => void }) {
  const owner = RouteOwnerSchema.parse(options.owner), now = options.now ?? Date.now;
  const check = () => options.assertCurrent?.();
  const mutate = <T>(id: string, run: (route: TwoLegRoute, routes: TwoLegRoute[]) => { route: TwoLegRoute; result: T }, current = true) => options.store.transaction(owner, routes => {
    if (current) check();
    const index = routes.findIndex(route => route.id === id), route = routes[index];
    if (!route) throw new Error('Route was not found for this wallet.');
    const next = run(freezeRoute(route), routes);
    const saved = freezeRoute({ ...next.route, revision: route.revision + 1, updatedAt: now() });
    return { routes: routes.map((value, i) => i === index ? saved : value), result: next.result };
  });
  const get = async (id: string) => options.store.transaction(owner, routes => { const route = routes.find(value => value.id === id); if (!route) throw new Error('Route was not found for this wallet.'); return { routes, result: freezeRoute(route) }; });
  const recordObservation = async (id: string, observation: NearObservation) => mutate(id, route => {
    const parsed = nearObservationSchema.parse(observation);
    if (route.nearObservation?.status === 'success' && parsed.status !== 'success') throw new Error('Provider observation contradicts saved completion.');
    const eligible = ['awaiting-source-funding', 'source-funding-unknown', 'near-processing', 'awaiting-settlement-proof'].includes(route.state.kind);
    if (!eligible) return { route, result: undefined };
    const state: TwoLegRouteState = parsed.status === 'success' ? { kind: 'awaiting-settlement-proof' } : parsed.status === 'refunded' ? { kind: 'refunded' } : parsed.status === 'failed' ? { kind: 'source-failed' } : route.state.kind === 'source-funding-unknown' ? route.state : { kind: 'near-processing' };
    return { route: { ...route, state, nearObservation: parsed }, result: undefined };
  }, false);
  const submitStored = async (route: TwoLegRoute) => {
    if (route.state.kind !== 'jupiter-submission-unknown') throw new Error('No saved submission to retry.');
    const state = route.state;
    // Reservation is already persisted. Late outcomes update this original owner even after UI account changes.
    const result = await options.adapters.submitJupiter(route, state.prepared, state.signedTransaction);
    if (!z.string().min(32).max(128).safeParse(result.signature).success) throw new Error('Invalid transaction acknowledgement.');
    await mutate(route.id, current => {
      if (current.state.kind !== 'jupiter-submission-unknown' || current.state.signedTransaction !== state.signedTransaction || current.state.prepared.message !== state.prepared.message) throw new Error('Saved submission changed.');
      return { route: { ...current, state: { ...current.state, kind: 'jupiter-processing', signature: result.signature } }, result: undefined };
    }, false);
    return get(route.id);
  };
  return Object.freeze({
    get,
    list: () => options.store.transaction(owner, routes => ({ routes, result: routes.map(freezeRoute) })),
    async create(input: { id: string; near: NearLeg }) {
      check(); const timestamp = now();
      const route = freezeRoute({ version: 1, id: input.id, owner, atomicity: 'recoverable-sequential', targetMint: SOLANA_CYPH.mint, near: input.near, sourceFunding: { kind: 'external' }, nearObservation: null, state: { kind: 'awaiting-source-funding' }, jupiterAttempts: [], revision: 0, createdAt: timestamp, updatedAt: timestamp });
      if (route.near.expiresAt <= timestamp || BigInt(route.near.minimumDestinationAmount) <= 0n) throw new Error('Expired or empty NEAR deposit instructions.');
      await options.store.transaction(owner, routes => {
        check(); if (routes.some(value => value.id === route.id || value.near.providerReference === route.near.providerReference || value.near.depositAddress === route.near.depositAddress && value.near.depositMemo === route.near.depositMemo)) throw new Error('Route or deposit identity is already saved.');
        return { routes: [...routes, route], result: undefined };
      }); return get(route.id);
    },
    async claimFunding(id: string) {
      const claimId = crypto.randomUUID();
      await mutate(id, route => {
        if (route.state.kind !== 'awaiting-source-funding' || route.near.expiresAt <= now()) throw new Error('Funding is already claimed or expired.');
        return { route: { ...route, sourceFunding: { kind: 'claimed', claimId, transactionId: null }, state: { kind: 'source-funding-unknown', claimId } }, result: undefined };
      }); return { claimId };
    },
    async recordFunding(id: string, claimId: string, transactionId: string) {
      if (!transactionId || transactionId.length > 256) throw new Error('Invalid funding transaction identity.');
      await mutate(id, route => {
        if (route.sourceFunding.kind !== 'claimed' || route.sourceFunding.claimId !== claimId || (route.sourceFunding.transactionId && route.sourceFunding.transactionId !== transactionId)) throw new Error('Funding claim changed.');
        return { route: { ...route, sourceFunding: { ...route.sourceFunding, transactionId }, state: route.state.kind === 'source-funding-unknown' ? { kind: 'near-processing' } : route.state }, result: undefined };
      }, false); return get(id);
    },
    recordNearObservation: recordObservation,
    async acceptIntermediate(id: string, evidence: unknown) {
      check(); const route = await get(id);
      if (route.state.kind !== 'awaiting-settlement-proof') throw new Error('NEAR has not completed this route.');
      const proof = IntermediateSettlementSchema.parse(await options.adapters.verifyIntermediate(route, evidence));
      check();
      if (proof.providerReference !== route.near.providerReference || proof.owner !== owner.solanaAddress || proof.mint !== route.near.destinationMint || BigInt(proof.amount) < BigInt(route.near.minimumDestinationAmount)) throw new Error('Intermediate settlement does not match this route.');
      await mutate(id, (current, routes) => {
        if (current.state.kind !== 'awaiting-settlement-proof') throw new Error('Intermediate settlement is already recorded.');
        if (routes.some(value => 'settlement' in value.state && value.state.settlement.signature === proof.signature)) throw new Error('Settlement is already assigned to a route.');
        return { route: { ...current, state: { kind: 'intermediate-ready', settlement: proof } }, result: undefined };
      }); return get(id);
    },
    async saveJupiterReview(id: string, value: PreparedSolanaSwap) {
      const prepared = PreparedSolanaSwapSchema.parse(value);
      await mutate(id, route => {
        if (route.state.kind !== 'intermediate-ready' && route.state.kind !== 'intermediate-recoverable' && route.state.kind !== 'awaiting-jupiter-review') throw new Error('Intermediate funds are not ready for a new review.');
        const { settlement } = route.state;
        if (prepared.review.owner !== owner.solanaAddress || prepared.review.inputMint !== settlement.mint || prepared.review.outputMint !== SOLANA_CYPH.mint || prepared.review.amount !== settlement.amount || prepared.review.expiresAt <= now()) throw new Error('Jupiter review does not match the settled intermediate funds.');
        validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false });
        return { route: { ...route, state: { kind: 'awaiting-jupiter-review', settlement, prepared } }, result: undefined };
      }); return get(id);
    },
    async executeJupiter(id: string, input: { accepted: true; reviewedMessage: string; signedTransaction: string }) {
      if (input.accepted !== true) throw new Error('Approve the exact second-leg review before signing.');
      await mutate(id, route => {
        if (route.state.kind !== 'awaiting-jupiter-review' || route.state.prepared.message !== input.reviewedMessage || route.state.prepared.review.expiresAt <= now()) throw new Error('Review changed, expired or already started.');
        validateSignedSolanaSwap(route.state.prepared, input.signedTransaction);
        return { route: { ...route, state: { ...route.state, kind: 'jupiter-submission-unknown', signedTransaction: input.signedTransaction, signature: null } }, result: undefined };
      });
      return submitStored(await get(id));
    },
    /** Explicit retry uses only the saved identical signed bytes. Never re-quotes, signs or starts a replacement. */
    async retryJupiter(id: string) { check(); return submitStored(await get(id)); },
    /** Reconciliation only. No method that signs or sends is called by resume. */
    async resume(id: string) {
      check(); const route = await get(id);
      if (['awaiting-source-funding', 'source-funding-unknown', 'near-processing', 'awaiting-settlement-proof'].includes(route.state.kind)) {
        await recordObservation(id, await options.adapters.refreshNear(route));
      } else if (route.state.kind === 'jupiter-submission-unknown' || route.state.kind === 'jupiter-processing') {
        const observation = await options.adapters.reconcileJupiter(route);
        await mutate(id, current => {
          if (current.state.kind !== 'jupiter-submission-unknown' && current.state.kind !== 'jupiter-processing') return { route: current, result: undefined };
          let state: TwoLegRouteState = current.state;
          if (observation.kind === 'completed' || observation.kind === 'failed' || observation.kind === 'not-included') {
            validateSignedSolanaSwap(current.state.prepared, observation.transaction, { requireOwnerSignature: true, requirePayerSignature: true });
            if ((observation.kind !== 'not-included' && observation.finality !== 'finalized') || solanaTransactionSignature(observation.transaction) !== observation.signature || (current.state.signature && current.state.signature !== observation.signature)) throw new Error('Finalized outcome belongs to a different transaction.');
          }
          if (observation.kind === 'completed') {
            if (observation.owner !== owner.solanaAddress || observation.mint !== SOLANA_CYPH.mint || observation.finality !== 'finalized' || BigInt(AtomicAmountSchema.parse(observation.outputAmount)) < BigInt(current.state.prepared.review.minimumOutputAmount) || (current.state.signature && current.state.signature !== observation.signature)) throw new Error('Second-leg settlement does not match the saved trade.');
            state = { kind: 'completed', settlement: current.state.settlement, signature: observation.signature, outputAmount: observation.outputAmount };
          } else if (observation.kind === 'not-included') {
            const proof = observation.proof;
            const verified = validateSignedSolanaSwap(current.state.prepared, observation.transaction, { requireOwnerSignature: true, requirePayerSignature: true });
            if (proof.kind !== 'finalized-block-signatures' || proof.signature !== observation.signature || proof.blockhash !== verified.message.recentBlockhash || proof.blockhashValid !== false || proof.fromSlot !== current.state.prepared.review.preparedAtSlot || !Number.isSafeInteger(proof.throughSlot) || proof.throughSlot < proof.fromSlot || proof.throughSlot - proof.fromSlot + 1 > 300 || !Number.isSafeInteger(proof.checkedBlocks) || proof.checkedBlocks < 1 || proof.checkedBlocks > 300 || proof.checkedBlocks > proof.throughSlot - proof.fromSlot + 1 || !Number.isSafeInteger(proof.finalizedBlockHeight) || proof.lastValidBlockHeight !== current.state.prepared.review.lastValidBlockHeight || proof.conservativeExpiryBlockHeight !== current.state.prepared.review.conservativeExpiryBlockHeight || proof.finalizedBlockHeight <= proof.conservativeExpiryBlockHeight || !SolanaAddressSchema.safeParse(proof.throughBlockhash).success) throw new Error('Non-inclusion coverage does not match this saved transaction.');
            state = { kind: 'intermediate-recoverable', settlement: current.state.settlement, reason: 'Complete finalized block history proves this signed transaction was not included before expiry.' };
          } else if (observation.kind === 'not-sent') {
            if (current.state.kind !== 'jupiter-submission-unknown' || current.state.signature !== null || !current.state.prepared.providerQuoteId || observation.providerQuoteId !== current.state.prepared.providerQuoteId || observation.reviewedMessage !== current.state.prepared.message) throw new Error('Cancellation does not match an unsigned, unsubmitted provider quote.');
            state = { kind: 'intermediate-recoverable', settlement: current.state.settlement, reason: observation.reason };
          } else if (observation.kind === 'failed') state = { kind: 'intermediate-recoverable', settlement: current.state.settlement, reason: observation.reason };
          const jupiterAttempts = observation.kind === 'completed' ? [...current.jupiterAttempts, { outcome: 'completed' as const, prepared: current.state.prepared, signedTransaction: observation.transaction, signature: observation.signature, outputAmount: observation.outputAmount, observedAt: now() }] : observation.kind === 'failed' ? [...current.jupiterAttempts, { outcome: 'failed' as const, prepared: current.state.prepared, signedTransaction: observation.transaction, signature: observation.signature, reason: observation.reason, observedAt: now() }] : current.jupiterAttempts;
          return { route: { ...current, state, jupiterAttempts }, result: undefined };
        }, false);
      }
      return get(id);
    },
  });
}
export type TwoLegController = ReturnType<typeof createTwoLegController>;
