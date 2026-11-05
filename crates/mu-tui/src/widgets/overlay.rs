//! The pickers and dialogs drawn in place of the composer, and what a key does inside each.
//! A key never reaches into the app: it either keeps the overlay open, closes it, or hands the
//! app an `OverlayEvent` saying what the user chose.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use crate::panels::jobs_panel::{JobsPanel, JobsResult};
use crate::panels::plan_review::{PlanReview, ReviewResult};
use crate::panels::reply::{ReplyPicker, ReplyResult};
use crate::widgets::select::{Item, Select, SelectResult};
use mu_base::events::{Answer, AskQuestion, PlanDecision};

/// One entry of the `/login` picker.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginTarget {
    /// An `omp login` provider, by its menu number.
    Provider(usize),
    /// An MCP server, by its name in `mcp.json`.
    Mcp(String),
    /// A native API-key provider, by id (`models::PROVIDERS`).
    ApiKey(String),
}

/// What the user does next in the API-key picker.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyOption {
    /// Type or paste the key (masked).
    Paste,
    /// Use `$NAME`, the variable the user already has set.
    Env(String),
    /// Type a `!command` that prints the key.
    Command,
    /// Validation could not reach the provider: keep the key anyway.
    SaveAnyway,
    Cancel,
}

/// A row of the `/mcp` server list.
#[derive(Clone, Debug, PartialEq)]
pub enum McpRow {
    Server(String),
    Add,
    Import,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpAction {
    Enable,
    Disable,
    Reconnect,
    Test,
    Login,
    Remove,
}

/// Which `/mcp` picker this is, with what its rows stand for.
#[derive(Clone, Debug, PartialEq)]
pub enum McpPick {
    Servers(Vec<McpRow>),
    Actions(String, Vec<McpAction>),
    /// Remove this server? Row 0 confirms.
    ConfirmRemove(String),
    /// A step of the `/mcp add` wizard.
    Wizard,
}

pub enum Overlay {
    Model {
        sel: Select,
        ids: Vec<String>,
        assign: bool,
        /// The role the model is for, when the picker was opened from a role that has none.
        target: Option<String>,
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
    /// The API-key picker for `provider`: the options it offers.
    KeyChoice {
        sel: Select,
        provider: String,
        options: Vec<KeyOption>,
    },
    /// `/logout`: the stored API keys that can be removed.
    Logout {
        sel: Select,
        providers: Vec<String>,
    },
    /// A `/mcp` picker: the server list, a server's actions, or a wizard step.
    Mcp {
        sel: Select,
        pick: McpPick,
    },
    Session {
        sel: Select,
        paths: Vec<PathBuf>,
    },
    /// The repository's parked plans.
    Plans {
        sel: Select,
        ids: Vec<String>,
        /// Chosen plans are delivered, not opened.
        deliver: bool,
    },
    /// A question, or the review after the last one. Boxed: the largest variant.
    Ask(Box<AskState>),
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
    /// This session's background jobs.
    Jobs {
        st: Box<JobsPanel>,
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
    /// The API-key picker chose this for `provider` (`Cancel` when it was dismissed).
    Key {
        provider: String,
        choice: KeyOption,
    },
    /// Remove the stored API key of this provider.
    Logout(String),
    /// A `/mcp` picker closed: the row chosen, or `None` when dismissed.
    Mcp(McpPick, Option<usize>),
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
    /// A parked plan was chosen to be delivered.
    DeliverPlan(String),
    /// Cancel one running job by id, or all of them.
    KillJobs(Option<String>),
    /// Message a live session (`mu notify`).
    NotifySession {
        session: PathBuf,
        text: String,
    },
    /// Stop a live session; `pid` and `started` say which run the user saw.
    StopSession {
        session: PathBuf,
        pid: u32,
        started: String,
        kind: String,
    },
    /// Ask an interactive mu to exit (confirmed in the panel).
    TerminateSession {
        session: PathBuf,
        pid: u32,
        started: String,
    },
    /// Take over a `-p` run: the panel closes and `mu take` runs on the terminal.
    TakeSession(PathBuf),
}

/// The question as a picker: its options, a free-text row, multi-select, the recommended choice
/// preselected. Single-select answers on the digit keys (`y`/`n` for a confirm), and a note can be
/// typed on the highlighted option (Right, or Tab in single-select). Options show their
/// descriptions in place.
#[cfg(test)]
pub fn ask_select(q: &AskQuestion) -> Select {
    ask_at(q, None, None)
}

/// A question's picker: `pos` is its place among the questions (`k` of `n`) for the title, and
/// `prior` its earlier answer, preselected so a revisit shows it.
fn ask_at(q: &AskQuestion, pos: Option<(usize, usize)>, prior: Option<&Answer>) -> Select {
    let items: Vec<Item> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| Item {
            label: if q.recommended == Some(i) { format!("{} (Recommended)", o.label) } else { o.label.clone() },
            detail: o.description.clone().unwrap_or_default(),
        })
        .collect();
    let previews = q.options.iter().map(|o| o.preview.clone()).collect();
    let kinds = q.options.iter().map(|o| o.kind).collect();
    let title = q.header.as_ref().map_or(q.question.clone(), |h| format!("{h}: {}", q.question));
    let title = match pos {
        Some((k, n)) => format!("{k}/{n} · {title}"),
        None => title,
    };
    let back = pos.is_some_and(|(k, _)| k > 1);
    let mut s = Select::new(&title, items)
        .context(q.context.as_deref().unwrap_or(""))
        .free_text("Other (type your own)")
        .previews(previews)
        .preview_kinds(kinds)
        .notes()
        .described()
        .back_keys(back)
        .hint(&ask_hint(q, pos));
    if q.multi {
        s = s.multi();
    } else if q.confirm {
        s = s.quick_keys(vec![('y', 0), ('n', 1)]);
    } else {
        let n = q.options.len().min(9);
        s = s.quick_keys((0..n).map(|i| (char::from(b'1' + i as u8), i)).collect());
    }
    match prior {
        Some(prior) => prefill(s, q, prior),
        None => match q.recommended {
            Some(r) => s.preselect(r),
            None => s,
        },
    }
}

/// Preselects an earlier answer: its options, its note, or its free text.
fn prefill(s: Select, q: &AskQuestion, prior: &Answer) -> Select {
    let indices = |labels: &[String]| -> Vec<usize> {
        labels.iter().filter_map(|l| q.options.iter().position(|o| o.label == *l)).collect()
    };
    match prior {
        Answer::Options(labels) => s.preselect_all(&indices(labels)),
        Answer::Noted { picks, note } => s.preselect_all(&indices(picks)).note_text(note),
        Answer::Custom(text) => s.prefill_text(text),
        Answer::Cancelled => s,
    }
}

/// The footer: the keys of the question's kind, then how to go back (or cancel, on the first).
fn ask_hint(q: &AskQuestion, pos: Option<(usize, usize)>) -> String {
    let mut hint = if q.multi {
        "↑↓ move · Tab toggle · Enter submit · → note".to_string()
    } else if q.confirm {
        "y yes · n no · Enter choose · Tab/→ note".to_string()
    } else {
        format!("↑↓ move · Enter choose · 1–{} pick · Tab/→ note", q.options.len().min(9))
    };
    if pos.is_some_and(|(k, _)| k > 1) {
        hint.push_str(" · ← / ⇧Tab back");
    }
    hint.push_str(" · Esc cancel");
    hint
}

/// The review screen: each question with its answer, then a Submit row, which starts highlighted.
fn review_select(qs: &[AskQuestion], answers: &[Option<Answer>]) -> Select {
    let mut items: Vec<Item> = qs
        .iter()
        .zip(answers)
        .enumerate()
        .map(|(i, (q, a))| {
            let name = q.header.as_deref().filter(|h| !h.is_empty()).map_or(q.question.as_str(), |h| h);
            Item {
                label: format!("{}. {}", i + 1, name.replace('\n', " ")),
                detail: a.as_ref().map_or(String::new(), answer_text),
            }
        })
        .collect();
    items.push(Item { label: "Submit answers".into(), detail: String::new() });
    let submit = qs.len();
    Select::new("Review your answers", items)
        .preselect(submit)
        .back_keys(true)
        .hint("↑↓ move · Enter reopen or submit · ← / ⇧Tab back · Esc cancel")
}

/// An answer on one line, as the review lists it.
fn answer_text(a: &Answer) -> String {
    let text = match a {
        Answer::Options(picks) => picks.join(", "),
        Answer::Noted { picks, note } => format!("{} — note: {note}", picks.join(", ")),
        Answer::Custom(t) => format!("\"{t}\""),
        Answer::Cancelled => "(cancelled)".to_string(),
    };
    text.replace('\n', " ")
}

/// An ask in progress: its questions, the answer to each so far, and the picker on screen.
pub struct AskState {
    qs: Vec<AskQuestion>,
    /// The question being answered, or reopened from the review.
    idx: usize,
    /// Each question's answer. Going back keeps them, so a revised answer replaces the earlier one.
    answers: Vec<Option<Answer>>,
    /// The picker for question `idx`, or the review list while `review` is set.
    sel: Select,
    review: bool,
    /// Reopened from the review: answering the question returns to the review.
    revisit: bool,
    reply: Option<oneshot::Sender<Option<Vec<Answer>>>>,
}

impl AskState {
    fn new(qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) -> Self {
        let answers = vec![None; qs.len()];
        let sel = picker(&qs, 0, &answers);
        AskState { qs, idx: 0, answers, sel, review: false, revisit: false, reply: Some(reply) }
    }

