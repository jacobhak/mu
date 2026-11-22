# ai-provider-gateway

A small tool that wraps the [oh-my-pi](https://github.com/can1357/oh-my-pi) (`omp`) CLI so any client can reach
the AI providers an omp login covers (subscription logins and the providers omp supports) through one local HTTP
gateway.

It starts one `omp auth-broker serve` + `omp auth-gateway serve` pair per omp profile, on loopback ports, and
hands out the gateway's URL and bearer token. The gateway serves Anthropic Messages at `/v1/messages`, the usable
models at `/v1/models` and quota windows at `/v1/usage`; it also accepts OpenAI Chat/Responses requests. The
tool also lists model prices (`omp models ls --json`) and runs `omp login`. A `mu-plugin` mode adapts all of
this to [mu](https://github.com/jacobhak/mu)'s gateway provider contract.

## How the gateway lives and dies

- **One pair per profile.** Every client of the same omp profile shares the pair, because the pair holds that
  profile's credentials and `models.yml`. Profiles never share one.
- **Leases.** A client asks for the endpoint with its pid (`--client-pid`, default the caller's parent). That
  touches a lease file `leases/<pid>`. The daemon drops leases whose process has died.
- **Idle timers.** The daemon stops both omp processes when no lease has been touched for
  `AI_PROVIDER_GATEWAY_IDLE_SECS` (default 1800), checking every `AI_PROVIDER_GATEWAY_TICK_SECS` (default 30).
  It also stops when the broker or the gateway exits, and on SIGTERM, SIGHUP or SIGINT. A lease is refreshed
  by every `endpoint` call, so a long-lived client should call it again before the idle time passes.
- **Cleanup.** The daemon is detached (`setsid`). Each omp child is its own process group, with a guard process
  that kills the group if the daemon dies by any means. A start that finds a dead daemon's recorded children
  stops them first.
- **Memory.** Idle, the pair measured about 306 MB total and 216 MB private (broker 93 / 58 MB, gateway
  214 / 158 MB); `scripts/measure-gateway.sh` reproduces it.

## Commands

Every command takes `--profile NAME` (default `$OMP_PROFILE`, then `$PI_PROFILE`). JSON goes to stdout; errors go
to stderr with exit 1.

| Command | Output |
| --- | --- |
| `endpoint [--client-pid PID] [--no-start]` | `{"baseUrl":"http://127.0.0.1:PORT","token":"…"}`. Takes a lease for `PID` and starts the daemon if needed. With `--no-start` and no daemon running it prints `null`, exits 0, and touches nothing (no lease, no files). |
| `prices` | The parsed output of `omp models ls --json` (60 s limit): `{"models":[{provider,id,name?,reasoning?,pricingStatus?,cost:{input,output,cacheRead,cacheWrite,longContext?}}]}`. |
| `status` | `{"running":true,"pid":123,"url":"http://127.0.0.1:PORT","leases":2}`, or `running:false` with `pid` and `url` null. `leases` counts leases of live processes. |
| `stop` | `{"stopped":true}` after stopping the daemon (SIGTERM, then SIGKILL after the grace omp's broker needs to finish a token refresh), `{"stopped":false}` when none ran. |
| `login [PROVIDER]` | Runs `omp login [PROVIDER]` with your terminal, so subscription logins need no other tool. |
| `mu-plugin` | mu's plugin protocol on stdio (below). |

`daemon` and `guard` are internal.

```sh
ai-provider-gateway login anthropic
curl -H "Authorization: Bearer $(ai-provider-gateway endpoint | jq -r .token)" \
  "$(ai-provider-gateway endpoint | jq -r .baseUrl)/v1/models"
```

## Where things live

`$AI_PROVIDER_GATEWAY_HOME`, else `$XDG_STATE_HOME/ai-provider-gateway`, else `~/.local/state/ai-provider-gateway`:

```
<home>/state.json  leases/  daemon.log  lock      the profile-less gateway
<home>/profiles/<name>/…                          the same, per omp profile
<home>/omp/<version>/omp                          the pinned omp (top level, shared by profiles)
```

A profile name must be one path component (no `/`, not `.` or `..`); anything else reads as no profile.

## How it finds omp

1. `$AI_PROVIDER_GATEWAY_OMP`, if set.
2. The pinned copy `<home>/omp/<version>/omp`, where `<version>` is the `omp` pin in `upstream.toml` this build
   was made with (`scripts/fetch-omp.sh` installs it, checking the release's checksum).
3. `omp` on `PATH`.

omp's own config and credentials stay where omp keeps them: `~/.omp` (`~/.omp/profiles/<name>` under a profile),
with the agent directory in `agent/` (or `$PI_CODING_AGENT_DIR`).

## Install

Without mu:

```sh
scripts/install.sh --from-source . --no-mu        # build this checkout
scripts/install.sh --no-mu                        # or the latest release (needs network or an authenticated gh)
scripts/install.sh --tag v0.1.0 --prefix ~/bin-prefix --no-mu
```

The binary goes to `<prefix>/bin/ai-provider-gateway` (default `~/.local`), and the pinned omp to
`<home>/omp/<version>/omp`. A release tarball unpacks to `ai-provider-gateway/` and runs the same
`scripts/install.sh` from there with its own binary. Release downloads are checked against `SHA256SUMS`, and
against the build attestation when the repo has one.

With mu: leave out `--no-mu`. When `--mu-home DIR` (default `$MU_HOME`, else `~/.mu`) exists, the installer writes
`<mu-home>/plugins/gateway/plugin.toml`:

```toml
name = "gateway"
command = ["/abs/path/to/ai-provider-gateway", "mu-plugin"]
lifetime = "idle:10m"
provider = "gateway"
```

## The mu adapter

`ai-provider-gateway mu-plugin` serves mu's plugin JSON-RPC on stdio:

- `initialize` answers `{}` and unseals omp's credentials in the background (below).
- `provider/endpoint {clientPid, start}` is `endpoint --client-pid clientPid`, with `--no-start` when `start` is
  false (default true). It answers `{baseUrl, token}` or `null`.
- `provider/prices {}` is `prices`.
- `shutdown` (a request; mu keeps the plugin's stdin open until it is answered) seals omp's credentials once
  more, bounded to 4 s, leaves the shared memory copy (below) and only then replies.

**Sealing through `core/secrets`.** When mu has state encryption on and unlocked (`core/secrets {"op":"info"}`
answers `sealed` and `unlocked`), the adapter keeps omp's credentials (`agent/agent.db`, `auth-broker.token`,
`auth-gateway.token`) sealed in mu's per-plugin secret store, never on disk in plaintext:

- It restores them from mu (`core/secrets` `get`/`put` with base64 values, names `agent.db`, `auth-broker.token`,
  `auth-gateway.token`) into a private memory-backed directory (tmpfs under `$XDG_RUNTIME_DIR` or `/dev/shm` on
  Linux, a 0700 temp directory on macOS) that mirrors omp's config root: everything else is a symlink to the real
  entry, so sessions, config and caches stay where they are.
- There is one such copy per profile, `<memory dir>/ai-provider-gateway-<uid>-<profile hash>`, shared by every
  adapter and by the daemon, whichever adapter started it. Adapters register in it as `users/<pid>` (dead ones
  are pruned) under a file lock, so adapters starting together do not race. The first one restores the
  credentials; later ones join without restoring over it. The copy records what was last sealed: a joining
  adapter seals at once anything the copy holds beyond that (rotations the daemon made while no adapter was
  attached), and never overwrites a newer sealed copy with a clean, older one.
- The last adapter to leave seals a final time and removes the copy only when no daemon runs for the profile
  (`state.json`). With a daemon running, the copy stays and the daemon keeps writing into it; the next adapter
  seals the difference, and removes and rebuilds a clean copy that nothing uses.
- Every omp it starts (the daemon pair, `models ls`) gets `PI_CONFIG_DIR` pointing at that copy (and
  `PI_CODING_AGENT_DIR` when you had overridden it). The loopback tokens are read from the copy too.
- Every adapter seals changes back every 30 s (the SQLite file by online backup, WAL included), after starting
  the daemon and after `prices`, and at shutdown, writing only what differs from the last sealed content. A crash
  between flushes loses what omp changed since the last one; the sealed copy can then hold a stale refresh token.
- A plaintext `agent.db` or token omp left on the volume (say, from `ai-provider-gateway login` run outside mu) is
  imported into mu's store the first time and removed from the volume; with a sealed copy already there, the
  sealed copy wins.
- When state encryption is off, or mu is locked, the adapter does nothing of this and omp uses its own files.

## Releases and the maintenance bot

CI (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` on Linux (`arc-ai-provider-gateway`) and on a
self-hosted Mac (`ai-provider-gateway-mac`): `cargo fmt --check`, shellcheck, `cargo clippy -D warnings`,
`cargo test`, then the contract tests (`-- --ignored contract_`) against the pinned omp, fetched with
`scripts/fetch-omp.sh`. `SKIP_CONTRACT=1` skips that last stage; `REQUIRE_SHELLCHECK=1` makes a missing
shellcheck a failure. Tags `v*` run `.github/workflows/release.yml`, which builds `linux-x64` and `darwin-arm64`
tarballs, writes `SHA256SUMS`, attests them where the repo allows, and creates the release with the omp version
in its notes.

`scripts/maint/` is a nightly bot (systemd user timer `apg-maint-nightly.timer`, set up by
`scripts/maint/setup.sh`) that keeps the omp pin current:

1. **S0:** checks `origin/main` (build, tests, contract); if that is red it opens or updates a `maint:s0` issue
   and stops.
2. **Detect:** compares the latest omp release with `upstream.toml`. One bump PR at a time, on `maint/omp-bump`.
3. **Bump:** `bump.sh` rewrites `upstream.toml` and proves the new omp asset downloads and matches its checksum;
   then the same checks run on the bump.
4. **Triage:** `material.sh` diffs omp's changelogs, broker/gateway source and CLI between the releases, and the
   `maint-triage` agent (run through an installed `mu`) says whether anything this repo owns is now wrong.
5. **Outcome:** a green bump with no behaviour change is labelled `maint:bump maint:auto`, waits for the PR's
   `check` job and is squash-merged, and the bot tags the next patch version (`vX.Y.Z+1`), which the release
   workflow publishes. Anything else is labelled `maint:bump maint:needs-review` (plus `maint:red` when checks
   fail) for a human.

Agent spend is capped per run and per month (`APG_MAINT_TRIAGE_USD`, `APG_MAINT_MONTHLY_USD`). `APG_MAINT_DRY_RUN=1`
logs every `gh` and `git` write instead of making it; `scripts/maint/test.sh` runs a dry run as a smoke test.

## Open risk

The broker refreshes OAuth tokens in the same credential store that running omp sessions use. If a provider
rotates refresh tokens, omp and the broker could invalidate each other's.

## License

MIT.
