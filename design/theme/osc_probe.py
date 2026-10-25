#!/usr/bin/env python3
"""What `theme = "terminal"` would see: queries the terminal's colours and prints them as colors.toml.

    python3 design/theme/osc_probe.py                  # run it in a real terminal, inside and outside tmux
    python3 design/theme/osc_probe.py --passthrough    # inside tmux: send the OSC 4 queries to the outer terminal

Queries OSC 10 (foreground), OSC 11 (background), OSC 17 (selection) and OSC 4;0-15 (palette), with a DA1
query last as a sentinel: every terminal answers DA1, so the read stops as soon as the answers are in.
Unanswered keys are reported as missing; mu would fill them from colors.toml or derive them.
"""
import os
import re
import select
import sys
import termios
import time
import tty

SLOTS = ["background", "red", "green", "yellow", "blue", "magenta", "cyan", "foreground", "muted",
         "bright_red", "bright_green", "bright_yellow", "bright_blue", "bright_magenta", "bright_cyan", "bright_foreground"]
ST = "\x1b\\"


def passthrough(s: str) -> str:
    """tmux DCS passthrough (needs `allow-passthrough on` and a visible pane)."""
    return "\x1bPtmux;" + s.replace("\x1b", "\x1b\x1b") + "\x1b\\"


def query(timeout=1.5, wrap=False) -> bytes:
    fd = os.open("/dev/tty", os.O_RDWR)
    old = termios.tcgetattr(fd)
    tty.setraw(fd)
    try:
        pal = "".join(f"\x1b]4;{i};?{ST}" for i in range(16))
        q = f"\x1b]10;?{ST}\x1b]11;?{ST}\x1b]17;?{ST}" + (passthrough(pal) if wrap else pal) + "\x1b[c"
        os.write(fd, q.encode())
        buf, end = b"", time.time() + timeout
        while time.time() < end:
            if select.select([fd], [], [], 0.05)[0]:
                buf += os.read(fd, 4096)
                if re.search(rb"\x1b\[\?[\d;]*c", buf):
                    break
        return buf
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        os.close(fd)


def to_hex(spec: str) -> str:
    """`rgb:rrrr/gggg/bbbb` (1-4 hex digits per channel) → #rrggbb."""
    parts = spec.split(":", 1)[1].split("/")
    return "#" + "".join("%02x" % round(int(p, 16) * 255 / (16 ** len(p) - 1)) for p in parts)


def main():
    raw = query(wrap="--passthrough" in sys.argv[1:]).decode(errors="replace")
    found = []  # (key, note, #hex)
    for code, rest in re.findall(r"\x1b\](\d+);([^\x07\x1b]*)", raw):
        if code == "4":
            n, spec = rest.split(";", 1)
            found.append((SLOTS[int(n)], f"slot {n}", to_hex(spec)))
        else:
            found.append(({"10": "foreground", "11": "background", "17": "selection"}[code], f"OSC {code}", to_hex(rest)))
    env = {k: os.environ.get(k, "") for k in ("TERM", "COLORTERM", "TMUX", "TERM_PROGRAM")}
    print(f"# {' '.join(f'{k}={v!r}' for k, v in env.items())}")
    print(f"# {len(found)}/19 answered")
    for key, note, v in found:
        r, g, b = (int(v[i:i + 2], 16) for i in (1, 3, 5))
        print(f'{key:<17} = "{v}"  \x1b[48;2;{r};{g};{b}m      \x1b[0m  # {note}')
    slots = {note for _, note, _ in found}
    missing = [f"{i} ({s})" for i, s in enumerate(SLOTS) if f"slot {i}" not in slots]
    if missing:
        print(f"# palette slots not answered: {', '.join(missing)}")


if __name__ == "__main__":
    main()
