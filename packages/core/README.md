# @z-stack/core

Platform-independent helpers and types for Zcash wallet apps. This package has
no runtime dependencies and does not access the DOM, storage, or the network.
It does not derive keys, scan blocks, or construct transactions.

```ts
import { formatZatoshis, parseZecToZatoshis } from "@z-stack/core";

const amount = parseZecToZatoshis("0.25"); // bigint: 25_000_000n
const display = formatZatoshis(amount);   // "0.25000000"
```

Other exports include ZIP-321 requests, unified-address receiver sets, history
classification, sync progress helpers, `WalletSnapshot`, and `WalletError`.
Classify history when displaying it; the engine can refine a transaction as it
learns more. Do not persist the derived transaction classification.

Wallet apps can import these helpers through `@z-stack/sdk`. Use this standalone
package for UI or server code that does not need the WASM engine.

Alpha archives are built with the matching SDK release. See
[installation and licensing](https://github.com/tzarebczan/z-stack/blob/main/docs/RELEASE.md).
