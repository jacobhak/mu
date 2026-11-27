//! A mu palette resolved to the TUI's roles (the same keys and mixes as `crates/mu-tui/src/theme.rs`, `DEFS`),
//! the GUI-only roles, and the type scale. Every size lives in `SCALES` so a review edits one place.
use gpui::{BoxShadow, Hsla, Rgba, point, px, rgb};
use std::collections::HashMap;

pub type Rgb = (u8, u8, u8);

pub const SANS: &str = "IBM Plex Sans";
pub const MONO: &str = "Lilex";

/// The two sets of type sizes under review; a key toggles between them.
#[derive(Clone, Copy, Debug)]
pub struct TypeScale {
    pub name: &'static str,
    /// Model prose, Plex Sans.
    pub prose: f32,
    /// Interface text (bars, headers, rows, hints), Plex Sans.
    pub ui: f32,
    /// Code, tool rows, diffs, paths, the composer, Lilex.
    pub code: f32,
    /// Small text: meta lines, key hints.
    pub small: f32,
    /// Line heights as multiples of the size.
    pub prose_lh: f32,
    pub code_lh: f32,
}

/// Minimum contrast of the text roles against the surfaces they sit on (WCAG AAA body, AA, and AA for dim text).
const FLOOR_TEXT: f64 = 11.0;
const FLOOR_MUTED: f64 = 7.0;
const FLOOR_FAINT: f64 = 4.5;

pub const SCALES: [TypeScale; 2] = [
    TypeScale { name: "16/14", prose: 16.0, ui: 14.0, code: 13.0, small: 12.0, prose_lh: 1.55, code_lh: 1.45 },
    TypeScale { name: "17/15", prose: 17.0, ui: 15.0, code: 14.0, small: 13.0, prose_lh: 1.55, code_lh: 1.45 },
];

/// Spacing and shape constants shared by the views.
pub mod metrics {
    /// The chrome showing between tiles, and around them.
    pub const TILE_GAP: f32 = 4.0;
    pub const TILE_RADIUS: f32 = 10.0;
    pub const PAD: f32 = 14.0;
    pub const PAD_SM: f32 = 8.0;
    pub const ROW_H: f32 = 28.0;
    /// Cards and rows; chips, options and key caps; overlays and the dock.
    pub const RADIUS: f32 = 8.0;
    pub const RADIUS_SM: f32 = 6.0;
    pub const RADIUS_LG: f32 = 12.0;
    pub const LEFT_BAR_W: f32 = 260.0;
    pub const RIGHT_BAR_W: f32 = 300.0;
    pub const TAB_BAR_H: f32 = 38.0;
    pub const STATUS_BAR_H: f32 = 28.0;
}

const PALETTES: &[(&str, &str)] = &[
    ("catppuccin", include_str!("../assets/palettes/catppuccin.toml")),
    ("tokyo-night", include_str!("../assets/palettes/tokyo-night.toml")),
    ("catppuccin-latte", include_str!("../assets/palettes/catppuccin-latte.toml")),
    ("flexoki-light", include_str!("../assets/palettes/flexoki-light.toml")),
];

pub fn palette_names() -> Vec<&'static str> {
    PALETTES.iter().map(|(n, _)| *n).collect()
}

pub struct Palette {
    rgb: HashMap<String, Rgb>,
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

fn luminance((r, g, b): Rgb) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `a` moved `p` percent of the way to `b`.
pub fn mix(a: Rgb, b: Rgb, p: u8) -> Rgb {
    let m = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * p as f64 / 100.0).round().clamp(0.0, 255.0) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn hsla((r, g, b): Rgb) -> Hsla {
    rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
}

impl Palette {
    /// `key = "#rrggbb"` lines only, which is all the bundled palettes use.
    pub fn parse(text: &str) -> Option<Self> {
        let mut rgb = HashMap::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            if let Some(c) = parse_hex(v.trim().trim_matches('"')) {
                rgb.insert(k.trim().to_string(), c);
            }
        }
        for (to, from) in [
            ("muted", "dark_foreground"),
            ("dark_foreground", "muted"),
            ("bright_foreground", "foreground"),
            ("selection", "selection_background"),
            ("orange", "yellow"),
        ] {
            if !rgb.contains_key(to)
                && let Some(c) = rgb.get(from).copied()
            {
                rgb.insert(to.into(), c);
            }
        }
        let (bg, fg) = (*rgb.get("background")?, *rgb.get("foreground")?);
        let dark = luminance(bg) < luminance(fg);
        rgb.entry("dark_background".into()).or_insert_with(|| if dark { mix(bg, (0, 0, 0), 22) } else { mix(bg, fg, 6) });
        let lifted = mix(fg, bg, 55);
        let faint = rgb.get("dark_foreground").copied();
        if faint.is_none_or(|c| contrast(c, bg) < 2.4 && contrast(lifted, bg) > contrast(c, bg)) {
            rgb.insert("dark_foreground".into(), lifted);
        }
        Some(Self { rgb })
    }

