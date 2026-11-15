import { BroadcastRejection, type BlockTransport, type SubmissionContext } from '@z-stack/sdk';
import { GiftError } from './protocol';
export type GiftTransportCall =
  | { method: 'tip' | 'info' | 'mempool' }
  | { method: 'blocks' | 'sharedMemos'; start: number; end: number }
  | { method: 'treeState'; height: number }
  | { method: 'subtreeRoots'; protocol: 'sapling' | 'orchard' | 'ironwood'; startIndex?: number; maxEntries?: number }
  | { method: 'tx'; txid: string }
  | { method: 'submit'; rawHex: string; context: SubmissionContext };
type Method = GiftTransportCall['method'];
export type GiftTransportDescriptor = { kind: 'gift-transport'; transportKind: string; label: string; maxBlocksPerRequest?: number; maxSharedMemoBlocksPerRequest?: number; methods: Method[] };
type Result<M extends Method> = Awaited<ReturnType<NonNullable<BlockTransport[M]>>>;
export type GiftTransportResult = { [M in Method]: { method: M; value: Result<M> } }[Method];
export type GiftTransportResponse = { op: 'transport-result'; requestId: number } & ({ ok: true; result: GiftTransportResult } | { ok: false; code: 'connection' | 'broadcast_rejected'; rejectionCode?: number; reason?: string });
export type GiftTransportEvent = { kind: 'transport-call'; requestId: number; call: GiftTransportCall } | { kind: 'transport-cancel'; requestId: number };
const methods: readonly Method[] = ['tip', 'info', 'blocks', 'sharedMemos', 'treeState', 'subtreeRoots', 'tx', 'mempool', 'submit'];
const height = (value: number) => Number.isSafeInteger(value) && value >= 0 && value <= 0xffffffff;
const hash = (value: string) => /^[a-f0-9]{64}$/i.test(value);
export function describeGiftTransport(transport: BlockTransport): GiftTransportDescriptor {
  if (typeof transport.tip !== 'function' || typeof transport.blocks !== 'function' || !transport.kind || !transport.label) throw new GiftError('connection');
  return { kind: 'gift-transport', transportKind: transport.kind, label: transport.label,
    ...(transport.maxBlocksPerRequest !== undefined ? { maxBlocksPerRequest: transport.maxBlocksPerRequest } : {}),
    ...(transport.maxSharedMemoBlocksPerRequest !== undefined ? { maxSharedMemoBlocksPerRequest: transport.maxSharedMemoBlocksPerRequest } : {}),
    methods: methods.filter(method => typeof transport[method] === 'function') };
}
/** Invoke only this explicitly selected transport. No URL, credentials or alternate provider crosses the worker boundary. */
export async function runGiftTransportCall(transport: BlockTransport, call: GiftTransportCall, signal: AbortSignal): Promise<GiftTransportResult> {
  signal.throwIfAborted();
  switch (call.method) {
    case 'tip': return { method: 'tip', value: await transport.tip(signal) };
    case 'info': if (transport.info) return { method: 'info', value: await transport.info(signal) }; break;
    case 'mempool': if (transport.mempool) return { method: 'mempool', value: await transport.mempool(signal) }; break;
    case 'blocks':
    case 'sharedMemos': {
      const limit = call.method === 'blocks' ? transport.maxBlocksPerRequest ?? 4000 : transport.maxSharedMemoBlocksPerRequest ?? 10;
      if (!height(call.start) || !height(call.end) || call.end < call.start || call.end - call.start + 1 > limit) throw new GiftError('connection');
      if (call.method === 'blocks') return { method: 'blocks', value: await transport.blocks(call.start, call.end, signal) };
      if (transport.sharedMemos) return { method: 'sharedMemos', value: await transport.sharedMemos(call.start, call.end, signal) }; break;
    }
    case 'treeState': if (height(call.height) && transport.treeState) return { method: 'treeState', value: await transport.treeState(call.height, signal) }; break;
    case 'subtreeRoots':
      if (['sapling', 'orchard', 'ironwood'].includes(call.protocol) && (call.startIndex === undefined || height(call.startIndex)) && (call.maxEntries === undefined || height(call.maxEntries)) && transport.subtreeRoots) return { method: 'subtreeRoots', value: await transport.subtreeRoots(call.protocol, call.startIndex, signal, call.maxEntries) }; break;
    case 'tx': if (hash(call.txid) && transport.tx) return { method: 'tx', value: await transport.tx(call.txid, signal) }; break;
    case 'submit':
      if (transport.submit && typeof call.rawHex === 'string' && call.rawHex.length > 0 && call.rawHex.length <= 4_000_000 && call.rawHex.length % 2 === 0 && /^[a-f0-9]+$/i.test(call.rawHex) && hash(call.context?.txid)) return { method: 'submit', value: await transport.submit(call.rawHex, signal, call.context) }; break;
  }
  throw new GiftError('connection');
}
export function giftTransportFailure(error: unknown): Omit<Extract<GiftTransportResponse, { ok: false }>, 'op' | 'requestId'> {
  return error instanceof BroadcastRejection ? { ok: false, code: 'broadcast_rejected', rejectionCode: error.code, reason: error.reason.slice(0, 1024) } : { ok: false, code: 'connection' };
}
/** One RPC client per gift worker, with cancellation independent of the wallet command queue. */
export function createGiftTransportProxy(descriptor: GiftTransportDescriptor, post: (event: GiftTransportEvent) => void) {
  let sequence = 0, stopped = false;
  const pending = new Map<number, { method: Method; resolve: (result: GiftTransportResult) => void; reject: (error: unknown) => void; cleanup: () => void }>();
  const invoke = <M extends Method>(call: GiftTransportCall & { method: M }, signal?: AbortSignal): Promise<Result<M>> => {
    if (stopped || !descriptor.methods.includes(call.method)) return Promise.reject(new GiftError('connection'));
    signal?.throwIfAborted(); const requestId = ++sequence;
    return new Promise<GiftTransportResult>((resolve, reject) => {
      const abort = () => { pending.delete(requestId); post({ kind: 'transport-cancel', requestId }); reject(new GiftError('closed')); };
      const cleanup = () => signal?.removeEventListener('abort', abort);
      pending.set(requestId, { method: call.method, resolve, reject, cleanup });
      signal?.addEventListener('abort', abort, { once: true });
      try { post({ kind: 'transport-call', requestId, call }); } catch (error) { pending.delete(requestId); cleanup(); reject(error); }
    }).then(result => { if (result.method !== call.method) throw new GiftError('connection'); return result.value as Result<M>; });
  };
  const transport: BlockTransport = { kind: descriptor.transportKind, label: descriptor.label,
    maxBlocksPerRequest: descriptor.maxBlocksPerRequest, maxSharedMemoBlocksPerRequest: descriptor.maxSharedMemoBlocksPerRequest,
    tip: signal => invoke({ method: 'tip' }, signal), blocks: (start, end, signal) => invoke({ method: 'blocks', start, end }, signal) };
  if (descriptor.methods.includes('info')) transport.info = signal => invoke({ method: 'info' }, signal);
  if (descriptor.methods.includes('treeState')) transport.treeState = (height, signal) => invoke({ method: 'treeState', height }, signal);
  if (descriptor.methods.includes('subtreeRoots')) transport.subtreeRoots = (protocol, startIndex, signal, maxEntries) => invoke({ method: 'subtreeRoots', protocol, startIndex, maxEntries }, signal);
  if (descriptor.methods.includes('tx')) transport.tx = (txid, signal) => invoke({ method: 'tx', txid }, signal);
  if (descriptor.methods.includes('mempool')) transport.mempool = signal => invoke({ method: 'mempool' }, signal);
  if (descriptor.methods.includes('sharedMemos')) transport.sharedMemos = (start, end, signal) => invoke({ method: 'sharedMemos', start, end }, signal);
  if (descriptor.methods.includes('submit')) transport.submit = (rawHex, signal, context) => {
    if (!context || !hash(context.txid)) return Promise.reject(new GiftError('connection'));
    return invoke({ method: 'submit', rawHex, context }, signal);
  };
  return {
    transport,
    receive(reply: GiftTransportResponse) {
      const waiting = pending.get(reply.requestId); if (!waiting) return;
      pending.delete(reply.requestId); waiting.cleanup();
      if (reply.ok) { if (reply.result.method !== waiting.method) waiting.reject(new GiftError('connection')); else waiting.resolve(reply.result); }
      else waiting.reject(reply.code === 'broadcast_rejected' && typeof reply.rejectionCode === 'number' ? new BroadcastRejection(reply.rejectionCode, reply.reason ?? '') : new GiftError('connection'));
    },
    dispose() { stopped = true; for (const [requestId, waiting] of pending) { waiting.cleanup(); waiting.reject(new GiftError('closed')); post({ kind: 'transport-cancel', requestId }); } pending.clear(); },
  };
}
