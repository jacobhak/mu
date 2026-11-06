# Memory

mu keeps long-term memory as short markdown files: one file about you, and one per repository. They are loaded into the first prompt of a session, edited with the `memory` tool, reviewed in the background by a cheaper model, and versioned in git so any change can be undone. There is no database and no daemon: the `mu-memory` plugin starts on first use and exits after 10 idle minutes.

## Where it lives

Per omp profile (see [Profile home](usage.md#profile-home)), so work and private memory never mix:

```
<profile home>/memory/            a git repository, one commit per change
  user.md                         who you are, preferences that hold everywhere
  projects/<repo-key>/
    memory.md                     always loaded for this repository
    notes/<slug>.md               longer notes, read on demand
  .state/                         not committed: review cursors, the review queue, the lock
<profile home>/session-index.db   the session search index
```

`<repo-key>` is the main checkout of the clone (the same key durable plans use), so every worktree of one clone shares its memory. Separate clones do not; outside git the working directory is the key.

`user.md` and `memory.md` hold entries separated by a line with only `§`. Entries are short, self-contained statements. They are capped at 1,500 and 4,000 characters (`[memory]` in [config.toml](config.md) changes both). You can edit the files by hand: text without `§` lines is one entry. A file you leave over its cap is not edited automatically until you shorten it.

A note has front matter (`description`, one line shown in the index, and `type`: `project`, `reference` or `feedback`) and free text. A repository has at most 40 notes.

Memory holds what the code, git history and `AGENTS.md` do not say: your preferences and corrections (`feedback`), decisions and constraints (`project`), and where things live (`reference`).

## What the model sees

On the first prompt of a new session the plugin returns a fenced `<memory>` block: your entries, this repository's entries and the note descriptions. The host keeps it in the system prompt for the whole session and restores it on resume, so the cached prefix never changes mid-session. Changes made during the session show in the `memory` tool's replies, not in that block. The block says it is background knowledge that may be stale, and that the code wins when they disagree.

The `memory` tool:

- `read` shows the live state, or one note (`note: "<slug>"`).
- `edit` applies a batch of ops, all or nothing: `add`, `replace` and `remove` on `user` or `project` (`match` must name exactly one entry, ignoring whitespace, quote and dash style; otherwise the reply lists the candidates), `write_note` and `delete_note`. Caps are checked on the result of the whole batch. If it does not fit, the reply shows the file's entries and the excess so one call can consolidate. An exact duplicate `add` does nothing.

Text is passed through the [secret filter](secrets.md) before it is saved.

`session_search` finds earlier conversations; see below.

## Background review

Memory is not only written when the model decides to. A review reads a session and proposes edits with the same ops as the tool:

- before each compaction, in the background while the session goes on;
- at session end, queued in `.state/queue.jsonl` and run when a later session starts (at most 3 per start, oldest first; a failed one stays queued).

The review runs on the `memory` [role](model-roles.md) (`@smol`, then `@default`). It sees your messages, the assistant's text and tool call names with short argument summaries, never tool results, so file and web content cannot reach memory. It is told that most sessions deserve no change, to record only what you said or confirmed, to prefer replacing stale entries over adding, and to stay within the caps. A parse or cap error is retried once; if it fails again you get a warning and the session is tried again later.

Not reviewed: subagent sessions, sessions with fewer than two user turns since the last review, and everything when `[memory] review = false`. Each session has a cursor in `.state/cursors/`, so a review never repeats earlier turns.

## History and `/memory`

Every change is a commit in `<profile home>/memory`: `tool <session>: <ops>` from the tool, `review <session> turns a-b` from the review, and `manual edit` for changes you made by hand (committed just before the next automatic change). Without git on PATH, changes still apply but have no history.

`/memory` lists `User`, `This repository`, each note and `History`. Enter opens a file in `$EDITOR`. In History, Enter shows a commit's diff and `r` reverts it (`git revert`; a conflict aborts and says so).

Memory in one profile is never read from another. Old mnemopi data is not imported and omp's store is left untouched.

## Session search

`session_search` is keyword search (SQLite FTS5) over your profile's sessions, user and assistant text only; tool results and thinking are not indexed. Arguments: `query` (plain words are quoted per term; FTS5 syntax such as phrases, `OR` and `prefix*` works), `scope` (`repo`, the default, covers every worktree of this clone; or `all`) and `limit` (sessions, default 5, at most 10).

Each result is a session (title, date, path) with up to 3 hits and the message before and after each. The current session is left out. The index in `session-index.db` follows the append-only session files and updates on each search; the first search indexes everything and says so when that takes over 2 seconds. No model is called.

## What is never stored

Secrets the filter recognises; tool results and file or web content (the review does not read them); thinking; and anything over the caps. The memory directory is plain text you can read, edit or delete: removing it resets memory, and `git log` shows what was saved and when.
