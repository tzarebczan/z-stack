import "./style.css";
import { WalletError } from "@z-stack/sdk";
import { startApp } from "./app";
import { element } from "./dom";

void startApp().catch(error => {
  const safe = WalletError.fromUnknown(error);
  // Provider errors can include private data. Log only the stable error code.
  console.error("Wallet startup failed", { code: safe.code });
  element("status").textContent = `${safe.userMessage()} Reload to try again.`;
  for (const id of ["create", "sync", "restore", "lock"]) element(id, HTMLButtonElement).disabled = true;
});
