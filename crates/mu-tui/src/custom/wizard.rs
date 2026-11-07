//! The "add a custom OpenAI-compatible endpoint" flow as a state machine: text lines for the id,
//! label, base URL, key and model limits, pickers for the choices. Pure: the app shows each `Step`,
//! runs the one network check the wizard asks for (`Step::Fetch`) and feeds back an `Answer`. Only
//! `Step::Save` leaves the wizard, and the app is what writes it.

use mu_provider::auth::store::{Entry, cmd_label};
use mu_provider::model::discovery::CustomRow;
use mu_provider::provider::custom::{self, AuthMode, CustomModel, CustomProvider, DEFAULT_CONTEXT, DEFAULT_OUTPUT};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// A line of text. `secret` takes it masked. An empty answer takes `default` when there is one.
    Text { prompt: String, secret: bool, default: Option<String> },
    /// A picker; answer with `Answer::Pick(index)`.
    Pick { title: String, context: String, items: Vec<PickItem> },
    /// Read the endpoint's models with this credential; answer with `Answer::Fetched`.
    Fetch(Probe),
    /// Save this. Only the review's Save gets here.
    Save(Box<Saved>),
    /// Finished without saving.
    Cancelled,
}

/// What a check reads: the endpoint, and its credential (`None` when it takes none).
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub id: String,
    pub base_url: String,
    pub credential: Option<Entry>,
}

/// What Save writes. `listed` is the listing that was read; it is cached once the definition is saved.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub id: String,
    pub provider: CustomProvider,
    pub entry: Option<Entry>,
    pub listed: Option<Vec<CustomRow>>,
}

/// The files a save writes, named in the review.
#[derive(Debug, Clone, PartialEq)]
pub struct Destinations {
    pub providers: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fetch {
    Listed(Vec<CustomRow>),
    /// The credential could not be resolved: a variable is not set, a command failed.
    Credential(String),
    /// The endpoint refused the credential (401 or 403).
    Rejected(String),
    /// Anything else: unreachable, another status, or a listing mu cannot read.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Pick(usize),
    Text(String),
    Fetched(Fetch),
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
enum Auth {
    /// The entry the key comes from, and how the review names it (never the key itself).
    Bearer {
        entry: Entry,
        source: String,
    },
    Keyless,
}

/// A model's (context window, output limit) as the endpoint lists them.
type Limits = (Option<u32>, Option<u32>);

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Id,
    Label,
    Base,
    Auth,
    Key,
    EnvName,
    Command,
    Fetching,
    /// `/models` could not be read: retry, type a model id, or continue unvalidated.
    Unlisted,
    /// The listed models, and a row to type one instead.
    Model,
    ModelId,
    Context(String, Limits),
    Output(String, u32, Option<u32>),
    More,
    Review,
    Done,
    Cancelled,
}

pub struct Wizard {
    phase: Phase,
    dests: Destinations,
    taken: Box<dyn Fn(&str) -> bool>,
    id: String,
    label: Option<String>,
    base_url: String,
    auth: Option<Auth>,
    /// The endpoint's listing, when it was read.
    listed: Option<Vec<CustomRow>>,
    /// Why the listing was not read.
    unlisted: String,
    models: BTreeMap<String, CustomModel>,
    /// Lines to print before the next step (a refused key, say); drained by `take_notes`.
    notes: Vec<String>,
}

impl Wizard {
    /// Starts at the provider id. `taken` says whether an id is in use (a definition or a stored key).
    pub fn new(dests: Destinations, taken: impl Fn(&str) -> bool + 'static) -> Self {
        Self {
            phase: Phase::Id,
            dests,
            taken: Box::new(taken),
            id: String::new(),
            label: None,
            base_url: String::new(),
            auth: None,
            listed: None,
            unlisted: String::new(),
            models: BTreeMap::new(),
            notes: Vec::new(),
        }
    }

    /// Whether the wizard waits for a line of text, which the composer takes.
    pub fn awaits_text(&self) -> bool {
        matches!(self.step(), Step::Text { .. })
    }

