# Enhance v7 client

This transport-neutral Rust client uses wallet-pir `24f7694cd4f8274172a11cbff43c158bcab5d500`'s q48 v7 protocol. The vendored wrapper disables diagnostic HTTP; its math, parameters, wire encoding and OS/browser randomness are unchanged. See `vendor/pir/UPSTREAM.json` and the retained licenses.

A caller must validate the historical manifest anchor against independently scanned wallet state, then open the accepted manifest and fetch only its canonical public session identifiers. Each query owns a fresh private response context. Success, malformed replies, stale routing and cancellation consume that context. Responses are exactly 30,836 bytes; decoded records are exactly 653 bytes. Record decoding alone does not authenticate a note or authorize a wallet update.

The `protocol-bindings` WASM feature exposes standalone qualification bindings. Production wallet bindings retain positions and action identities in Rust. The synthetic local fixture proves native server/client and Chromium/Firefox ST/MT wire compatibility; it does not establish production service capacity, all database sizes, or wallet-level completion.
