import assert from "node:assert/strict";
import { test } from "node:test";
import { encodeGiftLink, parseGiftLink, encodeBase64Url, decodeBase64Url, giftFundingAmount, zecAmount } from "../src/protocol.ts";
const endpoint = "https://wallet.example/gift";
const card = { network: "regtest" as const, entropy: new Uint8Array(32), birthdayHeight: 150, amountZat: 1_000_000n };
const read = (link: string, network: "mainnet" | "testnet" | "regtest" = "regtest") => parseGiftLink(link, { endpoints: [endpoint], network });
test("compact v3 entropy preserves positional Vizor protocol and optional metadata", () => {
  for (const network of ["mainnet", "testnet", "regtest"] as const) for (const length of [16, 20, 24, 28, 32]) {
    const input = { ...card, network, entropy: new Uint8Array(length).fill(length), label: "A gift", message: "Enjoy 🎁", artworkId: "sun", fiatUsd: 5 };
    assert.deepEqual(read(encodeGiftLink(input, endpoint), network), input);
  }
  const link = encodeGiftLink(card, endpoint);
  assert.deepEqual(JSON.parse(new TextDecoder().decode(decodeBase64Url(link.split("#v3=")[1]))),
    ["regtest", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 150, "1000000"]);
  assert.equal(giftFundingAmount(card), 1_010_000n);
  assert.equal(zecAmount(1_010_000n), "0.01010000");
});
test("strict bounded secret parser rejects redirected endpoints, malformed entropy and schemas", () => {
  const link = encodeGiftLink(card, endpoint);
  const payload = (values: unknown[]) => endpoint + "#v3=" + encodeBase64Url(new TextEncoder().encode(JSON.stringify(values)));
  for (const invalid of [link.replace("wallet.example", "attacker.example"), link.replace("/gift#", "/other#"),
    link.replace("#v3=", "?seed=bad#v3="), link + "=", link.replace("https://", "https://user:password@"),
    payload(["regtest", "AA", 150, "1000000"]), payload(["regtest", encodeBase64Url(card.entropy), 0, "1000000"]),
    payload(["regtest", encodeBase64Url(card.entropy), 150, "01"]), payload(["regtest", encodeBase64Url(card.entropy), 150, "0"]),
    payload(["regtest", encodeBase64Url(card.entropy), 150, "1000000", null, null, "x".repeat(129)]), "x".repeat(20_000)]) {
    assert.throws(() => read(invalid), (e: unknown) => e instanceof Error && !e.message.includes(invalid) && "code" in e && e.code === "invalid_link");
  }
  assert.throws(() => read(link, "mainnet"), (e: unknown) => typeof e === "object" && e !== null && "code" in e && e.code === "wrong_network");
  assert.throws(() => giftFundingAmount({ amountZat: 2_100_000_000_000_000n }));
  assert.throws(() => encodeGiftLink({ ...card, network: "mainnet" }, "http://localhost/gift"));
});

test("documented public unfunded Vizor v3 vector decodes and encodes exactly", () => {
  const vizorEndpoint = "https://link.vizor.cash/payment-links/open";
  // Public zero-entropy vector from Vizor's compact-gift-links.md. Never fund it.
  const link = vizorEndpoint + "#v3=WyJtYWluIiwiQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQSIsMzQ4MzE0MSwiMTAwMDAwMCIsImtuaWdodE1hZ2ljIiwxMS4xNzQ3LCJJdCdzIGEgZ3JlYXQgZGF5IHRvIHNoaWVsZCB5b3VyIFpFQyDwn5uh77iPIl0";
  const decoded = parseGiftLink(link, { endpoints: [vizorEndpoint], network: "mainnet" });
  assert.deepEqual(decoded, { network: "mainnet", entropy: new Uint8Array(32),
    birthdayHeight: 3_483_141, amountZat: 1_000_000n, artworkId: "knightMagic",
    fiatUsd: 11.1747, message: "It's a great day to shield your ZEC 🛡️", label: undefined });
  assert.equal(link.length, 233);
  assert.equal(encodeGiftLink(decoded, vizorEndpoint), link);
});

test("Vizor messages count trimmed grapheme clusters and UTF-8 bytes", () => {
  for (const message of ["e\u0301".repeat(128), "👨‍💻".repeat(46), "😀".repeat(128)]) {
    const link = encodeGiftLink({ ...card, message: `  ${message}  ` }, endpoint);
    assert.equal(read(link).message, message);
  }
  for (const message of ["e\u0301".repeat(129), "👨‍💻".repeat(47), "x" + "\u0301".repeat(256)])
    assert.throws(() => encodeGiftLink({ ...card, message }, endpoint));
  assert.equal(read(encodeGiftLink({ ...card, message: "   " }, endpoint)).message, undefined);
});

test("Vizor labels preserve empty values and have only the bounded link envelope limit", () => {
  for (const label of ["", " ", "long label ".repeat(600)]) {
    const decoded = read(encodeGiftLink({ ...card, label }, endpoint));
    assert.equal(decoded.label, label.trim());
  }
  const normalized = read(encodeGiftLink({ ...card, artworkId: "  unknown-art  " }, endpoint));
  assert.equal(normalized.artworkId, "unknown-art");
  assert.throws(() => encodeGiftLink({ ...card, artworkId: "x".repeat(65) }, endpoint));
  assert.throws(() => encodeGiftLink({ ...card, label: "x".repeat(16_384) }, endpoint));
  assert.throws(() => encodeGiftLink({ ...card, label: "\u0001".repeat(3_000) }, endpoint));
  const payload = [" regtest ", encodeBase64Url(card.entropy), 150, "2100000000000000", null, null, null, ""];
  const decoded = read(endpoint + "#v3=" + encodeBase64Url(new TextEncoder().encode(JSON.stringify(payload))));
  assert.equal(decoded.network, "regtest");
  assert.equal(decoded.amountZat, 2_100_000_000_000_000n);
  assert.equal(decoded.label, "");
  assert.throws(() => giftFundingAmount(decoded), "creation still requires room for the claim reserve");
});

test("legacy Vizor envelopes remain explicitly unsupported", () => {
  const link = encodeGiftLink(card, endpoint);
  for (const version of ["v1", "v2"])
    assert.throws(() => read(link.replace("#v3=", `#${version}=`)));
});
