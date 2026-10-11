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

Canonical recovery (`native-pir`, regtest only)

`begin_regtest_pir_recovery(&chain, &RecoveryCancellation)` durably sets Pending
before transport starts. `recover_regtest_pir(&chain, verified_blocks, filters,
transport, limits, &token)` commits authenticated canonical effects behind that
barrier, then reconciles mandatory private discovery against the exact native
receiver scope and full-block transparent effects. Its receipt releases selection
only when both agree. Provider failure leaves authenticated facts persisted and
selection blocked. `regtest_pir_recovery_for(&chain)` returns an exact context,
anchor, scanner and scope receipt or a curated reconciliation error.

The initial profile requires birthday 1 and the complete prefix. Allocated native
external, internal and previously exposed ephemeral receivers are included after the verified scan
establishes the chain tip required by the pinned ephemeral API. Discovery on an
unscanned database cannot enumerate exposed ephemeral receivers and does not qualify
canonical recovery. Unexposed ephemeral gap enumeration remains a qualification
gate for the full recovery profile. Import-enabled builds are outside this initial feature set.

`RecoveryCancellation::cancel()` serializes with the complete SQLite commit.
Network fetches and pure parsing use its read-only flag; writers hold its commit
gate through the actual transaction commit. Returning from cancel prevents any
later transaction from that token committing. `rewind_regtest_pir` combines native
truncate, PIR rollback and Pending in one transaction and refuses adjusted native
checkpoint heights rather than claiming an inexact rewind.

Persisted recovery markers are respected by ordinary native builds: a feature
downgrade refuses new preparation/signing/send/shield. Submission of an existing
durable signed intent remains available because it retransmits saved bytes.
Raw source data differing from an existing wallet transaction is refused before
canonical changes, including txid-equivalent authorizing-data substitutions.

`NativeWallet::open_offline(root)` opens persisted wallet state with instance
transport capability disabled. Stored endpoint metadata and endpoint edits cannot
re-enable it. Instance connection, validator lookup, enhancement and broadcast
paths refuse before socket I/O. Existing static transport helpers still require
explicit endpoints and must not be called by a cloud-only session adapter.
