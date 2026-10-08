import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  NATIVE_BRIDGE_URL,
  LOCAL_GRPC_WEB,
  LOCAL_LWD_PIPE,
  LOCAL_ZAINO_GRPC,
  LOCAL_ZAINO_GRPC_MAINNET,
  LOCAL_ZAKURA_RPC_TESTNET,
  createEngineClient,
  createWasmClient,
  forgetWasmWallet,
  cancelWasmSync,
  setWasmSpendingSeed,
  grpcWebTransport,
  httpLwdTransport,
  initialize,
  scanWorkerStarting,
  scanWorkerRuntime,
  looksLikeGrpcWeb,
  looksLikeLwdPipe,
  usesFastSync,
  localEndpoints,
  peekWasmWallet,
  probeEngine,
  zip321Uri,
  unifiedAddressForSet,
  canDeriveUaReceiverSet,
  inspectAddressSummary,
  uaReceiverSetLabel,
  uaIncludesTransparent,
  DEFAULT_UA_RECEIVER_SET,
  persistEncryptedSeed,
  unlockEncryptedSeed,
  hasEncryptedSeed,
  splitProxyAuth,
  tryRegisterPasskeySeed,
  unlockPasskeySeed,
  forgetPasskeySeed,
  hasPasskeySeed,
  isPasskeyAbort,
  passkeyInfo,
  passkeyCapabilities,
  isTreeConflictError,
  type EngineClient,
  type FeeEstimate,
  type InspectedAddress,
  type MaxSend,
  type Network,
  type UaReceiverSet,
  type WalletSnapshot,
  type WasmProgress,
  type WasmRuntime,
  type PasskeyInfo,
} from "@z-stack/sdk/lab";
import {
  classifyHistoryList,
  WalletError,
  dateFromHeight,
  formatHistoryTime,
  parseBirthdayInput,
  parseZip321,
  syncEta,
  typicalTip,
  ymdDaysAgo,
  type HistoryEntry,
  type UnlockPolicy,
} from "@z-stack/core";
import { QrPlate } from "./Qr";
import { SyncOverlay } from "./SyncOverlay";
import { UnlockOverlay, type PendingSpend } from "./UnlockOverlay";
import { applyBalanceEvent, formatUiError, inspectTarget, syncEventToProgress } from "./dx";

type TransportKind = "proxy" | "grpc-web" | "pipe";
type RestoreMode = "seed" | "ufvk";
type HistoryFilter = "all" | "sent" | "received" | "shielding" | "swaps";

function wordCount(s: string): number {
  return s.trim().split(/\s+/).filter(Boolean).length;
}

function keepNoteBalance(cur: WalletSnapshot | null, next: WalletSnapshot): WalletSnapshot {
  if (cur && (cur.balance.totalAvailable ?? 0) > 0 && (next.balance.totalAvailable ?? 0) === 0) {
    return { ...next, balance: cur.balance };
  }
  return next;
}

const BRIDGE_TOKEN_STORAGE = "z-stack.web.bridge-token.v1";
const LAB_STORAGE = "z-stack.web.lab.v1";

type NetEndpoints = {
  lwdUrl: string;
  rpcUrl: string;
  transportKind: TransportKind;
  birthday: string;
  birthdayDate: string;
};

type LabPersisted = {
  network: Network;
  proxyUrl: string;
  birthday: string;
  birthdayDate: string;
  unlockPolicy: UnlockPolicy;
  autoShield: boolean;
  byNetwork: Partial<Record<Network, NetEndpoints>>;
};

type LabBoot = NetEndpoints & {
  network: Network;
  proxyUrl: string;
  unlockPolicy: UnlockPolicy;
  autoShield: boolean;
};

function asNetwork(v: unknown): Network | null {
  return v === "mainnet" || v === "testnet" || v === "regtest" ? v : null;
}

function asTransport(v: unknown, lwdUrl: string, proxyUrl?: string): TransportKind {
  if (v === "pipe" || looksLikeLwdPipe(lwdUrl) || looksLikeLwdPipe(proxyUrl ?? "")) return "pipe";
  if (looksLikeGrpcWeb(lwdUrl)) return "grpc-web";
  return v === "grpc-web" ? "grpc-web" : "proxy";
}

function asUnlock(v: unknown): UnlockPolicy {
  return v === "each-spend" || v === "always" ? v : "session";
}

function defaultTransportKind(n: Network, light: string): TransportKind {
  if (looksLikeGrpcWeb(light)) return "grpc-web";
  // Local Zaino is native gRPC. This lab reaches it through `z-wallet pipe` (:1239),
  // not the JSON bridge (:8787). Regtest compose still uses the loopback bridge.
  return n === "regtest" ? "proxy" : "pipe";
}

function proxyForTransport(kind: TransportKind, current: string): string {
  if (kind === "pipe") return LOCAL_LWD_PIPE;
  if (kind === "grpc-web") return current;
  return looksLikeLwdPipe(current) ? NATIVE_BRIDGE_URL : current || NATIVE_BRIDGE_URL;
}

function endpointsFor(n: Network, saved: LabPersisted | null): NetEndpoints {
  const hit = saved?.byNetwork?.[n];
  if (hit?.lwdUrl) {
    const lwdUrl =
      n === "mainnet" && hit.lwdUrl === LOCAL_ZAINO_GRPC ? LOCAL_ZAINO_GRPC_MAINNET : hit.lwdUrl;
    return {
      lwdUrl,
      rpcUrl: hit.rpcUrl ?? "",
      transportKind: asTransport(hit.transportKind, hit.lwdUrl, saved?.proxyUrl),
      birthday: hit.birthday ?? "",
      birthdayDate: hit.birthdayDate ?? "",
    };
  }
  const ep = localEndpoints(n);
  return {
    lwdUrl: ep.light,
    rpcUrl: ep.validatorRpc,
    transportKind: defaultTransportKind(n, ep.light),
    birthday: n === "regtest" ? "1" : "",
    birthdayDate: n === "regtest" ? "" : ymdDaysAgo(90),
  };
}

function loadLab(): LabPersisted | null {
  try {
    const raw = localStorage.getItem(LAB_STORAGE);
    if (!raw) return null;
    const p = JSON.parse(raw) as LabPersisted;
    if (!asNetwork(p.network)) return null;
    return p;
  } catch {
    return null;
  }
}

function writeLab(p: LabPersisted): void {
  try {
    localStorage.setItem(LAB_STORAGE, JSON.stringify(p));
  } catch {
    /* ignore quota / private mode */
  }
}

function readBoot(): LabBoot {
  const saved = loadLab();
  const network = asNetwork(saved?.network) ?? "regtest";
  const ep = endpointsFor(network, saved);
  return {
    network,
    proxyUrl:
      saved?.proxyUrl?.trim() || (ep.transportKind === "pipe" ? LOCAL_LWD_PIPE : NATIVE_BRIDGE_URL),
    unlockPolicy: asUnlock(saved?.unlockPolicy),
    autoShield: !!saved?.autoShield,
    lwdUrl: ep.lwdUrl,
    rpcUrl: ep.rpcUrl,
    transportKind: ep.transportKind,
    birthday: ep.birthday || saved?.birthday || (network === "regtest" ? "1" : ""),
    birthdayDate: ep.birthdayDate || saved?.birthdayDate || "",
  };
}

function loadBridgeToken(proxyUrl: string): string {
  try {
    const stored = localStorage.getItem(BRIDGE_TOKEN_STORAGE);
    if (stored?.trim()) return stored.trim();
  } catch {
    /* ignore */
  }
  return splitProxyAuth(proxyUrl).token ?? "";
}

