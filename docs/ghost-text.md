# Ghost text

Ghost text predicts the rest of the word or a prompt you repeat here; Tab or → accepts. Tab (also when completing a `/command` or its argument) adds a space after the completion; if the next key is punctuation, Enter or Ctrl+Q, that space is removed again. → adds no space. It is configured under `[predict]` (see [Config](config.md)). `mu-predict bench` replays your own prompt history to measure keystrokes saved.

The predictor learns from mu's own prompt history (`history.jsonl` in the profile home) and, unless you turn it off, from two other tools' histories, read-only: omp's `history.db` (in omp's agent directory) and Claude Code's `~/.claude/history.jsonl`. Only the prompts you typed and the directory they were typed in are read, up to 2000 per directory; nothing is sent anywhere and nothing is written to those files. `other_histories = false` under `[predict]` leaves them unread.

By default the word comes from an n-gram of that history. `engine = "smollm"` switches to SmolLM2-135M, which first needs `mu predict setup smollm`: it downloads the 145 MB weights, checks each file's size and SHA-256, and keeps them under `~/.mu/models/SmolLM2-135M/` (`mu predict setup smollm --status` reports what is there). If the weights are missing or damaged, mu shows one notice and uses the n-gram.

How the engines compare, and what was tried and not shipped, is in the [ghost-text evaluation](ghost-text/README.md).
