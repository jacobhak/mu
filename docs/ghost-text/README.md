# Ghost-text evaluation, October 2026

What was tried to improve mu's composer ghost text, what it measured, what shipped, and where the unshipped
code lives. Written when the experiment worktrees were removed, so it can be picked up again later.

## Outcome

- **Shipped:** SmolLM2-135M as an opt-in word engine (`[predict] engine = "smollm"`, `mu predict setup smollm`),
  asked from the first typed letter. The default stays omp's word n-gram. Code: `plugins/predict/src/{smollm,weights,worker,settings}.rs`,
  `src/predict_setup.rs`; docs in the README config block and DESIGN.md.
- **Not shipped:** the phrase model, multi-word ghosts, the 360M model, tuned n-gram parameters, and every bench
  improvement listed below. All of it lives on the archived branches.
- **Headline:** with mu's real accept rule SmolLM2-135M saves roughly 4-6 net keystroke points more than the n-gram
  (about 17-19% against 12-13%). Nothing else we tried moved the n-gram by more than about 1 point.

## Ground rules the numbers depend on

- **Accept inserts exactly the ghost: no trailing space.** This is deliberate (omp's Tab adds the space; mu's composer does not).
  Do not benchmark the space back in. The bench on `main` still credits a free space after an accepted ghost and prints gross
  savings only, so its percentages run about 10 points high. The accurate bench is on `predict/eval` (`--space off`, the default there).
- **Metrics.** Gross KSR = keystrokes saved / typed characters. Net KSR charges each wrong ghost one keystroke, counted once per
  word however long it stays up. Wrong ghosts per 100 words is reported next to both. The cost of a wrong ghost is an assumption:
  it has never been measured (see Open questions).
- **Replay.** Prompts from omp `history.db`, Claude Code `history.jsonl` and mu `history.jsonl` (about 4,000 prompts), replayed in
  time order with online learning, so a prompt only ever sees earlier ones. omp's `use_count` is expanded into repeat sightings.
  Windows are the last N single-line sightings; they drift as history grows, so rows from different runs are not strictly comparable.
- **Typist model.** Accepts a ghost whenever it is right, whole ghost, one keystroke. Optimistic by construction.

## Branches (worktrees removed, branches kept)

| Branch | Tip | What it holds |
|---|---|---|
| `predict/eval` | `7a88f9d` | Bench rewrite: tau sweep, net KSR, `--accept`, `--space`, `--smollm DIR`; phrase model (`phrase.rs`) with expected-value gating; `bench ctx` (reply context), `bench stock`, `bench replies`, `bench quotes` (`convo.rs`); `bench stems` (`stems.rs`) |
| `predict/tune` | `56024cb` | `bench tune`: coordinate and random search of the n-gram `Params`, three disjoint windows, paired bootstrap (`tune.rs`) |
| `predict/multiword` | `3af94be` | Vendored `pi-predict` with `SmolLm::complete_phrase`, `bench mw` (multi-word, latency, memory); `MU_SMOLLM_WEIGHTS` override for the 360M weights |
| `feat/predict-smollm` | merged | The shipped opt-in engine |

`predict/tune` and `predict/multiword` branch from `882b8f5` (an earlier commit on `predict/eval`). Rebuild with
`cargo build --release -p mu-predict` and run `./target/release/mu-predict bench ...`.
SmolLM weights for the bench: `mu predict setup smollm` or the pinned files under `~/.cache/mu-predict-bench/`.

Raw outputs of the runs below are in [`raw/`](raw/). The outputs of `bench tune`, `ctx`, `stock`, `replies`, `quotes` and `stems`
were not saved to files; their key numbers are reproduced here and each can be rerun with the command shown.

## Results

### 1. What the n-gram actually delivers

Accept without trailing space, last ~575 sightings (`raw/01-*`, `bench --last 600` on `predict/eval`):

| Engine | tau | Net | Gross | Wrong per 100 words |
|---|---|---|---|---|
| n-gram | 0.15 | 10.5% | 21.1% | 59.7 |
| n-gram | 0.3 | **12.5%** | 19.4% | 38.8 |
| n-gram | 0.5 | 12.3% | 15.5% | 18.2 |

