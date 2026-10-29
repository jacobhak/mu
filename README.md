# mu

I love omp, but in the age of AI, RAM ain't cheap and every session being the equivalent of a chrome tab simply isn't sustainable.

mu is a terminal coding agent with a Rust core (agent loop, TUI, sessions) built on the crates, CLI and auth gateway of [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp), and borrowing many of its prompts and much of its behaviour. Credit for the design goes to the omp authors.

## How mu is different

mu keeps most of what omp does and changes where the memory goes. A session holds your conversation, and anything else runs only while it's being used.

- **Small core.** A native binary holds the agent loop, the TUI and the built-in tools. An idle session uses about 2 MB.
- **Features start on use.** Web, memory, prediction and the model gateway are separate processes. Each starts on first use and exits after a few idle minutes. MCP servers start on their first tool call; their tool lists are cached so mu can advertise the tools without starting them.
- **Shared across sessions.** Sessions on one omp profile share one gateway (about 160 MB while it runs), and all sessions share one memory daemon.
- **Bulky content goes to disk.** Large tool output and images are written to files instead of being kept in memory.
- **Works with your omp setup.** mu uses omp's login, config, models, MCP setup, context files and skills. Sessions are in omp's format, so you can use both and switch either way.

The cost: features that live inside omp's runtime, such as TypeScript extensions, aren't available. The reasoning and measurements behind the design are in [`DESIGN.md`](DESIGN.md).

## Status

Early and experimental: started in October 2026 and changing quickly.

- No stable version. Releases are nightly builds of `main` (`mu-YYYYMMDD-<sha7>`); config, flags and plugin interfaces can change without notice.
- Needs a working omp setup: mu uses omp's credentials, `models.yml` and model roles, and has no login of its own; `/login` hands off to omp.
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

### Updating

`mu update` installs the latest release by running the repo's `install.sh`. `mu update --check` only reports; `--tag mu-YYYYMMDD-<sha7>` picks a release; `--force` replaces a dev build, which is otherwise refused. On startup the TUI checks for a newer release at most once a day (cached in `~/.mu/update-check`) and shows a notice; it stays silent when offline or on a dev build. Turn it off with `[update] check = false` in config.toml. Checks use the unauthenticated GitHub API (60 requests an hour), or `gh` when the repo is private.

### Pinned omp

`upstream.toml` pins one oh-my-pi release for everything mu uses: the Rust crates (`tag = "v<ver>"` in `Cargo.toml`), `@oh-my-pi/pi-mnemopi` in `plugins/memory`, and the `omp` CLI mu runs as a subprocess. mu runs `$MU_OMP`, else `~/.mu/bin/omp`, else `omp` on PATH, so the `omp` you use interactively can update itself without affecting mu. `scripts/fetch-omp.sh <ver> <dest>` installs a checksummed omp binary.

### Manual build

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web,mu-predict,mu-memory}
scripts/build-mcp-http.sh                # target/release/mu-mcp-http (see below)
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

## Use

```
mu [--model SEL] [--thinking off|low|medium|high|xhigh|max] [--profile NAME] [--theme NAME] [-c] [--resume [ID|PATH]]
mu -p PROMPT [--agent NAME] [--max-cost USD]
mu --acp
mu take <session|id>
mu review [--base REF] [--plan FILE] [--focus TEXT] [--out FILE] [--markdown]
mu update [--check] [--tag TAG] [--force]
```

`SEL` is `provider/model[:level]` or `@role`. `-c` resumes the latest session for the directory; `--resume` takes a session id or path, or opens a picker. `--profile` picks an omp profile (sets `OMP_PROFILE`); `--theme` overrides `[theme] source` for one run. Sessions are omp v3 JSONL under `~/.mu/sessions/` and open in omp.

Thinking levels are `off`, `low`, `medium`, `high`, `xhigh` and `max` (`minimal` reads as `low`). `xhigh` and `max` send the matching `output_config.effort` through the gateway, which clamps an effort the model does not support to the nearest one it does. Thinking budgets: low 4096, medium 8192, high 16384, xhigh 32768, max 64000, always capped to leave room for the answer.

`-p` runs one prompt and prints the answer. `--agent` runs it as a custom agent (see Custom agents); `--max-cost` aborts once the session has cost more than that many USD and always prints `mu: cost $X` to stderr.

