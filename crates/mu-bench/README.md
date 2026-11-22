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

> **Read this before quoting any mu number.** mu is measured without its gateway. Normally mu reaches models through a gateway plugin (`mu-gateway`, which runs `omp auth-broker` + `omp auth-gateway`): about 306 MB total and 216 MB private when idle (broker 93/58 MB, gateway 214/158 MB), shared by every mu session. That pair is not in the mu rows. With it, one mu session costs a little more than claude and less than omp, and mu's advantage shows only with several sessions sharing one gateway. The other harnesses call their providers directly.

Runs one prompt headless in each of mu, claude, opencode, omp and pi, all against the same fake model server. `--harness` picks a subset, `--runs` sets the rounds (default 5, harnesses interleaved), `--sessions` is a list of session counts to run at once (default `3`; `--sessions 2,4,8`; `1` skips that part). Each round runs every harness alone and then, for each count, as that many concurrent sessions that share one `HOME`, as several terminals of one user do.

Each table row is the median over the runs. A harness that does not finish the scenario is reported with the tail of its output.

- **private and total** are what the harness's whole process tree still held in its last sample before exit (sampled every 10 ms): private is heap, stack and other memory only it holds; total also counts the shared pages (the binary, the runtime) divided among the processes that share them. With several sessions at once, total is therefore each session's real share. They are not peaks.
- **peak** is the total at its highest. The JSON also has the kernel's peak RSS of the largest process, which counts shared pages in full and cannot be under-sampled.
- **all sessions together** (printed with several counts, and in the JSON as `group_peak_*`) is the peak, sampled every 10 ms, of the memory summed over every process of every live session at the same instant, for total and for private. The last columns give what each further session costs, `(sum at the most sessions − sum of one) / (sessions − 1)`, and that as a share of one session. It is the figure to compare harnesses on, since it is the machine's real cost; the per-session tables are each session's own view and its last sample before exit, which can fall after teardown has begun.
- **Private copies.** Every harness is run from a private copy of its executable (for a node script such as pi, of `node`) made in the scratch directory. Processes that map the same file share its pages and `Pss` divides them among those processes, so a real session or daemon of the same tool running on the machine would otherwise lower the harness's numbers. Libraries such as libc are still shared with the machine's other processes, for every harness alike. Run on an otherwise quiet machine anyway: 8 sessions of opencode need about 3.5 GB.
- **cpu s** is user plus system CPU time of the process and its children, from the kernel.
- **steps, result kB, files** check that every harness did the same work: model steps served, bytes of tool output sent back, files written.
- One untimed warm-up run per harness comes first, so first-run setup (opencode's database, omp's native libraries) is not charged to the measured runs.
- The fake model answers at once, so wall time is the harness's own cost: start-up and its tool loop. With a real model, model latency dominates.
- The scenario names abstract tools (`read`, `bash`, `write`). The server rewrites each call into the tool and argument names the harness offered in its request (`Read`/`file_path`, `bash`/`description`, and so on), with absolute paths.
- Each harness gets a config pointing at the fake server: `ANTHROPIC_BASE_URL` for claude, a provider in `models.yml` for omp, a provider in `models.json` for pi, a base-URL override for opencode, `MU_GATEWAY_URL` for mu. Configs that hold the server's URL are per session, so concurrent sessions do not read each other's. `mu-bench serve` prints every request a harness makes, to probe a new one.
- A harness is found on `PATH`, else in `~/.cache/mu-bench-tools` (`$MU_BENCH_TOOLS`). pi is installed there with `npm install --prefix ~/.cache/mu-bench-tools @mariozechner/pi-coding-agent@0.73.1`, so nothing goes into the system.
- **mu's gateway is not in these numbers.** `MU_GATEWAY_URL` bypasses the gateway plugin (`mu-gateway`, which runs a shared `omp auth-broker` + `omp auth-gateway` pair) that mu normally uses. Started in an empty `HOME` with no model traffic, the pair held about 306 MB total and 216 MB private after 8 s (2026-11-06). With a request in flight (rejected by the real API, since the gateway cannot be pointed at a fake provider) it held 294 to 296 MB total and 203 to 205 MB private. The pair cannot be pointed at the fake model: the gateway serves only catalog models and ignores `models.yml` on purpose, so a complete mu run through it needs a real provider. Tried on 2026-11-06 and not working: llama.cpp via `LLAMA_CPP_BASE_URL` (discovery reaches the fake, but the gateway lists no models for a keyless provider), and `ANTHROPIC_BASE_URL` or `HTTPS_PROXY` for a fake Anthropic (the provider call ignores both).
- Not covered: codex (it needs the OpenAI Responses protocol), and harnesses that are not installed (gemini-cli, crush, aider).

## Interactive sessions and the gateway pair

`mu-bench` runs mu headless in a pseudo-terminal and has no notion of several interactive sessions or of the real gateway. Two scripts cover that (Linux, `tmux`, `python3`; the second also `omp` and the network): `scripts/measure-sessions.sh` runs N interactive mu sessions against `scripts/fake-anthropic.py`, with a native key or through `MU_GATEWAY_URL`, and `scripts/measure-gateway.sh` measures the omp broker + gateway pair idle and with requests in flight. Method and results are in [`BENCHMARKS.md`](../../BENCHMARKS.md).

## Memory gate

```sh
cargo build --release && cargo run --release -p mu-bench -- check crates/mu-bench/scenarios/read-heavy.toml
cargo run --release -p mu-bench -- check --update crates/mu-bench/scenarios/read-heavy.toml
```

`check` runs the scenario as the default command does (`--runs`, default 3; the medians are compared). For each row it takes incidental memory, `mu_anon_kB − content kB`, and compares it with the committed baseline `crates/mu-bench/baselines/<scenario>.json`. A row fails when it is above `baseline × (1 + P/100) + 1 MB`, with `P` from `--tolerance-pct` (default 20). It prints every row and exits 1 listing the ones over. A row the baseline does not have is an error: run with `--update`. `--update` writes the measured values to the baseline and exits 0.

The gate is not run in CI: it samples `Pss_Anon`, which moves with machine load and with the baseline's own machine, so every row would fail on a loaded or differently-provisioned runner. Run it on a quiet machine. The baseline is measured on one machine, so a deliberate change that moves memory updates the committed baseline in the same commit.

## Not yet

Scenarios for compaction, subagents and plugins, mu through its real gateway, and the OpenAI protocol.
