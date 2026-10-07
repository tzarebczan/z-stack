These public contract runtime fixtures were read from `eth_getCode` on
https://base-rpc.publicnode.com on 2026-09-30. They contain no wallet owner
state. Tests independently hash each runtime against the fixed Base manifest.

A runtime match proves identity to these snapshots, not source/build provenance
or security review. The test manifest disables sponsorship. Integrators must independently verify
their deployment and provider before enabling these experimental primitives.
