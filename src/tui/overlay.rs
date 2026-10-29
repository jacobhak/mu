//! The pickers and dialogs drawn in place of the composer, and what a key does inside each.
//! A key never reaches into the app: it either keeps the overlay open, closes it, or hands the
//! app an `OverlayEvent` saying what the user chose.

use std::path::PathBuf;

use crossterm::event::KeyEvent;
use tokio::sync::oneshot;

use super::plan_review::{PlanReview, ReviewResult};
use super::reply::{ReplyPicker, ReplyResult};
use super::select::{Item, Select, SelectResult};
use crate::events::{Answer, AskQuestion, PlanDecision};

/// One entry of the `/login` picker.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginTarget {
    /// An `omp login` provider, by its menu number.
    Provider(usize),
    /// An MCP server, by its name in `mcp.json`.
    Mcp(String),
}

pub enum Overlay {
    Model {
        sel: Select,
        ids: Vec<String>,
        assign: bool,
    },
    /// Where a picked model goes: this session, or a role in omp's config.
    Role {
        sel: Select,
        model: String,
        roles: Vec<Option<String>>,
    },
    /// What can be logged into: `omp login` providers, then MCP servers that need auth.
    Login {
        sel: Select,
        targets: Vec<LoginTarget>,
    },
    Session {
        sel: Select,
        paths: Vec<PathBuf>,
    },
    /// The repository's parked plans.
    Plans {
        sel: Select,
        ids: Vec<String>,
    },
    Ask {
        qs: Vec<AskQuestion>,
        idx: usize,
        answers: Vec<Answer>,
        sel: Select,
        reply: Option<oneshot::Sender<Option<Vec<Answer>>>>,
    },
    Review {
        review: Box<PlanReview>,
        reply: Option<oneshot::Sender<PlanDecision>>,
        /// Opened from the store, not from a running `propose_plan`.
        stored: bool,
    },
    Copy {
        sel: Select,
        texts: Vec<String>,
    },
    /// Pick parts of the last reply to quote into the composer.
    Reply {
        st: Box<ReplyPicker>,
    },
}

/// What the user chose; the app carries it out.
pub enum OverlayEvent {
    /// Switch this session to the model.
    SetModel(String),
    /// A model was picked for assignment: ask which role gets it.
    PickRole(String),
    /// Assign `model` to a role (`None`: only use it for this session).
    AssignRole {
        role: Option<String>,
        model: String,
    },
    Login(LoginTarget),
    Copy(Option<String>),
    /// Insert these blockquotes at the end of the composer.
    Quote(Vec<String>),
    Resume(PathBuf),
    /// Copy the plan text.
    CopyPlan(String),
    /// Edit the plan file; the overlay stays up and reloads it afterwards.
    EditPlan(PathBuf),
    /// The plan review was dismissed. A live plan cancels the turn; a stored one just stays parked.
    PlanCancelled {
        stored: bool,
    },
    /// A parked plan was chosen from the list.
    OpenPlan(String),
}

/// The question as a picker: its options, a free-text row, multi-select, the recommended choice preselected.
pub fn ask_select(q: &AskQuestion) -> Select {
    let items: Vec<Item> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| Item {
            label: if q.recommended == Some(i) { format!("{} (Recommended)", o.label) } else { o.label.clone() },
            detail: o.description.clone().unwrap_or_default(),
        })
        .collect();
    let title = q.header.as_ref().map_or(q.question.clone(), |h| format!("{h}: {}", q.question));
    let mut s = Select::new(&title, items).free_text("Other (type your own)").detail_panel();
    if q.multi {
        s = s.multi();
    }
    if let Some(r) = q.recommended {
        s = s.preselect(r);
    }
    s
}

