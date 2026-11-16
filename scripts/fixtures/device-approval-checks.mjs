// Shared source/installed-package contract. Synthetic strings are not wallet credentials.
export async function runDeviceApprovalChecks(api) {
  const origin = "https://wallet.example";
  const wallet = { kind: "wallet", mnemonic: Array(12).fill("fixture").join(" "), fingerprint: "11".repeat(32), network: "mainnet", birthdayHeight: 123, label: "Disposable transport fixture" };
  const ufvk = "uview1" + "a".repeat(120) + "b";
  const view = { kind: "view-only", ufvk, fingerprint: "view-only", network: "mainnet", birthdayHeight: 456 };
  const checks = [];
  const check = (condition, label) => { if (!condition) throw new Error(`Device approval check failed: ${label}`); checks.push(label); };
  const same = (a, b) => JSON.stringify(Object.entries(a).sort()) === JSON.stringify(Object.entries(b).sort());
  const rejects = async (work, codes, label) => {
    try { await work(); } catch (error) {
      check(error instanceof api.DeviceApprovalError && codes.includes(error.code), label);
      return;
    }
    throw new Error(`Expected rejection: ${label}`);
  };
  const encode = (prefix, value) => {
    const raw = new TextEncoder().encode(JSON.stringify(value));
    return prefix + btoa(Array.from(raw, byte => String.fromCharCode(byte)).join("")).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  };
  const decode = (prefix, encoded) => JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(encoded.slice(prefix.length).replace(/-/g, "+").replace(/_/g, "/")), char => char.charCodeAt(0))));
  const change = (prefix, encoded, update) => { const value = decode(prefix, encoded); update(value); return encode(prefix, value); };
  const requestPrefix = api.DEVICE_APPROVAL_REQUEST_PREFIX, responsePrefix = api.DEVICE_APPROVAL_RESPONSE_PREFIX;
  const create = mode => api.createDeviceApprovalRequest({ origin, mode });
  const seal = (receiver, payload) => api.sealDeviceApproval({ origin, request: receiver.request, payload });

  const receiver = await create("wallet");
  const details = await api.parseDeviceApprovalRequest({ origin, request: receiver.request });
  check(Object.isFrozen(receiver) && Object.isFrozen(details), "immutable receiver and public details");
  check(details.requestId === receiver.details.requestId && details.mode === "wallet", "public inspection matches request");
  check(/^(?:[0-9A-F]{4}-){7}[0-9A-F]{4}$/.test(details.verificationCode), "128-bit request comparison code");
  check(!("privateKey" in details) && !JSON.stringify(details).includes(wallet.mnemonic), "request contains no private key or payload");
  const response = await seal(receiver, wallet);
  const responseDetails = await api.parseDeviceApprovalResponse({ origin, request: receiver.request, response });
  check(/^(?:[0-9A-F]{4}-){7}[0-9A-F]{4}$/.test(responseDetails.verificationCode), "128-bit response comparison code");
  check(same(await receiver.open(response), wallet), "wallet roundtrip");
  await rejects(() => receiver.open(response), ["consumed"], "successful response cannot replay");
  const wire = decode(responsePrefix, response);
  check(Object.keys(wire).sort().join() === "ciphertext,format,iv,requestId,senderPublicKey,version", "response has only public keys and ciphertext");
  const again = await seal(receiver, wallet);
  const againDetails = await api.parseDeviceApprovalResponse({ origin, request: receiver.request, response: again });
  check(response !== again && responseDetails.senderPublicKey !== againDetails.senderPublicKey && responseDetails.verificationCode !== againDetails.verificationCode, "each seal changes sender key and bound code");

  const exactMode = await create("wallet");
  await rejects(() => seal(exactMode, view), ["mode-mismatch"], "wallet mode cannot be silently downgraded");
  exactMode.dispose();
  const readOnly = await create("view-only");
  await rejects(() => seal(readOnly, wallet), ["mode-mismatch"], "view-only request cannot export mnemonic");
  check(same(await readOnly.open(await seal(readOnly, view)), view), "view-only sentinel roundtrip");
  const fullIdentity = await api.deviceApprovalViewingKeyFingerprint({ ufvk, network: "mainnet" });
  check(/^[0-9a-f]{64}$/.test(fullIdentity), "full viewing-key identity is 256 bits");
  check(fullIdentity !== await api.deviceApprovalViewingKeyFingerprint({ ufvk: ufvk.slice(0, -1) + "c", network: "mainnet" }), "viewing identity binds full key suffix");
  check(fullIdentity !== await api.deviceApprovalViewingKeyFingerprint({ ufvk, network: "testnet" }), "viewing identity binds network");
  const hashed = await create("view-only");
  const hashedView = { ...view, fingerprint: fullIdentity };
  check(same(await hashed.open(await seal(hashed, hashedView)), hashedView), "full viewing-key identity roundtrip");
  const invalidIdentity = await create("view-only");
  await rejects(() => seal(invalidIdentity, { ...view, fingerprint: "22".repeat(32) }), ["invalid-payload"], "wrong full viewing-key identity refused");
  invalidIdentity.dispose();

  const accidental = await create("wallet"), valid = await seal(accidental, wallet);
  await rejects(() => accidental.open(accidental.request), ["invalid-response"], "request scanned as response does not consume receiver");
  const another = await create("wallet"), otherResponse = await seal(another, wallet);
  await rejects(() => accidental.open(otherResponse), ["invalid-response"], "another recipient's response is refused");
  const relabelled = change(responsePrefix, otherResponse, value => { value.requestId = accidental.details.requestId; });
  await rejects(() => accidental.open(relabelled), ["decrypt-failed"], "relabelled response cannot cross recipient or session");
  for (const field of ["iv", "ciphertext"]) {
    const altered = change(responsePrefix, valid, value => { value[field] = (value[field][0] === "A" ? "B" : "A") + value[field].slice(1); });
    await rejects(() => accidental.open(altered), ["decrypt-failed"], `${field} tamper refused`);
  }
  const differentSender = decode(responsePrefix, otherResponse).senderPublicKey;
  await rejects(() => accidental.open(change(responsePrefix, valid, value => { value.senderPublicKey = differentSender; })), ["decrypt-failed"], "sender public key bound to ciphertext");
  await rejects(() => accidental.open(change(responsePrefix, valid, value => { value.privateKey = "forbidden"; })), ["invalid-response"], "unknown response fields refused");
  await rejects(() => accidental.open(valid + "="), ["invalid-response"], "noncanonical response encoding refused");
  check(same(await accidental.open(valid), wallet), "bad scans and tamper preserve valid retry");

  await rejects(() => api.parseDeviceApprovalRequest({ origin: "https://other.example", request: receiver.request }), ["origin-mismatch"], "cross-origin request refused");
  for (const bad of ["http://wallet.example", "https://wallet.example/", "https://user:password@wallet.example", "null"]) {
    await rejects(() => api.createDeviceApprovalRequest({ origin: bad, mode: "wallet" }), ["origin-mismatch"], "unsafe or noncanonical origin refused");
  }
  const loopback = await api.createDeviceApprovalRequest({ origin: "http://127.0.0.1:1234", mode: "wallet" }); loopback.dispose();
  for (const ttlMs of [0, -1, 0.5, 300001, Infinity]) await rejects(() => api.createDeviceApprovalRequest({ origin, mode: "wallet", ttlMs }), ["invalid-request"], "invalid TTL refused");
  for (const edit of [
    value => { value.extra = true; }, value => { value.version = 2; }, value => { value.mode = "spending"; },
    value => { value.sessionId = "short"; }, value => { value.expiresAt = value.createdAt + 300001; },
    value => { value.recipientPublicKey = "A".repeat(87); }, value => { delete value.origin; },
  ]) await rejects(() => api.parseDeviceApprovalRequest({ origin, request: change(requestPrefix, receiver.request, edit) }), ["invalid-request", "origin-mismatch"], "invalid request wire refused");
  const reordered = decode(requestPrefix, receiver.request);
  await rejects(() => api.parseDeviceApprovalRequest({ origin, request: encode(requestPrefix, Object.fromEntries(Object.entries(reordered).reverse())) }), ["invalid-request"], "request field order is canonical");
  await rejects(() => api.parseDeviceApprovalRequest({ origin, request: requestPrefix + "A".repeat(10000) }), ["invalid-request"], "oversized encoded request refused");
  const payloadInvalid = await create("wallet");
  for (const edit of [value => { value.fingerprint = "short"; }, value => { value.ufvk = ufvk; }, value => { value.birthdayHeight = -1; }, value => { value.label = "x".repeat(121); }, value => { value.mnemonic = "not enough words"; }, value => { value.privateKey = "forbidden"; }]) {
    const value = { ...wallet }; edit(value);
    await rejects(() => seal(payloadInvalid, value), ["invalid-payload"], "invalid or secret-bearing payload fields refused");
  }
  payloadInvalid.dispose();

  let now = Date.now();
  const expired = await api.createDeviceApprovalRequest({ origin, mode: "wallet", ttlMs: 1000, now: () => now });
  const expiresResponse = await api.sealDeviceApproval({ origin, request: expired.request, payload: wallet, now: () => now });
  now += 1000;
  await rejects(() => expired.open(expiresResponse), ["expired"], "receiver expires at exact deadline");
  await rejects(() => api.sealDeviceApproval({ origin, request: expired.request, payload: wallet, now: () => now }), ["expired"], "source refuses expired request");
  now -= 1000;
  await rejects(() => expired.open(expiresResponse), ["expired"], "clock rollback cannot revive expired receiver");

  const racing = await create("wallet"), racingResponse = await seal(racing, wallet);
  const results = await Promise.allSettled([racing.open(racingResponse), racing.open(racingResponse)]);
  check(results.filter(value => value.status === "fulfilled").length === 1 && results.some(value => value.status === "rejected" && value.reason.code === "busy"), "one in-flight opener wins");
  const disposed = await create("wallet"), disposedResponse = await seal(disposed, wallet);
  const opening = disposed.open(disposedResponse); disposed.dispose();
  await rejects(() => opening, ["disposed"], "dispose during open suppresses plaintext return");
  await rejects(() => disposed.open(disposedResponse), ["disposed"], "disposed receiver cannot reopen");
  another.dispose(); receiver.dispose(); readOnly.dispose(); hashed.dispose(); accidental.dispose(); racing.dispose();
  return checks;
}
