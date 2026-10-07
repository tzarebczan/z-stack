import { displayCatchUpPercent, NEAR_TIP_BLOCKS, STALL_QUIET_REMAINING } from "@z-stack/core";
import type { WasmProgress } from "@z-stack/sdk/lab";

export function SyncOverlay(props: {
  progress: WasmProgress;
  quiet?: boolean;
  canWipe?: boolean;
  onHide?: () => void;
  onForget?: () => void;
  onWipeResync?: () => void;
}) {
  const p = props.progress;
  const conflict =
    /tree conflict|Wipe scan/i.test(p.message ?? "") ||
    /commitment tree conflict/i.test(p.heading ?? "");
  const remaining = Math.max(0, p.tip - p.scanned);
  const nearTip = !conflict && p.tip > 0 && remaining > 0 && remaining <= NEAR_TIP_BLOCKS;
  const followOn = !conflict && p.tip > 0 && remaining > 0 && remaining <= STALL_QUIET_REMAINING;
  const pct = displayCatchUpPercent(p.percent, p.scanned, p.tip);
  const stalled = /not moving/i.test(p.message ?? "") || p.remainingHuman === "download not moving";
  const reconnecting = p.heading === "Waiting for light server";
  const stage = reconnecting
    ? "Waiting for light server"
    : stalled
    ? "Download stuck"
    : followOn
      ? "Catching up"
      : p.heading
        ? p.heading
        : p.stage === "connecting"
          ? "Connecting"
          : nearTip && (p.stage === "downloading" || p.stage === "scanning")
            ? "Catching up"
            : p.stage === "downloading"
              ? "Downloading compact blocks"
              : p.stage === "scanning"
                ? "Scanning"
                : p.stage === "enhancing"
                  ? "Reading memos"
                  : "Synced";
  const detail = reconnecting
    ? p.message || "Connection lost. Retrying…"
    : stalled
    ? p.message || "compact-block fetch not moving"
    : followOn || nearTip
      ? `${remaining} behind`
      : p.message
        ? p.message
        : `${p.scanned}/${p.tip}${p.blocksPerSecond ? ` · ${p.blocksPerSecond} blk/s` : ""}${
            p.remainingHuman && p.stage !== "synced" ? ` · ${p.remainingHuman}` : ""
          }`;
  const downloadLead = Math.max(0, (p.downloaded ?? p.scanned) - p.scanned);
  const activity = p.stage === "downloading" || p.stage === "scanning"
    ? [downloadLead > 0 ? `${downloadLead} fetched ahead` : "", p.blocksPerSecond ? `${p.blocksPerSecond} avg scan blk/s` : ""].filter(Boolean).join(" · ")
    : "";
  return (
    <div className={`sync-bar${props.quiet ? " quiet" : ""}`} role="status">
      <div className="sync-meta">
        <strong>{stage}</strong>
        <span>
          {followOn || nearTip ? `${pct.toFixed(0)}% · ${detail}` : detail}
          {activity && !nearTip ? ` · ${activity}` : ""}
          {(p.availableZat ?? 0) > 0 && p.availableZec ? ` · ${p.availableZec} ZEC` : ""}
        </span>
      </div>
      <div className="sync-track" aria-hidden="true">
        <div className="sync-fill" style={{ width: `${pct}%` }} />
      </div>
      <div className="sync-actions">
        <button className="btn ghost" type="button" onClick={() => props.onHide?.()}>
          Hide overlay
        </button>
        {props.canWipe ? (
          <button
            className="btn ghost"
            type="button"
            title="Clear notes and scan cache, then sync from birthday. Keys and seed stay."
            onClick={() => props.onWipeResync?.()}
          >
            Wipe scan & resync
          </button>
        ) : null}
        <button
          className="btn ghost"
          type="button"
          title="Delete this browser snapshot. Passkey stays. Not a scan wipe."
          onClick={() => props.onForget?.()}
        >
          Forget this device
        </button>
      </div>
    </div>
  );
}
