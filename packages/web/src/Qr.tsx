import { useMemo } from "react";
import { encode } from "uqr";

/** Square QR on the brass plate. Dark modules, no finder-quiet-zone chrome. */
export function QrPlate({ value, label }: { value: string; label: string }) {
  const qr = useMemo(() => encode(value, { ecc: "M", border: 1 }), [value]);
  const size = qr.size;
  const cells: Array<{ x: number; y: number }> = [];
  qr.data.forEach((row, y) => {
    row.forEach((on, x) => {
      if (on) cells.push({ x, y });
    });
  });

  return (
    <svg
      className="qr"
      viewBox={`0 0 ${size} ${size}`}
      role="img"
      aria-label={label}
      shapeRendering="crispEdges"
    >
      <rect width={size} height={size} fill="#ead9b8" />
      {cells.map(({ x, y }) => (
        <rect key={`${x}-${y}`} x={x} y={y} width={1} height={1} fill="#1c1410" />
      ))}
    </svg>
  );
}
