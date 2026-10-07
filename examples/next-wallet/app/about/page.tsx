import Link from "next/link";

export default function About() {
  return <article className="reading"><p className="eyebrow">The integration</p>
    <h1>Your browser is the wallet.</h1>
    <p>This demo runs the packaged z-stack SDK in a Next.js app. It needs no account,
      backup server or hosted account.</p>
    <h2>What stays here</h2><p>Keys, scanning and proofs run locally. The saved wallet
      contains viewing data and activity, so it can reopen locked. Recovery phrases
      are never sent to Next.js or persisted. An unacknowledged creation phrase
      stays in page memory so returning to the screen can present it again.</p>
    <h2>What goes to the network</h2><p>Create checks the chain tip. Sync downloads
      compact blocks from the configured testnet server. Memo retrieval is on-demand
      and this demo does not request it. The chain server can see your IP and timing.</p>
    <h2>Recovery and deletion</h2><p>Save a new wallet’s phrase yourself. Restoring uses
      that same phrase; it does not generate a new one. Removing the local wallet
      deletes this browser’s saved copy. It does not move funds or delete backups
      held elsewhere. Spending lock leaves viewing data readable.</p>
    <h2>Hosting</h2><p>The production build serves matching workers, WASM and integrity
      files. Isolation headers enable the threaded engine; without them, the
      single-thread engine remains available. The demo uses Next’s webpack compiler.</p>
    <Link className="button primary" href="/" prefetch={false}>Back to wallet</Link>
  </article>;
}
