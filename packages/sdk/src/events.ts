/**
 * In-process event bus for {@link EngineClient}. Not a websocket.
 * Register with `on`, unsubscribe with its return value or `off`.
 */

import type { SyncStage } from "@z-stack/core";
import type { WalletSnapshot } from "@z-stack/core";
import type { WasmRuntime } from "./runtime";

export type WalletEventName = "sync" | "balance" | "broadcast" | "runtime";

export type SyncEvent = {
  stage: SyncStage;
  scanned?: number;
  downloaded?: number;
  tip?: number;
  percent?: number;
  notesFound?: number;
  spendsFound?: number;
  blocksPerSecond?: number;
  remainingSeconds?: number;
  remainingHuman?: string;
  message?: string;
  heading?: string;
};

export type BalanceEvent = {
  availableZat: number;
  pendingZat?: number;
  orchardAvailable?: number;
  transparentAvailable?: number;
  totalZec?: string;
};

export type WalletEventMap = {
  /** Browser scanner changes, including startup, readiness and fallback. */
  runtime: WasmRuntime;
  sync: SyncEvent;
  balance: BalanceEvent;
  /** WASM: emitted after reservation persistence, immediately before network submission. */
  broadcast: { kind: "send" | "shield"; txid?: string };
};

export type WalletEventHandler<K extends WalletEventName> = (payload: WalletEventMap[K]) => void;

export type EventBus = {
  clear(): void;
  on<K extends WalletEventName>(event: K, handler: WalletEventHandler<K>): () => void;
  off<K extends WalletEventName>(event: K, handler: WalletEventHandler<K>): void;
  emit<K extends WalletEventName>(event: K, payload: WalletEventMap[K]): void;
};

export function createEventBus(): EventBus {
  const listeners = new Map<WalletEventName, Set<(payload: never) => void>>();
  return {
    clear() { listeners.clear(); },
    on(event, handler) {
      let set = listeners.get(event);
      if (!set) {
        set = new Set();
        listeners.set(event, set);
      }
      set.add(handler as (payload: never) => void);
      return () => set!.delete(handler as (payload: never) => void);
    },
    off(event, handler) {
      listeners.get(event)?.delete(handler as (payload: never) => void);
    },
    emit(event, payload) {
      const set = listeners.get(event);
      if (!set) return;
      for (const handler of set) {
        try {
          handler(payload as never);
        } catch {
          console.warn("wallet event handler");
        }
      }
    },
  };
}

export function balanceEvent(w: WalletSnapshot): BalanceEvent | null {
  const b = w.balance;
  if (!b) return null;
  return {
    availableZat: b.totalAvailable,
    pendingZat: b.totalPending ?? 0,
    orchardAvailable: b.orchardAvailable,
    transparentAvailable: b.transparentAvailable,
    totalZec: b.totalZec,
  };
}
