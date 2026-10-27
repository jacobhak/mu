#!/usr/bin/env python3
"""Render `tmux capture-pane -e` output (SGR only) as HTML, and optionally a PNG via headless Chromium.

    python3 design/theme/ansi2png.py capture.ansi out.png [--palette tokyo-night] [--cols 120] [--title T]

The terminal's own colours (default fg/bg and the 16 ANSI slots) come from the palette, mapped the way omarchy
pushes them to the terminal: slot 0 background-ish, 1-6 hues, 7 foreground, 8 muted, 9-15 bright forms.
"""
import html
import re
import subprocess
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
PALETTES = HERE.parent.parent / "src/tui/palettes"

# Ghostty's built-in TokyoNight: what a tokyo-night terminal answers for the 16 slots.
GHOSTTY_TOKYO = {
    "fg": "#c0caf5",
    "bg": "#1a1b26",
    "slots": [
        "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
        "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
    ],
}


def terminal(name: str) -> dict:
    if name == "ghostty-tokyo":
        return GHOSTTY_TOKYO
    p = tomllib.loads((PALETTES / f"{name}.toml").read_text())
    g = lambda *ks: next((p[k] for k in ks if k in p), p["foreground"])
    slots = [
        p.get("dark_background", p["background"]), g("red"), g("green"), g("yellow"), g("blue"), g("magenta"),
        g("cyan"), p["foreground"], g("muted", "dark_foreground"), g("bright_red", "red"),
        g("bright_green", "green"), g("bright_yellow", "yellow"), g("bright_blue", "blue"),
        g("bright_magenta", "magenta"), g("bright_cyan", "cyan"), g("bright_foreground", "foreground"),
    ]
    return {"fg": p["foreground"], "bg": p["background"], "slots": slots}


