# mu

A small, memory-lean coding agent. Rust core; everything heavy (gateway, web, memory) is an out-of-process plugin that starts on first use and exits when idle. It talks to models through the omp auth gateway and reads omp's config (`modelRoles`, `compaction.modelThresholds`, `models.yml`).

mu is a personal reimplementation of [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp) and is built on it: it uses omp's Rust crates, CLI and auth gateway, and borrows many of its prompts and much of its behaviour. Credit for the design goes to the omp authors.

## Status

Early and experimental: started in October 2026 and changing quickly.

- No stable version. Releases are nightly builds of `main` (`mu-YYYYMMDD-<sha7>`); config, flags and plugin interfaces can change without notice.
- Needs a working omp setup: mu uses omp's credentials, `models.yml` and model roles, and has no login of its own.
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

This installs `mu`, the plugins and a pinned `omp` under `~/.mu/{bin,plugins}` and links `~/.local/bin/mu` (not when `MU_HOME` is set). The memory plugin is skipped without Bun on PATH. `--skip-if-dev` leaves an installed dev build (dirty, unknown, or not on `origin/main`) alone. `mu --version` prints `mu 0.1.0 (<sha>) omp <version>`.

### Updating

`mu update` installs the latest release by running the repo's `install.sh`. `mu update --check` only reports; `--tag mu-YYYYMMDD-<sha7>` picks a release; `--force` replaces a dev build, which is otherwise refused. On startup the TUI checks for a newer release at most once a day (cached in `~/.mu/update-check`) and shows a notice; it stays silent when offline or on a dev build. Turn it off with `[update] check = false` in config.toml. Checks use the unauthenticated GitHub API (60 requests an hour), or `gh` when the repo is private.

### Pinned omp

`upstream.toml` pins one oh-my-pi release for everything mu uses: the Rust crates (`tag = "v<ver>"` in `Cargo.toml`), `@oh-my-pi/pi-mnemopi` in `plugins/memory`, and the `omp` CLI mu runs as a subprocess. mu runs `$MU_OMP`, else `~/.mu/bin/omp`, else `omp` on PATH, so the `omp` you use interactively can update itself without affecting mu. `scripts/fetch-omp.sh <ver> <dest>` installs a checksummed omp binary.

