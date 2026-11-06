# Benchmarks: mu vs other coding harnesses

Resource use of mu and four other harnesses on one scripted task, measured with [`mu-bench`](crates/mu-bench/README.md) against a fake model server. No real model is called, so every run costs nothing and repeats. Run 2026-11-06 at commit `8f81324` on one machine (AMD Ryzen 7 5800X, 7 CPUs available, 14 GB, Linux 7.0).

Versions: mu 0.1.0, claude 2.1.289, opencode 1.3.3, omp 18.12.6, pi 0.73.1.

## Read this first: mu's gateway is not in the mu rows

mu normally reaches models through a shared pair of processes, `omp auth-broker` and `omp auth-gateway`, one pair for all mu sessions. The mu rows below bypass it (`MU_GATEWAY_URL`) and are therefore **not** the cost of running mu as installed. A session with an API key added in mu needs no gateway (next section).

| | total | private |
|---|---|---|
| broker, idle | 81 MB | 48 MB |
| gateway, idle | 158 MB | 108 MB |
| **pair, idle** | **239 MB** | **155 MB** |

Measured 2026-11-06 in an empty `HOME` with no model traffic, 8 s after start, with Bun's JIT off (`BUN_JSC_useJIT=0`), as the daemon starts both processes. Two runs gave 238 and 240 MB total, 154 and 156 MB private. With the JIT on (`MU_GATEWAY_JIT=1`) the pair idles at about 306 MB total and 216 MB private (broker 93/58 MB, gateway 214/158 MB). The 2026-10-31 version of this table (251 / 210 MB) was measured with the JIT on and before that setting existed.

**With a request in flight** (2026-11-06, same setup, a request sent through the gateway to a fake Anthropic key in the broker, sampled 6 s in, two runs each): JIT off 241 / 157 and 238 / 154 MB, so a request adds nothing measurable (+0 to +1 MB; a later run with `scripts/measure-gateway.sh` gave 235 / 150 MB, so expect about ±5 MB between runs). With the JIT on it fell, to 294 and 296 MB total and 203 and 205 MB private. The request was rejected by Anthropic's real API (401, invalid key), because the gateway has no way to point a real provider at a fake server (it ignores `ANTHROPIC_BASE_URL` and does not use the proxy variables for the provider call), so a held-open model stream could not be measured. A real stream may cost more. The 2026-10-31 figure of about 305 / 275 MB in flight did not reproduce in either mode; it came from one earlier run that was not repeated. `DESIGN.md` has an A/B with one request served on the real daemon (119 MB private, 153 MB total with the JIT off).

