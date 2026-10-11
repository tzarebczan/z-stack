# Optional gift cards

`@z-stack/gift-cards` provides a compact bearer-link codec, integer amount helpers,
encrypted recovery-store integration, and explicit funding and redemption flows.
Its root has no runtime dependencies or browser initialization. Native applications
can use those helpers with their own engine adapter. The `/browser` entry uses an
optional matching `@z-stack/sdk` peer and a dedicated gift wallet worker.

The codec supports Vizor v3 links, which current Vizor shares by default. Configure
the exact `https://link.vizor.cash/payment-links/open` endpoint to accept them.
Older v1/v2 links are unsupported. Messages use `Intl.Segmenter` to enforce Vizor's
128-grapheme and 512-byte limits. Labels retain trimmed and empty values within
the 16 KiB link bound. See the integration guide for the separate Vizor recovery
size limit and network compatibility.

## Build and install the optional archive

The published alpha.9 preview predates gift cards. Build a source checkout that
contains this package and pack its matching SDK and gift-card archives first:

```sh
pnpm install --frozen-lockfile
pnpm build:sdk
pnpm pack:sdk
pnpm pack:gift-cards
```

Packages are local preview archives, not npm registry packages. A future published
preview must explicitly include the gift-card archive. To use only the pure
helpers, install the archive produced above:

```sh
npm install /path/to/z-stack-gift-cards-0.1.0-alpha.9.tgz
```

For browser wallets, also install the same checkout's SDK archive. The SDK includes its
core/passkey helpers and production WASM engines:

```sh
npm install /path/to/z-stack-sdk-0.1.0-alpha.9.tgz \
  /path/to/z-stack-gift-cards-0.1.0-alpha.9.tgz
```

```ts
import { parseGiftLink, giftFundingAmount } from "@z-stack/gift-cards";

const card = parseGiftLink(incomingLink, {
  network: "testnet",
  endpoints: ["https://wallet.example/gift"],
});
const depositZat = giftFundingAmount(card);
```

`incomingLink` comes from the application's explicit intake step. The parsed
entropy and the complete link permit spending. Keep both out of logs, analytics,
server requests, query strings, and public state.

See [the integration guide](https://github.com/tzarebczan/z-stack/blob/main/docs/GIFT-CARDS.md)
for browser configuration, funding review, authorization, recovery, and native
adapter boundaries. The ordinary SDK never imports this package automatically.

## Payment and recovery behavior

The gift recipient is an independent account. Its advertised amount is display
metadata; only scanned wallet evidence establishes available funds. Creation funds
the face amount plus a 10,000-zatoshi claim reserve. The actual funding fee is
additional. Redemption sends the reviewed face amount, and waits for six scanned
funding confirmations and enough confirmed funds to cover the engine's actual fee.

Initial browser funding requires a software source wallet because the SDK's
fee-capped send rejects hardware sources. Redemption can pay a hardware or
viewing-only destination wallet; the independent gift account signs the payment.
The initial browser adapter accepts same-network Unified Addresses with an
Orchard receiver for redemption. Native adapters can apply their own supported
shielded receiver policy.

Anyone holding a link, including its creator, can spend its funds. There is no
off-chain revocation. The application saves the bearer in an encrypted recovery
store before funding. Unknown submission outcomes keep their existing payment
attempt and wallet snapshot; checking them never constructs a replacement payment.
Retain gift secrets and databases while recovery evidence remains uncertain.

## Licensing and verification

Original code is Apache-2.0. The archive retains the project's license, notices,
and WASM dependency inventory. It adds no runtime dependency. Browser execution
uses the SDK's existing engine and legal inventory.

From a source checkout, run `pnpm build:gift-cards`, `pnpm check:gift-cards`,
`pnpm test:gift-cards`, and `pnpm test:gift-cards:package`. The browser check is
`pnpm test:gift-cards:browser`; install the Playwright browsers first. Tests use
disposable fixture keys and the real packaged engine.
