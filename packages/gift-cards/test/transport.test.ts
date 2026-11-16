import assert from 'node:assert/strict';
import test from 'node:test';
import { BroadcastRejection, type BlockTransport } from '@z-stack/sdk';
import { createGiftTransportProxy, describeGiftTransport, giftTransportFailure, runGiftTransportCall, type GiftTransportEvent } from '../src/transport-bridge';

test('gift worker reads and submits through exactly the captured transport with Rust txid context', async () => {
  const calls: string[] = [], txid = 'aa'.repeat(32);
  const selected: BlockTransport = { kind: 'private-cloud', label: 'Verified Private Cloud', maxBlocksPerRequest: 1000,
    tip: async () => { calls.push('tip'); return 55; }, blocks: async (start, end) => { calls.push(`blocks:${start}:${end}`); return new Uint8Array([1,2,3]); },
    submit: async (hex, _signal, context) => { assert.equal(hex, 'abcd'); assert.deepEqual(context, { txid }); calls.push('submit'); return txid; } };
  const host = new Map<number, AbortController>();
  const proxy = createGiftTransportProxy(describeGiftTransport(selected), event => {
    if (event.kind === 'transport-cancel') { host.get(event.requestId)?.abort(); return; }
    const controller = new AbortController(); host.set(event.requestId, controller);
    void runGiftTransportCall(selected, event.call, controller.signal).then(result => proxy.receive({ op: 'transport-result', requestId: event.requestId, ok: true, result }), error => proxy.receive({ op: 'transport-result', requestId: event.requestId, ...giftTransportFailure(error) }));
  });
  assert.equal(await proxy.transport.tip(), 55);
  assert.deepEqual(await proxy.transport.blocks(1, 2), new Uint8Array([1,2,3]));
  assert.equal(await proxy.transport.submit?.('abcd', undefined, { txid }), txid);
  await assert.rejects(proxy.transport.blocks(1, 1001));
  assert.equal(proxy.transport.tx, undefined);
  assert.equal(proxy.transport.transparentPir, undefined);
  assert.deepEqual(calls, ['tip', 'blocks:1:2', 'submit']);
  proxy.dispose(); await assert.rejects(proxy.transport.tip());
});

test('cancellation cannot publish a late transport result and explicit refusal preserves its SDK type', async () => {
  const events: GiftTransportEvent[] = [];
  const proxy = createGiftTransportProxy({ kind: 'gift-transport', transportKind: 'private-cloud', label: 'Private Cloud', methods: ['tip', 'blocks', 'submit'] }, event => events.push(event));
  const controller = new AbortController(), pending = proxy.transport.tip(controller.signal);
  const rejected = assert.rejects(pending, error => error instanceof Error && 'code' in error && error.code === 'closed');
  controller.abort(); await rejected;
  proxy.receive({ op: 'transport-result', requestId: 1, ok: true, result: { method: 'tip', value: 999 } });
  assert.deepEqual(events.map(event => event.kind), ['transport-call', 'transport-cancel']);
  const submitted = proxy.transport.submit!('abcd', undefined, { txid: 'bb'.repeat(32) });
  proxy.receive({ op: 'transport-result', requestId: 2, ...giftTransportFailure(new BroadcastRejection(-26, 'already known')) });
  await assert.rejects(submitted, error => error instanceof BroadcastRejection && error.code === -26 && error.reason === 'already known');
  proxy.dispose();
});
