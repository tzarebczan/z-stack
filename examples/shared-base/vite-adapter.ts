import type { Wallet } from "@z-stack/sdk";
import { mountBasePanel } from "./base-panel";
export function attachBase(
  wallet: Wallet,
  run: <T>(action: () => Promise<T>) => Promise<T>,
  identity: () => string | undefined,
) {
  const host = document.createElement("div");
  document.querySelector("main")!.append(host);
  const dispose = mountBasePanel(host, {
    identity,
    withWallet: (action) => run(() => action(wallet)),
  });
  const restore = (event: PageTransitionEvent) => {
    if (event.persisted) location.reload();
  };
  window.addEventListener("pageshow", restore);
  return () => {
    window.removeEventListener("pageshow", restore);
    dispose();
  };
}