Ghost text predicts the rest of the word or a prompt you repeat here; Tab or → accepts. It is configured under `[predict]` (see Config). `mu-predict bench` replays your own prompt history to measure keystrokes saved.

Typing `/` opens a command menu with descriptions and argument completion (↑/↓, Tab, Enter, Esc).

Slash commands: `/new /resume /model /thinking /compact /plan /plans /session /rename /todo /skills /plugins /login [provider|mcp:<name>] /copy /reply /image /jobs /hotkeys /exit`, plus prompt templates from `.omp/commands`, `<agentDir>/commands`, `.claude/commands`, `.omp/prompts` and `/skill:<name>`.

While a turn runs, Enter steers (the message is delivered after the current tool call), Ctrl+Q queues a follow-up, Esc aborts and returns queued text to the composer.

`/copy` opens a picker of recent replies, code blocks and bash commands; `/copy code`, `/copy cmd` and `/copy link` copy the latest one directly. Under tmux the text goes through `tmux load-buffer -w`; otherwise via OSC 52, plus `wl-copy`/`xclip` when a local display exists.

`Ctrl+R` or `/reply` opens a reply picker on the newest finished assistant text (it works during a turn and shows the last finished reply, not the streaming one). `[` and `]` switch to older and newer replies. Up/Down (or j/k) move between sentences, list items, headings, table rows and code lines; Left/Right (h/l) move word by word; PgUp/PgDn, Home/End jump. Space marks the current unit, `v` starts a word-level visual selection (Left/Right grow or shrink it by a word, Up/Down by a unit; Space or Enter turns it into a mark, Esc drops it), `c` clears the marks. Enter quotes the marks (or the current unit when nothing is marked) as `> ` blockquotes at the end of the composer and puts the cursor on the blank line after the first quote; Esc closes without a change.

Images: paste an image file path, run `/image <path>`, press Ctrl+V (reads `wl-paste`/`xclip`), or paste an image from the terminal clipboard when it supports kitty's OSC 5522 (tmux needs `allow-passthrough on`). PNG, JPEG, GIF and WebP up to 5 MB; no resizing.

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes; `/jobs` lists jobs and `/jobs kill <id|all>` cancels them. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

`mu review` runs `/review` headlessly, for detached or scripted runs: the same request, `review-lead` and workers, with no TUI and no session. `--base` defaults to `HEAD` (uncommitted changes); `--plan` is the approved plan file. It prints the verdict as JSON (`verdict`: `pass`, `fail`, `nothing` or `error`; `findings` with a `blocking` flag per finding) to stdout or `--out`, and progress to stderr. Exit codes: 0 pass or nothing to review, 1 fixes needed, 2 the review did not complete. The reviewer runs with the configured `review` role.

### Detached runs and taking them over

`mu -p` prints `session: <path>` to stderr as soon as the session exists. Exit codes: 0 done, 1 error, abort or cost cap, 3 interrupted and resumable. `kill -USR1 <pid>` (never a process group) asks a `-p` run to stop: it finishes the running tool, skips the rest of that batch, ends the turn, releases the session and exits 3 with `mu: interrupted; resume with: mu --resume <path>`. A second `SIGUSR1` cancels the current step. Either way the session loads without repair. `mu take <session|id>` does this for you: it signals the run, waits until the session is free, changes to the session's directory and execs `mu --resume <path>`. It refuses unless stdin and stdout are terminals, and refuses a session open in an interactive mu or held by a process that left no owner record. Ctrl-C while waiting sends the second signal; a second Ctrl-C gives up waiting. Every open session is recorded in `$MU_HOME/run/<id>.json` (`pid`, `print_run`, `started`, `session`), held under `flock` by its owner, so a busy-lock error can name the pid; the files are safe to delete while no mu runs. The session file format is unchanged. A taken-over session loses `--max-cost` and a custom `--agent` prompt; `ask` and jobs come back.

Stop details: a stop that arrives before the next tool starts skips every call of that batch, so the run ends at once; one that arrives during a tool lets that tool finish. Any second `SIGUSR1` cancels the current step, including one sent by `mu take` after a manual `kill -USR1` (or by two takes). A fatal error (such as a failed provider request) exits 1 even after a stop; non-fatal errors (a truncated reply, a failed auto-compaction) do not change exit 3. The pid in an owner record is only meaningful on the same host and pid namespace: `mu take` from another container or namespace that shares `MU_HOME` and the session directory can signal the wrong process or none, so run it where the `-p` run lives.

