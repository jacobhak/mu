# Config

`~/.mu/config.toml` is optional, and so is every key in it. The values below are the defaults. Model roles, per-model limits and compaction thresholds live in [`models.toml`](model-roles.md#models-toml), not here; a `[compaction]` table left in `config.toml` is no longer read and warns at startup.

```toml
[theme]
source = "tokyo-night"  # default; or "terminal" (follow your terminal's palette), "omarchy", a bundled or ~/.mu/themes name, or a colors.toml path
# accent = "blue"     # palette key or "#rrggbb"
cards = "filled"      # or "outline"

[predict]
enabled = true
threshold = 0.3       # minimum confidence for ghost text; 0.15 saves a few more keystrokes but shows about 55% more wrong ghosts
engine = "ngram"     # or "smollm" (opt-in, see ghost-text.md); any other value warns and means "ngram"
threads = 4          # CPU threads for SmolLM; 0 = all cores. Linux only: macOS runs SmolLM on the GPU

[ask]
nudge = true         # when a turn ends with a decision question in prose, remind the model to use `ask` instead

[edit]
mode = "hashline"     # or "sloppy": find/replace by text, no line tags

[retry]               # waiting out a provider's Retry-After (a 429, mostly); a request is retried when either holds
max_wait = "2m"       # Retry-After up to this: "90s", "30m", "2h", "1d". Longer fails at once
# context_below = 50  # or the context is less than this percent full (default: off)
max_attempts = 5     # attempts per request, retries included: 1 to 20; any other value warns and means 5
# [retry.providers.anthropic]   # both keys again, for one provider
# max_wait = "3h"
# context_below = 60

[status]              # when the footer's context share turns yellow (warn) or red (err); a level is reached at its percent of the window or its token count, whichever is lower
warn_percent = 50
warn_tokens = 270000
err_percent = 70
err_tokens = 500000

[bash]
wait_sleep_secs = 10  # while a background job runs, a model command that starts with a sleep this long or longer is not run and the turn ends (see background-jobs.md); 0 = only commands that do nothing but wait

[permissions]
mode = "yolo"          # ask | edits | yolo (see permissions.md); --permissions overrides it for one run
readonly_bash = true   # the built-in read-only command set counts as reading; false makes every bash call ask unless a rule allows it

# [permissions.rules]  # tool-name globs, or pattern tables for bash, write, edit, read, grep, glob and fetch
# bash = { "cargo test *" = "allow", "git push *" = "deny" }
# "mcp__github_*" = "allow"

[bell]                # what rings the terminal (BEL, plus a toast in some terminals; see detached-runs.md); omp's `completion: {notify: off}` silences both
turn_end = true       # a turn you typed ends with no background command or subagent running
attention = true      # a turn mu started itself (a finished job, a `mu notify` message) asks you something or needs a plan review

[mcp]
lifetime = "session"  # MCP server processes: "session", "idle:10m" or "call"

[update]
check = true          # daily release check at startup

[secrets]
env = []              # extra environment variables whose values count as secrets (redaction is always on; see secrets.md)

[memory]
review = true         # review sessions in the background and save what is worth keeping (see memory.md)
user_cap = 1500       # characters in user.md
project_cap = 4000    # characters in a repository's memory.md

[experimental]
interruptible_wait = false  # let a model command wait for its result and a new prompt release the wait instead of ending the turn (see background-jobs.md); interactive sessions only
```

`[experimental]` holds behaviour that may change or be removed. `interruptible_wait` is read when a session starts; a bad value warns and means `false`.
