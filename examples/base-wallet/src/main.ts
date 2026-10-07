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
  type BasePaymentReview,
  type BaseWallet,
  type BaseSubmission,
} from "@z-stack/base";
import {
  getAddress,
  isHash,
  parseTransaction,
  decodeFunctionData,
  parseAbi,
  parseUnits,
  formatUnits,
} from "viem";
import "./style.css";
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

const input = (id: string) => document.getElementById(id) as HTMLInputElement;
const element = (id: string) => document.getElementById(id)!;
const message = (text: string) => {
  element("message").textContent = text;
};
let wallet: BaseWallet | undefined,
  review: BasePaymentReview | undefined,
  epoch = 0,
  busy = false;
const key = (w: BaseWallet) => `z-stack-base-demo:84532:${w.address.toLowerCase()}`;
function pending(w: BaseWallet): BaseTransactionRecord | undefined {
  const raw = localStorage.getItem(key(w));
  if (!raw) return;
  const row = JSON.parse(raw);
  if (
    !row ||
    !isHash(row.hash) ||
    row.chainId !== 84532 ||
    typeof row.serializedTransaction !== "string" ||
    !Number.isSafeInteger(row.nonce) ||
    typeof row.from !== "string" ||
    row.from.toLowerCase() !== w.address.toLowerCase()
  )
    throw new DemoError("Saved payment could not be read. Keep this browser data.");
  return row;
}
function guard() {
  const captured = epoch;
  return {
    assertCurrent() {
      if (captured !== epoch || !wallet) throw new DemoError("The wallet changed.");
    },
  };
}
function resetReview() {
  review = undefined;
  input("unlock").value = "";
  element("approval").hidden = true;
}
async function refresh() {
  if (!wallet) return;
  const w = wallet,
    g = guard();
  const balance = formatBaseBalance(await w.getBalance(g));
  g.assertCurrent();
  element("balance").textContent = `${balance.usdc} USDC · ${balance.eth} ETH`;
  element("status").hidden = !pending(w);
  element("archive").hidden = true;
}
async function action(run: () => Promise<void>) {
  if (busy) return;
  busy = true;
  const buttons = [...document.querySelectorAll("button")];
  buttons.forEach((b) => (b.disabled = true));
  try {
    await run();
  } catch (error) {
    message(
      error instanceof BaseSubmissionUnknownError
        ? `${error.message} ${error.submission.hash}`
        : error instanceof DemoError
          ? error.message
          : baseErrorMessage(error),
    );
  } finally {
    input("unlock").value = "";
    input("secret").value = "";
    busy = false;
    buttons.forEach((b) => (b.disabled = false));
  }
}
element("connect").addEventListener("submit", (event) => {
  event.preventDefault();
  void action(async () => {
    const phrase = input("secret").value.trim();
    input("secret").value = "";
    wallet = createBaseWallet({
      address: deriveBaseAddress(phrase),
      network: BASE_SEPOLIA,
      rpcUrl: input("rpc").value,
    });
    epoch++;
    resetReview();
    element("connect").hidden = true;
    element("wallet").hidden = false;
    element("address").textContent = wallet.address;
    message("Wallet open. The recovery phrase was cleared.");
    element("status").hidden = !pending(wallet);
    await refresh();
  });
});
element("refresh").addEventListener("click", () => {
  void action(refresh);
});
element("disconnect").addEventListener("click", () => {
  epoch++;
  wallet = undefined;
  resetReview();
  input("secret").value = "";
  element("wallet").hidden = true;
  element("connect").hidden = false;
  message("Disconnected. Saved payment hashes are retained.");
});
element("payment").addEventListener("submit", (event) => {
  event.preventDefault();
  void action(async () => {
    if (!wallet) throw new DemoError("Open your wallet first.");
    if (pending(wallet)) throw new DemoError("Check the saved payment before sending again.");
    resetReview();
    message("Checking the balance and network fee…");
    const asset = input("asset").value === "eth" ? "eth" : "usdc";
    review = await wallet.reviewPayment(
      {
        asset,
        recipient: getAddress(input("recipient").value),
        amount: parseUnits(input("amount").value, asset === "eth" ? 18 : 6),
        deadline: Date.now() + 300_000,
      },
      guard(),
    );
    element("review").textContent =
      `${formatUnits(review.amount, asset === "eth" ? 18 : 6)} ${asset.toUpperCase()} to ${review.recipient}. Estimated network fee: ${formatUnits(review.fee, 18)} ETH. Signed L2 fee limit: ${formatUnits(review.l2Max, 18)} ETH. L1 and operator charges can change before inclusion.`;
    element("approval").hidden = false;
    message("Ready to review. Unlock only when you are ready to send.");
  });
});
element("cancel").addEventListener("click", () => {
  resetReview();
  message("Payment cancelled.");
});
element("approval").addEventListener("submit", (event) => {
  event.preventDefault();
  void action(async () => {
    if (!wallet || !review) throw new DemoError("Review the payment first.");
    const w = wallet,
      r = review,
      g = guard();
    message("Unlocking and refreshing the fee estimate…");
    const hash = await w.sendPayment(r, {
      guard: g,
      withSpendLock: async (run) => {
        if (!navigator.locks?.request)
          throw new DemoError("This browser cannot safely coordinate payments.");
        return navigator.locks.request(
          `base-spend:84532:${w.address.toLowerCase()}`,
          { mode: "exclusive" },
          run,
        );
      },
      withSigner: async (run) => {
        const phrase = input("unlock").value.trim();
        input("unlock").value = "";
        return run(deriveBaseAccount(phrase));
      },
      assertNoPending() {
        if (pending(w)) throw new DemoError("A saved payment must be checked first.");
      },
      releaseSubmission: async (submission) => {
        const saved = pending(w);
        if (saved && saved.hash !== submission.hash) throw new BaseError("storage_failed");
        if (saved) localStorage.removeItem(key(w));
        if (localStorage.getItem(key(w)) !== null) throw new BaseError("storage_failed");
        element("status").hidden = true;
      },
      reserveSubmission: async (submission: BaseSubmission) => {
        const raw = JSON.stringify({
          hash: submission.hash,
          chainId: submission.chainId,
          from: submission.from,
          nonce: submission.nonce,
          serializedTransaction: submission.serializedTransaction,
        });
        localStorage.setItem(key(w), raw);
        if (localStorage.getItem(key(w)) !== raw)
          throw new DemoError("The payment could not be saved.");
        element("status").hidden = false;
        message("Submitting once. Keep this tab open.");
      },
    });
    resetReview();
    message(`Payment submitted: ${hash}. Check its status for confirmation.`);
  });
});
element("retry").addEventListener("click", () => {
  void action(async () => {
    if (!wallet) return;
    const currentWallet = wallet,
      g = guard(),
      saved = pending(wallet);
    if (!saved || !window.confirm(retryDescription(saved, currentWallet))) return;
    const status = await getBasePaymentStatus({
      network: currentWallet.network,
      transaction: saved,
      guard: g,
    });
    if (status !== "unknown" && status !== "pending") {
      message("Check the saved payment status before retrying.");
      return;
    }
    await navigator.locks.request(
      `base-spend:84532:${currentWallet.address.toLowerCase()}`,
      { mode: "exclusive" },
      async () => {
        g.assertCurrent();
        if (pending(currentWallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        const latestStatus = await getBasePaymentStatus({
          network: currentWallet.network,
          transaction: saved,
          guard: g,
        });
        g.assertCurrent();
        if (pending(currentWallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        if (latestStatus !== "unknown" && latestStatus !== "pending") {
          message("The payment status changed. Check confirmation before retrying.");
          element("retry").hidden = true;
          return;
        }
        const hash = await rebroadcastBaseTransaction({
          network: currentWallet.network,
          transaction: saved,
          guard: g,
        });
        message(`Saved transaction submitted: ${hash}. Check its status for confirmation.`);
      },
    );
  });
});
element("archive").addEventListener("click", () => {
  void action(async () => {
    if (!wallet) return;
    const selectedWallet = wallet,
      saved = pending(wallet),
      g = guard();
    if (
      !saved ||
      !window.confirm(
        "Have you checked this payment in your Base activity? It may have completed. Archiving permits other payments; do not repeat this payment unless you have verified its outcome.",
      )
    )
      return;
    await navigator.locks.request(
      `base-spend:84532:${selectedWallet.address.toLowerCase()}`,
      { mode: "exclusive" },
      async () => {
        g.assertCurrent();
        if (pending(selectedWallet)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        if (
          (await getBasePaymentStatus({
            network: selectedWallet.network,
            transaction: saved,
            guard: g,
          })) !== "nonce-consumed"
        )
          throw new DemoError("This transaction could still execute. Keep checking its status.");
        g.assertCurrent();
        const archivedKey = `${key(selectedWallet)}:unverified:${saved.hash}`,
          raw = JSON.stringify(saved);
        localStorage.setItem(archivedKey, raw);
        if (localStorage.getItem(archivedKey) !== raw) throw new BaseError("storage_failed");
        localStorage.removeItem(key(selectedWallet));
        if (localStorage.getItem(key(selectedWallet)) !== null)
          throw new BaseError("storage_failed");
      },
    );
    element("archive").hidden = true;
    message(
      "Archived with an unverified outcome. Check Base activity before repeating this payment.",
    );
    await refresh();
  });
});
element("status").addEventListener("click", () => {
  void action(async () => {
    if (!wallet) return;
    const w = wallet,
      g = guard(),
      saved = pending(w);
    if (!saved) return;
    const status = await getBasePaymentStatus({
      network: w.network,
      transaction: saved,
      guard: g,
    });
    if (status === "unknown" || status === "pending" || status === "included") {
      message(
        status === "unknown"
          ? "No receipt yet. Retry the saved transaction to submit the same payment, with the same nonce and signed gas limits."
          : "Base payment is waiting for finality.",
      );
      element("retry").hidden = status === "included";
      return;
    }
    if (status === "nonce-consumed") {
      message(
        "This nonce was used, but this payment could not be verified. Check your Base activity before making another payment.",
      );
      element("retry").hidden = true;
      element("archive").hidden = false;
      return;
    }
    await navigator.locks.request(
      `base-spend:84532:${w.address.toLowerCase()}`,
      { mode: "exclusive" },
      async () => {
        g.assertCurrent();
        if (pending(w)?.hash !== saved.hash) throw new BaseError("wallet_changed");
        localStorage.removeItem(key(w));
        if (localStorage.getItem(key(w)) !== null) throw new BaseError("storage_failed");
      },
    );
    element("retry").hidden = true;
    message(
      status === "confirmed"
        ? "Payment confirmed."
        : "Base payment failed on chain. Review a new payment.",
    );
    await refresh();
  });
});

window.addEventListener("pagehide", () => {
  epoch++;
  wallet = undefined;
  resetReview();
  input("secret").value = "";
  input("unlock").value = "";
  element("wallet").hidden = true;
  element("connect").hidden = false;
  message("Disconnected. Reopen the wallet to continue.");
});
