//! `/theme`: the themes to pick from. The screen takes the highlighted theme as the highlight moves;
//! Enter keeps it, anything else puts the old one back.

use crate::theme::{self, Active, Entry, Origin};
use crate::widgets::picker::{HasSelect, Pick, Picker};
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;

pub struct ThemePicker {
    pick: Picker<String>,
    /// The theme in use when the picker opened. Put back when the picker goes away without a choice,
    /// by whatever route (`Drop`), so a preview never outlives the picker.
    original: Option<Active>,
}

pub enum ThemeAction {
    Pending,
    /// Enter on this source: the caller keeps it and saves it.
    Chosen(String),
    Close,
}

fn detail(e: &Entry, current: bool, live_terminal: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    match e.dark {
        Some(true) => parts.push("dark"),
        Some(false) => parts.push("light"),
        None => {}
    }
    match e.origin {
        Origin::Bundled => {}
        Origin::Custom => parts.push("~/.mu/themes"),
        Origin::Terminal if live_terminal => parts.push("your terminal's colours"),
        Origin::Terminal => parts.push("your terminal's colours, read at start-up"),
        Origin::Omarchy => parts.push("omarchy's current theme"),
    }
    if current {
        parts.push("current");
    }
    parts.join(" · ")
}

impl ThemePicker {
    pub fn new() -> Self {
        Self::with(theme::available(), theme::active())
    }

    fn with(entries: Vec<Entry>, original: Active) -> Self {
        let current = entries.iter().position(|e| e.name == original.source());
        let live_terminal = original.source() == "terminal";
        let rows = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                (Item { label: e.name.clone(), detail: detail(&e, current == Some(i), live_terminal) }, e.name)
            })
            .collect();
        let pick = Picker::from_rows(rows, |items| {
            let s = Select::new("Theme", items).hint("↑↓ preview · Enter apply and save · Esc cancel");
            match current {
                Some(i) => s.preselect(i),
                None => s,
            }
        });
        Self { pick, original: Some(original) }
    }

    fn put_back(&self) {
        if let Some(o) = &self.original {
            theme::restore(o);
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> ThemeAction {
        let before = self.pick.highlighted().cloned();
        match self.pick.on_key(k) {
            Pick::Pending | Pick::Chosen(None) => {
                let now = self.pick.highlighted().cloned();
                if now != before
                    && let Some(source) = now
                    && theme::switch(&source).is_err()
                {
                    self.put_back();
                }
                ThemeAction::Pending
            }
            Pick::Chosen(Some(source)) => {
                self.original = None;
                ThemeAction::Chosen(source)
            }
            Pick::Dismissed => {
                self.put_back();
                ThemeAction::Close
            }
        }
    }
}

impl Drop for ThemePicker {
    fn drop(&mut self) {
        self.put_back();
    }
}

impl HasSelect for ThemePicker {
    fn sel(&self) -> &Select {
        self.pick.sel()
    }

    fn sel_mut(&mut self) -> &mut Select {
        self.pick.sel_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use mu_testutil::key;

    fn entry(name: &str, origin: Origin, dark: Option<bool>) -> Entry {
        Entry { name: name.into(), origin, dark }
    }

    #[test]
    fn rows_say_what_each_theme_is_and_the_current_one_is_highlighted() {
        let entries = vec![
            entry("tokyo-night", Origin::Bundled, Some(true)),
            entry("white", Origin::Bundled, Some(false)),
            entry("mine", Origin::Custom, Some(true)),
            entry("terminal", Origin::Terminal, None),
        ];
        let p = ThemePicker::with(entries, theme::active());
        let items = p.sel().render(80, 12).0;
        let text: Vec<String> = items.iter().map(crate::text::line::line_text).collect();
        let row = |name: &str| text.iter().find(|l| l.contains(name)).unwrap_or_else(|| panic!("{name} in {text:?}"));
        assert!(row("tokyo-night").contains("dark · current"), "{text:?}");
        assert!(row("white").contains("light"), "{text:?}");
        assert!(row("mine").contains("~/.mu/themes"), "{text:?}");
        assert!(row("terminal").contains("read at start-up"), "{text:?}");
        assert_eq!(p.pick.highlighted().map(String::as_str), Some("tokyo-night"));
    }

    #[test]
    fn enter_chooses_the_highlighted_theme_and_escape_closes() {
        let entries =
            vec![entry("tokyo-night", Origin::Bundled, Some(true)), entry("white", Origin::Bundled, Some(false))];
        let mut p = ThemePicker::with(entries.clone(), theme::active());
        assert!(matches!(p.on_key(key(KeyCode::Enter)), ThemeAction::Chosen(s) if s == "tokyo-night"));
        assert!(p.original.is_none(), "a choice is not put back");
        let mut p = ThemePicker::with(entries, theme::active());
        assert!(matches!(p.on_key(key(KeyCode::Esc)), ThemeAction::Close));
    }

    #[test]
    fn the_bundled_palettes_are_listed_with_their_tone() {
        let all = theme::available();
        let find = |n: &str| all.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("{n} is listed"));
        assert_eq!(find("tokyo-night").dark, Some(true));
        assert_eq!(find("catppuccin-latte").dark, Some(false));
        assert_eq!(find("terminal").origin, Origin::Terminal);
        assert!(all.iter().position(|e| e.name == "terminal") > all.iter().position(|e| e.name == "white"));
    }

    #[test]
    fn a_source_that_cannot_be_applied_is_an_error_and_changes_nothing() {
        let before = theme::active();
        assert!(theme::switch("no-such-palette").is_err());
        assert!(theme::switch("terminal").unwrap_err().contains("start-up"));
        assert_eq!(theme::active().source(), before.source());
    }
}
