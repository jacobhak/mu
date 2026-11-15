# Config

`~/.mu/config.toml` is optional, and so is every key in it. The values below are the defaults. Model roles, per-model limits and compaction thresholds live in [`models.toml`](model-roles.md#models-toml), not here; a `[compaction]` table left in `config.toml` is no longer read and warns at startup.

```toml
# default_remote = "devbox"  # plain `mu` runs on this remote; `mu --local` runs here (see remote.md#remotes)

[theme]
source = "tokyo-night"  # default; or "terminal" (follow your terminal's palette), "omarchy", a bundled or ~/.mu/themes name, or a colors.toml path
# accent = "blue"     # palette key or "#rrggbb"
cards = "filled"      # or "outline"

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

[status]              # when the footer's context share turns yellow (warn) or red (err); a level is reached at its percent of the window or its token count, whichever is lower
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
attention = true      # a running turn asks you something or needs an approval or plan review, whoever started it

[mcp]
lifetime = "session"  # MCP server processes: "session", "idle:10m" or "call"

[task]                # a subagent's soft request budget: cross it and it is told to wrap up, at 1.5x it is stopped and asked for what it has, and five requests later the run ends; 0 turns the guard off
soft_request_budget = 200  # a scout is capped at 100 however high this is
budget_notice = true       # the wrap-up notice at the budget
max_cost = 5.0             # most a launched worker may spend (USD) on the model calls it makes through this session; 0 turns the cap off
# launcher = ["/path/to/pod-run.sh"]  # run every `task` subagent through this command instead of `mu --headless`; MU_TASK_LAUNCHER overrides it with one program path; see below

# [agent]                # `mu agent` with no client attached (see remote.md#when-nobody-is-attached)
# notify_command = "notify-send \"mu $1\" \"$2\""  # run under sh -c when a question or approval starts waiting; $1 is the session id, $2 a one-line summary
# approval_timeout = 0   # seconds after which a waiting approval is answered "unavailable" (a question is cancelled); 0 waits for ever

# [remote.devbox]        # where agents can run besides here; `mu -r devbox`, `mu remote list|check` (see remote.md#remotes)
# adapter = "ssh"        # "ssh" is built in (needs host); anything else is a program on PATH or a path
# host = "devbox"
# trusted = false        # true for a remote you control; only a trusted one may run the main agent
# repos = []             # on an untrusted remote: the repositories (paths or origin URLs) whose workers may run there
# [remote.devbox.settings]  # passed to the adapter as JSON; no secrets (keys like token, password, secret, key are refused)

[update]
check = true          # daily release check at startup

[secrets]
env = []              # extra environment variables whose values count as secrets (redaction is always on; see secrets.md)

[memory]
review = true         # review sessions in the background and save what is worth keeping (see memory.md)
user_cap = 1500       # characters in user.md
project_cap = 4000    # characters in a repository's memory.md
```

With `[task] launcher` set, every `task` subagent (not `/review`) runs through that command in the task's directory, with the same stdin and stdout as `mu --headless` and no `--headless` argument. Before each launch mu commits the working tree, untracked files included, as `refs/mu/task/<id>` and deletes the ref when the run ends. The command gets `MU_TASK_NAME` (the launch id), `MU_TASK_ROOT`, `MU_TASK_CWD`, `MU_TASK_REF`, `MU_TASK_RESULT_REF`, `MU_TASK_SESSION`, `MU_TASK_PARENT_PID`, `MU_TASK_MU_HOME`, `MU_TASK_PROFILE_HOME`, `MU_TASK_AGENT_DIR` and `MU_TASK_DEPTH`. After the `result` line it may write `{"type":"changes","ref":"refs/mu/result/<id>","base":"<sha>","files":N}` on file descriptor 3, which mu opens for the launcher; mu ignores the same line on stdout, where the worker itself writes, and accepts it only when `ref` is this launch's `MU_TASK_RESULT_REF` and `base` is a full commit hash. The task's result then ends with a line pointing at that ref, which mu never applies itself. `refs/mu/result/*` older than 14 days are deleted before each launch. A subagent ignores the setting: its own subagents spawn locally.

A launched worker has no credentials: its model calls and web searches go through this session. It may call only its task's model, that model's fallback roles and `@smol` (for titles); any other model is refused, and a worker's own subagents get no more than that. Calls the worker makes after it has sent its `result` are refused and any still running are cancelled, and at most eight run at once. The cost of its calls counts in this session's `--max-cost` and, past `[task] max_cost`, further calls fail with `task spend cap reached` and the task's result says so; one-off calls such as a title are not priced, so they do not count. A worker's web search goes through the same permission check as yours, as a network call: in `ask` mode it asks first, and the hosts it returns are noted for the worker only, so a `fetch` of one of them from this session still asks. The task carries mu's version: a worker of another version refuses the task with one line, which the task shows as its error. A line from the worker longer than 64 MiB and an image over 20 MB are dropped; a worker's session lines must be session entries, the first of them a header naming this session as its parent, and `cost` lines are dropped.
