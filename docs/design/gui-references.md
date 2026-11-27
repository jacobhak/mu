# Desktop app: reference UIs

A survey of agent UIs, done 2026-11-27 as input for the wireframes in [gui.md](gui.md). Most detail comes from docs, changelogs and issues. The Zed and Delta entries are from the full pages and their screenshots; for the other tools only search results could be read, so treat their finer details as unverified.

## The references

| Tool | What it is | Native? |
|---|---|---|
| **Zed** (Threads Sidebar + Agent Panel, Apr 2026) | Editor with parallel agent threads | Yes, GPUI |
| **Delta** (Zed team, Aug 2026) | An app built around the conversation, with review in the same window | Yes, GPUI-style Rust; web build via WASM/WebGL |
| **Warp** | Terminal with output in blocks, vertical tabs, agent mode | Yes, own Rust UI on wgpu |
| **Codex app** (OpenAI, Feb 2026) | Desktop app for Codex threads, with a review pane | macOS/Windows; stack unknown |
| **Cursor Agents Window** (3.x) | Window for agents across repos, with tiled panes | Electron (VS Code fork) |
| **Conductor, Sculptor, Vibe Kanban, Nimbalyst** | Apps that run parallel agents in worktrees or containers | Mostly web technology |
| **Paseo** | Client and daemon for many agent CLIs; desktop and mobile | Unknown; already an ACP client of mu |
| **Claude Code, Codex CLI, opencode** | Terminal UIs; useful for transcript and approval details | Terminal |

## Findings by topic

### 1. The window

- **Zed and Delta:** a sidebar of threads grouped by project, then the conversation, then (Delta) a review pane on the right.
  - Each thread row has a status icon, the title, and a meta line: worktree/branch, `+25 −15`, age.
  - The bottom bar holds the model, the thinking level, the mode, stop, and (Delta) the branch with its `+N −M`.
- **Zed:** a "Panel Layout > Agentic / Classic" preset, and the sidebar's side is a setting. Focus moves between panes by direction (`agent::FocusLeft`, and Up/Right/Down).
- **Cursor:** tiled panes, one agent per pane. Subagents and side chats open as tabs on their parent agent, so the parent's view stays put.
- **Warp:** vertical tabs with rich detail per session, pitched as the way to run several agents side by side.
- **Anti-pattern:** a tab UI with no sidebar and a "Toggle Sidebar" that does nothing (opencode desktop, issue 58101).

### 2. Transcript

- **Zed:**
  - Tool calls are one line each: an icon, a verb and its target, muted, such as `Read  flags.rs crates/feature_flags/src/`, or `— L251-600` for a range.
  - "Thinking…" is one line.
  - Prose is proportional, the user's prompt is a monospace box, and the transcript is a narrow centred column.
- **Delta's stance:** "diffs open in full, transcripts stay whole". Most tools hide things instead: they collapse diffs, truncate transcripts and summarise.
- **Warp:** status shown by the colour of a block's edge, such as a red background and red edge for a non-zero exit.
- **Claude Code and Codex CLI:** a compact transcript by default, and a separate detail view on a key (Ctrl+O, Ctrl+T) with timestamps and the model.
- **Codex CLI:** finished entries are never changed, and one live entry streams; this suits streaming.
- **Anti-patterns:**
  - Diffs that started collapsed after an update, with no setting to restore them (Claude Code issue 110859).
  - An "expand" that does not show everything (Claude Code issue 26954).
  - Added and removed lines in the same colour (Codex issue 58186).

### 3. Requests that need you

- **Warp:** a permission card shows the agent's reasoning, a short explanation and the exact command, with approval for the rest of the session.
- **Codex:**
  - It dropped a bare "No" option because it confused users.
  - Users asked for verbs on the buttons instead of Yes/No (not done).
  - "Don't ask again" remembered the exact command when users expected the program (issue 38328).
- **Cursor:** an agent that finishes or turns unread while you look at another shows in the macOS menu bar and Dock menu, unread first, with a count.
- **Anti-pattern:** an approval prompt that stops you scrolling the transcript or opening its detail (Codex issue 47865).

### 4. Composer

- **Steer and queue on separate keys:** in opencode, Enter steers and Ctrl+X Enter queues; in Codex, Tab queues.
- **The input stays at the bottom** while the transcript streams (Claude Code fullscreen, Zed, Delta).
- **Zed:** the placeholder teaches the syntax ("@ to include context, / for commands"), and a key expands the composer.
- **Codex:** the composer's footer changes its hint with the state ("tab to queue" while a task runs).

### 5. The agent list

- **Zed:** rows are grouped by project and show status, title, the agent's name, worktree/branch, `+N −M` and age. Ctrl+Tab cycles recent threads; finished threads are archived to a history view.
- **Cursor:** agents you haven't looked at are marked unread and listed first.
- **Codex:** the pull request's state shows in the sidebar.
- **Nimbalyst:** "workstreams" group sessions about the same files or topic.
- **Missing everywhere:** cost, host, and a "waiting on you" badge distinct from "finished". mu needs all three, because its agents run on several machines.

### 6. Palette and keyboard

- **Zed:** every action has a name and can be bound; keymaps are scoped by context; the sidebar has its own focus key; the thread list can be searched.
- **Delta:** "the conversation is a document, and your cursor works everywhere in it". You move through the thread with editor motions, put the cursor on any part (a diff line, a plan step, a thinking block) and type a comment that attaches there.
- **Codex app:** a command menu (Cmd+K / Cmd+Shift+P), and a shortcuts editor where pressing a key finds its binding.
- **Paseo:** a Cmd+K command centre; users asked for a fast jump between agents across hosts (issue 572).
- **Anti-pattern:** shortcuts that can only be discovered in a separate dialog.

### 7. Type, density, colour

- **Zed and Delta:**
  - prose in a proportional sans at a generous size;
  - code and diffs in monospace with line numbers and whole-line added/removed backgrounds, plus a stronger highlight for the changed part of a line (Delta);
  - muted grey for tool lines and paths;
  - colour kept for `+N −M`, status and stop.
- **Zed:** the user's prompt is set in monospace inside a bordered box, which sets it apart from the model's prose.

## What this suggests for mu

1. **Default to a sidebar with a narrow centred transcript and a review pane on the right**, as Zed's agentic layout and Delta do. The tabs/sidebar preference already decided stays.
2. **Two levels of detail:**
   - Tool calls default to one muted line (icon, verb, target).
   - Diffs and prose are never collapsed by default.
   - A key expands one block; a detail view shows everything.
3. **Delta's cursor in the conversation is the strongest keyboard-first idea here, and mu already has the start of it.** The reply picker quotes parts of a reply into an answer (`docs/reply-picker.md`). A cursor that moves through blocks and can reply to, copy, expand or open any of them generalises that.
4. **A request that needs you is a card in the transcript, never a modal.** It is also mirrored in a strip across the window and in the agent's row, so a background agent's request can be answered from anywhere.
   - The card shows the reasoning and the exact command.
   - Its buttons are verbs, and its scopes are named precisely ("allow `cargo` in this session").
5. **The agent's row needs states the others lack:** waiting on you (distinct from finished), host, cost and idle time.
6. **A command palette** for every action, a direct key for the sidebar, Ctrl+Tab through recent sessions, and a search across hosts.
