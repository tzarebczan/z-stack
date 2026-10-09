import assert from "node:assert/strict";

// Read only the disposable example's committed reference, never seed or vault data.
async function savedPreview(page) {
  return page.evaluate(() => new Promise((resolve, reject) => {
    const open = indexedDB.open("z-stack-wasm", 1);
    open.onerror = () => reject(new Error("Could not read example snapshot"));
    open.onsuccess = () => {
      const db = open.result;
      const read = db.transaction("wallets", "readonly").objectStore("wallets").get("default");
      read.onerror = () => { db.close(); reject(new Error("Could not read example snapshot")); };
      read.onsuccess = () => {
        const value = read.result;
        db.close();
        resolve(value ? { key: value.key, address: value.preview?.unifiedAddress,
          scanned: value.preview?.scannedHeight ?? 0 } : null);
      };
    };
  }));
}

export async function assertSavedHide(page, address) {
  const before = await savedPreview(page);
  assert.equal(before?.address, address);
  assert.equal(typeof before?.key, "string");
  // Keep the document alive so hide flush errors remain observable in Playwright.
  await page.evaluate(() => {
    window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: true }));
    return new Promise(resolve => setTimeout(resolve, 100));
  });
  const after = await savedPreview(page);
  assert.equal(after?.address, address);
  assert.ok(after.scanned >= before.scanned, "hide must preserve committed scan progress");
}

export async function verifyBackForward(page, ready, address) {
  const origin = new URL(page.url()).origin;
  await page.route("**/back-forward-check", route => route.fulfill({
    status: 200, contentType: "text/html", body: "<!doctype html><title>Navigation check</title>",
    headers: { "Cross-Origin-Opener-Policy": "same-origin", "Cross-Origin-Embedder-Policy": "require-corp" },
  }));
  await page.goto(`${origin}/back-forward-check`);
  await page.goBack();
  // Browsers may restore a cached document or load a fresh one; both must work.
  await ready();
  assert.equal(await page.locator("#address").textContent(), address);
  assert.equal((await savedPreview(page))?.address, address);
}
