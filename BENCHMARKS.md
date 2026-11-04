# Benchmarks: mu vs other coding harnesses

Resource use of mu and four other harnesses on one scripted task, measured with [`mu-bench`](crates/mu-bench/README.md) against a fake model server. No real model is called, so every run costs nothing and repeats. Run 2026-10-31 at commit `afffef8` on one machine (AMD Ryzen 7 5800X, 7 CPUs available, 14 GB, Linux 7.0).

Versions: mu 0.1.0, claude 2.1.289, opencode 1.3.3, omp 18.12.6, pi 0.73.1.

## Read this first: mu's gateway is not in the mu rows

mu normally reaches models through a shared pair of processes, `omp auth-broker` and `omp auth-gateway`, one pair for all mu sessions. The mu rows below bypass it (`MU_GATEWAY_URL`) and are therefore **not** the cost of running mu as installed.

| | total | private |
|---|---|---|
| broker, idle | 72 MB | 56 MB |
| gateway, idle | 179 MB | 154 MB |
| **pair, idle** | **251 MB** | **210 MB** |
| pair with a request in flight (one earlier run) | about 305 MB | about 275 MB |

The idle figures were measured in an empty `HOME` with no model traffic, 8 s after start. The in-flight figure comes from a run where the request was rejected, so a real model call may differ.

What this means, using the idle figure:
- **One mu session, gateway included:** about 276 MB total (25 + 251). That is in the range of claude (293 MB) and omp (286 MB), well below opencode (476 MB) and above pi (158 MB).
- **Three sessions:** the pair is paid once, so each session carries about 96 MB total (12 + 251 / 3). That is close to pi's 94 MB and well below claude (200 MB), omp (259 MB) and opencode (440 MB). With more sessions the share keeps falling.
- mu's own process is small in every case. Its advantage grows with the number of sessions sharing one gateway, and it is not an advantage with a single session.
- The other harnesses call their providers directly and have no such pair.
- A complete mu run through the real gateway could not be measured against the fake model: the gateway serves only catalog models and ignores `models.yml` on purpose. These combined figures are sums of separate measurements, not a measured run.

## With an API key in mu, there is no gateway

Since native providers, a session that talks to Anthropic, OpenAI or Baseten with a key added in mu starts no omp process, so the pair above is not part of its cost. Measured 2026-11-04 on the same machine with the release binary, interactive TUI, `--model anthropic/claude-sonnet-5-5`, one `say pong` prompt, against a local fake Anthropic server holding the stream open for 15 s (`Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup`, the `mu` process only, 9 s after start, then 6 s into the request). One run each: expect a MB or so of noise.

| | idle total | idle private | request in flight, total | in flight, private |
|---|---|---|---|---|
| before native providers, through a bypassed gateway (`MU_GATEWAY_URL`) | 18.7 MB | 5.1 MB | 19.6 MB | 5.4 MB |
| after, through a bypassed gateway | 21.2 MB | 5.5 MB | 22.2 MB | 5.8 MB |
| **after, native Anthropic key** | **19.0 MB** | **5.2 MB** | **21.5 MB** | **5.7 MB** |

What this means:
- **One native session:** about 22 MB total while a request runs, against about 276 MB for one session through the gateway pair (25 + 251, above). With several sessions the gateway share falls, but a native session never pays it.
- **Cost of the change in the core:** linking rustls and the model catalog snapshot makes the binary 2.1 MB larger (61.9 to 64.1 MB; 64.5 MB with the key store, TLS and the three wires). Bare idle TUI with no model selected: 17.0 to 18.6 MB total, 3.2 to 3.5 MB private. The differences between the rows above are within the run-to-run noise of one measurement, so no private-memory regression can be claimed or ruled out below about 0.5 MB.
- This is a fake model on loopback: it measures mu's memory, not TLS handshakes, real latency or what a real stream buffers.

## The task

One prompt, then a tool loop of 28 model steps that the fake model scripts: 10 file reads, 13 shell commands (`head`, `grep`), 4 file writes, a final answer. The shape comes from the author's own sessions: most are one prompt followed by an agent loop, and tool results are small (median read 2 kB, median shell output 0.5 kB). 12 source files of 1.5 to 12 kB are created for the task. The server rewrites each scripted call into the tool and argument names the harness offered in its request, so every harness does the same operations.

