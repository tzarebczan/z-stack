import {
  NATIVE_BRIDGE_URL,
  createEngineClient,
  probeEngine,
  type EngineClient,
  type EngineHealth,
  type WalletSnapshot,
} from "@z-stack/sdk/lab";
import type { HistoryEntry } from "@z-stack/core";

const BRIDGE_STORAGE = "z-stack.web.bridge.v1";
const BRIDGE_TOKEN_STORAGE = "z-stack.web.bridge-token.v1";

export type { EngineHealth, HistoryEntry, WalletSnapshot };
export type BridgeBalance = WalletSnapshot["balance"];
export type BridgeWallet = WalletSnapshot;
export type Health = EngineHealth;

export function getBridgeUrl(): string {
  try {
    const u = localStorage.getItem(BRIDGE_STORAGE);
    if (u && u.trim()) return u.trim().replace(/\/$/, "");
  } catch {
    /* ignore */
  }
  return NATIVE_BRIDGE_URL;
}

export function setBridgeUrl(url: string): void {
  const u = url.trim().replace(/\/$/, "");
  localStorage.setItem(BRIDGE_STORAGE, u || NATIVE_BRIDGE_URL);
}

export function getBridgeToken(): string {
  try {
    const t = localStorage.getItem(BRIDGE_TOKEN_STORAGE);
    if (t?.trim()) return t.trim();
  } catch {
    /* ignore */
  }
  try {
    const u = new URL(getBridgeUrl());
    return u.searchParams.get("token")?.trim() ?? "";
  } catch {
    return "";
  }
}

export function setBridgeToken(token: string): void {
  const t = token.trim();
  if (t) localStorage.setItem(BRIDGE_TOKEN_STORAGE, t);
  else localStorage.removeItem(BRIDGE_TOKEN_STORAGE);
}

function client(): EngineClient {
  const token = getBridgeToken();
  return createEngineClient(getBridgeUrl(), { token: token || undefined });
}

export async function health(): Promise<Health | null> {
  const token = getBridgeToken();
  return probeEngine(getBridgeUrl(), { token: token || undefined });
}

export function getWallet(): Promise<BridgeWallet> {
  return client().getWallet();
}

export function syncWallet(): Promise<BridgeWallet> {
  return client().sync();
}

export function shieldWallet(): Promise<BridgeWallet> {
  return client().shield();
}

export function sendWallet(to: string, amountZec: string, memo?: string): Promise<BridgeWallet> {
  return client().send(to, amountZec, memo);
}

export function estimateFee(to: string, amountZec?: string, memo?: string) {
  return client().estimateFee(to, amountZec, memo);
}

export function maxSend(to?: string) {
  return client().maxSend(to);
}

export function inspectAddress(encoded: string) {
  return client().inspectAddress(encoded);
}

export function createWallet(network: string, birthday?: number): Promise<BridgeWallet> {
  return client().create(network, birthday);
}

export function restoreWallet(
  mnemonic: string,
  network: string,
  birthday?: number,
): Promise<BridgeWallet> {
  return client().restore(mnemonic, network, birthday);
}

export function history(limit = 50): Promise<HistoryEntry[]> {
  return client().history(limit);
}

export function nextAddress(): Promise<BridgeWallet> {
  return client().nextAddress();
}

export function resetScan(): Promise<BridgeWallet> {
  return client().resetScan();
}
