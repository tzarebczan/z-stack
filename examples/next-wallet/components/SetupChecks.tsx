"use client";
import { useEffect, useRef, useState } from "react";
import { checkWalletSetup, type SetupReport } from "@z-stack/sdk/diagnostics";

export function SetupChecks() {
  const pending = useRef<AbortController | undefined>(undefined);
  const [report, setReport] = useState<SetupReport>();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  useEffect(() => () => { pending.current?.abort(); pending.current = undefined; }, []);
  return <details className="setup"><summary>Check this deployment</summary>
    <p className="hint">Checks browser support, workers and matching engine files.
      Downloads the engine files. Nothing is uploaded.</p>
    <button disabled={busy} onClick={() => {
      const controller = new AbortController(); pending.current = controller; setBusy(true); setFailed(false);
      void checkWalletSetup({ assets: true, worker: true, timeoutMs: 60_000, signal: controller.signal })
        .then(value => { if (pending.current === controller) setReport(value); })
        .catch(() => { if (pending.current === controller) setFailed(true); })
        .finally(() => { if (pending.current === controller) { pending.current = undefined; setBusy(false); } });
    }}>{busy ? "Checking…" : "Run setup checks"}</button>
    {failed && <p role="status">Could not complete setup checks. Try again.</p>}
    {report && <><p role="status">{report.ok ? "Deployment checks passed." : "Deployment needs attention."}</p>
      <ul className="checks">{report.checks.filter(check => check.status !== "skipped").map(check =>
        <li key={check.code}><span>{check.code.replaceAll("_", " ")}</span>
          <strong>{check.status}</strong>{check.status !== "pass" && <p className="hint">{check.message}</p>}</li>)}</ul></>}
  </details>;
}
