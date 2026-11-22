use crate::status::format_number;
use crate::text::accent;
use crate::text::line::{self, Line, dim, span, styled};
use crate::text::markdown;
use crate::theme::{self, Role};
use crate::widgets::frame::Frame;
use crate::widgets::select::clean_paste;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::ContentStyle;
use mu_base::events::{ActionChoice, ActionInput, ReviewRequest};
use std::path::PathBuf;

#[derive(serde::Serialize, serde::Deserialize)]
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
    Decided(ActionChoice),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    Body,
    Actions,
}

struct Row {
    id: String,
    label: String,
    off: bool,
    input: ActionInput,
    approves: bool,
}

pub struct DocReview {
    pub file: Option<PathBuf>,
    pub cancel_turn: bool,
    pub cancel_notice: String,
    header: String,
    title: String,
    uri: Option<String>,
    content: String,
    lines: Vec<Line>,
    cache_w: usize,
    scroll: usize,
    body_h: usize,
    focus: Focus,
    actions: Vec<Row>,
    note: Option<String>,
    sel: usize,
    models: bool,
    tiers: Vec<Tier>,
    tier: usize,
    editing: bool,
    text: String,
}

impl DocReview {
    pub fn new(req: ReviewRequest, context: Option<(u64, u64)>, tiers: Vec<Tier>) -> Self {
        let tier = tiers.iter().position(|t| t.role == "default").unwrap_or(0);
        let actions: Vec<Row> = req
            .actions
            .into_iter()
            .map(|a| {
                let (label, off) = match context.filter(|_| a.keeps_context) {
                    Some((t, w)) => {
                        (format!("{} (~{} / {})", a.label, format_number(t), format_number(w)), t * 100 > 95 * w)
                    }
                    None => (a.label, false),
                };
                Row { id: a.id, label, off: off || a.disabled, input: a.input, approves: a.approves }
            })
            .collect();
        let sel = if actions.get(req.default).is_some_and(|r| !r.off) {
            req.default
        } else {
            actions.iter().position(|r| !r.off).unwrap_or(0)
        };
        Self {
            file: req.file,
            cancel_turn: req.cancel_turn,
            cancel_notice: req.cancel_notice,
            header: req.header,
            title: req.title,
            uri: req.uri,
            content: req.content,
            lines: Vec::new(),
            cache_w: 0,
            scroll: 0,
            body_h: 10,
            focus: Focus::Actions,
            actions,
            note: req.note,
            sel,
            models: req.models,
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
        self.models && self.tiers.len() > 1
    }

    fn model(&self) -> Option<String> {
        if self.slider() { self.tiers[self.tier].model.clone() } else { None }
    }

    fn choice(&self, text: Option<String>) -> ReviewResult {
        let row = &self.actions[self.sel];
        let model = if row.approves { self.model() } else { None };
        ReviewResult::Decided(ActionChoice { id: row.id.clone(), text, model })
    }

    fn selected_input(&self) -> Option<ActionInput> {
        self.actions.get(self.sel).map(|r| r.input)
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.body_h)
    }

    fn scroll_by(&mut self, d: isize) {
        self.scroll = self.scroll.saturating_add_signed(d).min(self.max_scroll());
    }