    /// Whether a model check is running; input waits for its answer.
    pub fn busy(&self) -> bool {
        self.phase == Phase::Fetching
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    pub fn step(&self) -> Step {
        match &self.phase {
            Phase::Id => text("Provider id: lowercase letters, digits, `-` and `_` (e.g. modal)", false, None),
            Phase::Label => text("Label for the pickers (optional; Enter uses the id)", false, Some(String::new())),
            Phase::Base => {
                text("API base URL; include /v1 when the endpoint needs it (e.g. https://host/v1)", false, None)
            }
            Phase::Auth => Step::Pick {
                title: "How does the endpoint take its key?".into(),
                context: "A key goes to the auth store under this id, never into providers.toml.".into(),
                items: vec![
                    item("Paste a key", "typed now, shown as bullets"),
                    item("Use an environment variable", "read when mu needs it; it may be unset now"),
                    item("Use a command", "a command that prints the key, run through sh"),
                    item("No authentication", "requests carry no key"),
                ],
            },
            Phase::Key => text("Paste the API key and press Enter (shown as bullets)", true, None),
            Phase::EnvName => text("Environment variable that holds the key (e.g. MODAL_API_KEY)", false, None),
            Phase::Command => text("Command that prints the key, e.g. `pass show modal` (run through sh)", false, None),
            Phase::Fetching => Step::Fetch(self.probe()),
            Phase::Unlisted => Step::Pick {
                title: "The endpoint did not list its models".into(),
                context: format!("{}\nNothing is saved until you choose Save.", self.unlisted),
                items: vec![
                    item("Retry", &format!("ask {}/models again", self.base_url)),
                    item("Enter model manually", "type the model id; the listing is not used"),
                    item("Continue unvalidated", "save without the listing; type the model id"),
                ],
            },
            Phase::Model => {
                let listed: &[CustomRow] = self.listed.as_deref().unwrap_or(&[]);
                let mut items: Vec<PickItem> = listed.iter().map(|r| item(&r.id, &listed_detail(r))).collect();
                items.push(item("Enter a model id manually", "for one the listing does not show"));
                Step::Pick {
                    title: "Choose a model".into(),
                    context: format!("{} models listed at {}/models", listed.len(), self.base_url),
                    items,
                }
            }
            Phase::ModelId => text("Model id, as the endpoint names it (e.g. org/model)", false, None),
            Phase::Context(id, (ctx, _)) => {
                let d = ctx.unwrap_or(DEFAULT_CONTEXT);
                text(&format!("Context window of {id}, in tokens [{d}]"), false, Some(d.to_string()))
            }
            Phase::Output(id, window, out) => {
                let d = out.unwrap_or(DEFAULT_OUTPUT).min(*window);
                text(&format!("Output limit of {id}, in tokens, at most {window} [{d}]"), false, Some(d.to_string()))
            }
            Phase::More => Step::Pick {
                title: "Models for this endpoint".into(),
                context: self.models_line(),
                items: vec![
                    item("Continue", "review and save"),
                    item("Add another model", "pick one more from the listing or type it"),
                ],
            },
            Phase::Review => Step::Pick {
                title: "Save this endpoint?".into(),
                context: self.summary(),
                items: vec![item("Save", "writes the definition and the key"), item("Cancel", "nothing is written")],
            },
            Phase::Done => Step::Save(Box::new(self.saved())),
            Phase::Cancelled => Step::Cancelled,
        }
    }

    /// `Err(message)`: the answer was not usable and `step()` is the same as before.
    pub fn answer(&mut self, a: Answer) -> Result<(), String> {
        if a == Answer::Cancel {
            self.phase = Phase::Cancelled;
            return Ok(());
        }
        let phase = self.phase.clone();
        match (phase, a) {
            (Phase::Id, Answer::Text(s)) => self.set_id(&s)?,
            (Phase::Label, Answer::Text(s)) => {
                self.label = Some(s.trim().to_string()).filter(|l| !l.is_empty());
                self.phase = Phase::Base;
            }
            (Phase::Base, Answer::Text(s)) => {
                self.base_url = custom::check_base_url(s.trim()).map_err(|e| format!("{e:#}"))?;
                self.phase = Phase::Auth;
            }
            (Phase::Auth, Answer::Pick(i)) => {
                self.phase = match i {
                    0 => Phase::Key,
                    1 => Phase::EnvName,
                    2 => Phase::Command,
                    3 => {
                        self.auth = Some(Auth::Keyless);
                        Phase::Fetching
                    }
                    _ => return Err("choose one of the options".into()),
                };
            }
            (Phase::Key, Answer::Text(s)) => {
                let key = s.trim();
                if key.is_empty() {
                    return Err("the key is empty; paste it, or Esc cancels".into());
                }
                if key.contains('\n') {
                    return Err("a key is one line".into());
                }
                self.auth = Some(Auth::Bearer { entry: Entry::literal(key), source: "pasted key (not shown)".into() });
                self.phase = Phase::Fetching;
            }
            (Phase::EnvName, Answer::Text(s)) => {
                let source = format!("${}", s.trim());
                let entry = Entry::reference(&source).map_err(|e| e.to_string())?;
                self.auth = Some(Auth::Bearer { entry, source });
                self.phase = Phase::Fetching;
            }
            (Phase::Command, Answer::Text(s)) => {
                let cmd = s.trim();
                let cmd = if cmd.starts_with('!') { cmd.to_string() } else { format!("!{cmd}") };
                let entry = Entry::reference(&cmd).map_err(|e| e.to_string())?;
                self.auth = Some(Auth::Bearer { entry, source: cmd_label(cmd.trim_start_matches('!')) });
                self.phase = Phase::Fetching;
            }
            (Phase::Fetching, Answer::Fetched(f)) => self.fetched(f),
            (Phase::Unlisted, Answer::Pick(0)) => self.phase = Phase::Fetching,
            (Phase::Unlisted, Answer::Pick(1 | 2)) => self.phase = Phase::ModelId,
            (Phase::Model, Answer::Pick(i)) => {
                let listed: &[CustomRow] = self.listed.as_deref().unwrap_or(&[]);
                match listed.get(i) {
                    Some(row) => {
                        self.phase = Phase::Context(row.id.clone(), (row.context_length, row.max_completion_tokens))
                    }
                    None if i == listed.len() => self.phase = Phase::ModelId,
                    None => return Err("choose one of the options".into()),
                }
            }
            (Phase::ModelId, Answer::Text(s)) => {
                let id = s.trim();
                if id.is_empty() {
                    return Err("a model id is needed".into());
                }
                self.phase = Phase::Context(id.to_string(), (None, None));
            }
            (Phase::Context(id, (ctx, out)), Answer::Text(s)) => {
                let window = tokens(&s, ctx.unwrap_or(DEFAULT_CONTEXT), "the context window")?;
                self.phase = Phase::Output(id, window, out);
            }
            (Phase::Output(id, window, out), Answer::Text(s)) => {
                let max = tokens(&s, out.unwrap_or(DEFAULT_OUTPUT).min(window), "the output limit")?;
                if max > window {
                    return Err(format!("the output limit ({max}) must not exceed the context window ({window})"));
                }
                self.models.insert(
                    id,
                    CustomModel { context_window: Some(window), max_tokens: Some(max), ..Default::default() },
                );
                self.phase = Phase::More;
            }
            (Phase::More, Answer::Pick(0)) => self.phase = Phase::Review,
            (Phase::More, Answer::Pick(1)) => {
                self.phase = if self.listed.is_some() { Phase::Model } else { Phase::ModelId }
            }
            (Phase::Review, Answer::Pick(0)) => self.phase = Phase::Done,
            (Phase::Review, Answer::Pick(1)) => self.phase = Phase::Cancelled,
            (Phase::Fetching, _) => return Err("still checking the endpoint".into()),
            (_, Answer::Text(_)) => return Err("choose one of the options".into()),
            (_, Answer::Pick(_)) => return Err("type an answer".into()),
            _ => return Err("that answer does not fit this step".into()),
        }
        Ok(())
    }

    fn set_id(&mut self, s: &str) -> Result<(), String> {
        let id = s.trim();
        custom::check_id(id).map_err(|e| format!("{e:#}"))?;
        if (self.taken)(id) {
            return Err(format!("`{id}` is already in use by a custom endpoint or a stored key; pick another id"));
        }
        self.id = id.to_string();
        self.phase = Phase::Label;
        Ok(())
    }

    fn fetched(&mut self, f: Fetch) {
        match f {
            Fetch::Listed(rows) => {
                self.listed = Some(rows);
                self.phase = Phase::Model;
            }
            Fetch::Credential(m) | Fetch::Rejected(m) => {
                self.notes.push(format!("{m}. Nothing was saved; choose the key again."));
                self.auth = None;
                self.phase = Phase::Auth;
            }
            Fetch::Failed(m) => {
                self.unlisted = m;
                self.phase = Phase::Unlisted;
            }
        }
    }

    fn probe(&self) -> Probe {
        let credential = match &self.auth {
            Some(Auth::Bearer { entry, .. }) => Some(entry.clone()),
            _ => None,
        };
        Probe { id: self.id.clone(), base_url: self.base_url.clone(), credential }
    }

    fn saved(&self) -> Saved {
        let mut provider = CustomProvider::new(&self.base_url);
        provider.label = self.label.clone();
        provider.auth = if matches!(self.auth, Some(Auth::Keyless)) { AuthMode::None } else { AuthMode::Bearer };
        provider.models = self.models.clone();
        Saved { id: self.id.clone(), provider, entry: self.probe().credential, listed: self.listed.clone() }
    }

    fn auth_text(&self) -> String {
        match &self.auth {
            Some(Auth::Bearer { source, .. }) => source.clone(),
            Some(Auth::Keyless) => "none (no key)".into(),
            None => "not chosen".into(),
        }
    }

    fn models_line(&self) -> String {
        if self.models.is_empty() {
            return "No models added yet.".into();
        }
        let names: Vec<&str> = self.models.keys().map(String::as_str).collect();
        format!("Models: {}", names.join(", "))
    }

    /// The redacted review: the id, base URL, where the key comes from (never the key or a
    /// command's arguments), the models, the validation state and the files a save writes.
    fn summary(&self) -> String {
        let mut s = format!(
            "Provider id: {}\nLabel: {}\nBase URL: {}\nAuth: {}\nModels:\n",
            self.id,
            self.label.as_deref().unwrap_or(&self.id),
            self.base_url,
            self.auth_text()
        );
        for (id, m) in &self.models {
            s.push_str(&format!(
                "  {id}: context {} · output {}\n",
                m.context_window.unwrap_or_default(),
                m.max_tokens.unwrap_or_default()
            ));
        }
        match &self.listed {
            Some(rows) => s.push_str(&format!("Checked: {}/models listed {} models\n", self.base_url, rows.len())),
            None => s.push_str(&format!("Not checked: {}; the models above were typed in\n", self.unlisted)),
        }
        s.push_str(&format!("Writes: {} and, with a key, {}", self.dests.providers, self.dests.auth));
        s
    }
}

fn text(prompt: &str, secret: bool, default: Option<String>) -> Step {
    Step::Text { prompt: prompt.into(), secret, default }
}

fn item(label: &str, detail: &str) -> PickItem {
    PickItem { label: label.into(), detail: detail.into() }
}

fn listed_detail(row: &CustomRow) -> String {
    match (row.context_length, row.max_completion_tokens) {
        (None, None) => String::new(),
        (c, o) => {
            let n = |v: Option<u32>| v.map_or_else(|| "?".to_string(), |v| v.to_string());
            format!("{} context · {} output", n(c), n(o))
        }
    }
}

/// A positive token count; an empty answer takes `default`.
fn tokens(s: &str, default: u32, what: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(default);
    }
    s.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{what} is a positive number of tokens"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard(taken: &'static [&'static str]) -> Wizard {
        let dests = Destinations { providers: "/p/providers.toml".into(), auth: "/p/auth.json".into() };
        Wizard::new(dests, move |id| taken.contains(&id))
    }

