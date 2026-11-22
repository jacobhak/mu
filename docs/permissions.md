# Tool permissions

By default mu runs every tool call without asking. `[permissions]` in `~/.mu/config.toml`, `--permissions` and `/permissions` decide when a call needs your approval instead.

```toml
[permissions]
mode = "yolo"          # ask | edits | yolo (default: yolo)
readonly_bash = true   # the built-in read-only command set counts as reading

[permissions.rules]
bash = { "cargo test *" = "allow", "git push *" = "deny", "rm *" = "ask" }
edit = { "*.lock" = "ask" }
"mcp__github_*" = "allow"   # a string decides every call of the tool
```

`--permissions ask|edits|yolo` overrides the mode for one run, and `/permissions` shows or sets it while mu runs (it works mid-turn). The status line shows the mode whenever it is not `yolo`. A [mode](modes.md) with `writes: local` (plan, say) is independent: it keeps the tree read-only whatever the permission mode. The model's `enter_mode` call follows each mode's `model_entry`: `ask` always asks, in every permission mode including `yolo`, with no standing grant; `allow` skips the prompt.

## Modes

| Mode | Runs without asking | Asks |
|---|---|---|
| `ask` | reads inside the project: `read`, `grep`, `glob`, read-only `bash`; and `todo`, `ask`, `web_search`, `fetch` of a host a `web_search` returned, … | everything that writes or runs, reads outside the project, and `fetch` of any other host |
| `edits` | the same, plus file edits inside the project | commands, writes outside the project or into `.git`, and reads outside the project |
| `yolo` | everything | only what a rule says to ask |

"Inside the project" means under its root, with symlinks resolved. `local://` files (plans, notes) count as reading, as do the `artifact://`, `skill://` and `mu://` pages the tools refuse to write, so those always stay quiet; `fetch` and `web_search` read the network, not the machine, and are not gated by their reach. In `ask` mode `fetch` is asked per host, because a model that has read project files can send their content in a URL: the prompt offers `https://host/*` as a grant, and a host that a `web_search` of the same session returned (and one a subagent inherits from its parent) is not asked about. `edits` and `yolo` do not ask. A redirect from an allowed host is followed without a new prompt. Reading outside the project asks in both `ask` and `edits`, so neither mode can walk `~/.ssh` or another checkout without permission; only `yolo` reads anywhere. Anything under the project's `.git` or under `~/.mu` is never covered by `edits`. Subagents run with the same mode and the same session grants.

`readonly_bash = false` makes every `bash` call ask unless a rule allows it. The read-only set covers programs such as `ls`, `cat`, `grep`, `git status`, `git log`, `git diff`, `find` without `-delete`/`-exec`, and pipelines and loops made only of those; anything the classifier is not sure about is not read-only.

A read-only command is judged by the files it may open — its path words and its input redirections — so `cat README.md` and `grep -r TODO src/` stay quiet while `cat /etc/passwd`, `cat < /etc/passwd` and `grep -r x /etc` ask. A glob is judged by its prefix (`src/*.rs` cannot leave `src/`), but a `$VAR`, a leading `~` or an absolute path that leaves the root counts as outside, because mu cannot know where it points. Relative paths are resolved against the session's directory: mu does not track the shell's own `cd`, so a relative read after `cd /etc` is not caught. A flag that names a file (`grep -f`, `rg -f`, `wc --files0-from`, `date -f`) makes that file a path word; `rg` and `tree` run quietly only with flags from a list (no `rg --pre`, no `tree -o`), and `sort --compress-program`, `sort -o` and `git grep -O` are not read-only.

The modes prevent mistakes; they are not a boundary against a model that is trying to get past them. The classifier knows the common forms of each program, and a program will have flags it does not know yet: anything it does not recognise asks, but a flag it recognises wrongly is a gap. Do not rely on `ask` or `edits` to contain code you do not trust.

## Rules

Keys are tool-name globs, values are `allow`, `ask` or `deny`, either for every call of the tool (`"mcp__github_*" = "allow"`) or for commands and paths that match a pattern:

- `bash`: every simple command of the line, as written (`cargo test --release`), including those inside `$(…)`, loops and subshells. A `deny` or `ask` rule also sees the command with quotes, a directory on the program (`/usr/bin/git`) and the wrappers `sudo`, `doas`, `command`, `builtin`, `exec`, `env`, `nice`, `nohup`, `time`, `timeout` and `xargs` taken off, and the commands inside a literal `sh -c '…'`, `bash -c` or `eval` string (three levels deep), so `"git push *" = "deny"` also stops `env git push` and `sh -c 'git push'`. It does not see through a variable, a `cd` or a string built at run time.
- `write`, `edit`, `read`, `grep`, `glob`: each target path, matched either as it is shown (relative to the project when inside it) or by its absolute path, so `src/*` and `/home/jacob/code/private/mu/src/*` both cover `src/a.rs`.
- `fetch`: the URL.
- Other tools: only `"*"` and whole-tool string values match.

