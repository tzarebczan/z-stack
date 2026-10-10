export function element(id: string): HTMLElement;
export function element<T extends HTMLElement>(id: string, constructor: new () => T): T;
export function element(id: string, constructor: new () => HTMLElement = HTMLElement): HTMLElement {
  const node = document.getElementById(id);
  if (!(node instanceof constructor)) throw new Error(`Missing or invalid template element: ${id}`);
  return node;
}

/** Clipboard writes always follow an explicit user action. */
export async function copyText(text: string, output: HTMLElement, secret = false): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    output.textContent = secret
      ? "Phrase copied. Your clipboard now contains your recovery words."
      : "Address copied.";
  } catch {
    output.textContent = secret
      ? "Could not copy. Save the numbered words in order."
      : "Could not copy. Select the address instead.";
  }
}