impl Overlay {
    /// The picker inside this overlay, for everything but the plan review.
    pub fn select(&self) -> Option<&Select> {
        match self {
            Overlay::Model { sel, .. }
            | Overlay::Role { sel, .. }
            | Overlay::Login { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::Plans { sel, .. }
            | Overlay::Ask { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Review { .. } | Overlay::Reply { .. } => None,
        }
    }

    pub fn select_mut(&mut self) -> Option<&mut Select> {
        match self {
            Overlay::Model { sel, .. }
            | Overlay::Role { sel, .. }
            | Overlay::Login { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::Plans { sel, .. }
            | Overlay::Ask { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Review { .. } | Overlay::Reply { .. } => None,
        }
    }

    /// The overlay's lines (a blank row first) and the cursor within them. `used` is how many
    /// rows the transcript and status already take; the plan review fits itself to what is left.
    pub fn render(&mut self, w: usize, rows: usize, used: usize) -> (Vec<super::line::Line>, (usize, usize)) {
        let (body, cur) = match self {
            Overlay::Review { review, .. } => {
                let avail = rows.saturating_sub(1).saturating_sub(used);
                review.render(w, avail.saturating_sub(1))
            }
            Overlay::Reply { st } => {
                let budget = (rows / 2).max(6).saturating_sub(2);
                st.render(w, budget)
            }
            other => match other.select() {
                Some(sel) => sel.render(w, (rows / 2).max(6).saturating_sub(2)),
                None => (Vec::new(), (0, 0)),
            },
        };
        let mut framed: Vec<super::line::Line> = vec![Vec::new()];
        framed.extend(body);
        (framed, (cur.0 + 1, cur.1))
    }

    pub fn on_paste(&mut self, s: &str) {
        match self {
            Overlay::Review { review, .. } => review.on_paste(s),
            other => {
                if let Some(sel) = other.select_mut() {
                    sel.on_paste(s);
                }
            }
        }
    }

    /// Handles one key. Returns the overlay if it stays open, and what the user chose, if anything.
    pub fn on_key(self, k: KeyEvent) -> (Option<Overlay>, Option<OverlayEvent>) {
        match self {
            Overlay::Model { mut sel, ids, assign } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Model { sel, ids, assign }), None),
                SelectResult::Chosen(i) => {
                    let ev = i.first().and_then(|i| ids.get(*i)).map(|id| {
                        if assign && !id.starts_with('@') {
                            OverlayEvent::PickRole(id.clone())
                        } else {
                            OverlayEvent::SetModel(id.clone())
                        }
                    });
                    (None, ev)
                }
                _ => (None, None),
            },
            Overlay::Role { mut sel, model, roles } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Role { sel, model, roles }), None),
                SelectResult::Chosen(i) => {
                    let ev = i
                        .first()
                        .and_then(|i| roles.get(*i))
                        .map(|role| OverlayEvent::AssignRole { role: role.clone(), model: model.clone() });
                    (None, ev)
                }
                _ => (None, None),
            },
            Overlay::Login { mut sel, targets } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Login { sel, targets }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| targets.get(*i)).cloned().map(OverlayEvent::Login))
                }
                _ => (None, None),
            },
            Overlay::Copy { mut sel, texts } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Copy { sel, texts }), None),
                SelectResult::Chosen(i) => {
                    (None, Some(OverlayEvent::Copy(i.first().and_then(|i| texts.get(*i)).cloned())))
                }
                _ => (None, None),
            },
            Overlay::Reply { mut st } => match st.on_key(k) {
                ReplyResult::Pending => (Some(Overlay::Reply { st }), None),
                ReplyResult::Close => (None, None),
                ReplyResult::Quote(q) => (None, Some(OverlayEvent::Quote(q))),
            },
            Overlay::Session { mut sel, paths } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Session { sel, paths }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| paths.get(*i)).map(|p| OverlayEvent::Resume(p.clone())))
                }
                _ => (None, None),
            },
            Overlay::Plans { mut sel, ids } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Plans { sel, ids }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| ids.get(*i)).map(|id| OverlayEvent::OpenPlan(id.clone())))
                }
                _ => (None, None),
            },
            Overlay::Review { mut review, mut reply, stored } => match review.on_key(k) {
                ReviewResult::Pending => (Some(Overlay::Review { review, reply, stored }), None),
                ReviewResult::Copy => {
                    let text = review.content().to_string();
                    (Some(Overlay::Review { review, reply, stored }), Some(OverlayEvent::CopyPlan(text)))
                }
                ReviewResult::Edit => {
                    let file = review.file.clone();
                    (Some(Overlay::Review { review, reply, stored }), Some(OverlayEvent::EditPlan(file)))
                }
                ReviewResult::Cancelled => (None, Some(OverlayEvent::PlanCancelled { stored })),
                ReviewResult::Decided(d) => {
                    if let Some(r) = reply.take() {
                        let _ = r.send(d);
                    }
                    (None, None)
                }
            },
            Overlay::Ask { qs, idx, mut answers, mut sel, mut reply } => {
                let ans = match sel.on_key(k) {
                    SelectResult::Pending => return (Some(Overlay::Ask { qs, idx, answers, sel, reply }), None),
                    SelectResult::Chosen(i) => {
                        let labels =
                            i.iter().filter_map(|i| qs[idx].options.get(*i)).map(|o| o.label.clone()).collect();
                        Answer::Options(labels)
                    }
                    SelectResult::Text(t) => Answer::Custom(t),
                    SelectResult::Cancelled => {
                        let mut all = answers;
                        all.extend(std::iter::repeat_n(Answer::Cancelled, qs.len() - all.len()));
                        if let Some(r) = reply.take() {
                            let _ = r.send(Some(all));
                        }
                        return (None, None);
                    }
                };
                answers.push(ans);
                if idx + 1 < qs.len() {
                    let sel = ask_select(&qs[idx + 1]);
                    (Some(Overlay::Ask { qs, idx: idx + 1, answers, sel, reply }), None)
                } else {
                    if let Some(r) = reply.take() {
                        let _ = r.send(Some(answers));
                    }
                    (None, None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AskOption;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect()
    }

    #[test]
    fn choosing_a_model_yields_an_event_and_closes() {
        let ov = Overlay::Model {
            sel: Select::new("m", items(&["a/x", "@smol"])),
            ids: vec!["a/x".into(), "@smol".into()],
            assign: true,
        };
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert!(matches!(ev, Some(OverlayEvent::PickRole(m)) if m == "a/x"), "a concrete model asks for a role");
        let ov = Overlay::Model { sel: Select::new("m", items(&["@smol"])), ids: vec!["@smol".into()], assign: true };
        assert!(matches!(ov.on_key(key(KeyCode::Enter)).1, Some(OverlayEvent::SetModel(m)) if m == "@smol"));
    }

    #[test]
    fn typing_keeps_the_overlay_open_and_select_helpers_find_the_picker() {
        let ov = Overlay::Copy { sel: Select::new("c", items(&["one"])), texts: vec!["1".into()] };
        let (rest, ev) = ov.on_key(key(KeyCode::Char('o')));
        assert!(ev.is_none());
        let mut rest = rest.expect("still open");
        assert!(rest.select().is_some() && rest.select_mut().is_some());
        rest.on_paste("ne");
        assert!(matches!(rest.on_key(key(KeyCode::Esc)), (None, None)));
    }

    #[test]
    fn ask_walks_its_questions_and_replies_once() {
        let q = |text: &str| AskQuestion {
            question: text.into(),
            header: None,
            options: vec![],
            multi: false,
            recommended: None,
        };
        let (tx, mut rx) = oneshot::channel();
        let qs = vec![q("one"), q("two")];
        let sel = ask_select(&qs[0]);
        let ov = Overlay::Ask { qs, idx: 0, answers: vec![], sel, reply: Some(tx) };
        let (rest, _) = ov.on_key(key(KeyCode::Esc));
        assert!(rest.is_none());
        let all = rx.try_recv().unwrap().unwrap();
        assert_eq!(all.len(), 2, "cancelling answers every question");
    }

    #[test]
    fn ask_picker_shows_the_highlighted_description_below_the_options() {
        let opt = |label: &str, d: Option<&str>| AskOption { label: label.into(), description: d.map(Into::into) };
        let q = AskQuestion {
            question: "Which?".into(),
            header: None,
            options: vec![opt("Yes", Some("apply the change now and run the tests afterwards")), opt("No", None)],
            multi: false,
            recommended: None,
        };
        let (lines, _) = ask_select(&q).render(40, 20);
        let t: Vec<String> = lines.iter().map(crate::tui::line::line_text).collect();
        assert_eq!(t[2].trim_end(), "❯ Yes", "{t:?}");
        assert_eq!(t[5], "", "{t:?}");
        assert_eq!(t[6], "  ↳ apply the change now and run the", "{t:?}");
        assert_eq!(t[7], "    tests afterwards", "{t:?}");
    }

    #[test]
    fn ask_picker_shows_a_multi_line_question_in_full() {
        let q = AskQuestion {
            question: "Deploy the canary?\nstatus: 3 services green\nerrors: none in the last hour".into(),
            header: None,
            options: vec![
                AskOption { label: "Yes".into(), description: None },
                AskOption { label: "No".into(), description: None },
            ],
            multi: false,
            recommended: None,
        };
        let (lines, cur) = ask_select(&q).render(30, 20);
        let t: Vec<String> = lines.iter().map(crate::tui::line::line_text).collect();
        assert_eq!(
            t[..3],
            ["Deploy the canary?", "status: 3 services green", "errors: none in the last hour"],
            "{t:?}"
        );
        assert_eq!(t[3].trim_end(), "›", "{t:?}");
        assert_eq!(cur, (3, 2));
        assert!(t[4].starts_with("❯ Yes"), "{t:?}");
        assert!(lines.iter().all(|l| crate::tui::line::line_width(l) <= 30), "{t:?}");
    }
}
