# Desktop app: wireframes

Structure and states only: no colour, type or exact sizes, which the GPUI sketch app settles later. These are drafts to react to: a section is marked **agreed** once decided. Decisions behind them are in [gui.md](gui.md), and the survey they draw on is in [gui-references.md](gui-references.md).

Conventions:

- `▸` marks a collapsed block and `▾` an expanded one.
- `[Enter]` and the like are key hints shown in the UI.
- `┃` marks the focused tile's edge.

## 1. Window shell (agreed)

### The model

- **Tabs** run along the top. Each tab holds its own arrangement of tiles, and named layouts save and recall those arrangements.
- **Tiles.** The main area of a tab is tiled like a tiling window manager:
  - A tile shows one view.
  - Splitting the focused tile divides it, alternating direction (the "spiral" or "dwindle" scheme of bspwm or Hyprland).
  - Tiles are resized, swapped and closed by key, and one can be zoomed to fill the tab and zoomed back.
- **Bars.** The left bar and the right bar are docked at the window's edges, outside the tiling.
  - Each is optional, and you choose what it holds. Several views in one bar are stacked or tabbed inside it.
  - A key toggles each bar, and another focuses it.
- **Views:** session, agent list, review, jobs, plan, files.
  - Any view can go in a tile or in a bar.
  - A review, jobs, plan or files view follows the most recently focused session. Pinning it locks it to one session, which its header then names.
- **Moving between tiles and bars:** keys move focus by direction (left, right, up, down), and the bars count as the outermost columns. The focused tile has an accent edge (`┃`), and the headers of the other tiles are dimmed.
- **Requests from other agents** show only as `⚑` in the agent list and on tab titles. There is no attention strip. A key jumps to the oldest request waiting on you; see section 3.
- **The status bar** at the bottom covers the whole app: profile, totals, and whether each remote host is connected.

### 1a. A default layout: agent list on the left, two tiles

```
┌────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ ⚑ Design · 2 │ ● Release │ +                                                                 ⌘P │
├─────────────────────────┬────────────────────────────────────────┬─────────────────────────────────┤
│ ⌕ Find agent…           │┃ Desktop app design       local    ⋯  │ Fix flaky attach test  devbox ⋯ │
│                         │┃                                      │                                 │
│ NEEDS YOU               │┃  ┌──────────────────────────────────┐│  (transcript)                   │
│ ⚑ Deploy pod adapter    │┃  │ let's work through the design    ││                                 │
│   test-cluster · 4m     │┃  └──────────────────────────────────┘│                                 │
│ ⚑ Fix flaky attach test │┃                                      │                                 │
│   devbox · 12m          │┃  ⌕ Read  docs/remote-contracts.md    │                                 │
│                         │┃  ⌕ Grep  `Busy` in crates/           │                                 │
│ MU                      │┃                                      │                                 │
│ ● Desktop app design    │┃  Prose of the reply, proportional,   │                                 │
│   local · gui-design    │┃  wrapped to the tile's width.        ├─────────────────────────────────┤
│   +245 −13 · 2m   $3.10 │┃                                      │ Review · Desktop app design  📌 │
│ ○ Bump omp to 0.46      │┃  ✎ Edit  docs/design/gui.md  +180    │ Branch since main  +245 −13     │
│   local · maint · 3h    │┃   ┌ diff in full ───────────────────┐│ ▾ docs/design/gui.md      +180  │
│ ✓ Release notes         │┃   │ + # Desktop app: design         ││   + # Desktop app: design       │
│   local · 1d      $0.08 │┃   └─────────────────────────────────┘│   + …                           │
│                         │┃  ⠋ Thinking · 14s                    │ ▸ docs/README.md        +1 −1   │
│ BREWERY                 │┃──────────────────────────────────────│                                 │
│ ○ Formula audit         │┃  Message mu — @ files, / commands    │                                 │
│   build-pod-3 · 2d      │┃                                      │                                 │
│                         │┃  opus-5 · high · edits  41%  $3.10 ■ │                                 │
├─────────────────────────┴────────────────────────────────────────┴─────────────────────────────────┤
│ μ private   3 running · 2 need you · today $4.79                         test-cluster ✓  devbox ✓  │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Left bar: the agent list.** Rows are grouped by project, with a pinned "Needs you" group at the top.
  - A row shows a state icon (`⚑` needs you, `●` running, `○` idle, `✓` finished and unseen, `✗` failed), the title and a meta line: host, branch, `+N −M`, age, cost.
  - The rows themselves are drawn in section 5.
- **Tiles.** The focused session fills the left tile. The right column is split in two: another session above, and below it a review pinned to the first session (`📌`).
- **Session tile.** A header (title, host, menu), the transcript as a column, then the composer with the session bar under it: model, thinking level, permission mode, context share, cost, stop.
- **Tab titles** carry the most urgent state of the sessions in the tab (`⚑ Design · 2` means two of them need you).

### 1b. No bars: tiles only

```
┌────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ ⚑ Design · 2 │ ● Release │ +                                                                 ⌘P │
├──────────────────────────────────────────────────┬─────────────────────────────────────────────────┤
│┃ Desktop app design                     local ⋯  │ Deploy pod adapter              test-cluster ⋯ │
│┃ (transcript)                                     │ (transcript)                                    │
│┃                                                  │                                                 │
│┃ (composer, session bar)                          │ (composer, session bar)                         │
├──────────────────────────────────────────────────┴─────────────────────────────────────────────────┤
│ μ private   3 running · 2 need you · today $4.79                         test-cluster ✓  devbox ✓  │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Without the agent list**, you navigate by:
  - tabs;
  - Ctrl+Tab through recent sessions;
  - the palette's agent search (section 6);
  - opening the agent list as a tile or bar for a moment.

