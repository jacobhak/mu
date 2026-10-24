//! omp-compatible themes: `vars` + `colors` JSON, resolved to terminal colours.
use super::theme_data::THEMES;
use crossterm::style::{Color, ContentStyle};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

pub struct Theme {
    colors: HashMap<String, Option<Color>>,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// The active theme; the built-in `dark` before `init`.
pub fn get() -> &'static Theme {
    THEME.get_or_init(|| Theme::load("dark", Path::new("")))
}

pub fn init(name: &str, agent_dir: &Path) {
    let _ = THEME.set(Theme::load(name, agent_dir));
}

fn truecolor_terminal() -> bool {
    let v = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let ct = v("COLORTERM");
    ct.contains("truecolor") || ct.contains("24bit") || v("TERM").contains("direct") || std::env::var_os("WT_SESSION").is_some()
}

/// Nearest xterm-256 entry: best of the 6x6x6 cube and the 24-step grey ramp.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] - v as i32).abs()).unwrap_or(0);
    let (ci, cj, ck) = (near(r), near(g), near(b));
    let dist = |c: [i32; 3]| {
        let d = |a: i32, b: u8| (a - b as i32).pow(2);
        d(c[0], r) + d(c[1], g) + d(c[2], b)
    };
    let cube = dist([LEVELS[ci], LEVELS[cj], LEVELS[ck]]);
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gv = 8 + 10 * gi;
    if dist([gv; 3]) < cube { 232 + gi as u8 } else { (16 + 36 * ci + 6 * cj + ck) as u8 }
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

impl Theme {
    /// `<agent_dir>/themes/<name>.json`, then the embedded omp themes, then embedded `dark`.
    pub fn load(name: &str, agent_dir: &Path) -> Self {
        let custom = std::fs::read_to_string(agent_dir.join("themes").join(format!("{name}.json"))).ok();
        let embedded = |n: &str| THEMES.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string());
        let (_, text) = match custom.map(|t| (name, t)).or_else(|| embedded(name).map(|t| (name, t))) {
            Some(x) => x,
            None => ("dark", embedded("dark").unwrap_or_default()),
        };
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Self::from_json(&json, truecolor_terminal())
    }

    pub fn from_json(json: &Value, truecolor: bool) -> Self {
        let vars = json["vars"].as_object().cloned().unwrap_or_default();
        let mut colors = HashMap::new();
        if let Some(c) = json["colors"].as_object() {
            for (k, v) in c {
                colors.insert(k.clone(), Self::resolve(v, &vars, truecolor, 0));
            }
        }
        Self { colors }
    }

    fn resolve(v: &Value, vars: &serde_json::Map<String, Value>, truecolor: bool, depth: u8) -> Option<Color> {
        if depth > 8 {
            return None;
        }
        match v {
            Value::Number(n) => n.as_u64().map(|n| Color::AnsiValue(n.min(255) as u8)),
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => match parse_hex(s) {
                Some((r, g, b)) if truecolor => Some(Color::Rgb { r, g, b }),
                Some((r, g, b)) => Some(Color::AnsiValue(rgb_to_256(r, g, b))),
                None => vars.get(s).and_then(|v| Self::resolve(v, vars, truecolor, depth + 1)),
            },
            _ => None,
        }
    }

    /// `None` means the terminal default (or an unknown key).
    pub fn color(&self, key: &str) -> Option<Color> {
        self.colors.get(key).copied().flatten()
    }

    pub fn fg(&self, key: &str) -> ContentStyle {
        ContentStyle { foreground_color: self.color(key), ..Default::default() }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vars_numbers_and_defaults() {
        let j: Value = serde_json::from_str(
            r##"{"vars":{"a":"#102030","b":"a","n":238},"colors":{"x":"b","y":"","z":"n","w":"nope","h":"#ffffff"}}"##,
        )
        .unwrap();
        let t = Theme::from_json(&j, true);
        assert_eq!(t.color("x"), Some(Color::Rgb { r: 0x10, g: 0x20, b: 0x30 }));
        assert_eq!((t.color("y"), t.color("w"), t.color("missing")), (None, None, None));
        assert_eq!(t.color("z"), Some(Color::AnsiValue(238)));
        let t = Theme::from_json(&j, false);
        assert_eq!(t.color("h"), Some(Color::AnsiValue(231)));
        assert!(matches!(t.color("x"), Some(Color::AnsiValue(_))));
    }

    #[test]
    fn dark_near_greys_map_to_the_grey_ramp() {
        assert!((232..=237).contains(&rgb_to_256(0x16, 0x16, 0x1e)));
        assert!((232..=237).contains(&rgb_to_256(0x1a, 0x1e, 0x2e)));
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
    }

    #[test]
    fn every_embedded_theme_resolves_core_keys() {
        for (name, text) in THEMES {
            let j: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let t = Theme::from_json(&j, true);
            for k in ["toolPendingBg", "toolSuccessBg", "mdHeading", "syntaxKeyword"] {
                assert!(t.colors.contains_key(k), "{name} lacks {k}");
            }
        }
    }
}
