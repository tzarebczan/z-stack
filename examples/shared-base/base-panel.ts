import {
  BASE_SEPOLIA,
  BaseSubmissionUnknownError,
  BaseError,
  baseErrorMessage,
  getBasePaymentStatus,
  rebroadcastBaseTransaction,
  type BaseTransactionRecord,
  createBaseWallet,
  deriveBaseAccount,
  deriveBaseAddress,
  formatBaseBalance,
  type BaseWallet,
  type BasePaymentReview,
} from "@z-stack/base";
import { WalletError, type Wallet } from "@z-stack/sdk";
import {
  getAddress,
  isHash,
  parseTransaction,
  decodeFunctionData,
  parseAbi,
  parseUnits,
  formatUnits,
} from "viem";

class DemoError extends Error {}
function retryDescription(saved: BaseTransactionRecord, wallet: BaseWallet): string {
  const tx = parseTransaction(saved.serializedTransaction as `0x02${string}`);
  let amount = tx.value ?? 0n,
    recipient = tx.to,
    asset = "ETH",
    decimals = 18;
  if (tx.data && tx.data !== "0x") {
    if (tx.to?.toLowerCase() !== wallet.network.usdc.toLowerCase())
      throw new BaseError("transaction_mismatch");
    const decoded = decodeFunctionData({
      abi: parseAbi(["function transfer(address to, uint256 amount) returns (bool)"]),
      data: tx.data,
    });
    [recipient, amount] = decoded.args;
    asset = "USDC";
    decimals = 6;
  }
  return `Retry ${formatUnits(amount, decimals)} ${asset} to ${recipient} on Base Sepolia?\nSigned L2 gas ceiling: ${formatUnits(tx.gas! * tx.maxFeePerGas!, 18)} ETH. Base's variable fees may change.\nThe same signed bytes and nonce cannot create a second transaction.`;
}

/** App-owned coordination. A phrase must unlock the selected Zcash wallet before Base derivation. */
export interface BasePanelHost {
  identity(): string | undefined;
  withWallet<T>(action: (wallet: Wallet) => Promise<T>): Promise<T>;
}

