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

## Comparing harnesses

```sh
cargo build --release && cargo run --release -p mu-bench -- compare crates/mu-bench/scenarios/compare-agentic.toml
```

Runs one prompt headless in each of mu, claude, opencode, omp and pi (`--harness` picks a subset, `--runs` sets the repeats, default 5, interleaved), all against the same fake model server. The table shows wall time, peak memory summed over the harness's whole process tree (sampled every 10 ms; total counts shared pages by their share, private is heap and stack), and how many model steps the harness got through. A harness that does not finish the scenario is reported with the tail of its output.

- The fake model answers at once, so wall time is the harness's own cost: start-up and its tool loop. With a real model, model latency dominates.
- The scenario names abstract tools (`read`, `bash`, `write`). The server rewrites each call into the tool and argument names the harness offered in its request (`Read`/`file_path`, `bash`/`description`, and so on), with absolute paths.
- Each harness runs in a scratch `HOME` with its own config pointing at the fake server (`ANTHROPIC_BASE_URL` for claude, a provider in `models.yml` for omp, a provider in `models.json` for pi, a base-URL override for opencode, `MU_GATEWAY_URL` for mu). `mu-bench serve` prints every request a harness makes, to probe a new one.
- A harness is found on `PATH`, else in `~/.cache/mu-bench-tools` (`$MU_BENCH_TOOLS`). pi is installed there with `npm install --prefix ~/.cache/mu-bench-tools @mariozechner/pi-coding-agent@0.73.1`, so nothing goes into the system.
- Not covered: codex (it needs the OpenAI Responses protocol), and harnesses that are not installed (gemini-cli, crush, aider).

## Not yet

A pass/fail gate with baselines, scenarios for compaction, subagents and plugins, concurrent sessions, and the OpenAI protocol.