With the trailing space credited (omp's Tab, `--space on`) the same n-gram scores about 30% gross, matching omp's published 30.3%.
So the original "30% of keystrokes saved" was an accounting difference, not a bug.

### 2. SmolLM2-135M against the n-gram (`raw/01-*`, 573 sightings)

| Engine | tau | Net | Gross | Wrong per 100 words |
|---|---|---|---|---|
| SmolLM2-135M | 0.15 | **18.9%** | 26.4% | 42.6 |
| SmolLM2-135M | 0.3 | 16.7% | 21.6% | 27.4 |
| SmolLM2-135M | 0.5 | 13.1% | 15.2% | 11.5 |

A second window (192 sightings, `bench stems`): SmolLM 19.3% against n-gram 12.0% at tau 0.15; 16.6% against 13.7% at tau 0.3.

### 3. Things that did little or nothing

| Idea | Result | Where |
|---|---|---|
| Tune the n-gram `Params` on this user's history | TEST net 12.90% to 13.29%, diff +0.40 points, 95% CI [+0.20, +0.60]. Nearly all from `cache_weight` 0.5 to 0.25; tau also moved 0.3 to 0.4. The top-3 candidates were one parameter set. Gross rose 0.9 but wrong ghosts rose too, so the gain depends on the wrong-ghost cost being about 1 | `predict/tune`, `bench tune` |
| Phrase model from history (word n-gram over past prompts) + expected-value gating | 13.7% against 12.7% net on the last 600 (and 13.1 to 13.7, 12.8 to 13.7, 13.2 to 13.7 on windows of 200/600/2000). Needs heavy smoothing (alpha 8); context order 2-4 makes no difference | `predict/eval`, `phrase.rs` |
| Prepend the assistant's last message as context (120 turns) | n-gram -1.45/-0.69/-0.64 net points at tau 0.15/0.3/0.5; SmolLM +2.09/+0.81/+0.80, not significant at n=120 (about 120 keystrokes, no test) | `bench ctx` |
| A ghost at an empty buffer (stock replies) | Online top-3 hit rate 3.0% globally, 5.3% per project; saves 0.12-0.17% of keystrokes. About 21% of history sightings are 20 characters or less, but spread thin ("yes" 1.6%, "go" 0.6%, "continue" 0.5%) | `bench stock` |
| Truncate wrong ghosts to the right stem (oracle, uses the future) | n-gram +1.1 to +1.6 net points; SmolLM +1.1 to +1.2. About 38-58% of wrong n-gram ghosts are ending-only misses, but only about 7-11% share 2+ leading characters with what was typed, so cutting the ghost rarely helps | `bench stems` |
| Combine the two engines (show their common prefix; SmolLM if the n-gram agrees) | Worse at every tau (net 11.0% against 16.6% at tau 0.3). Wrong ghosts fall to 9.7 per 100 words only because far fewer ghosts are shown | `bench stems --smollm` |

### 4. SmolLM2 multi-word ghosts (`raw/02-*`, last 100 sightings, whole-ghost accept)

| Config | Net | Wrong per 100 words | Characters saved per accept |
|---|---|---|---|
| word only, tau 0.15 | 18.3% | 46.4 | 1.7 |
| word only, tau 0.3 | 15.5% | 29.1 | 1.6 |
| best phrase length by expected value, gate 0.5 | **20.1%** | 48.2 | 3.2 |
| same, gate 1 | 18.4% | 34.0 | 3.3 |

Fixed 2- and 3-word ghosts gated on cumulative confidence almost never clear the threshold. Ghosts after a space (empty prefix) hurt.
With a hypothetical word-wise accept key (does not exist) the gate-0.5 config reaches 23.2%. Needs a vendored or patched `pi-predict`
(the search and KV cache are private), 8,993 added lines on `predict/multiword`. Not shipped.

### 5. SmolLM2-360M against 135M (`raw/03-*`, 60 sightings, two windows sharing 57 prompts)

| | 135M | 360M |
|---|---|---|
| Net, word only, tau 0.15 (window 1 / 2) | 16.8% / 19.1% | 19.5% / 21.3% |
| Net, expected-value phrases, gate 1 (window 1 / 2) | 16.6% / 19.3% | 20.8% / 21.9% |
| Word latency p50 / p90 | 50-54 / 92-129 ms | 95-109 / 151-228 ms |
| Phrase latency p50 / p99 | 132-138 / 256-296 ms | 240-261 / 2,642-6,971 ms |
| Process RSS after a run | about 296 MB | about 576-581 MB |

360M was ahead in all 12 paired cells (by 0.9 to 4.2 points), but the windows are effectively one 60-prompt sample and the 135M model itself
moved 2.3 points between them. Latency tail is too slow for ghost text. Not pursued.

### 6. Production path and the two-letter gate (`raw/04-*`, 94 sightings)

The first version of the opt-in engine only asked SmolLM after two typed letters (undocumented, added by the implementing agent). Gross savings
and wrong ghosts per 100 words, with this bench's free-space accounting (compare rows, not absolute values):

| Engine | tau 0.15 | tau 0.3 | tau 0.5 |
|---|---|---|---|
| n-gram | 34.2% / 56.0 | 31.8% / 38.4 | 26.0% / 18.1 |
| SmolLM, asked from 1 letter | **40.8% / 41.5** | **33.7% / 26.5** | 23.4% / 11.2 |
| SmolLM, asked from 2 letters | 31.7% / 27.4 | 27.7% / 19.7 | 21.6% / 8.7 |

The gate cost SmolLM about 9 points at tau 0.15 and put it below the n-gram, so it was removed. The 94-prompt sample is small; a full-size
re-measure of the production path with corrected accounting has not been done.

### 7. Cost of the shipped engine (agent measurements on this 7-core Linux box)

| Threads | p50 | p90 | p99 |
|---|---|---|---|
| 1 | 106 ms | 236 ms | 282 ms |
| 2 | 66 ms | 124 ms | 148 ms |
| 4 (default) | 40 ms | 80 ms | 100 ms |
| all (7) | 33 ms | 67 ms | 88 ms |

Weights 145 MB (Q8_0). Plugin RSS about 230 MB with SmolLM against 33 MB for the n-gram. Loads in the background (about 140 ms with warm page
cache, so `initialize` answers at once); the first keystrokes after start can exceed the 300 ms host timeout while it warms up.

### 8. Side findings, not about prediction

- Replies to a question (assistant message ending in `?`, or a numbered list): 28.5% of session prompts but only 7.7% of typed characters
  (28% excluding pastes over 500 characters); median 55 characters, 41% at most 40 characters. A one-key answer could save at most about 0.5%
  (2.3% excluding pastes) of typed characters, so the `ask` tool work was justified by cognitive load, not keystrokes. (`bench replies`)
- About 4.1% of prompts reuse 30 or more characters of the previous assistant message verbatim (quotes): 48% phrases of 30-59 characters,
  32% about a sentence, 17% a few sentences. This motivated the reply picker. (`bench quotes`; its structure-alignment check was not trustworthy)

## Open questions and ideas not tried

- **True cost of a wrong ghost.** Every net figure assumes one keystroke. Live telemetry (accepted, dismissed, typed past) is the only way to
  measure it, and the best tau depends strongly on it (0.15 at cost 0.5, 0.3 at cost 1, 0.5 at cost 2).
- **Ceiling analysis.** Oracle accuracy when the right word is in the top 1, 3, 5 or 10 candidates, to tell whether showing several candidates
  (SmolLM already computes three) could help.
- **Fine-tuning SmolLM2 on the user's prompts** (LoRA on about 4k prompts, export to GGUF Q8_0); never tried.
- **More in-domain text for the n-gram** (commit messages, docs). omp's published figures used about 58k prompts of history; mu has about 4k.
- **Word-wise accept key** plus multi-word ghosts: about +5 net points in the hypothetical, requires owning the `pi-predict` changes.
- **Other models in the class** (Granite 4.0 Nano 350M, Gemma 3 270M, Qwen3-0.6B-Base): omp's decoder only loads Llama-architecture GGUF, so each
  needs decoder work. SmolLM2-360M is the one that drops in unchanged and it did not look worth the latency.
- **Re-measure the production path** (min prefix 1) on a few hundred prompts with the corrected accounting.

## Lessons for next time

- Check the simulator against the product: the biggest swing in this whole exercise (30% to 20%) was the trailing space, not any model.
- Evaluate on a window and a held-out window; the 60- and 94-prompt runs drift by 2+ points between nearly identical windows.
- SmolLM runs are slow (about 10-40 minutes per few hundred prompts on CPU, worse if two run at once). Print progress, run one at a time.
- Filter harness runs under `/tmp` out of session transcripts: they send thousands of one-word prompts ("go" 2,558 times) and wreck any statistic.
- Agent reports sometimes summarised without the data; only the saved output files could be trusted.
