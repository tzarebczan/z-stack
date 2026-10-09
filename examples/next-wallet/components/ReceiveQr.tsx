"use client";
import { useEffect, useRef } from "react";
import { drawReceiveQr } from "../lib/receive-qr";
export function ReceiveQr({ address }: { address: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => { if (ref.current) drawReceiveQr(ref.current, address); }, [address]);
  return <canvas id="receive-qr" ref={ref} className="receive-qr" role="img" aria-label="Receive address QR code">Use Copy address to receive.</canvas>;
}
