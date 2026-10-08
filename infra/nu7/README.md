# NU7 light-server fixture

This is optional local test infrastructure, not a wallet dependency or a
production deployment. The stock Zaino 0.10.0 fixture rejects the NU7 branch ID.
Zaino 0.10.1 still uses Zebra's placeholder branch ID; the adaptation here uses
published Zakura node and Common crates instead. Use it with Zakura 1.6.0 to
run the [NU7 acceptance checks](../../docs/RELEASE.md#nu7-acceptance).

The patch pins Zaino source commit
`3244a74bb09fa6a09a4b2deeb6be53bab0890747` (release `0.10.1`), switches dependency
aliases, adapts RPC response types and retains the resulting Cargo lockfile.
It changes no cryptographic or consensus implementation. The light server is
built with local unencrypted traffic support; bind it only to loopback.

Use a fresh directory outside this repository. Building this optional server
requires Rust 1.97 or later; the wallet engine's toolchain is unchanged. The
fixture was tested with Rust 1.98.0 on Linux x64.

```sh
git clone https://github.com/zingolabs/zaino.git /path/to/zaino-nu7
cd /path/to/zaino-nu7
git checkout --detach 3244a74bb09fa6a09a4b2deeb6be53bab0890747
git apply --check /path/to/z-stack/infra/nu7/zaino-0.10.1-zakura.patch
git apply /path/to/z-stack/infra/nu7/zaino-0.10.1-zakura.patch
cargo +1.98.0 build --locked -p zainod --features no_tls_use_unencrypted_traffic
export ZAINOD=/path/to/zaino-nu7/target/debug/zainod
```

The patched Cargo.lock SHA-256 is
`ef7a70409530bd0e74a4d901439c2ad2d6ca5d4dce554dc97dac593ebb5b813b`.
Restart the disposable fixture when changing the binary; the launcher does not
replace a running process. Keep the validator and SDK activation schedules equal.
No additional server is required for an app using a compatible hosted gRPC-Web
provider.

Zaino's original [license notice](ZAINO-LICENSE) is retained. Remove this
adaptation when an upstream release supports the published NU7 branch and
Ironwood data without it. Re-run funded native and browser acceptance before
changing these pins. This fixture does not certify other server releases or
public testnet providers.
