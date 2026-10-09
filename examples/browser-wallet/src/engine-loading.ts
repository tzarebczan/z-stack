import type { EngineLoadProgress } from "@z-stack/sdk";

/** Keep concurrent key/scanner downloads separate; neither represents total startup. */
export function engineLoading(output: HTMLElement, bar: HTMLProgressElement) {
  const latest = new Map<EngineLoadProgress["component"], EngineLoadProgress>([
    ["keys", { component: "keys", phase: "initialize" }],
    ["scanner", { component: "scanner", phase: "initialize" }],
  ]);
  return (progress: EngineLoadProgress) => {
    latest.set(progress.component, progress);
    const pending = [...latest.values()].filter(value => value.phase !== "ready");
    output.textContent = pending.map(value => {
      const label = value.component === "keys" ? "Wallet engine" : "Scanner";
      const mb = value.loadedBytes === undefined ? "" : ` · ${(value.loadedBytes / 1_000_000).toFixed(1)} MB`;
      return value.phase === "download" ? `${label}: downloading${mb}` :
        value.phase === "verify" ? `${label}: checking download${mb}` :
        value.phase === "fallback" ? `Starting compatible ${value.component === "keys" ? "wallet engine" : "scanner"}…` : `${label}: starting…`;
    }).join(" · ");
    bar.hidden = pending.length === 0;
    const current = pending.find(value => value.phase === "download" && value.totalBytes);
    if (pending.length === 1 && current?.totalBytes) {
      bar.max = current.totalBytes; bar.value = Math.min(current.loadedBytes ?? 0, current.totalBytes);
    } else bar.removeAttribute("value");
  };
}
