# mu

> I love omp, but in the age of AI, RAM ain't cheap and every session being the equivalent of a chrome tab simply isn't sustainable.

mu is a terminal coding agent with a Rust core (agent loop, TUI, sessions) built on the crates of [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp), and borrowing many of its prompts and much of its behaviour. Credit for the design goes to the omp authors.

## How mu is different

mu keeps most of what omp does and changes where the memory goes. A session holds your conversation, and anything else runs only while it's being used.

- **Small core.** A native binary holds the agent loop, the TUI and the built-in tools. An idle session holds about 3.6 MB of private memory (about 20 MB counting the binary's shared pages).
- **Features start on use.** Web, memory and prediction are separate processes, and so is the optional model gateway (a [gateway provider plugin](docs/providers.md#gateway-plugins)). Each starts on first use and exits after a few idle minutes. A session that talks to a provider with your own key or ChatGPT sign-in starts no gateway at all (see [Providers](docs/providers.md)). MCP servers start on their first tool call; their tool lists are cached so mu can advertise the tools without starting them.
- **Shared across sessions.** When a gateway is needed, sessions on one omp profile share one gateway and its broker (about 306 MB total, 216 MB private, idle; measured with `scripts/measure-gateway.sh`). [`BENCHMARKS.md`](BENCHMARKS.md) shows what that means per session.
- **Bulky content goes to disk.** Large tool output and images are written to files instead of being kept in memory.
- **Works with your omp files.** mu reads omp's context files, skills and MCP config, and writes sessions in omp's format, so you can use both and switch either way. Model roles and limits are mu's own (`mu models import` copies omp's). You don't need an existing omp setup, and the installer fetches omp only when you ask (`--with-omp`, for the gateway).

The cost: features that live inside omp's runtime, such as TypeScript extensions, aren't available. The reasoning and measurements behind the design are in [`DESIGN.md`](DESIGN.md).

## Beyond omp

mu also adds a few things omp doesn't have:

- **[Reply picker](docs/reply-picker.md).** `Ctrl+R` lets you move through the last reply by sentence, list item or code line, with vim or emacs keys, and mark parts of it. The marked parts are quoted into your answer, so you can reply point by point.
- **[Durable plans](docs/durable-plans.md).** A plan doesn't have to be approved in the session that wrote it. You can park it, list it and refine it, then approve it later in the TUI or headless (`mu plan`), or hand it to `/deliver` to be implemented, reviewed and committed in its own worktree. When you open a parked plan, mu tells you which of the files it was based on have changed since.
- **[Scheduled prompts](docs/schedule.md).** `/schedule 10m <prompt>` sends a prompt later, as if you had typed it then, so you can walk away and come back to an answer. A prompt that comes due during a turn waits for the turn to end.
- **[Your own modes](docs/modes.md).** A mode is a markdown file: a prompt, a model, a tool policy and an exit step with approval actions. Plan mode is just the built-in one, and you can override it or add others without writing code.
- **[Taking over detached runs](docs/detached-runs.md).** Stop a `mu -p` run at a tool boundary and pick up its session (`mu take`, `mu --resume`), or leave a message for a running session with `mu notify`.

## Status

Early and experimental: started in October 2026 and changing quickly.

- No stable version. Releases are builds of `main`, tagged `mu-YYYYMMDD-<sha7>` and published once `main` passes the checks; config, flags and plugin interfaces can change without notice.
- Providers: Anthropic, OpenAI and Baseten work with an API key you add in mu (`/login`, `mu auth add <provider>`), and OpenAI models also run on your ChatGPT plan (`/login`). Neither involves a gateway. Every other provider, and subscription logins, go through the `mu-gateway` plugin that ships with mu, which needs the omp CLI (`install.sh --with-omp`, about 300 MB; log in with `mu-gateway login <provider>`). Details in [Providers](docs/providers.md).
- Prebuilt for Linux x86_64 and macOS arm64; other platforms are untested.
- No support promised. Issues and PRs are welcome but may go unanswered.

## Build and install

Releases (Linux x86_64, macOS arm64) are published from `main`:

```sh
curl -fsSL https://raw.githubusercontent.com/jacobhak/mu/main/scripts/install.sh | bash
curl -fsSL https://raw.githubusercontent.com/jacobhak/mu/main/scripts/install.sh | bash -s -- --tag mu-YYYYMMDD-<sha7>
scripts/install.sh --from-source .            # build and install a checkout
```

If the repo is private (or you install from a private fork), the public URLs return 404. `install.sh` and `mu update` then fall back to an authenticated `gh`; fetch the script itself with `gh api repos/jacobhak/mu/contents/scripts/install.sh -H 'Accept: application/vnd.github.raw' | bash`.

The installer checks the tarball against the `SHA256SUMS` of the same release, which catches a bad download but not a replaced release. When `gh` is signed in and the release carries a GitHub build attestation (the release workflow makes one where the repo supports them: public repos, or GitHub Enterprise Cloud), it also verifies the attestation and refuses to install if that fails. Without an attestation it says so and relies on the checksum.

This installs `mu` and its plugins under `~/.mu/{bin,plugins}` and links `~/.local/bin/mu` (not when `MU_HOME` is set). Installing `--from-source` needs only a Rust toolchain. `--skip-if-dev` leaves an installed dev build (dirty, unknown, or not on `origin/main`) alone. `--from-source` asks git whether the tree has uncommitted changes and records the answer in `~/.mu/dev-build` (removed by any other install), because the version's `-dirty` suffix misses an unstaged edit. `mu --version` prints `mu 0.1.0 (<sha>) oh-my-pi crates <version>`.

Then start `mu` and run `/login` to add a key or sign in with ChatGPT (see Status above for which providers need what). For the gateway, install with `--with-omp` (`curl … | bash -s -- --with-omp`). Web search and fetch need no gateway, and MCP OAuth is done by mu itself (`/mcp login <name>`).

### Updating

`mu update` installs the latest release by running the repo's `install.sh`. A dev build is also one that `~/.mu/dev-build` marks. `mu update --check` only reports; `--tag mu-YYYYMMDD-<sha7>` picks a release; `--force` replaces a dev build, which is otherwise refused. On startup the TUI checks for a newer release at most once a day (cached in `~/.mu/update-check`) and shows a notice; it stays silent when offline or on a dev build. Turn it off with `[update] check = false` in config.toml. Checks use the unauthenticated GitHub API (60 requests an hour), or `gh` when the repo is private.

### Pinned oh-my-pi

`upstream.toml` pins one oh-my-pi release: the Rust crates mu builds on (`tag = "v<ver>"` in `Cargo.toml`) and the omp CLI that `--with-omp` installs. Only the gateway plugin runs that CLI; mu itself runs no omp binary.

### Manual build

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web,mu-predict,mu-memory}
scripts/build-mcp-http.sh                # target/release/mu-mcp-http (see docs/mcp.md)
mkdir -p ~/.mu/plugins
for p in gateway web memory predict; do ln -sfn "$PWD/plugins/$p" ~/.mu/plugins/$p; done
```

Plugins are found in `~/.mu/plugins/*/plugin.toml`; their binaries (`mu-gateway`, `mu-web`, `mu-memory`, `mu-predict`, `mu-mcp-http`) are resolved next to the `mu` binary (the gateway's source and README are in [`plugins/gateway`](plugins/gateway/README.md)). `MU_GATEWAY_URL` plus `MU_GATEWAY_TOKEN` bypass the gateway plugin and point at a running gateway directly.

### Syntax highlighting

Fenced code is highlighted with tree-sitter for Rust, JSON, Bash, TOML, YAML, Python, JavaScript, TypeScript (and TSX), C, C++, CSS, Go, HTML, Java, regex, diff and Markdown. Markdown fences inject the other grammars.

Grammars load on first use of that language (about 1-2 MB resident each); idle memory is unaffected.

## Documentation

Usage, configuration and the individual features are documented in [`docs/`](docs/README.md).

## Development

### Tests

`scripts/ci-checks.sh` runs everything CI does (fmt, shellcheck, cargo-deny, clippy, the tests); run it before pushing. `cargo test --workspace` runs the unit tests; `scripts/build-mcp-http.sh test` runs the bridge's (it is built outside the workspace, see [MCP servers](docs/mcp.md)). `scripts/smoke.sh` runs live end-to-end checks (gateway, tmux TUI, subagents, compaction; it needs a gateway plugin, set with `MU_SMOKE_GATEWAY_PLUGIN`, default the `gateway` plugin installed under `~/.mu/plugins`); `scripts/smoke-native.sh` checks a native API-key provider offline against a loopback fake API; `scripts/measure-idle.sh target/release/mu` reports idle memory.

`cargo test --workspace --locked -- --ignored contract_` runs the native, key-gated provider tests.

### Debugging

`MU_DUMP_REQUESTS=/path/file.jsonl` appends the shape of each model request (never its content); see [Providers](docs/providers.md#debugging).

### Maintenance bot

A local systemd timer keeps mu current with oh-my-pi; everything is reported through GitHub issues and PRs. `scripts/maint/setup.sh [--profile NAME]` installs it (config in `~/.mu/maint/config.env`, budgets `MU_MAINT_*_USD`, `MU_MAINT_DRY_RUN=1` prints instead of mutating GitHub). Its LLM steps are the agents in `.omp/agents/` (`maint-triage`, `maint-fixer`, `maint-digest`), run by the installed `mu`. When `offload` is installed and reachable, `check.sh` runs its `test` stage (`scripts/ci-checks.sh`) as `offload run -- scripts/ci-checks.sh`; the build, contract and smoke stages stay local (they need a live model and the release binary). `MU_MAINT_OFFLOAD=0` forces everything local; a stage that runs past `MU_MAINT_STAGE_SECS` (default 2400) fails.

| Level | Trigger | Action |
|---|---|---|
| S0 | `scripts/maint/check.sh` fails on `origin/main` | issue `maint:s0`; fixer PR `maint:needs-review`; no bump or publish |
| S1 | new omp release, bump fails checks or cargo cannot resolve the new pin | fixer adapts mu; PR `maint:bump` + `maint:needs-review` (+ `maint:red`). If only the live smoke stage fails the fixer is not run and the PR carries the smoke log |
| S2 | green bump, nothing in mu is wrong after it | PR `maint:bump` + `maint:auto`, squash-merged once the PR's own CI run (the `check` job) passes; if that fails or does not finish within `MU_MAINT_CI_WAIT_SECS` (default 3600) it is labelled `maint:needs-review` and left open. Upstream changes mu only passes through are listed in the PR |
| S2r | green bump, the release leaves mu's own code, prompts or docs wrong | fixer corrects it; PR `maint:bump` + `maint:needs-review`, not merged. If the fixer makes no edit, or edits only tests and docs (`scripts/maint/test-only.sh`), the PR is merged as S2 |
| S3 | weekly | issue `maint:digest` with omp changes worth porting |
| finding | the fixer notices a problem in mu the bump did not cause | issue `maint:finding` |
| publish | `origin/main` is green and newer than the last release | tag `mu-YYYYMMDD-<sha7>`; `.github/workflows/release.yml` builds and releases; the bot reinstalls unless a dev build is installed |

There is one bump PR at a time, on the branch `maint/omp-bump`. A newer omp release rebuilds that branch from `main` and updates the open PR in place, so commits pushed to it are lost; closing the PR unmerged makes the next nightly run open a fresh one.

Nightly runs at 03:30, the digest on Mondays 05:00. The bot never edits `.github/`.

## License

MIT, see `LICENSE`. mu includes prompts and other material from [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT) and colour palettes from [omarchy](https://github.com/basecamp/omarchy) (MIT); see `THIRD-PARTY-NOTICES.md`.
