# Config

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
threshold = 0.3       # minimum confidence for ghost text; 0.15 saves a few more keystrokes but shows about 55% more wrong ghosts
engine = "ngram"     # or "smollm" (opt-in, see ghost-text.md); any other value warns and means "ngram"
threads = 4          # CPU threads for SmolLM; 0 = all cores. Linux only: macOS runs SmolLM on the GPU

[ask]
nudge = true         # when a turn ends with a decision question in prose, remind the model to use `ask` instead

[edit]
mode = "hashline"     # or "sloppy": find/replace by text, no line tags

[mcp]
lifetime = "session"  # MCP server processes: "session", "idle:10m" or "call"

[update]
check = true          # daily release check at startup

[secrets]
env = []              # extra environment variables whose values count as secrets (redaction is always on; see secrets.md)
```
