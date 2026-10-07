# Optional Base wallet

A Base Sepolia wallet using only `@z-stack/base` and viem. No Zcash WASM,
account backend, passkey service, fiat integration, bundler or paymaster is installed.
Use a disposable test phrase. Each payment requires another unlock; this
example never persists or retains the phrase between actions.

From the repository, run `pnpm --filter @z-stack/base build` and `pnpm pack:base`,
then `node scripts/create-example.mjs base-wallet /tmp/my-base-wallet --install`.
In the generated directory, run `npm run dev`.

The UI demonstrates balance reads, ETH/USDC review, fee refresh, a shared Web
Lock and a checked local submission reservation. A lost acknowledgement blocks
new payments. Status checks require finalized canonical receipt data before
clearing that reservation. If no receipt exists, keep the record and investigate;
this example deliberately has no “retry” or “forget” action.

Local storage retains the signed transaction for an exact retry, never recovery phrases. Keep these spending-authority bytes private until broadcast. It
coordinates tabs on this origin only. Production apps must supply durable
cross-device exclusion where they expose the same signer on multiple devices.
HTTPS or localhost and the Web Locks API are required for sending.

[The Base integration guide](https://github.com/tzarebczan/z-stack/blob/main/docs/BASE-WALLET.md) explains app-owned unlock,
provider configuration, optional sponsorship, persistence and privacy limits.
Live funded sponsorship is not demonstrated by this app.
