/**
 * Unified-address receiver sets we actually generate / accept.
 *
 * ZODL v1: receive UA **including transparent**, auto-shield, spend shielded only.
 * `full` is that receive set. `orchard` is orchard-only. `shielded` is orchard+sapling
 * (no t). There is no transparent-only set — no first-class t-spend.
 *
 * Keep lockstep with `z_engine::keys::UaReceiverSet`.
 */

export type UaReceiver = "orchard" | "sapling" | "p2pkh";

export type UaReceiverSet = "full" | "orchard" | "shielded";

/** Default receive set: orchard + sapling + p2pkh (`AllAvailableKeys`). */
export const DEFAULT_UA_RECEIVER_SET: UaReceiverSet = "full";

export const UA_RECEIVERS: Record<UaReceiverSet, readonly UaReceiver[]> = {
  full: ["orchard", "sapling", "p2pkh"],
  orchard: ["orchard"],
  shielded: ["orchard", "sapling"],
};

export function uaReceivers(set: UaReceiverSet): readonly UaReceiver[] {
  return UA_RECEIVERS[set];
}

export function isUaReceiverSet(s: string): s is UaReceiverSet {
  return s === "full" || s === "orchard" || s === "shielded";
}

/** `"full"` / `"all"` → full; `"orchard"`; `"shielded"`. */
export function parseUaReceiverSet(s: string): UaReceiverSet {
  const t = s.trim().toLowerCase();
  if (t === "full" || t === "all") return "full";
  if (t === "orchard") return "orchard";
  if (t === "shielded") return "shielded";
  throw new Error(`unknown UA receiver set: ${s}`);
}

export function classifyUaReceiverSet(receivers: readonly UaReceiver[]): UaReceiverSet | null {
  const set = new Set(receivers);
  const orchard = set.has("orchard");
  const sapling = set.has("sapling");
  const p2pkh = set.has("p2pkh");
  if (orchard && sapling && p2pkh) return "full";
  if (orchard && sapling && !p2pkh) return "shielded";
  if (orchard && !sapling && !p2pkh) return "orchard";
  return null;
}

/** ZODL receive-incl-t is `full` only. */
export function uaIncludesTransparent(set: UaReceiverSet): boolean {
  return set === "full";
}

/** Short plate / chip labels. `full` is ZODL receive-incl-t. */
export function uaReceiverSetLabel(set: UaReceiverSet): string {
  switch (set) {
    case "full":
      return "Full (+t receive)";
    case "orchard":
      return "Orchard";
    case "shielded":
      return "Shielded";
  }
}

export function inspectAddressSummary(a: InspectedAddress): string {
  const rec = a.receivers.length ? a.receivers.join(" + ") : "none";
  const set = a.receiverSet ? uaReceiverSetLabel(a.receiverSet) : a.kind;
  return `${a.kind} · ${set} · ${rec}`;
}

export type InspectedAddress = {
  network: "mainnet" | "testnet" | "regtest";
  kind: "unified" | "sapling" | "p2pkh" | "p2sh" | "tex" | "sprout";
  receivers: UaReceiver[];
  receiverSet: UaReceiverSet | null;
};
