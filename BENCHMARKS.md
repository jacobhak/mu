# Benchmarks: mu vs other coding harnesses

Resource use of mu and four other harnesses on one scripted task, measured with [`mu-bench`](crates/mu-bench/README.md) against a fake model server. No real model is called, so every run costs nothing and repeats.

**Two runs are combined here.** Only mu was re-measured on 2026-11-25 (commit `092c394`, 181 commits after the first run, release binary 93.7 MB against 69.2 MB). The other harnesses' rows and the earlier mu rows are from 2026-11-06 at commit `8f81324` and were not re-run. Both ran on an AMD Ryzen 7 5800X under Linux 7.0, but not in the same environment: 2026-11-06 had 7 CPUs and 14 GB available; 2026-11-25 had 2 CPUs and 3.4 GB, with a few unrelated processes running (an omp, a mu session, editors). So a mu row dated 2026-11-25 set against another harness's 2026-11-06 row is a cross-run comparison. Memory should move little with the CPU count; wall time does. Every table says which run each row is from.

Versions: mu 0.1.0, claude 2.1.289, opencode 1.3.3, omp 18.12.6, pi 0.73.1 (2026-11-06). The gateway pair was re-measured on 2026-11-25 with the omp that mu pins, 19.1.1.

## Read this first: mu's gateway is not in the mu rows

mu normally reaches models through a gateway provider plugin. The one measured here is `mu-gateway`'s shared pair of processes, `omp auth-broker` and `omp auth-gateway`, one pair for all mu sessions. The mu rows below bypass it (`MU_GATEWAY_URL`) and are therefore **not** the cost of running mu as installed. A session with an API key added in mu needs no gateway (next section).

2026-11-06, omp 18.12.6:

| | total | private |
|---|---|---|
| broker, idle | 93 MB | 58 MB |
| gateway, idle | 214 MB | 158 MB |
| **pair, idle** | **306 MB** | **216 MB** |

Re-measured 2026-11-25 with omp 19.1.1 (the version mu pins; the 2026-11-06 figures used omp 18.12.6) in an empty `HOME` with no model traffic, 8 s after start, four runs. Medians; the range of the four is in brackets.

| | total | private |
|---|---|---|
| broker, idle | 80 MB (79 to 80) | 48 MB (47 to 49) |
| gateway, idle | 195 MB (181 to 202) | 150 MB (137 to 156) |
| **pair, idle** | **275 MB** (261 to 282) | **199 MB** (185 to 203) |

That is the current figure. The gateway is the unsteady half; the broker barely moves. The 2026-11-06 run with omp 18.12.6 gave 306 MB total and 216 MB private (broker 93/58 MB, gateway 214/158 MB), so the pair is now about 30 MB smaller in total and 17 MB in private memory. That is a different omp version and a different machine state; the cause was not isolated.

**With requests in flight** (2026-11-25, same setup, 1 and 8 simultaneous requests sent through the gateway to a fake Anthropic key in the broker, sampled 6 s in, two runs of each count): 263 to 284 MB total and 186 to 207 MB private, median 266 and 190 MB, against 275 and 199 MB idle. Requests add nothing measurable, and 8 at once did not differ from 1 (the run-to-run spread, 21 MB, is larger than any step between the counts). The requests were rejected by Anthropic's real API (401, invalid key), because the gateway has no way to point a real provider at a fake server (it ignores `ANTHROPIC_BASE_URL` and does not use the proxy variables for the provider call), so a held-open model stream could not be measured. A real stream may cost more. (2026-11-06, omp 18.12.6: 294 to 296 MB total and 203 to 205 MB private with one request. The 2026-10-31 figure of about 305 / 275 MB in flight did not reproduce; it came from one earlier run that was not repeated.)

