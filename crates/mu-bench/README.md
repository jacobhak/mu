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
- After startup and after each turn, once mu has been idle for the settle time, it samples `Pss_Anon` (private) and `Pss` (total) from `/proc/<pid>/smaps_rollup` for mu and for its whole process tree.
- First-token latency is the time from the server writing a reply's first text delta to the text appearing on the terminal. The server marks each text reply with a `BMKnn.nn` token that is searched for in the terminal output.
- `content kB` is the conversation content so far (prompts, model text and thinking, tool output), for reading memory against content.

## Scenarios

`scenarios/*.toml`: generated files for the tools to find, then turns of scripted replies. Every step but the last of a turn calls tools; the last ends the turn. Tool calls run for real in the scratch directory. `mu-bench` warns when a tool call fails or mu sends more requests than the scenario scripts.

## Not yet

A pass/fail gate with baselines, scenarios for compaction, subagents and plugins, concurrent sessions, and comparison with other harnesses.
