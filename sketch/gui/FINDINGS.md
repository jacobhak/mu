# Findings from the sketch

Running notes, written into `docs/design/gui*.md` at the end of the review; then `sketch/` is deleted.

## Build

- The offload image has none of GPUI's Linux build libraries (fontconfig fails first). `build.sh` installs them in the job with `sudo apt-get` (libfontconfig-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libx11-xcb-dev libxcb1-dev libxcb-xkb-dev libssl-dev libasound2-dev). The real app's CI image needs them too.
- Cold build of GPUI on an offload node: about 3 min.
- **macOS:** plain `cargo build` fails with `unable to find utility "metal"` on a machine with only the Command Line Tools: GPUI compiles its shaders with Xcode's `metal`. The sketch enables `gpui_platform`'s `runtime_shaders` feature on macOS (shaders compile when the app starts), so no Xcode is needed. It also needs `gpui_platform`'s `font-kit` feature: without it the Core Text text system is not compiled in, and the window draws every box but no glyph, with no error. The real app picks either that or requiring Xcode with its Metal toolchain in the Mac build image.
- The binary needs `libxkbcommon-x11.so.0` at run time (`libxkbcommon-x11-0`) on the machine that runs it.

## Look (after the first review)

- **Chosen with the reviewer:** panels with hairlines (a 4px chrome gutter between tiles and around them, every tile and bar a rounded card with a 1px hairline); focus as a 2px accent ring plus dimmed content in the other tiles (0.74 opacity); the request dock as a raised, shadowed card and the composer as a lifted input card with an accent edge while its tile has focus; tool arguments, paths, diffs, bash commands and code stay in Lilex while everything else (the user's prompt, composer, tool verbs, counts, key caps, subagent rows) is Plex Sans; type 16/14/13/12 (a 17/15/14/13 toggle), radii 6 (chips, rows), 8 (cards), 10 (tiles), 12 (dock, composer, menus), pills fully round; depth tuned per theme (`Theme::shadow`: heavier on dark, soft and wide on light).
- **Three layers:** chrome (darkest, behind the bars and between tiles), bars, then tile cards; overlays and the dock sit above on `raised`.
- **Shadows get clipped by `overflow_hidden`** on the wrapper around a tile, so the split wrappers must not clip; the tile itself does, for its rounded corners.
- **The focus ring is an absolutely positioned overlay,** so focusing a tile never shifts its layout.

## Keys

- **Emacs bindings (changed after trying the sketch):** `C-x o` / `O` cycle focus, `C-x b` opens the agent palette, `M-x` the full one, `C-g` cancels. `C-x 0` / `1` / `2` / `3` close, zoom, split down and right; `C-x t …` are the tabs; `C-x r w` / `r j` are layouts. Directional focus moved from `hjkl` (`C-x h`, `k` are mark and kill-buffer in Emacs) to `C-x ←↓↑→`, because Shift+arrow is the composer's. Tab 2 has no leader key (`t 2` is "new tab" as in Emacs): use Cmd+2.
- **A mac `^x` conflict is possible:** macOS prints "Bad key binding selector for '^x'" at start-up from the user's own key-binding file; `C-x o` worked on the test machine despite it.

- **Collision in the wireframes' Linux column:** Ctrl+Shift+K is both "full palette" and "focus up". The sketch gives it to the palette; focus up stays on the leader (`ctrl-x ↑`).
