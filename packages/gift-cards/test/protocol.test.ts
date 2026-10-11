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