    /// Shows question `idx` with its earlier answer.
    fn goto(&mut self, idx: usize) {
        self.idx = idx;
        self.review = false;
        self.revisit = false;
        self.sel = picker(&self.qs, idx, &self.answers);
    }

    fn show_review(&mut self) {
        self.review = true;
        self.revisit = false;
        self.sel = review_select(&self.qs, &self.answers);
    }

    fn keep(self) -> (Option<Overlay>, Option<OverlayEvent>) {
        (Some(Overlay::Ask(Box::new(self))), None)
    }

    /// Replies with every answer; a question never answered reads as cancelled.
    fn done(mut self) -> (Option<Overlay>, Option<OverlayEvent>) {
        let all: Vec<Answer> = self.answers.drain(..).map(|a| a.unwrap_or(Answer::Cancelled)).collect();
        self.reply_with(all)
    }

    /// Cancels from the current question on: the answers before it stand; from the review, all cancel.
    fn cancel(mut self) -> (Option<Overlay>, Option<OverlayEvent>) {
        let cut = if self.review { 0 } else { self.idx };
        let all: Vec<Answer> = self
            .answers
            .drain(..)
            .enumerate()
            .map(|(i, a)| match a {
                Some(a) if i < cut => a,
                _ => Answer::Cancelled,
            })
            .collect();
        self.reply_with(all)
    }