### Manual build

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web,mu-predict}
mkdir -p ~/.mu/plugins
for p in gateway web memory predict; do ln -sfn "$PWD/plugins/$p" ~/.mu/plugins/$p; done
(cd plugins/memory && bun install)       # memory plugin needs Bun on PATH
```

Plugins are found in `~/.mu/plugins/*/plugin.toml`; `mu-gateway`, `mu-web` and `mu-predict` are resolved next to the `mu` binary. `MU_GATEWAY_URL` / `MU_GATEWAY_TOKEN` bypass the gateway plugin.

### Syntax highlighting

Fenced code is highlighted with tree-sitter, one Cargo feature per language: `lang-rust`, `lang-json`, `lang-bash`, `lang-toml`, `lang-yaml`, `lang-python`, `lang-javascript`, `lang-typescript`, `lang-diff`, `lang-markdown` (all on by default; `all-langs` is the same set). Markdown fences inject the other grammars. Pick a subset:

```sh
cargo build --release --no-default-features --features lang-rust,lang-bash   # only these
cargo build --release --no-default-features                                   # no highlighting
```

Grammars load on first use of that language (about 1-2 MB resident each); idle memory is unaffected.

## Use

```
mu [--model SEL] [--thinking off|low|medium|high] [-c] [--resume [PATH]] [-p PROMPT]
```

`SEL` is `provider/model[:level]` or `@role`. `-c` resumes the latest session for the directory, `--resume` opens a picker. `-p` runs one prompt and prints the answer. Typing `/` opens a command menu with descriptions and argument completion (↑/↓, Tab, Enter, Esc).

Ghost text predicts the rest of the word or a prompt you repeat here; Tab or → accepts. `[predict] enabled/threshold` in config.toml. `mu-predict bench` replays your own prompt history to measure keystrokes saved.

Slash commands: `/new /resume /model /thinking /compact /plan /session /rename /todo /skills /plugins /copy /image /jobs /hotkeys /exit`, plus prompt templates from `.omp/commands`, `<agentDir>/commands`, `.claude/commands`, `.omp/prompts` and `/skill:<name>`.

While a turn runs, Enter steers (the message is delivered after the current tool call), Ctrl+Q queues a follow-up, Esc aborts and returns queued text to the composer.

`/copy` opens a picker of recent replies, code blocks and bash commands; `/copy code`, `/copy cmd` and `/copy link` copy the latest one directly. Under tmux the text goes through `tmux load-buffer -w`; otherwise via OSC 52, plus `wl-copy`/`xclip` when a local display exists.

Images: paste an image file path, run `/image <path>`, press Ctrl+V (reads `wl-paste`/`xclip`), or paste an image from the terminal clipboard when it supports kitty's OSC 5522 (tmux needs `allow-passthrough on`). PNG, JPEG, GIF and WebP up to 5 MB; no resizing.

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes; `/jobs` lists jobs and `/jobs kill <id|all>` cancels them. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

### Agent Client Protocol

`mu --acp` serves [ACP](https://agentclientprotocol.com) over stdio, so ACP clients such as [Paseo](https://paseo.sh) can drive it. In Paseo's `~/.paseo/config.json`:

```json
{"agents": {"providers": {"mu": {"extends": "acp", "label": "mu", "command": ["mu", "--acp"], "env": {"OMP_PROFILE": "private"}}}}}
```

Supported: `session/new`, `load` (replays history), `resume`, `close`, `prompt` (text, images, embedded resources), `cancel`, and `set_config_option` for `model` and `thinking`. Updates cover message and thought chunks, tool calls, the todo list as a plan, context usage, and session titles. Each session runs its own agent in the `cwd` it was opened with.

Not supported: `mcpServers` in `session/new` (ignored; mu uses its own plugins), the `ask` and `propose_plan` tools (hidden from the model), permission prompts (mu never asks), and streaming bash output (the full output arrives when the tool ends). Setting a config option while a prompt runs is rejected. A failed turn ends with `end_turn` plus an `**Error:**` message chunk, not a JSON-RPC error.

## Config

`~/.mu/config.toml` (optional):

```toml
[compaction.model_thresholds]      # same keys/values as omp's compaction.modelThresholds; overrides per key
"anthropic/claude-haiku-5-5" = "f200000"
```

Custom agents: omp-format `.md` files in `.omp/agents` and `<agentDir>/agents` (frontmatter `name`, `description`, `tools`, `model` as a list of `@role`/`provider/model`, `thinkingLevel`). `mu -p PROMPT --agent NAME` runs one; they also appear in the `task` tool. `--max-cost USD` (with `-p`) aborts when the session cost passes the cap and always prints `mu: cost $X` to stderr.

Sessions are omp v3 JSONL under `~/.mu/sessions/` and open in omp.

## Tests

`cargo test --workspace` runs the unit tests. `scripts/smoke.sh` runs live end-to-end checks (gateway, tmux TUI, subagents, compaction); `scripts/measure-idle.sh target/release/mu` reports idle memory.

`cargo test --workspace --locked -- --ignored contract_` checks the pinned omp's output formats against mu's parsers (needs network and the omp from `MU_OMP`/`~/.mu/bin`).

## Maintenance bot

A local systemd timer keeps mu current with oh-my-pi; everything is reported through GitHub issues and PRs. `scripts/maint/setup.sh [--profile NAME]` installs it (config in `~/.mu/maint/config.env`, budgets `MU_MAINT_*_USD`, `MU_MAINT_DRY_RUN=1` prints instead of mutating GitHub). Its LLM steps are the agents in `.omp/agents/` (`maint-triage`, `maint-fixer`, `maint-digest`), run by the installed `mu`.

| Level | Trigger | Action |
|---|---|---|
| S0 | `scripts/maint/check.sh` fails on `origin/main` | issue `maint:s0`; fixer PR `maint:needs-review`; no bump or publish |
| S1 | new omp release, bump fails checks | fixer adapts mu; PR `maint:bump` + `maint:needs-review` (+ `maint:red`) |
| S2 | green bump, no behaviour change | PR `maint:bump` + `maint:auto`, squash-merged |
| S2r | green bump, behaviour change | PR `maint:bump` + `maint:needs-review`, not merged |
| S3 | weekly | issue `maint:digest` with omp changes worth porting |
| publish | `origin/main` is green and newer than the last release | tag `mu-YYYYMMDD-<sha7>`; `.github/workflows/release.yml` builds and releases; the bot reinstalls unless a dev build is installed |

Nightly runs at 03:30, the digest on Mondays 05:00. The bot never edits `.github/`.

Design notes and milestones: `DESIGN.md`.

## License

MIT, see `LICENSE`. mu includes prompts and other material from [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT) and colour palettes from [omarchy](https://github.com/basecamp/omarchy) (MIT); see `THIRD-PARTY-NOTICES.md`.
