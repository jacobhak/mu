//! The `/web` setup flow as a state machine: choose where searches go, approve a keyless third
//! party, or add the key first. Pure: the app shows each `Step` and feeds back an `Answer`.

use mu_provider::auth::store::now;
use mu_provider::auth::web::{SearchBackend, WebSearch, about, key_id, label};

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    /// Shown under the list while the row is highlighted.
    pub about: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Pick {
        title: String,
        context: String,
        items: Vec<PickItem>,
    },
    /// The key for this key-store id must be added before the backend can be used.
    NeedKey(&'static str),
    Done(WebSearch),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Pick(usize),
    KeySaved,
    KeyCancelled,
    /// Esc on a picker.
    Cancel,
}

type Choice = (SearchBackend, bool);

/// The backend rows, in the order they are offered. No row is recommended.
const CHOICES: [Choice; 7] = [
    (SearchBackend::Parallel, true),
    (SearchBackend::Parallel, false),
    (SearchBackend::Exa, true),
    (SearchBackend::Exa, false),
    (SearchBackend::Anthropic, false),
    (SearchBackend::Openai, false),
    (SearchBackend::Off, false),
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Backend,
    Approve(Choice),
    NeedKey(Choice),
    Done,
    Cancelled,
}

pub struct Wizard {
    phase: Phase,
    current: Option<WebSearch>,
    has_key: Box<dyn Fn(&str) -> bool>,
    done: Option<WebSearch>,
}

impl Wizard {
    pub fn new(current: Option<WebSearch>, has_key: impl Fn(&str) -> bool + 'static) -> Self {
        Self { phase: Phase::Backend, current, has_key: Box::new(has_key), done: None }
    }

    fn keyed_ready(&self, b: SearchBackend) -> bool {
        key_id(b).is_some_and(|id| (self.has_key)(id))
    }

    pub fn step(&self) -> Step {
        match self.phase {
            Phase::Backend => self.backend_step(),
            Phase::Approve((b, _)) => Step::Pick {
                title: format!("Approve {}'s free endpoint?", if b == SearchBackend::Exa { "Exa" } else { "Parallel" }),
                context: about(b, true).to_string(),
                items: ["Approve", "Back", "Cancel"]
                    .iter()
                    .map(|l| PickItem { label: (*l).into(), detail: String::new(), about: String::new() })
                    .collect(),
            },
            Phase::NeedKey((b, _)) => Step::NeedKey(key_id(b).unwrap_or_default()),
            Phase::Done => self.done.clone().map_or(Step::Cancelled, Step::Done),
            Phase::Cancelled => Step::Cancelled,
        }
    }

    fn backend_step(&self) -> Step {
        let mut context = "mu sends the query the model writes, never your files or conversation, to the backend you choose. Nothing is contacted until you choose. Change this any time with /web.".to_string();
        if let Some(c) = &self.current {
            let when = c.approved.as_deref().filter(|_| c.keyless).map(|d| format!(" (approved {d})"));
            context.push_str(&format!("\nNow: {}{}", label(c.backend, c.keyless), when.unwrap_or_default()));
        }
        let items = CHOICES
            .iter()
            .map(|&(b, keyless)| {
                let detail = match (b, keyless) {
                    (SearchBackend::Off, _) => "nothing is contacted",
                    (_, true) => "approve next",
                    _ if self.keyed_ready(b) => "key saved",
                    _ => "add a key next",
                };
                PickItem { label: label(b, keyless).into(), detail: detail.into(), about: about(b, keyless).into() }
            })
            .collect();
        Step::Pick { title: "Web search: choose where searches go".into(), context, items }
    }

    pub fn answer(&mut self, a: Answer) {
        if a == Answer::Cancel {
            self.phase = Phase::Cancelled;
            return;
        }
        match (self.phase, a) {
            (Phase::Backend, Answer::Pick(i)) => {
                let Some(&(b, keyless)) = CHOICES.get(i) else { return };
                self.phase = if b == SearchBackend::Off {
                    self.finish(WebSearch::off())
                } else if keyless {
                    Phase::Approve((b, keyless))
                } else if self.keyed_ready(b) {
                    self.finish(WebSearch::keyed(b))
                } else {
                    Phase::NeedKey((b, keyless))
                };
            }
            (Phase::Approve((b, _)), Answer::Pick(0)) => self.phase = self.finish(WebSearch::keyless(b, now())),
            (Phase::Approve(_), Answer::Pick(1)) => self.phase = Phase::Backend,
            (Phase::Approve(_), Answer::Pick(2)) => self.phase = Phase::Cancelled,
            (Phase::NeedKey((b, _)), Answer::KeySaved) => self.phase = self.finish(WebSearch::keyed(b)),
            (Phase::NeedKey(_), Answer::KeyCancelled) => self.phase = Phase::Backend,
            _ => {}
        }
    }

