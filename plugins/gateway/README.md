# mu-gateway

mu's gateway provider plugin. It wraps the [oh-my-pi](https://github.com/can1357/oh-my-pi) (`omp`) CLI so mu (or
any other client) can reach the AI providers an omp login covers (subscription logins and the providers omp
supports) through one local HTTP gateway. It is only needed for models that have no API key or ChatGPT sign-in in
mu; the plugin starts on the first such request and exits when idle.

It starts one `omp auth-broker serve` + `omp auth-gateway serve` pair per omp profile, on loopback ports, and
hands out the gateway's URL and bearer token. The gateway serves Anthropic Messages at `/v1/messages`, the usable
models at `/v1/models` and quota windows at `/v1/usage`; it also accepts OpenAI Chat/Responses requests. The
tool also lists model prices (`omp models ls --json`) and runs `omp login`. Its `mu-plugin` mode is mu's
[gateway provider contract](../../docs/providers.md#gateway-plugins).

A second backend, selected with `--backend pi`, reaches providers through the [pi](https://github.com/earendil-works/pi)
coding agent instead (see [The pi backend](#the-pi-backend)). Everything below describes the default omp backend
unless it says otherwise.

## How the gateway lives and dies

- **One pair per profile.** Every client of the same omp profile shares the pair, because the pair holds that
  profile's credentials and `models.yml`. Profiles never share one.
- **Leases.** A client asks for the endpoint with its pid (`--client-pid`, default the caller's parent). That
  touches a lease file `leases/<pid>`. The daemon drops leases whose process has died.
- **Idle timers.** The daemon stops both omp processes when no lease has been touched for
  `MU_GATEWAY_IDLE_SECS` (default 1800), checking every `MU_GATEWAY_TICK_SECS` (default 30).
  It also stops when the broker or the gateway exits, and on SIGTERM, SIGHUP or SIGINT. A lease is refreshed
  by every `endpoint` call, so a long-lived client should call it again before the idle time passes.
- **Cleanup.** The daemon is detached (`setsid`). Each omp child is its own process group, with a guard process
  that kills the group if the daemon dies by any means. A start that finds a dead daemon's recorded children
  stops them first.
- **Memory.** Idle, the pair measured about 275 MB total and 199 MB private (broker 80 / 48 MB, gateway
  195 / 150 MB, omp 19.1.1, 2026-11-25); `scripts/measure-gateway.sh` reproduces it.

## Commands

Every command takes `--profile NAME` (default `$OMP_PROFILE`, then `$PI_PROFILE`) and `--backend omp|pi` (default
`$MU_GATEWAY_BACKEND`, then `omp`). JSON goes to stdout; errors go to stderr with exit 1.

| Command | Output |
| --- | --- |
| `endpoint [--client-pid PID] [--no-start]` | `{"baseUrl":"http://127.0.0.1:PORT","token":"…"}`. Takes a lease for `PID` and starts the daemon if needed. With `--no-start` and no daemon running it prints `null`, exits 0, and touches nothing (no lease, no files). |
| `prices` | The parsed output of `omp models ls --json` (60 s limit): `{"models":[{provider,id,name?,reasoning?,pricingStatus?,cost:{input,output,cacheRead,cacheWrite,longContext?}}]}`. |
| `status` | `{"running":true,"pid":123,"url":"http://127.0.0.1:PORT","leases":2}`, or `running:false` with `pid` and `url` null. `leases` counts leases of live processes. |
| `stop` | `{"stopped":true}` after stopping the daemon (SIGTERM, then SIGKILL after the grace omp's broker needs to finish a token refresh), `{"stopped":false}` when none ran. |
| `login [PROVIDER]` | Runs `omp login [PROVIDER]` (`pi auth login [PROVIDER]` with `--backend pi`) with your terminal, so subscription logins need no other tool. |
| `mu-plugin` | mu's plugin protocol on stdio (below). |

`daemon` and `guard` are internal.

```sh
mu-gateway login anthropic
curl -H "Authorization: Bearer $(mu-gateway endpoint | jq -r .token)" \
  "$(mu-gateway endpoint | jq -r .baseUrl)/v1/models"
```

## Where things live

`$MU_GATEWAY_HOME`, else `<mu home>/gateway` (`~/.mu/gateway`):

```
<home>/state.json  leases/  daemon.log  lock      the profile-less gateway
<home>/profiles/<name>/…                          the same, per omp profile
```

A profile name must be one path component (no `/`, not `.` or `..`); anything else reads as no profile.

## How it finds omp

1. `$MU_GATEWAY_OMP`, if set.
2. The `omp` next to the `mu-gateway` binary (`~/.mu/bin/omp`), which mu's `scripts/install.sh --with-omp`
   installs at the `omp` pin in `upstream.toml` (`scripts/fetch-omp.sh`, which checks the release's checksum). An
   omp already installed there is kept at the pin by later installs and `mu update`.
3. `omp` on `PATH`.

omp's own config and credentials stay where omp keeps them: `~/.omp` (`~/.omp/profiles/<name>` under a profile),
with the agent directory in `agent/` (or `$PI_CODING_AGENT_DIR`).

## Install

mu's `scripts/install.sh` (and `mu update`) installs `mu-gateway` next to `mu` and its manifest,
[`plugin.toml`](plugin.toml), under `~/.mu/plugins/gateway`. The omp CLI it runs is a 300 MB download and is
opt-in: `scripts/install.sh --with-omp`, which also links `mu-gateway` into `~/.local/bin`. Then sign in from mu's
`/login`, or by hand with `mu-gateway login <provider>`.

## The mu adapter

`mu-gateway mu-plugin` serves mu's plugin JSON-RPC on stdio:

- `initialize` answers `{}` and unseals omp's credentials in the background (below).
- `provider/endpoint {clientPid, start}` is `endpoint --client-pid clientPid`, with `--no-start` when `start` is
  false (default true). It answers `{baseUrl, token}` or `null`.
- `provider/prices {}` is `prices`.
- `provider/logins {}` answers `{"providers":[{id,label}]}`: `anthropic`, `openai-codex`, and one row without an `id`
  that leaves the choice to `omp login` (`pi auth login` with `--backend pi`). mu lists them in `/login`.
- `provider/login {provider?}` answers `{"command":[argv…],"env":{…}}`, the `login` command's program, for mu to run
  on its own terminal (a plugin's stdio is the JSON-RPC pipe, so it cannot). `provider` must be an id made of
  letters, digits, `-` and `_`. With sealing on, the credentials are unsealed first and `env` points omp at the
  memory copy, so the sign-in lands in it.
- `provider/login_done {}` is sent when that program has exited, whatever its status: it seals what the sign-in
  wrote (the pi backend has nothing to seal).
- `shutdown` (a request; mu keeps the plugin's stdin open until it is answered) seals omp's credentials once
  more, bounded to 4 s, leaves the shared memory copy (below) and only then replies.

**Sealing through `core/secrets`.** When mu has state encryption on and unlocked (`core/secrets {"op":"info"}`
answers `sealed` and `unlocked`), the adapter keeps omp's credentials (`agent/agent.db`, `auth-broker.token`,
`auth-gateway.token`) sealed in mu's per-plugin secret store, never on disk in plaintext:

- It restores them from mu (`core/secrets` `get`/`put` with base64 values, names `agent.db`, `auth-broker.token`,
  `auth-gateway.token`) into a private memory-backed directory (tmpfs under `$XDG_RUNTIME_DIR` or `/dev/shm` on
  Linux, a 0700 temp directory on macOS) that mirrors omp's config root: everything else is a symlink to the real
  entry, so sessions, config and caches stay where they are.
- There is one such copy per profile, `<memory dir>/mu-gateway-<uid>-<profile hash>`, shared by every
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
- A plaintext `agent.db` or token omp left on the volume (say, from `mu-gateway login` run outside mu) is
  imported into mu's store the first time and removed from the volume; with a sealed copy already there, the
  sealed copy wins.
- When state encryption is off, or mu is locked, the adapter does nothing of this and omp uses its own files.

## Tests and the omp pin

The unit and integration tests run with the workspace (`cargo test --workspace`). The contract tests (`-- --ignored
contract_`) run the real omp, named by `MU_GATEWAY_OMP`; mu's maintenance bot (`scripts/maint/`) runs them on every
omp bump, with the pinned omp fetched by `scripts/fetch-omp.sh`, and its triage agent reads omp's auth-broker and
auth-gateway changes against the touchpoints listed in `scripts/maint/material.sh`.

## The pi backend

`--backend pi` (or `MU_GATEWAY_BACKEND=pi`; mu selects it with the `gateway-pi` plugin, whose manifest runs
`mu-gateway --backend pi mu-plugin`) replaces the omp pair with one pi process. Pi has no auth server, so the
daemon runs `pi --mode rpc --no-session -ne -e <proxy.mjs> --no-mcp --no-skills --no-prompt-templates --no-themes
--no-context-files -na` with `plugins/gateway-pi/proxy.mjs`, an extension that listens on a loopback port and
serves the same endpoints through pi's own model registry (credentials, OAuth refresh, catalogue, streaming):
Anthropic Messages at `/v1/messages`, `/v1/models`, `/v1/usage` and an unauthenticated `/healthz`.

- **Process.** The daemon makes a random token, writes it to `<dir>/token` (0600) and passes it to pi as
  `MU_GW_PI_TOKEN`, with `MU_GW_PI_MODE=serve`. The extension prints `mu-gateway-pi proxy listening on
  http://127.0.0.1:PORT` on stderr; the daemon takes the first `http://` URL from pi's output. The pi child is its
  own process group with the same guard and parent-death handling as the omp pair, and its stdin stays open in
  the daemon, so pi also exits when the daemon dies. Leases, idle timers, `status` and `stop` work as above.
- **State.** `$MU_GATEWAY_PI_HOME`, else `<mu home>/gateway-pi`, with the same layout as the omp home plus
  `token`; `--profile` selects a mu profile directory only and is not passed to pi.
- **Finding pi.** `$MU_GATEWAY_PI`, else the `pi` next to the `mu-gateway` binary (`~/.mu/bin/pi`), else `pi` on
  `PATH`. **Finding the proxy.** `$MU_GATEWAY_PI_PROXY`, else `proxy.mjs` in the working directory (mu starts
  plugins in their own directory), else `<exe dir>/../plugins/gateway-pi/proxy.mjs`.
- **Prices.** A one-shot run of the same command with `MU_GW_PI_MODE=prices` and `MU_GW_PI_OUT=<temp file>`
  (60 s limit); the extension writes `{"models":[…]}` from pi's catalogue and exits.
- **Login.** `mu-gateway --backend pi login [PROVIDER]` runs `pi auth login [PROVIDER]`.
- **Credentials are not sealed.** The adapter never uses mu's `core/secrets` for pi: no unsealing, no flush, no
  environment overrides. Pi's own files (`~/.pi`) stay where pi keeps them, in plaintext.
- **Quota.** Pi reports no quota windows, so `/v1/usage` answers an empty `reports` list.

## Open risk

The broker refreshes OAuth tokens in the same credential store that running omp sessions use. If a provider
rotates refresh tokens, omp and the broker could invalidate each other's.
