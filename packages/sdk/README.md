# @z-stack/sdk

A Zcash wallet for browser apps, backed by Rust/WebAssembly. Keys, compact-block
scanning, and proofs run on the device. The same package includes a client for
the native loopback engine.

**Alpha:** distributed as matching SDK, core, and passkey archives from a
z-stack checkout. Not currently published to npm; original z-stack
code is Apache-2.0, with retained third-party licenses. See
[release status](https://github.com/tzarebczan/z-stack/blob/main/docs/RELEASE.md).

## Vite

```ts
// vite.config.ts
import { defineConfig } from "vite";
import { zStack } from "@z-stack/sdk/vite";

export default defineConfig({ plugins: [zStack()] });
```

```ts
// Run in browser code, not during server rendering.
import { createWallet } from "@z-stack/sdk";

const wallet = await createWallet({
  network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev",
  memoFetch: "on-demand",
});
const saved = await wallet.load();
// If there is no saved wallet, ask the user to create or restore one.
```

Only one browser client may own the engine at a time. Choose a local storage
namespace with `indexedDbWalletStorage({ name: "my-app-wallet" })`, or supply
a transactional `WalletStorage`. Multiple concurrently active wallets are not
supported; a second owner fails with `busy`.
Register `wallet.on("sync", …)` and `wallet.on("balance", …)` before syncing.
Each registration returns an unsubscribe function. Unsubscribe and call
`await wallet.close()` when your app tears down the wallet screen. It locks,
cancels work, clears subscriptions and releases the owner; saved data stays.

The Vite plugin configures module workers and isolation headers in development
and preview. Production HTML must send COOP/COEP for threaded scanning; the
engine uses one thread when isolation is unavailable. Verify OAuth popups and
third-party embeds with those headers. The
[integration guide](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md)
includes host configuration, setup steps, and a runnable example.

## API boundaries

| Import | Purpose |
| --- | --- |
| `@z-stack/sdk` | Browser lifecycle, sends, amounts, events, transports and local storage |
| `@z-stack/sdk/hardware` | Ledger/Keystone accounts and signers |
| `@z-stack/sdk/native` | Native loopback wallet with its own capability surface |
| `@z-stack/sdk/core` | Pure amounts, history, addresses and errors |
| `@z-stack/sdk/engine` | Unstable raw engine clients for diagnostics/tooling |
| `@z-stack/sdk/services` | Optional account ceremonies and encrypted-vault store/provider interfaces |
| `@z-stack/sdk/diagnostics` | Explicit, payload-free browser deployment checks |
| `@z-stack/sdk/vite` | Vite plugin; imported only by build configuration |
| `@z-stack/sdk/lab` | Unstable benchmarks, diagnostic helpers, and regtest fixtures |

Amounts passed to spending methods are decimal ZEC strings. Use
`parseZecToZatoshis` and `formatZatoshis` at display boundaries. Estimate fees
through the wallet rather than hardcoding them.

Keep the recovery phrase when importing a wallet. New wallet creation returns
`{ recoveryPhrase, wallet }` once. The phrase is separate from wallet state
and event payloads; imports keep their original phrase. For onboarding,
`create({ beforeCommit })` lets your app confirm or encrypt recovery before
the first durable snapshot. Handle its abort signal and your own external-write
rollback; the hook is optional and imposes no account or backup provider. Create/import refuse
to overwrite saved data without an explicit `{ replace: true }`. Encrypted seed storage and a passkey vault do not by themselves
provide an off-device backup. Saving and confirming an encrypted recovery record
on your server is application work.

Network requests disclose your IP address and scan timing. The default memo mode
is `on-demand`, deferring wallet-transaction lookups until `fetchMemos()`.
`auto` explicitly opts into disclosing those transaction IDs during sync. Transparent address
lookups require opt-in for remote servers. Shared memo ranges require a gateway
that explicitly supports `/zstack/memos`; ordinary gRPC-Web servers do not.

An uncertain broadcast (`broadcast_failed`) retains its transaction ID even
after close, forget or replacement; creating another payment can pay twice. Handle error codes
through `WalletError`, never by parsing diagnostic messages.

See the [wallet API and behavior](https://github.com/tzarebczan/z-stack/blob/main/docs/SDK.md),
[security and privacy](https://github.com/tzarebczan/z-stack/blob/main/docs/SECURITY.md),
[passkey vault guide](https://github.com/tzarebczan/z-stack/blob/main/packages/passkey/README.md),
and [upstream dependency ledger](https://github.com/tzarebczan/z-stack/blob/main/docs/UPSTREAM.md).

Optional registration, authentication and encrypted backup adapters are available
through `@z-stack/sdk/services`. See [accounts and backups](https://github.com/tzarebczan/z-stack/blob/main/docs/SERVICES.md) for the boundary
between the local wallet and your server. Your application configures account
and backup providers.

By default, creation/import leaves spending locked. Call `wallet.unlock(words)`
for each spend, or provide a `WalletUnlocker` backed by your own local keystore.
`unlockPolicy: "session"` opts into memory retention until `lock()`/`close()`;
reload always locks. Plaintext seeds are never saved in web storage.
Automatic shielding is off unless explicitly enabled.

Adapter acceptance helpers live under the unstable `@z-stack/sdk/lab` surface. See [adapter checks](https://github.com/tzarebczan/z-stack/blob/main/docs/ADAPTERS.md) for disposable storage fixtures; never run these destructive checks on a user wallet or production backup.
