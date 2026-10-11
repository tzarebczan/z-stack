# Integration examples

Generate an independent app with the [local archive helper](../docs/GETTING-STARTED.md).
No example uses source aliases or requires an npm publication.

| Template | Demonstrates | Optional services |
| --- | --- | --- |
| [browser-wallet](browser-wallet/README.md) | Plain TypeScript/Vite: recovery, sync, payment review, proving, receipts and lock | None |
| [next-wallet](next-wallet/README.md) | Next App Router/webpack: client lifetime, recovery guard, spending and receipts | None; phrases never go to Next |
| [react-wallet](react-wallet/README.md) | React provider ownership, subscriptions and teardown | None |
| [local-passkey](local-passkey/README.md) | Local PRF-encrypted passkey vault and unsupported-PRF handling | None |
| [remote-backup](remote-backup/README.md) | Application-owned encrypted backup store, verifier and recovery | Example backup server |

All templates use testnet or explicit disposable loopback fixtures. They show
integration contracts. Applications own their UI and any iframe integration.

The [Base-only wallet](base-wallet/README.md) uses the optional `@z-stack/base` archive; it does not install Zcash packages or WASM.

## Add the gift-card API

Pack the optional package and select it when scaffolding a Zcash wallet template:

```sh
pnpm pack:sdk
pnpm pack:gift-cards
node scripts/create-example.mjs browser-wallet /path/to/gift-wallet --with-gift-cards --install
```

This installs the API dependency. Add create/redeem controls with an encrypted
application vault and explicit payment authorization using the
[gift-card guide](../docs/GIFT-CARDS.md). Ordinary examples remain independent of
gift cards. The same flag works with the Next.js, React, passkey, and backup templates.

## Add Base to an existing wallet example

Choose Base explicitly when scaffolding either complete Zcash wallet:

```sh
pnpm pack:sdk
pnpm pack:base
node scripts/create-example.mjs browser-wallet /path/to/combined-wallet --with-base --install
# Or use next-wallet with the same flag.
```

The generated app checks the recovery phrase against its Zcash wallet before deriving the Base address. One app controller coordinates both payment flows; the Base panel never keeps a signer. It demonstrates native ETH/USDC on Base Sepolia, explicit RPC selection, fee review and durable lost-acknowledgement recovery. The ordinary templates still install only the Zcash packages. See [the Base integration guide](../docs/BASE-WALLET.md) for production boundaries.
