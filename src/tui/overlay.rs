//! The pickers and dialogs drawn in place of the composer, and what a key does inside each.
//! A key never reaches into the app: it either keeps the overlay open, closes it, or hands the
//! app an `OverlayEvent` saying what the user chose.

use std::path::PathBuf;

use crossterm::event::KeyEvent;
use tokio::sync::oneshot;

use super::plan_review::{PlanReview, ReviewResult};
use super::select::{Item, Select, SelectResult};
use crate::events::{Answer, AskQuestion, PlanDecision};

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
    /// `omp login` providers as (menu number, name).
    Login {
        sel: Select,
        nums: Vec<usize>,
    },
    Session {
        sel: Select,
        paths: Vec<PathBuf>,
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
    },
    Copy {
        sel: Select,
        texts: Vec<String>,
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
    Login(usize),
    Copy(Option<String>),
    Resume(PathBuf),
    /// Copy the plan text.
    CopyPlan(String),
    /// Edit the plan file; the overlay stays up and reloads it afterwards.
    EditPlan(PathBuf),
    /// The plan review was dismissed: the turn is cancelled.
    PlanCancelled,
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
    let mut s = Select::new(&title, items).free_text("Other (type your own)");
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
            | Overlay::Ask { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Review { .. } => None,
        }
    }

    pub fn select_mut(&mut self) -> Option<&mut Select> {
        match self {
            Overlay::Model { sel, .. }
            | Overlay::Role { sel, .. }
            | Overlay::Login { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::Ask { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Review { .. } => None,
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
            Overlay::Login { mut sel, nums } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Login { sel, nums }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| nums.get(*i)).map(|n| OverlayEvent::Login(*n)))
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
            Overlay::Session { mut sel, paths } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Session { sel, paths }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| paths.get(*i)).map(|p| OverlayEvent::Resume(p.clone())))
                }
                _ => (None, None),
            },
            Overlay::Review { mut review, mut reply } => match review.on_key(k) {
                ReviewResult::Pending => (Some(Overlay::Review { review, reply }), None),
                ReviewResult::Copy => {
                    let text = review.content().to_string();
                    (Some(Overlay::Review { review, reply }), Some(OverlayEvent::CopyPlan(text)))
                }
                ReviewResult::Edit => {
                    let file = review.file.clone();
                    (Some(Overlay::Review { review, reply }), Some(OverlayEvent::EditPlan(file)))
                }
                ReviewResult::Cancelled => (None, Some(OverlayEvent::PlanCancelled)),
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
}