    fn get(&self, k: &str) -> Rgb {
        self.rgb.get(k).copied().unwrap_or((128, 128, 128))
    }
}

/// Every colour a view may use. The first block is the TUI's roles, computed as `DEFS` does.
#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    pub ty: TypeScale,

    pub strong: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub thinking: Hsla,
    pub line: Hsla,
    pub line_faint: Hsla,
    /// Recessed panel behind cards and the user prompt.
    pub surface: Hsla,
    pub selected: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub ok: Hsla,
    pub warn: Hsla,
    pub err: Hsla,
    pub err_soft: Hsla,
    pub literal: Hsla,
    pub tool: Hsla,
    pub syn_keyword: Hsla,
    pub syn_function: Hsla,
    pub syn_string: Hsla,
    pub syn_number: Hsla,
    pub syn_type: Hsla,
    pub syn_comment: Hsla,
    pub syn_punct: Hsla,
    /// The palette's plain text and background, for the roles that sit between the above.
    pub text: Hsla,

    // GUI-only roles, derived when the palette does not set them.
    /// The outermost layer: the gutter between tiles, behind the tab and status bars.
    pub chrome: Hsla,
    /// A tile's card, and the transcript's background.
    pub window: Hsla,
    /// Side bars: between the chrome and a tile.
    pub bar: Hsla,
    /// Menus, palettes, the dock: surfaces that sit above the window.
    pub raised: Hsla,
    pub hover: Hsla,
    /// The focused tile's edge.
    pub focus_ring: Hsla,
    pub diff_add_bg: Hsla,
    pub diff_add_hi: Hsla,
    pub diff_del_bg: Hsla,
    pub diff_del_hi: Hsla,
    /// Behind overlays.
    pub scrim: Hsla,
}

