import qrcode from "qrcode-generator";

/** Encode only the public receive address; no remote QR service or recovery data. */
export function drawReceiveQr(canvas: HTMLCanvasElement, address: string) {
  const context = canvas.getContext("2d");
  if (!context) return;
  const code = qrcode(0, "M");
  code.addData(address, "Byte");
  code.make();
  const count = code.getModuleCount(), cell = 4, margin = 4;
  canvas.width = canvas.height = (count + margin * 2) * cell;
  context.fillStyle = "#fff";
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.fillStyle = "#000";
  for (let row = 0; row < count; row++) for (let col = 0; col < count; col++) {
    if (code.isDark(row, col)) context.fillRect((col + margin) * cell, (row + margin) * cell, cell, cell);
  }
}