### Open in this section

- **Default keys for splitting, resizing, swapping, zooming and moving between tiles.** These are new actions in a new key context; they are settled with the palette in section 6.
- **What a new window shows the first time:** a starting named layout.

## 2. Transcript (draft)

What a turn contains, from the agent's events (`crates/mu-base/src/events.rs`, `AgentEvent`):

- the user's prompt, with images and skill calls;
- a "woken" mark for a turn that no prompt started (a finished job or a `mu notify` message);
- prose;
- thinking;
- tool calls: the arguments stream in, the call runs with its output streaming, the call ends, or it is skipped;
- the todo list as it changes;
- retries and notices;
- errors;
- compaction;
- results of background jobs handed back to the model;
- a parked plan;
- the end of the turn: normal, aborted or failed.

### 2a. One turn, at rest

```
│┃  ┌──────────────────────────────────────────────────────────────┐   │
│┃  │ make attach reconnect without asking when the client id is   │   │
│┃  │ the same, and add a test                          [image 1]  │   │
│┃  └──────────────────────────────────────────────────────────────┘   │
│┃                                                                     │
│┃  ▸ Thought for 14s                                                  │
│┃  ⌕ Read   src/run/serve.rs  L340-400                                │
│┃  ⌕ Read   src/run/attach.rs                                         │
│┃  ⌕ Grep   `reconnect` in src/ · 6 matches                           │
│┃                                                                     │
│┃  The agent already ignores `reconnect` (it matches the hello with  │
│┃  `..`). I'll make a hello with the same client id take over        │
│┃  without `Busy`, and test it with two connections.                 │
│┃                                                                     │
│┃  ✎ Edit   src/run/serve.rs  +6 −2                                   │
│┃  ┌────────────────────────────────────────────────────────────────┐ │
│┃  │ 370   if let Some(old) = &self.client {                        │ │
│┃  │ 371 −     if old.who == hello.client && !hello.client.is_empty│ │
│┃  │ 371 +     let same = !hello.client.is_empty() && old.who == …  │ │
│┃  │ …                                                              │ │
│┃  └────────────────────────────────────────────────────────────────┘ │
│┃  $ cargo test -p mu --test agent reconnect          ✓ 0 · 41s      │
│┃    test result: ok. 2 passed; 0 failed                   (2 lines) │
│┃                                                                     │
│┃  ◆ Subagents  2 done                                                │
│┃    ✓ ReviewDiff      scout   1m 12s   "No issues; one nit on …"    │
│┃    ✓ CheckMacOS      task    3m 40s   "Builds and passes on …"     │
│┃                                                                     │
│┃  Done: a hello with the same client id now takes over silently,    │
│┃  and `tests/agent.rs` covers it.                                    │
│┃                                                   4m 52s · $0.84   │
```

- **The user's prompt** is a bordered box in monospace, as Zed does, so it reads as the user's own words, apart from the model's prose.
- **Tool calls are one muted line each:** an icon, a verb, the target, and short facts (a range, a match count, `+N −M`, an exit code, the duration).
- **Reads and searches show no output by default**; Enter expands one.
- **Diffs are shown in full** (with line numbers and whole-line added/removed backgrounds), never collapsed by default. A very long diff shows its first screen and "N more lines", and expands in place or opens in a review tile.
- **Bash shows its last few lines.** The command is a header with its exit code and duration, and a failure has a red edge.
- **Thinking is one line** ("Thought for 14s") that expands. While it streams, see 2b.
- **Subagents are a group:** one row per job, with its kind, duration and the first line of its result. Opening a row shows that subagent's own transcript in a tile.
- **The turn ends with its duration and cost,** right-aligned and muted.

### 2b. A turn in progress

```
│┃  ⠋ Thinking · 6s                                                    │
│┃    The reconnect flag is sent but serve.rs never reads it, so the  │
│┃    fix belongs in join(); check whether take() …                   │
│┃  ⌕ Read   src/run/serve.rs  …                                       │
│┃  $ cargo test -p mu --test agent reconnect                ⠋ 12s    │
│┃    Compiling mu v0.1.0                                              │
│┃    Compiling mu-tui v0.1.0                                          │
│┃                                                                     │
│┃  ↻ Retrying in 8s (2/5): overloaded                                 │
```

- **The live block streams at the bottom.** Thinking shows its last lines, dimmed, while it streams, then folds to one line.
- **A call whose arguments are still streaming** shows what has arrived so far (`…`).
- **A running command shows its live tail and a timer.**
- **Retries, compaction and notices** are one-line rows with their own icons.
- **An error** is a row with a red edge and the message.
- **The view stays at the bottom while you are there.** Once you scroll up it holds still, and shows "↓ new output" until you return.

### 2c. Keyboard in the transcript

A cursor moves block by block, and nothing in the transcript needs the mouse:

- `j`/`k` (or ↑/↓) move between blocks: the prompt, a prose paragraph, a tool call, a diff hunk, a subagent row.
- Enter expands or folds the block under the cursor, and `o` opens it in a tile (a diff in review, a subagent's transcript, a tool's full output).
- `y` copies the block, and `r` quotes it into the composer. This is the TUI's reply picker, generalised to every block.
- `i` (or typing any character) goes to the composer.

### Questions for this section

- How much bash output shows by default (the last N lines, or none), and whether reads and searches stay output-free.
- Whether runs of similar calls (five reads in a row) fold into one row ("Read 5 files").
- Whether the cursor moves only by block, or also by line and word inside a block, as in Delta.
- What the end-of-turn line shows.
