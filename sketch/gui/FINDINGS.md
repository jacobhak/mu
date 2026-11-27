# Findings from the sketch

Running notes, written into `docs/design/gui*.md` at the end of the review; then `sketch/` is deleted.

## Build

- The offload image has none of GPUI's Linux build libraries (fontconfig fails first). `build.sh` installs them in the job with `sudo apt-get` (libfontconfig-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libx11-xcb-dev libxcb1-dev libxcb-xkb-dev libssl-dev libasound2-dev). The real app's CI image needs them too.
- Cold build of GPUI on an offload node: about 3 min.
- The binary needs `libxkbcommon-x11.so.0` at run time (`libxkbcommon-x11-0`) on the machine that runs it.

## Keys

- **Collision in the wireframes' Linux column:** Ctrl+Shift+K is both "full palette" and "focus up". The sketch gives it to the palette; focus up stays on the leader (`ctrl-x k`).