export function App() {
  const [boot] = useState(readBoot);
  const [runtimeOptions] = useState(() => {
    const params = new URLSearchParams(location.search);
    const height = (name: string) => {
      const value = params.get(name);
      return value === null ? undefined : Number(value);
    };
    return {
      threads: Number(params.get("threads")) || undefined,
      regtestNu63Height: height("regtestNu63"),
      regtestNu7Height: height("regtestNu7"),
    };
  });
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [network, setNetwork] = useState<Network>(boot.network);
  const [transportKind, setTransportKind] = useState<TransportKind>(boot.transportKind);
  const [proxyUrl, setProxyUrl] = useState(boot.proxyUrl);
  const [bridgeToken, setBridgeToken] = useState(() => loadBridgeToken(boot.proxyUrl));
  const [lwdUrl, setLwdUrl] = useState(boot.lwdUrl);
  const [rpcUrl, setRpcUrl] = useState(boot.rpcUrl);
  const [probeNote, setProbeNote] = useState<string | null>(null);
  const [probeBusy, setProbeBusy] = useState(false);
  const [lightOk, setLightOk] = useState(false);
  const [tip, setTip] = useState(0);
  const [restore, setRestore] = useState("");
  const [restoreMode, setRestoreMode] = useState<RestoreMode>("seed");
  const [spendSeed, setSpendSeed] = useState("");
  const [birthday, setBirthday] = useState(boot.birthday);
  const [birthdayDate, setBirthdayDate] = useState(boot.birthdayDate);
  const [unlockPolicy, setUnlockPolicy] = useState<UnlockPolicy>(boot.unlockPolicy);
  const [freshSeed, setFreshSeed] = useState<string | null>(null);
  const [copied, setCopied] = useState<"ua" | "zip" | "ufvk" | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [catchupBusy, setCatchupBusy] = useState(false);
  const [wallet, setWallet] = useState<WalletSnapshot | null>(null);
  const [txHistory, setTxHistory] = useState<HistoryEntry[]>([]);
  const [historyFilter, setHistoryFilter] = useState<HistoryFilter>("all");
  const [progress, setProgress] = useState<WasmProgress | null>(null);
  const [sendTo, setSendTo] = useState("");
  const [sendAmt, setSendAmt] = useState("");
  const [sendMemo, setSendMemo] = useState("");
  const [zipPayCount, setZipPayCount] = useState(0);
  const [feeEst, setFeeEst] = useState<FeeEstimate | null>(null);
  const [feeNote, setFeeNote] = useState<string | null>(null);
  const [maxInfo, setMaxInfo] = useState<MaxSend | null>(null);
  const [toInspect, setToInspect] = useState<InspectedAddress | null>(null);
  const [uaSet, setUaSet] = useState<UaReceiverSet>(DEFAULT_UA_RECEIVER_SET);
  const [derivedReceive, setDerivedReceive] = useState<string | null>(null);
  const [receiveInspect, setReceiveInspect] = useState<InspectedAddress | null>(null);
  const [canUaSets, setCanUaSets] = useState(false);
  const [autoShield, setAutoShield] = useState(boot.autoShield);
  const [seedPass, setSeedPass] = useState("");
  const [nativeUp, setNativeUp] = useState(false);
  const [epoch, setEpoch] = useState(0);
  const [runtime, setRuntime] = useState<WasmRuntime | null>(null);
  const [hasSeedEnc, setHasSeedEnc] = useState(false);
  const [hasPasskey, setHasPasskey] = useState(false);
  const [pkInfo, setPkInfo] = useState<PasskeyInfo | null>(null);
  const [protectPasskey, setProtectPasskey] = useState(false);
  const [localhostPasskeyUrl, setLocalhostPasskeyUrl] = useState<string | undefined>();
  const [passkeyOk, setPasskeyOk] = useState(false);
  const [overlay, setOverlay] = useState<PendingSpend | null>(null);
  const [overlayHidden, setOverlayHidden] = useState(false);
  const [benchOpen, setBenchOpen] = useState(true);
  const foldedBench = useRef(false);
  const emptyPasskeyAbort = useRef<AbortController | null>(null);

  const transport = useMemo(() => {
    if (transportKind === "grpc-web") return grpcWebTransport(lwdUrl);
    if (transportKind === "pipe") {
      return httpLwdTransport(proxyUrl || LOCAL_LWD_PIPE, network, lwdUrl, rpcUrl || undefined);
    }
    return httpLwdTransport(
      proxyUrl,
      network,
      lwdUrl,
      rpcUrl || undefined,
      bridgeToken || undefined,
    );
  }, [transportKind, proxyUrl, lwdUrl, network, rpcUrl, bridgeToken]);

  useEffect(() => {
    if (looksLikeLwdPipe(lwdUrl)) setTransportKind("pipe");
    else if (looksLikeGrpcWeb(lwdUrl) && transportKind !== "pipe") setTransportKind("grpc-web");
  }, [lwdUrl, transportKind]);

  const client: EngineClient = useMemo(
    () =>
      createWasmClient(
        {
          network,
          transport,
          lightUrl: lwdUrl,
          allowDeepSync: transportKind === "pipe",
          autoShield,
          unlockPolicy,
        },
        (p) => {
          setProgress(p);
        },
      ),
    [network, transport, transportKind, lwdUrl, epoch, autoShield, unlockPolicy],
  );

  useEffect(() => {
    const offSync = client.on("sync", (e) => {
      setProgress((cur) => syncEventToProgress(e, cur));
    });
    const offBal = client.on("balance", (e) => {
      setWallet((cur) => applyBalanceEvent(cur, e));
    });
    return () => {
      offSync();
      offBal();
    };
  }, [client]);

  useEffect(() => {
    if (!ready) return;
    setCanUaSets(canDeriveUaReceiverSet());
  }, [ready]);

  const localLight = usesFastSync(lwdUrl);
  const grpcWeb = transportKind === "grpc-web";
  const lwdPipe = transportKind === "pipe";
  const liveTip = tip > 0 ? tip : typicalTip(network);
  const birthdayPreview = useMemo(() => {
    const etaOpts = { grpcWeb, lwdPipe };
    const birthdayNetwork = { network, regtestNu7Height: runtimeOptions.regtestNu7Height };
    try {
      if (birthdayDate.trim()) {
        const h = parseBirthdayInput(birthdayDate.trim(), liveTip, birthdayNetwork);
        return {
          height: h,
          date: birthdayDate.trim(),
          eta: syncEta(h, liveTip, localLight, etaOpts).human,
        };
      }
      if (Number(birthday) > 0) {
        const h = Number(birthday);
        return {
          height: h,
          date: dateFromHeight(h, liveTip, birthdayNetwork),
          eta: syncEta(h, liveTip, localLight, etaOpts).human,
        };
      }
      const h = Math.max(1, liveTip - 100);
      return {
        height: h,
        date: dateFromHeight(h, liveTip, birthdayNetwork),
        eta: syncEta(h, liveTip, localLight, etaOpts).human,
      };
    } catch (e) {
      return { error: (e as Error).message };
    }
  }, [birthday, birthdayDate, liveTip, network, runtimeOptions, localLight, grpcWeb, lwdPipe]);

  const birthdayRef = useRef(birthday);
  birthdayRef.current = birthday;
  const birthdayDateRef = useRef(birthdayDate);
  birthdayDateRef.current = birthdayDate;
  const probeGen = useRef(0);
  const reconnectSync = useRef(false);
  const walletLoadGen = useRef(0);
  const syncedAddr = useRef<string | null>(null);
  const catchupRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    void peekWasmWallet().then((w) => {
      if (!cancelled && w) setWallet((cur) => cur ?? w);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setProgress({
      stage: "connecting",
      heading: "Starting",
      message: "loading engine",
      scanned: 0,
      tip: 0,
      notesFound: 0,
      spendsFound: 0,
      percent: 1,
    });
    // `?threads=N` sizes the scan pool for bench runs; the SDK default otherwise.
    // `?regtestNu63=150&regtestNu7=250` matches the optional NU7 fixture.
    initialize({ preferMulticore: true, prewarmProvingKey: false, prewarmProveWorker: true, ...runtimeOptions })
      .then((rt) => {
        if (cancelled) return;
        setRuntime(rt);
        setReady(true);
        void scanWorkerStarting().then(() => {
          const scanner = scanWorkerRuntime();
          if (!cancelled && scanner) setRuntime(scanner);
        });
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [runtimeOptions]);

  useEffect(() => {
    if (!ready) return;
    const gen = ++walletLoadGen.current;
    let cancelled = false;
    setBusy((b) => b ?? "loading wallet…");
    setProgress((p) =>
      p && p.stage !== "synced" && p.stage !== "connecting"
        ? p
        : {
            stage: "connecting",
            heading: "Restoring snapshot",
            message: "reading last balance",
            scanned: p?.scanned ?? 0,
            tip: p?.tip ?? 0,
            notesFound: 0,
            spendsFound: 0,
            percent: Math.max(2, p?.percent ?? 2),
          },
    );
    const loadMs = 12_000;
    const timer = window.setTimeout(() => {
      if (cancelled || gen !== walletLoadGen.current) return;
      setBusy((b) => (b === "loading wallet…" ? null : b));
      setError(
        (cur) =>
          cur ??
          "Restoring snapshot is taking too long. Hide this overlay to see the last snapshot, wipe scan & resync, or forget this device.",
      );
    }, loadMs);
    client
      .getWallet()
      .then(async (w) => {
        if (cancelled || gen !== walletLoadGen.current) return;
        setWallet((cur) => (cur ? keepNoteBalance(cur, w) : w));
        void client
          .history(30)
          .then((rows) => {
            if (cancelled || gen !== walletLoadGen.current) return;
            setTxHistory(rows);
          })
          .catch(() => {});
        if (w.unlockPolicy) setUnlockPolicy(w.unlockPolicy);
      })
      .catch((e: Error) => {
        if (cancelled || gen !== walletLoadGen.current) return;
        if (!/no wasm wallet/i.test(e.message)) setError(formatUiError(e));
      })
      .finally(() => {
        window.clearTimeout(timer);
        if (!cancelled && gen === walletLoadGen.current) {
          setBusy((b) => (b === "loading wallet…" ? null : b));
        }
      });
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [ready, client]);

  useEffect(() => {
    writeLab({
      network,
      proxyUrl,
      birthday,
      birthdayDate,
      unlockPolicy,
      autoShield,
      byNetwork: {
        ...loadLab()?.byNetwork,
        [network]: { lwdUrl, rpcUrl, transportKind, birthday, birthdayDate },
      },
    });
  }, [
    network,
    proxyUrl,
    birthday,
    birthdayDate,
    unlockPolicy,
    autoShield,
    lwdUrl,
    rpcUrl,
    transportKind,
  ]);

  useEffect(() => {
    const fromUrl = splitProxyAuth(proxyUrl).token;
    if (fromUrl) setBridgeToken(fromUrl);
  }, [proxyUrl]);

  useEffect(() => {
    try {
      if (bridgeToken.trim()) localStorage.setItem(BRIDGE_TOKEN_STORAGE, bridgeToken.trim());
      else localStorage.removeItem(BRIDGE_TOKEN_STORAGE);
    } catch {
      /* ignore */
    }
  }, [bridgeToken]);

  useEffect(() => {
    let cancelled = false;
    probeEngine(proxyUrl, { token: bridgeToken.trim() || undefined }).then((h) => {
      if (!cancelled) setNativeUp(!!h?.ok);
    });
    hasEncryptedSeed()
      .then((v) => {
        if (!cancelled) setHasSeedEnc(v);
      })
      .catch(() => {});
    hasPasskeySeed()
      .then((v) => {
        if (!cancelled) setHasPasskey(v);
      })
      .catch(() => {});
    passkeyInfo()
      .then((v) => {
        if (!cancelled) setPkInfo(v);
      })
      .catch(() => {});
    passkeyCapabilities()
      .then((c) => {
        if (cancelled) return;
        setPasskeyOk(!!c.rpId && c.webauthn && c.secureContext);
        setLocalhostPasskeyUrl(c.localhostUrl);
        setProtectPasskey(!!c.rpId && c.webauthn && c.secureContext);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [proxyUrl, bridgeToken]);

  const askedAlways = useRef(false);
  useEffect(() => {
    if (askedAlways.current) return;
    if (
      wallet &&
      !client.hasSpendingSeed() &&
      (hasSeedEnc || hasPasskey) &&
      unlockPolicy === "always"
    ) {
      askedAlways.current = true;
      setOverlay("session");
    }
  }, [wallet?.unifiedAddress, hasSeedEnc, hasPasskey, unlockPolicy, client]);

  const refreshPasskey = useCallback(async () => {
    const gen = walletLoadGen.current;
    try {
      const [hp, info] = await Promise.all([hasPasskeySeed(), passkeyInfo()]);
      if (gen !== walletLoadGen.current) return;
      setHasPasskey(hp);
      setPkInfo(info);
    } catch {
      if (gen !== walletLoadGen.current) return;
      setHasPasskey(false);
      setPkInfo(null);
    }
  }, []);

  const rememberSeed = useCallback(
    (mnemonic: string) => {
      if (unlockPolicy === "each-spend") {
        setWasmSpendingSeed("");
        return;
      }
      setSpendSeed(mnemonic);
      setWasmSpendingSeed(mnemonic);
    },
    [unlockPolicy],
  );

  const refreshHistory = useCallback(async () => {
    const gen = walletLoadGen.current;
    try {
      const rows = await client.history(30);
      if (gen === walletLoadGen.current) setTxHistory(rows);
    } catch {
      if (gen === walletLoadGen.current) setTxHistory([]);
    }
  }, [client]);

  // Fill the ledger while a long scan runs instead of only when it returns.
  // A history read waits for at most one compact-block batch in the worker.
  const ledgerSeen = useRef({ found: 0, at: 0 });
  useEffect(
    () =>
      client.on("sync", (e) => {
        const found = (e.notesFound ?? 0) + (e.spendsFound ?? 0);
        const now = Date.now();
        if (found === ledgerSeen.current.found || now - ledgerSeen.current.at < 3_000) return;
        ledgerSeen.current = { found, at: now };
        void refreshHistory();
      }),
    [client, refreshHistory],
  );

  const runGen = useRef(0);
  const run = useCallback(
    async (label: string, fn: () => Promise<WalletSnapshot | void>) => {
      const gen = ++runGen.current;
      setError(null);
      setBusy(label);
      try {
        const w = await fn();
        if (gen !== runGen.current) return;
        if (w) {
          setWallet(w);
          if (w.mnemonic) {
            setFreshSeed(w.mnemonic);
            rememberSeed(w.mnemonic);
          }
        }
        await refreshHistory();
        await refreshPasskey();
        if (gen !== runGen.current) return;
        if (
          unlockPolicy === "each-spend" &&
          (label.startsWith("send") || label.startsWith("shield"))
        ) {
          setSpendSeed("");
          setWasmSpendingSeed("");
        }
      } catch (e) {
        if (gen !== runGen.current) return;
        const msg = formatUiError(e);
        if (msg !== "sync cancelled" && !/scan worker restarted/i.test(msg)) {
          setError(msg);
          if (WalletError.fromUnknown(e).code === "broadcast_failed") {
            try {
              const pendingWallet = await client.getWallet();
              if (gen !== runGen.current) return;
              setWallet(pendingWallet);
              await refreshHistory();
            } catch { /* Keep the submission error visible if refresh also fails. */ }
          }
          if (isTreeConflictError(msg)) setOverlayHidden(false);
        }
      } finally {
        if (gen === runGen.current) setBusy(null);
      }
    },
    [client, rememberSeed, refreshHistory, refreshPasskey, unlockPolicy],
  );

  const probeLight = useCallback(
    async (opts?: { fillBirthday?: boolean; silent?: boolean }) => {
      const gen = ++probeGen.current;
      setProbeBusy(true);
      if (!opts?.silent) setError(null);
      try {
        const p = await client.probeSetup(network, lwdUrl, rpcUrl || undefined);
        if (gen !== probeGen.current) return;
        if (p.light.tip) {
          setTip(p.light.tip);
          if (
            opts?.fillBirthday &&
            !birthdayRef.current.trim() &&
            !birthdayDateRef.current.trim() &&
            network !== "regtest"
          ) {
            setBirthday(String(Math.max(1, p.light.tip - 100)));
          }
        }
        let zakura = p.validator.error || "";
        if (transportKind === "proxy" && nativeUp && bridgeToken.trim()) {
          const n = await createEngineClient(proxyUrl, {
            token: bridgeToken.trim(),
          }).probeSetup(network, lwdUrl, rpcUrl || undefined);
          if (gen !== probeGen.current) return;
          if (n.validator.ok && !rpcUrl.trim()) setRpcUrl(n.validator.url);
          zakura = `Zakura ${n.validator.ok ? "ok" : "down"} ${n.validator.chain || "?"} h ${n.validator.height ?? "—"} ${n.validator.subversion || n.validator.error || ""}`;
        }
        setLightOk(!!p.light.ok);
        setProbeNote(
          `Zaino ${p.light.ok ? "ok" : "down"} ${p.light.chain || "?"} tip ${p.light.tip ?? "—"} via ${transport.kind}${p.light.error ? ` (${p.light.error})` : ""}${zakura ? ` · ${zakura}` : ""}`,
        );
        if (!p.light.ok && !opts?.silent) {
          setError(p.light.error || "light probe failed");
        }
      } catch (e) {
        if (gen !== probeGen.current) return;
        setLightOk(false);
        const msg = (e as Error).message;
        setProbeNote(`probe failed via ${transport.kind}: ${msg}`);
        if (!opts?.silent) setError(formatUiError(e));
      } finally {
        if (gen === probeGen.current) setProbeBusy(false);
      }
    },
    [
      client,
      network,
      lwdUrl,
      rpcUrl,
      transport.kind,
      transportKind,
      nativeUp,
      bridgeToken,
      proxyUrl,
    ],
  );

  useEffect(() => {
    const t = window.setTimeout(() => {
      void probeLight({ silent: true });
    }, 150);
    return () => window.clearTimeout(t);
  }, [lwdUrl, rpcUrl, transportKind, network, probeLight]);

  useEffect(() => {
    const key = wallet?.unifiedAddress ?? null;
    if (key !== syncedAddr.current) {
      syncedAddr.current = key;
      reconnectSync.current = false;
    }
  }, [wallet?.unifiedAddress]);

  useEffect(() => {
    if (!ready || !wallet || busy || probeBusy || !lightOk || reconnectSync.current) return;
    if (wallet.network && wallet.network !== network) return;
    if (catchupRef.current) return;
    reconnectSync.current = true;
    catchupRef.current = true;
    setCatchupBusy(true);
    client
      .sync()
      .then(async (next) => {
        setWallet(next);
        await refreshHistory();
      })
      .catch((e) => {
        const msg = formatUiError(e);
        if (msg !== "sync cancelled" && !/scan worker restarted/i.test(msg)) {
          setError(msg);
          if (isTreeConflictError(msg)) setOverlayHidden(false);
        }
      })
      .finally(() => {
        catchupRef.current = false;
        setCatchupBusy(false);
      });
  }, [ready, wallet, busy, probeBusy, lightOk, client, network, refreshHistory]);

  useEffect(() => {
    if (wallet) {
      if (!foldedBench.current) {
        foldedBench.current = true;
        setBenchOpen(false);
      }
    } else {
      foldedBench.current = false;
      setBenchOpen(true);
    }
  }, [wallet]);

  useEffect(() => {
    if (
      busy === "syncing…" ||
      busy === "rescanning…" ||
      busy === "restoring…" ||
      busy === "creating…"
    ) {
      setOverlayHidden(false);
    }
  }, [busy]);

  const walletRef = useRef(wallet);
  walletRef.current = wallet;
  const tipRef = useRef(tip);
  tipRef.current = tip;
  useEffect(() => {
    if (!wallet || busy) return;
    if (wallet.network && wallet.network !== network) return;
    const t = window.setInterval(() => {
      if (catchupRef.current) return;
      const w = walletRef.current;
      if (w?.network && w.network !== network) return;
      const chainTip = tipRef.current;
      const scanned = w?.scannedHeight ?? 0;
      if (chainTip > 0 && scanned > 0 && chainTip - scanned > 400) return;
      catchupRef.current = true;
      setCatchupBusy(true);
      client
        .sync()
        .then(async (next) => {
          setWallet(next);
          await refreshHistory();
        })
        .catch((e) => {
          const msg = formatUiError(e);
          if (isTreeConflictError(msg)) {
            setError(msg);
            setOverlayHidden(false);
          }
        })
        .finally(() => {
          catchupRef.current = false;
          setCatchupBusy(false);
        });
    }, 15_000);
    return () => window.clearInterval(t);
  }, [wallet?.unifiedAddress, client, busy, refreshHistory, network]);

  useEffect(() => {
    if (!wallet?.ufvk || !wallet.network || uaSet === DEFAULT_UA_RECEIVER_SET) {
      setDerivedReceive(null);
      return;
    }
    try {
      setDerivedReceive(unifiedAddressForSet(wallet.ufvk, wallet.network as Network, uaSet));
    } catch {
      setDerivedReceive(null);
    }
  }, [wallet?.ufvk, wallet?.network, uaSet]);

  useEffect(() => {
    const addr = derivedReceive || wallet?.unifiedAddress;
    if (!addr) {
      setReceiveInspect(null);
      return;
    }
    let cancelled = false;
    void client
      .inspectAddress(addr)
      .then((a) => {
        if (!cancelled) setReceiveInspect(a);
      })
      .catch(() => {
        if (!cancelled) setReceiveInspect(null);
      });
    return () => {
      cancelled = true;
    };
  }, [client, derivedReceive, wallet?.unifiedAddress]);

  useEffect(() => {
    const dest = sendTo.trim();
    if (!dest || !wallet) {
      setToInspect(null);
      setFeeEst(null);
      setFeeNote(null);
      return;
    }
    let cancelled = false;
    const t = window.setTimeout(() => {
      void client
        .inspectAddress(inspectTarget(dest))
        .then((a) => {
          if (!cancelled) setToInspect(a);
        })
        .catch(() => {
          if (!cancelled) setToInspect(null);
        });
      const zip = dest.toLowerCase().startsWith("zcash:");
      if (!zip && !sendAmt.trim()) {
        setFeeEst(null);
        setFeeNote(null);
        return;
      }
      void client
        .estimateFee(dest, sendAmt.trim() || undefined, sendMemo.trim() || undefined)
        .then((f) => {
          if (cancelled) return;
          setFeeEst(f);
          setFeeNote(null);
        })
        .catch((e) => {
          if (cancelled) return;
          setFeeEst(null);
          setFeeNote(formatUiError(e));
        });
    }, 350);
    return () => {
      cancelled = true;
      window.clearTimeout(t);
    };
  }, [client, wallet, sendTo, sendAmt, sendMemo]);

  const receiveUa = derivedReceive || wallet?.unifiedAddress || "";
  const requestUri = receiveUa ? zip321Uri(receiveUa) : "";
  const birthdayInput = (): number | string | undefined => {
    if (birthdayDate.trim()) return birthdayDate.trim();
    if (Number(birthday) > 0) return Number(birthday);
    return undefined;
  };
  const classified = classifyHistoryList(txHistory);
  const visibleHistory = classified.filter((row) => {
    if (historyFilter === "all") return true;
    if (historyFilter === "shielding") return row.action === "shielding" || row.action === "deshielding";
    // A swap requires verified swap protocol metadata; a memo or two nearby
    // payments alone is not enough to label one.
    if (historyFilter === "swaps") return false;
    return row.action === historyFilter;
  });
  const restoreWords = wordCount(restore);
  const viewOnly = !!wallet?.viewOnly && !client.hasSpendingSeed();
  const netMismatch = !!wallet?.network && wallet.network !== network;
  const emptyCompletedScan = !!wallet && !netMismatch && classified.length === 0 &&
    (wallet.balance.totalAvailable ?? 0) === 0 &&
    (wallet.balance.totalPending ?? 0) === 0 &&
    progress?.stage === "synced" &&
    tip > 0 && (wallet.scannedHeight ?? 0) >= tip - 2;
  const walletLocked = !!busy || netMismatch;
  // The snapshot's height moves when a sync returns; show the scan's progress
  // meanwhile. A restore's scan runs as a background catch-up after `busy`
  // clears, and prefetch ticks report "downloading" with the same scan height.
  // Memo reading ("enhancing") follows the scan with its final height; without
  // it the header fell back to the restore-time snapshot for about a second.
  const shownScanned = Math.max(
    wallet?.scannedHeight ?? 0,
    (busy || catchupBusy) &&
      (progress?.stage === "scanning" ||
        progress?.stage === "downloading" ||
        progress?.stage === "enhancing")
      ? progress.scanned
      : 0,
  );
  const wipeLocked =
    netMismatch ||
    busy === "sending…" ||
    busy === "shielding…" ||
    busy === "creating…" ||
    busy === "restoring…" ||
    busy === "rescanning…";
  const birthdayLocked = busy === "creating…" || busy === "restoring…";
  const bootBusy = !ready || (busy === "loading wallet…" && !wallet);
  const treeConflict = !!error && isTreeConflictError(error);
  const overlayProgress =
    progress ??
    (bootBusy
      ? {
          stage: "connecting" as const,
          heading: !ready ? "Starting" : "Restoring snapshot",
          message: !ready ? "loading engine" : "reading last balance",
          scanned: wallet?.scannedHeight ?? 0,
          tip,
          notesFound: 0,
          spendsFound: 0,
          percent: !ready ? 1 : 8,
        }
      : treeConflict
        ? {
            stage: "scanning" as const,
            heading: "Commitment tree conflict",
            message: error ?? "",
            scanned: wallet?.scannedHeight ?? 0,
            tip,
            notesFound: 0,
            spendsFound: 0,
            percent: 0,
          }
        : null);
  const overlayRemaining = overlayProgress
    ? Math.max(0, (overlayProgress.tip ?? 0) - (overlayProgress.scanned ?? 0))
    : 0;
  const followOnCatchup =
    !treeConflict &&
    catchupBusy &&
    !bootBusy &&
    busy !== "syncing…" &&
    busy !== "rescanning…" &&
    busy !== "restoring…" &&
    busy !== "creating…" &&
    overlayRemaining <= 32;
  const showSync =
    (!overlayHidden || treeConflict) &&
    !!overlayProgress &&
    overlayProgress.stage !== "synced" &&
    !followOnCatchup &&
    (bootBusy ||
      busy === "syncing…" ||
      busy === "rescanning…" ||
      busy === "restoring…" ||
      busy === "creating…" ||
      catchupBusy ||
      treeConflict);

  function needsUnlock(): boolean {
    if (unlockPolicy === "each-spend") return true;
    if (viewOnly) return true;
    return !client.hasSpendingSeed();
  }

  function requestSpend(kind: PendingSpend) {
    if (netMismatch) return;
    if (needsUnlock()) {
      setOverlay(kind);
      return;
    }
    if (kind === "shield") run("shielding…", () => client.shield(1));
    else if (kind === "send") {
      run("sending…", () =>
        client.send(sendTo.trim(), sendAmt.trim(), sendMemo.trim() || undefined),
      );
    }
  }

  async function fillMaxSend() {
    if (walletLocked) return;
    setError(null);
    try {
      const m = await client.maxSend(sendTo.trim() || undefined);
      setMaxInfo(m);
      setSendAmt(m.maxSendZec);
      setFeeEst({ feeZat: m.feeZat, feeZec: m.feeZec });
      setFeeNote(null);
    } catch (e) {
      setFeeEst(null);
      setFeeNote(formatUiError(e));
    }
  }

  async function copyText(text: string, which: "ua" | "zip" | "ufvk") {
    await navigator.clipboard.writeText(text);
    setCopied(which);
    setTimeout(() => setCopied(null), 1200);
  }

  async function finishUnlock(mnemonic: string) {
    setError(null);
    try {
      await client.attachSeed(mnemonic);
      rememberSeed(mnemonic);
      const pending = overlay;
      setOverlay(null);
      setWallet(await client.getWallet());
      if (pending === "shield") await run("shielding…", () => client.shield(1));
      if (pending === "send") {
        await run("sending…", () =>
          client.send(sendTo.trim(), sendAmt.trim(), sendMemo.trim() || undefined),
        );
      }
    } catch (e) {
      setError(formatUiError(e));
    }
  }

  function switchNetwork(n: Network) {
    const snapshot: LabPersisted = {
      network,
      proxyUrl,
      birthday,
      birthdayDate,
      unlockPolicy,
      autoShield,
      byNetwork: {
        ...loadLab()?.byNetwork,
        [network]: { lwdUrl, rpcUrl, transportKind, birthday, birthdayDate },
      },
    };
    const ep = endpointsFor(n, { ...snapshot, network: n });
    const nextProxy = proxyForTransport(ep.transportKind, snapshot.proxyUrl);
    writeLab({ ...snapshot, network: n, proxyUrl: nextProxy });
    setNetwork(n);
    setLwdUrl(ep.lwdUrl);
    setRpcUrl(ep.rpcUrl);
    setTransportKind(ep.transportKind);
    setProxyUrl(nextProxy);
    setBirthday(ep.birthday);
    setBirthdayDate(ep.birthdayDate);
    setLightOk(false);
    reconnectSync.current = false;
  }

  function createWallet() {
    emptyPasskeyAbort.current?.abort();
    walletLoadGen.current += 1;
    void run("creating…", () =>
      client.create(network, birthdayInput(), {
        passphrase: seedPass.trim() || undefined,
        passkey: protectPasskey,
      }),
    );
  }

  async function forgetDevice() {
    if (
      !window.confirm(
        "Forget this device deletes the snapshot on this browser. Passkey stays so Restore from passkey still works. This is not Wipe scan (that keeps the wallet and resyncs from birthday).",
      )
    ) {
      return;
    }
    const gen = ++runGen.current;
    emptyPasskeyAbort.current?.abort();
    walletLoadGen.current += 1;
    cancelWasmSync();
    catchupRef.current = false;
    setCatchupBusy(false);
    setOverlayHidden(false);
    setBusy("forgetting…");
    setFreshSeed(null);
    setSpendSeed("");
    setRestore("");
    setSeedPass("");
    setError(null);
    try {
      await forgetWasmWallet();
      if (gen !== runGen.current) return;
      setEpoch((n) => n + 1);
      setWallet(null);
      setFreshSeed(null);
      setSpendSeed("");
      setTxHistory([]);
      setError(null);
      setHasSeedEnc(false);
      await refreshPasskey();
    } catch (e) {
      if (gen === runGen.current) setError(formatUiError(e));
    } finally {
      if (gen === runGen.current) setBusy(null);
    }
  }

  function hideSyncOverlay() {
    setOverlayHidden(true);
    setBusy((b) => (b === "loading wallet…" ? null : b));
    catchupRef.current = false;
    setCatchupBusy(false);
    cancelWasmSync();
  }

  function wipeResync() {
    if (!window.confirm("Clear notes and scan cache? Sync starts from birthday.")) {
      return;
    }
    cancelWasmSync();
    catchupRef.current = false;
    setCatchupBusy(false);
    setOverlayHidden(false);
    walletLoadGen.current += 1;
    reconnectSync.current = false;
    const gen = walletLoadGen.current;
    void run("rescanning…", async () => {
      const cleared = await client.resetScan();
      try {
        return await client.sync();
      } catch (e) {
        if (gen === walletLoadGen.current) {
          setWallet(cleared);
          setTxHistory([]);
        }
        throw e;
      }
    });
  }

  function restoreWallet() {
    emptyPasskeyAbort.current?.abort();
    walletLoadGen.current += 1;
    void run("restoring…", async () => {
      setFreshSeed(null);
      const seed = restore.trim();
      const bday = birthdayInput() ?? (network === "regtest" ? 1 : undefined);
      const isUfvk = restoreMode === "ufvk" || /^uview/i.test(seed);
      const pass = seedPass.trim() || undefined;
      const w = isUfvk
        ? await client.restoreUfvk(seed, network, bday)
        : await client.restore(seed, network, bday, {
            passphrase: pass,
            passkey: protectPasskey,
          });
      if (isUfvk) {
        setSpendSeed("");
        setWasmSpendingSeed("");
      } else {
        rememberSeed(seed);
      }
      setRestore("");
      return w;
    });
  }

  async function restoreMnemonicFromPasskey(mnemonic: string) {
    walletLoadGen.current += 1;
    await run("restoring…", async () => {
      setFreshSeed(null);
      const bday = birthdayInput() ?? (network === "regtest" ? 1 : undefined);
      const w = await client.restore(mnemonic, network, bday, {
        passphrase: seedPass.trim() || undefined,
      });
      rememberSeed(mnemonic);
      return w;
    });
  }

  async function restoreFromPasskey() {
    const gen = ++walletLoadGen.current;
    emptyPasskeyAbort.current?.abort();
    setError(null);
    setBusy("waiting for passkey…");
    try {
      const mnemonic = await unlockPasskeySeed({ mediation: "required" });
      if (gen !== walletLoadGen.current) return;
      await restoreMnemonicFromPasskey(mnemonic);
    } catch (e) {
      if (!isPasskeyAbort(e)) setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy((b) => (b === "waiting for passkey…" ? null : b));
    }
  }

  async function removePasskeyFromBrowser() {
    if (
      !window.confirm(
        "Remove the encrypted passkey backup from this browser? Your authenticator passkey stays. Keep your recovery phrase or a confirmed portable backup.",
      )
    ) {
      return;
    }
    setError(null);
    try {
      await forgetPasskeySeed();
      await refreshPasskey();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    if (wallet || overlay || !passkeyOk || bootBusy || busy) return;
    const ac = new AbortController();
    emptyPasskeyAbort.current = ac;
    void unlockPasskeySeed({ mediation: "conditional", signal: ac.signal })
      .then((mnemonic) => { if (!ac.signal.aborted) return restoreMnemonicFromPasskey(mnemonic); })
      .catch((e: unknown) => {
        if (isPasskeyAbort(e)) return;
        if (e instanceof DOMException && e.name === "NotSupportedError") return;
        setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      ac.abort();
      if (emptyPasskeyAbort.current === ac) emptyPasskeyAbort.current = null;
    };
    // Empty plate: Bitwarden / Chrome conditional UI. Restore still needs a chooser gesture.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [wallet, overlay, passkeyOk, bootBusy, busy]);

  function openRestore() {
    setBenchOpen(true);
    window.requestAnimationFrame(() => {
      document.getElementById("bench-restore")?.scrollIntoView({ block: "start" });
    });
  }

  const transportLabel =
    transportKind === "pipe" ? "pipe" : transportKind === "grpc-web" ? "gRPC-Web" : "bridge";

  return (
    <div className="shell">
      <header className="top">
        <div className="brand">
          <div className="mark">z-stack</div>
          <div className="policy">Shielded wallet</div>
        </div>
        <div className="top-status">
          <span className={`lamp${lightOk ? " on" : ""}`} aria-hidden="true" />
          <select
            className="net-select"
            value={network}
            onChange={(e) => switchNetwork(e.target.value as Network)}
            disabled={!!busy}
            aria-label="Network"
          >
            <option value="regtest">regtest</option>
            <option value="testnet">testnet</option>
            <option value="mainnet">mainnet</option>
          </select>
          <span>
            {wallet ? `scanned ${shownScanned || "—"}` : "no wallet"}
            {tip ? ` / ${tip}` : ""}
          </span>
          <span>{transportLabel}</span>
        </div>
      </header>

      {showSync && overlayProgress ? (
        <SyncOverlay
          progress={overlayProgress}
          quiet={
            !treeConflict &&
            ((catchupBusy && !busy && !bootBusy) ||
              overlayRemaining <= 32 ||
              ((overlayProgress.heading === "Hashing commitment trees" ||
                /hashing commitment trees/i.test(overlayProgress.message ?? "")) &&
                (wallet?.balance.totalAvailable ?? 0) > 0))
          }
          canWipe={!!(wallet?.ufvk || wallet?.unifiedAddress)}
          onHide={hideSyncOverlay}
          onForget={() => void forgetDevice()}
          onWipeResync={wipeResync}
        />
      ) : null}
      {overlay ? (
        <UnlockOverlay
          kind={
            hasPasskey || passkeyOk
              ? "passkey"
              : hasSeedEnc && overlay === "session"
                ? "passphrase"
                : "seed"
          }
          pending={overlay}
          hasEncrypted={hasSeedEnc}
          hasPasskey={hasPasskey}
          offerPasskey={passkeyOk || hasPasskey}
          passkeyPortable={pkInfo?.portable}
          busy={!!busy}
          onCancel={() => setOverlay(null)}
          onSeed={finishUnlock}
          onError={(msg) => setError(msg)}
          onPassphrase={async (pass) => {
            try {
              const m = await unlockEncryptedSeed(pass);
              await finishUnlock(m);
            } catch (e) {
              setError((e as Error).message);
            }
          }}
        />
      ) : null}

      <div className="layout">
        {freshSeed ? (
          <div className="seed-once">
            <strong>Write these words down. They are not stored in the snapshot.</strong>
            <code>{freshSeed}</code>
            <div className="actions">
              {passkeyOk && !hasPasskey ? (
                <button
                  className="btn ember"
                  type="button"
                  disabled={!!busy}
                  onClick={async () => {
                    setError(null);
                    const r = await tryRegisterPasskeySeed(freshSeed);
                    if (r.status === "ok") {
                      setHasPasskey(true);
                      setPkInfo(r.info);
                    } else if (r.status === "error") {
                      setError(r.message);
                    }
                  }}
                >
                  Save to passkey
                </button>
              ) : null}
              <button className="btn ghost" type="button" onClick={() => setFreshSeed(null)}>
                I copied the words
              </button>
            </div>
            {hasPasskey ? (
              <div className="status ok">
                Encrypted passkey backup saved
                {pkInfo?.portable ? " with a portable copy on your passkey" : " in this browser"}.
                Keep your recovery phrase.
              </div>
            ) : null}
          </div>
        ) : null}

        <div className="plate" aria-label={wallet ? "Wallet" : "Open a wallet"}>
          {wallet ? (
            <>
              <div className="plate-kicker">
                {wallet.network} · orchard
                {viewOnly ? <span className="badge">view-only</span> : null}
                {netMismatch ? <span className="badge warn">network mismatch</span> : null}
              </div>
              <div className="plate-body">
                <div className="plate-copy">
                  <div className="plate-bal">
                    <strong>{wallet.balance.totalZec}</strong>
                    <span>ZEC</span>
                  </div>
                  <div className="plate-pools">
                    orchard {wallet.balance.orchardZec}
                    {(wallet.balance.orchardPending ?? 0) > 0
                      ? ` · pending ${wallet.balance.orchardPendingZec}`
                      : ""}
                    {" · "}t {wallet.balance.transparentZec}
                  </div>
                  {receiveUa ? <div className="ua">{receiveUa}</div> : null}
                  <div className="chips ua-sets">
                    {(["full", "orchard", "shielded"] as const).map((id) => (
                      <button
                        key={id}
                        type="button"
                        className={`chip${uaSet === id ? " on" : ""}`}
                        disabled={!!busy || (id !== "full" && !canUaSets)}
                        title={
                          id === "full"
                            ? "Orchard + sapling + transparent receive. Spend stays shielded."
                            : id === "orchard"
                              ? "Orchard-only unified address"
                              : "Orchard + sapling, no transparent receiver"
                        }
                        onClick={() => setUaSet(id)}
                      >
                        {uaReceiverSetLabel(id)}
                      </button>
                    ))}
                  </div>
                  {receiveInspect ? (
                    <div className="status">
                      Receive {inspectAddressSummary(receiveInspect)}
                      {uaIncludesTransparent(uaSet) ? " · t is receive-only" : ""}
                    </div>
                  ) : null}
                  <div className="meta">
                    <span>scanned {shownScanned || "—"}</span>
                    <span className="meta-actions">
                      <button
                        className="btn ghost"
                        type="button"
                        disabled={wipeLocked}
                        title="Clear notes and scan cache, then sync from birthday. Keys and seed stay."
                        onClick={wipeResync}
                      >
                        {busy === "rescanning…" ? "Rescanning…" : "Wipe scan & resync"}
                      </button>
                      {receiveUa ? (
                        <button
                          className="btn ghost"
                          type="button"
                          onClick={() => copyText(receiveUa, "ua")}
                        >
                          {copied === "ua" ? "Copied" : "Copy UA"}
                        </button>
                      ) : null}
                      {requestUri ? (
                        <button
                          className="btn ghost"
                          type="button"
                          onClick={() => copyText(requestUri, "zip")}
                        >
                          {copied === "zip" ? "Copied" : "Copy ZIP-321"}
                        </button>
                      ) : null}
                      <button
                        className="btn ghost"
                        type="button"
                        disabled={!!busy}
                        onClick={() => run("new address…", () => client.nextAddress())}
                      >
                        Next UA
                      </button>
                      {wallet.ufvk ? (
                        <button
                          className="btn ghost"
                          type="button"
                          onClick={() => copyText(wallet.ufvk!, "ufvk")}
                        >
                          {copied === "ufvk" ? "Copied" : "Copy UFVK"}
                        </button>
                      ) : null}
                    </span>
                  </div>
                  {wallet.transparentAddress && uaIncludesTransparent(uaSet) ? (
                    <div className="edge">
                      Transparent (loopback t-scan only · receive, then shield)
                      <br />
                      {wallet.transparentAddress}
                    </div>
                  ) : null}
                </div>
                {requestUri ? <QrPlate value={requestUri} label="ZIP-321 receive QR" /> : null}
              </div>
              {netMismatch ? (
                <div className="plate-warn">
                  This snapshot is {wallet.network}. The bench is {network}. Sync and spend stay
                  blocked until you switch back or open a {network} wallet.
                  <span className="actions">
                    <button
                      className="btn ember"
                      type="button"
                      disabled={!!busy}
                      onClick={() => {
                        const n = asNetwork(wallet.network);
                        if (n) switchNetwork(n);
                      }}
                    >
                      Switch to {wallet.network}
                    </button>
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={!!busy}
                      onClick={() => void forgetDevice()}
                    >
                      Forget this device
                    </button>
                  </span>
                </div>
              ) : null}
            </>
          ) : (
            <div className="plate-empty">
              <div className="plate-kicker">This device</div>
              <h1>{bootBusy ? "Opening wallet" : "Open a shielded wallet"}</h1>
              <p>
                {bootBusy
                  ? !ready
                    ? "Loading the scanner. Last snapshot appears as soon as IndexedDB answers."
                    : "Restoring the last snapshot from this browser."
                  : "Create a seed here, restore words, or restore from a passkey saved on this site. Forget this device clears the snapshot, not the authenticator. Birthday, network, and the block pipe sit in the bench."}
              </p>
              {passkeyOk ? (
                <input
                  className="webauthn-bait"
                  type="text"
                  name="username"
                  autoComplete="username webauthn"
                  aria-label="Passkey"
                  tabIndex={-1}
                />
              ) : null}
              <div className="actions">
                <button
                  className="btn ember"
                  type="button"
                  disabled={!ready || !!busy}
                  onClick={createWallet}
                >
                  {busy === "creating…" ? "Creating…" : "Create wallet"}
                </button>
                {passkeyOk ? (
                  <button
                    className="btn ghost"
                    type="button"
                    disabled={!ready || !!busy}
                    onClick={() => void restoreFromPasskey()}
                  >
                    {busy === "waiting for passkey…"
                      ? "Waiting for passkey…"
                      : "Restore from passkey"}
                  </button>
                ) : null}
                <button className="btn ghost" type="button" disabled={!!busy} onClick={openRestore}>
                  Restore
                </button>
                <button className="btn ghost" type="button" onClick={() => void forgetDevice()}>
                  Forget this device
                </button>
              </div>
            </div>
          )}
        </div>

        {wallet ? (
          <>
            <div className="toolbar">
              {netMismatch ? (
                <div className="status">Wallet network does not match the bench.</div>
              ) : (
                <>
                  <button
                    className="btn"
                    type="button"
                    disabled={walletLocked}
                    onClick={() => run("syncing…", () => client.sync())}
                  >
                    {busy === "syncing…" ? "Syncing…" : "Sync"}
                  </button>
                  <button
                    className="btn ghost"
                    type="button"
                    disabled={wipeLocked}
                    title="Clear notes and scan cache, then sync from birthday. Keys and seed stay."
                    onClick={wipeResync}
                  >
                    {busy === "rescanning…" ? "Rescanning…" : "Wipe scan & resync"}
                  </button>
                  <button
                    className="btn ember"
                    type="button"
                    disabled={walletLocked}
                    onClick={() => requestSpend("shield")}
                  >
                    Shield
                  </button>
                  {viewOnly ? (
                    <button
                      className="btn ghost"
                      type="button"
                      onClick={() => setOverlay("session")}
                    >
                      Paste seed to send
                    </button>
                  ) : null}
                </>
              )}
              {busy ? (
                <div className="status">{busy}</div>
              ) : probeBusy ? (
                <div className="status">probing…</div>
              ) : null}
            </div>

            {netMismatch ? null : (
              <div className="pay">
                <div className="pay-kicker">Pay</div>
                <div className="send-row">
                  <input
                    value={sendTo}
                    onChange={(e) => {
                      const v = e.target.value;
                      setSendTo(v);
                      if (v.trim().toLowerCase().startsWith("zcash:")) {
                        try {
                          const p = parseZip321(v);
                          if ((p.payments?.length ?? 1) > 1) {
                            setSendAmt("");
                            setSendMemo("");
                            setZipPayCount(p.payments.length);
                          } else {
                            setZipPayCount(0);
                            if (p.amountZec) setSendAmt(p.amountZec);
                            if (p.memo) setSendMemo(p.memo);
                          }
                        } catch {
                          /* keep typing */
                        }
                      } else {
                        setZipPayCount(0);
                      }
                    }}
                    placeholder="unified address or zcash: URI"
                    disabled={walletLocked}
                  />
                  <div className="send-amt">
                    <input
                      value={sendAmt}
                      onChange={(e) => {
                        setSendAmt(e.target.value);
                        setMaxInfo(null);
                      }}
                      placeholder="ZEC"
                      disabled={walletLocked}
                    />
                    <input
                      value={sendMemo}
                      onChange={(e) => setSendMemo(e.target.value)}
                      placeholder="memo"
                      disabled={walletLocked}
                    />
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={walletLocked}
                      onClick={() => void fillMaxSend()}
                    >
                      Max
                    </button>
                    <button
                      className="btn ember"
                      type="button"
                      disabled={
                        walletLocked ||
                        !sendTo.trim() ||
                        (!sendAmt.trim() &&
                          !sendMemo.trim() &&
                          !sendTo.trim().toLowerCase().startsWith("zcash:"))
                      }
                      onClick={() => requestSpend("send")}
                    >
                      Send
                    </button>
                  </div>
                </div>
                {toInspect ? (
                  <div className="status">To {inspectAddressSummary(toInspect)}</div>
                ) : null}
                {feeEst ? (
                  <div className="status ok">
                    Fee {feeEst.feeZec} ZEC
                    {maxInfo ? ` · max ${maxInfo.maxSendZec} ZEC` : ""}
                  </div>
                ) : feeNote ? (
                  <div className="status">{feeNote}</div>
                ) : null}
                {zipPayCount > 1 ? (
                  <div className="status">ZIP-321 multi-pay · {zipPayCount} shielded outputs</div>
                ) : null}
              </div>
            )}

            <div className="ledger">
              <div className="ledger-kicker">Activity</div>
              {classified.length > 0 && (
                <div className="history-filters" aria-label="Filter activity">
                  {(["all", "sent", "received", "shielding", "swaps"] as const).map((filter) => (
                    <button key={filter} type="button" aria-pressed={historyFilter === filter}
                      onClick={() => setHistoryFilter(filter)}>
                      {filter[0].toUpperCase() + filter.slice(1)}
                    </button>
                  ))}
                </div>
              )}
              {classified.length ? (
                <div className="history">
                  {visibleHistory.length === 0 && (
                    <div className="status">{historyFilter === "swaps"
                      ? "No verified swaps in this wallet history."
                      : `No ${historyFilter} transactions in this wallet history.`}</div>
                  )}
                  {visibleHistory.map((e) => {
                    const sign =
                      e.action === "sent" || e.action === "deshielding"
                        ? "−"
                        : e.action === "received"
                          ? "+"
                          : "";
                    return (
                      <div key={e.txid} className={`history-row action-${e.action}`}>
                        <span className="history-action">{e.label}</span>
                        <span>
                          {sign}
                          {e.displayZec} ZEC
                          {e.feeZec ? ` · fee ${e.feeZec}` : ""}
                          {e.status !== "mined" ? ` · ${e.status}` : ""}
                          {e.confirmations ? ` · ${e.confirmations} conf` : ""}
                          {e.blockTime
                            ? ` · ${formatHistoryTime(e.blockTime)}`
                            : e.minedHeight
                              ? ` · #${e.minedHeight}`
                              : ""}
                          {e.memos[0] ? ` · “${e.memos[0]}”` : ""}
                        </span>
                        <code title={e.txid}>{e.txid.slice(0, 16)}…</code>
                      </div>
                    );
                  })}
                </div>
              ) : (
                <div className="status">
                  {emptyCompletedScan
                    ? "No funds or activity found in the scanned range. If this restored wallet previously held funds, its birthday may be too recent. Restore it again using a date before its first transaction; Wipe scan & resync keeps the current birthday."
                    : "No activity yet. Sync to fill this ledger."}
                </div>
              )}
            </div>
          </>
        ) : null}

        {error ? (
          <div className="err">
            <div>{error}</div>
            {isTreeConflictError(error) ? (
              <button className="btn ghost" type="button" onClick={wipeResync}>
                Wipe scan & resync
              </button>
            ) : null}
          </div>
        ) : null}
        {localhostPasskeyUrl ? (
          <div className="status warn-banner">
            Passkeys need <code>localhost</code>, not <code>127.0.0.1</code>. Open{" "}
            <a href={localhostPasskeyUrl}>{localhostPasskeyUrl}</a>
            {protectPasskey ? null : " — this origin cannot register a WebAuthn RP ID."}
          </div>
        ) : null}
        {!ready && !error ? <div className="status">Loading Zakura wasm…</div> : null}

        <details
          className="bench"
          open={benchOpen}
          onToggle={(e) => setBenchOpen((e.target as HTMLDetailsElement).open)}
        >
          <summary>
            <strong>Bench</strong>
            <span className="bench-meta">
              {transportLabel}
              {runtime ? ` · wasm ${runtime.mode} · ${runtime.threads}t` : " · wasm…"}
              {nativeUp
                ? transportKind === "pipe"
                  ? " · pipe up"
                  : transportKind === "proxy"
                    ? " · bridge up"
                    : ""
                : ""}
            </span>
          </summary>
          <div className="bench-body">
            <section className="bench-col">
              <label>
                Network
                <select
                  value={network}
                  onChange={(e) => switchNetwork(e.target.value as Network)}
                  disabled={!!busy}
                >
                  <option value="regtest">regtest</option>
                  <option value="testnet">testnet</option>
                  <option value="mainnet">mainnet</option>
                </select>
              </label>
              <label>
                Block transport
                <select
                  value={transportKind}
                  onChange={(e) => {
                    const v = e.target.value as TransportKind;
                    setTransportKind(v);
                    if (v === "pipe") {
                      setProxyUrl(LOCAL_LWD_PIPE);
                      if (looksLikeGrpcWeb(lwdUrl)) setLwdUrl(localEndpoints(network).light);
                    }
                    if (v === "proxy" && looksLikeLwdPipe(proxyUrl)) setProxyUrl(NATIVE_BRIDGE_URL);
                  }}
                  disabled={!!busy}
                >
                  <option value="pipe">Zaino pipe (native gRPC fan-out)</option>
                  <option value="grpc-web">gRPC-Web (CORS LWD)</option>
                  <option value="proxy">loopback /lwd proxy (z-wallet serve)</option>
                </select>
              </label>
              {transportKind === "pipe" ? (
                <div className="status">
                  One HTTP stream to `z-wallet pipe` on :1239. Compact blocks come from the Zaino
                  that process was started with (`pnpm lwd:pipe` defaults to mainnet `:8138`). Set
                  Light server to native loopback, not gRPC-Web `:1238`.
                </div>
              ) : null}
              {transportKind === "grpc-web" ? (
                <div className="status">
                  gRPC-Web is for CORS CompactTxStreamer (local Zaino often `:1238`, or zec.rocks).
                  Native `:8137` is HTTP/2 — use the loopback `/lwd` proxy (or desktop) for that
                  port.
                </div>
              ) : null}
              {transportKind === "pipe" ? (
                <label>
                  Pipe
                  <input
                    value={proxyUrl}
                    onChange={(e) => setProxyUrl(e.target.value)}
                    placeholder="http://127.0.0.1:1239"
                    disabled={!!busy}
                  />
                </label>
              ) : null}
              {transportKind === "proxy" ? (
                <>
                  <label>
                    Proxy
                    <input
                      value={proxyUrl}
                      onChange={(e) => setProxyUrl(e.target.value)}
                      placeholder="http://127.0.0.1:8787"
                      disabled={!!busy}
                    />
                  </label>
                  <label>
                    Bridge token
                    <input
                      value={bridgeToken}
                      onChange={(e) => setBridgeToken(e.target.value)}
                      placeholder="from z-wallet serve"
                      autoComplete="off"
                      disabled={!!busy}
                    />
                  </label>
                  {!bridgeToken.trim() ? (
                    <div className="status">
                      Token is for `z-wallet serve` on 127.0.0.1:8787, not Zaino. gRPC-Web on :1238
                      needs no token — pick that transport (or set Light server to
                      http://127.0.0.1:1238).
                    </div>
                  ) : null}
                </>
              ) : null}
              <label>
                Light server
                <select
                  value={
                    lwdUrl === "http://127.0.0.1:28137"
                      ? "regtest"
                      : lwdUrl === LOCAL_ZAINO_GRPC_MAINNET
                        ? "local-mainnet"
                        : lwdUrl === LOCAL_ZAINO_GRPC
                          ? "local"
                          : lwdUrl === LOCAL_GRPC_WEB
                            ? "grpcweb"
                            : lwdUrl === "https://testnet.zec.rocks:443"
                              ? "testnet"
                              : lwdUrl === "https://zec.rocks:443"
                                ? "mainnet"
                                : "custom"
                  }
                  onChange={(e) => {
                    const v = e.target.value;
                    if (v === "regtest") setLwdUrl("http://127.0.0.1:28137");
                    else if (v === "local-mainnet") setLwdUrl(LOCAL_ZAINO_GRPC_MAINNET);
                    else if (v === "local") setLwdUrl(LOCAL_ZAINO_GRPC);
                    else if (v === "grpcweb") setLwdUrl(LOCAL_GRPC_WEB);
                    else if (v === "testnet") setLwdUrl("https://testnet.zec.rocks:443");
                    else if (v === "mainnet") setLwdUrl("https://zec.rocks:443");
                  }}
                  disabled={!!busy}
                >
                  <option value="regtest">regtest Zaino (127.0.0.1:28137)</option>
                  <option value="local-mainnet">local mainnet Zaino (127.0.0.1:8138)</option>
                  <option value="local">local testnet Zaino (127.0.0.1:8137)</option>
                  <option value="grpcweb">local gRPC-Web (127.0.0.1:1238)</option>
                  <option value="testnet">testnet.zec.rocks (shield-only)</option>
                  <option value="mainnet">zec.rocks (shield-only)</option>
                  <option value="custom">custom</option>
                </select>
                <input
                  value={lwdUrl}
                  onChange={(e) => setLwdUrl(e.target.value)}
                  disabled={!!busy}
                />
              </label>
              <label>
                Zakura RPC (optional, independent of Zaino)
                <input
                  value={rpcUrl}
                  onChange={(e) => setRpcUrl(e.target.value)}
                  placeholder={LOCAL_ZAKURA_RPC_TESTNET}
                  disabled={!!busy}
                />
              </label>
              <button
                className="btn ghost"
                type="button"
                disabled={probeBusy || !!busy || !ready}
                onClick={() => void probeLight({ fillBirthday: true })}
              >
                {probeBusy ? "Probing…" : "Probe Zaino / Zakura"}
              </button>
              {probeNote ? <div className="status">{probeNote}</div> : null}
              <div className="status">
                {runtime
                  ? `wasm ${runtime.mode} · ${runtime.threads} thread${runtime.threads === 1 ? "" : "s"}${
                      runtime.simd ? " · SIMD128" : ""
                    }${runtime.scanWorker ? " · scan-worker" : ""}${
                      runtime.crossOriginIsolated ? " · COOP/COEP" : ""
                    }`
                  : "wasm loading…"}
              </div>
              <div className="status">
                {transport.utxos
                  ? "t-scan on (loopback Zaino)."
                  : transport.kind === "grpc-web"
                    ? "t-scan off — gRPC-Web is compact blocks only (no GetAddressUtxos)."
                    : transport.kind === "lwd-pipe"
                      ? "t-scan on through the pipe when Light server is loopback."
                      : "t-scan off — public LWD is shield-only."}
              </div>
              {wallet ? (
                <>
                  <div className="section-label">Scan tools</div>
                  <div className="status">
                    Wipe scan keeps keys, seed, and birthday, then trial-decrypts from birthday.
                    Forget this device deletes the snapshot (passkey stays).
                  </div>
                  <div className="actions">
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={wipeLocked}
                      title="Clear notes and scan cache, then sync from birthday. Keys and seed stay."
                      onClick={wipeResync}
                    >
                      {busy === "rescanning…" ? "Rescanning…" : "Wipe scan & resync"}
                    </button>
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={walletLocked || !transport.mine}
                      onClick={() =>
                        run("mining 1…", async () => {
                          if (!transport.mine) throw new Error("mine is loopback-only");
                          await transport.mine(1);
                          return client.sync();
                        })
                      }
                    >
                      Mine 1
                    </button>
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={walletLocked || !transport.mine}
                      onClick={() =>
                        run("mining 100…", async () => {
                          if (!transport.mine) throw new Error("mine is loopback-only");
                          await transport.mine(100);
                          return client.sync();
                        })
                      }
                    >
                      Mine 100
                    </button>
                  </div>
                  {unlockPolicy !== "each-spend" ? (
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={autoShield}
                        onChange={(e) => setAutoShield(e.target.checked)}
                        disabled={!!busy}
                      />
                      auto-shield after sync
                    </label>
                  ) : null}
                </>
              ) : null}
            </section>

            <section className="bench-col">
              <div className="section-label">Birthday</div>
              <label>
                Exact height
                <input
                  value={birthday}
                  onChange={(e) => {
                    setBirthday(e.target.value);
                    setBirthdayDate("");
                  }}
                  placeholder="block height"
                  disabled={birthdayLocked}
                />
              </label>
              <label>
                Or first-used date
                <input
                  value={birthdayDate}
                  onChange={(e) => {
                    setBirthdayDate(e.target.value);
                    setBirthday("");
                  }}
                  placeholder="YYYY-MM-DD"
                  disabled={birthdayLocked}
                />
              </label>
              <div className="chips">
                {([30, 90, 365] as const).map((d) => (
                  <button
                    key={d}
                    className="chip"
                    type="button"
                    disabled={birthdayLocked}
                    onClick={() => {
                      setBirthdayDate(ymdDaysAgo(d));
                      setBirthday("");
                    }}
                  >
                    {d === 365 ? "1y ago" : `${d}d ago`}
                  </button>
                ))}
              </div>
              {"error" in birthdayPreview ? (
                <div className="err">{birthdayPreview.error}</div>
              ) : (
                <div className="status ok">
                  Scan from {birthdayPreview.height} · ~{birthdayPreview.date} (date estimate) ·{" "}
                  {birthdayPreview.eta}
                </div>
              )}

              <div className="section-label">Unlock</div>
              <div className="chips">
                {(
                  [
                    ["session", "Session only"],
                    ["each-spend", "Ask each time"],
                    ["always", "Unlock at load"],
                  ] as const
                ).map(([id, label]) => (
                  <button
                    key={id}
                    type="button"
                    className={`chip${unlockPolicy === id ? " on" : ""}`}
                    disabled={!!busy}
                    onClick={() => {
                      setUnlockPolicy(id);
                      void client.setUnlockPolicy(id);
                    }}
                  >
                    {label}
                  </button>
                ))}
              </div>
              <div className="status">
                {unlockPolicy === "each-spend"
                  ? "Confirm seed or passphrase on every send or shield. Auto-shield is off."
                  : unlockPolicy === "always"
                    ? hasPasskey
                      ? "Unlock with the passkey (or passphrase) when this page loads."
                      : "Unlock from the encrypted seed when this page loads."
                    : "Unlock once for this page. Reload locks the wallet."}
              </div>

              {localhostPasskeyUrl ? (
                <div className="status">
                  Passkeys: use <a href={localhostPasskeyUrl}>{localhostPasskeyUrl}</a>
                </div>
              ) : null}
              <label className="check">
                <input
                  type="checkbox"
                  checked={protectPasskey}
                  onChange={(e) => setProtectPasskey(e.target.checked)}
                  disabled={!!busy || !passkeyOk}
                />
                Protect seed with passkey
                {pkInfo
                  ? ` · ${pkInfo.portable ? "portable encrypted copy" : "local encrypted copy"}`
                  : passkeyOk
                    ? " · requires PRF support"
                    : " · unavailable on this origin"}
              </label>
              <div className="status">
                The encrypted backup stays in this browser unless your passkey confirms a portable
                copy. A synced passkey alone may not restore your wallet. Keep your recovery phrase.
              </div>

              <label>
                Optional passphrase
                <input
                  type="password"
                  value={seedPass}
                  onChange={(e) => setSeedPass(e.target.value)}
                  placeholder="encrypt seed on this device (create / restore)"
                  disabled={!!busy}
                />
              </label>
              <div className="actions">
                <button
                  className="btn ember"
                  type="button"
                  disabled={!ready || !!busy}
                  onClick={createWallet}
                >
                  {busy === "creating…" ? "Creating…" : "Create wallet"}
                </button>
                <button
                  className="btn ghost"
                  type="button"
                  disabled={!!busy}
                  onClick={() => void forgetDevice()}
                >
                  Forget this device
                </button>
                {hasPasskey ? (
                  <button
                    className="btn ghost"
                    type="button"
                    disabled={!!busy}
                    onClick={() => void removePasskeyFromBrowser()}
                  >
                    Remove passkey from this browser
                  </button>
                ) : null}
              </div>
              {wallet ? (
                <div className="actions">
                  <button
                    className="btn ghost"
                    type="button"
                    disabled={
                      !!busy || !seedPass || !(spendSeed.trim() || client.hasSpendingSeed())
                    }
                    onClick={async () => {
                      try {
                        const words = spendSeed.trim();
                        if (!words) throw new Error("unlock or paste the seed first");
                        await persistEncryptedSeed(seedPass, words);
                        setHasSeedEnc(true);
                      } catch (e) {
                        setError((e as Error).message);
                      }
                    }}
                  >
                    Encrypt seed
                  </button>
                  <button
                    className="btn ghost"
                    type="button"
                    disabled={!!busy || !hasSeedEnc}
                    onClick={() => setOverlay("session")}
                  >
                    Unlock seed
                  </button>
                  {hasPasskey && wallet ? (
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={!!busy}
                      onClick={() => setOverlay("session")}
                    >
                      Unlock with passkey
                    </button>
                  ) : null}
                  {wallet && passkeyOk && !hasPasskey ? (
                    <button
                      className="btn ghost"
                      type="button"
                      disabled={!!busy || !(spendSeed.trim() || client.hasSpendingSeed())}
                      onClick={async () => {
                        try {
                          const words = spendSeed.trim();
                          if (!words) throw new Error("unlock or paste the seed first");
                          const r = await tryRegisterPasskeySeed(words);
                          if (r.status === "ok") {
                            setHasPasskey(true);
                            setPkInfo(r.info);
                          } else if (r.status === "error") {
                            setError(r.message);
                          }
                        } catch (e) {
                          setError((e as Error).message);
                        }
                      }}
                    >
                      Save seed to passkey
                    </button>
                  ) : null}
                </div>
              ) : null}

              <div className="section-label" id="bench-restore">
                Restore
              </div>
              {passkeyOk ? (
                <div className="actions">
                  <button
                    className="btn ember"
                    type="button"
                    disabled={!ready || !!busy}
                    onClick={() => void restoreFromPasskey()}
                  >
                    {busy === "waiting for passkey…"
                      ? "Waiting for passkey…"
                      : "Restore from passkey"}
                  </button>
                  <span className="status">
                    {hasPasskey
                      ? "Unlock the encrypted backup saved in this browser."
                      : "Use a passkey with a portable encrypted backup, or restore from your recovery phrase."}
                  </span>
                </div>
              ) : null}
              <div className="chips">
                <button
                  type="button"
                  className={`chip${restoreMode === "seed" ? " on" : ""}`}
                  onClick={() => setRestoreMode("seed")}
                >
                  Restore seed
                </button>
                <button
                  type="button"
                  className={`chip${restoreMode === "ufvk" ? " on" : ""}`}
                  onClick={() => setRestoreMode("ufvk")}
                >
                  Restore view-only
                </button>
              </div>
              <label>
                {restoreMode === "ufvk" ? "Unified full viewing key" : "Mnemonic"}
                <textarea
                  value={restore}
                  onChange={(e) => {
                    const v = e.target.value;
                    setRestore(v);
                    if (/^uview/i.test(v.trim())) setRestoreMode("ufvk");
                  }}
                  placeholder={
                    restoreMode === "ufvk"
                      ? "uview1… + birthday above"
                      : "twelve or twenty four words"
                  }
                />
                {restoreMode === "seed" ? (
                  <span className={restoreWords === 12 || restoreWords === 24 ? "ok" : "status"}>
                    {restoreWords} words
                  </span>
                ) : (
                  <span className="status">UFVK + birthday. Paste the seed later to send.</span>
                )}
              </label>
              <button
                className="btn"
                type="button"
                disabled={
                  !ready ||
                  !restore.trim() ||
                  !!busy ||
                  (restoreMode === "seed" && restoreWords !== 12 && restoreWords !== 24)
                }
                onClick={restoreWallet}
              >
                {busy === "restoring…"
                  ? "Restoring…"
                  : restoreMode === "ufvk"
                    ? "Restore view-only"
                    : "Restore seed"}
              </button>
              {busy === "restoring…" ? (
                <div className="status">
                  Stopping any in-flight scan, then deriving keys. Compact-block download starts
                  after that.
                </div>
              ) : null}
            </section>
          </div>
        </details>
      </div>
    </div>
  );
}
