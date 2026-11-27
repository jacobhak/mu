# Desktop app: design

Status: design only, decided 2026-11-27. Nothing here is built yet.

A native desktop app for mu, with no web engine or runtime. It is a cockpit for many agents (local and remote) and a workbench for working in one session, and both jobs count equally. The reasons for it are better text rendering and readability, and better notifications. It is keyboard-first: everything in it can be reached and done without the mouse.

## Decisions

### Scope and relation to the TUI

- **Both UIs are first-class.** A feature lands in the TUI and the app. The TUI stays what you use over ssh and on machines without a display.
- **Parity comes from shared code, extracted incrementally.** A new crate (working name `mu-front`) holds what does not depend on a renderer: view state, commands, dialogs, keymap actions and host calls. The app starts on the wire plus a thin shared layer (the action list and the transcript model). Each TUI subsystem moves into the crate when the app needs it. Until a subsystem has moved, the app lacks it.

### Framework and packaging

- **GPUI**, with `gpui-component` for widgets. Until Zed publishes the standalone crate (announced 2026-11-23), the app pins a git revision (or `gpui-unofficial`, which republishes each Zed tag).
- **A separate binary**, `mu-gui` (`crates/mu-gui`). On macOS it ships as a `.app` bundle; on Linux, as a binary plus a `.desktop` file. `mu` stays free of the GUI stack.
- **No Developer ID yet.** macOS builds are ad-hoc signed for personal use; signing and notarization in release builds come later.
- **CI:** `mu-gui` is a workspace member, left out of the default `scripts/ci-checks.sh` gate, with its own job on Linux and macOS. A GUI break does not block core work. Linux build nodes, including offload nodes, need the system libraries GPUI links against (wayland, xkbcommon, vulkan).

### Process model

- **The app stays running.** Closing the window leaves it in the menu bar or tray; sessions continue and notifications still fire. Quit asks first when agents are busy.
- **Each local session runs as a `mu agent` child** that the app starts, supervises and attaches to over the wire: the same code path as a remote session. A crash in the renderer does not take agents down, and a relaunched app attaches to them again.
- **One app and agent version.** The app and its agents must be the same mu release, as with `mu --attach`.

### Several clients per agent

- **Observers.** An agent accepts many observer connections and at most one controller. Today a second client gets `Busy` or takes the session over. This needs a wire bump, and `Busy` and `take` keep their meaning for a client that asks for control.
- **Any connection may answer an approval or an ask.** The first answer wins, and the dialog closes on the other connections (`WireEvent::Pending` already does this for a request answered elsewhere). A prompt or steer comes only from the controller. An observer that starts a prompt asks to take control, and the old controller becomes an observer instead of being disconnected.
- **Every process that hosts a session listens.** The TUI exposes each session it hosts in-process on a private socket, with the same rules as `mu agent` (mode 0600 in a 0700 directory). Any UI can then observe any local session.

### Remote agents

- **Existing contracts first.** The app finds remote agents through `[sessions] sources` (contract D) and opens each with its `attach` command (contract C): one attach process per agent it watches.
- **Multiplexing later.** A contract with one connection per host for many agents' streams waits until the number of processes is a real problem.

### Keyboard

- **One action list, the same default keys.** Both UIs share one list of actions with the same default bindings. The app adds bindings that only make sense there, such as Cmd+1..9 to switch sessions. `docs/keys.md` lists the app's extra bindings as well.
- **GPUI key contexts mirror the TUI's keymap contexts** (app, chat, editor, jobs, reply, review, select).

### Layout and rendering

- **Sidebar or tabs.** A setting chooses between a sidebar of agents and a tab bar; a tab can be split into panes showing several sessions side by side.
- **Proportional prose.** Model prose uses a proportional font, with real headings and lists. Code, tool output, diffs and paths use a monospace font. The transcript reflows on resize, and fonts and sizes are configurable.
- **Theme.** The app uses the TUI's palette files (`crates/mu-tui/src/palettes/`), extended with roles only a GUI needs (surfaces, borders, focus ring, selection). Each new role gets a default derived from the palette when a palette does not set it, so one theme choice covers both UIs.
- **Composer.** Text editing and IME use `gpui-component`'s Input. mu's actions (submit, history, steer, `/` and `@` completion) are bound around it through the shared action list.

### Interactive `bash`

- **v1 declines it.** A `bash` call with `pty: true` asks the client for a terminal (`WireEvent::PtyOpen`). The first version answers `WireCommand::Dropped`, and the tool falls back to running non-interactively.
- **Later: an embedded terminal** (alacritty_terminal in GPUI, as in Zed), shown as a block in the transcript that can take keyboard focus.

### Notifications

- **Linux:** freedesktop notifications over D-Bus (`notify-rust`), with action buttons where the notification server supports them.
- **macOS:** plain notifications that focus the app, until a signed bundle exists. Approve and reply from the notification need UNUserNotificationCenter, which only works in a `.app` with a bundle identifier.

## First milestone

The first version handles one session, end to end:

- start a local `mu agent` child, or attach to a running agent;
- a streaming transcript;
- the composer;
- approvals and asks;
- keyboard navigation between them.

It has no agent list, observers or remote agents yet. Its purpose is to prove GPUI, the wire client and the first extraction into the shared crate.

## Order after that (sketch)

1. Observers in the agent (wire bump), and a socket for sessions hosted in the TUI.
2. The cockpit: the agent list (local records and session sources), badges for agents waiting on you, notifications.
3. Sidebar/tabs and splits; attaching to remote agents through session sources.
4. Embedded terminal; actionable notifications with a signed bundle; the multiplexing contract.

## Open questions

- Which TUI subsystems move into the shared crate first, where its boundary lies, and how `crates/mu-tui/src/host/` (local vs remote host state) splits.
- The observer protocol:
  - the hello field;
  - how control passes from one connection to another;
  - what an observer receives in the catch-up;
  - compatibility with `Busy` and `take`.
- Whether the agents the app supervises outlive Quit, so they keep running and are attached again at the next launch.
- Which events notify (an approval waiting, a turn done, an error, a job finished), how often, and what clicking a notification focuses.
- Accessibility: GPUI's screen-reader support (AccessKit) needs deliberate work.