export function mountBasePanel(host: HTMLElement, app: BasePanelHost) {
  // Static markup only: wallet data is assigned through textContent, never interpolated.
  host.innerHTML = `<section aria-label="Base Sepolia wallet" class="base-panel">
    <h2>Base, with the same recovery phrase.</h2>
    <p class="hint">Optional · Base Sepolia test funds only. Base activity is public.</p>
    <form id="base-connect" method="post">
      <label>Base Sepolia RPC<input id="base-rpc" type="url" required value="https://sepolia.base.org" /></label>
      <label>Your Zcash recovery phrase<input id="base-secret" type="password" required autocomplete="off" spellcheck="false" /></label>
      <button>Enable Base Sepolia</button>
    </form>
    <section id="base-wallet" hidden>
      <code id="base-address"></code><p id="base-balance"></p>
      <div class="actions"><button id="base-refresh" type="button">Refresh Base balance</button>
      <button id="base-disconnect" type="button">Disconnect Base</button></div>
      <form id="base-payment" method="post">
        <label>Base asset<select id="base-asset"><option value="usdc">USDC</option><option value="eth">ETH</option></select></label>
        <label>Base recipient<input id="base-recipient" required autocomplete="off" /></label>
        <label>Base amount<input id="base-amount" required inputmode="decimal" /></label>
        <button>Review Base payment</button>
      </form>
      <form id="base-approval" method="post" hidden>
        <h3>Review Base payment</h3><p id="base-review"></p>
        <label>Zcash recovery phrase for Base payment<input id="base-unlock" type="password" required autocomplete="off" spellcheck="false" /></label>
        <button>Send Base payment</button><button id="base-cancel" type="button">Cancel Base payment</button>
      </form>
      <button id="base-check" type="button" hidden>Check saved Base payment</button>
      <button id="base-retry" type="button" hidden>Retry saved transaction</button>
      <button id="base-archive" type="button" hidden>I checked Base activity — archive unverified payment</button>
    </section>
    <p id="base-message" role="status" aria-live="polite"></p>
  </section>`;
  const nodes = new Map(
    [...host.querySelectorAll<HTMLElement>("[id]")].map((node) => [node.id, node]),
  );
  const node = (id: string) => nodes.get(`base-${id}`)!;
  const input = (id: string) => node(id) as HTMLInputElement;
  const lifecycle = new AbortController();
  let w: BaseWallet | undefined,
    review: BasePaymentReview | undefined,
    selected: string | undefined;
  let epoch = 0,
    busy = false;
  const message = (value: string) => {
    if (!lifecycle.signal.aborted) node("message").textContent = value;
  };
  const key = (wallet: BaseWallet) => `z-stack-base-demo:84532:${wallet.address.toLowerCase()}`;
  const lockName = (wallet: BaseWallet) => `base-spend:84532:${wallet.address.toLowerCase()}`;
  function pending(wallet: BaseWallet): BaseTransactionRecord | undefined {
    const raw = localStorage.getItem(key(wallet));
    if (!raw) return;
    if (raw.length > 262_144)
      throw new DemoError("Saved payment could not be read. Keep this browser data.");
    const row = JSON.parse(raw);
    if (
      !row ||
      !isHash(row.hash) ||
      row.chainId !== 84532 ||
      typeof row.serializedTransaction !== "string" ||
      !Number.isSafeInteger(row.nonce) ||
      typeof row.from !== "string" ||
      row.from.toLowerCase() !== wallet.address.toLowerCase()
    )
      throw new DemoError("Saved payment could not be read. Keep this browser data.");
    return row;
  }
  function guard() {
    const captured = epoch,
      identity = selected;
    return {
      signal: lifecycle.signal,
      assertCurrent() {
        lifecycle.signal.throwIfAborted();
        if (captured !== epoch || !w || identity !== app.identity())
          throw new DemoError("The wallet changed.");
      },
    };
  }
  function resetReview() {
    review = undefined;
    input("unlock").value = "";
    node("approval").hidden = true;
    node("payment").hidden = false;
  }
  async function spendLock<T>(wallet: BaseWallet, run: () => Promise<T>): Promise<T> {
    if (!navigator.locks?.request)
      throw new DemoError("This browser cannot safely coordinate payments.");
    return navigator.locks.request(
      lockName(wallet),
      { mode: "exclusive", signal: lifecycle.signal },
      run,
    );
  }
  async function refresh() {
    if (!w) return;
    const wallet = w,
      g = guard(),
      balance = formatBaseBalance(await wallet.getBalance(g));
    g.assertCurrent();
    node("balance").textContent = `${balance.usdc} USDC · ${balance.eth} ETH`;
    node("check").hidden = !pending(wallet);
    node("archive").hidden = true;
  }
  async function action(run: (wallet: Wallet) => Promise<void>) {
    if (busy || lifecycle.signal.aborted) return;
    busy = true;
    const controls = [...host.querySelectorAll<HTMLInputElement>("button,input,select")];
    controls.forEach((control) => {
      control.disabled = true;
    });
    try {
      await app.withWallet(run);
    } catch (error) {
      const detail =
        error instanceof BaseError && ["unlock_failed", "payment_blocked"].includes(error.code)
          ? error.cause
          : error;
      message(
        error instanceof BaseSubmissionUnknownError
          ? `Submission acknowledgement is unknown. Check the saved payment: ${error.submission.hash}`
          : detail instanceof WalletError
            ? detail.userMessage()
            : detail instanceof DemoError
              ? detail.message
              : baseErrorMessage(error),
      );
    } finally {
      input("secret").value = "";
      input("unlock").value = "";
      busy = false;
      controls.forEach((control) => {
        control.disabled = false;
      });
      if (review && !lifecycle.signal.aborted) input("unlock").focus();
    }
  }
  function listen(id: string, event: string, run: (event: Event) => void) {
    node(id).addEventListener(event, run, { signal: lifecycle.signal });
  }
  listen("connect", "submit", (event) => {
    event.preventDefault();
    const words = input("secret").value.trim(),
      rpcUrl = input("rpc").value;
    input("secret").value = "";
    void action(async (zcash) => {
      const identity = app.identity();
      if (!identity) throw new DemoError("Create or restore your Zcash wallet first.");
      message("Checking your recovery phrase…");
      try {
        await zcash.unlock(words);
        lifecycle.signal.throwIfAborted();
        if (identity !== app.identity()) throw new DemoError("The wallet changed.");
        w = createBaseWallet({ address: deriveBaseAddress(words), network: BASE_SEPOLIA, rpcUrl });
      } finally {
        zcash.lock();
      }
      selected = identity;
      epoch++;
      resetReview();
      node("connect").hidden = true;
      node("wallet").hidden = false;
      node("address").textContent = w.address;
      message("Base enabled. Your recovery phrase was cleared.");
      await refresh();
    });
  });
  listen("refresh", "click", () => {
    void action(refresh);
  });
  listen("disconnect", "click", () => {
    if (busy) return;
    epoch++;
    w = undefined;
    selected = undefined;
    resetReview();
    node("connect").hidden = false;
    node("wallet").hidden = true;
    message("Base disconnected. Saved payment hashes are retained.");
  });
  listen("payment", "submit", (event) => {
    event.preventDefault();
    const asset = input("asset").value === "eth" ? "eth" : "usdc";
    const recipient = input("recipient").value,
      amount = input("amount").value;
    void action(async () => {
      if (!w) throw new DemoError("Enable Base first.");
      if (pending(w)) throw new DemoError("Check the saved payment before sending again.");
      resetReview();
      message("Checking the balance and network fee…");
      review = await w.reviewPayment(
        {
          asset,
          recipient: getAddress(recipient),
          amount: parseUnits(amount, asset === "eth" ? 18 : 6),
          deadline: Date.now() + 300_000,
        },
        guard(),
      );
      node("review").textContent =
        `${formatUnits(review.amount, asset === "eth" ? 18 : 6)} ${asset.toUpperCase()} to ${review.recipient}. Estimated network fee: ${formatUnits(review.fee, 18)} ETH. Signed L2 fee limit: ${formatUnits(review.l2Max, 18)} ETH. L1 and operator charges can change before inclusion.`;
      node("payment").hidden = true;
      node("approval").hidden = false;
      input("unlock").focus();
      message("Check the recipient, amount and fee estimate.");
    });
  });
  listen("cancel", "click", () => {
    resetReview();
    message("Base payment cancelled.");
  });
  listen("approval", "submit", (event) => {
    event.preventDefault();
    const words = input("unlock").value.trim();
    input("unlock").value = "";
    void action(async (zcash) => {
      if (!w || !review) throw new DemoError("Review the payment first.");
      const wallet = w,
        approved = review,
        g = guard();
      message("Unlocking and refreshing the fee estimate…");
      try {
        const hash = await wallet.sendPayment(approved, {
          guard: g,
          withSpendLock: (run) => spendLock(wallet, run),
          withSigner: async (run) => {
            await zcash.unlock(words);
            g.assertCurrent();
            return run(deriveBaseAccount(words));
          },
          assertNoPending() {
            if (pending(wallet)) throw new DemoError("Check the saved payment first.");
          },
          releaseSubmission: async (submission) => {
            const saved = pending(wallet);
            if (saved && saved.hash !== submission.hash) throw new BaseError("storage_failed");
            if (saved) localStorage.removeItem(key(wallet));
            if (localStorage.getItem(key(wallet)) !== null) throw new BaseError("storage_failed");
            node("check").hidden = true;
          },
          reserveSubmission: async (submission) => {
            const raw = JSON.stringify({
              hash: submission.hash,
              chainId: submission.chainId,
              from: submission.from,
              nonce: submission.nonce,
              serializedTransaction: submission.serializedTransaction,
            });
            localStorage.setItem(key(wallet), raw);
            if (localStorage.getItem(key(wallet)) !== raw)
              throw new DemoError("The payment could not be saved.");
            node("check").hidden = false;
            message("Submitting once. Keep this tab open.");
          },
        });
        message(`Base payment submitted: ${hash}. Check its status for confirmation.`);
      } finally {
        zcash.lock();
        resetReview();
      }
    });
  });
  listen("check", "click", () => {
    void action(async () => {
      if (!w) return;
      const wallet = w,
        g = guard(),
        saved = pending(wallet);
      if (!saved) return;
      const status = await getBasePaymentStatus({
        network: wallet.network,
        transaction: saved,
        guard: g,
      });
      if (status === "unknown" || status === "pending" || status === "included") {
        message(
          status === "unknown"
            ? "No receipt yet. Retry the saved transaction to submit the same payment, with the same nonce and signed gas limits."
            : "Base payment is waiting for finality.",
        );
        node("retry").hidden = status === "included";
        return;
      }
      if (status === "nonce-consumed") {
        message(
          "This nonce was used, but this payment could not be verified. Check your Base activity before making another payment.",
        );
        node("retry").hidden = true;
        node("archive").hidden = false;
        return;
      }
      await spendLock(wallet, async () => {
        g.assertCurrent();
        if (pending(wallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        localStorage.removeItem(key(wallet));
        if (localStorage.getItem(key(wallet)) !== null) throw new BaseError("storage_failed");
      });
      node("retry").hidden = true;
      message(
        status === "confirmed"
          ? "Base payment confirmed."
          : "Base payment failed on chain. Review a new payment.",
      );
      await refresh();
    });
  });
  listen("retry", "click", () => {
    void action(async () => {
      if (!w) return;
      const wallet = w,
        g = guard(),
        saved = pending(w);
      if (!saved || !window.confirm(retryDescription(saved, wallet))) return;
      const status = await getBasePaymentStatus({
        network: wallet.network,
        transaction: saved,
        guard: g,
      });
      if (status !== "unknown" && status !== "pending") {
        message("Check the saved payment status before retrying.");
        return;
      }
      await spendLock(wallet, async () => {
        g.assertCurrent();
        if (pending(wallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        const latestStatus = await getBasePaymentStatus({
          network: wallet.network,
          transaction: saved,
          guard: g,
        });
        g.assertCurrent();
        if (pending(wallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        if (latestStatus !== "unknown" && latestStatus !== "pending") {
          message("The payment status changed. Check confirmation before retrying.");
          node("retry").hidden = true;
          return;
        }
        const hash = await rebroadcastBaseTransaction({
          network: wallet.network,
          transaction: saved,
          guard: g,
        });
        message(`Saved transaction submitted: ${hash}. Check its status for confirmation.`);
      });
    });
  });
  listen("archive", "click", () => {
    void action(async () => {
      if (!w) return;
      const wallet = w,
        saved = pending(w),
        g = guard();
      if (
        !saved ||
        !window.confirm(
          "Have you checked this payment in your Base activity? It may have completed. Archiving permits other payments; do not repeat this payment unless you have verified its outcome.",
        )
      )
        return;
      await spendLock(wallet, async () => {
        g.assertCurrent();
        if (pending(wallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        if (
          (await getBasePaymentStatus({
            network: wallet.network,
            transaction: saved,
            guard: g,
          })) !== "nonce-consumed"
        )
          throw new DemoError("This transaction could still execute. Keep checking its status.");
        g.assertCurrent();
        const archivedKey = `${key(wallet)}:unverified:${saved.hash}`,
          raw = JSON.stringify(saved);
        localStorage.setItem(archivedKey, raw);
        if (localStorage.getItem(archivedKey) !== raw) throw new BaseError("storage_failed");
        localStorage.removeItem(key(wallet));
        if (localStorage.getItem(key(wallet)) !== null) throw new BaseError("storage_failed");
      });
      node("archive").hidden = true;
      message(
        "Archived with an unverified outcome. Check Base activity before repeating this payment.",
      );
      await refresh();
    });
  });
  function dispose() {
    lifecycle.abort();
    epoch++;
    w = undefined;
    selected = undefined;
    input("secret").value = "";
    input("unlock").value = "";
    host.replaceChildren();
  }
  window.addEventListener("pagehide", dispose, { once: true, signal: lifecycle.signal });
  return dispose;
}
