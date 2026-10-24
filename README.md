# mu

A small, memory-lean coding agent. Rust core; everything heavy (gateway, web, memory) is an out-of-process plugin that starts on first use and exits when idle. It talks to models through the omp auth gateway and reads omp's config (`modelRoles`, `compaction.modelThresholds`, `models.yml`).

## Build and install

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web}
mkdir -p ~/.mu/plugins
for p in gateway web memory; do ln -sfn "$PWD/plugins/$p" ~/.mu/plugins/$p; done
(cd plugins/memory && bun install)       # memory plugin needs Bun on PATH
```

Plugins are found in `~/.mu/plugins/*/plugin.toml`; `mu-gateway` and `mu-web` are resolved next to the `mu` binary. `MU_GATEWAY_URL` / `MU_GATEWAY_TOKEN` bypass the gateway plugin.

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

`SEL` is `provider/model[:level]` or `@role`. `-c` resumes the latest session for the directory, `--resume` opens a picker. `-p` runs one prompt and prints the answer. Tab completes slash commands.

Slash commands: `/new /resume /model /thinking /compact /plan /session /rename /todo /skills /plugins /copy /image /jobs /hotkeys /exit`, plus prompt templates from `.omp/commands`, `<agentDir>/commands`, `.claude/commands`, `.omp/prompts` and `/skill:<name>`.

While a turn runs, Enter steers (the message is delivered after the current tool call), Ctrl+Q queues a follow-up, Esc aborts and returns queued text to the composer.

`/copy` opens a picker of recent replies, code blocks and bash commands; `/copy code`, `/copy cmd` and `/copy link` copy the latest one directly. Under tmux the text goes through `tmux load-buffer -w`; otherwise via OSC 52, plus `wl-copy`/`xclip` when a local display exists.

Images: paste an image file path, run `/image <path>`, press Ctrl+V (reads `wl-paste`/`xclip`), or paste an image from the terminal clipboard when it supports kitty's OSC 5522 (tmux needs `allow-passthrough on`). PNG, JPEG, GIF and WebP up to 5 MB; no resizing.

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes; `/jobs` lists jobs and `/jobs kill <id|all>` cancels them. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

## Config

`~/.mu/config.toml` (optional):

```toml
[compaction.model_thresholds]      # same keys/values as omp's compaction.modelThresholds; overrides per key
"anthropic/claude-haiku-5-5" = "f200000"
```

Sessions are omp v3 JSONL under `~/.mu/sessions/` and open in omp.

## Tests

`cargo test --workspace` runs the unit tests. `scripts/smoke.sh` runs live end-to-end checks (gateway, tmux TUI, subagents, compaction); `scripts/measure-idle.sh target/release/mu` reports idle memory.

Design notes and milestones: `DESIGN.md`.