`*` matches anything, including `/` and spaces; `?` matches one character; everything else is literal. A pattern ending in ` *` also matches the text before it, so `cargo test *` matches `cargo test`.

A `deny` always wins, then an `ask`, and only then does a covering `allow` (or the read-only classifier) let the call run. `yolo` still honours `deny` and `ask` rules. A command that does not parse, or one that starts with an assignment (`FOO=1 cmd`), can never be covered by an `allow` rule.

One deny is built in and no mode or rule lifts it: a recursive `rm` of `/`, `~` or `$HOME` (also written as `/*`, `~/*`, `"$HOME"`, the home path itself, with `..` resolved so `/..` and `~/../..` count, behind `sudo` and the like, or inside `sh -c '…'` and `eval`). It reads the literal command: `cd / && rm -rf *`, `X=/; rm -rf $X` and `echo ~ | xargs rm -rf` are not caught.

## The prompt

When a call needs approval, mu first asks how much to allow. The widths are landmarks, not every ancestor, so the list stays short however deep the path is:

- **Yes, just this once** — run it now and remember nothing.
- **For what it asked for** — the exact paths or command line the call touched.
- **For its directory** — `/etc/*` for a read of `/etc/passwd`.
- **For this checkout** — the git root the path lives in, `/home/jacob/code/private/mu/*`.
- **For the checkouts beside it** — that root's parent, `/home/jacob/code/private/*`.
- **For all of ~** and **For everything** — `~/*` and `/*`.
- **e** — type a pattern of your own, prefilled with the directory, so anything else is one keystroke away.
- **No** — do not run it. Esc on this step denies.

Then, for any width, where to remember it:

1. **This session** — until mu exits.
2. **In this project** — saved under `~/.mu/permissions/`, per checkout (worktrees share it). Never in the repository. The file is named after the checkout's path with `/` written as `-`, so `~/code/a-b` and `~/code/a/b` name the same file; each file records the checkout's absolute path (`root`) and applies only to that one. For any other project the file is ignored with a warning, and a new grant is not saved over it.
3. **Everywhere** — added to `~/.mu/config.toml`.
4. **No** — do not run it.

Esc on the second step goes back to the widths without answering. Tab (or Right) attaches a note for the model at either step, and a note on **No** lets the turn continue instead of ending it.

The grant offered is the program plus its subcommand (`cargo test *`) for a command that runs, the paths a write, edit or read touches, or the site of a `fetch`. A read-only command that leaves the project is granted as the line it ran, with its outside path in place (`cat /etc/passwd`), and widens by that path (`cat /etc/*`, then `cat /home/*`).

A file tool widens the same way wherever the path is: `ask` mode prompts for a write inside the project too, and `src/parser/a.rs` offers `…/mu/src/parser/*`, then `…/mu/*`, the checkouts beside it, `~` and `/`. Those widths are absolute, which is also how a rule matches an inside path, so saving one holds for the files it names. Since a grant is an explicit approval, it can cover what the mode alone would not: `ask` mode plus `…/mu/*` grants writes anywhere in the checkout, `.git` included, just as `cargo test *` silences a command that `edits` would otherwise ask about.

A grant made through `read`, `grep` or `glob` is saved for all three, since they reach the same files, and so is one made through `write` or `edit`, since they change the same paths: approving the write of a file also covers editing it. The two families never cross, so a read approval does not allow writing and a write approval does not open reads. A directory grant also includes the directory itself (`/etc` as well as `/etc/*`), so searching the granted directory is covered. Commands keep their own grant: `cat /etc/hosts` is still its own decision. A rule stored for a directory is a glob, so it covers everything below it.

A denial with a note lets the turn continue: the model sees `Not run: the user denied this bash call: <note>` and can try something else. A denial without a note ends the turn, as Esc does.

Rules that deny show up in the tool result: `Not run: permission rule "git push *" = "deny" for bash (~/.mu/config.toml).` Rules are read at startup; a run already going does not see an edit made meanwhile.

## Subagents and headless runs

A `task` subagent inherits the mode and the session grants, and its prompts are forwarded to your UI labelled with the subagent that asked (`from Task1 (task)`).

A `-p` run has nobody to ask, so a call that needs approval does not run: stderr says `approval needed: bash: touch x` and the model is told nobody can approve it in this run. A read outside the project is refused the same way, so a scripted run that reads elsewhere needs `--permissions yolo` or a rule. `mu deliver` runs its subprocesses with `--permissions yolo`: approving the plan is the authorization, and deny rules still apply.
