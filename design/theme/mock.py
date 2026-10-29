#!/usr/bin/env python3
"""Static mockups of mu's transcript: the current omp theme vs the proposed colors.toml roles.

    python3 design/theme/mock.py                       # current (as seen now) then proposed, stacked
    python3 design/theme/mock.py --side                # same, side by side (needs ~2x width)
    python3 design/theme/mock.py current-256 proposed proposed-ansi --side
    python3 design/theme/mock.py proposed --palette gruvbox
    python3 design/theme/mock.py --html /tmp/mock.html # every variant, browser-viewable
    python3 design/theme/mock.py --swatch              # derived roles for the palette
    python3 design/theme/mock.py --side --paint        # paint the palette background (terminal not themed)

Variants:
    current-256    mu today under tmux without RGB: omp dark-tokyo-night snapped to xterm-256
    current        mu today with truecolor
    proposed       quiet boxes, roles derived from colors.toml, truecolor
    proposed-fill  as proposed, cards on a raised surface instead of the bare background
    proposed-ansi  the proposed design without truecolor: palette slots 0-15 and SGR dim, no tints

Truecolor variants need a truecolor path to the terminal: under tmux, the client must have the RGB
feature (`tmux display -p '#{client_termfeatures}'` lists RGB; detach and reattach after changing it).
Nothing here is wired into mu; this file is design reference only.
"""
from __future__ import annotations

import argparse
import html
import json
import re
import sys
import tomllib
import unicodedata
from dataclasses import dataclass, replace
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
OMP_THEME = HERE / "omp-dark-tokyo-night.json"

# ── colours ──────────────────────────────────────────────────────────────────────────────────────
# A colour is ("rgb", "#rrggbb") or ("idx", n): n < 16 is a terminal palette slot, else xterm-256.
Color = tuple


def rgb(h: str) -> Color:
    return ("rgb", h.lower())


def idx(n: int) -> Color:
    return ("idx", n)


def hex3(h: str) -> tuple[int, int, int]:
    return int(h[1:3], 16), int(h[3:5], 16), int(h[5:7], 16)


def mix(a: str, b: str, pct: float) -> str:
    """omarchy's `mix a b pct`: pct of the way from a to b, in sRGB."""
    (r1, g1, b1), (r2, g2, b2) = hex3(a), hex3(b)
    t = pct / 100
    return "#%02x%02x%02x" % tuple(round(x + (y - x) * t) for x, y in ((r1, r2), (g1, g2), (b1, b2)))


CUBE = [0, 95, 135, 175, 215, 255]


