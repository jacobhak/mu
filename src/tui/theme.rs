//! Semantic colour roles derived from a palette: the terminal's own (OSC probe) or an omarchy-style `colors.toml`.
use super::probe::{self, Answers, Rgb};
use crossterm::style::{Attribute, Color, ContentStyle};
use mu_base::config::{Cards, MuTheme};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

const PALETTES: &[(&str, &str)] = &[
    ("catppuccin", include_str!("palettes/catppuccin.toml")),
    ("catppuccin-latte", include_str!("palettes/catppuccin-latte.toml")),
    ("ethereal", include_str!("palettes/ethereal.toml")),
    ("everforest", include_str!("palettes/everforest.toml")),
    ("flexoki-light", include_str!("palettes/flexoki-light.toml")),
    ("gruvbox", include_str!("palettes/gruvbox.toml")),
    ("hackerman", include_str!("palettes/hackerman.toml")),
    ("jade-horse", include_str!("palettes/jade-horse.toml")),
    ("kanagawa", include_str!("palettes/kanagawa.toml")),
    ("last-horizon", include_str!("palettes/last-horizon.toml")),
    ("lumon", include_str!("palettes/lumon.toml")),
    ("lupine", include_str!("palettes/lupine.toml")),
    ("matte-black", include_str!("palettes/matte-black.toml")),
    ("miasma", include_str!("palettes/miasma.toml")),
    ("nord", include_str!("palettes/nord.toml")),
    ("retro-82", include_str!("palettes/retro-82.toml")),
    ("ristretto", include_str!("palettes/ristretto.toml")),
    ("rose-pine", include_str!("palettes/rose-pine.toml")),
    ("solitude", include_str!("palettes/solitude.toml")),
    ("starship", include_str!("palettes/starship.toml")),
    ("tokyo-night", include_str!("palettes/tokyo-night.toml")),
    ("vantablack", include_str!("palettes/vantablack.toml")),
    ("white", include_str!("palettes/white.toml")),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Strong,
    Muted,
    Faint,
    Thinking,
    Line,
    LineFaint,
    /// Recessed panel behind cards and the user prompt: the palette's `dark_background`.
    Surface,
    Selected,
    Accent,
    AccentSoft,
    Ok,
    Warn,
    Err,
    ErrSoft,
    Literal,
    Tool,
    SynKeyword,
    SynFunction,
    SynString,
    SynNumber,
    SynType,
    SynComment,
    SynPunct,
}

const ROLE_COUNT: usize = Role::SynPunct as usize + 1;

#[derive(Clone, Copy)]
enum Fb {
    Slot(u8),
    Dim,
    None,
}

