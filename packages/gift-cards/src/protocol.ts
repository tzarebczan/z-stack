/** Compact v3 positional format interoperable with Vizor mainnet/regtest cards.
 * Independently implemented from its documented Apache-2.0 protocol.
 * The fragment is a spending credential. Never log it or send it to a server. */
export type GiftNetwork = "mainnet" | "testnet" | "regtest";
export const GIFT_CLAIM_RESERVE_ZAT = 10_000n;
export const GIFT_CONFIRMATIONS = 6;
export const MAX_GIFT_LINK_LENGTH = 16_384;
const MAX_MONEY = 2_100_000_000_000_000n;
const wireNetwork = { mainnet: "main", testnet: "test", regtest: "regtest" };

export type GiftCard = {
  network: GiftNetwork;
  entropy: Uint8Array;
  birthdayHeight: number;
  amountZat: bigint;
  artworkId?: string;
  fiatUsd?: number;
  message?: string;
  label?: string;
};
export type GiftErrorCode = "invalid_link" | "wrong_network" | "invalid_amount" | "not_found" |
  "conflict" | "unresolved" | "confirming" | "insufficient_funds" | "review_changed" | "storage" | "closed" | "connection";
const messages: Record<GiftErrorCode, string> = {
  invalid_link: "That gift card link is invalid or unsupported.",
  wrong_network: "This gift card belongs to a different network.",
  invalid_amount: "Enter a valid gift amount.",
  not_found: "This gift card is not saved on this device.",
  conflict: "This gift card changed. Open the saved card and check its status.",
  unresolved: "A payment is still being checked. Check its status before trying again.",
  confirming: "Waiting for six block confirmations.",
  insufficient_funds: "This card does not yet have enough confirmed funds, including its fee.",
  review_changed: "The fee or wallet changed. Review this gift again.",
  storage: "The gift record could not be saved. Check its status before continuing.",
  closed: "Unlock the wallet to continue.",
  connection: "Could not reach the light server. Try checking this card again.",
};
export class GiftError extends Error {
  readonly code: GiftErrorCode;
  constructor(code: GiftErrorCode) { super(messages[code]); this.name = "GiftError"; this.code = code; }
  userMessage(): string { return this.message; }
}

export function giftFundingAmount(card: Pick<GiftCard, "amountZat">): bigint {
  if (card.amountZat <= 0n || card.amountZat > MAX_MONEY - GIFT_CLAIM_RESERVE_ZAT) throw new GiftError("invalid_amount");
  return card.amountZat + GIFT_CLAIM_RESERVE_ZAT;
}

export function zecAmount(zat: bigint): string {
  if (zat < 0n || zat > MAX_MONEY) throw new GiftError("invalid_amount");
  return `${zat / 100_000_000n}.${(zat % 100_000_000n).toString().padStart(8, "0")}`;
}

export function encodeBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

export function decodeBase64Url(encoded: string, maxLength = MAX_GIFT_LINK_LENGTH): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/.test(encoded) || encoded.length > maxLength) throw new GiftError("invalid_link");
  try {
    const binary = atob(encoded.replaceAll("-", "+").replaceAll("_", "/"));
    const bytes = Uint8Array.from(binary, ch => ch.charCodeAt(0));
    if (encodeBase64Url(bytes) !== encoded) throw new GiftError("invalid_link");
    return bytes;
  } catch { throw new GiftError("invalid_link"); }
}

function presentationString(value: unknown, limit: number): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "string" || Array.from(value).length > limit || new TextEncoder().encode(value).length > 512) throw new GiftError("invalid_link");
  return value.trim() || undefined;
}

export function validateGiftCard(card: GiftCard): void {
  if (!["mainnet", "testnet", "regtest"].includes(card.network) ||
    !(card.entropy instanceof Uint8Array) || ![16, 20, 24, 28, 32].includes(card.entropy.length) ||
    !Number.isInteger(card.birthdayHeight) || card.birthdayHeight < 1 || card.birthdayHeight > 0xffffffff) throw new GiftError("invalid_link");
  giftFundingAmount(card);
  if (card.artworkId !== undefined && !/^[a-zA-Z0-9_-]{1,64}$/.test(card.artworkId)) throw new GiftError("invalid_link");
  presentationString(card.label, 128);
  presentationString(card.message, 128);
  if (card.fiatUsd !== undefined && (!Number.isFinite(card.fiatUsd) || card.fiatUsd < 0)) throw new GiftError("invalid_link");
}

function endpointUrl(raw: string): URL {
  try {
    const url = new URL(raw);
    const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
    if ((url.protocol !== "https:" && !(loopback && url.protocol === "http:")) || url.username || url.password || url.search || url.hash) throw new GiftError("invalid_link");
    return url;
  } catch { throw new GiftError("invalid_link"); }
}

export function encodeGiftLink(card: GiftCard, endpoint: string): string {
  validateGiftCard(card);
  const url = endpointUrl(endpoint);
  if (card.network === "mainnet" && url.protocol !== "https:") throw new GiftError("invalid_link");
  const payload: unknown[] = [wireNetwork[card.network], encodeBase64Url(card.entropy), card.birthdayHeight,
    card.amountZat.toString(), card.artworkId ?? null, card.fiatUsd ?? null, card.message ?? null, card.label ?? null];
  while (payload.length > 4 && payload.at(-1) === null) payload.pop();
  url.hash = `v3=${encodeBase64Url(new TextEncoder().encode(JSON.stringify(payload)))}`;
  if (url.href.length > MAX_GIFT_LINK_LENGTH) throw new GiftError("invalid_link");
  return url.href;
}

export function parseGiftLink(raw: string, options: { endpoints: readonly string[]; network: GiftNetwork }): GiftCard {
  try {
    if (raw.length > MAX_GIFT_LINK_LENGTH) throw new GiftError("invalid_link");
    const url = new URL(raw.trim());
    if (url.username || url.password || url.search || !url.hash.startsWith("#v3=")) throw new GiftError("invalid_link");
    const trusted = options.endpoints.some(endpoint => {
      const expected = endpointUrl(endpoint);
      return url.origin === expected.origin && url.pathname === expected.pathname;
    });
    if (!trusted || (options.network === "mainnet" && url.protocol !== "https:")) throw new GiftError("invalid_link");
    const payload: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(decodeBase64Url(url.hash.slice(4))));
    if (!Array.isArray(payload) || payload.length < 4 || payload.length > 8) throw new GiftError("invalid_link");
    const [net, encodedEntropy, height, amount, artwork, fiat, message, label] = payload;
    const network = net === "main" ? "mainnet" : net === "test" ? "testnet" : net === "regtest" ? "regtest" : undefined;
    if (!network || typeof encodedEntropy !== "string" || encodedEntropy.length > 43 || typeof height !== "number" ||
      typeof amount !== "string" || !/^[1-9]\d{0,15}$/.test(amount)) throw new GiftError("invalid_link");
    if (network !== options.network) throw new GiftError("wrong_network");
    if (fiat !== undefined && fiat !== null && typeof fiat !== "number") throw new GiftError("invalid_link");
    const card: GiftCard = { network, entropy: decodeBase64Url(encodedEntropy), birthdayHeight: height, amountZat: BigInt(amount),
      artworkId: presentationString(artwork, 64), message: presentationString(message, 128), label: presentationString(label, 128),
      ...(typeof fiat === "number" ? { fiatUsd: fiat } : {}) };
    validateGiftCard(card);
    return card;
  } catch (error) {
    if (error instanceof GiftError && error.code === "wrong_network") throw error;
    throw new GiftError("invalid_link");
  }
}