def x256(n: int, slots: list[str]) -> str:
    if n < 16:
        return slots[n]
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (lv[n // 36], lv[n // 6 % 6], lv[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


def mixhex(a: str, b: str, p: float) -> str:
    ca = [int(a[i : i + 2], 16) for i in (1, 3, 5)]
    cb = [int(b[i : i + 2], 16) for i in (1, 3, 5)]
    return "#" + "".join("%02x" % round(x + (y - x) * p) for x, y in zip(ca, cb))


SGR = re.compile(r"\x1b\[([0-9;:]*)m")
OTHER = re.compile(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-ln-z]|\x1b[()][0-9A-B]|\x1b[=>]")


def convert(text: str, term: dict, cols: int) -> str:
    slots = term["slots"]
    st = {}

    def color(params, i):
        if params[i + 1] == 5:
            return x256(params[i + 2], slots), i + 3
        return "#%02x%02x%02x" % tuple(params[i + 2 : i + 5]), i + 5

    rows = []
    for raw in OTHER.sub("", text).split("\n"):
        out = []
        pos = 0
        for m in list(SGR.finditer(raw)) + [None]:
            chunk = raw[pos : m.start() if m else len(raw)]
            if chunk:
                fg = st.get("fg") or term["fg"]
                bg = st.get("bg")
                if st.get("rev"):
                    fg, bg = (bg or term["bg"]), fg
                if st.get("dim"):
                    fg = mixhex(fg, term["bg"], 0.5)
                css = [f"color:{fg}"]
                if bg:
                    css.append(f"background:{bg}")
                if st.get("bold"):
                    css.append("font-weight:bold")
                if st.get("it"):
                    css.append("font-style:italic")
                deco = [d for d, k in (("underline", "ul"), ("line-through", "strike")) if st.get(k)]
                if deco:
                    css.append("text-decoration:" + " ".join(deco))
                out.append(f'<span style="{";".join(css)}">{html.escape(chunk)}</span>')
            if not m:
                break
            pos = m.end()
            ps = [int(x) if x else 0 for x in re.split("[;:]", m.group(1))] or [0]
            i = 0
            while i < len(ps):
                c = ps[i]
                if c == 0:
                    st = {}
                elif c == 1:
                    st["bold"] = True
                elif c == 2:
                    st["dim"] = True
                elif c == 3:
                    st["it"] = True
                elif c == 4:
                    st["ul"] = True
                elif c == 7:
                    st["rev"] = True
                elif c == 9:
                    st["strike"] = True
                elif c == 22:
                    st.pop("bold", None), st.pop("dim", None)
                elif c == 23:
                    st.pop("it", None)
                elif c == 24:
                    st.pop("ul", None)
                elif c == 27:
                    st.pop("rev", None)
                elif c == 29:
                    st.pop("strike", None)
                elif 30 <= c <= 37:
                    st["fg"] = slots[c - 30]
                elif 90 <= c <= 97:
                    st["fg"] = slots[c - 90 + 8]
                elif 40 <= c <= 47:
                    st["bg"] = slots[c - 40]
                elif 100 <= c <= 107:
                    st["bg"] = slots[c - 100 + 8]
                elif c in (38, 48):
                    v, i = color(ps, i)
                    st["fg" if c == 38 else "bg"] = v
                    continue
                elif c == 39:
                    st.pop("fg", None)
                elif c == 49:
                    st.pop("bg", None)
                i += 1
        rows.append("".join(out))
    while rows and not rows[-1].strip():
        rows.pop()
    return "\n".join(rows)


def page(body: str, term: dict, cols: int, title: str) -> str:
    return f"""<!doctype html><meta charset=utf-8><style>
body{{margin:0;background:{term['bg']}}}
.t{{font:14px/1.25 'JetBrainsMono Nerd Font','JetBrains Mono','DejaVu Sans Mono',monospace;color:{term['fg']};
background:{term['bg']};white-space:pre;padding:12px 14px;width:{cols}ch}}
.h{{font:12px sans-serif;color:#888;padding:6px 14px;background:#000}}
</style>{f'<div class=h>{html.escape(title)}</div>' if title else ''}<div class=t>{body}</div>"""


def annotate(text: str, term: dict) -> str:
    """Plain text with `{fg/bg+attrs}` markers at every style change; for reading a capture without a browser."""
    body = convert(text, term, 0)
    out = []
    for row in body.split("\n"):
        line, last = [], None
        for m in re.finditer(r'<span style="([^"]*)">([^<]*)</span>', row):
            css = dict(kv.split(":", 1) for kv in m.group(1).split(";"))
            fg = css.get("color", "")
            tag = ("" if fg == term["fg"] else fg) + ("/" + css["background"] if "background" in css else "")
            tag += "".join(f"+{a}" for a, k in (("b", "font-weight"), ("i", "font-style"), ("u", "text-decoration")) if k in css)
            if tag != last:
                line.append("{" + tag + "}")
                last = tag
            line.append(html.unescape(m.group(2)))
        out.append("".join(line))
    return "\n".join(out)


def main():
    a = sys.argv[1:]
    opt = lambda k, d: a[a.index(k) + 1] if k in a else d
    src = a[0]
    term = terminal(opt("--palette", "ghostty-tokyo"))
    if len(a) < 2 or a[1].startswith("--"):
        print(annotate(Path(src).read_text(errors="replace"), term))
        return
    dst = a[1]
    cols = int(opt("--cols", "120"))
    body = convert(Path(src).read_text(errors="replace"), term, cols)
    doc = page(body, term, cols, opt("--title", ""))
    htmlp = Path(dst).with_suffix(".html")
    htmlp.write_text(doc)
    if dst.endswith(".png"):
        nrows = body.count("\n") + 3
        h = int(nrows * 17.5) + 40
        w = int(cols * 8.45) + 40
        shell = next(Path.home().glob(".cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell"), None)
        subprocess.run(
            [str(shell) if shell else "chromium-browser", "--headless", "--no-sandbox", "--disable-gpu", "--hide-scrollbars",
             f"--window-size={w},{h}", f"--screenshot={Path(dst).resolve()}", f"file://{htmlp.resolve()}"],
            check=True, capture_output=True,
        )


if __name__ == "__main__":
    main()
