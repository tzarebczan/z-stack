import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { randomBytes } from "node:crypto";
import { generateRegistrationOptions, verifyRegistrationResponse, generateAuthenticationOptions, verifyAuthenticationResponse,
  type RegistrationResponseJSON, type AuthenticationResponseJSON } from "@simplewebauthn/server";
import { BackupDatabase } from "./database.ts";
import { validateRecord } from "./record.ts";

type Challenge = { kind: "register" | "authenticate"; challenge: string; owner?: string; expires: number; cookie: string };
class HttpFailure extends Error { constructor(readonly status: number, readonly code: string) { super(code); } }
const random = () => randomBytes(32).toString("base64url");
const cookies = (request: IncomingMessage) => Object.fromEntries((request.headers.cookie ?? "").split(";").map(part => part.trim().split("=")));
async function body(request: IncomingMessage) {
  if (!request.headers["content-type"]?.startsWith("application/json")) throw new HttpFailure(415, "json-required");
  let bytes = 0; const chunks: Buffer[] = [];
  for await (const chunk of request) {
    bytes += chunk.length;
    if (bytes > 256 * 1024) throw new HttpFailure(413, "request-too-large");
    chunks.push(chunk);
  }
  try {
    const value = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error();
    return value as Record<string, unknown>;
  } catch { throw new HttpFailure(400, "invalid-request"); }
}
/** Deliberately loopback-only reference, not a hosted account service. */
export function createBackupServer(database: BackupDatabase, origin = "http://localhost:5173") {
  const url = new URL(origin);
  if (url.hostname !== "localhost" || url.protocol !== "http:" || url.origin !== origin) throw new Error("Use an exact localhost HTTP origin for this example.");
  const challenges = new Map<string, Challenge>();
  let requests = 0, resetAt = Date.now() + 60_000;
  const reply = (response: ServerResponse, status: number, value: unknown) => { response.writeHead(status); response.end(JSON.stringify(value)); };
  const cookie = (response: ServerResponse, name: string, value: string, age: number) => {
    const existing = response.getHeader("Set-Cookie");
    response.setHeader("Set-Cookie", [...(Array.isArray(existing) ? existing : existing ? [String(existing)] : []),
      `${name}=${value}; Path=/api; HttpOnly; SameSite=Strict; Max-Age=${age}`]);
  };
  return createServer({ requestTimeout: 15_000, headersTimeout: 10_000 }, async (request, response) => {
    response.setHeader("Content-Type", "application/json"); response.setHeader("Cache-Control", "no-store");
    response.setHeader("X-Content-Type-Options", "nosniff"); response.setHeader("Referrer-Policy", "no-referrer");
    response.setHeader("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'");
    try {
      if (!["127.0.0.1", "::1", "::ffff:127.0.0.1"].includes(request.socket.remoteAddress ?? "")) throw new HttpFailure(403, "loopback-only");
      if (Date.now() > resetAt) { requests = 0; resetAt = Date.now() + 60_000; }
      if (++requests > 200) throw new HttpFailure(429, "rate-limited");
      if (request.headers["x-example-request"] !== "1" || (request.headers.origin && request.headers.origin !== origin) ||
        (request.method !== "GET" && request.headers.origin !== origin)) throw new HttpFailure(403, "origin-rejected");
      const path = new URL(request.url ?? "/", origin).pathname;
      const token = cookies(request)["example-session"] ?? "";
      const currentOwner = () => { const owner = database.owner(token); if (!owner) throw new HttpFailure(401, "sign-in-required"); return owner; };
      if (request.method === "POST" && ["/api/register/options", "/api/authenticate/options"].includes(path)) {
        for (const [key, value] of challenges) if (value.expires <= Date.now()) challenges.delete(key);
        if (challenges.size >= 100) throw new HttpFailure(429, "rate-limited");
        const kind = path.includes("register") ? "register" : "authenticate";
        const owner = kind === "register" ? random() : undefined;
        const options = kind === "register" ? await generateRegistrationOptions({ rpName: "Backup Example", rpID: "localhost",
          userName: "Example wallet", userID: Buffer.from(owner!, "base64url"), attestationType: "none",
          authenticatorSelection: { residentKey: "required", userVerification: "required" } }) :
          await generateAuthenticationOptions({ rpID: "localhost", userVerification: "required" });
        const requestId = random(), binding = random();
        challenges.set(requestId, { kind, owner, challenge: options.challenge, expires: Date.now() + 120_000, cookie: binding });
        cookie(response, `example-challenge-${requestId}`, binding, 120);
        return reply(response, 200, { requestId, options });
      }
      if (request.method === "POST" && ["/api/register/verify", "/api/authenticate/verify"].includes(path)) {
        const input = await body(request);
        const id = typeof input.requestId === "string" ? input.requestId : "";
        const challenge = challenges.get(id);
        // Consume before any asynchronous verification; replays cannot race.
        if (!challenge || challenge.cookie !== cookies(request)[`example-challenge-${id}`]) throw new HttpFailure(401, "invalid-challenge");
        challenges.delete(id);
        cookie(response, `example-challenge-${id}`, "", 0);
        const kind = path.includes("register") ? "register" : "authenticate";
        if (challenge.expires <= Date.now() || challenge.kind !== kind) throw new HttpFailure(401, "invalid-challenge");
        let owner: string;
        try {
          if (kind === "register") {
            const verified = await verifyRegistrationResponse({ response: input.response as RegistrationResponseJSON,
              expectedChallenge: challenge.challenge, expectedOrigin: origin, expectedRPID: "localhost", requireUserVerification: true });
            if (!verified.verified || !verified.registrationInfo) throw new Error();
            owner = challenge.owner!; database.register(owner, verified.registrationInfo.credential);
          } else {
            const credentialId = (input.response as AuthenticationResponseJSON)?.id;
            if (typeof credentialId !== "string") throw new Error();
            const stored = database.credential(credentialId); if (!stored) throw new Error();
            const verified = await verifyAuthenticationResponse({ response: input.response as AuthenticationResponseJSON,
              expectedChallenge: challenge.challenge, expectedOrigin: origin, expectedRPID: "localhost",
              requireUserVerification: true, credential: stored.credential });
            if (!verified.verified) throw new Error();
            database.advanceCounter(credentialId, stored.credential.counter, verified.authenticationInfo.newCounter); owner = stored.owner;
          }
        } catch { throw new HttpFailure(401, "verification-failed"); }
        database.logout(token); const session = random(); database.session(session, owner);
        cookie(response, "example-session", session, 1800);
        return reply(response, 200, { authenticated: true });
      }
      if (path === "/api/logout" && request.method === "POST") { database.logout(token); cookie(response, "example-session", "", 0); return reply(response, 200, { loggedOut: true }); }
      if (path === "/api/account" && request.method === "DELETE") { database.deleteAccount(currentOwner()); cookie(response, "example-session", "", 0); return reply(response, 200, { deleted: true }); }
      const match = /^\/api\/backups\/([A-Za-z0-9_-]{1,128})$/.exec(path);
      if (match) {
        if (request.method === "GET") {
          const row = database.get(currentOwner(), match[1]);
          return reply(response, row ? 200 : 404, row ?? { code: "backup-missing" });
        }
        if (request.method === "PUT") {
          const input = await body(request); const owner = currentOwner();
          let record;
          try { record = validateRecord(input.record); } catch { throw new HttpFailure(400, "invalid-record"); }
          if (Object.keys(input).some(key => !["record", "expectedRevision"].includes(key)) || record.id !== match[1] || record.purpose !== "wallet-seed" ||
            (!record.forgotten && (record.rpId !== "localhost" || record.userId !== owner || record.passkeys.some(key => database.credential(key.credentialId)?.owner !== owner))) ||
            (input.expectedRevision !== undefined && (!Number.isSafeInteger(input.expectedRevision) || Number(input.expectedRevision) < 1))) throw new HttpFailure(400, "invalid-record");
          try { database.put(owner, record, input.expectedRevision as number | undefined); }
          catch (error) { if (error instanceof Error && error.message === "revision-conflict") throw new HttpFailure(409, "revision-conflict"); throw error; }
          return reply(response, 200, { revision: record.revision });
        }
      }
      throw new HttpFailure(404, "route-missing");
    } catch (error) { reply(response, error instanceof HttpFailure ? error.status : 500, { code: error instanceof HttpFailure ? error.code : "request-failed" }); }
  });
}
