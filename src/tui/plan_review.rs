use super::line::{self, Line, dim, span, styled};
use super::markdown;
use super::select::clean_paste;
use super::status::format_number;
use super::theme::{self, Role};
use crate::events::PlanDecision;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::ContentStyle;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

pub struct Tier {
    pub role: String,
    pub detail: String,
    pub model: Option<String>,
}

pub enum ReviewResult {
    Pending,
    Cancelled,
    Copy,
    Edit,
    Decided(PlanDecision),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    Body,
    Actions,
}

pub struct PlanReview {
    pub file: PathBuf,
    title: String,
    uri: String,
    content: String,
    lines: Vec<Line>,
    cache_w: usize,
    scroll: usize,
    body_h: usize,
    focus: Focus,
    actions: [(String, bool); 4],
    sel: usize,
    tiers: Vec<Tier>,
    tier: usize,
    editing: bool,
    text: String,
}

fn accent_bold() -> ContentStyle {
    let mut s = theme::get().fg(Role::Accent);
    s.attributes.set(crossterm::style::Attribute::Bold);
    s
}

impl PlanReview {
    pub fn new(title: String, uri: String, file: PathBuf, content: String, context: Option<(u64, u64)>, tiers: Vec<Tier>) -> Self {
        let (keep, keep_off) = match context {
            Some((t, w)) => (format!("Approve and keep context (~{} / {})", format_number(t), format_number(w)), t * 100 > 95 * w),
            None => ("Approve and keep context".to_string(), false),
        };
        let tier = tiers.iter().position(|t| t.role == "default").unwrap_or(0);
        Self {
            file,
            title,
            uri,
            content,
            lines: Vec::new(),
            cache_w: 0,
            scroll: 0,
            body_h: 10,
            focus: Focus::Actions,
            actions: [
                ("Approve and execute".into(), false),
                ("Approve and compact context".into(), false),
                (keep, keep_off),
                ("Refine plan".into(), false),
            ],
            sel: 0,
            tiers,
            tier,
            editing: false,
            text: String::new(),
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, content: String) {
        self.content = content;
        self.cache_w = 0;
    }

    pub fn on_paste(&mut self, s: &str) {
        if self.editing {
            self.text.push_str(&clean_paste(s));
        }
    }

    fn slider(&self) -> bool {
        self.tiers.len() > 1
    }

    fn model(&self) -> Option<String> {
        if self.slider() { self.tiers[self.tier].model.clone() } else { None }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.body_h)
    }

    fn scroll_by(&mut self, d: isize) {
        self.scroll = self.scroll.saturating_add_signed(d).min(self.max_scroll());
    }

    fn enabled(&self, i: usize) -> bool {
        !self.actions[i].1
    }

