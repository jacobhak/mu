# Theme redesign

Design reference for mu's palette-derived theme (implemented in `src/tui/theme.rs`).

```sh
python3 design/theme/mock.py --side                 # mu as you see it today vs the proposal
python3 design/theme/mock.py --side proposed proposed-fill proposed-ansi
python3 design/theme/mock.py --side current proposed --palette gruvbox
python3 design/theme/mock.py --swatch               # derived roles for a palette
python3 design/theme/mock.py --side --paint         # paint the palette background (terminal not themed with it)
python3 design/theme/mock.py --html /tmp/mock.html  # every variant, browser-viewable
python3 design/theme/osc_probe.py                   # what `source = "terminal"` would see in this terminal

# Real renders, omp vs mu on the same transcript (private tmux server; resume costs nothing)
design/theme/shot.sh resume /tmp/shots                       # omp, mu (tokyo-night), mu-term (Ghostty TokyoNight)
design/theme/shot.sh resume /tmp/shots "" mu:catppuccin-latte
design/theme/shot.sh live /tmp/shots                         # re-record the fixture (one small model run)
python3 design/theme/ansi2png.py /tmp/shots/mu.ansi          # colours as text, for reading without a browser
```

`fixture.jsonl` is a recorded session (read, failing bash, edit, write, bash, markdown summary; thinking added by
hand). `ghostty-tokyo.toml` is what `source = "terminal"` reads from Ghostty's TokyoNight under tmux 3.6, which
forwards OSC 4 palette queries to the client.

Truecolor variants need a truecolor path: under tmux the client must list `RGB` in
`tmux display -p '#{client_termfeatures}'` (detach and reattach after changing `terminal-features`).

## Polish pass (tmux 3.6)

With tmux 3.6 forwarding OSC 4, omp's Tokyo Night rendered in true colour looked better than mu. What omp did
better, and what mu does now:

