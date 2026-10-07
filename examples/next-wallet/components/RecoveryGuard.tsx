"use client";
import { useEffect, useSyncExternalStore } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { pendingRecovery } from "../lib/recovery-memory";

/** Layout lifetime: a route change must not remove the unsaved-backup warning. */
export function RecoveryGuard() {
  const pending = useSyncExternalStore(pendingRecovery.subscribe, pendingRecovery.snapshot, () => undefined);
  const pathname = usePathname();
  useEffect(() => {
    if (!pending) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [pending]);
  return pending && pathname !== "/" ? <p className="recovery-reminder" role="status">
    Recovery phrase ready. <Link href="/" prefetch={false}>Save your recovery phrase</Link>
  </p> : null;
}
