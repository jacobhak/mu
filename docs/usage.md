# Using mu

```
mu [--model SEL] [--thinking off|low|medium|high|xhigh|max] [--profile NAME] [--theme NAME] [-c] [--resume [ID|PATH]]
mu -p PROMPT [--agent NAME] [--max-cost USD]
mu --acp
mu take <session|id>
mu review [--base REF] [--plan FILE] [--focus TEXT] [--out FILE] [--markdown]
mu update [--check] [--tag TAG] [--force]
```

`SEL` is `provider/model[:level]` or `@role`. `-c` resumes the latest session for the directory; `--resume` takes a session id or path, or opens a picker. `--profile` picks an omp profile (sets `OMP_PROFILE`); `--theme` overrides `[theme] source` for one run. Sessions are omp v3 JSONL under `~/.mu/sessions/` and open in omp.

Thinking levels are `off`, `low`, `medium`, `high`, `xhigh` and `max` (`minimal` reads as `low`). Through the gateway, `xhigh` and `max` send the matching `output_config.effort`. An effort the model does not serve goes to the tier the model maps it to, if any (a route serving only low/high/max takes `medium` as `high`), otherwise to the nearest lower tier it serves. On a native provider (a key added with `/login` or `mu auth`, see [Providers](providers.md)) each level maps to the nearest effort in the model's own ladder instead, and what is sent differs per provider and model kind (adaptive, budget, effort).

`-p` runs one prompt and prints the answer. `--agent` runs it as a [custom agent](custom-agents.md); `--max-cost` aborts once the session has cost more than that many USD and always prints `mu: cost $X` to stderr. Exit codes, interrupting and taking over a `-p` run are in [Detached runs](detached-runs.md).

While a turn runs, Enter steers (the message is delivered after the current tool call), Ctrl+Q queues a follow-up, Esc aborts and returns queued text to the composer.

Besides the built-in slash commands, prompt templates from `.omp/commands`, `<agentDir>/commands`, `.claude/commands` and `.omp/prompts` run as slash commands, and skills as `/skill:<name>`.

`/copy` opens a picker of recent replies, code blocks and bash commands; `/copy code`, `/copy cmd` and `/copy link` copy the latest one directly. Under tmux the text goes through `tmux load-buffer -w`; otherwise via OSC 52, plus `wl-copy`/`xclip` when a local display exists.

Images: paste an image file path, run `/image <path>`, press Ctrl+V (reads `wl-paste`/`xclip`), or paste an image from the terminal clipboard when it supports kitty's OSC 5522 (tmux needs `allow-passthrough on`). PNG, JPEG, GIF and WebP up to 5 MB; no resizing.
