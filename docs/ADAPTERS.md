# Test a custom storage adapter

`@z-stack/sdk/lab` exports `checkWalletStorageAdapter` and
`checkVaultStoreAdapter`. These are opt-in, destructive development helpers under the unstable lab surface.
Never run them against a user's wallet, production backup or shared test account.

```ts
import { checkWalletStorageAdapter } from "@z-stack/sdk/lab";
import { indexedDbWalletStorage } from "@z-stack/sdk";
const name = "my-disposable-adapter-test";
const report = await checkWalletStorageAdapter({
  open: () => indexedDbWalletStorage({ name }),
  dispose: () => deleteTestDatabase(name), // Your fixture's cleanup.
});
```

Every `open()` must connect to the same initially empty, isolated namespace.
Use distinct connections to the same backend to test cross-connection behavior.
`dispose()` runs in `finally`, including on failure. Supply `failNextCommit()`
when your fixture can inject a quota/disk failure at the next write commit.
Read `report.untested` alongside `report.passed`.

The wallet suite checks cloned values, rollback of multi-record writes,
pre-commit cancellation, serialized read callbacks, concurrent compare-and-swap,
readonly enforcement and deletion barriers across reopen. The vault suite checks
create-only writes, concurrent revision conflicts, cloning, cancellation,
forgotten tombstones and separate administrative hard deletion. Its synthetic
records test the storage protocol, not cryptographic decryption.

A failing contract throws `AdapterConformanceError` with a fixed `check` name;
provider errors and record contents are not included. If an adapter never
settles, use your test runner's timeout and process cleanup. These helpers cannot
prove power-loss durability, storage eviction behavior or the case where physical
commit wins a cancellation race. Exercise those with process interruption and
backend fault injection.

Remote adapters need separate authentication/authorization, revocation,
server-rollback and in-flight network tests. These helpers cannot establish
security just by calling `put/get`. The [remote backup example](../examples/remote-backup/README.md)
provides real server-side WebAuthn, ownership and atomic revision checks; its
acceptance tests remain separate from the generic store protocol.

For unlockers, test cancellation, empty/wrong phrases, account switching and
teardown. The SDK receives the secret locally; your unlocker must not send the
UFVK supplied for matching to an account service. See [storage](STORAGE.md)
and [services](SERVICES.md) for the underlying contracts.
