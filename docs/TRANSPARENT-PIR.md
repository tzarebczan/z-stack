# Private transparent history

`transparentScan: "pir"` retrieves confirmed transparent receives and spends using
Transparent v12 PIR. It requires a separately consented `server.transparentPir`
transport. It never calls `utxos(addresses)` as a fallback. The existing `"off"`
and `"compact"` policies remain available.

The browser and native engines share the Rust acquisition client in
`crates/z-transparent-pir-v2`. Its compatible upstream pin is
`c9262e8431b4caa7cf2e2c55edbf0084c1f2ddb2`. This protocol is separate from
Ironwood Enhance v7. See that crate's `UPSTREAM.md` for the source transformations,
licenses, exact upstream inputs and removal criteria.

## Trust and scope

PIR conceals which supported transparent scripts the wallet queries. An OHTTP
adapter can hide the client's IP from the index provider when the relay and
recipient do not collude. Public shard ranges, timing, request counts and size
classes remain visible. A provider reached through a gateway on your machine
sees that machine's public IP. Running the relay and gateway under one operator
does not establish independent operators.

The enrolled publisher is trusted for correct, complete confirmed history and
negative filters. Hashes, manifests, signatures and chain anchors detect
inconsistency; they do not prove that an index included every payment. The exact
offer fingerprint binds the publisher, protocol and fixed index origin. A changed
provider requires new consent. Map revisions do not change that source identity.

The wallet derives its own supported external/internal script inventory and gap
expansion. Mainnet and testnet scan those scripts from a birthday estimate
rounded down to 10,000 blocks, about nine days, so the request does not publish
the exact creation height. A journal that already required earlier history keeps
that obligation. Regtest keeps its fixture height. Completion applies to that
enrolled inventory at the accepted chain anchor. It cannot discover arbitrary
unissued addresses or unsupported imported scripts. Receive-then-spend history
remains recoverable even when the current transparent balance is zero.

Mempool coverage, pending transaction outcomes, complete mixed-pool transaction
details and fees are separate. Initial PIR history does not enable transparent
spending or shielding. `snapshot.transparentPir` labels the recovered confirmed
amount and coverage under publisher trust; ordinary spend readiness remains a
separate predicate. Unknown submissions and their input reservations are retained.

Native funding keeps a durable quarantine separate from provisional-row deletion
ownership. The shielding selector excludes publisher-only outputs individually;
independently confirmed outputs already in the canonical wallet remain eligible.
Transfer proposals and transaction creation repeat the provenance check. Revoking
the transparent profile, reopening, retiring a projection, or reobserving its
output does not promote it. Native independently verified promotion is not
implemented in this release. Native funding version 2 snapshots shared transaction
confirmation authority before applying a checkpoint and quarantines existing
pending siblings together. Absent/version-1 native funding state conservatively
quarantines all currently canonical transparent outputs, because prior sibling
confirmation authority is ambiguous. This migration is bounded and does not
promote funds or discard history, acquisition progress, or payment authority. Canonical totals and history remain visible; totals do not authorize
spending. The existing source-bound native spending refusal remains in force.

## Browser transport

The app enrolls its exact OHTTP offer independently of sync/submission consent,
then supplies this transport to `createWallet`:

```ts
const wallet = await createWallet({
  network: "mainnet",
  storage,
  server: { ...acceptedPrivateCloudTransport, transparentPir },
  transparentScan: "pir",
  submissionPolicy: "explicit",
  autoShield: false,
  unlockPolicy: "each-spend",
});
```

`transparentPir.sourceId` is the accepted offer fingerprint. `exchange` takes
Rust-produced UTF-8 operation JSON and an accepted public anchor, and returns
Rust-verified UTF-8 reply JSON. The host moves opaque bytes. Rust keeps scripts,
PIR random secrets, query selection, record validation and canonical wallet state.
A Coffer adapter admits six closed v2 operations, not arbitrary paths or URLs.

The SDK drives one outstanding Rust query context across asynchronous exchanges.
It persists source/scope state before network access. It applies each acquisition
checkpoint in Rust, persists it with the wallet, acknowledges that durable commit,
and persists the acknowledgment before reporting completion. Quota failures,
cross-tab conflicts and obsolete sessions cannot publish a new ready snapshot.
A cold reopen regenerates query entropy from logical progress; it never serializes
an outstanding private query secret or replays a reply against a fresh query.

A sync task allows up to four activity-driven scope passes, 64 private exchanges,
64 independent historical-chain reads and 120 seconds. Host and gateway limits
also bound actual padded ciphertext, requests, responses and simultaneous work.
Budget exhaustion is incomplete, resumable work. There is no provider substitution,
address lookup, automatic replacement send or full-transaction fallback.

## Verification and availability

The capability is a release candidate until its installed archive, actual
browser workers, native controller and explicit source offer pass release checks.
Passing a native test or compiling WASM alone does not qualify all clients.
MacOS, Windows, Safari and mobile require their own actual results.

The test suite uses a real pinned v12 shard server, nonzero records, paged history,
upstream-wallet differential checks, malformed content, bounded cold resume and
canonical wallet application. Synthetic fixtures are identified separately from
consensus/node integration and production-wallet evidence.