impl Theme {
    pub fn build(palette: usize, scale: usize) -> Self {
        let (name, text) = PALETTES[palette % PALETTES.len()];
        let pal = Palette::parse(text).expect("bundled palette parses");
        let ty = SCALES[scale % SCALES.len()];
        let bg = pal.get("background");
        let fg = pal.get("foreground");
        let accent = pal.rgb.get("accent").copied().unwrap_or_else(|| pal.get("blue"));
        let dark = luminance(bg) < luminance(fg);
        let h = |c: Rgb| hsla(c);
        let (green, red) = (pal.get("green"), pal.get("red"));
        // The changed part of a line is a stronger shade than the line.
        let (add_bg, add_hi, del_bg, del_hi) = if dark {
            (mix(bg, green, 22), mix(bg, green, 46), mix(bg, red, 22), mix(bg, red, 46))
        } else {
            (mix(bg, green, 20), mix(bg, green, 40), mix(bg, red, 18), mix(bg, red, 36))
        };
        // Layers, from the back: chrome, bar, surface (the prompt, code, cards in the transcript), window (the tile),
        // raised (the dock, composer, menus). The steps are wide on purpose: the TUI's bands read at a glance
        // because its darker and lighter backgrounds sit far apart.
        let black = (0, 0, 0);
        let (window, bar, raised) = (
            pal.rgb.get("window").copied().unwrap_or(bg),
            pal.rgb.get("bar").copied().unwrap_or_else(|| if dark { mix(bg, black, 32) } else { mix(bg, fg, 7) }),
            pal.rgb.get("raised").copied().unwrap_or_else(|| if dark { mix(bg, fg, 13) } else { mix(bg, (255, 255, 255), 70) }),
        );
        let surface = if dark { mix(bg, black, 48) } else { mix(bg, fg, 10) };
        let chrome = pal.rgb.get("chrome").copied().unwrap_or_else(|| if dark { mix(bg, black, 62) } else { mix(bg, fg, 16) });
        // The TUI's text roles, lifted until they clear a contrast floor on every surface they sit on. Glyphs at
        // 12-14 px in a proportional face look thinner than a terminal's, so the TUI's own floor (2.4 for faint
        // text) is too low here.
        let hi = pal.get("bright_foreground");
        let white = (255, 255, 255);
        let surfaces = [window, bar, raised, surface];
        let lift = |mut c: Rgb, min: f64| {
            for _ in 0..24 {
                if surfaces.iter().all(|s| contrast(c, *s) >= min) {
                    break;
                }
                c = mix(c, hi, 8);
            }
            c
        };
        // Dark themes read near-white (the TUI's text looks brighter than its palette's foreground because a terminal
        // draws it heavier); light themes get a step toward black.
        let (text, strong) = if dark {
            (mix(hi, white, 25), mix(hi, white, 60))
        } else {
            (lift(mix(fg, hi, 50), FLOOR_TEXT), hi)
        };
        let (muted, faint) = (lift(mix(text, bg, 30), FLOOR_MUTED), lift(pal.get("dark_foreground"), FLOOR_FAINT));
        Self {
            name,
            dark,
            ty,
            strong: h(strong),
            muted: h(muted),
            faint: h(faint),
            thinking: h(faint),
            line: h(mix(bg, fg, 32)),
            line_faint: h(mix(bg, fg, 20)),
            surface: h(surface),
            selected: h(pal.get("selection")),
            accent: h(accent),
            accent_soft: h(mix(bg, accent, 55)),
            ok: h(green),
            warn: h(pal.get("yellow")),
            err: h(red),
            err_soft: h(mix(bg, red, 55)),
            literal: h(mix(pal.get("cyan"), pal.get("bright_foreground"), 40)),
            tool: h(pal.get("magenta")),
            syn_keyword: h(pal.get("magenta")),
            syn_function: h(pal.get("blue")),
            syn_string: h(green),
            syn_number: h(pal.get("orange")),
            syn_type: h(pal.get("bright_cyan")),
            syn_comment: h(faint),
            syn_punct: h(mix(fg, bg, 20)),
            text: h(text),
            chrome: h(chrome),
            window: h(window),
            bar: h(bar),
            raised: h(raised),
            hover: h(mix(bg, fg, 14)),
            focus_ring: h(accent),
            diff_add_bg: h(add_bg),
            diff_add_hi: h(add_hi),
            diff_del_bg: h(del_bg),
            diff_del_hi: h(del_hi),
            scrim: gpui::hsla(0.0, 0.0, 0.0, if dark { 0.55 } else { 0.35 }),
        }
    }
}

impl Theme {
    /// A shadow for a surface lifted off its background. Dark themes need a heavier one to show at all; light
    /// themes read as paper, so theirs is soft and wide.
    pub fn shadow(&self, level: Lift) -> Vec<BoxShadow> {
        let (alpha, blur, y) = match (self.dark, level) {
            (true, Lift::Rest) => (0.30, 6.0, 1.0),
            (true, Lift::Card) => (0.45, 14.0, 4.0),
            (true, Lift::Float) => (0.60, 36.0, 12.0),
            (false, Lift::Rest) => (0.07, 4.0, 1.0),
            (false, Lift::Card) => (0.12, 14.0, 4.0),
            (false, Lift::Float) => (0.22, 36.0, 12.0),
        };
        vec![BoxShadow {
            color: gpui::hsla(0.0, 0.0, 0.0, alpha),
            offset: point(px(0.), px(y)),
            blur_radius: px(blur),
            spread_radius: px(0.),
            inset: false,
        }]
    }
}

/// How far a surface sits above what is behind it.
#[derive(Clone, Copy)]
pub enum Lift {
    /// A control or an input.
    Rest,
    /// A card: the dock, a focused tile.
    Card,
    /// An overlay.
    Float,
}

/// `Rgba` is what `gpui::rgb` returns; views that need a one-off colour can use it.
#[allow(dead_code)]
pub fn rgba_of(c: Hsla) -> Rgba {
    c.into()
}
