# Config

`~/.mu/config.toml` is optional, and so is every key in it. The values below are the defaults. Model roles, per-model limits and compaction thresholds live in [`models.toml`](model-roles.md#models-toml), not here; a `[compaction]` table left in `config.toml` is no longer read and warns at startup.

```toml
[theme]
source = "tokyo-night"  # default; or "terminal" (follow your terminal's palette), "omarchy", a bundled or ~/.mu/themes name, or a colors.toml path
# accent = "blue"     # palette key or "#rrggbb"
cards = "filled"      # or "outline"

# [keys.chat]         # rebind any shortcut: action = "ctrl+t" or ["ctrl+t", "f5"]; see keys.md
# toggle_thinking = "f5"

[predict]
enabled = true
threshold = 0.3       # minimum confidence for ghost text; 0.15 saves a few more keystrokes but shows about 55% more wrong ghosts
engine = "ngram"     # or "smollm" (opt-in, see ghost-text.md); any other value warns and means "ngram"
threads = 4          # CPU threads for SmolLM; 0 = all cores. Linux only: macOS runs SmolLM on the GPU
other_histories = true  # also learn from omp's history.db and Claude Code's history.jsonl (read-only); false = mu's own history only

[ask]
nudge = true         # when a turn ends with a decision question in prose, remind the model to use `ask` instead

[edit]
mode = "hashline"     # or "sloppy": find/replace by text, no line tags

[retry]               # waiting out a provider's Retry-After (a 429, mostly); a request is retried when either holds
max_wait = "2m"       # Retry-After up to this: "90s", "30m", "2h", "1d", "1h30m". Longer fails at once
# context_below = 50  # or the context is less than this percent full (default: off)
max_attempts = 5     # attempts per request, retries included: 1 to 20; any other value warns and means 5
# [retry.providers.anthropic]   # both keys again, for one provider
# max_wait = "3h"
# context_below = 60

[status]              # the footer is mu's own layout (omp's `statusLine` is not read); these set when its context share turns yellow (warn) or red (err); a level is reached at its percent of the window or its token count, whichever is lower
warn_percent = 50
warn_tokens = 270000
err_percent = 70
err_tokens = 500000

[permissions]
mode = "yolo"          # ask | edits | yolo (see permissions.md); --permissions overrides it for one run
readonly_bash = true   # the built-in read-only command set counts as reading; false makes every bash call ask unless a rule allows it

# [permissions.rules]  # tool-name globs, or pattern tables for bash, write, edit, read, grep, glob and fetch
# bash = { "cargo test *" = "allow", "git push *" = "deny" }
# "mcp__github_*" = "allow"

[bell]                # what rings the terminal (BEL, plus a toast in some terminals; see detached-runs.md); omp's `completion: {notify: off}` silences both
turn_end = true       # a turn you typed ends with no background command or subagent running
attention = true      # a running turn asks you something or needs an approval or plan review, or a pty command waits for keyboard input, whoever started it

# [display]
# hide_thinking_block = false  # hide the model's thinking blocks; unset follows omp's `hideThinkingBlock`. `mu models import --from pi` sets it from pi's setting

[mcp]
lifetime = "session"  # MCP server processes: "session", "idle:10m" or "call"

# [gateway]
# plugin = "gateway-pi"  # the installed plugin that serves as the gateway; unset, the first loaded plugin with `provider = "gateway"`. A name that is not loaded, or has no `provider = "gateway"`, is an error; MU_GATEWAY_URL and MU_GATEWAY_TOKEN still bypass it

[task]                # a subagent's soft request budget: cross it and it is told to wrap up, at 1.5x it is stopped and asked for what it has, and five requests later the run ends; 0 turns the guard off
soft_request_budget = 200  # a scout is capped at 100 however high this is
budget_notice = true       # the wrap-up notice at the budget
max_cost = 5.0             # most a launched worker may spend (USD) on the model calls it makes through this session; 0 turns the cap off
# launcher = ["mu-remote", "launch", "cluster"]  # a command that runs the kinds in `launch` in place of `mu --headless` (see below)
# launch = ["task"]          # agent kinds that go through the launcher; any other kind runs here

# [agent]                # `mu agent` with no client attached (see remote.md#when-nobody-is-attached)
# notify_command = "notify-send \"mu $1\" \"$2\""  # run under sh -c when a question or approval starts waiting; $1 is the session id, $2 a one-line summary
# approval_timeout = 0   # seconds after which a waiting approval is answered "unavailable" (a question is cancelled); 0 waits for ever
# state_key = "off"      # "ssh": seal auth.json and plugin secrets under a key a client unlocks with its SSH ed25519 key (see remote.md#encrypted-state)
# state_key_file = "/run/secrets/state-key"  # unattended instead: a 32-byte key (raw or 64 hex characters) read at start; implies encryption, wins over the command
# state_key_command = "cat /run/secrets/state-key"  # the same, printed by a command run under sh -c

# [sessions]             # the Sessions tab, beside this machine's (see remote.md#for-extension-authors)
# sources = [["mu-remote", "rows"]]  # commands that print other machines' agents as JSON lines: id, cwd, started, title, client, waiting, host, hint, attach

[update]
check = true          # daily release check at startup

[secrets]
env = []              # extra environment variables whose values count as secrets (redaction is always on; see secrets.md)

[memory]
review = true         # review sessions in the background and save what is worth keeping (see memory.md)
user_cap = 1500       # characters in user.md
project_cap = 4000    # characters in a repository's memory.md
```

`/theme` opens a picker of the bundled palettes, your `~/.mu/themes/*.toml`, `terminal`, and `omarchy` when its colours file exists, each tagged dark or light. The screen takes the highlighted theme as you move (the picker, composer and status line recolour; output already printed keeps its colours), Esc puts the old one back, and Enter keeps it and writes `[theme] source` to `config.toml`, leaving its comments and other keys alone. `/theme <name>` does the same without the picker. `terminal` reads the terminal's colours once, at start-up, so it can be previewed only when it was the theme then; chosen from another theme it is saved and takes effect on the next start. `--theme` still overrides the saved source for a run. `mu models import --from pi` also writes `[theme] source` from pi's `theme` (`dark` is `tokyo-night`, `light` is `catppuccin-latte`, `system` is `terminal`) unless one is set.

With `[task] launcher` and `[task] launch` set, a `task` subagent of a listed kind (`task`, `scout`, a custom agent's name) runs through the launcher, a command that stands in for `mu --headless` (usually it runs the subagent on another machine). Kinds not listed and `/review` run here, and so do a launched subagent's own subagents. Before each launch mu commits the working tree, untracked files included, as `refs/mu/task/<id>` (deleted when the run ends) and hands the launcher that commit with its full history as a git bundle. What the launcher's worker changed comes back as a bundle that mu imports as `refs/mu/result/<id>`; the task's result then ends with a line saying whether it was applied. The launcher's environment and the files involved are the launcher contract, in [remote.md](remote.md#for-extension-authors). `refs/mu/result/*` older than 14 days are deleted before each launch.

A launched worker has no credentials: its model calls and web searches go through this session. It may call only its task's model, that model's fallback roles and `@smol` (for titles); any other model is refused, and a worker's own subagents get no more than that. Calls the worker makes after it has sent its `result` are refused and any still running are cancelled, and at most eight run at once. The cost of its calls counts in this session's `--max-cost` and, past `[task] max_cost`, further calls fail with `task spend cap reached` and the task's result says so; one-off calls such as a title are not priced, so they do not count. A worker's web search goes through the same permission check as yours, as a network call: in `ask` mode it asks first, and the hosts it returns are noted for the worker only, so a `fetch` of one of them from this session still asks. The task carries mu's version: a worker of another version refuses the task with one line, which the task shows as its error. A line from the worker longer than 64 MiB and an image over 20 MB are dropped; a worker's session lines must be session entries, the first of them a header naming this session as its parent, and `cost` lines are dropped.
