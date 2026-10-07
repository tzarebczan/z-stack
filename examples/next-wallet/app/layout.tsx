import type { Metadata } from "next";
import type { ReactNode } from "react";
import Link from "next/link";
import { RecoveryGuard } from "../components/RecoveryGuard";
import "./globals.css";

export const metadata: Metadata = {
  title: "z-stack · Wallet demo",
  description: "A local Zcash testnet wallet built with the z-stack SDK and Next.js.",
  robots: { index: false, follow: false },
};
export default function Layout({ children }: { children: ReactNode }) {
  return <html lang="en"><body><div className="shell">
    <header className="masthead"><Link className="brand" href="/" prefetch={false}>
      <span className="mark" aria-hidden="true">z</span> z-stack <span className="muted">/ wallet demo</span>
    </Link><Link href="/about" prefetch={false}>How it works</Link></header>
    <RecoveryGuard />
    <main>{children}</main>
    <footer>Test funds only. Keys and proofs stay on this device.</footer>
  </div></body></html>;
}