#[derive(Clone, Copy)]
enum Def {
    Key(&'static str, Fb),
    Mix(&'static str, &'static str, u8, Fb),
}

use Def::{Key, Mix};
use Fb::{Dim, None as No, Slot};

/// Indexed by `Role as usize`. `"accent"` is the configured accent, not a palette key.
const DEFS: [Def; ROLE_COUNT] = [
    Key("bright_foreground", Slot(15)),
    Mix("foreground", "background", 35, Dim),
    Key("dark_foreground", Slot(8)),
    Key("dark_foreground", Slot(8)),
    Mix("background", "foreground", 22, Slot(8)),
    Mix("background", "foreground", 13, Slot(8)),
    Key("dark_background", No),
    Key("selection", Slot(8)),
    Key("accent", Slot(4)),
    Mix("background", "accent", 55, Slot(4)),
    Key("green", Slot(2)),
    Key("yellow", Slot(3)),
    Key("red", Slot(1)),
    Mix("background", "red", 55, Slot(1)),
    Mix("cyan", "bright_foreground", 40, Slot(6)),
    Key("magenta", Slot(5)),
    Key("magenta", Slot(5)),
    Key("blue", Slot(4)),
    Key("green", Slot(2)),
    Key("orange", Slot(11)),
    Key("bright_cyan", Slot(14)),
    Key("dark_foreground", Slot(8)),
    Mix("foreground", "background", 20, No),
];

/// Palette keys the terminal answers by slot. `muted` (slot 8) is read but not emitted by slot:
/// many terminal themes make it too dark for text, so `dark_foreground` is derived instead.
const SLOTS: &[(&str, u8)] = &[
    ("red", 1),
    ("green", 2),
    ("yellow", 3),
    ("blue", 4),
    ("magenta", 5),
    ("cyan", 6),
    ("foreground", 7),
    ("muted", 8),
    ("bright_red", 9),
    ("bright_green", 10),
    ("bright_yellow", 11),
    ("bright_blue", 12),
    ("bright_magenta", 13),
    ("bright_cyan", 14),
    ("bright_foreground", 15),
    ("orange", 11),
    ("accent", 4),
];

/// Text below this contrast with the background is hard to read; `dark_foreground` is lifted to it.
const FAINT_MIN_CONTRAST: f64 = 2.4;

fn slot_of(key: &str) -> Option<u8> {
    SLOTS.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

pub struct Palette {
    rgb: HashMap<String, Rgb>,
    by_slot: bool,
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

impl Palette {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let t: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut rgb: HashMap<String, Rgb> = HashMap::new();
        for (k, v) in &t {
            if let Some(c) = v.as_str().and_then(parse_hex) {
                rgb.insert(k.clone(), c);
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
        for k in ["background", "foreground"] {
            if !rgb.contains_key(k) {
                return Err(format!("missing {k}"));
            }
        }
        Ok(Self { rgb, by_slot: false }.completed())
    }

    pub fn from_answers(a: &Answers) -> Self {
        let mut rgb = HashMap::new();
        let mut put = |k: &str, v: Option<Rgb>| {
            if let Some(c) = v {
                rgb.insert(k.to_string(), c);
            }
        };
        put("foreground", a.fg);
        put("background", a.bg);
        put("selection", a.selection);
        put("dark_foreground", a.slots[8]);
        for (k, n) in SLOTS {
            if *k != "foreground" && *k != "accent" {
                put(k, a.slots[*n as usize]);
            }
        }
        Self { rgb, by_slot: true }.completed()
    }

    /// Fills what the theme needs and the source may lack: a recessed `dark_background` (darker on
    /// dark themes, a step toward the text on light ones) and a `dark_foreground` that is legible.
    fn completed(mut self) -> Self {
        let (Some(bg), Some(fg)) = (self.rgb.get("background").copied(), self.rgb.get("foreground").copied()) else {
            return self;
        };
        let dark = luminance(bg) < luminance(fg);
        self.rgb
            .entry("dark_background".into())
            .or_insert_with(|| if dark { mix(bg, (0, 0, 0), 22) } else { mix(bg, fg, 6) });
        let lifted = mix(fg, bg, 55);
        let faint = self.rgb.get("dark_foreground").copied();
        if faint.is_none_or(|c| contrast(c, bg) < FAINT_MIN_CONTRAST && contrast(lifted, bg) > contrast(c, bg)) {
            self.rgb.insert("dark_foreground".into(), lifted);
        }
        self
    }
}

/// WCAG relative luminance.
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

fn load_palette(source: &str) -> Result<Palette, String> {
    let err = |e: String| format!("theme: {source}: {e}");
    let file = |p: &std::path::Path| -> Result<Palette, String> {
        let text = std::fs::read_to_string(p).map_err(|e| err(format!("{}: {e}", p.display())))?;
        Palette::from_toml(&text).map_err(err)
    };
    if source == "terminal" {
        return Ok(Palette::from_answers(&probe::query(Duration::from_millis(1000))));
    }
    if source == "omarchy" {
        return file(&mu_common::paths::home().join(".local/state/omarchy/current/theme/colors.toml"));
    }
    if source.contains('/') || source.ends_with(".toml") {
        let p = match source.strip_prefix("~/") {
            Some(rest) => mu_common::paths::home().join(rest),
            None => source.into(),
        };
        return file(&p);
    }
    let user = mu_common::paths::mu_home().join("themes").join(format!("{source}.toml"));
    if user.exists() {
        return file(&user);
    }
    match PALETTES.iter().find(|(k, _)| *k == source) {
        Some((_, text)) => Palette::from_toml(text).map_err(err),
        None => Err(err("unknown palette".into())),
    }
}

pub fn truecolor_terminal() -> bool {
    let v = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let ct = v("COLORTERM");
    ct.contains("truecolor")
        || ct.contains("24bit")
        || v("TERM").contains("direct")
        || std::env::var_os("WT_SESSION").is_some()
}

pub struct Theme {
    colors: [Option<Color>; ROLE_COUNT],
    dim: [bool; ROLE_COUNT],
    cards: Cards,
}

static THEME: OnceLock<Theme> = OnceLock::new();
/// `init` ran. Reading the theme earlier would pin the bundled default for the whole run.
static INIT_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn bundled_default() -> Palette {
    Palette::from_toml(PALETTES.iter().find(|(k, _)| *k == "tokyo-night").map_or("", |p| p.1))
        .unwrap_or(Palette { rgb: HashMap::new(), by_slot: false })
}

/// The active theme; bundled tokyo-night, truecolor, outline cards before `init` (tests only).
pub fn get() -> &'static Theme {
    debug_assert!(
        cfg!(test) || INIT_DONE.load(std::sync::atomic::Ordering::Relaxed),
        "theme::get() called before theme::init()"
    );
    THEME.get_or_init(|| Theme::build(&bundled_default(), None, true, Cards::Outline).0)
}

/// Loads the configured palette (probing the terminal if asked) and installs the theme. Returns warnings.
pub fn init(cfg: &MuTheme) -> Vec<String> {
    let mut warnings = vec![];
    let pal = match load_palette(&cfg.source) {
        Ok(p) => p,
        Err(e) => {
            // A bad name or file falls back to the bundled default at once. Only `theme = "terminal"`
            // asks the terminal (and waits for its answer).
            warnings.push(format!("{e}; using the bundled tokyo-night palette"));
            bundled_default()
        }
    };
    let (t, w) = Theme::build(&pal, cfg.accent.as_deref(), truecolor_terminal(), cfg.cards);
    warnings.extend(w);
    INIT_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    if THEME.set(t).is_err() {
        warnings.push("theme: it was read before it was configured, so the configured theme is ignored".into());
    }
    warnings
}

fn mix(a: Rgb, b: Rgb, p: u8) -> Rgb {
    let m = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * p as f64 / 100.0).round().clamp(0.0, 255.0) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

struct Accent {
    key: Option<String>,
    rgb: Option<Rgb>,
    slot: u8,
}

impl Theme {
    pub fn build(pal: &Palette, accent: Option<&str>, truecolor: bool, cards: Cards) -> (Self, Vec<String>) {
        let mut warnings = vec![];
        let known = |k: &str| pal.rgb.contains_key(k) || slot_of(k).is_some();
        let default_key = if !pal.by_slot && pal.rgb.contains_key("accent") { "accent" } else { "blue" };
        let acc = match accent {
            Some(v) => match parse_hex(v) {
                Some(c) => Accent { key: None, rgb: Some(c), slot: 4 },
                None if known(v) => Accent { key: Some(v.into()), rgb: None, slot: slot_of(v).unwrap_or(4) },
                None => {
                    warnings.push(format!("theme: accent {v} is not a palette key or #rrggbb"));
                    Accent { key: Some(default_key.into()), rgb: None, slot: slot_of(default_key).unwrap_or(4) }
                }
            },
            None => Accent { key: Some(default_key.into()), rgb: None, slot: slot_of(default_key).unwrap_or(4) },
        };
        let lookup = |k: &str| -> Option<Rgb> {
            if k == "accent" {
                return acc.rgb.or_else(|| {
                    let key = acc.key.as_deref()?;
                    pal.rgb
                        .get(key)
                        .copied()
                        .or_else(|| if key == "accent" { pal.rgb.get("blue").copied() } else { None })
                });
            }
            pal.rgb.get(k).copied()
        };
        let mut colors = [None; ROLE_COUNT];
        let mut dim = [false; ROLE_COUNT];
        for (i, def) in DEFS.iter().enumerate() {
            let fb = |f: Fb, accent_slot: bool| match f {
                Slot(n) => Some(Color::AnsiValue(if accent_slot { acc.slot } else { n })),
                _ => None,
            };
            let is_accent = matches!(def, Key("accent", _) | Mix(_, "accent", _, _));
            let (fallback, c) = match *def {
                Key(k, f) => {
                    let slot = if k == "accent" { acc.key.as_deref().and_then(slot_of) } else { slot_of(k) };
                    let c = if !truecolor {
                        None
                    } else {
                        match (pal.by_slot, slot) {
                            (true, Some(n)) => Some(Color::AnsiValue(n)),
                            _ => lookup(k).map(|(r, g, b)| Color::Rgb { r, g, b }),
                        }
                    };
                    (f, c)
                }
                Mix(a, b, p, f) => {
                    let c = if !truecolor {
                        None
                    } else {
                        match (lookup(a), lookup(b)) {
                            (Some(x), Some(y)) => {
                                let (r, g, b) = mix(x, y, p);
                                Some(Color::Rgb { r, g, b })
                            }
                            _ => None,
                        }
                    };
                    (f, c)
                }
            };
            let used_fallback = c.is_none();
            colors[i] = c.or_else(|| fb(fallback, is_accent));
            dim[i] = used_fallback && matches!(fallback, Dim);
        }
        (Self { colors, dim, cards }, warnings)
    }

    /// `None` means the terminal default.
    pub fn color(&self, r: Role) -> Option<Color> {
        self.colors[r as usize]
    }

    pub fn fg(&self, r: Role) -> ContentStyle {
        self.apply(r, ContentStyle::default())
    }

    /// `s` with its foreground replaced by the role's, and Dim added where the role is SGR-dim.
    pub fn apply(&self, r: Role, mut s: ContentStyle) -> ContentStyle {
        s.foreground_color = self.color(r);
        if self.dim[r as usize] {
            s.attributes.set(Attribute::Dim);
        }
        s
    }

    pub fn cards(&self) -> Cards {
        self.cards
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokyo() -> Palette {
        Palette::from_toml(PALETTES.iter().find(|(k, _)| *k == "tokyo-night").unwrap().1).unwrap()
    }
    fn rgb(h: &str) -> Option<Color> {
        let (r, g, b) = parse_hex(h).unwrap();
        Some(Color::Rgb { r, g, b })
    }

    #[test]
    fn tokyo_night_truecolor_roles() {
        let (t, w) = Theme::build(&tokyo(), None, true, Cards::Outline);
        assert!(w.is_empty());
        assert_eq!(t.color(Role::Muted), rgb("#777d98"));
        assert_eq!(t.color(Role::Line), rgb("#393c4d"));
        assert_eq!(t.color(Role::Surface), rgb("#13141c"));
        assert_eq!(t.color(Role::Thinking), rgb("#565f89"));
        assert_eq!(t.color(Role::Tool), rgb("#ad8ee6"));
        assert_eq!(t.color(Role::Faint), rgb("#565f89"), "the palette's own dark_foreground is legible");
        assert_eq!(t.color(Role::AccentSoft), rgb("#4f6599"));
        assert_eq!(t.color(Role::Accent), rgb("#7aa2f7"));
        assert!(t.fg(Role::Muted).attributes.is_empty());
    }

    #[test]
    fn ansi_tier_uses_slots_and_dim() {
        let (t, _) = Theme::build(&tokyo(), None, false, Cards::Outline);
        assert_eq!(t.color(Role::Strong), Some(Color::AnsiValue(15)));
        assert_eq!(t.color(Role::Muted), None);
        assert!(t.fg(Role::Muted).attributes.has(Attribute::Dim));
        assert_eq!(t.color(Role::Surface), None);
        assert_eq!(t.color(Role::Accent), Some(Color::AnsiValue(4)));
        assert_eq!(t.color(Role::Thinking), Some(Color::AnsiValue(8)));
        assert_eq!(t.color(Role::Tool), Some(Color::AnsiValue(5)));
    }

    #[test]
    fn terminal_with_only_fg_bg() {
        let a = Answers { fg: Some((169, 177, 214)), bg: Some((26, 27, 38)), ..Default::default() };
        let (t, _) = Theme::build(&Palette::from_answers(&a), None, true, Cards::Outline);
        assert_eq!(t.color(Role::Ok), Some(Color::AnsiValue(2)));
        assert!(matches!(t.color(Role::Line), Some(Color::Rgb { .. })));
        assert_eq!(t.color(Role::AccentSoft), Some(Color::AnsiValue(4)));
        assert_eq!(t.color(Role::Surface), rgb("#14151e"), "derived: background toward black");
        assert_eq!(t.color(Role::Faint), rgb("#5a5f75"), "derived: foreground toward background");
    }

    #[test]
    fn terminal_faint_is_slot_8_only_when_legible() {
        let mut a = Answers { fg: Some((192, 202, 245)), bg: Some((26, 27, 38)), ..Default::default() };
        a.slots[8] = Some((0x41, 0x48, 0x68)); // Ghostty TokyoNight: 1.9:1 on the background
        let (t, _) = Theme::build(&Palette::from_answers(&a), None, true, Cards::Outline);
        assert!(t.color(Role::Faint) != rgb("#414868") && matches!(t.color(Role::Faint), Some(Color::Rgb { .. })));
        a.slots[8] = Some((0x73, 0x7a, 0xa2));
        let (t, _) = Theme::build(&Palette::from_answers(&a), None, true, Cards::Outline);
        assert_eq!(t.color(Role::Faint), rgb("#737aa2"));
    }

    #[test]
    fn terminal_with_all_slots() {
        let mut a = Answers { fg: Some((169, 177, 214)), bg: Some((26, 27, 38)), ..Default::default() };
        for (i, s) in a.slots.iter_mut().enumerate().take(16) {
            *s = Some((i as u8 * 10, 100, 50));
        }
        let (t, _) = Theme::build(&Palette::from_answers(&a), None, true, Cards::Outline);
        assert!(matches!(t.color(Role::Surface), Some(Color::Rgb { .. })));
        assert_eq!(t.color(Role::Ok), Some(Color::AnsiValue(2)));
        assert_eq!(t.color(Role::Accent), Some(Color::AnsiValue(4)));
    }

    #[test]
    fn every_bundled_palette_loads() {
        for (name, text) in PALETTES {
            let p = Palette::from_toml(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let (t, w) = Theme::build(&p, None, true, Cards::Outline);
            assert!(w.is_empty(), "{name}");
            assert!(t.color(Role::Surface).is_some(), "{name}");
        }
    }

    #[test]
    fn bad_sources_are_errors() {
        assert!(load_palette("nope").is_err());
        assert!(Palette::from_toml("foreground = \"#ffffff\"").is_err());
        assert!(load_palette("/nonexistent/colors.toml").is_err());
    }

    #[test]
    fn accent_key_hex_and_invalid() {
        let p = tokyo();
        let (t, _) = Theme::build(&p, Some("magenta"), true, Cards::Outline);
        assert_eq!(t.color(Role::Accent), rgb("#ad8ee6"));
        let (t, _) = Theme::build(&p, Some("magenta"), false, Cards::Outline);
        assert_eq!(t.color(Role::Accent), Some(Color::AnsiValue(5)));
        let (t, _) = Theme::build(&p, Some("#112233"), true, Cards::Outline);
        assert_eq!(t.color(Role::Accent), rgb("#112233"));
        let (t, w) = Theme::build(&p, Some("bogus"), true, Cards::Outline);
        assert_eq!(w, vec!["theme: accent bogus is not a palette key or #rrggbb"]);
        assert_eq!(t.color(Role::Accent), rgb("#7aa2f7"));
    }

    #[test]
    fn an_unknown_palette_falls_back_without_probing_the_terminal() {
        let started = std::time::Instant::now();
        let err = load_palette("no-such-palette").err().expect("unknown palette is an error");
        assert!(err.contains("unknown palette"));
        assert!(started.elapsed() < Duration::from_millis(500), "no terminal probe for a name lookup");
        assert!(bundled_default().rgb.contains_key("background") || !bundled_default().rgb.is_empty());
    }
}
