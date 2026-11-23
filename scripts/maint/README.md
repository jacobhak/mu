# Maintenance bot

A local systemd timer keeps mu current with oh-my-pi; everything is reported through GitHub issues and PRs. `scripts/maint/setup.sh [--profile NAME]` installs it (config in `~/.mu/maint/config.env`, budgets `MU_MAINT_*_USD`, `MU_MAINT_DRY_RUN=1` prints instead of mutating GitHub). Its LLM steps are the agents in `.omp/agents/` (`maint-triage`, `maint-fixer`, `maint-digest`), run by the installed `mu`. When `offload` is installed and reachable, `check.sh` runs its `test` stage (`scripts/ci-checks.sh`) as `offload run -- scripts/ci-checks.sh`; the build, contract and smoke stages stay local (they need a live model and the release binary). `MU_MAINT_OFFLOAD=0` forces everything local; a stage that runs past `MU_MAINT_STAGE_SECS` (default 2400) fails.

| Level | Trigger | Action |
|---|---|---|
| S0 | `scripts/maint/check.sh` fails on `origin/main` | issue `maint:s0`; fixer PR `maint:needs-review`; no bump or publish |
| S1 | new omp release, bump fails checks or cargo cannot resolve the new pin | fixer adapts mu; PR `maint:bump` + `maint:needs-review` (+ `maint:red`). If only the live smoke stage fails the fixer is not run and the PR carries the smoke log |
| S2 | green bump, nothing in mu is wrong after it | PR `maint:bump` + `maint:auto`, squash-merged once the PR's own CI run (the `check` job) passes; if that fails or does not finish within `MU_MAINT_CI_WAIT_SECS` (default 3600) it is labelled `maint:needs-review` and left open. Upstream changes mu only passes through are listed in the PR |
| S2r | green bump, the release leaves mu's own code, prompts or docs wrong | fixer corrects it; PR `maint:bump` + `maint:needs-review`, not merged. If the fixer makes no edit, or edits only tests and docs (`scripts/maint/test-only.sh`), the PR is merged as S2 |
| S3 | weekly | issue `maint:digest` with omp changes worth porting |
| finding | the fixer notices a problem in mu the bump did not cause | issue `maint:finding` |
| publish | `origin/main` is green and newer than the last release | tag `mu-YYYYMMDD-<sha7>`; `.github/workflows/release.yml` builds and releases; the bot reinstalls unless a dev build is installed |

There is one bump PR at a time, on the branch `maint/omp-bump`. A newer omp release rebuilds that branch from `main` and updates the open PR in place, so commits pushed to it are lost; closing the PR unmerged makes the next nightly run open a fresh one.

Nightly runs at 03:30, the digest on Mondays 05:00. The bot never edits `.github/`.
