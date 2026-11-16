/** @module @z-stack/passkey/device-approval */
/** Optional, memory-only QR transfer. Authentication and wallet import belong to the app. */
import { fromBase64Url, randomBytes, toBase64Url, utf8, wipe } from "./encoding";

const FORMAT = "z-stack/device-approval";
export const DEVICE_APPROVAL_REQUEST_PREFIX = "zstack-device-request:v1:";
export const DEVICE_APPROVAL_RESPONSE_PREFIX = "zstack-device-response:v1:";
export const DEVICE_APPROVAL_MAX_TTL_MS = 300_000;
const CLOCK_SKEW_MS = 30_000;
const MAX_REQUEST_BYTES = 2048;
const MAX_PAYLOAD_BYTES = 8192;
const MAX_RESPONSE_BYTES = 14_000;

export type DeviceApprovalMode = "wallet" | "view-only";
export type DeviceApprovalNetwork = "mainnet" | "testnet" | "regtest";
type PayloadContext = { network: DeviceApprovalNetwork; birthdayHeight: number; label?: string };
export type DeviceApprovalPayload =
  | (PayloadContext & { kind: "wallet"; mnemonic: string; fingerprint: string })
  | (PayloadContext & { kind: "view-only"; ufvk: string; fingerprint: string });
type RequestWire = {
  format: typeof FORMAT; version: 1; origin: string; mode: DeviceApprovalMode;
  sessionId: string; createdAt: number; expiresAt: number; recipientPublicKey: string;
};
type ResponseWire = {
  format: typeof FORMAT; version: 1; requestId: string; senderPublicKey: string; iv: string; ciphertext: string;
};
export type DeviceApprovalRequestDetails = Readonly<RequestWire & { requestId: string; verificationCode: string }>;
export type DeviceApprovalResponseDetails = Readonly<Pick<ResponseWire, "requestId" | "senderPublicKey"> & { verificationCode: string }>;
type Clock = () => number;
type RequestInput = { request: string; origin: string; now?: Clock };
export interface DeviceApprovalReceiver {
  readonly request: string;
  readonly details: DeviceApprovalRequestDetails;
  /** Failed scans preserve the request. A successful open consumes it. Concurrent opens are refused. */
  open(response: string): Promise<DeviceApprovalPayload>;
  dispose(): void;
}
export type DeviceApprovalErrorCode = "unsupported" | "invalid-request" | "invalid-response" | "invalid-payload" | "origin-mismatch" | "mode-mismatch" | "expired" | "busy" | "consumed" | "disposed" | "decrypt-failed";
export class DeviceApprovalError extends Error {
  constructor(readonly code: DeviceApprovalErrorCode, message: string) { super(message); this.name = "DeviceApprovalError"; }
}
function fail(code: DeviceApprovalErrorCode): never {
  const messages: Record<DeviceApprovalErrorCode, string> = {
    unsupported: "This device needs secure WebCrypto support.",
    "invalid-request": "This device approval request is invalid.",
    "invalid-response": "This device approval response is invalid or belongs to another request.",
    "invalid-payload": "The approved wallet details are invalid.",
    "origin-mismatch": "Open the same wallet site on both devices.",
    "mode-mismatch": "The approval must match the requested wallet access.",
    expired: "This device approval expired. Create a new request.",
    busy: "An approval is already being opened.",
    consumed: "This device approval was already opened.",
    disposed: "This device approval was cancelled.",
    "decrypt-failed": "This response could not be opened by this device.",
  };
  throw new DeviceApprovalError(code, messages[code]);
}
function subtle(): SubtleCrypto {
  if (!globalThis.crypto?.subtle || !globalThis.crypto?.getRandomValues) fail("unsupported");
  return globalThis.crypto.subtle;
}
function origin(value: string): string {
  try {
    const url = new URL(value);
    const local = url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
    if (url.origin !== value || url.username || url.password || (url.protocol !== "https:" && !local)) fail("origin-mismatch");
    return url.origin;
  } catch { return fail("origin-mismatch"); }
}
function record(value: unknown, keys: readonly string[], code: DeviceApprovalErrorCode): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some(key => !keys.includes(key))) fail(code);
  return value as Record<string, unknown>;
}
function integer(value: unknown, code: DeviceApprovalErrorCode): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) fail(code);
  return value;
}
function text(value: unknown, maximum: number, code: DeviceApprovalErrorCode): string {
  if (typeof value !== "string" || !value || value.length > maximum) fail(code);
  const raw = utf8(value);
  try { if (raw.length > maximum) fail(code); }
  finally { wipe(raw); }
  return value;
}
function bytes(value: unknown, minimum: number, maximum: number, code: DeviceApprovalErrorCode): Uint8Array<ArrayBuffer> {
  if (typeof value !== "string" || value.length > Math.ceil(maximum * 4 / 3) || !/^[A-Za-z0-9_-]+$/.test(value)) fail(code);
  try {
    const decoded = fromBase64Url(value);
    if (decoded.length < minimum || decoded.length > maximum || toBase64Url(decoded) !== value) fail(code);
    return decoded;
  } catch { return fail(code); }
}
function encode(prefix: string, value: RequestWire | ResponseWire): string {
  return prefix + toBase64Url(utf8(JSON.stringify(value)));
}
function decode(encoded: string, prefix: string, maximum: number, code: DeviceApprovalErrorCode): unknown {
  if (typeof encoded !== "string" || encoded.length > prefix.length + Math.ceil(maximum * 4 / 3) || !encoded.startsWith(prefix)) fail(code);
  const raw = bytes(encoded.slice(prefix.length), 1, maximum, code);
  try { return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(raw)); }
  catch { return fail(code); }
  finally { wipe(raw); }
}
function clock(now: Clock): number { return integer(now(), "invalid-request"); }
function fresh(request: RequestWire, now: Clock): void {
  const time = clock(now);
  if (request.createdAt > time + CLOCK_SKEW_MS || time >= request.expiresAt) fail("expired");
}
async function publicKey(encoded: string, code: DeviceApprovalErrorCode): Promise<CryptoKey> {
  const raw = bytes(encoded, 65, 65, code);
  try {
    if (raw[0] !== 4) fail(code);
    return await subtle().importKey("raw", raw, { name: "ECDH", namedCurve: "P-256" }, false, []);
  } catch { return fail(code); }
  finally { wipe(raw); }
}
async function digest(value: string): Promise<Uint8Array<ArrayBuffer>> {
  const raw = utf8(value);
  try { return new Uint8Array(await subtle().digest("SHA-256", raw)); }
  finally { wipe(raw); }
}
function verificationCode(hash: Uint8Array): string {
  const hex = Array.from(hash.subarray(0, 16), byte => byte.toString(16).padStart(2, "0")).join("").toUpperCase();
  return Array.from({ length: 8 }, (_, index) => hex.slice(index * 4, index * 4 + 4)).join("-");
}
async function parseRequest(input: RequestInput) {
  const value = record(decode(input.request, DEVICE_APPROVAL_REQUEST_PREFIX, MAX_REQUEST_BYTES, "invalid-request"), ["format", "version", "origin", "mode", "sessionId", "createdAt", "expiresAt", "recipientPublicKey"], "invalid-request");
  if (value.format !== FORMAT || value.version !== 1 || (value.mode !== "wallet" && value.mode !== "view-only")) fail("invalid-request");
  const requestOrigin = origin(text(value.origin, 512, "invalid-request"));
  if (requestOrigin !== origin(input.origin)) fail("origin-mismatch");
  const sessionId = text(value.sessionId, 43, "invalid-request"); wipe(bytes(sessionId, 32, 32, "invalid-request"));
  const createdAt = integer(value.createdAt, "invalid-request"), expiresAt = integer(value.expiresAt, "invalid-request");
  if (expiresAt <= createdAt || expiresAt - createdAt > DEVICE_APPROVAL_MAX_TTL_MS) fail("invalid-request");
  const recipientPublicKey = text(value.recipientPublicKey, 87, "invalid-request");
  const wire: RequestWire = { format: FORMAT, version: 1, origin: requestOrigin, mode: value.mode, sessionId, createdAt, expiresAt, recipientPublicKey };
  if (encode(DEVICE_APPROVAL_REQUEST_PREFIX, wire) !== input.request) fail("invalid-request");
  fresh(wire, input.now ?? Date.now);
  const recipient = await publicKey(recipientPublicKey, "invalid-request");
  const hash = await digest(JSON.stringify(wire));
  try {
    const details = Object.freeze({ ...wire, requestId: toBase64Url(hash), verificationCode: verificationCode(hash) });
    return { wire, recipient, details };
  } finally { wipe(hash); }
}
/** Public request inspection does not authorize a source wallet export. */
export async function parseDeviceApprovalRequest(input: RequestInput): Promise<DeviceApprovalRequestDetails> {
  return (await parseRequest(input)).details;
}
async function parseResponse(encoded: string, requestId: string) {
  const value = record(decode(encoded, DEVICE_APPROVAL_RESPONSE_PREFIX, MAX_RESPONSE_BYTES, "invalid-response"), ["format", "version", "requestId", "senderPublicKey", "iv", "ciphertext"], "invalid-response");
  if (value.format !== FORMAT || value.version !== 1 || value.requestId !== requestId) fail("invalid-response");
  const senderPublicKey = text(value.senderPublicKey, 87, "invalid-response");
  const iv = text(value.iv, 16, "invalid-response"); wipe(bytes(iv, 12, 12, "invalid-response"));
  const ciphertext = text(value.ciphertext, Math.ceil((MAX_PAYLOAD_BYTES + 16) * 4 / 3), "invalid-response"); wipe(bytes(ciphertext, 17, MAX_PAYLOAD_BYTES + 16, "invalid-response"));
  const wire: ResponseWire = { format: FORMAT, version: 1, requestId, senderPublicKey, iv, ciphertext };
  if (encode(DEVICE_APPROVAL_RESPONSE_PREFIX, wire) !== encoded) fail("invalid-response");
  return { wire, sender: await publicKey(senderPublicKey, "invalid-response") };
}
/** Compare this response-bound code physically on both devices before importing. It is not an account attestation. */
export async function parseDeviceApprovalResponse(input: RequestInput & { response: string }): Promise<DeviceApprovalResponseDetails> {
  const request = await parseRequest(input);
  const response = await parseResponse(input.response, request.details.requestId);
  const hash = await digest(JSON.stringify(response.wire));
  try { return Object.freeze({ requestId: response.wire.requestId, senderPublicKey: response.wire.senderPublicKey, verificationCode: verificationCode(hash) }); }
  finally { wipe(hash); }
}
function network(value: unknown): DeviceApprovalNetwork {
  if (value !== "mainnet" && value !== "testnet" && value !== "regtest") fail("invalid-payload");
  return value;
}
function viewingKey(value: unknown): string {
  const key = text(value, 4096, "invalid-payload");
  if (!/^uview(?:test|regtest)?1[a-z0-9]{32,}$/.test(key)) fail("invalid-payload");
  return key;
}
/** Full public-key identity, separate from the ZIP32 seed fingerprint. The app must validate the UFVK with its wallet engine. */
export async function deviceApprovalViewingKeyFingerprint(input: { ufvk: string; network: DeviceApprovalNetwork }): Promise<string> {
  const hash = await digest(JSON.stringify({ format: FORMAT, version: 1, purpose: "viewing-key-identity", network: network(input.network), ufvk: viewingKey(input.ufvk) }));
  try { return Array.from(hash, byte => byte.toString(16).padStart(2, "0")).join(""); }
  finally { wipe(hash); }
}
async function payload(value: unknown, mode: DeviceApprovalMode): Promise<DeviceApprovalPayload> {
  const parsed = record(value, ["kind", "network", "birthdayHeight", "label", "fingerprint", "mnemonic", "ufvk"], "invalid-payload");
  if (parsed.kind !== mode) fail("mode-mismatch");
  if ((mode === "wallet" && "ufvk" in parsed) || (mode === "view-only" && "mnemonic" in parsed)) fail("invalid-payload");
  const chain = network(parsed.network), birthdayHeight = integer(parsed.birthdayHeight, "invalid-payload");
  if (birthdayHeight > 0xffffffff) fail("invalid-payload");
  const fingerprint = text(parsed.fingerprint, 64, "invalid-payload");
  const label = parsed.label === undefined ? undefined : text(parsed.label, 120, "invalid-payload");
  if (label && /[\u0000-\u001f\u007f]/.test(label)) fail("invalid-payload");
  const common = { network: chain, birthdayHeight, ...(label === undefined ? {} : { label }) };
  if (mode === "wallet") {
    if (!/^[0-9a-f]{64}$/.test(fingerprint)) fail("invalid-payload");
    const mnemonic = text(parsed.mnemonic, 2048, "invalid-payload");
    if (mnemonic !== mnemonic.normalize("NFKD") || mnemonic.trim() !== mnemonic || ![12, 15, 18, 21, 24].includes(mnemonic.split(/\s+/u).length) || /[\u0000-\u001f\u007f]/.test(mnemonic)) fail("invalid-payload");
    return { kind: "wallet", mnemonic, fingerprint, ...common };
  }
  const ufvk = viewingKey(parsed.ufvk);
  if (fingerprint !== "view-only" && (!/^[0-9a-f]{64}$/.test(fingerprint) || fingerprint !== await deviceApprovalViewingKeyFingerprint({ ufvk, network: chain }))) fail("invalid-payload");
  return { kind: "view-only", ufvk, fingerprint, ...common };
}
function binding(request: RequestWire, senderPublicKey: string): string {
  return JSON.stringify({ format: FORMAT, version: 1, request, senderPublicKey });
}
async function encryptionKey(privateKey: CryptoKey, peer: CryptoKey, request: RequestWire, requestId: string, senderPublicKey: string): Promise<CryptoKey> {
  const shared = new Uint8Array(await subtle().deriveBits({ name: "ECDH", public: peer }, privateKey, 256));
  const salt = bytes(requestId, 32, 32, "invalid-request");
  const info = utf8(`${FORMAT}/v1/ECDH-P256/HKDF-SHA256/AES-256-GCM\n${binding(request, senderPublicKey)}`);
  try {
    const ikm = await subtle().importKey("raw", shared, "HKDF", false, ["deriveKey"]);
    return await subtle().deriveKey({ name: "HKDF", hash: "SHA-256", salt, info }, ikm, { name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
  } finally { wipe(shared); wipe(salt); wipe(info); }
}
function aad(request: RequestWire, senderPublicKey: string): Uint8Array<ArrayBuffer> {
  return utf8(`${FORMAT}/v1/response\n${binding(request, senderPublicKey)}`);
}
/** Call only after source-side fresh authentication and explicit export approval. */
export async function sealDeviceApproval(input: RequestInput & { payload: DeviceApprovalPayload }): Promise<string> {
  const request = await parseRequest(input), approved = await payload(input.payload, request.wire.mode);
  const plain = utf8(JSON.stringify(approved));
  if (plain.length > MAX_PAYLOAD_BYTES) { wipe(plain); fail("invalid-payload"); }
  let sender: CryptoKeyPair | null = null;
  let iv: Uint8Array<ArrayBuffer> | undefined;
  let additionalData: Uint8Array<ArrayBuffer> | undefined;
  try {
    iv = randomBytes(12);
    sender = await subtle().generateKey({ name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
    const senderPublicKey = toBase64Url(await subtle().exportKey("raw", sender.publicKey));
    const key = await encryptionKey(sender.privateKey, request.recipient, request.wire, request.details.requestId, senderPublicKey);
    additionalData = aad(request.wire, senderPublicKey);
    fresh(request.wire, input.now ?? Date.now);
    const ciphertext = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv, additionalData, tagLength: 128 }, key, plain));
    fresh(request.wire, input.now ?? Date.now);
    return encode(DEVICE_APPROVAL_RESPONSE_PREFIX, { format: FORMAT, version: 1, requestId: request.details.requestId, senderPublicKey, iv: toBase64Url(iv), ciphertext: toBase64Url(ciphertext) });
  } finally { sender = null; wipe(plain); wipe(iv); wipe(additionalData); }
}
/** The nonextractable ephemeral private key is retained only by this receiver's closure. */
export async function createDeviceApprovalRequest(options: { origin: string; mode: DeviceApprovalMode; ttlMs?: number; now?: Clock }): Promise<DeviceApprovalReceiver> {
  const now = options.now ?? Date.now, createdAt = clock(now), ttl = options.ttlMs ?? DEVICE_APPROVAL_MAX_TTL_MS;
  if (!Number.isSafeInteger(ttl) || ttl <= 0 || ttl > DEVICE_APPROVAL_MAX_TTL_MS || (options.mode !== "wallet" && options.mode !== "view-only")) fail("invalid-request");
  const expiresAt = integer(createdAt + ttl, "invalid-request");
  const requestOrigin = origin(options.origin);
  let pair: CryptoKeyPair | null = await subtle().generateKey({ name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
  const wire: RequestWire = { format: FORMAT, version: 1, origin: requestOrigin, mode: options.mode, sessionId: toBase64Url(randomBytes(32)), createdAt, expiresAt, recipientPublicKey: toBase64Url(await subtle().exportKey("raw", pair.publicKey)) };
  const request = encode(DEVICE_APPROVAL_REQUEST_PREFIX, wire), parsed = await parseRequest({ request, origin: options.origin, now });
  type State = { kind: "pending" | "opening"; key: CryptoKey } | { kind: "consumed" | "disposed" | "expired" };
  let state: State = { kind: "pending", key: pair.privateKey };
  pair = null;
  function assertOpen() {
    if (state.kind === "disposed" || state.kind === "consumed" || state.kind === "expired") fail(state.kind);
    try { fresh(wire, now); } catch { state = { kind: "expired" }; fail("expired"); }
  }
  return Object.freeze({ request, details: parsed.details,
    dispose() { state = { kind: "disposed" }; },
    async open(encoded: string) {
      assertOpen();
      if (state.kind === "opening") fail("busy");
      if (state.kind !== "pending") fail("disposed");
      const active = { kind: "opening" as const, key: state.key };
      state = active;
      const assertActive = () => { assertOpen(); if (state !== active) fail("disposed"); };
      let plain: Uint8Array<ArrayBuffer> | undefined, additionalData: Uint8Array<ArrayBuffer> | undefined;
      try {
        const response = await parseResponse(encoded, parsed.details.requestId);
        assertActive();
        const key = await encryptionKey(active.key, response.sender, wire, parsed.details.requestId, response.wire.senderPublicKey);
        assertActive();
        additionalData = aad(wire, response.wire.senderPublicKey);
        try {
          plain = new Uint8Array(await subtle().decrypt({ name: "AES-GCM", iv: bytes(response.wire.iv, 12, 12, "invalid-response"), additionalData, tagLength: 128 }, key, bytes(response.wire.ciphertext, 17, MAX_PAYLOAD_BYTES + 16, "invalid-response")));
        } catch { fail("decrypt-failed"); }
        assertActive();
        let decoded: unknown;
        try { decoded = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(plain)); } catch { fail("invalid-payload"); }
        const approved = await payload(decoded, wire.mode);
        if (new TextDecoder().decode(plain) !== JSON.stringify(approved)) fail("invalid-payload");
        assertActive();
        state = { kind: "consumed" };
        return approved;
      } finally {
        wipe(plain); wipe(additionalData);
        if (state === active) {
          try { fresh(wire, now); state = { kind: "pending", key: active.key }; }
          catch { state = { kind: "expired" }; }
        }
      }
    },
  });
}
