import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { WalletPanel } from "./WalletPanel";
import "./style.css";

function App() {
  const [mounted, setMounted] = useState(true);
  return <main>
    <h1>React testnet wallet</h1>
    <p>Use test funds only. Save your phrase before leaving this screen.</p>
    <button onClick={() => setMounted(value => !value)}>{mounted ? "Close wallet screen" : "Open wallet screen"}</button>
    {mounted && <WalletPanel />}
  </main>;
}
createRoot(document.getElementById("root")!).render(<StrictMode><App /></StrictMode>);
