# Bounded regtest recovery block evidence

The experimental `native-pir` feature exports
`verify_regtest_recovery_blocks(bytes, accepted_chain, cancel)`. It returns an
opaque `VerifiedRegtestRecoveryBlocks`; verification itself changes no wallet
state. Native canonical recovery is a separate, subsequent qualification.

V1 framing is the exact ASCII prefix `COFFER-REGTEST-BLOCKS-V1\n`, followed by
little-endian unsigned 32-bit start height and block count. Start must be 1 and
count must equal the independently accepted regtest chain target, from 1 through
2048. Each block follows as a little-endian unsigned 32-bit byte length and that
many bytes of its original complete serialized block. Individual blocks are
limited to 2,000,000 bytes; the whole envelope is limited to 128 MiB. No trailing
bytes, partial prefix, zero-length blocks or noncanonical CompactSize encodings
are accepted. Header solution and transaction-count lengths are bounded before
allocating the parsed block.

The pinned Rust block and transaction parsers own native transaction identifiers,
including ZIP 244 and the configured consensus branch. Verification compares each
claimed height, previous hash and header hash against the independently accepted
chain, requires supported versions in that branch, and checks the transaction-ID
Merkle root with duplicate-sibling mutation rejection. Sprout payloads, V6 and
Ironwood are currently refused rather than presented as supported recovery.

This proves transaction-effect/txid inclusion under the caller's independently
accepted valid regtest chain. It does not validate consensus, prove the chain's
source independent, or authenticate ZIP 244 authorizing data (signatures and
proofs). The post-NU5 block authorizing commitment needs additional chain-history
context and is a later qualification. Source transactions must never replace
wallet-authored pending transaction bytes or be reused for broadcast. A recovered
spend must be a new transaction constructed and signed by the native wallet.

The accepted-chain constructor validates shape and activation schedule, not a
remote publisher's honesty. Supplying hashes from the same publisher makes that
publisher the trust authority. Public PIR completeness and agreement require
separate coverage/reconciliation checks; verified common blocks alone do not
justify a PIR-complete status or a production privacy claim.
