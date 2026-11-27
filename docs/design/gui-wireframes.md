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
│   2m              $3.10 │┃                                      │ Review · Desktop app design  📌 │
│ ○ Bump omp to 0.46      │┃  ✎ Edit  docs/design/gui.md  +180    │ Branch since main  +245 −13     │
│   local · maint · 3h    │┃   ┌ diff in full ───────────────────┐│ ▾ docs/design/gui.md      +180  │
│ ✓ Release notes         │┃   │ + # Desktop app: design         ││   + # Desktop app: design       │
│   local · 1d      $0.08 │┃   └─────────────────────────────────┘│   + …                           │
│                         │┃  ⠋ Thinking · 14s                    │ ▸ docs/README.md        +1 −1   │
│ BREWERY                 │┃──────────────────────────────────────│                                 │
│ ○ Formula audit         │┃  Message mu — / for commands         │                                 │
│   build-pod-3 · 2d      │┃                                      │                                 │
│                         │┃  opus-5 · high · edits  41%  $3.10 ■ │                                 │
├─────────────────────────┴────────────────────────────────────────┴─────────────────────────────────┤
│ μ private   3 running · 2 need you · today $4.79                         test-cluster ✓  devbox ✓  │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Left bar: the agent list.** Rows are grouped by project, with a pinned "Needs you" group at the top.
  - A row shows a state icon (`⚑` needs you, `●` running, `○` idle, `✓` finished and unseen, `✗` failed), the title and a meta line: host, branch, age, cost.
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