    fn text_answer(w: &mut Wizard, s: &str) -> Result<(), String> {
        w.answer(Answer::Text(s.into()))
    }

    fn prompt(w: &Wizard) -> String {
        match w.step() {
            Step::Text { prompt, .. } => prompt,
            other => panic!("not a text step: {other:?}"),
        }
    }

    fn pick_title(w: &Wizard) -> String {
        match w.step() {
            Step::Pick { title, .. } => title,
            other => panic!("not a picker: {other:?}"),
        }
    }

    /// Through id, label and base URL, to the auth picker.
    fn to_auth(w: &mut Wizard) {
        text_answer(w, "modal").unwrap();
        text_answer(w, "").unwrap();
        text_answer(w, "https://modal.example/v1").unwrap();
    }

    fn listed(id: &str) -> CustomRow {
        CustomRow { id: id.into(), context_length: Some(65_536), max_completion_tokens: Some(8_192) }
    }

    #[test]
    fn the_steps_come_in_order_and_the_id_is_checked_in_place() {
        let mut w = wizard(&["taken"]);
        assert!(prompt(&w).starts_with("Provider id"));
        assert!(text_answer(&mut w, "Bad Id").is_err(), "uppercase is refused");
        assert!(text_answer(&mut w, "anthropic").is_err(), "a built-in name is refused");
        assert!(text_answer(&mut w, "taken").is_err(), "a name in use is refused");
        assert!(prompt(&w).starts_with("Provider id"), "a refused answer keeps the step");
        text_answer(&mut w, "modal").unwrap();
        assert!(prompt(&w).starts_with("Label"));
        assert_eq!(w.step(), Step::Text { prompt: prompt(&w), secret: false, default: Some(String::new()) });
        text_answer(&mut w, "").unwrap();
        assert!(text_answer(&mut w, "http://example.com/v1").is_err(), "plain http is refused off loopback");
        text_answer(&mut w, "https://modal.example/v1").unwrap();
        assert_eq!(pick_title(&w), "How does the endpoint take its key?");
    }