`mu notify <session|id|path> [--from NAME] <text|->` leaves a message for a session and, if an interactive mu has it open, wakes it: that mu starts a turn by itself, shows `↳ mu notify from NAME: …` and the model sees the text in a `<system-notice>` user message (`-` reads the text from stdin; at most 64 KiB; `--from` defaults to `another process`). It prints one line: `delivered (pid N)` or `queued (<reason>)`, where the reason is `not running`, `pid N does not take messages`, `owner unknown`, `owner changed` or `cannot signal pid N: <error>`; exit 0 for delivered or queued, 1 for an error (unknown session, empty or oversized text). Messages are files in `$MU_HOME/inbox/<id>/` (directory mode 0700), written atomically before the owner is looked at, and removed once the notice is in the session file. Only an interactive mu installs the `SIGUSR2` handler and says so in its owner record (`inbox: true`); `-p`, subagents and ACP are never signalled (the signal would kill them), so a message for them waits until the session is opened again (`mu take`, `--resume`), which announces it (`N message(s) from mu notify waiting`) and sends it with the next prompt. A message that arrives mid-turn goes out after the current tool batch or reply, in the same turn. Every `mu notify` call deletes queued messages older than 30 days. A turn that began without user input (a `mu notify` message or a finished background job) rings the terminal when it ends or when it asks you something (`ask`, plan review): BEL, plus an OSC 9 notification under tmux (needs `allow-passthrough`) and in Ghostty, WezTerm, iTerm2 and Warp; omp's `completion: {notify: off}` in `config.yml` turns it off. Bash commands (and `!cmd`) get `PI_SESSION_FILE` and `PI_SESSION_ID` (empty without a session), as in omp.

### MCP servers

mu reads omp's MCP config unchanged: `<cwd>/.omp/mcp.json`, `<agentDir>/mcp.json` and the other files omp reads (`.mcp.json` variants, `./mcp.json`), merged the way omp merges them, with `${VAR}` / `${VAR:-default}` expansion, `disabledServers` / `enabledServers` and `enabled`/`timeout`/`instructions`/`requestIdFormat` fields. Tools are named `mcp__<server>_<tool>`, as in omp.

- **stdio** servers run as a child process. **http** servers (`"type": "http"`, or a `url` with no type) run through `mu-mcp-http`, a small bridge plugin that keeps TLS out of the core. Legacy `"type": "sse"` servers are skipped with a warning. The bridge is its own Cargo project outside the workspace (`scripts/build-mcp-http.sh`), because building it with `mu` would unify features and give the core's HTTP client rustls without a crypto provider; `scripts/build-mcp-http.sh check` fails if the core ever depends on rustls.
- Nothing starts at launch. A server's tool list is cached in `~/.mu/mcp/tools/` (keyed by a hash of its config), so its tools are advertised without starting it; it starts on the first call. The first session after a config change starts the server once to list its tools. A server's process lives for the session (`[mcp] lifetime = "idle:10m"` in `config.toml` changes that).
- **OAuth** goes through omp: mu runs `omp token <credential>` (through the auth broker when the gateway is running), never reads omp's credential store and never holds a refresh token. A server that needs a login is reported as `needs login`; log in from interactive mu with `/login mcp:<name>` (or pick it in `/login`), which hands the terminal to omp's own flow (`omp -p --no-session '/mcp reauth <name>'`) and makes the server usable in the same session. Headless, ACP and subagent runs cannot log in. Over SSH the browser needs a port forward to omp's local callback port, as for omp itself.
- **Project config can run commands and contact URLs**: a repository's `.omp/mcp.json` starts whatever it names, exactly as in omp. Only use mu in directories you trust.
- Not supported: `/mcp` management, MCP resources and prompts, and Claude/Cursor config imports.
- Notes: with a cold cache the first turn waits (up to 30 s per server, 60 s when `timeout` is 0) for servers to list their tools; Esc or Ctrl-C leaves it. The broker is used for token refresh only once the gateway plugin is already running, so the first listing of a session may fetch the token without it. Config warnings (a skipped SSE server, a bad entry) show in the TUI and on stderr for `-p`; ACP and subagents do not show them.