What this means, using the idle figure:
- **One mu session, gateway included:** about 271 MB peak total (32 + 239). That is a little below claude (303 MB), below omp (380 MB) and opencode (485 MB), and above pi (173 MB).
- **Several sessions:** the pair is paid once, so mu with the gateway is below every other harness from two sessions on. The table in [Several sessions at once](#several-sessions-at-once) has the numbers.
- The other harnesses call their providers directly and have no such pair.
- A complete mu run through the real gateway could not be measured against the fake model: the gateway serves only catalog models and ignores `models.yml` on purpose. The combined figures are sums of separate measurements, not a measured run.

## With an API key in mu, there is no gateway

Since native providers, a session that talks to Anthropic, OpenAI or Baseten with a key added in mu starts no omp process, so the pair above is not part of its cost. Re-measured 2026-11-06 with `scripts/measure-sessions.sh` (the 2026-11-04 table is replaced: its setup was not recorded and it did not reproduce, with 5 MB private where a clean `HOME` gives 2 to 3 MB). Release binary, interactive TUI in tmux, `--model anthropic/claude-sonnet-5-5`, a scratch `HOME` and `MU_HOME`, one `say pong` prompt against a local fake Anthropic server that holds the stream open for 15 s. `Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup` of the `mu` process only, 9 s after start, then 6 s into the request. Medians of 5 rounds, the three rows interleaved; the spread was under 1 MB total and 0.1 MB private in every column. "Before" is the parent of the native-providers commit (`50e2396`, binary 61.8 MB), built separately; "after" is `8f81324` (69.2 MB).

| | idle total | idle private | request in flight, total | in flight, private |
|---|---|---|---|---|
| before native providers, through a bypassed gateway (`MU_GATEWAY_URL`) | 15.3 MB | 2.1 MB | 16.3 MB | 2.4 MB |
| after, through a bypassed gateway | 18.3 MB | 2.6 MB | 19.6 MB | 2.7 MB |
| **after, native Anthropic key** | **16.7 MB** | **2.4 MB** | **19.6 MB** | **2.8 MB** |

What this means:
- **One native session:** about 20 MB total while a request runs, against about 259 MB for one session through the gateway pair (19.6 + 239, above). With several sessions the gateway share falls, but a native session never pays it. The native session has no child process.
- **Since the native-providers commit (96 commits and 7.4 MB of binary, not only the providers):** +3.0 MB total and +0.5 MB private idle through a bypassed gateway, +3.3 MB and +0.3 MB with a request in flight. The total is a real increase in every round; the private part is small. The native row is 1.6 MB below the bypassed row at idle in every round and equal in flight; the cause was not investigated.
- **Measuring.** Leftover mu processes from earlier runs share the binary's pages and divide its `Pss`, which lowered the idle total by up to 10 MB in a first attempt. `scripts/measure-sessions.sh` aborts if another process runs the same binary. `scripts/measure-idle.sh` (no model selected, no scripted prompt) gave 20.3 to 20.8 MB total and 3.6 to 3.7 MB private on the same build, in the real `HOME`, so absolute figures depend on the config mu loads.
- This is a fake model on loopback: it measures mu's memory, not TLS handshakes, real latency or what a real stream buffers.

## Several sessions at once

How memory grows when several sessions of one harness run at once, on the same task for every harness. "All sessions together" is the peak, sampled every 10 ms, of the memory summed over every process of every session at the same instant (`Pss`, so a page shared by several processes is counted once in total). The sessions share one `HOME` and are started together. 7 rounds, medians, the harnesses interleaved.

**Peak total, all sessions together (MB)**

| harness | 1 session | 2 | 4 | 8 | each further session | as a share of the first |
|---|---|---|---|---|---|---|
| **mu** (no gateway) | 32 | 37 | 46 | 64 | 4.5 | 14% |
| **mu + gateway pair** (estimate) | 271 | 276 | 285 | 303 | 4.5 | 2% |
| pi | 173 | 289 | 517 | 995 | 117 | 68% |
| claude | 303 | 461 | 756 | 1269 | 138 | 46% |
| omp | 380 | 595 | 1015 | 1696 | 188 | 49% |
| opencode | 485 | 886 | 1674 | 3180 | 385 | 79% |

**Peak private, all sessions together (MB)**

| harness | 1 session | 2 | 4 | 8 | each further session | as a share of the first |
|---|---|---|---|---|---|---|
| **mu** (no gateway) | 4 | 8 | 16 | 33 | 4.1 | 102% |
| **mu + gateway pair** (estimate) | 159 | 163 | 171 | 188 | 4.1 | 3% |
| pi | 123 | 238 | 466 | 941 | 117 | 95% |
| claude | 182 | 340 | 634 | 1146 | 138 | 76% |
| omp | 245 | 461 | 879 | 1574 | 190 | 78% |
| opencode | 432 | 833 | 1620 | 3124 | 385 | 89% |

"Each further session" is `(sum at 8 − sum at 1) / 7`.

- **Every harness shares something.** A further session costs 14% (mu), 46% (claude), 49% (omp), 68% (pi) and 79% (opencode) of the first in total memory, because the binary and the runtime are shared. In private memory a further session costs 76 to 102% of the first: what is private is paid per session.
- **mu's further session is the cheapest by far:** 4.5 MB, against 117 to 385 MB for the others.
- **With the gateway** the pair is added once (239 MB total, 155 MB private, idle, from above) to the mu rows, assuming the pair does not grow with requests (it did not in the test above). mu with the gateway is above pi at one session and below every other harness from two sessions on (276 against pi's 289 at two). At 8 sessions it holds 303 MB against 995 to 3180 MB. Without the gateway (an API key in mu) it is below all of them at every count. The gateway rows are estimates, not a measured run.
- **The comparison is conservative for mu.** The peak of a sum is lower than the sum of the peaks when the sessions do not peak together: claude's private sum at 8 sessions is 1146 MB, not 8 × 182 = 1456 MB. That understates the other harnesses a little. mu's sessions overlapped fully (its private sum, 33 MB, is 8 × 4.1 MB).
- **At 8 sessions the 7 CPUs are the limit.** Wall time per session rises with the count (claude 1.4 s alone, 4.4 s at 8; opencode 2.0 s, 8.1 s; mu 0.08 s, 0.76 s). The sessions still overlap; whether queueing for the CPU changes what each holds was not tested.
- **Executables are run from private copies.** A real `claude` and a node daemon were running on the machine while measuring. Pages of a file are shared by every process that maps it and `Pss` divides them, so another instance would have lowered the harness's numbers. `mu-bench` copies each harness's executable (for pi, node) and runs the copy, so only the benchmark's own sessions share it. Libraries such as libc are still shared with the rest of the machine, for every harness alike.

**Interactive mu sessions** (an idle TUI and one reply, not the 28-step task), N sessions in one `HOME`, each prompted and sampled 6 s into a held-open request with a native key (2026-11-06, `scripts/measure-sessions.sh`, one run each). This is a different workload from the tables above, so the figures are not comparable with them; the other harnesses' interactive sessions were not measured.

| sessions | sum, total | sum, private | per session, total | same sessions through the gateway (estimate), sum | per session |
|---|---|---|---|---|---|
| 1 | 19.9 MB | 2.8 MB | 19.9 MB | 259 MB | 259 MB |
| 2 | 24.1 MB | 5.6 MB | 12.1 MB | 263 MB | 132 MB |
| 4 | 30.6 MB | 11.3 MB | 7.6 MB | 270 MB | 68 MB |
| 8 | 42.0 MB | 22.6 MB | 5.2 MB | 281 MB | 35 MB |

- Private memory grows by about 2.8 MB per session in flight (2.4 MB idle), independent of how many others run. The rest of the total is the binary's pages, about 15 MB, held once. The marginal session costs about 3.2 MB total in flight. No session started a child process.
- The headless task needs more (4.5 MB per further session, above) because it runs a 28-step tool loop with shell commands.

**The pair does not grow with requests.** With JIT off, 1, 8 and 32 simultaneous requests left it at 242, 242 and 241 MB total (idle 239 to 241). With JIT on, 8 requests gave 295 MB. These requests are rejected at once by the real API (see above), so none was held open; `DESIGN.md` has a run with 36 concurrent requests served that was also flat.

## The task

One prompt, then a tool loop of 28 model steps that the fake model scripts: 10 file reads, 13 shell commands (`head`, `grep`), 4 file writes, a final answer. The shape comes from the author's own sessions: most are one prompt followed by an agent loop, and tool results are small (median read 2 kB, median shell output 0.5 kB). 12 source files of 1.5 to 12 kB are created for the task. The server rewrites each scripted call into the tool and argument names the harness offered in its request, so every harness does the same operations.

Every harness finished every run (including every concurrent session) with 28 steps, no tool errors, 4 files written and 67 to 71 kB of tool output sent back, so the work is equal.

Compared with the 2026-10-31 run, mu's wall time doubled (0.04 to 0.08 s, 0.03 to 0.06 s CPU) and its peak total rose from 25 to 32 MB (the binary grew by about 5 MB over 122 commits); its private memory did not change. omp's total is 380 MB here against 286 MB then, with the same version and similar private memory (245 against 225 MB). The earlier run used the installed executables directly, which other processes on the machine may have shared (not checked); the benchmark now runs private copies.

## Results

One session at a time, 7 rounds, medians. The harnesses are interleaved within each round. One untimed warm-up run each comes first, so first-run setup (opencode's database, omp's native libraries) is not charged to the measured runs.

| harness | wall | CPU | peak private | peak total |
|---|---|---|---|---|
| **mu** (no gateway) | 0.08 s | 0.06 s | 4 MB | 32 MB |
| pi | 0.79 s | 1.04 s | 123 MB | 173 MB |
| claude | 1.41 s | 1.68 s | 182 MB | 303 MB |
| omp | 1.75 s | 2.38 s | 245 MB | 380 MB |
| opencode | 1.95 s | 2.89 s | 432 MB | 485 MB |

Reading the columns:
- **peak private and peak total** are the highest values of the harness's whole process tree, sampled every 10 ms. Private is heap, stack and other memory only it holds. Total adds the shared pages (the binary, the runtime) divided among the processes that share them. The earlier version of this file used the last sample before exit; that can fall after teardown has begun (pi: 86 MB private in the last sample, 123 MB at the peak), so peaks are used throughout. `mu-bench` still prints the last-sample columns, and the JSON has them and the kernel's peak RSS, which counts shared pages in full.
- **CPU** is user plus system time of the process and its children, from the kernel.
- Wall times use a fake model that answers at once, so they are start-up plus the tool loop. With a real model, model latency dominates and these differences disappear into it.

## What this does not show

- **Long sessions.** These are 28-step headless runs. Memory after hours of use, after compaction, or with large tool output is not measured here.
- **The interactive UI of the other harnesses.** The comparison is headless. An idle interactive session, and any first-run or login screen, is not measured for them. mu's idle terminal UI measured about 20 MB total and 4 MB private on this machine on 2026-11-06 with a separate script (`scripts/measure-idle.sh`).
- **Quality and speed with a real model.** Task success, model latency and token cost are outside this benchmark.
- **codex, gemini-cli, crush, aider.** codex needs the OpenAI Responses protocol, which the fake server does not speak; the others are not installed.
- **One machine, one scenario.** 7 rounds. Ranges are in the JSON output; the wall-time ranges are narrow, but a different machine will give different absolute numbers.

## Method

- A fake model server speaks the Anthropic Messages protocol and replays the scenario. mu is pointed at it with `MU_GATEWAY_URL`; claude with `ANTHROPIC_BASE_URL`; omp and pi with a provider entry in their model config; opencode with a base-URL override. Each runs in a scratch `HOME`, with the config that names the server kept per session.
- Memory is `Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup`, summed over the process tree; for several sessions, summed over the trees of all live sessions in one scan of `/proc`. CPU and peak RSS come from the kernel's resource accounting of the child.
- Each harness runs from a private copy of its executable (for pi, of node), so that other processes running the same file do not change its `Pss`.
- pi was installed into `~/.cache/mu-bench-tools` with `npm install --prefix`, not into the system.
- Reproduce: `cargo build --release -p mu-bench && ./target/release/mu-bench compare crates/mu-bench/scenarios/compare-agentic.toml --runs 7 --sessions 2,4,8`. Details, the scenario format and how to probe another harness are in [`crates/mu-bench/README.md`](crates/mu-bench/README.md). Run it on a machine that is otherwise quiet: with 8 sessions of opencode it needs about 3.5 GB.
- Native-provider and interactive tables (Linux; need `tmux` and `python3`): `scripts/measure-sessions.sh -m native target/release/mu 1 2 4 8` runs N interactive sessions against `scripts/fake-anthropic.py`, and `-m bypass` does the same through `MU_GATEWAY_URL`. A table row is the median of 5 runs with the rows interleaved (one run of each row per round). For the "before native providers" row, build the parent of that commit in a separate worktree (`git worktree add --detach <dir> <commit>^`, `CARGO_TARGET_DIR=<elsewhere> cargo build --release -p mu`) and pass its binary. The script aborts if another process runs the binary, since that would share its pages.
- Gateway pair: `scripts/measure-gateway.sh jit-off 1` and `jit-on 8` (an empty `HOME`, `omp` on `PATH`, the network). The requests are rejected at once by the real API and no real credential is sent. A held-open model stream through the gateway cannot be measured offline (see `crates/mu-bench/README.md`).

## History

This file previously held an informal comparison from 2026-10-25 (one real-model task, 1 to 5 runs per tool, reasoning effort not pinned, an idle-memory table). It is replaced because its method was not repeatable, and its idle figure for mu (7 MB) did not reproduce: on this machine mu's idle terminal UI measures about 17 MB total. The old text is in the git history.