    #[test]
    fn an_empty_label_means_the_id_and_a_label_is_kept() {
        let mut w = wizard(&[]);
        text_answer(&mut w, "modal").unwrap();
        text_answer(&mut w, "  Modal GPU  ").unwrap();
        text_answer(&mut w, "https://modal.example/v1").unwrap();
        w.answer(Answer::Pick(3)).unwrap();
        let Step::Fetch(_) = w.step() else { panic!() };
        w.answer(Answer::Fetched(Fetch::Listed(vec![listed("org/m")]))).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Save(saved) = w.step() else { panic!("not saved") };
        assert_eq!(saved.provider.label.as_deref(), Some("Modal GPU"));
        assert_eq!(saved.provider.auth, AuthMode::None);
    }

    #[test]
    fn a_pasted_key_is_masked_and_never_in_the_review() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(0)).unwrap();
        assert_eq!(w.step(), Step::Text { prompt: prompt(&w), secret: true, default: None });
        assert!(text_answer(&mut w, "   ").is_err());
        assert!(text_answer(&mut w, "line\nbreak").is_err());
        text_answer(&mut w, " live-0123456789 ").unwrap();
        let Step::Fetch(probe) = w.step() else { panic!("no check") };
        assert_eq!(probe.credential.as_ref().and_then(|e| e.key.as_deref()), Some("live-0123456789"));
        assert_eq!(w.answer(Answer::Pick(0)), Err("still checking the endpoint".into()));
        w.answer(Answer::Fetched(Fetch::Listed(vec![listed("org/m")]))).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let review = match w.step() {
            Step::Pick { context, .. } => context,
            other => panic!("{other:?}"),
        };
        assert!(!review.contains("live-0123456789"), "{review}");
        assert!(review.contains("pasted key (not shown)") && review.contains("/p/auth.json"));
    }

    #[test]
    fn a_command_is_shown_by_its_program_only() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(2)).unwrap();
        text_answer(&mut w, "pass show modal/token --secret-flag arg-123").unwrap();
        let Step::Fetch(probe) = w.step() else { panic!("no check") };
        assert_eq!(
            probe.credential.and_then(|e| e.reference).as_deref(),
            Some("!pass show modal/token --secret-flag arg-123")
        );
        w.answer(Answer::Fetched(Fetch::Failed("down".into()))).unwrap();
        w.answer(Answer::Pick(2)).unwrap();
        text_answer(&mut w, "org/m").unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Pick { context, .. } = w.step() else { panic!("no review") };
        assert!(context.contains("Auth: !pass …"), "{context}");
        assert!(!context.contains("arg-123") && !context.contains("secret-flag"), "{context}");
    }

    #[test]
    fn a_missing_variable_or_a_refused_key_returns_to_the_credential_choice() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(1)).unwrap();
        text_answer(&mut w, "MU_TEST_NOT_SET_ANYWHERE").unwrap();
        w.answer(Answer::Fetched(Fetch::Credential("$MU_TEST_NOT_SET_ANYWHERE (the key for modal) is not set".into())))
            .unwrap();
        assert_eq!(pick_title(&w), "How does the endpoint take its key?");
        assert!(w.take_notes()[0].contains("is not set"), "the reason is kept as a note");

        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "wrong-0123456789").unwrap();
        w.answer(Answer::Fetched(Fetch::Rejected("modal rejected the credential (HTTP 401)".into()))).unwrap();
        assert_eq!(pick_title(&w), "How does the endpoint take its key?", "a 401 cannot be saved");
        assert!(w.step() != Step::Cancelled);
    }

    #[test]
    fn a_failed_listing_is_a_discovery_failure_and_offers_a_manual_id() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(3)).unwrap();
        w.answer(Answer::Fetched(Fetch::Failed("modal listed no usable models at /models".into()))).unwrap();
        assert_eq!(pick_title(&w), "The endpoint did not list its models");
        w.answer(Answer::Pick(2)).unwrap();
        assert!(prompt(&w).starts_with("Model id"), "continuing unvalidated still collects a model id");
        assert!(text_answer(&mut w, "  ").is_err());
        text_answer(&mut w, "org/typed").unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Pick { context, .. } = w.step() else { panic!() };
        assert!(context.contains("Not checked:") && context.contains("org/typed"), "{context}");
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Save(saved) = w.step() else { panic!() };
        assert_eq!(saved.listed, None, "nothing to cache");
        assert_eq!(saved.provider.models["org/typed"].context_window, Some(DEFAULT_CONTEXT));
        assert_eq!(saved.provider.models["org/typed"].max_tokens, Some(DEFAULT_OUTPUT));
    }

    #[test]
    fn retry_asks_the_endpoint_again_with_the_same_credential() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(3)).unwrap();
        w.answer(Answer::Fetched(Fetch::Failed("unreachable".into()))).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        assert!(
            matches!(w.step(), Step::Fetch(p) if p.credential.is_none() && p.base_url == "https://modal.example/v1")
        );
    }

    #[test]
    fn listed_models_prefill_their_limits_and_limits_are_checked() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(3)).unwrap();
        w.answer(Answer::Fetched(Fetch::Listed(vec![listed("org/a"), listed("org/b")]))).unwrap();
        assert_eq!(pick_title(&w), "Choose a model");
        let Step::Pick { items, .. } = w.step() else { panic!() };
        assert_eq!(items.last().map(|i| i.label.as_str()), Some("Enter a model id manually"));
        w.answer(Answer::Pick(1)).unwrap();
        assert_eq!(
            w.step(),
            Step::Text {
                prompt: "Context window of org/b, in tokens [65536]".into(),
                secret: false,
                default: Some("65536".into())
            }
        );
        text_answer(&mut w, "0").unwrap_err();
        text_answer(&mut w, "").unwrap();
        assert_eq!(
            w.step(),
            Step::Text {
                prompt: "Output limit of org/b, in tokens, at most 65536 [8192]".into(),
                secret: false,
                default: Some("8192".into())
            }
        );
        assert!(text_answer(&mut w, "70000").is_err(), "output may not exceed the window");
        text_answer(&mut w, "4096").unwrap();
        w.answer(Answer::Pick(1)).unwrap();
        assert_eq!(pick_title(&w), "Choose a model", "adding another model lists them again");
        w.answer(Answer::Pick(2)).unwrap();
        text_answer(&mut w, "org/manual").unwrap();
        text_answer(&mut w, "32768").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Pick { context, .. } = w.step() else { panic!() };
        assert!(context.contains("org/b") && context.contains("org/manual"), "{context}");
        assert!(context.contains("Checked:"), "{context}");
    }

    #[test]
    fn cancel_at_any_step_ends_it_without_a_save() {
        let mut w = wizard(&[]);
        assert!(!w.busy() && w.awaits_text());
        w.answer(Answer::Cancel).unwrap();
        assert_eq!(w.step(), Step::Cancelled);

        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "0123456789-key").unwrap();
        assert!(w.busy(), "the check is running");
        w.answer(Answer::Cancel).unwrap();
        assert_eq!(w.step(), Step::Cancelled, "a result that arrives after this is never answered");
    }

    #[test]
    fn the_review_cancel_writes_nothing() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(3)).unwrap();
        w.answer(Answer::Fetched(Fetch::Listed(vec![listed("org/a")]))).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        w.answer(Answer::Pick(1)).unwrap();
        assert_eq!(w.step(), Step::Cancelled);
    }

    #[test]
    fn a_keyless_endpoint_saves_no_entry_and_a_bearer_one_saves_its_entry() {
        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(3)).unwrap();
        w.answer(Answer::Fetched(Fetch::Listed(vec![listed("org/a")]))).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        text_answer(&mut w, "").unwrap();
        text_answer(&mut w, "").unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        w.answer(Answer::Pick(0)).unwrap();
        let Step::Save(saved) = w.step() else { panic!() };
        assert!(saved.entry.is_none());
        assert_eq!(saved.provider.auth, AuthMode::None);
        assert_eq!(saved.listed.map(|r| r.len()), Some(1));

        let mut w = wizard(&[]);
        to_auth(&mut w);
        w.answer(Answer::Pick(1)).unwrap();
        text_answer(&mut w, "MODAL_API_KEY").unwrap();
        assert!(
            matches!(w.step(), Step::Fetch(p) if p.credential.as_ref().and_then(|e| e.reference.clone()).as_deref() == Some("$MODAL_API_KEY"))
        );
    }
}