| omp | mu before | mu now |
|---|---|---|
| cards and prompt on a recessed panel (`#16161e`) | outline only; prompt on a lighter band | `cards = "filled"` is the default, on `dark_background` (the palette's own mantle; derived for `terminal`); prompt band on the same surface; failed cards on a faint red surface |
| syntax-highlighted diffs with a line-number gutter | grey diff text, numbers fused to the sign (`-8\|`) | gutter, sign column, both diff sides highlighted as whole files, tinted rows, the changed words of a modified line picked out; a new file reads as a numbered listing |
| blank line between blocks | cards stacked tight | a blank row after every tool block |
| readable dim text | `terminal` used slot 8 for faint text (`#414868`, 1.9:1 in Ghostty TokyoNight) | `dark_foreground` must reach 2.4:1, else it is lifted to fg→bg 55% |
| — | resumed sessions lost edit diffs and thinking | diffs persist in `details.diff` (where omp keeps them); thinking replays |

Kept from the redesign: one accent, hues only for meaning, no hand-picked hexes.

## Why the current themes look garish

1. **Quantisation.** Under tmux without the `RGB` feature and with `COLORTERM` unset, `theme.rs` snaps every hex to
   xterm-256. Tokyo Night's blue-black surfaces become neutral greys (`#16161e` → `#1c1c1c`), the three tool-card
   tints collapse into the same `#262626`, and accents oversaturate (`#9ece6a` → `#afd75f`).
2. **Off-palette literals** in omp's theme: cost in ANSI 205 (hot pink), git counts in 70/178/39, a brown error
   background, and a purple accent where Tokyo Night's own UI accent is blue.
3. **Hue overload.** The status line shows six hues at rest; purple means accent, heading, tool title, keyword,
   model and session name at once, so it means nothing.
4. **Heavy chrome.** Every card has a coloured border and a filled background; Tokyo Night itself is flat.

## Decisions

| question | decision |
|---|---|
| tool cards | keep boxes, make them quiet: neutral outline, no fill, no coloured titles |
| palette source | follow the terminal by default; a colors.toml can override or be selected |
| omp compatibility | dropped. omp theme JSON, `theme_data.rs` and omp's `theme.dark` are not read any more |

## Principles

1. Body text is the terminal's default foreground. Hierarchy comes from a text ramp (strong, text, muted, faint),
   not from hue.
2. One accent, for focus and activity only: the prompt glyph, spinners, the running card's outline, links, the plan
   badge, the selected row marker.
3. Other hues carry meaning only: green ok/added, red error/removed, yellow attention. Syntax highlighting is the
   one place the whole palette appears.
4. Every non-palette colour is a mix of the palette with itself (background toward foreground or a hue). No
   hand-picked hexes, so nothing leaves the theme's colour family.
5. Never approximate. Without truecolor, emit palette slots (the terminal shows its exact colours) and drop tints.

## Palette input: omarchy's colors.toml

The input schema is omarchy's `colors.toml`, verbatim: `background foreground accent muted selection`,
`dark_/darker_/lighter_background`, `dark_/light_/bright_foreground`, the six hues and their `bright_` forms,
`orange brown`, `mode`. All 23 omarchy palettes are vendored in `palettes/` (MIT, omarchy `2fca7fe`).

The terminal maps onto the same schema, because it is exactly what omarchy pushes to the terminal
(`omarchy-theme-osc`): OSC 10/11 are foreground/background, OSC 17 is selection, and OSC 4 slots 0-15 are
`background red green yellow blue magenta cyan foreground muted bright_red … bright_cyan bright_foreground`.

### Resolution

```toml
# ~/.mu/config.toml
[theme]
source = "terminal"     # "terminal" (default) | a bundled palette name | a path to a colors.toml | "omarchy"
accent = "blue"         # palette key or hex; needed for "terminal", which has no accent slot
cards  = "filled"       # "filled" (default: recessed dark_background surface) | "outline" (border only)
[theme.roles]           # optional per-role overrides, e.g.
# line = "#3b3e52"
```

- `terminal`: query OSC 10, 11, 17 and 4;0-15 at startup with a DA1 sentinel (bounded wait, about 100 ms). Hues are
  always emitted as slot references, so they are exact even when the query fails. Answered values feed the mixes.
  Unanswered hue RGBs come from a reference palette and are used only for the low-percentage tints, where an error
  is invisible. With no answers at all, render as the ANSI tier.
- `omarchy`: `~/.local/state/omarchy/current/theme/colors.toml` when it exists (local omarchy machines).
- A name or path: that colors.toml. Missing keys use omarchy's own fallbacks (`omarchy-theme-color`).
- Light or dark comes from background luminance; the mixes need no special case because they mix toward the
  foreground.

Measured here on tmux 3.4 with a Ghostty client: tmux answers OSC 10/11 itself (it reported `#ffffff` on `#1e1e1e`;
see open question 1) but does **not** answer OSC 4. Passthrough (`allow-passthrough on`) was untested in a visible
pane. So inside tmux, "terminal" gets neutrals right and hues by slot, and only the tints use reference RGBs.

## Roles

Tokyo Night values; derivations are omarchy's sRGB `mix a b pct`.

| role | derivation | tokyo-night | ANSI tier |
|---|---|---|---|
| `text` | terminal default | — | default |
| `strong` | bright_foreground | `#c0caf5` | slot 15 |
| `muted` | mix fg → bg 35% | `#777c98` (4.2:1) | SGR dim |
| `faint` | dark_foreground | `#565f89` (2.8:1) | slot 8 |
| `line` | mix bg → fg 22% | `#393c4d` | slot 8 |
| `line.faint` | mix bg → fg 13% | `#2d2e3d` | slot 8 |
| `surface` | dark_background (derived: bg → black 22%, light themes bg → fg 6%) | `#13141c` | none |
| `selected` | selection | `#292e42` | slot 8 bg |
| `accent` | accent | `#7aa2f7` | slot 4 (configurable) |
| `accent.soft` | mix bg → accent 55% | `#4f6599` | slot 4 |
| `ok` / `warn` / `err` | green / yellow / red | `#9ece6a` `#e0af68` `#f7768e` | slots 2 / 3 / 1 |
| `err.soft` | mix bg → red 55% | `#944d5f` | slot 1 |
| `ok.tint` / `err.tint` | mix bg → green/red 15% | | none |
| `literal` | cyan | `#449dab` | slot 6 |
| `syn.keyword function string number type comment punct` | magenta, blue, green, orange, bright_cyan, dark_foreground (italic), mix fg → bg 20% | | slots 5 4 2 11 14 8, default |

Contrast ratios are against the background. `muted` is for secondary reading text (tool output); `faint` only for
hints, facts and glyphs.

## Surfaces

| surface | today | proposed |
|---|---|---|
| user prompt | `bgDark` band | `surface` band, accent `▎` left bar, `strong` text; ANSI tier keeps the bar only |
| headings | bold purple | bold `strong`, `##` in `faint` |
| bullets, quote | cyan, `comment` | `faint` bullet; `line` bar, `muted` italic quote |
| inline code, links | lightBlue, cyan + URL | `literal`; `accent` underlined, URL in `faint` |
| code fence | `gray` fences | fences `line`, language `faint`, body syntax roles |
| thinking | `dimGray` | `faint` italic; live spinner in `accent` |
| tool header | bold purple name and args | `faint` dot when done (`accent` spinner running, `err` dot failed), bold `strong` name, `muted` args |
| card outline | accent / dim / red, tinted fill | `accent.soft` running, `line` done, `err.soft` failed; no fill (`cards = "filled"` uses `raised`) |
| card labels, footer | bold purple label, border-coloured facts | `faint` label and facts, `line` separators; `Exit`/`timed out` in `err`; `+n`/`-n` in `ok`/`err` |
| bash card | `$` dim, command lightBlue, output dimGray | `$` `faint`, command `strong`, output `muted`; on failure the first output line in `err` |
| diff rows | whole line green/red | `ok.tint`/`err.tint` across the card width, sign in `ok`/`err`, code in `text`; context `muted`, `@@` `faint` |
| subagents | accent/green/red mark, all text dimGray | same marks, name in `text`, rest `muted` |
| composer | accent `μ ❯`, dim placeholder | same, plus a `line.faint` rule above |
| status line | six hues, `│` in 238 | everything `muted`, model name `text`, ` · ` in `line`, counts/units `faint`; colour only for attention: context `warn` ≥ 50%, `err` ≥ 70%, plan badge `accent` |
| menus, selection | `selectedBg` | `selected` row, `accent` marker |

## Implementation notes (later)

- `theme.rs` becomes palette → roles: a `Palette` (colors.toml keys plus which keys came from the terminal), a
  `Roles` table, and a `Tier` (truecolor or ANSI). Every role keeps its source slot so the ANSI tier can emit it.
- Call sites switch from omp keys (`fg("toolTitle")`) to roles (`roles().strong`). The omp key names go away.
- Truecolor detection: `COLORTERM` truecolor/24bit, `TERM` `*-direct`, or a tmux client with `RGB`. Otherwise use
  the ANSI tier; drop `rgb_to_256`.
- Delete `src/tui/themes/`, `theme_data.rs` and the `theme` field of `OmpConfig`.
- Ghostty, kitty and WezTerm answer OSC 4 directly; outside tmux the terminal source is complete.

## Environment (applied)

`~/.tmux.conf`:

```tmux
set -as terminal-features ',xterm-256color:RGB,xterm-ghostty:RGB,xterm-kitty:RGB,alacritty:RGB,foot*:RGB,wezterm:RGB'
set-environment -g COLORTERM truecolor
```

Both were also applied to the running server. New panes get `COLORTERM`; the RGB feature takes effect on the next
attach.

## Open questions

1. tmux reported foreground `#ffffff` on background `#1e1e1e`, which is not Tokyo Night. Is the client terminal
   themed? Run `osc_probe.py` outside tmux to see what the terminal itself answers.
2. Does OSC 4 through `allow-passthrough` work from the visible pane? If so, "terminal" is complete under tmux too.
3. `outline` vs `filled` cards: compare `proposed` and `proposed-fill`.