Every harness finished every run with 28 steps, no tool errors, 4 files written and 67 to 71 kB of tool output sent back, so the work is equal.

## Results

7 rounds, medians. The harnesses are interleaved within each round. One untimed warm-up run each comes first, so first-run setup (opencode's database, omp's native libraries) is not charged to the measured runs.

**One session at a time**

| harness | wall | CPU | private | total | peak total |
|---|---|---|---|---|---|
| **mu** (no gateway) | 0.04 s | 0.03 s | 4 MB | 25 MB | 25 MB |
| pi | 0.82 s | 1.06 s | 120 MB | 158 MB | 164 MB |
| claude | 1.02 s | 1.26 s | 195 MB | 293 MB | 293 MB |
| omp | 1.54 s | 2.22 s | 225 MB | 286 MB | 300 MB |
| opencode | 1.91 s | 2.85 s | 427 MB | 476 MB | 479 MB |

**Three sessions of one harness at once, per session** (sharing one `HOME`, started together)

| harness | wall | private | total | peak total |
|---|---|---|---|---|
| **mu** (no gateway) | 0.07 s | 4 MB | 12 MB | 12 MB |
| pi | 1.03 s | 80 MB | 94 MB | 137 MB |
| claude | 1.41 s | 153 MB | 200 MB | 206 MB |
| omp | 1.91 s | 225 MB | 259 MB | 263 MB |
| opencode | 2.74 s | 420 MB | 440 MB | 440 MB |

Reading the columns:
- **private and total** are what the harness's whole process tree still held in its last sample before it exited. Private is heap, stack and other memory only it holds. Total adds the shared pages (the binary, the runtime) divided among the processes that share them, so with three sessions it is each session's real share.
- **peak total** is the total at its highest, sampled every 10 ms. The JSON output also has the kernel's peak RSS of the largest process, which counts shared pages in full.
- **CPU** is user plus system time of the process and its children, from the kernel.
- Wall times use a fake model that answers at once, so they are start-up plus the tool loop. With a real model, model latency dominates and these differences disappear into it.

## What this does not show

- **Long sessions.** These are 28-step headless runs. Memory after hours of use, after compaction, or with large tool output is not measured here.
- **The interactive UI.** All harnesses ran headless. An idle interactive session, and any first-run or login screen, is not measured. mu's idle terminal UI measured about 17 MB total and 3 MB private on this machine with a separate script (`scripts/measure-idle.sh`); the other harnesses' idle figures were not re-measured.
- **Quality and speed with a real model.** Task success, model latency and token cost are outside this benchmark.
- **codex, gemini-cli, crush, aider.** codex needs the OpenAI Responses protocol, which the fake server does not speak; the others are not installed.
- **One machine, one scenario.** 7 rounds. Ranges are in the JSON output; the wall-time ranges are narrow, but a different machine will give different absolute numbers.

## Method

- A fake model server speaks the Anthropic Messages protocol and replays the scenario. mu is pointed at it with `MU_GATEWAY_URL`; claude with `ANTHROPIC_BASE_URL`; omp and pi with a provider entry in their model config; opencode with a base-URL override. Each runs in a scratch `HOME`, with the config that names the server kept per session.
- Memory is `Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup`, summed over the process tree. CPU and peak RSS come from the kernel's resource accounting of the child.
- pi was installed into `~/.cache/mu-bench-tools` with `npm install --prefix`, not into the system.
- Reproduce: `cargo build --release -p mu-bench && ./target/release/mu-bench compare crates/mu-bench/scenarios/compare-agentic.toml`. Details, the scenario format and how to probe another harness are in [`crates/mu-bench/README.md`](crates/mu-bench/README.md).

## History

This file previously held an informal comparison from 2026-10-25 (one real-model task, 1 to 5 runs per tool, reasoning effort not pinned, an idle-memory table). It is replaced because its method was not repeatable, and its idle figure for mu (7 MB) did not reproduce: on this machine mu's idle terminal UI measures about 17 MB total. The old text is in the git history.