### Agent Client Protocol

`mu --acp` serves [ACP](https://agentclientprotocol.com) over stdio, so ACP clients such as [Paseo](https://paseo.sh) can drive it. In Paseo's `~/.paseo/config.json`:

```json
{"agents": {"providers": {"mu": {"extends": "acp", "label": "mu", "command": ["mu", "--acp"], "env": {"OMP_PROFILE": "private"}}}}}
```

Supported: `session/new`, `load` (replays history), `resume`, `close`, `prompt` (text, images, embedded resources), `cancel`, and `set_config_option` for `model` and `thinking`. `model` takes an advertised `provider/model` id or a role selector such as `@slow` or `@slow:high`, resolved as `--model` is (the advertised options list only concrete models); an unconfigured role fails with an invalid-params error. Updates cover message and thought chunks, tool calls, the todo list as a plan, context usage, and session titles. Each session runs its own agent in the `cwd` it was opened with.

Not supported: `mcpServers` in `session/new` (ignored; mu uses its own plugins), the `ask` and `propose_plan` tools (hidden from the model), permission prompts (mu never asks), and streaming bash output (the full output arrives when the tool ends). Setting a config option while a prompt runs is rejected. A failed turn ends with `end_turn` plus an `**Error:**` message chunk, not a JSON-RPC error.

### Custom agents

Custom agents are omp-format `.md` files in `.omp/agents` and `<agentDir>/agents` (frontmatter `name`, `description`, `tools`, `model` as a list of `@role`/`provider/model`, `thinkingLevel`). `mu -p PROMPT --agent NAME` runs one; they also appear in the `task` tool.

### Memory

The memory plugin ships as two binaries: `mu-memory`, a small Rust shim that each `mu` session starts, and `mu-memory-daemon`, built by `bun build --compile`. The shim starts one shared daemon per `$MU_HOME` on demand (socket `$MU_HOME/memory/sock`, log `$MU_HOME/logs/memory-daemon.log`) and relays the session to it, so several `mu` sessions share one Bun runtime and embedder. The daemon exits after 10 idle minutes without a session (`MU_MEMORY_IDLE_SECS`).

## Config

`~/.mu/config.toml` is optional, and so is every key in it. Apart from the compaction example, the values below are the defaults.

```toml
[compaction.model_thresholds]   # same keys/values as omp's compaction.modelThresholds; overrides per key
"anthropic/claude-haiku-5-5" = "f200000"

[theme]
source = "tokyo-night"  # default; or "terminal" (follow your terminal's palette), "omarchy", a bundled or ~/.mu/themes name, or a colors.toml path
# accent = "blue"     # palette key or "#rrggbb"
cards = "filled"      # or "outline"

[predict]
enabled = true
threshold = 0.3       # minimum confidence for ghost text

[edit]
mode = "hashline"     # or "sloppy": find/replace by text, no line tags

[mcp]
lifetime = "session"  # MCP server processes: "session", "idle:10m" or "call"

[update]
check = true          # daily release check at startup
```

## Development

### Tests

`cargo test --workspace` runs the unit tests; `scripts/build-mcp-http.sh test` runs the bridge's (it is built outside the workspace, see MCP servers). `scripts/smoke.sh` runs live end-to-end checks (gateway, tmux TUI, subagents, compaction); `scripts/measure-idle.sh target/release/mu` reports idle memory.

`cargo test --workspace --locked -- --ignored contract_` checks the pinned omp's output formats against mu's parsers (needs network and the omp from `MU_OMP`/`~/.mu/bin`).

### Debugging

`MU_DUMP_REQUESTS=/path/file.jsonl` appends one JSON line per model request: timestamp, model, `max_tokens`, `thinking`, `output_config` and mu's level. The system prompt, messages and tools are only counted. The omp gateway's request log does not record the effort it sends upstream, so this file is the place to check it.

### Maintenance bot

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

## License

MIT, see `LICENSE`. mu includes prompts and other material from [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT) and colour palettes from [omarchy](https://github.com/basecamp/omarchy) (MIT); see `THIRD-PARTY-NOTICES.md`.
