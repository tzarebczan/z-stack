import assert from 'node:assert/strict';
import test from 'node:test';
import { getAuroraRingAssetSupport } from '../src/index';

const usdc = {
  assetId: 'nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near',
  blockchain: 'sol',
  symbol: 'USDC',
  contractAddress: 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v',
  decimals: 6,
};
const zec = { assetId: 'nep141:zec.omft.near', blockchain: 'zec', symbol: 'ZEC', decimals: 8 };

test('wrapped ZEC entries cannot enable native ZEC Ring delivery', async () => {
  const result = await getAuroraRingAssetSupport({
    fetch: async url => {
      assert.equal(url, 'https://intents-connect-api.aurora.dev/api/v1/supported_tokens?flow=inOperation');
      return Response.json({ result: {
        in: [
          { ...zec, blockchain: 'near', assetId: '1cs_v1:near:nep141:zec.omft.near' },
          { ...zec, blockchain: 'sol', assetId: '1cs_v1:sol:spl:A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS' },
        ],
        out: [usdc],
      } });
    },
  });
  assert.deepEqual(result, { kind: 'native-zec-unavailable' });
});

test('requires the canonical Solana USDC mint and only reports catalog support', async () => {
  const fetchCatalog = (out: unknown[]) => async () => Response.json({ result: { in: [zec], out } });
  assert.deepEqual(await getAuroraRingAssetSupport({
    fetch: fetchCatalog([{ ...usdc, contractAddress: 'other-asset' }]),
  }), { kind: 'solana-usdc-unavailable' });
  assert.deepEqual(await getAuroraRingAssetSupport({ fetch: fetchCatalog([usdc]) }), {
    kind: 'assets-listed',
    sourceAssetId: zec.assetId,
    destinationAssetId: usdc.assetId,
  });
});
