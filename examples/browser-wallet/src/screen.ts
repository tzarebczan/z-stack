import { classifyHistory, formatZatoshis, type Wallet, type WalletSnapshot } from "@z-stack/sdk";
import { copyText, element } from "./dom";
import { drawReceiveQr } from "./receive-qr";
import { pendingFunds, confirmationLabel, activityMovement, confirmationPolicyText, scannerLabel } from "./wallet-view";
import { connection } from "./connection";

/** Render committed wallet data. Sync progress never clears this view. */
export function attachScreen(wallet: Wallet, unit: string) {
  const balance = element("balance");
  const address = element("address");
  const history = element("history");
  let loaded: { snapshot: WalletSnapshot; pendingPayment: boolean } | undefined;
  let server = "Configured transport";
  try {
    if (typeof connection.server === "string") server = new URL(connection.server, window.location.href).host;
  } catch { /* Never display a credential-bearing provider URL. */ }

  const showRuntime = () => { element("runtime").textContent = scannerLabel(wallet.runtime); };
  showRuntime();
  wallet.on("runtime", showRuntime);
  wallet.on("balance", value => showBalance(BigInt(value.availableZat), BigInt(value.pendingZat ?? 0), loaded?.snapshot.confirmations));
  element("copy-address").addEventListener("click", () => {
    if (loaded) void copyText(loaded.snapshot.unifiedAddress, element("copy-status"));
  });

  function showBalance(available: bigint, pending: bigint, policy?: WalletSnapshot["confirmations"]) {
    balance.textContent = `${formatZatoshis(available)} ${unit}`;
    for (const id of ["pending-balance", "pending-help", "send-confirming"]) element(id).hidden = pending === 0n;
    element("pending-balance").textContent = `Confirming · ${formatZatoshis(pending)} ${unit}`;
    element("pending-help").textContent = `Confirming funds cannot be spent yet. ${confirmationPolicyText(policy)}`;
    element("send-confirming").textContent = `${formatZatoshis(pending)} ${unit} is still confirming. Only available funds can be spent.`;
  }
  function hasScanned() {
    return loaded !== undefined && (loaded.snapshot.scannedHeight ?? 0) >= loaded.snapshot.birthdayHeight;
  }
  async function render(snapshot: WalletSnapshot) {
    loaded = { snapshot, pendingPayment: (await wallet.pending(1)).length > 0 };
    for (const id of ["balance-panel", "activity-panel", "receive-panel", "scan-panel", "send-panel", "copy-address"]) element(id).hidden = false;
    showRuntime();
    const scanned = snapshot.scannedHeight;
    const scanText = scanned !== undefined && scanned >= snapshot.birthdayHeight
      ? "Scanned through " + scanned.toLocaleString() : "Not scanned yet";
    element("scan-details").textContent = `${connection.network} · ${server} · Birthday ${snapshot.birthdayHeight.toLocaleString()} · ${scanText}`;
    if (address.textContent !== snapshot.unifiedAddress) drawReceiveQr(element("receive-qr", HTMLCanvasElement), snapshot.unifiedAddress);
    address.textContent = snapshot.unifiedAddress;
    showBalance(BigInt(snapshot.balance.totalAvailable), pendingFunds(snapshot), snapshot.confirmations);
    const entries = await wallet.history(20);
    history.replaceChildren(...entries.map(entry => {
      const row = document.createElement("li");
      const movement = document.createElement("strong");
      const confirmations = document.createElement("span");
      const txid = document.createElement("code");
      movement.textContent = activityMovement(entry, unit);
      confirmations.textContent = confirmationLabel(entry, snapshot);
      txid.textContent = entry.txid;
      row.append(movement, confirmations, txid);
      for (const memo of classifyHistory(entry).memos) {
        const text = document.createElement("p");
        text.className = "memo";
        text.textContent = memo;
        row.append(text);
      }
      return row;
    }));
    element("history-empty").hidden = entries.length > 0;
    element("history-empty").textContent = hasScanned() ? "No activity yet." : "Sync to look for activity.";
    return entries;
  }
  function clear() {
    loaded = undefined;
    address.textContent = "";
    balance.textContent = `— ${unit}`;
    history.replaceChildren();
    element("copy-status").textContent = "";
    element("remove-status").textContent = "";
    element("remove-confirm", HTMLInputElement).checked = false;
    element("remove-panel", HTMLDetailsElement).open = false;
    for (const id of ["pending-balance", "pending-help", "send-confirming", "receive-panel", "scan-panel", "send-panel", "balance-panel", "activity-panel"]) element(id).hidden = true;
    element("history-empty").hidden = false;
    element("history-empty").textContent = "Create or restore a wallet to see activity.";
    showRuntime();
  }
  return { render, clear, hasScanned,
    get snapshot() { return loaded?.snapshot; },
    get pendingPayment() { return loaded?.pendingPayment ?? false; },
  };
}
