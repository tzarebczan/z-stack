# Contribution guidance

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, and
[docs/README.md](docs/README.md) for integration contracts.

## Architecture

- Zcash cryptography, transaction construction and hardware signature verification
  belong in `z-engine`. TypeScript handles transport, workers, storage and events.
  Optional Base operations use viem; do not implement cryptographic primitives.
- Keep the wallet independent of registration, authentication and remote backup.
  These are optional, application-owned services. Base, swap and fiat integrations
  must not become requirements for a Zcash wallet.
- SDK root exports are the supported integration surface. `/engine` and `/lab`
  are experimental. Consumers use built `dist/` exports; repository tools may use
  the `@z-stack/source` condition. Verify installed archives after export changes.
- WebAuthn and PRF vault operations belong in `@z-stack/passkey`, which has no
  runtime dependencies. Keep browser vault `navigator.credentials` calls there.
  Server-side WebAuthn verification belongs to the app.
- Keep native I/O behind Rust feature gates. `z-wasm` disables default features;
  both single-threaded and threaded WASM must build. Keep Sapling proving
  parameters separate and integrity-checked. Run proving off the UI thread.
- Keep native wallet bridges and development block pipes bound to loopback.
  Browser deployments use an explicitly configured gRPC-Web server or transport.

## Security and correctness

- Never commit seeds, private keys, wallet databases, credentials or secret `.env`
  files. Use empty wallets or disposable regtest fixtures for reproductions.
  Never write a plaintext `mnemonic.txt`; use encrypted seed storage or an OS keystore.
- Preserve committed state on cancellation, quota failures and cross-tab conflicts.
  Persist spending reservations before broadcast; reconcile uncertain submissions
  rather than constructing replacement payments. Never log private error payloads.
- Lock removes spending access, not persisted viewing history. Account logout,
  local deletion and remote backup deletion are separate operations.
- Registration and backup policies must remain explicit. Automatic shielding and
  transaction-ID memo retrieval are opt-in; do not change privacy defaults silently.
- Match every regtest client's activation schedule to its fixture. Compose and
  native fixtures use different NU6.3 heights; do not run competing funded tests
  against the same faucet.

## Changes and verification

- Add regression coverage for persistence, authorization, spending and concurrency
  changes. Rebuild both WASM variants after Rust changes; typed stubs do not verify
  engine behavior. Use [the verification guide](docs/RELEASE.md) for relevant checks.
- Keep SDK, core, passkey, Base, workspace and `SDK_VERSION` versions in sync.
  Update public documentation and examples when changing an integration contract.
- Use pinned Zakura dependencies. Extend `z-engine` before patching upstream;
  record necessary patches, provenance, tests and removal criteria in
  [docs/UPSTREAM.md](docs/UPSTREAM.md). Preserve original third-party legal notices.
- Sign commits with your registered signing key. Keep commit messages focused on
  the change. Report vulnerabilities through [SECURITY.md](SECURITY.md).

## Cursor Cloud specific instructions

- Start with the installed browser example in [the walkthrough](docs/GETTING-STARTED.md) for public testnet integration. `pnpm web:dev` is a source diagnostic tool, not that walkthrough.
- The diagnostic bench is `pnpm web:dev` (Vite on port 5174). Key derivation
  runs locally in the browser. Restore also queries the light server; Create does too with an automatic or date-based birthday. Only Create
  with an explicit block height can run offline. Sync always needs the configured
  server. For regtest, start the matching loopback fixture separately.
- `pnpm test`, package builds, and the bench need both WASM engines from
  `pnpm build:sdk`. Keep `CARGO_BUILD_JOBS` at 1 so release compiles stay within
  this VM's memory. The environment start script enables an 8G `/swapfile`.
- CI uses Node 22.23.3. `/exec-daemon/node` on the base image is older; the
  environment install puts 22.23.3 first on `PATH`.

## Shared remote development and CI

Use Tom’s existing workload-fleet controller for remote work. Load the installed
`workload-fleet` skill first. Proton Computer is the current provider; use the
approved catalog rather than assuming a particular provider or machine.

From Beast, inspect:

```sh
workload-fleet catalog
workload-fleet status
```

If this project is not enrolled, prepare an enrollment request with its repository,
toolchain, worktree paths, commands and resource requirements. Do not grant
yourself profiles, create paid machines or change controller policy.

Current catalog note (verified via `workload-fleet catalog`): only `eggomi`
(`eggomi-smoke`, `eggomi-fleet-tests`, `eggomi-dev-session`) and `bot-dial`
(`bot-dial-syntax`, `bot-dial-dev-session`) are currently enrolled across the
approved Proton machines `eggomi-dev` (`proton-eggomi-dev`, `ssh.proton.computer:22024`),
`eggomi-ci` (`proton-eggomi-ci`, `ssh.proton.computer:22023`) and `eggomi-ci-2`
(`proton-eggomi-ci-2`, `ssh.proton.computer:22034`). Until `z-stack` is enrolled
by the administrator with `sudo fleet-enroll`, do not invent or submit
unapproved profile names.

### Interactive work

Reserve from Beast or the always-on lab before connecting to a sleeping worker:

```sh
fleet-session --host lab-dev begin \
  --profile <APPROVED_SESSION_PROFILE> \
  --workspace <CANONICAL_REMOTE_WORKTREE> \
  --key <UNIQUE_TASK_SESSION_KEY>
```

Wait for `state: held`. Save the returned job ID with the task.

- Tools with native SSH support can open the verified remote workspace.
- Other tools can keep the chat local and execute commands over SSH.
- Keep one active writer per worktree.
- Keep the reservation while commands, builds, previews or agents remain active.
- Release only this task’s reservation after its remote work stops:

```sh
fleet-session end <JOB_ID>
```

Alternatively, launch an explicitly requested remote CLI through the supervised
wrapper. Use this instead of creating a separate `begin` reservation:

```sh
fleet-session --host lab-dev run \
  --profile <APPROVED_SESSION_PROFILE> \
  --workspace <CANONICAL_REMOTE_WORKTREE> \
  --remote proton-eggomi-dev -- <AGENT_CLI>
```

The wrapper reserves, wakes, launches and supervises the command. Starting a VM
does not transfer a running chat. A handoff must record the task, worktree,
branch/head, dirty files, completed checks and reservation ownership.

### CI

Follow this repository’s CI instructions. Submit or join one matching run/batch
and reuse its watcher. For enrolled CI pools, queued approved labels trigger
controller-managed wake, admission and idle shutdown.

Do not manually start runners, restore labels, clear lifecycle fences or switch
to unapproved compute. A stopped idle worker is normal. Investigate blocked
queues through controller status and authoritative provider inventory.

### Boundaries

A sleeping worker cannot wake itself. Provider control stays outside it.
New VMs/providers require separately authorized provisioning, reviewed adapters,
verified identity and administrator enrollment.

Preserve other tasks’ reservations. Never copy authentication stores, private
keys or release credentials between machines. Never infer that all work ended
from low CPU, a disconnected GUI, a timeout or a missed heartbeat.