What this means, using the idle figure (275 MB):
- **One mu session, gateway included:** about 321 MB peak total (46 + 275). Set against the 2026-11-06 rows of the other harnesses, that is above claude (303 MB) and pi (173 MB), below omp (380 MB) and opencode (485 MB), the same order as before.
- **Several sessions:** the pair is paid once, so mu with the gateway is below every other harness from four sessions on. The table in [Several sessions at once](#several-sessions-at-once) has the numbers.
- The other harnesses call their providers directly and have no such pair.
- A complete mu run through the real gateway could not be measured against the fake model: the gateway serves only catalog models and ignores `models.yml` on purpose. The combined figures are sums of separate measurements, not a measured run.

## With an API key in mu, there is no gateway

Since native providers, a session that talks to Anthropic, OpenAI or Baseten with a key added in mu starts no gateway process, so the pair above is not part of its cost. Measured with `scripts/measure-sessions.sh` (the 2026-11-04 table was replaced on 2026-11-06: its setup was not recorded and it did not reproduce, with 5 MB private where a clean `HOME` gives 2 to 3 MB). Release binary, interactive TUI in tmux, `--model anthropic/claude-sonnet-5-5`, a scratch `HOME` and `MU_HOME`, one `say pong` prompt against a local fake Anthropic server that holds the stream open for 15 s. `Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup` of the `mu` process only, 9 s after start, then 6 s into the request. Medians of 5 rounds, rows interleaved. Spread: 2026-11-06 under 1 MB total and 0.1 MB private in every column; 2026-11-25 up to 1.5 MB total and 0.3 MB private (over the sessions' sums, in the table below). "Before" is the parent of the native-providers commit (`50e2396`, binary 61.8 MB), built separately; "after" is `8f81324` (69.2 MB); "now" is `092c394` (93.7 MB). The "before" and "after" rows were not re-run.

| | idle total | idle private | request in flight, total | in flight, private |
|---|---|---|---|---|
| before native providers, through a bypassed gateway (`MU_GATEWAY_URL`), 2026-11-06 | 15.3 MB | 2.1 MB | 16.3 MB | 2.4 MB |
| after, through a bypassed gateway, 2026-11-06 | 18.3 MB | 2.6 MB | 19.6 MB | 2.7 MB |
| after, native Anthropic key, 2026-11-06 | 16.7 MB | 2.4 MB | 19.6 MB | 2.8 MB |
| now, through a bypassed gateway, 2026-11-25 | 23.8 MB | 2.9 MB | 26.0 MB | 3.1 MB |
| **now, native Anthropic key, 2026-11-25** | **22.4 MB** | **2.8 MB** | **25.9 MB** | **3.1 MB** |

What this means:
- **One native session:** about 26 MB total while a request runs (2026-11-06: 20 MB), against about 301 MB for one session through the gateway pair (25.9 + 275, above). With several sessions the gateway share falls, but a native session never pays it. The native session has no child process.
- **Since 8f81324 (181 commits, binary 69.2 to 93.7 MB):** +5.5 MB total and +0.3 MB private idle through a bypassed gateway, +6.4 MB and +0.4 MB with a request in flight. The private part is almost unchanged; the total grew with the binary's resident pages (+5.5 MB against +24.5 MB of file). The different machine state (2 CPUs, 3.4 GB, under memory pressure) was not separated from the code change; building `8f81324` here would do it. The native row is 1.4 MB below the bypassed row at idle in every round (2026-11-06: 1.6 MB) and about equal in flight; the cause was not investigated.
- **Measuring.** Leftover mu processes from earlier runs share the binary's pages and divide its `Pss`, which lowered the idle total by up to 10 MB in a first attempt. `scripts/measure-sessions.sh` aborts if another process runs the same binary. `scripts/measure-idle.sh` (no model selected, no scripted prompt) gave 20.3 to 20.8 MB total and 3.6 to 3.7 MB private on the same build, in the real `HOME`, so absolute figures depend on the config mu loads.
- This is a fake model on loopback: it measures mu's memory, not TLS handshakes, real latency or what a real stream buffers.

## Several sessions at once

How memory grows when several sessions of one harness run at once, on the same task for every harness. "All sessions together" is the peak, sampled every 10 ms, of the memory summed over every process of every session at the same instant (`Pss`, so a page shared by several processes is counted once in total). The sessions share one `HOME` and are started together. 7 rounds, medians, the harnesses interleaved. The mu rows are from 2026-11-25 (2 CPUs, 3.4 GB, 7 rounds, `mu-bench compare --harness mu`); the other harnesses are the 2026-11-06 rows (7 CPUs, 14 GB) and were not re-run, so the comparison crosses two runs. Rows of 2026-11-06 for mu are in the git history.

**Peak total, all sessions together (MB)**

| harness | 1 session | 2 | 4 | 8 | each further session | as a share of the first |
|---|---|---|---|---|---|---|
| **mu** (no gateway) | 46 | 52 | 64 | 85 | 5.6 | 12% |
| **mu + gateway pair** (estimate) | 321 | 327 | 339 | 360 | 5.6 | 2% |
| pi | 173 | 289 | 517 | 995 | 117 | 68% |
| claude | 303 | 461 | 756 | 1269 | 138 | 46% |
| omp | 380 | 595 | 1015 | 1696 | 188 | 49% |
| opencode | 485 | 886 | 1674 | 3180 | 385 | 79% |

**Peak private, all sessions together (MB)**

| harness | 1 session | 2 | 4 | 8 | each further session | as a share of the first |
|---|---|---|---|---|---|---|
| **mu** (no gateway) | 5 | 10 | 20 | 40 | 5.0 | 100% |
| **mu + gateway pair** (estimate) | 204 | 209 | 219 | 239 | 5.0 | 2% |
| pi | 123 | 238 | 466 | 941 | 117 | 95% |
| claude | 182 | 340 | 634 | 1146 | 138 | 76% |
| omp | 245 | 461 | 879 | 1574 | 190 | 78% |
| opencode | 432 | 833 | 1620 | 3124 | 385 | 89% |

"Each further session" is `(sum at 8 − sum at 1) / 7`.

- **Every harness shares something.** A further session costs 12% (mu), 46% (claude), 49% (omp), 68% (pi) and 79% (opencode) of the first in total memory, because the binary and the runtime are shared. In private memory a further session costs 76 to 100% of the first: what is private is paid per session.
- **mu's further session is the cheapest by far:** 5.6 MB, against 117 to 385 MB for the others.
- **With the gateway** the pair is added once (275 MB total, 199 MB private, idle, from above) to the mu rows, assuming the pair does not grow with requests (it did not in the test above). mu with the gateway is above pi at one and two sessions (327 against pi's 289 at two) and below every harness from four sessions on. At 8 sessions it holds 360 MB against 995 to 3180 MB. Without the gateway (an API key in mu) it is below all of them at every count. The gateway rows are estimates, not a measured run.
- **The comparison is conservative for mu.** The peak of a sum is lower than the sum of the peaks when the sessions do not peak together: claude's private sum at 8 sessions is 1146 MB, not 8 × 182 = 1456 MB. That understates the other harnesses a little. mu's sessions overlapped fully (its private sum, 40 MB, is 8 × 5.0 MB).
- **At 8 sessions the CPUs are the limit.** Wall time per session rises with the count (2026-11-06, 7 CPUs: claude 1.4 s alone, 4.4 s at 8; opencode 2.0 s, 8.1 s; mu 0.08 s, 0.76 s; 2026-11-25, 2 CPUs: mu 0.09 s alone, 0.15 s at 2, 0.35 s at 4, 0.98 s at 8). The sessions still overlap; whether queueing for the CPU changes what each holds was not tested. The two machines' wall times are not comparable.
- **Executables are run from private copies.** A real `claude` and a node daemon were running on the machine while measuring. Pages of a file are shared by every process that maps it and `Pss` divides them, so another instance would have lowered the harness's numbers. `mu-bench` copies each harness's executable (for pi, node) and runs the copy, so only the benchmark's own sessions share it. Libraries such as libc are still shared with the rest of the machine, for every harness alike.

**Interactive mu sessions** (an idle TUI and one reply, not the 28-step task), N sessions in one `HOME`, each prompted and sampled 6 s into a held-open request with a native key (2026-11-25, `scripts/measure-sessions.sh -m native`, median of 5 runs per cell, rounds interleaved with the bypassed rows). This is a different workload from the tables above, so the figures are not comparable with them; the other harnesses' interactive sessions were not measured.

| sessions | sum, total | sum, private | per session, total | same sessions through the gateway (estimate), sum | per session |
|---|---|---|---|---|---|
| 1 | 25.9 MB | 3.1 MB | 25.9 MB | 301 MB | 301 MB |
| 2 | 30.7 MB | 6.2 MB | 15.4 MB | 306 MB | 153 MB |
| 4 | 37.7 MB | 12.5 MB | 9.4 MB | 313 MB | 78 MB |
| 8 | 50.5 MB | 25.0 MB | 6.3 MB | 326 MB | 41 MB |

Idle, the sums are 22.4, 27.2, 33.7 and 45.2 MB total and 2.8, 5.5, 11.0 and 22.0 MB private. Through a bypassed gateway (`-m bypass`) the in-flight sums are 26.0, 30.7, 37.5 and 50.7 MB total and 3.1, 6.3, 12.6 and 25.1 MB private, within 0.2 MB of the native ones at every count. (2026-11-06, one run each, 1 to 8 sessions: 19.9 to 42.0 MB total in flight and 2.8 to 22.6 MB private.)

- Private memory grows by about 3.1 MB per session in flight (2.7 MB idle), independent of how many others run. The rest of the total is the binary's pages, about 20 MB, held once (22.4 MB idle total less 2.8 MB private for one session). The marginal session costs about 3.5 MB total in flight. No session started a child process.
- The headless task needs more (5.6 MB per further session, above) because it runs a 28-step tool loop with shell commands.

**The pair does not grow with requests.** 8 simultaneous requests gave 267 to 284 MB total, against an idle 261 to 282 MB (2026-11-25, omp 19.1.1; 2026-11-06, omp 18.12.6: 295 against 306 MB). These requests are rejected at once by the real API (see above), so none was held open.

## The task

One prompt, then a tool loop of 28 model steps that the fake model scripts: 10 file reads, 13 shell commands (`head`, `grep`), 4 file writes, a final answer. The shape comes from the author's own sessions: most are one prompt followed by an agent loop, and tool results are small (median read 2 kB, median shell output 0.5 kB). 12 source files of 1.5 to 12 kB are created for the task. The server rewrites each scripted call into the tool and argument names the harness offered in its request, so every harness does the same operations.

Every harness finished every run (including every concurrent session) with 28 steps, no tool errors, 4 files written and 67 to 71 kB of tool output sent back, so the work is equal. The 2026-11-25 mu runs did the same (7/7 alone, 14/14, 28/28 and 56/56 at 2, 4 and 8 sessions; 28 steps, 69 kB, 4 files).

Compared with the 2026-10-31 run, mu's wall time doubled (0.04 to 0.08 s, 0.03 to 0.06 s CPU) and its peak total rose from 25 to 32 MB (the binary grew by about 5 MB over 122 commits); its private memory did not change. omp's total is 380 MB here against 286 MB then, with the same version and similar private memory (245 against 225 MB). The earlier run used the installed executables directly, which other processes on the machine may have shared (not checked); the benchmark now runs private copies.

Compared with the 2026-11-06 run, mu's peak total rose from 32 to 46 MB and its private memory from 4 to 5 MB, over 181 commits and a binary that grew from 69.2 to 93.7 MB; wall time went from 0.08 to 0.09 s and CPU from 0.06 to 0.05 s (within the range of the runs). The memory increase is mostly the binary's shared pages. How much of it is the code and how much the 2-CPU machine was not separated.

## Results

One session at a time, 7 rounds, medians. The harnesses are interleaved within each round. One untimed warm-up run each comes first, so first-run setup (opencode's database, omp's native libraries) is not charged to the measured runs. The mu row is 2026-11-25 (2 CPUs; wall 0.08 to 0.11 s over the 7 runs); the other rows are 2026-11-06 (7 CPUs) and were not re-run.

| harness | wall | CPU | peak private | peak total |
|---|---|---|---|---|
| **mu** (no gateway) | 0.09 s | 0.05 s | 5 MB | 46 MB |
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
- **The interactive UI of the other harnesses.** The comparison is headless. An idle interactive session, and any first-run or login screen, is not measured for them. mu's idle terminal UI measured about 24 MB total and 3.4 MB private on 2026-11-25 (20 MB and 4 MB on 2026-11-06) with a separate script (`scripts/measure-idle.sh`, three runs of 24.1 to 24.3 MB).
- **Quality and speed with a real model.** Task success, model latency and token cost are outside this benchmark.
- **codex, gemini-cli, crush, aider.** codex needs the OpenAI Responses protocol, which the fake server does not speak; the others are not installed.
- **One machine, one scenario.** 7 rounds. Ranges are in the JSON output; the wall-time ranges are narrow, but a different machine will give different absolute numbers.

## Method

- A fake model server speaks the Anthropic Messages protocol and replays the scenario. mu is pointed at it with `MU_GATEWAY_URL`; claude with `ANTHROPIC_BASE_URL`; omp and pi with a provider entry in their model config; opencode with a base-URL override. Each runs in a scratch `HOME`, with the config that names the server kept per session.
- Memory is `Pss` and `Pss_Anon` from `/proc/<pid>/smaps_rollup`, summed over the process tree; for several sessions, summed over the trees of all live sessions in one scan of `/proc`. CPU and peak RSS come from the kernel's resource accounting of the child.
- Each harness runs from a private copy of its executable (for pi, of node), so that other processes running the same file do not change its `Pss`.
- pi was installed into `~/.cache/mu-bench-tools` with `npm install --prefix`, not into the system.
- Reproduce: `cargo build --release -p mu-bench && ./target/release/mu-bench compare crates/mu-bench/scenarios/compare-agentic.toml --runs 7 --sessions 2,4,8`. Details, the scenario format and how to probe another harness are in [`crates/mu-bench/README.md`](crates/mu-bench/README.md). Run it on a machine that is otherwise quiet: with 8 sessions of opencode it needs about 3.5 GB. The 2026-11-25 mu rows came from the same command with `--harness mu`, which needs none of the other harnesses installed.
- Native-provider and interactive tables (Linux; need `tmux` and `python3`): `scripts/measure-sessions.sh -m native target/release/mu 1 2 4 8` runs N interactive sessions against `scripts/fake-anthropic.py`, and `-m bypass` does the same through `MU_GATEWAY_URL`. A table row is the median of 5 runs with the rows interleaved (one run of each row per round). For the "before native providers" row, build the parent of that commit in a separate worktree (`git worktree add --detach <dir> <commit>^`, `CARGO_TARGET_DIR=<elsewhere> cargo build --release -p mu`) and pass its binary. The script aborts if another process runs the binary, since that would share its pages.
- Gateway pair: `scripts/measure-gateway.sh 1` and `scripts/measure-gateway.sh 8` (an empty `HOME`, `omp` on `PATH` or `MU_GATEWAY_OMP=~/.mu/bin/omp` for the omp that mu pins, the network). The requests are rejected at once by the real API and no real credential is sent. A held-open model stream through the gateway cannot be measured offline (see `crates/mu-bench/README.md`).

## History

This file previously held an informal comparison from 2026-10-25 (one real-model task, 1 to 5 runs per tool, reasoning effort not pinned, an idle-memory table). It is replaced because its method was not repeatable, and its idle figure for mu (7 MB) did not reproduce: on this machine mu's idle terminal UI measures about 17 MB total. The old text is in the git history.
