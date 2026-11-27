# Desktop app: decisions by the owner

Only what the owner said or chose, in their words where there are words. No values, rationale or implementation from the agent. Where the sketch or the other design docs disagree with this file, this file wins and they are wrong. What is not decided is under [Open](#open), without proposals.

Source tags: **S** = said or chose in the session of 2026-11-28. **E** = from earlier sessions, taken from the agent's own summary of them and not re-confirmed; check with the owner before relying on it.

## How the work is done

- **Visual design is not the agent's call.** "Please do not use your own judgement for visual design decisions. Use me." (S)
- **The TUI is the reference.** "When in doubt, the way the tui works is how the gui should work." (S) The GUI uses the TUI's palette keys and roles and does not invent colours, tints or styles the TUI lacks. (E)
- **Sizes are derived, not voted on.** The question is not how many text sizes there should be but how many the UI needs, and they are not decided "based on nothing". Lists of numbers to pick from are useless. (S)
- **Where a reference is needed, find real implementations.** For Catppuccin: "catppuccin looks ridiculously bad in the tui too, so you shouldn't use that as a reference. Find good catpuccin implementation screenshots instead." (S)

## Decided

### Fonts
- **Default is the system:** the system UI font and a system monospace. Fonts are configurable. (S)
- **This matters for the real GUI, not for the sketch.** "This doesn't matter for the sketch, just for the real gui." (S)
- **The previously used faces are not wanted.** "I have not set any of these rules and I think both are ugly fonts." (S) Any earlier rule about which face is used where was not the owner's.
- **The composer input is in the same face as prose** (sans, not monospace). (S)

### Colour
- **Catppuccin Latte is corrected to Catppuccin's own values, in both the sketch's palette and the TUI's palette file:** mauve is used where pink was, and the TUI file is fixed as well. `bright_magenta` is mauve too. (S)
- **The unfocused-tile treatment keeps both the dimming and the sunk background.** (S)
- **The focus ring is `accent_soft`.** (S)

### Surfaces and contrast
- **A box never has the same background as the tile it sits in.** Backgrounds, foregrounds and the differentiation between layers have to be clearly distinguishable. This was said several times and was not met when the sketch drew the dock, composer and option rows on the tile's own background. (S)
- **The GUI differs from the TUI here by construction.** A terminal has one background; the GUI needs layered surfaces. "The backgrounds and foregrounds in the gui is different by construction from the tui." (S)

### Diffs
- **Coloured text only**, no line backgrounds. (E, "Coloured text only")

### Layout and interaction (all E)
- Emacs-like keys.
- Panels separated by hairlines.
- Focus shown by a ring, with the other tiles dimmed.
- The request dock raised; the composer a card.
- Larger and softer type; depth tuned per theme.
- A lone session tile has a maximum width proportional to the window width, and its content fills the tile.

## Open

None of these has a decision; do not treat anything in the code as one.

- **The actual sizes:** text sizes, spacing, control and row heights, corner radii. The owner has said they must be derived from what the UI needs and from a real reference, not from the agent's taste. Which reference, if any, has not been chosen.
- **Which colours the layers are:** that they must be clearly differentiated is decided (above); which palette keys or values make up the layers, and the foreground hierarchy, are not. "I don't know" was the answer when asked.
- **Primary text colour:** whether plain body text is the palette's `foreground` or `bright_foreground` is not decided. The owner said it was "not even close" and has not said what it should be.
