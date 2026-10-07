import { useEffect, useRef, useState } from "react";
import { isPasskeyAbort, unlockPasskeySeed } from "@z-stack/sdk/lab";

export type UnlockKind = "seed" | "passphrase" | "passkey";
export type PendingSpend = "send" | "shield" | "session";

export function UnlockOverlay(props: {
  kind: UnlockKind;
  pending: PendingSpend;
  hasEncrypted: boolean;
  hasPasskey: boolean;
  /** Offer portable recovery when WebAuthn is available without a local vault. */
  offerPasskey?: boolean;
  passkeyPortable?: boolean;
  busy: boolean;
  onCancel: () => void;
  onSeed: (mnemonic: string) => Promise<void>;
  onPassphrase: (pass: string) => Promise<void>;
  onError?: (message: string) => void;
}) {
  const offerPasskey = props.offerPasskey ?? props.hasPasskey;
  const [seed, setSeed] = useState("");
  const [pass, setPass] = useState("");
  const [mode, setMode] = useState<UnlockKind>(props.kind);
  const [passkeyBusy, setPasskeyBusy] = useState(false);
  const condAbort = useRef<AbortController | null>(null);
  const words = seed.trim().split(/\s+/).filter(Boolean).length;
  const title =
    props.pending === "shield"
      ? "Confirm shield"
      : props.pending === "send"
        ? "Confirm send"
        : "Unlock wallet";
  const hint =
    mode === "passkey"
      ? props.hasPasskey && props.passkeyPortable
        ? "Unlock the encrypted backup saved to your passkey."
        : props.hasPasskey
          ? "Unlock the encrypted backup saved in this browser."
          : "Choose a passkey with a portable encrypted backup, or use your recovery phrase."
      : mode === "passphrase"
        ? "Decrypt the seed stored on this device."
        : "Paste the matching 24-word seed. It is not written unless you encrypt it later.";

  useEffect(() => {
    setMode(props.kind);
  }, [props.kind]);

  useEffect(() => {
    if (!offerPasskey) return;
    const ac = new AbortController();
    condAbort.current = ac;
    void unlockPasskeySeed({ mediation: "conditional", signal: ac.signal })
      .then((mnemonic) => props.onSeed(mnemonic))
      .catch((e: unknown) => {
        if (isPasskeyAbort(e)) return;
        if (e instanceof DOMException && e.name === "NotSupportedError") return;
        props.onError?.(e instanceof Error ? e.message : String(e));
      });
    return () => {
      ac.abort();
      condAbort.current = null;
    };
    // Mount-only: start Bitwarden / Chrome conditional UI once per overlay.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [offerPasskey]);

  async function runPasskey() {
    condAbort.current?.abort();
    setPasskeyBusy(true);
    try {
      const mnemonic = await unlockPasskeySeed({ mediation: "required" });
      await props.onSeed(mnemonic);
    } catch (e) {
      if (!isPasskeyAbort(e)) {
        props.onError?.(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setPasskeyBusy(false);
    }
  }

  return (
    <div className="overlay" role="dialog" aria-modal="true" aria-labelledby="unlock-title">
      <div className="overlay-card">
        <div className="overlay-kicker">Spending seed</div>
        <h2 id="unlock-title">{title}</h2>
        <p className="status">{hint}</p>
        {offerPasskey ? (
          <input
            className="webauthn-bait"
            type="text"
            name="username"
            autoComplete="username webauthn"
            aria-label="Passkey"
            tabIndex={-1}
          />
        ) : null}
        {props.hasEncrypted || offerPasskey ? (
          <div className="chips">
            {offerPasskey ? (
              <button
                type="button"
                className={`chip${mode === "passkey" ? " on" : ""}`}
                onClick={() => setMode("passkey")}
              >
                Passkey
              </button>
            ) : null}
            {props.hasEncrypted ? (
              <button
                type="button"
                className={`chip${mode === "passphrase" ? " on" : ""}`}
                onClick={() => setMode("passphrase")}
              >
                Passphrase
              </button>
            ) : null}
            <button
              type="button"
              className={`chip${mode === "seed" ? " on" : ""}`}
              onClick={() => setMode("seed")}
            >
              Paste seed
            </button>
          </div>
        ) : null}
        {mode === "passkey" ? (
          <div className="status">
            {props.hasPasskey && props.passkeyPortable
              ? "Confirmed portable backup"
              : props.hasPasskey
                ? "Local encrypted backup"
                : "Requires passkey PRF support and an encrypted backup"}
          </div>
        ) : mode === "passphrase" ? (
          <label>
            Seed passphrase
            <input
              type="password"
              value={pass}
              autoComplete="current-password"
              onChange={(e) => setPass(e.target.value)}
              disabled={props.busy || passkeyBusy}
            />
          </label>
        ) : (
          <label>
            Mnemonic
            <textarea
              value={seed}
              onChange={(e) => setSeed(e.target.value)}
              placeholder="twenty four words"
              disabled={props.busy || passkeyBusy}
            />
            <span className={words === 12 || words === 24 ? "ok" : "status"}>{words} words</span>
          </label>
        )}
        <div className="actions">
          <button
            className="btn ghost"
            type="button"
            disabled={props.busy || passkeyBusy}
            onClick={props.onCancel}
          >
            Cancel
          </button>
          {mode === "passkey" ? (
            <button
              className="btn ember"
              type="button"
              disabled={props.busy || passkeyBusy}
              onClick={() => void runPasskey()}
            >
              {passkeyBusy ? "Waiting for passkey…" : "Use passkey"}
            </button>
          ) : (
            <button
              className="btn ember"
              type="button"
              disabled={
                props.busy ||
                passkeyBusy ||
                (mode === "passphrase" ? !pass : words !== 12 && words !== 24)
              }
              onClick={() =>
                mode === "passphrase" ? props.onPassphrase(pass) : props.onSeed(seed.trim())
              }
            >
              {props.pending === "session" ? "Unlock" : title}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
