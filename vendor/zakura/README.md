# Zakura field crates

These are published 1.2.0 sources with the existing wasm32 field optimizations.
The original MIT/Apache-2.0 licenses and copyright files are retained.

- Provenance, scope, and removal criteria: [dependency ledger](../../docs/UPSTREAM.md).
- Exact patches and baseline hashes: [patch manifest](../../patches/zakura/manifest.json).
- Verify all source files: `pnpm check:patches` from the repository root.

This directory replaces the private `forks/` submodule. It is a source build
input, not an additional npm package. Do not change arithmetic here without an
updated patch and the required native and WASM regression checks.
