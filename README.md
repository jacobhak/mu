# mu

> I love omp, but in the age of AI, RAM ain't cheap and every session being the equivalent of a chrome tab simply isn't sustainable.

mu is a terminal coding agent with a Rust core (agent loop, TUI, sessions) built on the crates, CLI and auth gateway of [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp), and borrowing many of its prompts and much of its behaviour. Credit for the design goes to the omp authors.

## How mu is different

mu keeps most of what omp does and changes where the memory goes. A session holds your conversation, and anything else runs only while it's being used.

- **Small core.** A native binary holds the agent loop, the TUI and the built-in tools. An idle session holds about 3.5 MB of private memory (about 19 MB counting the binary's shared pages).
- **Features start on use.** Web, memory and prediction are separate processes, and so is the model gateway, which only providers without an API key or ChatGPT sign-in in mu need. Each starts on first use and exits after a few idle minutes. A session that talks only to Anthropic, OpenAI or Baseten with your own key, or to OpenAI on your ChatGPT plan, starts no omp process at all. MCP servers start on their first tool call; their tool lists are cached so mu can advertise the tools without starting them.
- **Shared across sessions.** Sessions on one omp profile share one gateway and its broker (about 250 MB while they run, idle), and all sessions share one memory daemon. [`BENCHMARKS.md`](BENCHMARKS.md) shows what that means per session.
- **Bulky content goes to disk.** Large tool output and images are written to files instead of being kept in memory.
- **Works with your omp setup.** mu uses omp's config, models, MCP setup, context files and skills, and its login for OAuth providers. Sessions are in omp's format, so you can use both and switch either way. You don't need an existing omp setup: the installer bundles omp for the providers that need it, and Anthropic, OpenAI and Baseten work with just an API key (OpenAI models also run on a ChatGPT plan).

The cost: features that live inside omp's runtime, such as TypeScript extensions, aren't available. The reasoning and measurements behind the design are in [`DESIGN.md`](DESIGN.md).

## Beyond omp

mu also adds a few things omp doesn't have:

- **[Reply picker](docs/reply-picker.md).** `Ctrl+R` lets you move through the last reply by sentence, list item or code line, with vim or emacs keys, and mark parts of it. The marked parts are quoted into your answer, so you can reply point by point.
- **[Durable plans](docs/durable-plans.md).** A plan doesn't have to be approved in the session that wrote it. You can park it, list it and refine it, then approve it later in the TUI or headless (`mu plan`), or hand it to `/deliver` to be implemented, reviewed and committed in its own worktree. When you open a parked plan, mu tells you which of the files it was based on have changed since.

## Status

Early and experimental: started in October 2026 and changing quickly.

- No stable version. Releases are nightly builds of `main` (`mu-YYYYMMDD-<sha7>`); config, flags and plugin interfaces can change without notice.
- Model roles and `models.yml` are omp's. Anthropic, OpenAI and Baseten work with an API key you add in mu (`/login`, `mu auth`), with no omp involved; OpenAI models also run on your ChatGPT plan through Sign in with ChatGPT (`/login`). Every other provider, and subscription logins, go through omp (see [Providers](docs/providers.md)).
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

This installs `mu`, the plugins and a pinned `omp` under `~/.mu/{bin,plugins}` and links `~/.local/bin/mu` (not when `MU_HOME` is set). Releases include the memory daemon as a compiled binary, so they need no Bun; installing `--from-source` needs Bun to build it and otherwise leaves the memory plugin out (keeping any installed one). `--skip-if-dev` leaves an installed dev build (dirty, unknown, or not on `origin/main`) alone. `mu --version` prints `mu 0.1.0 (<sha>) omp <version>`.

Then add an API key for Anthropic, OpenAI or Baseten with `/login` in mu, or `mu auth add <provider>`; those models then run without omp. Or sign in with ChatGPT (`/login`, or `mu auth login openai-chatgpt` from a shell) to run OpenAI models on your plan. omp is only needed for subscription (OAuth) logins, the other providers, web search and fetch, and MCP OAuth: log in with `/login` for those, or `~/.mu/bin/omp login` for `mu -p`.

### Updating

`mu update` installs the latest release by running the repo's `install.sh`. `mu update --check` only reports; `--tag mu-YYYYMMDD-<sha7>` picks a release; `--force` replaces a dev build, which is otherwise refused. On startup the TUI checks for a newer release at most once a day (cached in `~/.mu/update-check`) and shows a notice; it stays silent when offline or on a dev build. Turn it off with `[update] check = false` in config.toml. Checks use the unauthenticated GitHub API (60 requests an hour), or `gh` when the repo is private.

### Pinned omp

`upstream.toml` pins one oh-my-pi release for everything mu uses: the Rust crates (`tag = "v<ver>"` in `Cargo.toml`), `@oh-my-pi/pi-mnemopi` in `plugins/memory`, and the `omp` CLI mu runs as a subprocess. mu runs `$MU_OMP`, else `~/.mu/bin/omp`, else `omp` on PATH, so the `omp` you use interactively can update itself without affecting mu. `scripts/fetch-omp.sh <ver> <dest>` installs a checksummed omp binary.

### Manual build

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web,mu-predict,mu-memory}
scripts/build-mcp-http.sh                # target/release/mu-mcp-http (see docs/mcp.md)
mkdir -p ~/.mu/plugins
for p in gateway web memory predict; do ln -sfn "$PWD/plugins/$p" ~/.mu/plugins/$p; done
(cd plugins/memory && bun install)       # dev checkout: the memory plugin runs under Bun
```

Plugins are found in `~/.mu/plugins/*/plugin.toml`; `mu-gateway`, `mu-web`, `mu-predict` and `mu-mcp-http` are resolved next to the `mu` binary. `MU_GATEWAY_URL` / `MU_GATEWAY_TOKEN` bypass the gateway plugin.

### Syntax highlighting

Fenced code is highlighted with tree-sitter, one Cargo feature per language: `lang-rust`, `lang-json`, `lang-bash`, `lang-toml`, `lang-yaml`, `lang-python`, `lang-javascript`, `lang-typescript`, `lang-diff`, `lang-markdown` (all on by default; `all-langs` is the same set). Markdown fences inject the other grammars. Pick a subset:

```sh
cargo build --release --no-default-features --features lang-rust,lang-bash   # only these
cargo build --release --no-default-features                                   # no highlighting
```

Grammars load on first use of that language (about 1-2 MB resident each); idle memory is unaffected.

## Documentation

Usage, configuration and the individual features are documented in [`docs/`](docs/README.md).

## Development

### Tests

`cargo test --workspace` runs the unit tests; `scripts/build-mcp-http.sh test` runs the bridge's (it is built outside the workspace, see [MCP servers](docs/mcp.md)). `scripts/smoke.sh` runs live end-to-end checks (gateway, tmux TUI, subagents, compaction); `scripts/smoke-native.sh` checks a native API-key provider offline against a loopback fake API; `scripts/measure-idle.sh target/release/mu` reports idle memory.

`cargo test --workspace --locked -- --ignored contract_` checks the pinned omp's output formats against mu's parsers (needs network and the omp from `MU_OMP`/`~/.mu/bin`).

### Debugging

`MU_DUMP_REQUESTS=/path/file.jsonl` appends one JSON line per model request: timestamp, model, `max_tokens`, `thinking`, `output_config` and mu's level. The system prompt, messages and tools are only counted. The omp gateway's request log does not record the effort it sends upstream, so this file is the place to check it.

### Maintenance bot

A local systemd timer keeps mu current with oh-my-pi; everything is reported through GitHub issues and PRs. `scripts/maint/setup.sh [--profile NAME]` installs it (config in `~/.mu/maint/config.env`, budgets `MU_MAINT_*_USD`, `MU_MAINT_DRY_RUN=1` prints instead of mutating GitHub). Its LLM steps are the agents in `.omp/agents/` (`maint-triage`, `maint-fixer`, `maint-digest`), run by the installed `mu`.

| Level | Trigger | Action |
|---|---|---|
| S0 | `scripts/maint/check.sh` fails on `origin/main` | issue `maint:s0`; fixer PR `maint:needs-review`; no bump or publish |
| S1 | new omp release, bump fails checks | fixer adapts mu; PR `maint:bump` + `maint:needs-review` (+ `maint:red`) |
| S2 | green bump, no behaviour change reaching mu | PR `maint:bump` + `maint:auto`, squash-merged |
| S2r | green bump, behaviour change reaching mu | fixer reviews it; PR `maint:bump` + `maint:needs-review`, not merged. If the fixer makes no edit the PR is merged as S2 |
| S3 | weekly | issue `maint:digest` with omp changes worth porting |
| finding | the fixer notices a problem in mu the bump did not cause | issue `maint:finding` |
| publish | `origin/main` is green and newer than the last release | tag `mu-YYYYMMDD-<sha7>`; `.github/workflows/release.yml` builds and releases; the bot reinstalls unless a dev build is installed |

Nightly runs at 03:30, the digest on Mondays 05:00. The bot never edits `.github/`.

## License

MIT, see `LICENSE`. mu includes prompts and other material from [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT) and colour palettes from [omarchy](https://github.com/basecamp/omarchy) (MIT); see `THIRD-PARTY-NOTICES.md`.
