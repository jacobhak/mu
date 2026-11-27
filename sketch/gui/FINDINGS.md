# Findings from the sketch

Running notes, written into `docs/design/gui*.md` at the end of the review; then `sketch/` is deleted.

## Build

- The offload image has none of GPUI's Linux build libraries (fontconfig fails first). `build.sh` installs them in the job with `sudo apt-get` (libfontconfig-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libx11-xcb-dev libxcb1-dev libxcb-xkb-dev libssl-dev libasound2-dev). The real app's CI image needs them too.
- Cold build of GPUI on an offload node: about 3 min.
- **macOS:** plain `cargo build` fails with `unable to find utility "metal"` on a machine with only the Command Line Tools: GPUI compiles its shaders with Xcode's `metal`. The sketch enables `gpui_platform`'s `runtime_shaders` feature on macOS (shaders compile when the app starts), so no Xcode is needed. It also needs `gpui_platform`'s `font-kit` feature: without it the Core Text text system is not compiled in, and the window draws every box but no glyph, with no error. The real app picks either that or requiring Xcode with its Metal toolchain in the Mac build image.
- The binary needs `libxkbcommon-x11.so.0` at run time (`libxkbcommon-x11-0`) on the machine that runs it.

## Keys

- **Collision in the wireframes' Linux column:** Ctrl+Shift+K is both "full palette" and "focus up". The sketch gives it to the palette; focus up stays on the leader (`ctrl-x k`).