    fn reply_with(&mut self, all: Vec<Answer>) -> (Option<Overlay>, Option<OverlayEvent>) {
        if let Some(r) = self.reply.take() {
            let _ = r.send(Some(all));
        }
        (None, None)
    }

    fn key(mut self, k: KeyEvent) -> (Option<Overlay>, Option<OverlayEvent>) {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        let n = self.qs.len();
        let res = self.sel.on_key(k);
        if matches!(res, SelectResult::Pending) {
            return self.keep();
        }
        if ctrl_c {
            return self.cancel();
        }
        if self.review {
            match res {
                SelectResult::Chosen(i) if i.first() == Some(&n) => return self.done(),
                SelectResult::Chosen(i) => {
                    if let Some(&j) = i.first() {
                        self.goto(j);
                        self.revisit = true;
                    }
                }
                SelectResult::Back => self.goto(n - 1),
                SelectResult::Cancelled => return self.cancel(),
                _ => {}
            }
            return self.keep();
        }
        let ans = match res {
            SelectResult::Chosen(i) => Answer::Options(picked_labels(&self.qs[self.idx], &i)),
            SelectResult::Noted(i, note) => Answer::Noted { picks: picked_labels(&self.qs[self.idx], &i), note },
            SelectResult::Text(t) => Answer::Custom(t),
            SelectResult::Back => {
                if self.idx > 0 {
                    self.goto(self.idx - 1);
                }
                return self.keep();
            }
            SelectResult::Cancelled => return self.cancel(),
            SelectResult::Pending => return self.keep(),
        };
        self.answers[self.idx] = Some(ans);
        if n == 1 {
            return self.done();
        }
        if self.revisit || self.idx + 1 == n {
            self.show_review();
        } else {
            self.goto(self.idx + 1);
        }
        self.keep()
    }
}

/// The picker on screen for question `idx`.
fn picker(qs: &[AskQuestion], idx: usize, answers: &[Option<Answer>]) -> Select {
    let pos = (qs.len() > 1).then_some((idx + 1, qs.len()));
    ask_at(&qs[idx], pos, answers[idx].as_ref())
}

/// The labels of the picked options of an ask question.
fn picked_labels(q: &AskQuestion, picks: &[usize]) -> Vec<String> {
    picks.iter().filter_map(|i| q.options.get(*i)).map(|o| o.label.clone()).collect()
}

impl Overlay {
    /// An ask: its questions, answered one by one, then the review (after two or more).
    pub fn new_ask(qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) -> Overlay {
        Overlay::Ask(Box::new(AskState::new(qs, reply)))
    }
    /// The picker inside this overlay, for everything but the plan review.
    pub fn select(&self) -> Option<&Select> {
        match self {
            Overlay::Model { sel, .. }
            | Overlay::Role { sel, .. }
            | Overlay::Login { sel, .. }
            | Overlay::KeyChoice { sel, .. }
            | Overlay::Logout { sel, .. }
            | Overlay::Mcp { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::Plans { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Ask(a) => Some(&a.sel),
            Overlay::Review { .. } | Overlay::Reply { .. } | Overlay::Jobs { .. } => None,
        }
    }

    pub fn select_mut(&mut self) -> Option<&mut Select> {
        match self {
            Overlay::Model { sel, .. }
            | Overlay::Role { sel, .. }
            | Overlay::Login { sel, .. }
            | Overlay::KeyChoice { sel, .. }
            | Overlay::Logout { sel, .. }
            | Overlay::Mcp { sel, .. }
            | Overlay::Session { sel, .. }
            | Overlay::Plans { sel, .. }
            | Overlay::Copy { sel, .. } => Some(sel),
            Overlay::Ask(a) => Some(&mut a.sel),
            Overlay::Review { .. } | Overlay::Reply { .. } | Overlay::Jobs { .. } => None,
        }
    }

    /// The overlay's lines (a blank row first) and the cursor within them. `used` is how many
    /// rows the transcript and status already take; the plan review fits itself to what is left.
    pub fn render(&mut self, w: usize, rows: usize, used: usize) -> (Vec<crate::text::line::Line>, (usize, usize)) {
        let (body, cur) = match self {
            Overlay::Review { review, .. } => {
                let avail = rows.saturating_sub(1).saturating_sub(used);
                review.render(w, avail.saturating_sub(1))
            }
            Overlay::Reply { st } => {
                let avail = rows.saturating_sub(1).saturating_sub(used);
                st.render(w, avail.saturating_sub(1))
            }
            Overlay::Jobs { st } => {
                let avail = rows.saturating_sub(1).saturating_sub(used);
                st.render(w, avail.saturating_sub(1), std::time::SystemTime::now())
            }
            Overlay::Ask(a) => {
                let avail = rows.saturating_sub(1).saturating_sub(used);
                a.sel.render(w, avail.saturating_sub(1))
            }
            other => match other.select() {
                Some(sel) => sel.render(w, (rows / 2).max(6).saturating_sub(2)),
                None => (Vec::new(), (0, 0)),
            },
        };
        let mut framed: Vec<crate::text::line::Line> = vec![Vec::new()];
        framed.extend(body);
        (framed, (cur.0 + 1, cur.1))
    }

    pub fn on_paste(&mut self, s: &str) {
        match self {
            Overlay::Review { review, .. } => review.on_paste(s),
            Overlay::Jobs { st } => st.on_paste(s),
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
            Overlay::Model { mut sel, ids, assign, target } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Model { sel, ids, assign, target }), None),
                SelectResult::Chosen(i) => {
                    let ev = i.first().and_then(|i| ids.get(*i)).map(|id| {
                        if assign && !id.starts_with('@') {
                            match target {
                                Some(role) => OverlayEvent::AssignRole { role: Some(role), model: id.clone() },
                                None => OverlayEvent::PickRole(id.clone()),
                            }
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
            Overlay::KeyChoice { mut sel, provider, options } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::KeyChoice { sel, provider, options }), None),
                SelectResult::Chosen(i) => {
                    let choice = i.first().and_then(|i| options.get(*i)).cloned().unwrap_or(KeyOption::Cancel);
                    (None, Some(OverlayEvent::Key { provider, choice }))
                }
                _ => (None, Some(OverlayEvent::Key { provider, choice: KeyOption::Cancel })),
            },
            Overlay::Logout { mut sel, providers } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Logout { sel, providers }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| providers.get(*i)).cloned().map(OverlayEvent::Logout))
                }
                _ => (None, None),
            },
            Overlay::Mcp { mut sel, pick } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Mcp { sel, pick }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::Mcp(pick, i.first().copied()))),
                _ => (None, Some(OverlayEvent::Mcp(pick, None))),
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
            Overlay::Jobs { mut st } => match st.on_key(k) {
                JobsResult::Pending => (Some(Overlay::Jobs { st }), None),
                JobsResult::Close => (None, None),
                JobsResult::Kill(id) => (Some(Overlay::Jobs { st }), Some(OverlayEvent::KillJobs(id))),
                JobsResult::Notify { session, text } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::NotifySession { session, text }))
                }
                JobsResult::Stop { session, pid, started, kind } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::StopSession { session, pid, started, kind }))
                }
                JobsResult::Terminate { session, pid, started } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::TerminateSession { session, pid, started }))
                }
                JobsResult::Take { session } => (None, Some(OverlayEvent::TakeSession(session))),
            },
            Overlay::Session { mut sel, paths } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Session { sel, paths }), None),
                SelectResult::Chosen(i) => {
                    (None, i.first().and_then(|i| paths.get(*i)).map(|p| OverlayEvent::Resume(p.clone())))
                }
                _ => (None, None),
            },
            Overlay::Plans { mut sel, ids, deliver } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Plans { sel, ids, deliver }), None),
                SelectResult::Chosen(i) => {
                    let id = i.first().and_then(|i| ids.get(*i)).cloned();
                    (
                        None,
                        id.map(|id| if deliver { OverlayEvent::DeliverPlan(id) } else { OverlayEvent::OpenPlan(id) }),
                    )
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
            Overlay::Ask(a) => a.key(k),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use mu_base::events::AskOption;
    use mu_testutil::key;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect()
    }

    #[test]
    fn choosing_a_model_yields_an_event_and_closes() {
        let ov = Overlay::Model {
            sel: Select::new("m", items(&["a/x", "@smol"])),
            ids: vec!["a/x".into(), "@smol".into()],
            assign: true,
            target: None,
        };
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert!(matches!(ev, Some(OverlayEvent::PickRole(m)) if m == "a/x"), "a concrete model asks for a role");
        let ov = Overlay::Model {
            sel: Select::new("m", items(&["@smol"])),
            ids: vec!["@smol".into()],
            assign: true,
            target: None,
        };
        assert!(matches!(ov.on_key(key(KeyCode::Enter)).1, Some(OverlayEvent::SetModel(m)) if m == "@smol"));
        let ov = Overlay::Model {
            sel: Select::new("m", items(&["a/x"])),
            ids: vec!["a/x".into()],
            assign: true,
            target: Some("review".into()),
        };
        assert!(
            matches!(ov.on_key(key(KeyCode::Enter)).1, Some(OverlayEvent::AssignRole { role: Some(r), model }) if r == "review" && model == "a/x"),
            "a picker opened for a role assigns it without asking again"
        );
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
            context: None,
            header: None,
            options: vec![],
            multi: false,
            recommended: None,
            confirm: false,
        };
        let (tx, mut rx) = oneshot::channel();
        let qs = vec![q("one"), q("two")];
        let ov = Overlay::new_ask(qs, tx);
        let (rest, _) = ov.on_key(key(KeyCode::Esc));
        assert!(rest.is_none());
        let all = rx.try_recv().unwrap().unwrap();
        assert_eq!(all.len(), 2, "cancelling answers every question");
    }

    #[test]
    fn ask_picker_shows_the_highlighted_description_below_the_options() {
        let opt = |label: &str, d: Option<&str>| AskOption {
            label: label.into(),
            description: d.map(Into::into),
            preview: None,
            kind: None,
        };
        let q = AskQuestion {
            question: "Which?".into(),
            context: None,
            header: None,
            options: vec![opt("Yes", Some("apply the change now and run the tests afterwards")), opt("No", None)],
            multi: false,
            recommended: None,
            confirm: false,
        };
        let (lines, _) = ask_select(&q).render(40, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t[2].trim_end(), "❯ Yes", "{t:?}");
        assert_eq!(t[3], "    apply the change now and run the", "{t:?}");
        assert_eq!(t[4], "    tests afterwards", "{t:?}");
        assert_eq!(t[5], "  No", "{t:?}");
    }

    #[test]
    fn ask_picker_shows_a_multi_line_question_in_full() {
        let q = AskQuestion {
            question: "Deploy the canary?\nstatus: 3 services green\nerrors: none in the last hour".into(),
            context: None,
            header: None,
            options: vec![
                AskOption { label: "Yes".into(), description: None, preview: None, kind: None },
                AskOption { label: "No".into(), description: None, preview: None, kind: None },
            ],
            multi: false,
            recommended: None,
            confirm: false,
        };
        let (lines, cur) = ask_select(&q).render(30, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(
            t[..3],
            ["Deploy the canary?", "status: 3 services green", "errors: none in the last hour"],
            "{t:?}"
        );
        assert_eq!(t[3].trim_end(), "›", "{t:?}");
        assert_eq!(cur, (3, 2));
        assert!(t[4].starts_with("❯ Yes"), "{t:?}");
        assert!(lines.iter().all(|l| crate::text::line::line_width(l) <= 30), "{t:?}");
    }

    #[test]
    fn ask_picker_shows_the_context_above_the_question() {
        let q = AskQuestion {
            question: "Keep the old API?".into(),
            context: Some("Three callers in `src/ui` still use `Foo::old`; removing it breaks the build.".into()),
            header: None,
            options: vec![
                AskOption { label: "Keep".into(), description: None, preview: None, kind: None },
                AskOption { label: "Remove".into(), description: None, preview: None, kind: None },
            ],
            multi: false,
            recommended: None,
            confirm: false,
        };
        let (lines, cur) = ask_select(&q).render(30, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t[0], "Three callers in `src/ui`", "{t:?}");
        let q_at = t.iter().position(|l| l == "Keep the old API?").expect("question row");
        assert!(q_at >= 3, "the context comes first: {t:?}");
        assert_eq!(cur.0, q_at + 1, "{t:?}");
        assert!(lines.iter().all(|l| crate::text::line::line_width(l) <= 30), "{t:?}");
        let (lines, _) = ask_select(&q).render(30, 4);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(t.iter().any(|l| l == "Keep the old API?"), "short screens keep the question: {t:?}");
    }

    fn ask_ov(q: AskQuestion) -> (Overlay, oneshot::Receiver<Option<Vec<Answer>>>) {
        let (tx, rx) = oneshot::channel();
        (Overlay::new_ask(vec![q], tx), rx)
    }

    fn ask_q(labels: &[&str], multi: bool, confirm: bool, recommended: Option<usize>) -> AskQuestion {
        let options =
            labels.iter().map(|l| AskOption { label: l.to_string(), description: None, preview: None, kind: None });
        AskQuestion {
            question: "Q?".into(),
            context: None,
            header: None,
            options: options.collect(),
            multi,
            recommended,
            confirm,
        }
    }

    /// Feeds keys to an open ask, which must stay open.
    fn press(ov: Overlay, keys: &[KeyCode]) -> Overlay {
        keys.iter().fold(ov, |ov, k| ov.on_key(key(*k)).0.expect("still open"))
    }

    fn typed(s: &str) -> Vec<KeyCode> {
        s.chars().map(KeyCode::Char).collect()
    }

    fn answer(rx: &mut oneshot::Receiver<Option<Vec<Answer>>>) -> String {
        let all = rx.try_recv().expect("replied").expect("answered");
        format!("{all:?}")
    }

    #[test]
    fn note_on_the_highlighted_option_is_submitted_with_it() {
        let (ov, mut rx) = ask_ov(ask_q(&["Skip", "Keep"], false, false, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Right]);
        let ov = press(ov, &typed("tests later"));
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none() && ev.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["Keep"], note: "tests later" }]"#);
    }

    #[test]
    fn esc_leaves_the_note_open_and_enter_without_text_picks_plainly() {
        let (ov, mut rx) = ask_ov(ask_q(&["Skip", "Keep"], false, false, None));
        let ov = press(ov, &[KeyCode::Right, KeyCode::Char('x'), KeyCode::Esc]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Skip"])]"#);
    }

    #[test]
    fn multi_note_goes_with_the_list_of_picks() {
        let (ov, mut rx) = ask_ov(ask_q(&["X", "Y", "Z"], true, false, None));
        let ov = press(ov, &[KeyCode::Tab, KeyCode::Down, KeyCode::Right]);
        let ov = press(ov, &typed("both"));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["X"], note: "both" }]"#);
    }

    #[test]
    fn confirm_answers_on_y_or_n_at_once() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Char('n'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["No"])]"#);
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Char('Y'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Yes"])]"#);
    }

    #[test]
    fn confirm_enter_takes_the_recommended_side_and_defaults_to_yes() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Yes"])]"#);
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, Some(1)));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["No"])]"#);
    }

    #[test]
    fn confirm_tab_adds_a_note_before_submitting() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Tab]);
        let ov = press(ov, &typed("because"));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["No"], note: "because" }]"#);
    }

    #[test]
    fn confirm_esc_cancels_like_any_ask() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Cancelled]");
    }

    #[test]
    fn digit_keys_pick_an_option_in_single_select() {
        let (ov, mut rx) = ask_ov(ask_q(&["A", "B", "C"], false, false, None));
        assert!(ov.on_key(key(KeyCode::Char('2'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["B"])]"#);
    }

    #[test]
    fn other_still_takes_custom_text() {
        let (ov, mut rx) = ask_ov(ask_q(&["A"], false, false, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        assert!(press(ov, &typed("hi")).on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Custom("hi")]"#);
    }

    #[test]
    fn picker_footer_names_the_keys() {
        let (lines, _) = ask_select(&ask_q(&["Yes", "No"], false, true, None)).render(60, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t.last().map(String::as_str), Some("  y yes · n no · Enter choose · Tab/→ note · Esc cancel"));
    }

    #[test]
    fn every_option_shows_its_preview_and_a_declared_kind_labels_the_box() {
        let mut q = ask_q(&["A", "B"], false, false, None);
        q.options[0].preview = Some("fn a() {}".into());
        q.options[1].preview = Some("plain words".into());
        q.options[1].kind = Some(mu_base::events::PreviewKind::Diff);
        let (lines, _) = ask_select(&q).render(40, 30);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(t[3].contains("┌─ code ─") && t[4].contains("fn a() {}"), "{t:?}");
        assert!(t[7].contains("┌─ diff ─") && t[8].contains("plain words"), "{t:?}");
    }

    /// Two questions with two options each, for the back keys and the review.
    fn two_q() -> Vec<AskQuestion> {
        vec![ask_q(&["A", "B"], false, false, None), ask_q(&["X", "Y"], false, false, None)]
    }

    fn open(qs: Vec<AskQuestion>) -> (Overlay, oneshot::Receiver<Option<Vec<Answer>>>) {
        let (tx, rx) = oneshot::channel();
        (Overlay::new_ask(qs, tx), rx)
    }

    /// The overlay's rows as text, trailing spaces dropped.
    fn screen(ov: &mut Overlay) -> Vec<String> {
        let (lines, _) = ov.render(100, 30, 0);
        lines.iter().map(|l| crate::text::line::line_text(l).trim_end().to_string()).collect()
    }

    #[test]
    fn left_and_shift_tab_go_back_and_restore_the_answer() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::Left]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l == "❯ B"), "the earlier answer is preselected: {t:?}");
        assert!(t.iter().any(|l| l.contains("1/2")), "{t:?}");
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Down, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("2/2")), "Shift-Tab on the review goes back to question two");
        let ov = press(ov, &[KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["B"]), Options(["Y"])]"#);
    }

    #[test]
    fn left_with_text_in_the_filter_moves_the_cursor_instead() {
        let (ov, _rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter]);
        let ov = press(ov, &typed("x"));
        let mut ov = press(ov, &[KeyCode::Left]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("2/2")), "still on question two");
    }

    #[test]
    fn shift_tab_restores_notes_and_custom_text() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Right]);
        let ov = press(ov, &typed("why"));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(screen(&mut ov).iter().any(|l| l == "    note: why"), "the note comes back under its option");
        let ov = press(ov, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        let ov = press(ov, &typed("hi"));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(screen(&mut ov).iter().any(|l| l == "❯ hi"), "the custom answer comes back on the free row");
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::Enter]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("Review your answers")));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Custom("hi"), Options(["X"])]"#);
    }

    #[test]
    fn review_reopens_a_question_and_returns_to_the_review() {
        let (ov, mut rx) = open(two_q());
        let mut ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("1. Q?") && l.ends_with('A')), "{t:?}");
        assert!(t.iter().any(|l| l.contains("2. Q?") && l.ends_with('X')), "{t:?}");
        assert!(t.iter().any(|l| l == "❯ Submit answers"), "Submit starts highlighted: {t:?}");
        let mut ov = press(ov, &[KeyCode::Up, KeyCode::Enter]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("2/2")), "{t:?}");
        assert!(t.iter().any(|l| l == "❯ X"), "the earlier answer is preselected: {t:?}");
        let mut ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("2. Q?") && l.ends_with('Y')));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["A"]), Options(["Y"])]"#);
    }

    #[test]
    fn esc_cancels_the_ask_from_the_review_and_from_any_question() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Cancelled, Cancelled]", "Esc on the review cancels everything");
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Options([\"A\"]), Cancelled]", "Esc on question 2 cancels like Ctrl-C");
    }

    #[test]
    fn left_steps_back_from_the_review_and_then_from_each_question() {
        let (ov, _rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter, KeyCode::Left]);
        let mut ov = ov;
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("2/2")) && t.iter().any(|l| l == "❯ X"), "{t:?}");
        let mut ov = press(ov, &[KeyCode::Left]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("1/2")) && t.iter().any(|l| l == "❯ A"), "{t:?}");
    }

    #[test]
    fn ctrl_c_cancels_from_the_current_question_and_keeps_earlier_answers() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter]);
        assert!(ov.on_key(ctrl_c).0.is_none());
        assert_eq!(answer(&mut rx), "[Options([\"A\"]), Cancelled]");
    }

    #[test]
    fn footers_name_the_back_and_cancel_keys_per_step() {
        let (ov, _rx) = open(two_q());
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.ends_with("Esc cancel")), "{:?}", screen(&mut ov));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.ends_with("⇧Tab back · Esc cancel")));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.contains("Enter reopen or submit")));
    }

    #[test]
    fn a_single_question_shows_no_position_and_no_review() {
        let (ov, mut rx) = open(vec![ask_q(&["A", "B"], false, false, None)]);
        let mut ov = ov;
        assert!(!screen(&mut ov).iter().any(|l| l.contains("/1")));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["A"])]"#);
    }

    #[test]
    fn going_back_restores_a_multi_select_set() {
        let qs = vec![ask_q(&["X", "Y", "Z"], true, false, None), ask_q(&["P", "Q"], false, false, None)];
        let (ov, mut rx) = open(qs);
        let ov = press(ov, &[KeyCode::Tab, KeyCode::Down, KeyCode::Tab, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l == "❯ [x] X"), "{t:?}");
        assert!(t.iter().any(|l| l == "  [x] Y"), "{t:?}");
        assert!(t.iter().any(|l| l == "  [ ] Z"), "{t:?}");
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["X", "Y"]), Options(["P"])]"#);
    }
}
