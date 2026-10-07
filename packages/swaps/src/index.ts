/** Experimental application adapter contract. No provider implementation is shipped. */

export interface SwapQuoteRequest {
  fromAsset: string;
  toAsset: string;
  amount: string;
  /** Shielded ZEC deposit/withdraw address when applicable */
  zecAddress?: string;
}

export interface SwapAdapter {
  readonly id: string;
  quote(req: SwapQuoteRequest): Promise<unknown>;
}
