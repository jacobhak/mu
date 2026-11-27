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
