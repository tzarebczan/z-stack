import { SOLANA_USDC } from '@z-stack/solana';
import { z } from 'zod';

const CATALOG = 'https://intents-connect-api.aurora.dev/api/v1/supported_tokens?flow=inOperation';
const catalogToken = z.object({
  assetId: z.string().min(1),
  blockchain: z.string().min(1),
  symbol: z.string().min(1),
  contractAddress: z.string().optional(),
  decimals: z.number().int().nonnegative(),
});
const catalogSchema = z.object({
  result: z.object({ in: z.array(catalogToken), out: z.array(catalogToken) }),
});

/** Discovery only. Listing both assets does not qualify a route or authorize a payment. */
export async function getAuroraRingAssetSupport(options: { fetch?: typeof fetch; signal?: AbortSignal } = {}) {
  const response = await (options.fetch ?? fetch)(CATALOG, {
    signal: options.signal,
    redirect: 'error',
    headers: { accept: 'application/json' },
  });
  if (!response.ok) throw new Error('Aurora Intents Connect asset catalog is unavailable.');
  const { result } = catalogSchema.parse(await response.json());
  const nativeZec = result.in.find(asset =>
    (asset.blockchain === 'zec' || asset.blockchain === 'zcash') && asset.symbol === 'ZEC' && asset.decimals === 8,
  );
  if (!nativeZec) return { kind: 'native-zec-unavailable' as const };

  const usdc = result.out.find(asset =>
    asset.blockchain === 'sol' && asset.contractAddress === SOLANA_USDC.mint && asset.decimals === SOLANA_USDC.decimals,
  );
  if (!usdc) return { kind: 'solana-usdc-unavailable' as const };
  return { kind: 'assets-listed' as const, sourceAssetId: nativeZec.assetId, destinationAssetId: usdc.assetId };
}
