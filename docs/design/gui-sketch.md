# Desktop app: sketch plan

Status: plan only, not built. It turns the agreed wireframes ([gui-wireframes.md](gui-wireframes.md)) into a throwaway GPUI app with fake data. Its purpose is to settle what wireframes cannot: type, density, colour and theme roles, and how the keys feel.

## Goals and non-goals

- **Goals:**
  - every agreed screen in real type and colour, on Linux and macOS;
  - the window keys from section 6 working, so the tiling and the leader key can be tried;
  - a list of theme roles and sizes for the real app.
- **Non-goals:**
  - no wire, no agent, no shared crate, no persistence;
  - no real text input: the composer shows fixed states;
  - no code reused later. The sketch is deleted once its findings are written down.

## Decided

- **GPUI from `gpui-unofficial`,** pinned exactly (`=1.30.2`, Zed's release of 2026-11-25), together with `gpui-platform-gpui-unofficial` at the same version.
  - No `gpui-component`: it tracks Zed's git GPUI, not this republish, and the sketch needs no real input.
- **It lives in `sketch/gui/` in this branch, with its own Cargo workspace and lockfile.** mu's `Cargo.lock`, `deny.toml` and `scripts/ci-checks.sh` are untouched, and the root `Cargo.toml` lists `sketch/gui` under `exclude`.
- **It runs on both platforms:** Linux first, then the Mac.

## Layout of the crate

| File | Holds |
|---|---|
| `main.rs` | App start, key bindings, window |
| `theme.rs` | A mu palette resolved to the TUI's roles, plus the GUI-only roles and type sizes |
| `data.rs` | Fake sessions, agents, jobs and requests, built from the screens list below |
| `shell.rs` | Tabs, tile tree, bars, status bar, focus, zoom, the overlays (palette, floating panel) |
| `views/transcript.rs` | Every block type, at rest and streaming |
| `views/request.rs` | The dock: approval (both steps), questions with a preview, document review |
| `views/composer.rs` | Queue rows, the composer's states, the `/` menu, the session bar |
| `views/agents.rs` | The agent list, narrow (bar) and wide (tile) |
| `views/jobs.rs` | The jobs view |
| `views/palette.rs` | The agent palette and the full palette |
| `assets/` | Four of mu's palettes |

- **Views get their data passed in.** Each view is a function from the theme, the data, its session and whether it has focus to an element, so the views can be written independently of the shell and of each other.

## Theme

- **The TUI's roles, computed the same way:** the same palette keys and mixes as `crates/mu-tui/src/theme.rs` (`DEFS`). The roles are strong, muted, faint, thinking, line, surface, selected, accent, ok, warn, err, tool and the syntax roles.
- **Everything beyond the TUI's roles is undecided:** which surfaces, colours, sizes, spacing, radii and type the GUI has beyond the TUI's roles and the owner's decisions is not settled here. See [gui-decisions.md](gui-decisions.md); this file does not set any of it.
- **Fonts:** the system UI font and a system monospace, nothing bundled.
- **Four palettes,** two dark and two light (catppuccin, tokyo-night, catppuccin-latte, flexoki-light). A key cycles them, to check that every role holds up in each.
- **Sizes are constants in one place,** so a review can adjust them in a single edit.

## Screens and states

Each wireframe section becomes states the fake data puts on screen. A tab per scenario makes them reachable by key.

| Tab | Shows | Wireframe |
|---|---|---|
| Cockpit | Agent list in the left bar; two sessions split; a review tile pinned to one; the status bar | 1a, 5a |
| Tiles only | No bars, two sessions | 1b |
| Turn at rest | Prompt with an image, a folded thinking line, read/grep rows, prose with headings, lists and inline code, a diff in full, a passing and a failing bash, a subagent group | 2a |
| Turn streaming | Thinking streaming, a call with arguments streaming, a running bash with its tail and timer, a retry row, an error row, "↓ new output" | 2b |
| Approval | The dock at step 1 (widths and model choices) and at step 2 (scope), with an answered one-liner above | 3a |
| Questions | A batch of three, with a preview beside the highlighted option, and the final review step | 3b |
| Plan review | The dock grown to hold a markdown document, with the actions and the model slider | 3c |
| Composer | Queue rows, ghost text, an image chip, the `/` menu open, the session bar while running | 4 |
| Agents wide | The agent list as a table: groups, needs-you, subagents, recent | 5a |
| Jobs | The jobs view with running and finished jobs and the details of one | 5b |

These overlays open on top of any tab:

- the agent palette;
- the full palette, with key hints;
- the floating panel for the next request (3d).

## Keys in the sketch

- **What works,** with the section 6 defaults on both platforms:
  - focus by direction;
  - split (spiral, right, down), close, zoom, swap, resize;
  - bars and tabs;
  - both palettes;
  - the jump to the next request, and Esc to close overlays.
  - Leader sequences use GPUI's multi-keystroke bindings (`ctrl-x h`).
- **Also bound:** cycling the palette, and toggling between the two type sizes under review.
- **Not wired up:** the TUI's own contexts (composer, reply picker, dialogs). Their keys appear only as hints in the screens.

## Building and running

- **Linux:** `offload run --arch amd64 --fetch sketch/gui/target/release/mu-gui-sketch -- cargo build --release --manifest-path sketch/gui/Cargo.toml`, then run the fetched binary here. The offload image must have the GPUI Linux libraries (wayland, xkbcommon, vulkan and fontconfig headers); if it lacks them, that is the first finding, and adding them is a change to the image.
- **macOS:** build on the self-hosted Mac with plain `cargo build`, since offload runs no macOS nodes, and run it there.

## Reviewing it

1. **Screenshots:** one per tab per palette, on each platform. You take them, or the sketch saves them if GPUI's screen capture proves easy.
2. **For each tab, look at:**
   - readability of prose next to monospace;
   - density (lines per screen against the TUI);
   - whether each role is distinct in every palette (added/removed, muted/faint, accent/focus);
   - whether the focused tile is obvious;
   - whether the dock crowds the transcript.
3. **Try the keys** for a while: are the leader and the platform tier right, and what collides with the window manager or the OS.
4. **Write the findings into the design docs:**
   - the final role list and type scale, in `gui.md`;
   - key changes, in the wireframes, section 6;
   - anything that changes a wireframe.

   Then delete `sketch/`.

## How the work splits

1. **First, one person:** the crate skeleton, `theme.rs`, `data.rs` (types and fake content) and `shell.rs`, with views as placeholders. Done once it builds on offload and the tiling keys work.
2. **Then four slices in parallel,** each in its own files against the data types from step 1. They write without building, and one integration build fixes them up:
   - transcript;
   - request dock and composer;
   - agent list and jobs;
   - palettes and the floating panel.
3. **Last:** run it on Linux, then the Mac, and review it with you.

## Open

- **Whether the offload image already carries GPUI's Linux build dependencies.** This is unknown until the first build.
- **Everything visual.** Colours, sizes, spacing, radii and which face is used where are decided by the owner, not by this plan; see [gui-decisions.md](gui-decisions.md).
