# @z-stack/base

**Alpha.** Part of the matching `0.1.0-alpha.6` archive release; not published to npm.

Optional Base wallet support. Use the existing recovery phrase to derive a Base
account, read ETH/USDC balances, review and send payments, and integrate your
own paymaster through explicit Simple7702/EntryPoint 0.8 primitives.

The package sends no telemetry. Zcash WASM, passkeys, accounts, cloud backups
and fiat integrations are not required and do not load with it. Apps own
unlocking, opt-in, persistence and provider configuration.
`@z-stack/sdk` does not depend on this package. EVM activity is public.

```ts
import { BASE_SEPOLIA, createBaseWallet, deriveBaseAddress } from '@z-stack/base';

const wallet = createBaseWallet({
  address: deriveBaseAddress(unlockedPhrase),
  network: BASE_SEPOLIA,
  rpcUrl: 'https://your-public-rpc.example',
});
```

[Follow the integration walkthrough](https://github.com/tzarebczan/z-stack/blob/main/docs/BASE-WALLET.md)
for review, fresh unlock, shared signing locks, durable submission reservations,
unknown acknowledgements, privacy and sponsorship adapters. The
[Base-only example](https://github.com/tzarebczan/z-stack/tree/main/examples/base-wallet)
builds from a local archive without a Rust toolchain or npm publication.

Build with `pnpm --filter @z-stack/base build`; pack with `pnpm pack:base`.
The smart-account surface lives at `@z-stack/base/smart-account` and is
experimental. No paymaster or delegation is enabled by default. Live funded
sponsorship requires deployment/provider verification beyond the fixture tests.

Apache-2.0 for original code. viem and its dependencies retain their original
licenses; this package includes its own 13-dependency notice inventory.