    pub fn on_key(&mut self, k: KeyEvent) -> ReviewResult {
        if k.kind == KeyEventKind::Release {
            return ReviewResult::Pending;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !ctrl && !k.modifiers.contains(KeyModifiers::ALT);
        if self.editing {
            match k.code {
                KeyCode::Esc => {
                    self.editing = false;
                    self.text.clear();
                }
                KeyCode::Enter if !self.text.trim().is_empty() => {
                    return ReviewResult::Decided(PlanDecision::Refine(self.text.clone()));
                }
                KeyCode::Backspace => {
                    self.text.pop();
                }
                KeyCode::Char('c') if ctrl => return ReviewResult::Cancelled,
                KeyCode::Char('u') if ctrl => self.text.clear(),
                KeyCode::Char(c) if plain => self.text.push(c),
                _ => {}
            }
            return ReviewResult::Pending;
        }
        let page = self.body_h.saturating_sub(1).max(1) as isize;
        match k.code {
            KeyCode::Esc => return ReviewResult::Cancelled,
            KeyCode::Char('c') if ctrl => return ReviewResult::Cancelled,
            KeyCode::Char('g') if ctrl => return ReviewResult::Edit,
            KeyCode::Char('c') if k.modifiers.is_empty() => return ReviewResult::Copy,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Body { Focus::Actions } else { Focus::Body };
            }
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll(),
            _ if self.focus == Focus::Body => match k.code {
                KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.scroll >= self.max_scroll() {
                        self.focus = Focus::Actions;
                    } else {
                        self.scroll_by(1);
                    }
                }
                KeyCode::Char('g') => self.scroll = 0,
                KeyCode::Char('G') => self.scroll = self.max_scroll(),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Actions,
                _ => {}
            },
            KeyCode::Up | KeyCode::Char('k') => match (0..self.sel).rev().find(|&i| self.enabled(i)) {
                Some(i) => self.sel = i,
                None => self.focus = Focus::Body,
            },
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(i) = (self.sel + 1..self.actions.len()).find(|&i| self.enabled(i)) {
                    self.sel = i;
                }
            }
            KeyCode::Left => self.tier = self.tier.saturating_sub(1),
            KeyCode::Right => self.tier = (self.tier + 1).min(self.tiers.len().saturating_sub(1)),
            KeyCode::Char('h') if self.slider() => self.tier = self.tier.saturating_sub(1),
            KeyCode::Char('l') if self.slider() => self.tier = (self.tier + 1).min(self.tiers.len() - 1),
            KeyCode::Enter => {
                let model = self.model();
                return match self.sel {
                    0 => ReviewResult::Decided(PlanDecision::Execute { model }),
                    1 => ReviewResult::Decided(PlanDecision::Compact { model }),
                    2 if self.enabled(2) => ReviewResult::Decided(PlanDecision::Keep { model }),
                    3 => {
                        self.editing = true;
                        ReviewResult::Pending
                    }
                    _ => ReviewResult::Pending,
                };
            }
            _ => {}
        }
        ReviewResult::Pending
    }

    pub fn render(&mut self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        if self.cache_w != width {
            self.lines = markdown::render(&self.content, width).iter().flat_map(|l| line::wrap(l, width)).collect();
            self.cache_w = width;
        }
        let n = self.lines.len();
        let slider = self.slider();
        let chrome = 1 + 1 + if slider { 2 } else { 0 } + 4 + 1;
        self.body_h = n.max(1).min(max_rows.saturating_sub(chrome).max(3));
        self.scroll = self.scroll.min(n.saturating_sub(self.body_h));

        let mut out: Vec<Line> = Vec::new();
        let pos = if n == 0 {
            " · (empty plan)".to_string()
        } else {
            format!(" · {} · {}–{}/{}", self.uri, self.scroll + 1, (self.scroll + self.body_h).min(n), n)
        };
        let mut title = vec![span(format!("Plan review: {}", self.title), accent_bold())];
        title.push(span(pos, dim()));
        out.push(line::truncate(&title, width));
        for l in self.lines.iter().skip(self.scroll).take(self.body_h) {
            out.push(line::truncate(l, width));
        }
        out.push(styled("─".repeat(width), |_| dim()));
        if slider {
            let accent = theme::get().fg(Role::Accent);
            let arrow = |on: bool| if on { accent } else { dim() };
            let last = self.tiers.len() - 1;
            out.push(line::truncate(
                &vec![
                    span("continue with  ", dim()),
                    span("◂", arrow(self.tier > 0)),
                    span(format!("  {}  ", self.tiers[self.tier].role), ContentStyle::default()),
                    span("▸", arrow(self.tier < last)),
                ],
                width,
            ));
            out.push(line::truncate(&styled(format!("  ↳ {}", self.tiers[self.tier].detail), |_| dim()), width));
        }
        let first_action = out.len();
        for (i, (label, off)) in self.actions.iter().enumerate() {
            let selected = i == self.sel;
            let focused = selected && self.focus == Focus::Actions;
            let marker_style = if focused { theme::get().fg(Role::Accent) } else { dim() };
            let mut l: Line = vec![span(if selected { "❯ " } else { "  " }, marker_style)];
            if i == 3 && self.editing {
                l.push(span(format!("{label}: {}", self.text), if focused { line::bold() } else { ContentStyle::default() }));
                if self.text.is_empty() {
                    l.push(span("type feedback", dim()));
                }
            } else {
                let style = if *off {
                    dim()
                } else if focused {
                    line::bold()
                } else {
                    ContentStyle::default()
                };
                l.push(span(label.clone(), style));
            }
            let mut l = line::truncate(&l, width);
            if focused {
                l = line::fill(&l, width, theme::get().color(Role::Selected));
            }
            out.push(l);
        }
        let help: Vec<&str> = if self.editing {
            vec!["Enter send", "Esc back"]
        } else if self.focus == Focus::Body {
            vec!["↑↓ scroll", "PgUp/PgDn page", "g/G ends", "c copy", "Ctrl+G edit", "Tab actions", "Esc cancel"]
        } else {
            let mut h = vec!["↑↓ select", "Enter confirm"];
            if slider {
                h.push("←→ model");
            }
            h.extend(["c copy", "Ctrl+G edit", "Tab plan", "Esc cancel"]);
            h
        };
        out.push(line::truncate(&styled(help.join(" · "), |_| dim()), width));

        let cur = if self.editing {
            let w = format!("❯ Refine plan: {}", self.text).width();
            (first_action + 3, w.min(width.saturating_sub(1)))
        } else {
            (first_action + self.sel, 0)
        };
        (out, cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn review(content: &str, ctx: Option<(u64, u64)>, tiers: Vec<Tier>) -> PlanReview {
        PlanReview::new("T".into(), "local://t-plan.md".into(), PathBuf::new(), content.into(), ctx, tiers)
    }

    fn tiers() -> Vec<Tier> {
        ["smol", "default"].iter().map(|r| Tier { role: r.to_string(), detail: "d".into(), model: Some(format!("@{r}")) }).collect()
    }

    #[test]
    fn keep_label_and_disable() {
        let r = review("x", Some((42_000, 200_000)), vec![]);
        assert_eq!(r.actions[2].0, "Approve and keep context (~42K / 200K)");
        assert!(!r.actions[2].1);
        let mut r = review("x", Some((196_000, 200_000)), vec![]);
        assert!(r.actions[2].1);
        r.sel = 1;
        r.on_key(key(KeyCode::Down));
        assert_eq!(r.sel, 3);
    }

    #[test]
    fn scroll_and_focus() {
        let body = (0..80).map(|i| format!("line {i}\n")).collect::<String>();
        let mut r = review(&body, None, vec![]);
        let (l, _) = r.render(60, 20);
        assert!(l.len() <= 20);
        r.on_key(key(KeyCode::Tab));
        assert_eq!(r.focus, Focus::Body);
        r.on_key(key(KeyCode::End));
        assert_eq!(r.scroll, r.lines.len() - r.body_h);
        r.on_key(key(KeyCode::Down));
        assert_eq!(r.focus, Focus::Actions);
        r.on_key(key(KeyCode::Up));
        assert_eq!(r.focus, Focus::Body);
        assert!(r.render(60, 20).0.len() <= 20);
    }

    #[test]
    fn refine_flow() {
        let mut r = review("x", None, vec![]);
        for _ in 0..3 {
            r.on_key(key(KeyCode::Down));
        }
        r.on_key(key(KeyCode::Enter));
        assert!(r.editing);
        r.on_key(key(KeyCode::Char('a')));
        r.on_key(key(KeyCode::Char('b')));
        assert!(matches!(r.on_key(key(KeyCode::Esc)), ReviewResult::Pending));
        assert!(!r.editing);
        r.on_key(key(KeyCode::Enter));
        r.on_key(key(KeyCode::Char('x')));
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Decided(PlanDecision::Refine(t)) if t == "x"));
    }

    #[test]
    fn tier_model() {
        let mut r = review("x", None, tiers());
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Decided(PlanDecision::Execute { model: Some(m) }) if m == "@default"));
        let mut r = review("x", None, tiers());
        r.on_key(key(KeyCode::Left));
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Decided(PlanDecision::Execute { model: Some(m) }) if m == "@smol"));
        let mut r = review("x", None, tiers().into_iter().take(1).collect());
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Decided(PlanDecision::Execute { model: None })));
    }
}
