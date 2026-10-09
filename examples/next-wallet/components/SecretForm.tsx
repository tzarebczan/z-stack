"use client";
import { validateBirthdayInput, WalletError } from "@z-stack/sdk";
import { useEffect, useRef, useState } from "react";

export function SecretForm({ restore, disabled, submit, clearStatus }: {
  restore?: boolean; disabled: boolean; submit: (words: string, birthday: string) => Promise<boolean | void>;
  clearStatus?(expected: string): void;
}) {
  const words = useRef<HTMLTextAreaElement>(null);
  const birthday = useRef<HTMLInputElement>(null);
  const [validation, setValidation] = useState<{message: string; field?: "words" | "birthday"}>();
  const clear = () => { if (words.current) words.current.value = ""; };
  const clearValidation = (field?: "words" | "birthday") => {
    if (field === "birthday" || !field) birthday.current?.setCustomValidity("");
    if (!field || !validation?.field || validation.field === field) {
      if (validation) clearStatus?.(validation.message);
      setValidation(undefined);
    }
  };
  useEffect(() => {
    window.addEventListener("pagehide", clear);
    return () => { clear(); window.removeEventListener("pagehide", clear); };
  }, []);
  const id = restore ? "restore" : "unlock";
  return <form onInvalid={event => {
    const field = event.target === words.current ? "words" : "birthday";
    if (event.target === birthday.current && birthday.current?.validity.customError) return;
    setValidation({field, message: field === "words" ? "Enter your recovery phrase." : "Enter a wallet birthday."});
  }} onSubmit={event => {
    event.preventDefault();
    const value = words.current?.value.trim() ?? "";
    const height = birthday.current?.value.trim() ?? "";
    if (restore && birthday.current) {
      try { validateBirthdayInput(height); }
      catch (error) {
        const message = WalletError.fromUnknown(error).userMessage();
        setValidation({field:"birthday", message});
        birthday.current.setCustomValidity(message);
        birthday.current.reportValidity(); birthday.current.focus(); return;
      }
    }
    // Spending unlocks clear before async work; restore retains words on failure.
    if (!restore) clear();
    clearValidation();
    void (async () => {
      try { const restored = await submit(value, height); if (restore && restored === true) clear(); }
      catch (error) {
        const safe = WalletError.fromUnknown(error);
        setValidation({message: safe.userMessage(), field: safe.code === "invalid_birthday" ? "birthday"
          : safe.code === "invalid_recovery_phrase" || safe.code === "seed_mismatch" ? "words" : undefined});
      }
    })();
  }}>
    <label htmlFor={`${id}-words`}>Recovery phrase</label>
    <textarea id={`${id}-words`} ref={words} required disabled={disabled} rows={3}
      autoComplete="off" spellCheck={false} autoCapitalize="none" autoCorrect="off"
      onInput={() => clearValidation("words")} aria-invalid={validation?.field === "words" || undefined}
      placeholder="Enter your saved words" aria-describedby={`${id}-privacy ${id}-error`} />
    <p className="hint" id={`${id}-privacy`}>{restore ? "Used on this device. Kept if recovery fails; cleared on success or when you leave." : "Used on this device. Cleared when you unlock."}</p>
    {restore && <><label htmlFor="birthday">Wallet birthday</label>
      <input id="birthday" ref={birthday} required disabled={disabled} placeholder="Block height or YYYY-MM-DD"
        aria-invalid={validation?.field === "birthday" || undefined} aria-describedby={`birthday-hint ${id}-error`}
        onInput={() => clearValidation("birthday")} />
      <p className="hint" id="birthday-hint">Use a height or date from before your first deposit. Restore accepts valid 12-, 15-, 18-, 21- or 24-word phrases.</p></>}
    <p id={`${id}-error`} role="alert">{validation?.message}</p>
    <div className="actions"><button className="primary" disabled={disabled}>
      {restore ? "Restore wallet" : "Unlock spending"}</button>
      <button type="button" className="quiet" onClick={() => { clear(); clearValidation(); }}>Clear words</button></div>
  </form>;
}
