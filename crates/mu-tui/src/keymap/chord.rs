//! A key chord: a key plus Ctrl/Alt/Shift/Super, in one canonical form so that a configured
//! `ctrl+T`, a terminal's `Char('t')` + CONTROL and the kitty protocol's report all compare equal.
//!
//! Canonical form: a shifted character is the character itself (`Shift+k` is `K`, SHIFT dropped);
//! Shift+Tab is `BackTab`, SHIFT dropped. Every other key keeps SHIFT.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::CHORD_MODS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Chord {
    fn canonical(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods & CHORD_MODS;
        let code = match code {
            KeyCode::Char(c) => {
                let shifted = mods.contains(KeyModifiers::SHIFT);
                mods.remove(KeyModifiers::SHIFT);
                let mut up = c.to_uppercase();
                match (shifted, up.next(), up.next()) {
                    (true, Some(u), None) => KeyCode::Char(u),
                    _ => KeyCode::Char(c),
                }
            }
            KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            c => c,
        };
        Self { code, mods }
    }

    pub fn of_event(k: &KeyEvent) -> Self {
        Self::canonical(k.code, k.modifiers)
    }

    /// `ctrl+shift+a`, `alt+enter`, `f5`, `?`: modifiers joined by `+` (case-insensitive), then one
    /// key: a named key (`enter`, `esc`, `tab`, `space`, `up`, `pageup`, `plus`, `f1`..`f24`, ...)
    /// or one character.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (mods_part, key) = if s == "+" {
            ("", "+")
        } else if let Some(m) = s.strip_suffix("++") {
            (m, "+")
        } else {
            match s.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", s),
            }
        };
        if key.is_empty() {
            return Err(format!("`{s}` names no key"));
        }
        let mut mods = KeyModifiers::NONE;
        if !mods_part.is_empty() {
            for m in mods_part.split('+') {
                mods |= match m.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => KeyModifiers::CONTROL,
                    "alt" | "opt" | "option" | "meta" => KeyModifiers::ALT,
                    "shift" => KeyModifiers::SHIFT,
                    "super" | "cmd" | "win" => KeyModifiers::SUPER,
                    other => return Err(format!("`{other}` in `{s}` is not a modifier (ctrl, alt, shift, super)")),
                };
            }
        }
        let code = match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "space" => KeyCode::Char(' '),
            "plus" => KeyCode::Char('+'),
            f if f.len() > 1 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
                match f[1..].parse::<u8>() {
                    Ok(n @ 1..=24) => KeyCode::F(n),
                    _ => return Err(format!("`{key}` in `{s}` is not a function key (f1..f24)")),
                }
            }
            _ => {
                let mut cs = key.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) => {
                        if mods.contains(KeyModifiers::SHIFT) && !c.is_alphabetic() {
                            return Err(format!("`{s}`: write the shifted character itself, not shift+{c}"));
                        }
                        // `ctrl+T` names the key T, not a shifted one; only a bare `K` is shifted.
                        let held = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                        let unshifted = !mods.contains(KeyModifiers::SHIFT);
                        KeyCode::Char(if held && unshifted { c.to_lowercase().next().unwrap_or(c) } else { c })
                    }
                    _ => return Err(format!("`{key}` in `{s}` is not a key name or a single character")),
                }
            }
        };
        Ok(Self::canonical(code, mods))
    }

    /// As shown in a hint: `Ctrl+T`, `Alt+Enter`, `Shift+Tab`, `F5`.
    pub fn label(&self) -> String {
        let mut out = String::new();
        let mut mods = self.mods;
        if self.code == KeyCode::BackTab {
            mods |= KeyModifiers::SHIFT;
        }
        for (m, n) in [
            (KeyModifiers::CONTROL, "Ctrl"),
            (KeyModifiers::ALT, "Alt"),
            (KeyModifiers::SHIFT, "Shift"),
            (KeyModifiers::SUPER, "Super"),
        ] {
            if mods.contains(m) {
                out.push_str(n);
                out.push('+');
            }
        }
        let key = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) if self.mods.contains(KeyModifiers::CONTROL) => c.to_uppercase().collect(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::F(n) => format!("F{n}"),
            other => format!("{other:?}"),
        };
        out.push_str(&key);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> Chord {
        Chord::of_event(&KeyEvent::new(code, mods))
    }

    #[test]
    fn parses_modifiers_names_and_characters() {
        assert_eq!(Chord::parse("ctrl+t").unwrap(), ev(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(Chord::parse("Ctrl+T").unwrap(), Chord::parse("ctrl+t").unwrap());
        assert_eq!(Chord::parse("alt+enter").unwrap(), ev(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(Chord::parse("shift+tab").unwrap(), ev(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(Chord::parse("shift+tab").unwrap(), ev(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(Chord::parse("ctrl+alt+up").unwrap(), ev(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(Chord::parse("f5").unwrap(), ev(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(Chord::parse("space").unwrap(), ev(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(Chord::parse("?").unwrap(), ev(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(Chord::parse("+").unwrap(), ev(KeyCode::Char('+'), KeyModifiers::NONE));
        assert_eq!(Chord::parse("ctrl++").unwrap(), ev(KeyCode::Char('+'), KeyModifiers::CONTROL));
        assert_eq!(Chord::parse("ctrl+plus").unwrap(), ev(KeyCode::Char('+'), KeyModifiers::CONTROL));
    }

    #[test]
    fn a_shifted_character_is_the_character() {
        assert_eq!(Chord::parse("shift+k").unwrap(), Chord::parse("K").unwrap());
        assert_eq!(Chord::parse("K").unwrap(), ev(KeyCode::Char('k'), KeyModifiers::SHIFT));
        assert_eq!(Chord::parse("K").unwrap(), ev(KeyCode::Char('K'), KeyModifiers::NONE));
        assert!(Chord::parse("shift+1").is_err());
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        for bad in ["", "ctrl+", "ctrl+bogus", "hyper+a", "f0", "f25", "ab"] {
            assert!(Chord::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels_read_like_the_docs() {
        for (spec, label) in [
            ("ctrl+t", "Ctrl+T"),
            ("alt+enter", "Alt+Enter"),
            ("shift+tab", "Shift+Tab"),
            ("ctrl+shift+up", "Ctrl+Shift+Up"),
            ("space", "Space"),
            ("f5", "F5"),
            ("q", "q"),
        ] {
            assert_eq!(Chord::parse(spec).unwrap().label(), label, "{spec}");
        }
    }
}
