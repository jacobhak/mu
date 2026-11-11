# Using mu

```
mu [--model SEL] [--thinking off|low|medium|high|xhigh|max] [--profile NAME] [--theme NAME] [-c] [--resume [ID|PATH]]
mu -p PROMPT [--agent NAME] [--max-cost USD]
mu --acp
mu take <session|id>
mu review [--base REF] [--plan FILE] [--focus TEXT] [--out FILE] [--markdown]
mu update [--check] [--tag TAG] [--force]
mu migrate-profile NAME [--dry-run]
```

`SEL` is `provider/model[:level]` or `@role`. `-c` resumes the latest session for the directory; `--resume` takes a session id or path, or opens a picker. `--profile` picks an omp profile (sets `OMP_PROFILE`); `--theme` overrides `[theme] source` for one run. Sessions are omp v3 JSONL under `<profile home>/sessions/` and open in omp.

## Profile home

Everything one omp profile's work produces lives in its profile home: `~/.mu` (`$MU_HOME`) without a profile, `~/.mu/profiles/<name>` under one. That covers sessions, their owner records and inbox, artifacts and `local://` files, image blobs, durable plans and deliveries, memory and the session search index, prompt history, the ghost-text corpus, cached MCP tool lists, API keys and model caches. Profiles never see each other's: `-c`, `--resume`, `/plans` and the jobs panel show only the active profile's. Install-wide files stay in `~/.mu`: `bin/`, `plugins/`, `logs/`, `themes/`, `models/` and `config.toml`.

Before this split, mu kept that state in `~/.mu` for every profile. `mu migrate-profile NAME` moves it into `NAME`'s profile home once: close every mu first (it refuses while a session in `~/.mu` is open), and `--dry-run` lists what would move. Where the profile already has a file of the same name, the profile's copy wins: identical files are dropped, prompt history is joined (older lines first) and any other clash stays in place and is reported (exit 1). Session headers, plans and deliveries name sessions by absolute path; those paths are rewritten to the new location so subagent and fork links survive.

Thinking levels are `off`, `low`, `medium`, `high`, `xhigh` and `max` (`minimal` reads as `low`). Through the gateway, `xhigh` and `max` send the matching `output_config.effort`. An effort the model does not serve goes to the tier the model maps it to, if any (a route serving only low/high/max takes `medium` as `high`), otherwise to the nearest lower tier it serves. On a native provider (a key added with `/login` or `mu auth`, or a ChatGPT sign-in; see [Providers](providers.md)) each level maps to the nearest effort in the model's own ladder instead, and what is sent differs per provider and model kind (adaptive, budget, effort).

`-p` runs one prompt and prints the answer. `--agent` runs it as a [custom agent](custom-agents.md); `--max-cost` aborts once the session has cost more than that many USD and always prints `mu: cost $X` to stderr. Exit codes, interrupting and taking over a `-p` run are in [Detached runs](detached-runs.md).

While a turn runs, Enter steers (the message is delivered after the current tool call), Ctrl+Q queues a follow-up, Esc aborts and returns queued text to the composer.

A prompt can also be sent later, without staying at the keyboard: `/schedule 10m <prompt>` (see [Scheduled prompts](schedule.md)).

Ctrl+C only exits: the first press says so and changes nothing else, and the second exits mu whatever is on screen (a running turn, a panel, a wizard, an embedded terminal). Esc is what backs out of those, one screen at a time, and in an embedded terminal it kills the command instead of being typed into it.

Shift+Enter inserts a newline only in terminals with the kitty keyboard protocol. In tmux and other terminals, use Ctrl+J or Alt+Enter.

File tools (`read`, `write`, `edit`, `grep`, `glob`) resolve relative paths against the session's directory, not against wherever a `cd` in `bash` has moved the shell. When the shell is elsewhere and a call used a relative path, the result ends with a note naming both directories, so a mistaken edit shows on the first call. In `edit`'s `sloppy` mode, an unindented line such as `*** Find all` that looks like a header but is not one is refused instead of being written into the file; indent a line that is meant as text. An `Insert Before` or `Insert After` whose body repeats its `*** Find` lines (the Find is only the anchor) is refused too, unless the anchor is under 8 characters. The persistent shell keeps `export`s between `bash` calls, so a call that sets or changes an exported variable ends with a reminder naming it (never its value).

Besides the built-in slash commands, prompt templates from `.omp/commands`, `<agentDir>/commands`, `.claude/commands`, `.omp/prompts`, and the harness-neutral `.agent/commands` and `.agents/commands` (in the project and in `~`) run as slash commands, and skills as `/skill:<name>`. On a name clash the earlier directory in that list wins.

`/copy` opens a picker of recent replies, code blocks and bash commands; `/copy code`, `/copy cmd` and `/copy link` copy the latest one directly. Under tmux the text goes through `tmux load-buffer -w`; otherwise via OSC 52, plus `wl-copy`/`xclip` when a local display exists.

`/memory` opens the memory picker: the user's memory file, this repository's, each note, and the history. Enter on a file opens it in `$VISUAL` or `$EDITOR`. Enter on the history lists the last 50 commits; Enter on one shows its message and diff in the transcript, and `r` reverts it. The history is only there when git is installed and memory has been written. `/memory` waits for a running turn to finish.

Images: paste an image file path, run `/image <path>`, press Ctrl+V (reads `wl-paste`/`xclip`), or paste an image from the terminal clipboard when it supports kitty's OSC 5522 (tmux needs `allow-passthrough on`). PNG, JPEG, GIF and WebP up to 5 MB; no resizing.
