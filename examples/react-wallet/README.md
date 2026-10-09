# React wallet example

React 18 + Vite, with local create/load, sync, retained activity and spending lock.
No accounts or backend are required. Wallet initialization runs in a client effect,
so importing the component for server rendering opens no engine or storage.

The app-owned lifetime queue waits for initialization and `close()` before a new
mount opens an owner. It handles StrictMode setup/cleanup and rapid screen changes;
event listeners and stale results are discarded on unmount. After a close error,
the next mount attempts to open through the SDK, whose ownership guard still
refuses an unreleased client. A rollback storage error reports failure after
cleanup; inspect the reopened wallet before attempting another spend.
Creation displays recovery through optional SDK preparation and waits for explicit
confirmation before committing. Unmount or pagehide before confirmation cancels
creation; acknowledged wallets reopen locked. A page restored from the back/forward
cache acquires a fresh owner. The sample asks for an offline copy; production apps
can implement verified encrypted backup in the same optional hook.
This sample creates wallets; use the plain TypeScript example to try restore.

The external-package check builds this app and server-renders its component under
browser API traps. The optional browser gate runs this production app with real
WASM, StrictMode and repeated remounts without remote requests. An app-owned
chain fixture also exercises recovery confirmation, unconfirmed unmount/reload
and acknowledged locked reopening. It is not a
Next.js or Remix deployment test.

Follow the [build instructions](https://github.com/tzarebczan/z-stack/blob/main/README.md).

Copy this directory outside the workspace, then run:

```sh
npm install /path/to/sdk-alpha/z-stack-sdk-0.1.0-alpha.4.tgz
npm run dev
```

Use the localhost URL printed by Vite. `npm run build` checks types and builds
real WASM and workers. Production hosting needs the [deployment configuration](https://github.com/tzarebczan/z-stack/blob/main/docs/INTEGRATION.md#production-host).
Use fresh browser profiles for examples; wallet storage is origin-scoped. Never enter a mainnet seed.
