# Enhance PIR transport-neutral client

`enhance-pir` and its optional `pir-native` path dependency are copied from
wallet-pir commit `24f7694cd4f8274172a11cbff43c158bcab5d500`.
Original licenses are retained. `UPSTREAM.json` records original file digests.

The only functional patch gates the diagnostic reqwest HTTP wrapper and its
experimental status wrapper behind a default-on `http-client` feature. Existing
native default behavior is retained; `z-enhance-pir` disables that wrapper and
supplies an application-owned, bounded exchange instead. The upstream reqwest
wrapper calls Response::chunk, which is unavailable on wasm32.

PIR math, scalar fallback, parameters, protocol revision, randomness and wire
serialization are unchanged. ipir-sp, inspiring, reinspiring and spiral are
exact published registry dependencies, with no local patches. Their unused
server mmap APIs compile to unsupported-platform stubs on wasm32; no browser
client path invokes them.