    fn enabled(&self, i: usize) -> bool {
        !self.actions[i].off
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
                KeyCode::Enter if self.selected_input() == Some(ActionInput::Optional) => {
                    let text = self.text.trim();
                    let text = (!text.is_empty()).then(|| self.text.clone());
                    return self.choice(text);
                }
                KeyCode::Enter if !self.text.trim().is_empty() => {
                    return self.choice(Some(self.text.clone()));
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
            KeyCode::Char('g') if ctrl && self.file.is_some() => return ReviewResult::Edit,
            KeyCode::Char('c') if k.modifiers.is_empty() => return ReviewResult::Copy,
            KeyCode::Char('n')
                if k.modifiers.is_empty()
                    && self.focus == Focus::Actions
                    && self.selected_input() == Some(ActionInput::Optional) =>
            {
                self.editing = true;
            }
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
                let Some(row) = self.actions.get(self.sel) else { return ReviewResult::Pending };
                if row.off {
                    return ReviewResult::Pending;
                }
                if row.input == ActionInput::Required {
                    self.editing = true;
                    return ReviewResult::Pending;
                }
                return self.choice(None);
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
        let (footer, first_action, edit_cursor) = self.footer(width);
        let mut frame = Frame { header: vec![Vec::new()], footer };
        if let Some(note) = &self.note {
            frame.header.push(styled(note.clone(), |_| dim()));
        }
        self.body_h = n.max(1).min(frame.body_rows(max_rows, 3));
        self.scroll = self.scroll.min(n.saturating_sub(self.body_h));

        let pos = if n == 0 {
            " · (empty)".to_string()
        } else {
            let uri = self.uri.as_ref().map(|u| format!(" · {u}")).unwrap_or_default();
            format!("{uri} · {}–{}/{}", self.scroll + 1, (self.scroll + self.body_h).min(n), n)
        };
        frame.header[0] = vec![span(format!("{}: {}", self.header, self.title), accent()), span(pos, dim())];
        let body: Vec<Line> = self.lines.iter().skip(self.scroll).take(self.body_h).cloned().collect();
        let first_action = frame.header.len() + body.len() + first_action;
        let cur = match edit_cursor.filter(|_| self.editing) {
            Some((row, col)) => (frame.header.len() + body.len() + row, col),
            None => (first_action + self.sel, 0),
        };
        (frame.finish(body, width), cur)
    }

    /// The rows under the plan: a rule, the model slider, the actions and the key line. Also the row
    /// of the first action in them and, while feedback is typed, where the text cursor goes
    /// (measured as the row is built, so it cannot drift from what is drawn).
    fn footer(&self, width: usize) -> (Vec<Line>, usize, Option<(usize, usize)>) {
        let mut out: Vec<Line> = vec![styled("─".repeat(width), |_| dim())];
        if self.slider() {
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
        let mut edit_cursor: Option<(usize, usize)> = None;
        for (i, Row { label, off, .. }) in self.actions.iter().enumerate() {
            let selected = i == self.sel;
            let focused = selected && self.focus == Focus::Actions;
            let marker_style = if focused { theme::get().fg(Role::Accent) } else { dim() };
            let mut l: Line = vec![span(if selected { "❯ " } else { "  " }, marker_style)];
            if selected && self.editing {
                l.push(span(
                    format!("{label}: {}", self.text),
                    if focused { line::bold() } else { ContentStyle::default() },
                ));
                edit_cursor = Some((out.len(), line::line_width(&l).min(width.saturating_sub(1))));
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
            let mut h = vec!["↑↓ scroll", "PgUp/PgDn page", "g/G ends", "c copy"];
            if self.file.is_some() {
                h.push("Ctrl+G edit");
            }
            h.extend(["Tab actions", "Esc cancel"]);
            h
        } else {
            let mut h = vec!["↑↓ select", "Enter confirm"];
            if self.slider() {
                h.push("←→ model");
            }
            h.push("c copy");
            if self.file.is_some() {
                h.push("Ctrl+G edit");
            }
            h.push("Tab document");
            if self.selected_input() == Some(ActionInput::Optional) {
                h.push("n note");
            }
            h.push("Esc cancel");
            h
        };
        out.push(styled(help.join(" · "), |_| dim()));
        (out, first_action, edit_cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::events::ActionSpec;
    use mu_testutil::key;

    const REFINE_LABEL: &str = "Refine plan";

    fn spec(id: &str, label: &str, input: ActionInput, keeps: bool, approves: bool) -> ActionSpec {
        ActionSpec { id: id.into(), label: label.into(), disabled: false, input, keeps_context: keeps, approves }
    }

    fn request(content: &str, stored: bool) -> ReviewRequest {
        let n = ActionInput::None;
        ReviewRequest {
            header: "Plan review".into(),
            title: "T".into(),
            uri: Some("local://t-plan.md".into()),
            file: Some(PathBuf::new()),
            content: content.into(),
            note: None,
            actions: vec![
                spec("execute", "Approve and execute", n, false, true),
                spec("compact", "Approve and compact context", n, false, true),
                spec("keep", "Approve and keep context", n, true, true),
                if stored {
                    spec("drop", "Drop plan", n, false, false)
                } else {
                    spec("park", "Park plan", n, false, false)
                },
                spec("refine", REFINE_LABEL, ActionInput::Required, false, false),
            ],
            default: 0,
            models: true,
            cancel_turn: !stored,
            cancel_notice: String::new(),
        }
    }

    fn review(content: &str, ctx: Option<(u64, u64)>, tiers: Vec<Tier>) -> DocReview {
        DocReview::new(request(content, false), ctx, tiers)
    }

    fn decided(r: ReviewResult) -> ActionChoice {
        match r {
            ReviewResult::Decided(c) => c,
            _ => panic!("not decided"),
        }
    }

    fn tiers() -> Vec<Tier> {
        ["smol", "default"]
            .iter()
            .map(|r| Tier { role: r.to_string(), detail: "d".into(), model: Some(format!("@{r}")) })
            .collect()
    }

    #[test]
    fn the_review_fits_the_rows_it_was_given() {
        let body = (0..80).map(|i| format!("line {i}\n")).collect::<String>();
        for (tiers, note) in [(vec![], false), (tiers(), false), (tiers(), true)] {
            let mut r = review(&body, None, tiers);
            r.note = note.then(|| "a note".to_string());
            for max in [14, 20, 40] {
                let (rows, cur) = r.render(60, max);
                assert!(rows.len() <= max, "{} rows in {max}", rows.len());
                assert!(cur.0 < rows.len());
            }
        }
    }

    #[test]
    fn keep_label_and_disable() {
        let r = review("x", Some((42_000, 200_000)), vec![]);
        assert_eq!(r.actions[2].label, "Approve and keep context (~42K / 200K)");
        assert!(!r.actions[2].off);
        let mut r = review("x", Some((196_000, 200_000)), vec![]);
        assert!(r.actions[2].off);
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
        for _ in 0..4 {
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
        let (rows, (row, col)) = r.render(60, 30);
        let text = line::line_text(&rows[row]);
        assert!(text.starts_with(&format!("❯ {REFINE_LABEL}: x")), "{text:?}");
        assert_eq!(col, line::line_width(&vec![span(format!("❯ {REFINE_LABEL}: x"), ContentStyle::default())]));
        let c = decided(r.on_key(key(KeyCode::Enter)));
        assert_eq!((c.id.as_str(), c.text.as_deref()), ("refine", Some("x")));
    }

    #[test]
    fn labels_come_from_the_request() {
        let r = review("x", None, vec![]);
        let labels: Vec<&str> = r.actions.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Approve and execute",
                "Approve and compact context",
                "Approve and keep context",
                "Park plan",
                REFINE_LABEL
            ]
        );
    }

    #[test]
    fn enter_on_a_required_row_opens_the_text_line() {
        let mut r = review("x", None, vec![]);
        r.sel = 4;
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Pending));
        assert!(r.editing);
        assert!(matches!(r.on_key(key(KeyCode::Enter)), ReviewResult::Pending));
    }

    #[test]
    fn n_adds_a_note_on_an_optional_row() {
        let mut req = request("x", false);
        req.actions[0].input = ActionInput::Optional;
        let mut r = DocReview::new(req, None, vec![]);
        r.on_key(key(KeyCode::Char('n')));
        assert!(r.editing);
        r.on_key(key(KeyCode::Char('h')));
        r.on_key(key(KeyCode::Char('i')));
        let c = decided(r.on_key(key(KeyCode::Enter)));
        assert_eq!((c.id.as_str(), c.text.as_deref()), ("execute", Some("hi")));
        let mut req = request("x", false);
        req.actions[0].input = ActionInput::Optional;
        let mut r = DocReview::new(req, None, vec![]);
        r.on_key(key(KeyCode::Char('n')));
        assert_eq!(decided(r.on_key(key(KeyCode::Enter))).text, None);
    }

    #[test]
    fn tier_model() {
        let mut r = review("x", None, tiers());
        let c = decided(r.on_key(key(KeyCode::Enter)));
        assert_eq!((c.id.as_str(), c.model.as_deref()), ("execute", Some("@default")));
        let mut r = review("x", None, tiers());
        r.on_key(key(KeyCode::Left));
        assert_eq!(decided(r.on_key(key(KeyCode::Enter))).model.as_deref(), Some("@smol"));
        let mut r = review("x", None, tiers().into_iter().take(1).collect());
        assert_eq!(decided(r.on_key(key(KeyCode::Enter))).model, None);
    }

    #[test]
    fn park_and_drop_actions() {
        let live = review("x", None, vec![]);
        assert_eq!(live.actions[3].label, "Park plan");
        let mut r = review("x", None, tiers());
        for _ in 0..3 {
            r.on_key(key(KeyCode::Down));
        }
        let c = decided(r.on_key(key(KeyCode::Enter)));
        assert_eq!((c.id.as_str(), c.model), ("park", None));
        let mut r = DocReview::new(request("x", true), None, vec![]);
        assert_eq!(r.actions[3].label, "Drop plan");
        for _ in 0..3 {
            r.on_key(key(KeyCode::Down));
        }
        assert_eq!(decided(r.on_key(key(KeyCode::Enter))).id, "drop");
    }

    #[test]
    fn note_is_the_second_row() {
        let mut req = request("x", true);
        req.note = Some("N".into());
        let mut r = DocReview::new(req, None, vec![]);
        let (rows, _) = r.render(60, 30);
        assert_eq!(line::line_text(&rows[1]), "N");
    }
}
