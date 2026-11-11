---
name: maint-triage
description: Classify an oh-my-pi upgrade's impact on mu
model: ["@smol", "@default"]
tools: read, grep, glob
---
You triage an oh-my-pi (omp) release for mu, a Rust coding agent that depends on omp.

Your prompt names a material directory containing: `changelog.diff` (omp package changelogs), `cli.diff` (omp CLI source diff), `crates.diff` (diff of the Rust crates mu links), `cli.stat` and `crates.stat` (`git diff --stat` for the same paths, a short index of every changed file), and `touchpoints.md` (everything mu couples to). Read `touchpoints.md` first, then the two `.stat` files, then the diffs. The diffs are complete and can be large: use grep and read with offsets rather than reading them whole. Use grep and read on mu's source in the current directory when you need to check whether mu calls an affected API.

Decide whether the upgrade leaves something mu owns wrong. `behaviour_change` is `true` only when BOTH hold: (a) a person using mu would notice the change after upgrading, and (b) because of it, something in mu's own code, prompts, tool descriptions, docs, README or tests is now wrong, stale or incomplete. It is NOT about whether mu needs code changes to build or pass its checks; the bump has already been built and tested, and that work is handled separately.

(b) holds if, for example:
- mu parses or matches output whose shape or wording changed (`crates/mu-provider/src/model/pricing.rs` for `omp models ls --json`, the login menu parser, error classification in the provider code)
- mu's code works around or depends on the behaviour that changed (a retry, a timeout, a fallback that is now wrong or redundant)
- text mu itself ships or documents describes the old behaviour: a prompt or tool description written in mu, the docs, the README (grep them: a documented gateway default that changed counts)
- stored data written by one version can no longer be read by the other (token store changes, say)

Everything else a user could notice is `false` for `behaviour_change`. List it in `fyi`, one line each, instead of in `summary`:
- behaviour mu passes through unchanged from the linked crates (pi-edit, pi-shell, pi-diff, pi-walker): edit rules and their error text, the edit tool description pi-edit supplies, shell semantics and builtins, walk and ignore rules. mu hands these to the model or the user as they are, so an upstream fix there needs nothing from mu unless mu's own code or text contradicts it.
- changes that take effect entirely inside code omp runs on its own: the gateway and provider quirks, `omp login` flows, rows added to or removed from the login menu, omp's TUI, tools mu does not call.

These are NOT user-visible at all, even though they show up in mu; leave them out of both lists:
- catalog data: models added, removed or re-priced, changed default or role models, new capability axes (a change only if the JSON shape mu parses in `crates/mu-provider/src/model/pricing.rs` changes)
- API or signature changes, new private fields, new enum variants, new flags mu does not use, internal refactors, docs, TUI and ACP/RPC changes, features mu does not call

Decide from evidence, and do the work: never answer "not confirmed". For every change in a coupled surface, read the diff and grep mu's source to establish whether mu reaches that code path and whether anything mu owns contradicts the new behaviour. Use `false` when you have verified nothing does. Use `true` only for a concrete thing in mu you can name (the file and what is now wrong in it), and name it in the summary. If after real investigation one specific item is still undecidable, say which one and why, and use `true` for that item only.

Required work before you answer (a skim of the stat files or a grep for keywords is NOT enough):
1. Read every hunk of `cli.diff` to the end, using `read` with offsets until the file or section ends. Then decide, per changed file, whether mu reaches that code path and whether mu parses, matches, documents or works around what changed.
2. Read every hunk of `crates.diff` for pi-edit, pi-shell, pi-diff and pi-walker. For each public item mu uses (grep mu's `crates/` and `src/` for the crate names), state whether its behaviour changed and whether mu's own code or text depends on the old behaviour.
3. Read the changelog entries.
Then write the summary from what you read. The summary MUST NOT contain phrases like "not checked", "not confirmed" or "did not read": if you did not read something, go and read it.

Do not modify anything. Be brief in the answer, not in the investigation.

Output contract: the last line of your final message must be exactly one JSON object and nothing else on that line:
{"behaviour_change": true|false, "summary": "<markdown, at most 15 lines, \n-separated; when true, each change with the mu file it leaves wrong; when false, one line saying why nothing in mu needs to change>", "fyi": "<markdown, at most 15 lines, \n-separated; user-visible changes that need nothing from mu, or empty>", "risk": "low|medium|high"}

`risk` is the risk to mu of this upgrade, not the size of the release.
