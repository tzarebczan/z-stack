# Local passkey example

A wallet with an optional PRF passkey vault. It imports wallet methods from the
root entry and vault methods from `/services`; no registration server, Cloudflare,
Google login, fiat integration or application token is used.

Create a testnet wallet and confirm that you saved its recovery phrase before
the wallet commits. Leaving before confirmation saves no wallet. Then choose a
wallet name and click
Protect with passkey. The chosen name is used in the password manager. PRF is
required; cancellation or an unsupported provider leaves the phrase available.
Both ceremonies begin directly in their click handlers. Unlocked byte buffers
are wiped after the seed is attached; JavaScript strings cannot be reliably wiped.

Reload opens viewing state with spending locked. Unlock uses the local encrypted
vault; lock removes spending access and clears the displayed phrase while activity
remains readable. This is local protection, not cross-device backup. Synced
credentials alone do not copy this IndexedDB record; storage deletion can lose
it. Restore with the recovery phrase using the browser example. A production app
must implement its own recovery/export and explicit replacement flows.

The package check verifies types and the real production build. Physical PRF
providers and their synced/cross-device behavior remain unverified in this example.
The optional browser gate verifies real engine startup, recovery-before-commit,
unconfirmed page teardown and locked reload with only a disposable app-owned
chain fixture. It does not certify a physical passkey provider.

Follow the [build instructions](https://github.com/tzarebczan/z-stack/blob/main/README.md).

Copy this directory outside the workspace, then run:

```sh
npm install /path/to/artifacts/z-stack-sdk-0.1.0-alpha.3.tgz
npm run dev
```

Use the localhost URL printed by Vite. `npm run build` checks types and builds
real WASM and workers. Production hosting needs the [deployment configuration](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#production-host).
Use fresh browser profiles for examples; wallet storage is origin-scoped. Never enter a mainnet seed.