    fn finish(&mut self, w: WebSearch) -> Phase {
        self.done = Some(w);
        Phase::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard(keys: &'static [&'static str]) -> Wizard {
        Wizard::new(None, move |id| keys.contains(&id))
    }

    fn pick(w: &Wizard) -> (String, String, Vec<PickItem>) {
        let Step::Pick { title, context, items } = w.step() else { panic!("not a picker: {:?}", w.step()) };
        (title, context, items)
    }

    #[test]
    fn the_backend_rows_come_in_order_with_no_recommendation() {
        let w = wizard(&["exa"]);
        let (title, context, items) = pick(&w);
        assert_eq!(title, "Web search: choose where searches go");
        assert!(context.contains("Nothing is contacted until you choose") && !context.contains("Now:"));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Parallel (no key)",
                "Parallel (API key)",
                "Exa (no key)",
                "Exa (API key)",
                "Anthropic web search (API key)",
                "OpenAI web search (API key)",
                "Off"
            ]
        );
        let details: Vec<&str> = items.iter().map(|i| i.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "approve next",
                "add a key next",
                "approve next",
                "key saved",
                "add a key next",
                "add a key next",
                "nothing is contacted"
            ]
        );
        assert!(items.iter().all(|i| !i.about.is_empty() && !i.label.contains("ecommend")));
        assert_eq!(items[0].about, about(SearchBackend::Parallel, true));
    }

    #[test]
    fn keyless_parallel_goes_through_approval() {
        let mut w = wizard(&[]);
        w.answer(Answer::Pick(0));
        let (title, context, items) = pick(&w);
        assert_eq!(title, "Approve Parallel's free endpoint?");
        assert_eq!(context, about(SearchBackend::Parallel, true));
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Approve", "Back", "Cancel"]);
        w.answer(Answer::Pick(0));
        let Step::Done(s) = w.step() else { panic!() };
        assert!(s.keyless && s.backend == SearchBackend::Parallel && s.approved.is_some());
    }

    #[test]
    fn back_returns_to_the_backends_and_cancel_ends() {
        let mut w = wizard(&[]);
        w.answer(Answer::Pick(2));
        assert_eq!(pick(&w).0, "Approve Exa's free endpoint?");
        w.answer(Answer::Pick(1));
        assert_eq!(pick(&w).0, "Web search: choose where searches go");
        w.answer(Answer::Pick(2));
        w.answer(Answer::Pick(2));
        assert_eq!(w.step(), Step::Cancelled);
    }

    #[test]
    fn a_keyed_backend_without_a_key_asks_for_it_first() {
        let mut w = wizard(&[]);
        w.answer(Answer::Pick(4));
        assert_eq!(w.step(), Step::NeedKey("anthropic"));
        w.answer(Answer::KeyCancelled);
        assert_eq!(pick(&w).0, "Web search: choose where searches go");
        w.answer(Answer::Pick(4));
        w.answer(Answer::KeySaved);
        assert_eq!(w.step(), Step::Done(WebSearch::keyed(SearchBackend::Anthropic)));
    }

    #[test]
    fn a_stored_key_finishes_at_once_and_off_is_a_choice() {
        let mut w = wizard(&["openai"]);
        w.answer(Answer::Pick(5));
        assert_eq!(w.step(), Step::Done(WebSearch::keyed(SearchBackend::Openai)));
        let mut w = wizard(&[]);
        w.answer(Answer::Pick(6));
        assert_eq!(w.step(), Step::Done(WebSearch::off()));
    }

    #[test]
    fn escape_cancels_and_the_current_choice_is_shown() {
        let mut w = wizard(&[]);
        w.answer(Answer::Cancel);
        assert_eq!(w.step(), Step::Cancelled);
        let cur = WebSearch::keyless(SearchBackend::Exa, "2026-01-01T00:00:00Z".into());
        let w = Wizard::new(Some(cur), |_| false);
        assert!(pick(&w).1.ends_with("Now: Exa (no key) (approved 2026-01-01T00:00:00Z)"));
    }
}
