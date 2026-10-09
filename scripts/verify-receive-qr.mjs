import assert from 'node:assert/strict';
import jsQR from 'jsqr';

// Decode the rendered canvas with an independent reader, not the encoder.
export async function verifyReceiveQr(page, address) {
  await page.waitForFunction(() => {
    const canvas = document.querySelector('#receive-qr');
    return canvas && canvas.width === canvas.height && canvas.width > 0;
  });
  const pixels = await page.locator('#receive-qr').evaluate(canvas => {
    const {data} = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height);
    return {data: Array.from(data), width: canvas.width, height: canvas.height};
  });
  const decoded = jsQR(new Uint8ClampedArray(pixels.data), pixels.width, pixels.height);
  assert.equal(decoded?.data, address, 'Receive QR must contain exactly the displayed public address');
}
