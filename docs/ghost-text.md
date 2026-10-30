# Ghost text

Ghost text predicts the rest of the word or a prompt you repeat here; Tab or → accepts. It is configured under `[predict]` (see [Config](config.md)). `mu-predict bench` replays your own prompt history to measure keystrokes saved.

By default the word comes from an n-gram of your own history. `engine = "smollm"` switches to SmolLM2-135M, which first needs `mu predict setup smollm`: it downloads the 145 MB weights, checks each file's size and SHA-256, and keeps them under `~/.mu/models/SmolLM2-135M/` (`mu predict setup smollm --status` reports what is there). If the weights are missing or damaged, mu shows one notice and uses the n-gram.

How the engines compare, and what was tried and not shipped, is in the [ghost-text evaluation](ghost-text/README.md).
