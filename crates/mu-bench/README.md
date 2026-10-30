# mu-bench

Measures mu's memory and responsiveness in a real terminal against a fake model server. No model, network or credentials are involved, so runs cost nothing and repeat.

```sh
cargo build --release
cargo run --release -p mu-bench -- crates/mu-bench/scenarios/read-heavy.toml
```

Options: `--mu PATH` (default `target/release/mu`), `--runs N` (default 3, the table shows medians), `--settle-ms MS` (idle time before each sample, default 2000), `--out FILE.json` (full per-run results).

## How it works

- The fake server speaks the Anthropic Messages protocol that mu uses through the gateway and is reached with `MU_GATEWAY_URL`. It replays the scenario's scripted replies (text, thinking, tool calls) in order. Requests without tools get a short canned reply; subagent and compaction requests are not scripted yet.
- mu runs in a pseudo-terminal in a scratch `HOME`, `MU_HOME` and working directory, with a fresh price catalogue cached (as in a normal session). The harness answers the terminal queries a real terminal would.
- After startup and after each turn, once mu has been idle for the settle time, it samples `Pss_Anon` (private) and `Pss` (total) from `/proc/<pid>/smaps_rollup` for mu and for its whole process tree. It also splits mu's own memory by region from `/proc/<pid>/smaps`: code, read-only data, data, libraries, heap, stack and anonymous mappings, each as resident (shared pages counted by their share) and private. Code and read-only data are clean, file-backed pages of the binary; heap, stack and anonymous memory are private. Read-only data includes the relocated data that the loader makes private.
- First-token latency is the time from the server writing a reply's first text delta to that text appearing on the terminal. Each turn's text starts with one distinct character (①, ②, …) that is searched for in the terminal output. It is one character because mu reveals streamed text a few characters per 33 ms frame, so a longer marker would measure that pacing. Text with nothing queued ahead of it shows after one frame (about 33 ms); text that follows thinking waits behind the thinking still queued for reveal.
- `content kB` is the conversation content so far (prompts, model text and thinking, tool output), for reading memory against content.

## Scenarios

`scenarios/*.toml`: generated files for the tools to find, then turns of scripted replies. Every step but the last of a turn calls tools; the last ends the turn. Tool calls run for real in the scratch directory. `mu-bench` warns when a tool call fails or mu sends more requests than the scenario scripts.

Replies stream in bursts by default (`chunk_bytes` every `chunk_delay_ms`, 24 KB/s). That is fine for memory but about 60 times faster than a real model, and a burst backs up mu's reveal queue and inflates first-token times. Latency scenarios set a model-like rate (`streaming.toml`: 400 B/s). Each report prints the rate it ran at.

## Not yet

A pass/fail gate with baselines, scenarios for compaction, subagents and plugins, concurrent sessions, and comparison with other harnesses.