def to256(h: str) -> int:
    """mu's `rgb_to_256` (src/tui/theme.rs): nearest of the 6x6x6 cube and the grey ramp."""
    r, g, b = hex3(h)
    near = lambda v: min(range(6), key=lambda i: abs(CUBE[i] - v))
    ci, cj, ck = near(r), near(g), near(b)
    dist = lambda c: sum((x - y) ** 2 for x, y in zip(c, (r, g, b)))
    cube = dist([CUBE[ci], CUBE[cj], CUBE[ck]])
    gi = max(0, min(23, ((r + g + b) // 3 - 8 + 5) // 10))
    gv = 8 + 10 * gi
    return 232 + gi if dist([gv] * 3) < cube else 16 + 36 * ci + 6 * cj + ck


def xterm256_hex(n: int) -> str:
    if n >= 232:
        v = 8 + 10 * (n - 232)
        return "#%02x%02x%02x" % (v, v, v)
    n -= 16
    return "#%02x%02x%02x" % (CUBE[n // 36], CUBE[n // 6 % 6], CUBE[n % 6])


@dataclass(frozen=True)
class Style:
    fg: Color | None = None
    bg: Color | None = None
    bold: bool = False
    dim: bool = False
    italic: bool = False
    underline: bool = False


PLAIN = Style()
Span = tuple  # (text, Style)
Line = list  # [Span]


def width(line: Line) -> int:
    return sum(sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in t) for t, _ in line)


def fill(line: Line, w: int, bg: Color | None) -> Line:
    """mu's `line::fill`: paint `bg` under spans that have none and pad to `w`."""
    out = [(t, s if s.bg or not bg else replace(s, bg=bg)) for t, s in line]
    pad = w - width(out)
    if pad > 0:
        out.append((" " * pad, Style(bg=bg)))
    return out


# ── palettes ─────────────────────────────────────────────────────────────────────────────────────


def load_palette(name_or_path: str) -> dict[str, str]:
    p = Path(name_or_path)
    if not p.exists():
        p = REPO / "src/tui/palettes" / f"{name_or_path}.toml"
    pal = tomllib.loads(p.read_text())
    pal.setdefault("orange", pal.get("bright_yellow", pal["yellow"]))
    pal["_name"] = p.stem
    return pal


def ansi_slots(pal: dict) -> list[str]:
    """What omarchy pushes to the terminal (omarchy-theme-osc): OSC 4 slots 0-15."""
    k = ["background", "red", "green", "yellow", "blue", "magenta", "cyan", "foreground", "muted",
         "bright_red", "bright_green", "bright_yellow", "bright_blue", "bright_magenta", "bright_cyan", "bright_foreground"]
    return [pal[x] for x in k]


# ── current: omp JSON theme, resolved like src/tui/theme.rs ───────────────────────────────────────


class OmpTheme:
    def __init__(self, path: Path, truecolor: bool):
        j = json.loads(path.read_text())
        self.vars, self.truecolor = j["vars"], truecolor
        self.colors = {k: self._resolve(v, 0) for k, v in j["colors"].items()}

    def _resolve(self, v, depth):
        if depth > 8:
            return None
        if isinstance(v, int):
            return idx(min(v, 255))
        if v == "":
            return None
        if re.fullmatch(r"#[0-9a-fA-F]{6}", v):
            return rgb(v) if self.truecolor else idx(to256(v))
        return self._resolve(self.vars[v], depth + 1) if v in self.vars else None

    def c(self, k):
        return self.colors.get(k)

    def fg(self, k, **kw):
        return Style(fg=self.c(k), **kw)


# ── proposed: roles derived from a colors.toml palette ───────────────────────────────────────────

SLOT = {"red": 1, "green": 2, "yellow": 3, "blue": 4, "magenta": 5, "cyan": 6, "muted": 8,
        "bright_red": 9, "bright_green": 10, "bright_yellow": 11, "bright_blue": 12,
        "bright_magenta": 13, "bright_cyan": 14, "bright_foreground": 15}


class Roles:
    """The proposed role layer. Truecolor: hex values mixed from the palette. ANSI: palette slots,
    SGR dim for secondary text, and no background tints (they cannot be expressed)."""

    def __init__(self, pal: dict, truecolor: bool, filled: bool = False):
        self.pal, self.tc, self.filled = pal, truecolor, filled
        bg, fg = pal["background"], pal["foreground"]
        P = pal
        # (truecolor value, ANSI fallback) per role; fallback None = terminal default / no paint.
        self.table = {
            # text ramp
            "text":       (None, None),
            "strong":     (P["bright_foreground"], idx(15)),
            "muted":      (mix(fg, bg, 35), "dim"),
            "faint":      (P["dark_foreground"], idx(8)),
            # lines and surfaces
            "line":       (mix(bg, fg, 22), idx(8)),
            "line.faint": (mix(bg, fg, 13), idx(8)),
            "raised":     (mix(bg, fg, 5), None),
            "selected":   (P["selection"], idx(8)),
            # accent and states
            "accent":      (P["accent"], idx(4)),
            "accent.soft": (mix(bg, P["accent"], 55), idx(4)),
            "ok":          (P["green"], idx(2)),
            "warn":        (P["yellow"], idx(3)),
            "err":         (P["red"], idx(1)),
            "err.soft":    (mix(bg, P["red"], 55), idx(1)),
            "ok.tint":     (mix(bg, P["green"], 15), None),
            "err.tint":    (mix(bg, P["red"], 15), None),
            "literal":     (P["cyan"], idx(6)),
            # syntax: the only place the whole palette appears
            "syn.keyword":  (P["magenta"], idx(5)),
            "syn.function": (P["blue"], idx(4)),
            "syn.string":   (P["green"], idx(2)),
            "syn.number":   (P["orange"], idx(11)),
            "syn.type":     (P["bright_cyan"], idx(14)),
            "syn.comment":  (P["dark_foreground"], idx(8)),
            "syn.punct":    (mix(fg, bg, 20), None),
        }

    def c(self, role) -> Color | None:
        tc, ansi = self.table[role]
        if self.tc:
            return rgb(tc) if tc else None
        return ansi if isinstance(ansi, tuple) else None

    def fg(self, role, **kw) -> Style:
        tc, ansi = self.table[role]
        if not self.tc and ansi == "dim":
            return Style(dim=True, **kw)
        return Style(fg=self.c(role), **kw)

    def surface(self):
        return self.c("raised") if self.filled else None


# ── scene content (shared) ───────────────────────────────────────────────────────────────────────

PROMPT = "the status line feels noisy; make it calmer and check the tests still pass"
HEADING = "Making the status line calmer"
PARA = [("t", "It uses six hues today. Every segment moves to "), ("code", "text.muted"), ("t", " and colour")]
PARA2 = [("t", "stays for states that need attention, as in "), ("link", "Codex"), ("t", ".")]
URL = "https://openai.com/codex"
BULLETS = [
    [("t", "separators become "), ("code", " · "), ("t", " in "), ("code", "line.faint")],
    [("t", "context turns "), ("b", "yellow"), ("t", " at 50% and red at 70%")],
    [("t", "the plan badge is the only accent")],
]
CODE = [  # rust, pre-tokenised
    [("keyword", "fn"), ("", " "), ("function", "sep"), ("punct", "() -> "), ("type", "Style"), ("punct", " {")],
    [("", "    "), ("function", "roles"), ("punct", "()."), ("", "line_faint"), ("", " "), ("comment", "// was 238")],
    [("punct", "}")],
]
QUOTE = "Tokyo Night is flat; depth comes from small background shifts."
DIFF = [
    "@@ -173,7 +173,7 @@",
    "     }",
    " ",
    " fn group(parts: &[Part]) -> Line {",
    "-    let sep = theme::get().fg(\"statusLineSep\");",
    "+    let sep = roles().line_faint;",
    "     let plain = ContentStyle::default();",
]
TEST_OUT = ["running 41 tests", "........................................ 41/41", "test result: ok. 41 passed; 0 failed"]
CLIPPY_OUT = ["error: unused import: `theme`", "  --> src/tui/status.rs:4:5", "  |", "4 | use super::theme;",
              "  |     ^^^^^^^^^^^^", "error: could not compile `mu` (bin \"mu\") due to 1 previous error"]
LIVE_TAIL = ["   Compiling mu v0.9.0 (~/code/mu)", "   Compiling crossterm v0.28.1"]
SUBAGENTS = [("running", "scout", 4, "reading src/tui/theme.rs"), ("done", "palette", 9, "derived 22 roles"),
             ("failed", "osc-probe", 2, "tmux did not answer OSC 4")]
SPIN = "⠋"


# ── renderers ────────────────────────────────────────────────────────────────────────────────────


@dataclass
class Tinted:
    """A card row painted with its own background across the full content width (diff lines)."""
    bg: Color | None
    line: Line


class Card:
    def __init__(self, tone, sections, footer=None):
        self.tone, self.sections, self.footer = tone, sections, footer  # sections: [(label|None, rows)]


class Current:
    """Mirrors src/tui/{blocks,markdown,status,composer}.rs as of 0bf1fbd."""

    def __init__(self, truecolor: bool):
        self.t = OmpTheme(OMP_THEME, truecolor)

    def k(self, key, **kw):
        return self.t.fg(key, **kw)

    def user(self, text, w):
        bg = self.t.c("userMessageBg")
        return [fill([], w, bg), fill([(" " + text, PLAIN)], w, bg), fill([], w, bg)]

    def inline(self, segs):
        out = []
        for kind, s in segs:
            if kind == "code":
                out.append((s, self.k("mdCode")))
            elif kind == "b":
                out.append((s, Style(bold=True)))
            elif kind == "link":
                out += [(s, self.k("mdLink", underline=True)), (f" ({URL})", self.k("mdLinkUrl"))]
            else:
                out.append((s, PLAIN))
        return out

    def markdown(self, w):
        out = [[(f"## {HEADING}", self.k("mdHeading", bold=True))], [], self.inline(PARA), self.inline(PARA2), []]
        out += [[("• ", self.k("mdListBullet"))] + self.inline(b) for b in BULLETS]
        out += [[], [("```rust", self.k("mdCodeBlockBorder"))]]
        syn = {"keyword": "syntaxKeyword", "function": "syntaxFunction", "type": "syntaxType", "punct": "syntaxPunctuation",
               "comment": "syntaxComment", "": "syntaxVariable"}
        out += [[("  ", PLAIN)] + [(s, self.k(syn[kind])) for kind, s in l] for l in CODE]
        out += [[("```", self.k("mdCodeBlockBorder"))], [], [("│ ", self.k("mdQuoteBorder")), (QUOTE, self.k("mdQuote"))]]
        return out

    def thinking(self, secs):
        return [[(f"◇ Thought for {secs}s", self.k("thinkingText"))]]

    def header(self, icon, icon_style, name, summary):
        return [(f"{icon} ", icon_style), (name, self.k("toolTitle", bold=True)), (f" {summary}", self.k("toolTitle"))]

    def frame(self, card, w):
        bg = self.t.c({"pending": "toolPendingBg", "success": "toolSuccessBg", "error": "toolErrorBg"}[card.tone])
        bd = replace(self.k({"pending": "accent", "success": "dim", "error": "error"}[card.tone]), bg=bg)
        cw = w - 4

        def rule(l, r, label=None):
            out, used = [(f"{l}───", bd)], 5
            if label:
                out.append((f" {label[0]} ", replace(label[1], bg=bg)))
                used += len(label[0]) + 2
            out += [("─" * (w - used), bd), (r, bd)]
            return fill(out, w, bg)

        out = [rule("╭", "╮")]
        for i, (label, rows) in enumerate(card.sections):
            if label:
                out.append(rule("├", "┤", (label, self.k("toolTitle", bold=True))))
            elif i:
                out.append(rule("├", "┤"))
            out += [[("│ ", bd)] + fill(r, cw, bg) + [(" │", bd)] for r in rows]
        out.append(rule("╰", "╯", (card.footer, bd) if card.footer else None))
        return out

    def read(self, path, n, w):
        return [self.header("●", PLAIN, "read", path), [(f"  {n} lines", self.k("toolOutput"))]]

    def diff_rows(self):
        key = {"+": "toolDiffAdded", "-": "toolDiffRemoved", "@": "toolOutput"}
        return [[(l, self.k(key.get(l[:1], "toolDiffContext")))] for l in DIFF]

    def edit(self, path, w):
        return [self.header("●", PLAIN, "edit", path)] + self.frame(Card("success", [(None, self.diff_rows())], "+1 -1"), w)

    def bash(self, cmd, out, tone, footer, w):
        dim = self.k("toolOutput")
        secs = [(None, [[("$ ", dim), (cmd, self.k("mdCode"))]])]
        if out:
            secs.append(("Output", [[(l, dim)] for l in out]))
        return self.frame(Card(tone, secs, footer), w)

    def subagents(self):
        mark = {"running": (SPIN, "accent"), "done": ("✓", "success"), "failed": ("✗", "error")}
        return [[(f"  {mark[s][0]} ", self.k(mark[s][1])), (f"{n} · {t} tools · {last}", self.k("toolOutput"))]
                for s, n, t, last in SUBAGENTS]

    def thinking_live(self):
        return [[(f"{SPIN} Thinking… 3s", self.k("thinkingText"))]]

    def composer(self, w):
        hint = "/ commands · ⇧⇥ thinking"
        left = [(" μ ❯", self.k("accent")), (" ", PLAIN), ("type here…", self.k("dim"))]
        return [[], left + [(" " * (w - width(left) - len(hint)), PLAIN), (hint, self.k("dim"))]]

    def status(self, w):
        sep = self.k("statusLineSep")
        parts = [
            [(f"{SPIN} 42s", self.k("dim"))],
            [("Opus 5.5 ◒", self.k("statusLineModel"))],
            [("~/c/p/mu", self.k("statusLinePath"))],
            [("main", self.k("statusLineGitDirty")), (" ", PLAIN), ("*2", self.k("statusLineDirty")),
             (" ", PLAIN), ("?1", self.k("statusLineUntracked"))],
            [("12.3% of 1M", self.k("statusLineContext"))],
            [("$0.42", self.k("statusLineCost"))],
        ]
        left = [(" ", PLAIN)]
        for i, p in enumerate(parts):
            left += ([(" ", PLAIN), ("│", sep), (" ", PLAIN)] if i else []) + p
        left.append((" ", PLAIN))
        right = [(" ", PLAIN), ("calm status", self.k("accent")), (" ", PLAIN)]
        return [left + [(" " * max(1, w - width(left) - width(right)), PLAIN)] + right]


class Proposed(Current):
    """Quiet boxes: same geometry as today, colour only where it carries meaning."""

    def __init__(self, pal, truecolor, filled=False):
        self.r = Roles(pal, truecolor, filled)

    def k(self, role, **kw):
        return self.r.fg(role, **kw)

    def user(self, text, w):
        bg = self.r.c("raised")
        bar = ("▎", replace(self.k("accent"), bg=bg))
        return [fill([bar], w, bg), fill([bar, (text, self.k("strong", bg=bg))], w, bg), fill([bar], w, bg)]

    def inline(self, segs):
        out = []
        for kind, s in segs:
            if kind == "code":
                out.append((s, self.k("literal")))
            elif kind == "b":
                out.append((s, self.k("strong", bold=True)))
            elif kind == "link":
                out += [(s, self.k("accent", underline=True)), (f" ({URL})", self.k("faint"))]
            else:
                out.append((s, PLAIN))
        return out

    def markdown(self, w):
        out = [[("## ", self.k("faint", bold=True)), (HEADING, self.k("strong", bold=True))], [],
               self.inline(PARA), self.inline(PARA2), []]
        out += [[("• ", self.k("faint"))] + self.inline(b) for b in BULLETS]
        out += [[], [("```", self.k("line")), ("rust", self.k("faint"))]]
        syn = {"keyword": "syn.keyword", "function": "syn.function", "type": "syn.type", "punct": "syn.punct",
               "comment": "syn.comment", "": "text"}
        out += [[("  ", PLAIN)] + [(s, self.k(syn[kind], italic=kind == "comment")) for kind, s in l] for l in CODE]
        out += [[("```", self.k("line"))], [], [("│ ", self.k("line")), (QUOTE, self.k("muted", italic=True))]]
        return out

    def thinking(self, secs):
        return [[(f"◇ Thought for {secs}s", self.k("faint", italic=True))]]

    def header(self, icon, icon_style, name, summary):
        return [(f"{icon} ", icon_style), (name, self.k("strong", bold=True)), (f" {summary}", self.k("muted"))]

    def frame(self, card, w):
        bg = self.r.surface()
        bd = replace(self.k({"pending": "accent.soft", "success": "line", "error": "err.soft"}[card.tone]), bg=bg)
        cw = w - 4

        def rule(l, r, label=None):
            out, used = [(f"{l}─", bd)], 2
            if label:
                out += [(" ", Style(bg=bg))] + [(t, replace(s, bg=bg)) for t, s in label] + [(" ", Style(bg=bg))]
                used += sum(len(t) for t, _ in label) + 2
            out += [("─" * (w - used - 1), bd), (r, bd)]
            return fill(out, w, bg)

        out = [rule("╭", "╮")]
        for i, (label, rows) in enumerate(card.sections):
            if label:
                out.append(rule("├", "┤", [(label, self.k("faint"))]))
            elif i:
                out.append(rule("├", "┤"))
            for r in rows:
                body = fill(r.line, cw, r.bg) if isinstance(r, Tinted) else fill(r, cw, bg)
                out.append([("│ ", bd)] + body + [(" │", bd)])
        out.append(rule("╰", "╯", card.footer))
        return out

    def facts(self, *parts):
        """Footer facts: faint text, one semantic colour where a fact is bad news."""
        out = []
        for i, p in enumerate(parts):
            if i:
                out.append((" · ", self.k("line")))
            out += p if isinstance(p, list) else [(p, self.k("faint"))]
        return out

    def read(self, path, n, w):
        return [self.header("●", self.k("faint"), "read", path), [(f"  {n} lines", self.k("muted"))]]

    def diff_rows(self):
        rows = []
        for l in DIFF:
            sign = l[:1]
            if sign in "+-":
                role = "ok" if sign == "+" else "err"
                t = self.r.c(f"{role}.tint")
                rows.append(Tinted(t, [(sign, self.k(role, bg=t)), (l[1:], Style(bg=t))]))
            else:
                rows.append([(l, self.k("faint" if sign == "@" else "muted"))])
        return rows

    def edit(self, path, w):
        rows = self.diff_rows()
        footer = self.facts([("+1", self.k("ok")), (" ", PLAIN), ("-1", self.k("err"))])
        return [self.header("●", self.k("faint"), "edit", path)] + self.frame(Card("success", [(None, rows)], footer), w)

    def bash(self, cmd, out, tone, footer, w):
        secs = [(None, [[("$ ", self.k("faint")), (cmd, self.k("strong"))]])]
        if out:
            style = self.k("err") if tone == "error" else self.k("muted")
            secs.append(("output", [[(l, style if i == 0 or tone != "error" else self.k("muted"))] for i, l in enumerate(out)]))
        parts = []
        for f in footer.split(" · "):
            if f.startswith("Exit") or f == "timed out":
                parts.append([(f, self.k("err"))])
            else:
                parts.append(f)
        return self.frame(Card(tone, secs, self.facts(*parts)), w)

    def subagents(self):
        mark = {"running": (SPIN, "accent"), "done": ("✓", "ok"), "failed": ("✗", "err")}
        return [[(f"  {mark[s][0]} ", self.k(mark[s][1])), (n, PLAIN), (f" · {t} tools · {last}", self.k("muted"))]
                for s, n, t, last in SUBAGENTS]

    def thinking_live(self):
        return [[(f"{SPIN} ", self.k("accent")), ("Thinking… 3s", self.k("faint", italic=True))]]

    def composer(self, w):
        left = [(" μ ❯", self.k("accent")), (" ", PLAIN), ("type here…", self.k("faint"))]
        return [[("─" * w, self.k("line.faint"))], left]

    def status(self, w):
        m = self.k("muted")
        sep = [(" · ", self.k("line"))]
        parts = [
            [(SPIN, self.k("accent")), (" 42s", m)],
            [("Opus 5.5", PLAIN), (" (h)", m)],
            [("~/c/p/mu", m)],
            [("main", m), (" *2 ?1", self.k("faint"))],
            [("12.3%", m), (" of 1M", self.k("faint"))],
            [("$0.42", m)],
        ]
        left = [(" ", PLAIN)]
        for i, p in enumerate(parts):
            left += (sep if i else []) + p
        right = [("calm status", self.k("faint")), (" ", PLAIN)]
        return [left + [(" " * max(1, w - width(left) - width(right)), PLAIN)] + right]


def scene(r: Current, w: int) -> list[Line]:
    blocks = [
        r.user(PROMPT, w),
        r.thinking(4),
        r.markdown(w),
        r.read("src/tui/status.rs", 569, w),
        r.edit("src/tui/status.rs", w),
        r.bash("cargo test -q status", TEST_OUT, "success", "Wall: 3.2s · Timeout: 300s", w),
        r.bash("cargo clippy -- -D warnings", CLIPPY_OUT, "error", "Wall: 0.4s · Timeout: 300s · Exit: 101", w),
        r.subagents(),
        r.bash("cargo build --release", LIVE_TAIL, "pending", "running 12s · Timeout: 300s", w),
        r.thinking_live(),
    ]
    out: list[Line] = []
    for i, b in enumerate(blocks):
        if i:
            out.append([])
        out += b
    return out + r.composer(w) + r.status(w)


# ── output ───────────────────────────────────────────────────────────────────────────────────────


def sgr_color(c: Color, bg: bool) -> str:
    kind, v = c
    if kind == "rgb":
        r_, g_, b_ = hex3(v)
        return f"{48 if bg else 38};2;{r_};{g_};{b_}"
    if v < 8:
        return str((40 if bg else 30) + v)
    if v < 16:
        return str((100 if bg else 90) + v - 8)
    return f"{48 if bg else 38};5;{v}"


def to_ansi(line: Line) -> str:
    out = []
    for t, s in line:
        codes = [c for c, on in (("1", s.bold), ("2", s.dim), ("3", s.italic), ("4", s.underline)) if on]
        if s.fg:
            codes.append(sgr_color(s.fg, False))
        if s.bg:
            codes.append(sgr_color(s.bg, True))
        out.append(f"\x1b[{';'.join(codes)}m{t}\x1b[0m" if codes else t)
    return "".join(out)


def to_html(line: Line, pal: dict) -> str:
    slots = ansi_slots(pal)

    def css(c):
        kind, v = c
        return v if kind == "rgb" else (slots[v] if v < 16 else xterm256_hex(v))

    out = []
    for t, s in line:
        st = []
        fg = css(s.fg) if s.fg else pal["foreground"]
        if s.dim:
            fg = mix(fg, pal["background"], 40)
        if s.fg or s.dim:
            st.append(f"color:{fg}")
        if s.bg:
            st.append(f"background:{css(s.bg)}")
        if s.bold:
            st.append("font-weight:bold")
        if s.italic:
            st.append("font-style:italic")
        if s.underline:
            st.append("text-decoration:underline")
        e = html.escape(t)
        out.append(f'<span style="{";".join(st)}">{e}</span>' if st else e)
    return "".join(out)


VARIANTS = {
    "current-256": ("current · as seen now (tmux, no RGB → xterm-256)", lambda p: Current(False)),
    "current": ("current · omp dark-tokyo-night, truecolor", lambda p: Current(True)),
    "proposed": ("proposed · quiet boxes, truecolor", lambda p: Proposed(p, True)),
    "proposed-fill": ("proposed · quiet boxes on a raised surface", lambda p: Proposed(p, True, filled=True)),
    "proposed-ansi": ("proposed · no truecolor: palette slots, no tints", lambda p: Proposed(p, False)),
}


def swatch(pal: dict) -> list[Line]:
    r = Roles(pal, True)
    out = [[(f"{pal['_name']} → roles", Style(bold=True))]]
    for role, (tc, ansi) in r.table.items():
        a = ansi if isinstance(ansi, str) or ansi is None else f"slot {ansi[1]}"
        col = rgb(tc) if tc else None
        out.append([(f"  {role:<13}", PLAIN), ("      ", Style(bg=col)), (f"  {tc or 'terminal default':<17}", Style(fg=col)),
                    (f" ansi: {a or 'none'}", Style(dim=True))])
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("variants", nargs="*", metavar="VARIANT", help=", ".join(VARIANTS))
    ap.add_argument("--palette", default="tokyo-night", help="omarchy colors.toml name (design/theme/palettes) or path")
    ap.add_argument("--width", type=int, default=0, help="pane width (default: fit the terminal)")
    ap.add_argument("--side", action="store_true", help="variants side by side")
    ap.add_argument("--html", metavar="FILE", help="write every requested variant (default: all) to an HTML page")
    ap.add_argument("--swatch", action="store_true", help="print the palette's derived roles")
    ap.add_argument("--paint", action="store_true",
                    help="paint the palette background behind every pane, to preview a palette your terminal is not using")
    a = ap.parse_args()
    if bad := [v for v in a.variants if v not in VARIANTS]:
        ap.error(f"unknown variant(s) {bad}; choose from {list(VARIANTS)}")
    pal = load_palette(a.palette)
    if a.swatch:
        for l in swatch(pal):
            print(to_ansi(l))
        return
    names = a.variants or (list(VARIANTS) if a.html else ["current-256", "proposed"])
    cols = __import__("shutil").get_terminal_size((200, 50)).columns
    w = a.width or min(88, (cols - 3 * (len(names) - 1)) // len(names) if a.side else cols)
    panes = [(VARIANTS[n][0], scene(VARIANTS[n][1](pal), w)) for n in names]
    if a.paint:
        panes = [(t, [fill(l, w, rgb(pal["background"])) for l in ls]) for t, ls in panes]
    if a.html:
        bg, fg = pal["background"], pal["foreground"]
        body = "".join(
            f'<div class="pane"><h3>{html.escape(t)}</h3><pre>' + "\n".join(to_html(l, pal) for l in ls) + "</pre></div>"
            for t, ls in panes)
        Path(a.html).write_text(
            f"<!doctype html><meta charset=utf-8><title>mu theme mockups · {pal['_name']}</title><style>"
            f"body{{background:{bg};color:{fg};margin:24px;font:14px/1.35 'JetBrainsMono Nerd Font','JetBrains Mono',monospace}}"
            f".wrap{{display:flex;gap:32px;flex-wrap:wrap}}h3{{font-weight:normal;color:{pal['dark_foreground']}}}"
            f"pre{{margin:0;font:inherit}}</style><div class=wrap>{body}</div>")
        print(a.html)
        return
    if a.side:
        print("   ".join(f"\x1b[1m{t[:w]:<{w}}\x1b[0m" for t, _ in panes))
        for i in range(max(len(ls) for _, ls in panes)):
            row = []
            for _, ls in panes:
                l = ls[i] if i < len(ls) else []
                row.append(to_ansi(l) + " " * max(0, w - width(l)))
            print("   ".join(row))
    else:
        for t, ls in panes:
            print(f"\n\x1b[1m── {t} ──\x1b[0m\n")
            for l in ls:
                print(to_ansi(l))


if __name__ == "__main__":
    sys.exit(main())
