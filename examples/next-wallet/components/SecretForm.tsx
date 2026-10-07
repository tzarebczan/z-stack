"use client";
import { useEffect, useRef } from "react";

export function SecretForm({ restore, disabled, submit }: {
  restore?: boolean; disabled: boolean; submit: (words: string, birthday: string) => Promise<void>;
}) {
  const words = useRef<HTMLTextAreaElement>(null);
  const birthday = useRef<HTMLInputElement>(null);
  const clear = () => { if (words.current) words.current.value = ""; };
  useEffect(() => {
    window.addEventListener("pagehide", clear);
    return () => { clear(); window.removeEventListener("pagehide", clear); };
  }, []);
  const id = restore ? "restore" : "unlock";
  return <form onSubmit={event => {
    event.preventDefault();
    const value = words.current?.value.trim() ?? "";
    const height = birthday.current?.value.trim() ?? "";
    clear(); // Clear the DOM before the first await, including failed attempts.
    void submit(value, height);
  }}>
    <label htmlFor={`${id}-words`}>Recovery phrase</label>
    <textarea id={`${id}-words`} ref={words} required disabled={disabled} rows={3}
      autoComplete="off" spellCheck={false} autoCapitalize="none" autoCorrect="off"
      placeholder="Enter your saved words" aria-describedby={`${id}-privacy`} />
    <p className="hint" id={`${id}-privacy`}>Used on this device. Cleared after submitting.</p>
    {restore && <><label htmlFor="birthday">Wallet birthday</label>
      <input id="birthday" ref={birthday} required disabled={disabled} placeholder="Block height or YYYY-MM-DD" />
      <p className="hint">Use a height or date from before your first deposit.</p></>}
    <div className="actions"><button className="primary" disabled={disabled}>
      {restore ? "Restore wallet" : "Unlock spending"}</button>
      <button type="button" className="quiet" onClick={clear}>Clear words</button></div>
  </form>;
}
