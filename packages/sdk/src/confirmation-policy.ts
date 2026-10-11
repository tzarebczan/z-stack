import { formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/core";

/** A positive confirmation count that the Rust engine can represent exactly. */
export function minimumConfirmations(value: unknown): number | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1 || value > 0xffffffff)
    throw new WalletError("unknown", "minConfirmations must be an integer from 1 to 4294967295.");
  return value;
}

/** Add the public display fields to the engine's numeric Balance JSON. */
export function confirmationBalance(json: string): WalletSnapshot["balance"] {
  const value: unknown = JSON.parse(json);
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid engine balance");
  const number = (key: string, optional = false): number => {
    const amount: unknown = Reflect.get(value, key);
    if (optional && amount === undefined) return 0;
    if (typeof amount !== "number" || !Number.isSafeInteger(amount) || amount < 0 || amount > 2_100_000_000_000_000)
      throw new Error("invalid engine balance");
    return amount;
  };
  const saplingAvailable = number("saplingAvailable"), orchardAvailable = number("orchardAvailable"),
    ironwoodAvailable = number("ironwoodAvailable"), transparentAvailable = number("transparentAvailable"),
    totalAvailable = number("totalAvailable"), saplingPending = number("saplingPending", true),
    orchardPending = number("orchardPending", true), ironwoodPending = number("ironwoodPending", true),
    transparentPending = number("transparentPending", true), totalPending = number("totalPending", true);
  if (totalAvailable !== saplingAvailable + orchardAvailable + ironwoodAvailable + transparentAvailable ||
      totalPending !== saplingPending + orchardPending + ironwoodPending + transparentPending)
    throw new Error("invalid engine balance totals");
  const zec = (amount: number) => formatZatoshis(BigInt(amount));
  return { saplingAvailable, orchardAvailable, ironwoodAvailable, transparentAvailable, totalAvailable,
    saplingPending, orchardPending, ironwoodPending, transparentPending, totalPending,
    saplingZec: zec(saplingAvailable), orchardZec: zec(orchardAvailable), ironwoodZec: zec(ironwoodAvailable),
    transparentZec: zec(transparentAvailable), totalZec: zec(totalAvailable), pendingZec: zec(totalPending),
    orchardPendingZec: zec(orchardPending), transparentPendingZec: zec(transparentPending) };
}
