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

## Download the container

The optional `ghcr.io/tzarebczan/z-stack-zaino` image is built from this pinned
source and patch, runs as a non-root user and retains dependency license notices.
[Retained notice sources](licenses/sources.json) record upstream revisions and hashes.
It is tested on Linux x64. Docker Desktop, ARM64 and production deployments are
not covered. The existing Compose fixture remains on its earlier activation schedule.

From a current source checkout, download the digest receipt attached to the
matching SDK release:

```sh
gh release download v0.1.0-alpha.3 --repo tzarebczan/z-stack \
  --pattern z-stack-zaino-image.json --dir artifacts/nu7
gh attestation verify artifacts/nu7/z-stack-zaino-image.json --repo tzarebczan/z-stack
export Z_STACK_NU7_ZAINO_IMAGE=$(node -p "require('./artifacts/nu7/z-stack-zaino-image.json').image")
docker pull "$Z_STACK_NU7_ZAINO_IMAGE"
gh attestation verify "oci://$Z_STACK_NU7_ZAINO_IMAGE" --repo tzarebczan/z-stack
export ZAKURAD=/path/to/verified/zakurad
export Z_STACK_REGTEST_DIR=/path/to/disposable-nu7-chain
export Z_STACK_REGTEST_NU6_3=150
export Z_STACK_REGTEST_NU7=250
node scripts/regtest-native.mjs up
# Run the NU7 acceptance checks, then stop the fixture even if a test fails.
node scripts/regtest-native.mjs down
```

The launcher uses Linux host networking with both servers bound to `127.0.0.1`.
It mounts only the generated read-only Zaino config and this fixture's Zaino data,
drops capabilities and uses a read-only container filesystem. Run it as your
normal user. Changing the image or switching to a native binary requires `down`
first; leave `Z_STACK_NU7_ZAINO_IMAGE` unset when using a binary. Container logs are
available through the `docker logs` command printed at startup.

The image tags are the SDK version and `sha-<full source commit>`, without a
mutable `latest` tag. Use the digest from the receipt for reproducible fixtures.
The [publishing workflow](../../.github/workflows/nu7-container.yml) builds and tests
before publishing, preserves the tested image and signs GitHub build provenance.
First-time package visibility must be set to public before anonymous pulls work.
Maintainers can dispatch it on `main` for an already-published SDK release;
subsequent published releases also trigger it. Existing tags are never overwritten.
If publication stops after pushing tags, preserve them and reconcile their digests
with the tested artifact before preparing a separate recovery change; a blind
rerun is intentionally refused.

## Build from source

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
