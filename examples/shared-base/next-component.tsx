"use client";
import { useEffect, useRef } from "react";
import type { Wallet } from "@z-stack/sdk";
import { mountBasePanel } from "../lib/base-panel";
export function BaseWallet(props: {
  identity: string;
  disabled: boolean;
  withWallet: <T>(action: (wallet: Wallet) => Promise<T>) => Promise<T>;
}) {
  const host = useRef<HTMLDivElement>(null);
  const latest = useRef(props);
  latest.current = props;
  useEffect(() => {
    if (!host.current) return;
    const mount = () =>
      mountBasePanel(host.current!, {
        identity: () => latest.current.identity,
        withWallet: (action) => latest.current.withWallet(action),
      });
    let dispose = mount();
    const restore = (event: PageTransitionEvent) => {
      if (!event.persisted) return;
      dispose();
      dispose = mount(); // Fresh opt-in/unlock; never restore signing authority.
    };
    window.addEventListener("pageshow", restore);
    return () => {
      window.removeEventListener("pageshow", restore);
      dispose();
    };
  }, [props.identity]);
  return (
    <fieldset disabled={props.disabled} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <div ref={host} />
    </fieldset>
  );
}
