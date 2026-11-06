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
# [retry.providers.anthropic]   # both keys again, for one provider
# max_wait = "3h"
# context_below = 60

[status]              # when the footer's context share turns yellow (warn) or red (err); a level is reached at its percent of the window or its token count, whichever is lower
warn_percent = 50
warn_tokens = 270000
err_percent = 70
err_tokens = 500000

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
```
