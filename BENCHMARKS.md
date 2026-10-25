# Benchmarks: mu vs other open-source coding harnesses

Run 2026-10-25 on one Linux x86_64 machine. Small, informal measurements, not a rigorous study.

Versions: mu (git 8bb3be2), omp 18.12.6, opencode 1.3.3, codex 0.150.1, claude 2.1.289, pi 0.73.1, gemini-cli 0.65.0, crush 0.98.2, aider 0.86.2.

## 1. Idle memory

Interactive TUI left at its start screen for 12 s, in a private tmux server. PSS summed over the tool's whole process tree.

| Harness | Idle PSS | Processes |
|---|---|---|
| **mu** | **7 MB** | 1 |
| crush | 66 MB | 1 |
| aider | 70 MB | 1 |
| codex | 87 MB | 2 |
| pi | 108 MB | 1 |
| claude | 164 MB | 1 |
| omp | 192 MB | 1 |
| gemini | 341 MB | 2 |
| opencode | 577 MB | 1 |

Caveats:
- mu's plugins (gateway, web, memory) start on first use, so 7 MB is a best case. During a model call mu's own process peaked at about 11 MB PSS. The shared gateway and broker add about 300 MB PSS (see section 3).
- A tool sitting on a first-run, login or trust prompt may be measured at that screen and not the full UI. The screens were not checked.

## 2. Task: fix a failing test (agent harnesses)

Fresh copy of a 3-function Python module (`slugify`, `truncate`, `word_count`) with 5 of 6 pytest tests failing. The prompt asks the agent to make the tests pass without editing them, then reply in one sentence. Model: `gpt-6-luna` via an OpenAI API key, for every tool. Pass means the tests pass and the test file is byte-identical.

Every run that completed passed, with the tests untouched.

| Harness | Runs | Wall time, median (range) | Peak RSS (largest process) |
|---|---|---|---|
| pi | 4 | 14.2 s (11.7-17.2) | 175 MB |
| crush | 4 | 14.2 s (13.3-16.4) | 92 MB |
| **mu**, gateway already running | 5 | 17.9 s (12.4-31.0) | 11 MB core + ~300 MB shared |
| **mu**, gateway started by the run | 3 | 20.0 s (18.5-33.4) | same |
| omp | 1 | 30.2 s | 299 MB |
| opencode | 1 | 54.4 s | 450 MB |
| codex | 1 | 55.9 s | 127 MB |

Reading it:
- pi, crush and mu are within noise of each other. Only the large gaps mean anything: those three against omp, opencode and codex.
- mu's spread (12-31 s for the same task) is wide. Cold and warm gateway runs overlap, so gateway startup (about 1-2 s) is small next to model variance.
- omp, opencode and codex have one run each, so treat their times as rough.

### Not compared on time: aider

aider passed in 8.7-9.7 s (4 runs, 244 MB peak RSS), but it is a single-shot editor, not a tool loop. It made one model request (2.7k tokens sent), applied the diff and never ran the tests. I named the files up front, which favors it on a task this small. Its time is not comparable with the agents above and is left out of the ranking. Its idle memory in section 1 is comparable.

## 3. mu and its shared gateway

mu gets non-native providers through one shared `mu-gateway daemon` (one `omp auth-broker` plus one `omp auth-gateway`) used by every mu process. It is guarded by a lock file, and clients keep lease files. The daemon exits after 30 minutes without a fresh lease (`MU_GATEWAY_IDLE_SECS`).

- Four concurrent `mu -p` sessions against a running daemon all answered, and the process counts stayed at 1 daemon, 1 gateway and 1 broker with unchanged PIDs.
- The shared pair costs about 280-300 MB PSS once (broker about 80 MB, gateway about 200 MB). Each extra session adds its own mu process (about 10 MB) and a small plugin shim.
- One session is roughly the same as omp (about 300 MB). The saving starts with the second session. The multi-session saving was not measured against several omp sessions.
- At the time of the benchmark the daemon was keyed by `~/.mu/gateway/` and not by profile, so the first session to start it decided which profile's credentials and `models.yml` it served, and a session in another profile could get a 401 or an "Unknown model" 404. That is fixed: each profile now gets its own daemon under `gateway/profiles/<name>/`. The profile-less default path is unchanged.

## 4. Method and caveats

- **One task, one model, small n.** One trivial task and 1-5 runs per tool.
- **Reasoning effort was not pinned.** Each tool used its own default, which affects every wall-time number.
- **claude and gemini-cli** are in section 1 only. They cannot run an OpenAI model.
- **codex** ran with `--dangerously-bypass-approvals-and-sandbox` and an API-key login in an isolated `CODEX_HOME`. Its default sandboxed mode may be slower.
- **aider** needed `use_temperature: false` in its model settings, because `gpt-6-luna` rejects `temperature=0`.
- **mu** reached the model through the omp gateway. The gateway only serves models whose credentials are in omp's credential store and ignores `OPENAI_API_KEY` in the environment, so the key was loaded into an isolated profile with `omp auth-broker migrate --from-local --include-env`.
- **Peak RSS** comes from `/usr/bin/time` and is the largest single process. It is not comparable with the summed PSS in section 1. Its figure for mu (about 1 GB) counts shared pages and is not used.
- pi, gemini-cli, crush and aider were installed into `/tmp/bench/tools` and not into the system.