## 2. Transcript (agreed)

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
```

- **The user's prompt** is a bordered box in monospace, as Zed does, so it reads as the user's own words, apart from the model's prose.
- **The same defaults as the TUI.** The cut-off lengths and summaries below are the TUI's (`crates/mu-tui/src/chat/blocks.rs`). They move into the shared crate with the semantic blocks, so the two UIs cannot drift apart.
- **Tool calls are one muted line each:** an icon, a verb, the target, and short facts (a range, a match count, `+N −M`, an exit code, the duration).
  - They are never folded together: five reads in a row are five lines.
- **Reads and searches show a summary only,** as in the TUI: "N lines", "N matches", "N files". Expanding one shows its output.
- **Diffs are shown in full** (with line numbers and whole-line added/removed backgrounds), never collapsed by default.
  - A diff past the TUI's cut-off shows that much and "N more lines".
  - It expands in place, or opens in a review tile.
- **Bash shows its last 10 lines,** as in the TUI.
  - The command is a header with its exit code and duration, and a failure has a red edge.
  - An error result shows its first 10 lines.
- **Thinking is one line** ("Thought for 14s") that expands. While it streams, see 2b.
- **Subagents are a group:** one row per job, with its kind, duration and the first line of its result. Opening a row shows that subagent's own transcript in a tile.
- **Nothing marks the end of a turn.** Duration, cost and context share live in the session bar.

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

The transcript behaves like a read-only document with a cursor that goes anywhere in it, as Delta's does, and nothing in it needs the mouse.

- **The cursor is the TUI's reply picker** (context `reply` in `docs/keys.md`), extended from the model's replies to the whole transcript, with the same keys:
  - units, words and pages, with counts (`3j`);
  - `{`/`}` between blocks and `[`/`]` between replies;
  - selections, and marks with `space`;
  - Enter quotes the marks and the selection into the composer.
- **Entering and leaving it work as in the TUI:** Ctrl+R from the composer, and Esc or `q` back. Focusing the transcript tile by direction also enters it.
- **What the app adds, as new actions in the `reply` context that the TUI gets too:**
  - a fold action, to expand or fold the block under the cursor. The TUI only has the global Ctrl+O (tool output) and Ctrl+T (thinking);
  - copying the selection or the block to the clipboard.
- **What only the app has:** opening the block under the cursor in a tile (a diff in review, a subagent's transcript, a tool's full output).

### Open in this section

- Keys for the new actions; settled with the palette in section 6.
- Whether a quote can be anchored to its place in the transcript (Delta's comments) rather than copied into the composer as text.

## 3. Requests that need you (agreed)

**Where the TUI already has an interaction, the app does the same thing.** It is drawn better, but has the same steps, the same keys (the `select`, `overlay` and `review` contexts in `docs/keys.md`) and the same results. The app departs from the TUI only for a reason that comes from being a GUI.

What the agent can ask for:

- **An approval** (`ApprovalRequest`, `crates/mu-base/src/permissions.rs`):
  - the request: a title (`tool: summary`), why it needs approval, the body (command, content or arguments), the subagent it was forwarded from, and the grant "don't ask again" would remember;
  - the choices: a model to run the call on instead;
  - the answer: allow once, for the session, the project or everywhere (with patterns picked from the wider levels), or deny, with an optional note.
- **Questions** (`AskQuestion`): a header and context, then options with descriptions and previews. A question can take several answers, have a recommended option, be a yes/no, or be skipped; any answer can carry a note, or be typed freely.
- **A document review** (`ReviewRequest`): a plan or another mode's document, or a `/review` report.
  - The document is markdown, with the actions the mode defines (execute, keep, compact, refine with text, park, …).
  - Some actions continue with a chosen model.
- **Web search setup,** and an interactive `bash` (declined in v1).

### 3a. Where a request sits

```
│┃  $ cargo test -p mu --test agent reconnect          ✓ 0 · 41s      │
│┃  (the transcript scrolls freely above the dock)                     │
│┃━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━│
│┃  bash: git push origin HEAD:refs/heads/fix/attach-reconnect          │
│┃  from CheckMacOS                                                     │
│┃  not covered by your rules (mode: ask)                               │
│┃  ┌────────────────────────────────────────────────────────────────┐ │
│┃  │ git push origin HEAD:refs/heads/fix/attach-reconnect           │ │
│┃  └────────────────────────────────────────────────────────────────┘ │
│┃ ▸ 1  Yes, just this once                                            │
│┃   2  For what it asked for      git push origin HEAD:refs/heads/…   │
│┃   3  For git push               git push *                          │
│┃   4  No                                                             │
│┃  y yes · n no · 1–4 pick · e type a pattern · Enter choose ·        │
│┃  Tab/→ note · Esc cancel                                            │
│┃━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━│
│┃  Message mu — the agent waits for your answer above                 │
│┃  opus-5 · high · ask    41%  $3.10                                ■ │
```

- **A request is docked between the transcript and the composer** until it is answered. The transcript scrolls freely above it.
- **It takes the keyboard when its session's tile is focused,** as the TUI's dialog does.
- **The TUI's steps, rows and hints** (`ApproveState`, `crates/mu-tui/src/widgets/overlay.rs`):
  1. **How much to allow:** yes this once, each model choice, each width the grant offers ("what it asked for", then wider ones), or no. `e` types a pattern instead.
  2. **Where to remember a width:** this session, this project, everywhere, or no.
- **Choices are picked by the TUI's row shortcuts** (`y`, `n`, digits), or by arrows and Enter. The shortcuts are not rebindable, as in the TUI.
- **Any change to labels or steps** (the survey's case for verb labels, for instance) is made in the shared dialog model, so both UIs get it.
- **Once answered,** the dock closes and the answer becomes one line in the transcript, saying who answered from where: "✓ Ran once · answered on devbox". If another connection answered first, the dock closes the same way.
- **Several requests are queued, as in the TUI:** the dock shows one at a time.

### 3b. Questions

```
│┃━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━│
│┃  Desktop app · question 2 of 3                                      │
│┃  The agent accepts one client today; a cockpit needs more.          │
│┃  How does a connection become an observer?                          │
│┃                                                                     │
│┃ ▸ 1  Role in hello  (recommended)       ┌ preview ────────────────┐ │
│┃      Explicit; old semantics kept       │ {"hello":{"role":       │ │
│┃   2  Observer by default                │   "observe", …}}        │ │
│┃      Busy disappears                    └─────────────────────────┘ │
│┃   3  Something else…                                                │
│┃━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━│
```

- **The same steps as the TUI's ask dialog:** one question after another, then a final review, with `back` to the previous step. It also has the same note, skip and free-text rows.
- **Context reads first, dimmed, and the question is the strongest line.**
- **An option with a preview** shows it beside the list while it is highlighted.

### 3c. Document review (a plan)

- **The same dialog as the TUI's review**, in the same dock: the document above, then the actions and the "continue with" model slider.
  - The keys are the TUI's `review` context: Tab between the document and the actions, `c` to copy, Ctrl+G to edit, `n` for a note.
  - The dock grows to hold the document, up to most of the tile's height. The transcript keeps at least a few lines above it.
- **What a GUI adds:** the document renders as real markdown in proportional type, and `o` opens it in a tile of its own when you want more room. Deciding still happens in the dock.

### 3d. A request from an agent you are not looking at

- **It shows only as `⚑`** on the agent's row in the list, and on the title of its tab if one shows it. The OS notification follows the `[notify]` rules.
- **A key jumps to the oldest request waiting on you,** in any session; pressed again, it goes to the next.
  - The session opens in a floating panel over the layout: its transcript, with the request docked at the bottom. The tiles underneath do not change.
  - Once you have answered, Esc closes the panel; it also closes by itself when nothing else waits in that session. `o` instead puts the session in the focused tile.
- **If a tile already shows that session,** the panel still opens, so the jump always behaves the same. That tile shows the answered request as soon as you answer.

## 4. Composer (agreed)

The TUI's composer (contexts `chat` and `editor` in `docs/keys.md`, `crates/mu-tui/src/chat/composer.rs`) with the same keys and behaviour. What it does there:

- **Sending:**
  - Enter sends. While a turn runs, Enter steers (delivered after the current tool call) and Ctrl+Q queues a follow-up.
  - Esc aborts the turn, and queued text returns to the composer.
  - Shift+↑ takes the last queued message back to edit it.
- **Above the composer, the queue:** one row per queued message (`↳ steer: …`, `↳ after turn: …`), the newest with "shift+↑ edit".
- **Completion:**
  - ghost text predicted from history, taken with Tab or → at the end of the line;
  - a `/` menu of commands and their arguments.
  - There is no `@` file completion.
- **Editing:**
  - ↑/↓ walk the prompt history from the first or last line, and Ctrl+G edits the prompt in `$EDITOR`;
  - Shift+Enter, Alt+Enter, Ctrl+Enter and Ctrl+J insert a newline;
  - the editor keys are emacs-style.
- **Attaching an image:** Ctrl+V pastes one from the clipboard, a pasted image path attaches it, and `/image <path>` does the same.
- **Other keys:**
  - Shift+Tab cycles the thinking level and Alt+M opens the model picker;
  - Ctrl+T shows or hides thinking, and Ctrl+O expands tool output;
  - Ctrl+R opens the reply picker, and Alt+J the jobs panel.

### 4a. The composer in its tile

```
│┃  (transcript)                                                       │
│┃                                                                     │
│┃  ↳ steer: also check that the macOS job is green                    │
│┃  ↳ after turn: then open a PR                       shift+↑ edit    │
│┃──────────────────────────────────────────────────────────────────── │
│┃  run the attach tests again▌ on both platforms and push            │
│┃  [image 1 · screenshot.png 212 KB]                                  │
│┃                                                                     │
│┃  opus-5 · high · ask · gui-design    ctx 41%   $3.10   ⠋ 1m 12s  ■ │
```

```
│┃  ┌ / ────────────────────────────────────────┐                       │
│┃  │ /model       switch model                 │                       │
│┃  │ /mcp         MCP servers                  │                       │
│┃  │ /memory      memory history               │                       │
│┃  └───────────────────────────────────────────┘                       │
│┃──────────────────────────────────────────────────────────────────── │
│┃  /m▌                                                                │
```

- **Ghost text is dimmed after the cursor** (`on both platforms and push` above).
- **An attached image shows as a chip** with its name and size.
- **The `/` menu opens above the composer,** with the TUI's entries and keys.
- **The session bar under the composer** shows the TUI status line's segments: model, thinking level, permission mode, branch, context share, cost, and the turn's spinner and timer.
  - Each segment that has a picker in the TUI (model, thinking, mode) opens it on its key.
  - ■ stops the turn (Esc).

### What being a GUI changes

- **Shift+Enter always inserts a newline.** The TUI needs the kitty keyboard protocol for that.
- **Images come from the native clipboard,** and can also be dropped onto the composer. There is no `wl-paste`/`xclip`, and no OSC 5522.
- **The text field is `gpui-component`'s Input** (decided in gui.md), so IME and selection are native. The editor keys are bound around it.

- **Ctrl+G grows the composer to fill its tile,** for a long prompt, and pressing it again shrinks it back. A second action, unbound by default, edits the prompt in `$VISUAL` (or a configured command) and waits for it to exit, as git does. Meanwhile the composer says "editing in …".
- **The session bar uses the TUI's status-line configuration** (the same segment ids and config keys), drawn as chips, each with its picker.
- **A dropped file that is not an image inserts its path,** quoted where needed, as pasting a path does in the TUI.

## 5. Agent list and jobs (agreed)

These are the TUI's jobs panel, as two views: the agent list is its Sessions tab, and the jobs view is its Jobs tab (`docs/background-jobs.md`). Each has the same rows, actions and keys (contexts `jobs`, `jobs_view`, `jobs_input`, `jobs_confirm`).

### 5a. Agent list

What a Sessions row has in the TUI:

- kind (`tui`, `print`, `subagent`, `acp`, `agent` with the agent's name), pid, cost, run time, directory and first prompt;
- for rows from `[sessions] sources`: the host, who is attached, and whether it waits for you;
- subagents, listed under the session that spawned them.

Narrow, in a bar:

```
┌─────────────────────────────┐
│ ⌕ Find…                     │
│                             │
│ NEEDS YOU                   │
│ ⚑ Deploy pod adapter        │
│   agent · test-cluster · 4m │
│ ⚑ Fix flaky attach test     │
│   agent · devbox · 12m      │
│                             │
│ ~/code/private/mu           │
│ ● Desktop app design   ◂    │
│   tui · here · gui-design   │
│   2h               $3.10    │
│   └ ● ReviewDiff  subagent  │
│ ○ Bump omp to 0.46          │
│   agent · here · maint/omp  │
│   3h               $0.20    │
│ ✓ Release notes             │
│   print · here · 1d  $0.08  │
│   RECENT                    │
│   Attach reconnect fix  2d  │
│   Pod adapter test      4d  │
│                             │
│ ~/code/private/brewery      │
│ ○ Formula audit             │
│   agent · build-pod-3 · 2d  │
└─────────────────────────────┘
```

Wide, as a tile: the same rows as a table, with the TUI's columns:

```
┌──────────────────────────────────────────────────────────────────────────────────────────────┐
│ ⌕ Find…                                                  group: project ▾         12 agents │
├──────────────────────────────────────────────────────────────────────────────────────────────┤
│    STATE  TITLE / FIRST PROMPT       KIND      HOST          BRANCH      ATTACHED   RAN   COST  │
│ NEEDS YOU                                                                                      │
│ ▸  ⚑      Deploy pod adapter         agent     test-cluster  pod-adapter —          4m   $0.41  │
│    ⚑      Fix flaky attach test      agent     devbox        fix/attach  laptop     12m  $1.20  │
│ ~/code/private/mu                                                                              │
│    ●      Desktop app design         tui       here          gui-design  —          2h   $3.10  │
│      └ ●  ReviewDiff                 subagent  here          gui-design  —          1m   $0.22  │
│    ○      Bump omp to 0.46           agent     here          maint/omp   —          3h   $0.20  │
│    RECENT                                                                                      │
│           Attach reconnect fix       ended 2d ago                                     $0.84  │
├──────────────────────────────────────────────────────────────────────────────────────────────┤
│ Enter open · n message · s stop · S terminate · t take · / find                              │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Rows are grouped by project** (the working directory's repository), with a pinned "Needs you" group. A subagent stays under its parent.
- **The state icon** comes from what the row already says (waiting, a turn running, idle, finished, failed):
  - `⚑` needs you, `●` running, `○` idle, `✓` finished and unseen, `✗` failed.
  - "Unseen" is new state, kept by the app: a turn that ended while no tile showed the session.
- **The branch** is a new field. Each agent publishes its working directory's branch in its owner record, and a session source's rows may carry it. The TUI's Sessions tab shows it too.
- **Recent sessions.** Under each project, a muted "Recent" group lists sessions that ended in the last few days. Opening one resumes it, as `/resume` does, and `/resume` keeps its picker for anything older.
- **The TUI's keys:** `n` sends a message, `s` stops the turn, `S` terminates after `y`, `t` takes a `-p` run over. Search with `/` is new.
- **Enter opens the session in the focused tile, as an observer,** local or remote alike. Typing a prompt asks to take control, as decided for observers. In the TUI, Enter on a local row only peeks at its transcript. Once observers exist, the TUI's Enter opens a session the same way.
- **A jump from a request** (section 3d) opens the session in a floating panel instead.

### 5b. Jobs view

- **The TUI's Jobs tab:** one row per subagent or `async` bash job, with state, name, agent, tool count, elapsed time, cost and last activity. The selected job's details sit below.
- **Its keys:** `x` cancels a job, `X` cancels all after `y`, `d`/`a` show or apply a finished job's result, `o` forwards a job's ports, Enter opens a job.
- **What the app changes:** Enter opens the job's transcript or output in a tile, as the TUI's `jobs_view` with its keys. The job rows above the composer (`Alt+↓` marks one, `k`/`x` cancels it) stay as in the TUI.
