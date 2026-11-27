# Desktop app: design

Status: design only, decided 2026-11-27. Nothing here is built yet.

A native desktop app for mu, with no web engine or runtime. It is a cockpit for many agents (local and remote) and a workbench for working in one session, and both jobs count equally. The reasons for it are better text rendering and readability, and better notifications. It is keyboard-first: everything in it can be reached and done without the mouse.

## Scope and relation to the TUI

- **Both UIs are first-class.** A feature lands in the TUI and the app. The TUI stays what you use over ssh and on machines without a display.
- **Parity comes from shared code, extracted incrementally.** Each TUI subsystem moves into a shared crate when the app needs it; until it has moved, the app lacks it. See [Shared crate](#shared-crate).

## Framework, packaging, CI

- **GPUI**, with `gpui-component` for widgets. Until Zed publishes the standalone crate (announced 2026-11-23), the app pins a git revision (or `gpui-unofficial`, which republishes each Zed tag).
- **A separate binary**, `mu-gui` (`crates/mu-gui`). On macOS it ships as a `.app` bundle; on Linux, as a binary plus a `.desktop` file. `mu` stays free of the GUI stack.
- **No Developer ID yet.** macOS builds are ad-hoc signed for personal use; signing and notarization in release builds come later.
- **CI:** `mu-gui` is a workspace member, left out of the default `scripts/ci-checks.sh` gate, with its own job on Linux and macOS. A GUI break does not block core work. Linux build nodes, including offload nodes, need the system libraries GPUI links against (wayland, xkbcommon, vulkan).

## Process model

- **The app stays running.** Cmd+Q and closing the window close the window only; the app stays in the menu bar or tray, sessions continue and notifications still fire.
- **Each local session is an ordinary `mu agent`.** The app starts it with `mu agent --detach`, remembers that it started it, and attaches over the wire: the same code path as a remote session.
  - Such an agent is indistinguishable from any other: `mu agent list` shows it, the TUI can attach to it, and `mu agent stop` stops it.
  - A crash in the renderer does not take agents down, and a relaunched app attaches to them again.
- **The tray menu's Quit** (and logout or an app update) ends the app itself. It stops the agents the app started, except those another UI controls or attends. When an agent it would stop is busy (a turn running, or a request waiting on you), it asks first.
- **Idle timeout.** A new agent setting (working name `[agent] idle_stop`, off by default): an agent with no turn running, no pending request and no connection for that long saves and exits. It resumes like any session. This also applies to remote agents.
- **One app and agent version.** The app and its agents must be the same mu release, as with `mu --attach`.

## Several connections per agent

Today an agent serves one client (`src/run/serve.rs`: `client: Option<Client>`). A second hello gets `Busy` and is closed, unless it sends `take`, which detaches the current client. The new model needs a wire bump.

- **The hello says the connection's role:** `observe` or `control`.
  - `observe` always joins.
  - `control` while another connection controls gets `Busy`, as today, or takes control with `take`.
- **An observer gets everything a controller does:** the same catch-up and the same live events.
- **Taking control.** An observer that starts a prompt or a steer is asked locally, "take control from <host>?" (as the TUI asks today). Control then moves at once, and the old controller becomes an observer instead of being disconnected.
- **Answers.** Any connection may answer an approval or an ask. The first answer wins, and the dialog closes on the other connections (`WireEvent::Pending` already does this for a request answered elsewhere).
- **Dropped replies.** `WireCommand::Dropped` acts only when it comes from the controller. An observer that closes its dialog does not deny the request; with no controller, the request waits.
- **Attending.** The hello also says whether the connection *attends*, meaning it notifies its user itself.
  - The agent sets `waiting` and runs `[agent] notify_command` only when no connection attends (today: when no client is attached).
  - The app attends. A passive dashboard would not.
- **Per-client state.** `Pushed` values (sign-in addresses, login notices) and host-call replies go to the connection they belong to. A `pty: true` `bash` call goes to the controller only.
- **Unlocking.** Unlocking a sealed agent stays agent-wide: any connection may do it. The app needs its own passphrase dialog for keys that have one, because today's attached client cannot prompt for one (`src/run/attach.rs`, `NoticeTerminal`).
- **The owner record** (`<profile home>/run/<id>.json`) lists every connection, not one `client`. `mu agent list` and the Sessions tab show them.
- **Every process that hosts a session listens.** The TUI exposes each session it hosts in-process on a private socket, with the same rules as `mu agent` (mode 0600 in a 0700 directory). Any UI can then observe any local session. Today only `mu agent` listens.

## Remote agents

- **Existing contracts first.** The app finds remote agents through `[sessions] sources` (contract D) and opens each with its `attach` command (contract C): one attach process per agent it watches.
- **Multiplexing later.** A contract with one connection per host for many agents' streams waits until the number of processes is a real problem.

## Shared crate

Working name `mu-front`. It depends on `mu-base` only, never on `mu-tools` or `mu-provider`, so building `mu-gui` does not compile the agent. The app only ever attaches over the wire, so it never needs `LocalHost`, which stays in mu-tui.

What moves there for the first milestone:

- **The client side of the wire.**
  - The attach driver, from `src/run/attach.rs`.
  - `RemoteHost` and the `Host` trait, from `crates/mu-tui/src/host/`.
  - `host/completion.rs` and `host/models.rs` import UI types; that coupling has to be cut first.
  - `ClientAdapter` needs a Tokio runtime. The app runs one on background threads and bridges its channels to GPUI's executor.
- **The event reducer** (`AgentView::on_event` and `Effect`, `crates/mu-tui/src/app/agent_view.rs`), without its terminal-size argument and its terminal effects (bell, clear screen).
- **The dialog queue and answer models:** `pickers.rs`, plus the ask and approval state in `widgets/overlay.rs`.
- **Semantic transcript blocks.** Today the transcript stores lines already wrapped to the terminal width (`chat/transcript.rs`), and block renderers are functions of `(data, width)` (`chat/blocks.rs`).
  - The new model is width-free: a user prompt, prose as parsed markdown, a tool call, a tool result, a diff, thinking.
  - The TUI lays the blocks out into lines when it commits them; the app lays them out natively and reflows on resize.
  - This TUI refactor comes before the app shows a transcript.
- **Styles as theme roles.** Spans carry a semantic role (heading, code, added, removed, link, muted, …) plus attributes, instead of crossterm's `ContentStyle`. Each UI resolves roles through the shared palette, so the app can, for example, set prose in a proportional font.
- **The keymap,** with a neutral key type instead of crossterm's `KeyCode`/`KeyModifiers`.
- **The composer's actions:** submit, history, steer, completion.

Process-wide globals (the theme, the keymap, host facts) are passed in explicitly, because the app has several windows and sessions.

## Keyboard

- **One action list, the same default keys.** Both UIs use the same list of actions with the same default bindings, and `[keys.<context>]` overrides apply to both.
- **GPUI dispatches keys in the app.** At start-up the app turns every action (context, name, keys) and the user's overrides into GPUI actions and key bindings. GPUI's key contexts mirror the TUI's (app, chat, editor, jobs, reply, review, select).
- **The app adds bindings that only make sense there,** such as Cmd+1..9 to switch sessions. `docs/keys.md` lists those too.

## Layout and rendering

- **The window is tabs of tiles, plus two bars.** Each tab is tiled like a tiling window manager, and any view (session, agent list, review, jobs, plan, files) can fill a tile. The left and right bars are docked outside the tiling and hold whatever views you put there. Named layouts save the arrangement. See [the window shell wireframes](gui-wireframes.md#1-window-shell-agreed).
- **Proportional prose.** Model prose uses a proportional font, with real headings and lists. Code, tool output, diffs and paths use a monospace font. Fonts and sizes are configurable.
- **Theme.** The app uses the TUI's palette files (`crates/mu-tui/src/palettes/`), extended with roles only a GUI needs (surfaces, borders, focus ring, selection). Each new role gets a default derived from the palette when a palette does not set it, so one theme choice covers both UIs.
- **Composer.** Text editing and IME use `gpui-component`'s Input. mu's actions are bound around it through the shared action list.

## Interactive `bash`

- **v1 declines it.** A `bash` call with `pty: true` asks the client for a terminal (`WireEvent::PtyOpen`). The first version answers `WireCommand::Dropped`, and the tool falls back to running non-interactively.
- **Later: an embedded terminal** (alacritty_terminal in GPUI, as in Zed), shown as a block in the transcript that can take keyboard focus.

## Notifications

- **One config section for both UIs.** A `[notify]` section has one switch per event. `[bell]` stays as an alias, so existing settings keep working. The TUI rings the terminal bell; the app posts an OS notification.

  | Event | Default |
  |---|---|
  | A request needs you (approval, ask, plan review, web setup) | on |
  | A turn you started finished | on |
  | A turn failed | off |
  | A background job finished | off |
  | A sealed agent needs unlocking | off |

- **Same rules as the TUI's bell today.** Nothing notifies while a client is catching up, and a turn started by `mu notify` or by a finished job does not notify when it ends.
- **When the app notifies:**
  - The app notifies for every agent it attends.
  - It posts no OS notification while it is focused and the session is visible in any pane.
  - In the app, other sessions get a badge on their sidebar entry or tab.
  - Clicking a notification focuses that session and opens the waiting dialog.
- **Linux:** freedesktop notifications over D-Bus (`notify-rust`), with action buttons where the notification server supports them.
- **macOS:** plain notifications until a signed bundle exists. Approve and reply from the notification need UNUserNotificationCenter, which only works in a `.app` with a bundle identifier.

## First milestone

One session, end to end:

- start a local `mu agent`, or attach to a running one;
- a streaming transcript built from semantic blocks;
- the composer;
- approvals and asks;
- keyboard navigation between them.

What it needs first:

- the shared crate, with the client side of the wire, the reducer, dialogs and keymap;
- semantic blocks and style roles in the TUI;
- the neutral key type.

It has no agent list, observers or remote agents yet.

## Order after that (sketch)

1. Observers, roles and attending in the agent (wire bump). A socket for sessions the TUI hosts, and connections listed in the owner record.
2. The cockpit:
   - the agent list (local records and session sources);
   - badges, notifications and the `[notify]` section;
   - Quit from the tray stops the app's agents;
   - the idle timeout.
3. Sidebar/tabs and splits; attaching to remote agents through session sources.
4. Embedded terminal; actionable notifications with a signed bundle; the multiplexing contract.

## Open questions

- The exact hello fields for role and attending, and how `Busy`/`take` read in them.
- How the TUI's take-over question changes once a connection can stay as an observer.
- The semantic block set, and how the TUI keeps its streaming reveal (`chat/transcript.rs`) on top of it.
- Accessibility: GPUI's screen-reader support (AccessKit) needs deliberate work.
