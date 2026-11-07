//! The custom-endpoint panel (`/login custom`, or the `/login` row that adds one): one screen of
//! fields that reads like the `providers.toml` table it writes. Enter edits the highlighted field,
//! `←` / `⇧Tab` go back, and Esc goes back too, until the first screen, where it writes nothing.
//!
//! Pure: the app draws each `Step`, feeds back an `Answer` per input, takes the key in the masked
//! composer (`Step::Key`) and runs the one check the panel asks for (`Action::Fetch`). Only
//! `Action::Save` writes, and the app is what writes it.

use crate::widgets::select::{Item, Select};
use mu_provider::auth::store::{Entry, cmd_label};
use mu_provider::model::discovery::CustomRow;
use mu_provider::provider::custom::{
    self, AuthMode, CustomModel, CustomProvider, DEFAULT_CONTEXT, DEFAULT_OUTPUT, EFFORT_LADDER, Reasoning,
};
use std::collections::BTreeMap;

/// The files a save writes, named on the first screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Destinations {
    pub providers: String,
    pub auth: String,
}

/// What the endpoint's `/models` check reads: the endpoint, and its credential (`None` when it
/// takes none).
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
    /// An existing definition is replaced rather than added.
    pub editing: bool,
    /// Whether the user chose how the key is taken; false leaves a stored key as it is.
    pub key_changed: bool,
    pub listed: Option<Vec<CustomRow>>,
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

/// One answer to the screen on show.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The row at this index of the screen's list.
    Pick(usize),
    /// The free-text row, with what was typed into it.
    Text(String),
    /// Esc on a list, or `←` / `⇧Tab`: back a screen, or out of the panel from the first.
    Back,
}

/// What the app must do next; `Step` is what to draw.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Pending,
    /// Close the panel without writing.
    Close,
    /// The answer was not usable; the screen keeps it so it can be corrected.
    Refused(String),
    /// Read the endpoint's `/models`; answer with `fetched`.
    Fetch(Probe),
    /// Write this.
    Save(Box<Saved>),
}

pub enum Step {
    /// The screen's list, ready to draw.
    Pick(Box<Select>),
    /// The masked composer takes the key.
    Key,
}

/// Which field of the panel is open for typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Id,
    Label,
    Base,
    EnvName,
    Command,
    ModelId,
    Context,
    Output,
    /// The request switches merged while thinking is on / off.
    SwitchesOn,
    SwitchesOff,
}

/// What the row at an index of the current screen stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    // The endpoint's fields.
    Id,
    Label,
    Base,
    Key,
    Models,
    Save,
    Cancel,
    // How it takes its key.
    Paste,
    Env,
    Command,
    NoKey,
    // Its models: one the listing shows, a typed id, or reading the listing again.
    Listed(usize),
    Manual,
    Check,
    // One model's limits.
    Context,
    Output,
    Reasoning,
    Add,
    Remove,
    // How a model reasons.
    ModeNone,
    ModeEffort,
    ModeTemplate,
    /// A level of the effort ladder, by its place in `EFFORT_LADDER`.
    Level(usize),
    SwitchesOn,
    SwitchesOff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Form,
    Auth,
    Models,
    Limits,
    Reasoning,
    /// A field open for typing; the screen behind it is the one it belongs to.
    Field(Field),
}

/// The endpoint's `/models`, as far as it has been read.
#[derive(Debug, Clone, PartialEq)]
enum Listing {
    /// Not read for the current endpoint and key.
    Unread,
    Reading,
    Read(Vec<CustomRow>),
    Failed(String),
}

/// The model being set: its limits and how it reasons, as Add writes it.
#[derive(Clone, Debug, PartialEq)]
struct Limits {
    id: String,
    model: CustomModel,
}

impl Limits {
    fn ctx(&self) -> u32 {
        self.model.context_window.unwrap_or(DEFAULT_CONTEXT)
    }

    fn out(&self) -> u32 {
        self.model.max_tokens.unwrap_or(DEFAULT_OUTPUT)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Auth {
    /// The entry the key comes from, and how the screen names it (never the key itself).
    Bearer {
        entry: Entry,
        source: String,
    },
    Keyless,
}

pub struct Panel {
    dests: Destinations,
    taken: Box<dyn Fn(&str) -> bool>,
    screen: Screen,
    id: String,
    label: String,
    base_url: String,
    auth: Option<Auth>,
    /// Whether the auth row was chosen here, so an edit only writes a key it was given.
    auth_changed: bool,
    /// Why the key was refused, shown on the auth screen.
    problem: String,
    discover: bool,
    /// An existing definition is edited: its id is fixed and its key stays unless replaced.
    editing: bool,
    listing: Listing,
    models: BTreeMap<String, CustomModel>,
    limits: Option<Limits>,
    /// What a field started from, so a refused answer can be corrected.
    entry: String,
    /// The auth screen waits for the key in the masked composer.
    pending: bool,
    /// What the app must print; drained by `take_notes`.
    notes: Vec<String>,
}

impl Panel {
    /// Starts on the endpoint's fields. `taken` says whether an id is in use (a definition or a
    /// stored key).
    pub fn new(dests: Destinations, taken: impl Fn(&str) -> bool + 'static) -> Self {
        Self {
            dests,
            taken: Box::new(taken),
            screen: Screen::Form,
            id: String::new(),
            label: String::new(),
            base_url: String::new(),
            auth: None,
            auth_changed: false,
            problem: String::new(),
            discover: true,
            editing: false,
            listing: Listing::Unread,
            models: BTreeMap::new(),
            limits: None,
            entry: String::new(),
            pending: false,
            notes: Vec::new(),
        }
    }

    /// Starts on an existing definition: the same fields, with the id fixed, the models loaded and
    /// the key the store holds (`entry`) shown as it is taken. Save replaces the definition there.
    pub fn edited(dests: Destinations, def: &CustomProvider, entry: Option<Entry>) -> Self {
        let mut p = Self::new(dests, |_| false);
        p.editing = true;
        p.id = def.id.clone();
        p.label = def.label.clone().unwrap_or_default();
        p.base_url = def.base_url.clone();
        p.discover = def.discover;
        p.models = def.models.clone();
        p.auth = match (def.auth, entry) {
            (AuthMode::None, _) => Some(Auth::Keyless),
            (AuthMode::Bearer, Some(entry)) => Some(Auth::Bearer { source: entry_source(&entry), entry }),
            (AuthMode::Bearer, None) => None,
        };
        p
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// What to draw now.
    pub fn step(&self) -> Step {
        match self.screen {
            Screen::Form => Step::Pick(Box::new(self.form())),
            Screen::Auth if self.pending => Step::Key,
            Screen::Auth => Step::Pick(Box::new(self.auth_screen())),
            Screen::Models => Step::Pick(Box::new(self.models_screen())),
            Screen::Limits => Step::Pick(Box::new(self.limits_screen())),
            Screen::Reasoning => Step::Pick(Box::new(self.reasoning_screen())),
            Screen::Field(f) => Step::Pick(Box::new(self.field_screen(f))),
        }
    }

    /// One answer. `Action::Fetch` means the app must read `/models` and answer with `fetched`.
    pub fn answer(&mut self, a: Answer) -> Action {
        let action = match a {
            Answer::Back => self.back(),
            Answer::Text(text) => self.submit(&text),
            Answer::Pick(i) => match self.rows().get(i).copied() {
                Some(row) => self.choose(row),
                None => Action::Pending,
            },
        };
        // A screen that needs the endpoint's models asks for them once.
        match action {
            Action::Pending => self.check(),
            other => other,
        }
    }

    /// The masked composer's text for the key.
    pub fn key_typed(&mut self, text: &str) -> Result<(), String> {
        if !self.pending {
            return Ok(());
        }
        let text = text.trim();
        if text.is_empty() {
            return Err("the key is empty; paste it, or Esc goes back".into());
        }
        if text.contains('\n') {
            return Err("a key is one line".into());
        }
        self.pending = false;
        let entry = Entry::literal(text);
        self.set_auth(Auth::Bearer { entry, source: "pasted key (not shown)".into() });
        self.screen = Screen::Form;
        Ok(())
    }

    /// Whether the masked composer is being asked for the key.
    pub fn awaits_key(&self) -> bool {
        self.pending
    }

    /// The `/models` check answered.
    pub fn fetched(&mut self, f: Fetch) -> Action {
        match f {
            Fetch::Listed(rows) => self.listing = Listing::Read(rows),
            Fetch::Failed(m) => self.listing = Listing::Failed(m),
            // A key that cannot be resolved or is refused is not one to save: ask for it again.
            Fetch::Credential(m) | Fetch::Rejected(m) => {
                self.notes.push(format!("{m}. Nothing was saved; choose the key again."));
                self.problem = m;
                self.auth = None;
                self.listing = Listing::Unread;
                self.screen = Screen::Auth;
            }
        }
        self.check()
    }

    // ---- the screens' answers ----------------------------------------------------------------

    /// Back one screen: the one behind this one, or out of the panel from the first.
    fn back(&mut self) -> Action {
        // Leaving the auth screen drops a key it was waiting for.
        self.pending = false;
        self.screen = match self.screen {
            Screen::Form => return Action::Close,
            Screen::Field(Field::Id | Field::Label | Field::Base) => Screen::Form,
            Screen::Field(Field::EnvName | Field::Command) => Screen::Auth,
            Screen::Field(Field::ModelId) => Screen::Models,
            Screen::Field(Field::Context | Field::Output) => Screen::Limits,
            Screen::Field(Field::SwitchesOn | Field::SwitchesOff) => Screen::Reasoning,
            Screen::Auth | Screen::Models => Screen::Form,
            Screen::Limits => Screen::Models,
            Screen::Reasoning => Screen::Limits,
        };
        Action::Pending
    }

    /// A row was chosen on its screen.
    fn choose(&mut self, row: Row) -> Action {
        match row {
            Row::Id => return self.edit(Field::Id, &self.id.clone()),
            Row::Label => return self.edit(Field::Label, &self.label.clone()),
            Row::Base => return self.edit(Field::Base, &self.base_url.clone()),
            Row::Key => self.screen = Screen::Auth,
            Row::Models => self.screen = Screen::Models,
            Row::Save => {
                return match self.ready() {
                    Ok(()) => Action::Save(Box::new(self.saved())),
                    Err(m) => Action::Refused(m),
                };
            }
            Row::Cancel => return Action::Close,
            Row::Paste => {
                self.pending = true;
                self.screen = Screen::Auth;
            }
            Row::Env => return self.edit(Field::EnvName, &self.env_default()),
            Row::Command => return self.edit(Field::Command, ""),
            Row::NoKey => {
                self.set_auth(Auth::Keyless);
                self.screen = Screen::Form;
            }
            Row::Listed(i) => {
                // The row's model, by its place in the listing the screen was built from.
                let id = match &self.listing {
                    Listing::Read(rows) => rows.get(i).map(|r| r.id.clone()),
                    _ => None,
                };
                return match id {
                    Some(id) => self.open_limits(&id),
                    None => Action::Pending,
                };
            }
            Row::Manual => return self.edit(Field::ModelId, ""),
            Row::Check => self.listing = Listing::Unread,
            Row::Context => {
                let ctx = self.limits.as_ref().map(|l| l.ctx().to_string()).unwrap_or_default();
                return self.edit(Field::Context, &ctx);
            }
            Row::Output => {
                let out = self.limits.as_ref().map(|l| l.out().to_string()).unwrap_or_default();
                return self.edit(Field::Output, &out);
            }
            Row::Reasoning => self.screen = Screen::Reasoning,
            Row::Add => return self.add_limits(),
            Row::Remove => {
                self.remove_limits();
                self.screen = Screen::Models;
            }
            Row::ModeNone => self.set_reasoning(Reasoning::None),
            Row::ModeEffort => self.set_reasoning(Reasoning::Effort),
            Row::ModeTemplate => self.set_reasoning(Reasoning::Template),
            Row::Level(i) => self.toggle_level(i),
            Row::SwitchesOn => {
                let text = self.limits.as_ref().map(|l| custom::switches_text(&l.model.extra_body_on));
                return self.edit(Field::SwitchesOn, &text.unwrap_or_default());
            }
            Row::SwitchesOff => {
                let text = self.limits.as_ref().map(|l| custom::switches_text(&l.model.extra_body_off));
                return self.edit(Field::SwitchesOff, &text.unwrap_or_default());
            }
        }
        Action::Pending
    }

    /// Switches the model's reasoning mode, dropping what the new mode cannot hold.
    fn set_reasoning(&mut self, mode: Reasoning) {
        let Some(l) = self.limits.as_mut() else { return };
        let m = &mut l.model;
        m.reasoning = mode;
        match mode {
            Reasoning::None => {
                m.efforts.clear();
                m.extra_body_on.clear();
                m.extra_body_off.clear();
            }
            // `efforts` only applies to effort, and a level list is never left empty.
            Reasoning::Effort if m.efforts.is_empty() => m.efforts = custom::default_efforts(),
            Reasoning::Effort => m.efforts = ordered_efforts(&m.efforts),
            Reasoning::Template => m.efforts.clear(),
        }
    }

    /// Adds or drops a level of the model's effort ladder.
    fn toggle_level(&mut self, idx: usize) {
        let Some(name) = EFFORT_LADDER.get(idx) else { return };
        let Some(l) = self.limits.as_mut() else { return };
        let m = &mut l.model;
        match m.efforts.iter().position(|e| e == name) {
            Some(at) => {
                m.efforts.remove(at);
            }
            None => m.efforts.push((*name).to_string()),
        }
        m.efforts = ordered_efforts(&m.efforts);
    }

    /// Opens a field for typing, starting from `text`.
    fn edit(&mut self, field: Field, text: &str) -> Action {
        self.screen = Screen::Field(field);
        self.entry = text.to_string();
        Action::Pending
    }

    /// The value typed into the field that is open.
    fn submit(&mut self, text: &str) -> Action {
        let Screen::Field(field) = self.screen else { return Action::Pending };
        match self.apply(field, text.trim()) {
            Ok(Some(action)) => action,
            Ok(None) => Action::Pending,
            Err(m) => {
                // The field stays open on what was typed, so it can be corrected.
                self.entry = text.to_string();
                self.screen = Screen::Field(field);
                Action::Refused(m)
            }
        }
    }

    /// The value typed into a field. `Ok(Some(_))` when it opens another screen.
    fn apply(&mut self, field: Field, v: &str) -> Result<Option<Action>, String> {
        match field {
            Field::Id => {
                custom::check_id(v).map_err(|e| format!("{e:#}"))?;
                if (self.taken)(v) {
                    return Err(format!(
                        "`{v}` is already in use by a custom endpoint or a stored key; pick another id"
                    ));
                }
                self.id = v.to_string();
                self.screen = Screen::Form;
            }
            Field::Label => {
                self.label = v.to_string();
                self.screen = Screen::Form;
            }
            Field::Base => {
                self.base_url = custom::check_base_url(v).map_err(|e| format!("{e:#}"))?;
                // A listing belongs to one URL, so the new one is read again.
                self.listing = Listing::Unread;
                self.screen = Screen::Form;
            }
            Field::EnvName => {
                let name = v.trim_start_matches('$');
                let entry = Entry::reference(&format!("${name}")).map_err(|_| {
                    "name the variable that holds the key, e.g. MODAL_API_KEY (letters, digits and `_`)".to_string()
                })?;
                self.set_auth(Auth::Bearer { entry, source: format!("${name}") });
                self.screen = Screen::Form;
            }
            Field::Command => {
                if v.is_empty() {
                    return Err("type the command that prints the key, e.g. `pass show modal`".into());
                }
                let cmd = if v.starts_with('!') { v.to_string() } else { format!("!{v}") };
                let entry = Entry::reference(&cmd).map_err(|e| e.to_string())?;
                self.set_auth(Auth::Bearer { entry, source: cmd_label(cmd.trim_start_matches('!')) });
                self.screen = Screen::Form;
            }
            Field::ModelId => {
                if v.is_empty() {
                    return Err("a model id is needed: the one the endpoint names, e.g. org/model".into());
                }
                return Ok(Some(self.open_limits(v)));
            }
            Field::Context => {
                let Some(mut l) = self.limits.clone() else { return Ok(None) };
                let ctx = tokens(v, l.ctx(), "the context window")?;
                // The output limit never exceeds the window.
                l.model.context_window = Some(ctx);
                l.model.max_tokens = Some(l.out().min(ctx));
                self.limits = Some(l);
                self.screen = Screen::Limits;
            }
            Field::Output => {
                let Some(mut l) = self.limits.clone() else { return Ok(None) };
                let out = tokens(v, l.out(), "the output limit")?;
                if out > l.ctx() {
                    return Err(format!("the output limit ({out}) must not exceed the context window ({})", l.ctx()));
                }
                l.model.max_tokens = Some(out);
                self.limits = Some(l);
                self.screen = Screen::Limits;
            }
            Field::SwitchesOn | Field::SwitchesOff => {
                let (name, on) = match field {
                    Field::SwitchesOn => ("extra_body_on", true),
                    _ => ("extra_body_off", false),
                };
                let map = custom::parse_switches(v)?;
                custom::check_extra(name, &map).map_err(|e| format!("{e:#}"))?;
                let Some(l) = self.limits.as_mut() else { return Ok(None) };
                if on {
                    l.model.extra_body_on = map;
                } else {
                    l.model.extra_body_off = map;
                }
                self.screen = Screen::Reasoning;
            }
        }
        Ok(None)
    }

    /// The check the current screen needs, when it needs one.
    fn check(&mut self) -> Action {
        if self.screen != Screen::Models || self.listing != Listing::Unread {
            return Action::Pending;
        }
        let Ok(base_url) = custom::check_base_url(&self.base_url) else { return Action::Pending };
        let Some(auth) = self.auth.as_ref() else { return Action::Pending };
        let credential = match auth {
            Auth::Bearer { entry, .. } => Some(entry.clone()),
            Auth::Keyless => None,
        };
        self.listing = Listing::Reading;
        Action::Fetch(Probe { id: self.id.clone(), base_url, credential })
    }

    /// Takes the limits and reasoning of `id` from what is configured, or the listing, or the
    /// defaults. A model nobody configured starts with the effort ladder.
    fn open_limits(&mut self, id: &str) -> Action {
        let listed = match &self.listing {
            Listing::Read(rows) => rows.iter().find(|r| r.id == id),
            _ => None,
        };
        let configured = self.models.get(id).cloned();
        let ctx = configured
            .as_ref()
            .and_then(|m| m.context_window)
            .or(listed.and_then(|r| r.context_length))
            .unwrap_or(DEFAULT_CONTEXT);
        let out = configured
            .as_ref()
            .and_then(|m| m.max_tokens)
            .or(listed.and_then(|r| r.max_completion_tokens))
            .unwrap_or(DEFAULT_OUTPUT.min(ctx))
            .min(ctx);
        let mut model = configured.unwrap_or_else(CustomModel::with_effort_defaults);
        model.context_window = Some(ctx);
        model.max_tokens = Some(out);
        self.limits = Some(Limits { id: id.to_string(), model });
        self.screen = Screen::Limits;
        Action::Pending
    }

    /// Adds the model as the screen sets it, or says why it cannot be added yet.
    fn add_limits(&mut self) -> Action {
        let Some(l) = self.limits.clone() else { return Action::Pending };
        let m = &l.model;
        match m.reasoning {
            Reasoning::Effort if m.efforts.is_empty() => {
                return Action::Refused("pick at least one effort level: Enter toggles one".into());
            }
            Reasoning::Template if m.extra_body_on.is_empty() || m.extra_body_off.is_empty() => {
                return Action::Refused(
                    "template reasoning needs a switch while thinking is on and one while it is off, \
                     e.g. `chat_template_kwargs.enable_thinking = true`"
                        .into(),
                );
            }
            _ => {}
        }
        if let Err(e) = m.validate() {
            return Action::Refused(format!("{e:#}"));
        }
        self.models.insert(l.id, l.model);
        self.screen = Screen::Models;
        Action::Pending
    }

    fn remove_limits(&mut self) {
        let Some(l) = self.limits.clone() else { return };
        self.models.remove(&l.id);
    }

    fn set_auth(&mut self, auth: Auth) {
        self.auth = Some(auth);
        self.auth_changed = true;
        // Another key means the listing is not known to be this endpoint's any more.
        self.listing = Listing::Unread;
        self.problem.clear();
    }

    /// Why Save cannot write yet.
    fn ready(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("give the endpoint an id first: enter on Provider id".into());
        }
        if self.base_url.is_empty() {
            return Err("give the endpoint its base URL first: enter on Base URL".into());
        }
        if self.auth.is_none() {
            return Err("choose how the endpoint takes its key first: enter on Key".into());
        }
        Ok(())
    }

    fn saved(&self) -> Saved {
        let mut provider = CustomProvider::new(&self.base_url);
        provider.label = Some(self.label.trim().to_string()).filter(|l| !l.is_empty());
        provider.auth = if matches!(self.auth, Some(Auth::Keyless)) { AuthMode::None } else { AuthMode::Bearer };
        provider.discover = self.discover;
        provider.models = self.models.clone();
        let entry = match &self.auth {
            Some(Auth::Bearer { entry, .. }) => Some(entry.clone()),
            _ => None,
        };
        let listed = match &self.listing {
            Listing::Read(rows) => Some(rows.clone()),
            _ => None,
        };
        Saved {
            id: self.id.clone(),
            provider,
            entry,
            editing: self.editing,
            // A keyless endpoint has no credential to write, whatever was chosen before it.
            key_changed: self.auth_changed && !matches!(self.auth, Some(Auth::Keyless)),
            listed,
        }
    }

    fn env_default(&self) -> String {
        if self.id.is_empty() { String::new() } else { custom::env_hint(&self.id) }
    }

    // ---- what the screens show --------------------------------------------------------------

    /// The rows of the screen on show, in the order `step()` lists them.
    fn rows(&self) -> Vec<Row> {
        match self.screen {
            // An edit keeps the id: the definition it replaces is the one named in the title.
            Screen::Form if self.editing => {
                vec![Row::Label, Row::Base, Row::Key, Row::Models, Row::Save, Row::Cancel]
            }
            Screen::Form => vec![Row::Id, Row::Label, Row::Base, Row::Key, Row::Models, Row::Save, Row::Cancel],
            Screen::Auth => vec![Row::Paste, Row::Env, Row::Command, Row::NoKey],
            Screen::Models => {
                let mut rows = Vec::new();
                if let Listing::Read(listed) = &self.listing {
                    rows.extend((0..listed.len()).map(Row::Listed));
                }
                rows.push(Row::Manual);
                if matches!(self.listing, Listing::Read(_) | Listing::Failed(_)) {
                    rows.push(Row::Check);
                }
                rows
            }
            Screen::Limits => {
                let mut rows = vec![Row::Context, Row::Output, Row::Reasoning, Row::Add];
                if self.limits.as_ref().is_some_and(|l| self.models.contains_key(&l.id)) {
                    rows.push(Row::Remove);
                }
                rows
            }
            Screen::Reasoning => {
                let mut rows = vec![Row::ModeNone, Row::ModeEffort, Row::ModeTemplate];
                match self.limits.as_ref().map(|l| l.model.reasoning) {
                    Some(Reasoning::Effort) => rows.extend((0..EFFORT_LADDER.len()).map(Row::Level)),
                    Some(Reasoning::Template) => rows.extend([Row::SwitchesOn, Row::SwitchesOff]),
                    _ => {}
                }
                rows
            }
            Screen::Field(_) => Vec::new(),
        }
    }

    /// The screen's rows as the picker draws them, in the order `rows()` gives.
    fn items(&self) -> Vec<Item> {
        self.rows().into_iter().map(|row| self.item(row)).collect()
    }

    /// One row: its label and the dim detail beside it.
    fn item(&self, row: Row) -> Item {
        let (label, detail) = match row {
            Row::Id => ("Provider id".to_string(), or(self.id.as_str(), "not set; e.g. modal")),
            Row::Label => ("Label".to_string(), or(self.label.trim(), &self.id)),
            Row::Base => ("Base URL".to_string(), or(self.base_url.as_str(), "not set; https://host/v1")),
            Row::Key => ("Key".to_string(), self.auth_text()),
            Row::Models => ("Models".to_string(), self.models_text()),
            Row::Save => ("Save".to_string(), self.save_text()),
            Row::Cancel => ("Cancel".to_string(), "nothing is written".to_string()),
            Row::Paste => ("Paste a key".to_string(), "typed now, shown as bullets; the composer takes it".to_string()),
            Row::Env => ("Use an environment variable".to_string(), self.env_text()),
            Row::Command => ("Use a command".to_string(), "a command that prints the key, run through sh".to_string()),
            Row::NoKey => ("No authentication".to_string(), "requests carry no key, and nothing is stored".to_string()),
            Row::Listed(i) => match &self.listing {
                Listing::Read(rows) => match rows.get(i) {
                    Some(row) => (row.id.clone(), self.listed_detail(row)),
                    None => (String::new(), String::new()),
                },
                _ => (String::new(), String::new()),
            },
            Row::Manual => ("Type a model id…".to_string(), "one the listing does not show".to_string()),
            Row::Check => ("Ask the endpoint again".to_string(), format!("read {}/models now", self.base_url)),
            Row::Context => (
                "Context window".to_string(),
                match &self.limits {
                    Some(l) => format!("{} tokens", l.ctx()),
                    None => format!("{DEFAULT_CONTEXT} tokens"),
                },
            ),
            Row::Output => (
                "Output limit".to_string(),
                match &self.limits {
                    Some(l) => format!("{} tokens, at most the context window", l.out()),
                    None => format!("{DEFAULT_OUTPUT} tokens, at most the context window"),
                },
            ),
            Row::Reasoning => ("Reasoning".to_string(), self.reasoning_text()),
            Row::ModeNone => ("No reasoning".to_string(), self.mode_detail(Reasoning::None)),
            Row::ModeEffort => ("Effort levels".to_string(), self.mode_detail(Reasoning::Effort)),
            Row::ModeTemplate => ("Request switches".to_string(), self.mode_detail(Reasoning::Template)),
            Row::Level(i) => {
                let name = EFFORT_LADDER[i];
                let on = self.limits.as_ref().is_some_and(|l| l.model.efforts.iter().any(|e| e == name));
                let detail = if on { "on; /thinking uses it" } else { "off; Enter adds it" };
                (name.to_string(), detail.to_string())
            }
            Row::SwitchesOn => ("While thinking".to_string(), self.switches_detail(true)),
            Row::SwitchesOff => ("While not thinking".to_string(), self.switches_detail(false)),
            Row::Add => {
                let id = self.limits.as_ref().map(|l| l.id.as_str()).unwrap_or_default();
                let label = if self.models.contains_key(id) { format!("Save {id}") } else { format!("Add {id}") };
                (label, "to the endpoint's models".to_string())
            }
            Row::Remove => ("Remove it".to_string(), "from the endpoint's models".to_string()),
        };
        // The row that says how the key is taken now says so.
        let current = matches!(row, Row::Paste | Row::Env | Row::Command | Row::NoKey) && self.auth_kind() == Some(row);
        let label = if current { format!("{label} (current)") } else { label };
        Item { label, detail }
    }

    fn form(&self) -> Select {
        let context = if self.editing {
            format!(
                "Editing `{}` in {}; its id stays and its entry is replaced. {} is written only for a key chosen here.",
                self.id, self.dests.providers, self.dests.auth
            )
        } else {
            format!(
                "Chat Completions, at <base URL>/chat/completions; never through omp.\nWrites {}, and {}.",
                self.dests.providers, self.dests.auth
            )
        };
        let title =
            if self.editing { format!("Edit custom endpoint {}", self.name()) } else { "Add a custom endpoint".into() };
        Select::new(&title, self.items())
            .context(&context)
            .hint("↑↓ move · Enter edit · Esc cancel; nothing is written")
    }

    fn auth_screen(&self) -> Select {
        let mut context = format!("Stored in {} under this id, never in {}.", self.dests.auth, self.dests.providers);
        if !self.problem.is_empty() {
            context.push_str(&format!("\n{}", self.problem));
        }
        Select::new("How does the endpoint take its key?", self.items())
            .context(&context)
            .back_keys(true)
            .hint("↑↓ move · Enter choose · ← / ⇧Tab / Esc back")
    }

    fn models_screen(&self) -> Select {
        Select::new(&format!("Choose a model for {}", self.name()), self.items())
            .context(&self.listing_text())
            .back_keys(true)
            .hint("↑↓ move · Enter choose · ← / ⇧Tab / Esc back")
    }

    fn limits_screen(&self) -> Select {
        let (id, known) = match &self.limits {
            Some(l) => (l.id.clone(), self.models.contains_key(&l.id)),
            None => (String::new(), false),
        };
        let listed = self.listed_row(&id);
        let context = match (listed, known) {
            (Some(r), _) if r.context_length.is_some() || r.max_completion_tokens.is_some() => {
                format!("What /models reported:\n  {}", limits_of(r))
            }
            (_, true) => "Its limits are set below; Enter saves them for this endpoint.".to_string(),
            _ => format!(
                "The listing gives no limits, so the defaults are here:\n  {DEFAULT_CONTEXT} context · {DEFAULT_OUTPUT} output"
            ),
        };
        Select::new(&format!("Limits of {id}"), self.items())
            .context(&context)
            .preselect(3)
            .back_keys(true)
            .hint("↑↓ move · Enter edit, reason or add · ← / ⇧Tab / Esc back")
    }

    fn reasoning_screen(&self) -> Select {
        let id = self.limits.as_ref().map(|l| l.id.clone()).unwrap_or_default();
        let context = match self.limits.as_ref().map(|l| l.model.reasoning) {
            Some(Reasoning::Effort) => {
                "Sends `reasoning_effort` with one of the levels below; `/thinking off` sends none.".to_string()
            }
            Some(Reasoning::Template) => {
                "No effort: the switches below are merged into the request while thinking is on, and the \
                 others while it is off."
                    .to_string()
            }
            _ => "No thinking parameters at all: `/thinking` will have nothing to send.".to_string(),
        };
        Select::new(&format!("How does {id} reason?"), self.items())
            .context(&context)
            // The first level or switch, once a mode above it adds rows there.
            .preselect(3)
            .back_keys(true)
            .hint("↑↓ move · Enter choose or toggle · ← / ⇧Tab / Esc back")
    }

    fn field_screen(&self, f: Field) -> Select {
        let title = f.title(self);
        Select::new(&title, Vec::new()).context(&f.help(self)).free_text(&title).edit_text(&self.entry)
    }

    fn name(&self) -> String {
        match (self.label.trim(), self.id.as_str()) {
            ("", "") => "this endpoint".to_string(),
            ("", id) => id.to_string(),
            (label, _) => label.to_string(),
        }
    }

    /// What the environment-variable row says: the conventional name, and whether it is set.
    fn env_text(&self) -> String {
        let env = self.env_default();
        if env.is_empty() {
            return "read when mu needs it; Enter to name it".into();
        }
        if env_is_set(&env) { format!("${env} is set") } else { format!("${env} is not set; read when mu needs it") }
    }

    fn auth_text(&self) -> String {
        match &self.auth {
            Some(Auth::Bearer { source, .. }) => source.clone(),
            Some(Auth::Keyless) => "none; requests carry no key".into(),
            None => "not chosen; Enter to choose".into(),
        }
    }

    /// The auth row that says how the endpoint takes its key now, if it does yet.
    fn auth_kind(&self) -> Option<Row> {
        match &self.auth {
            Some(Auth::Keyless) => Some(Row::NoKey),
            Some(Auth::Bearer { entry, .. }) => match entry.reference.as_deref() {
                Some(r) if r.starts_with('!') => Some(Row::Command),
                Some(_) => Some(Row::Env),
                None => Some(Row::Paste),
            },
            None => None,
        }
    }

    fn models_text(&self) -> String {
        if self.models.is_empty() {
            return "none yet; Enter to pick one".into();
        }
        let names: Vec<&str> = self.models.keys().map(String::as_str).collect();
        format!("{}: {}", names.len(), names.join(", "))
    }

    /// What the Limits screen says about the model's reasoning: its mode and the levels or switches.
    fn reasoning_text(&self) -> String {
        let Some(l) = self.limits.as_ref() else { return "not set".into() };
        let m = &l.model;
        match m.reasoning {
            Reasoning::None => "none; /thinking has nothing to set".into(),
            Reasoning::Effort if m.efforts.is_empty() => "effort; no level picked yet".into(),
            Reasoning::Effort => format!("effort: {}", m.efforts.join(", ")),
            Reasoning::Template => {
                let part = |label: &str, map: &serde_json::Map<String, serde_json::Value>| {
                    let text = custom::switches_text(map);
                    if text.is_empty() { format!("{label}: not set") } else { format!("{label}: {text}") }
                };
                format!("switches; {} · {}", part("on", &m.extra_body_on), part("off", &m.extra_body_off))
            }
        }
    }

    /// One reasoning mode's row: what it does, and whether it is the one the model has.
    fn mode_detail(&self, mode: Reasoning) -> String {
        let what = match mode {
            Reasoning::None => "no effort and no request switches",
            Reasoning::Effort => "sends reasoning_effort from the levels below",
            Reasoning::Template => "merges request switches while thinking is on or off",
        };
        let current = self.limits.as_ref().is_some_and(|l| l.model.reasoning == mode);
        if current { format!("{what}; current") } else { what.to_string() }
    }

    /// The switches merged while thinking is on (`on`) or off, as the reasoning screen shows them.
    fn switches_detail(&self, on: bool) -> String {
        let text = self
            .limits
            .as_ref()
            .map(|l| custom::switches_text(if on { &l.model.extra_body_on } else { &l.model.extra_body_off }))
            .unwrap_or_default();
        if text.is_empty() {
            "not set; Enter for `path = value`, e.g. chat_template_kwargs.enable_thinking = true".into()
        } else {
            text
        }
    }

    fn save_text(&self) -> String {
        match &self.auth {
            _ if self.editing && !self.auth_changed => "the definition; its stored key stays".into(),
            Some(Auth::Keyless) => "the definition".into(),
            _ => "the definition and the key".into(),
        }
    }

    fn listed_row(&self, id: &str) -> Option<&CustomRow> {
        match &self.listing {
            Listing::Read(rows) => rows.iter().find(|r| r.id == id),
            _ => None,
        }
    }

    fn listed_detail(&self, row: &CustomRow) -> String {
        let limits = limits_of(row);
        let Some(m) = self.models.get(&row.id) else { return limits };
        let added = format!(
            "added: {} ctx · {} out · {}",
            m.context_window.unwrap_or(DEFAULT_CONTEXT),
            m.max_tokens.unwrap_or(DEFAULT_OUTPUT),
            match m.reasoning {
                Reasoning::Effort => format!("effort {}", m.efforts.join("/")),
                other => other.as_str().to_string(),
            }
        );
        if limits.is_empty() { added } else { format!("{limits} · {added}") }
    }

    fn listing_text(&self) -> String {
        match &self.listing {
            Listing::Unread if self.base_url.is_empty() => {
                "Set the base URL first; a model id can be typed in the meantime.".into()
            }
            Listing::Unread if self.auth.is_none() => {
                "Choose how the endpoint takes its key to read its models, or type a model id.".into()
            }
            Listing::Unread => format!("{}/models has not been read yet.", self.base_url),
            Listing::Reading => format!("reading {}/models…", self.base_url),
            Listing::Read(rows) => {
                format!(
                    "{}/models lists {}; Enter adds one with its limits",
                    self.base_url,
                    plural(rows.len(), "model")
                )
            }
            Listing::Failed(m) => format!("{m}\nType a model id to use one the listing does not show."),
        }
    }
}

impl Field {
    /// The field's row label, which is also what its free-text row says.
    fn title(self, p: &Panel) -> String {
        match self {
            Self::Id => "Provider id".into(),
            Self::Label => "Label".into(),
            Self::Base => "Base URL".into(),
            Self::EnvName => match p.env_default() {
                d if d.is_empty() => "Environment variable".into(),
                d => format!("Environment variable [{d}]"),
            },
            Self::Command => "Command".into(),
            Self::ModelId => "Model id".into(),
            Self::Context => match p.limits.as_ref() {
                Some(l) => format!("Context window [{}]", l.ctx()),
                None => "Context window".into(),
            },
            Self::Output => match p.limits.as_ref() {
                Some(l) => format!("Output limit [{}, at most {}]", l.out(), l.ctx()),
                None => "Output limit".into(),
            },
            Self::SwitchesOn => "Switches while thinking".into(),
            Self::SwitchesOff => "Switches while not thinking".into(),
        }
    }

    /// What the field wants, under its title.
    fn help(self, p: &Panel) -> String {
        match self {
            Self::Id => "Lowercase letters, digits, `-` and `_`, and not a name mu already uses. It is \
                         the `[providers.<id>]` table and the start of every selector."
                .into(),
            Self::Label => format!("What the pickers and the status line show; empty means `{}`.", p.id),
            Self::Base => "The API root, `/v1` included when the endpoint serves one; `/models` and \
                           `/chat/completions` are appended. https, or http to this machine."
                .into(),
            Self::EnvName => match p.env_default() {
                d if d.is_empty() => "The variable that holds the key, e.g. MODAL_API_KEY.".into(),
                d => format!("The variable that holds the key; Enter uses ${d}."),
            },
            Self::Command => "A command that prints the key, run through sh; only its program is ever shown.".into(),
            Self::ModelId => "Exactly as the endpoint names it, e.g. org/model.".into(),
            Self::Context => "In tokens; Enter keeps the value shown.".into(),
            Self::Output => "In tokens, at most the context window; Enter keeps the value shown.".into(),
            Self::SwitchesOn | Self::SwitchesOff => {
                "One or more `path = value` switches, comma-separated; values are JSON (`true`, `0.6`, \
                 `\"text\"`). Enter keeps the value shown; an empty answer clears them."
                    .into()
            }
        }
    }
}

/// The ladder in `efforts`, in ascending order and without names the ladder does not have.
fn ordered_efforts(efforts: &[String]) -> Vec<String> {
    EFFORT_LADDER.iter().filter(|name| efforts.iter().any(|e| e == *name)).map(|n| (*n).to_string()).collect()
}

/// How the auth screen names a stored credential, never showing a literal key.
fn entry_source(entry: &Entry) -> String {
    match entry.reference.as_deref() {
        Some(r) if r.starts_with('!') => cmd_label(r.trim_start_matches('!')),
        Some(r) => r.to_string(),
        None => "the stored key (not shown)".into(),
    }
}

fn or<'a>(v: &'a str, then: &'a str) -> String {
    if v.is_empty() { then.to_string() } else { v.to_string() }
}

fn env_is_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

/// A positive token count; an empty answer takes `default`.
fn tokens(s: &str, default: u32, what: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(default);
    }
    s.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{what} is a positive number of tokens"))
}

/// `1 model`, `2 models`.
fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn limits_of(row: &CustomRow) -> String {
    let n = |v: Option<u32>| v.map_or_else(|| "?".to_string(), |v| v.to_string());
    match (row.context_length, row.max_completion_tokens) {
        (None, None) => String::new(),
        (c, o) => format!("{} ctx · {} out", n(c), n(o)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::line_text;

    fn panel() -> Panel {
        Panel::new(Destinations { providers: "/p/providers.toml".into(), auth: "/p/auth.json".into() }, |id| {
            id == "taken"
        })
    }

    /// The screen as drawn, its rows and prompt.
    fn screen(p: &Panel) -> String {
        match p.step() {
            Step::Pick(sel) => sel.render(100, 40).0.iter().map(line_text).collect::<Vec<_>>().join("\n"),
            Step::Key => "<key>".into(),
        }
    }

    /// The row whose label starts with `label` on the screen on show.
    fn row(p: &Panel, label: &str) -> usize {
        let text = screen(p);
        let lines: Vec<&str> = text.lines().collect();
        // The list starts right under the filter line; the title and its context are above it,
        // and the dim footer below it.
        let first = lines.iter().position(|l| l.starts_with("› ")).unwrap_or_else(|| panic!("no list:\n{text}"));
        // A row's line: the marker, then the label, with the detail dimmed after it.
        let plain = |l: &str| l.trim_start().trim_start_matches('❯').trim_start().to_string();
        lines[first + 1..]
            .iter()
            .position(|l| plain(l).starts_with(label))
            .unwrap_or_else(|| panic!("no `{label}` row in:\n{text}"))
    }

    fn pick(p: &mut Panel, label: &str) -> Action {
        let i = row(p, label);
        p.answer(Answer::Pick(i))
    }

    /// Opens a field, types `text` and submits it.
    fn type_into(p: &mut Panel, label: &str, text: &str) -> Action {
        pick(p, label);
        p.answer(Answer::Text(text.into()))
    }

    fn listed(id: &str, ctx: Option<u32>, out: Option<u32>) -> CustomRow {
        CustomRow { id: id.into(), context_length: ctx, max_completion_tokens: out }
    }

    /// Whether the screen on show is a field being typed into.
    fn typing(p: &Panel) -> bool {
        match p.step() {
            Step::Pick(sel) => sel.is_editing(),
            Step::Key => false,
        }
    }

    /// An endpoint with an id, a base URL and no key.
    fn ready() -> Panel {
        let mut p = panel();
        type_into(&mut p, "Provider id", "modal");
        type_into(&mut p, "Base URL", "https://modal.example/v1");
        pick(&mut p, "Key");
        pick(&mut p, "No authentication");
        p
    }

    #[test]
    fn the_first_screen_names_the_files_it_writes_and_what_is_still_missing() {
        let mut p = panel();
        let text = screen(&p);
        assert!(text.contains("Add a custom endpoint"), "{text}");
        assert!(text.contains("/p/providers.toml") && text.contains("/p/auth.json"), "{text}");
        assert!(text.contains("Provider id") && text.contains("not set"), "{text}");
        assert!(text.contains("Save") && text.contains("Cancel"), "{text}");
        assert!(
            matches!(pick(&mut p, "Save"), Action::Refused(m) if m.contains("id")),
            "an empty panel cannot be saved"
        );
    }

    #[test]
    fn a_field_opens_on_its_value_and_a_refused_value_keeps_the_field_open() {
        let mut p = panel();
        pick(&mut p, "Provider id");
        assert!(typing(&p), "the field opens for typing");
        // Uppercase is refused, and the field stays open on what was typed.
        let action = p.answer(Answer::Text("Bad Id".into()));
        assert!(matches!(action, Action::Refused(ref m) if m.contains("lowercase")), "{action:?}");
        assert!(screen(&p).contains("Provider id"), "{}", screen(&p));
        // A name mu already uses, or one in use, is refused too.
        assert!(matches!(p.answer(Answer::Text("anthropic".into())), Action::Refused(_)));
        assert!(matches!(p.answer(Answer::Text("taken".into())), Action::Refused(_)));
        assert_eq!(p.answer(Answer::Text(" modal ".into())), Action::Pending);
        assert!(screen(&p).contains("Provider id  modal"), "{}", screen(&p));
    }

    #[test]
    fn esc_goes_back_one_screen_and_the_first_one_closes_the_panel() {
        let mut p = ready();
        assert!(matches!(pick(&mut p, "Models"), Action::Fetch(_)), "entering the models reads the endpoint");
        assert_eq!(p.answer(Answer::Back), Action::Pending, "back to the fields");
        assert!(screen(&p).contains("Add a custom endpoint"));
        assert_eq!(p.answer(Answer::Back), Action::Close, "out of the panel");
    }

    #[test]
    fn a_key_is_taken_in_the_masked_composer_and_never_named_again() {
        let mut p = ready();
        pick(&mut p, "Key");
        pick(&mut p, "Paste a key");
        assert!(matches!(p.step(), Step::Key), "the composer takes the key");
        assert_eq!(p.key_typed("  "), Err("the key is empty; paste it, or Esc goes back".into()));
        assert_eq!(p.key_typed("line\nbreak"), Err("a key is one line".into()));
        p.key_typed("live-0123456789").unwrap();
        let text = screen(&p);
        assert!(text.contains("pasted key (not shown)"), "{text}");
        assert!(!text.contains("live-0123456789"), "{text}");
        pick(&mut p, "Key");
        assert!(screen(&p).contains("Paste a key (current)"), "{}", screen(&p));
    }

    #[test]
    fn an_environment_variable_is_offered_by_its_conventional_name() {
        let mut p = panel();
        type_into(&mut p, "Provider id", "modal");
        pick(&mut p, "Key");
        let text = screen(&p);
        assert!(text.contains("Use an environment variable"), "{text}");
        assert!(text.contains("$MODAL_API_KEY"), "the hint comes from the id:\n{text}");
        // Entering the auth row opens it on the hint, and Esc goes back to the choices.
        pick(&mut p, "Use an environment variable");
        assert!(typing(&p));
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(screen(&p).contains("How does the endpoint take its key?"), "{}", screen(&p));
        pick(&mut p, "Use an environment variable");
        assert_eq!(p.answer(Answer::Text("MODAL_API_KEY".into())), Action::Pending);
        assert!(screen(&p).contains("Key  $MODAL_API_KEY"), "{}", screen(&p));
    }

    #[test]
    fn the_models_screen_reads_the_endpoint_once_and_shows_what_was_read() {
        let mut p = ready();
        let Action::Fetch(probe) = pick(&mut p, "Models") else { panic!("no check") };
        assert_eq!(probe.base_url, "https://modal.example/v1");
        assert!(probe.credential.is_none(), "a keyless endpoint checks as itself");
        assert!(matches!(p.step(), Step::Pick(_)), "the list stays on screen while it reads");
        assert!(screen(&p).contains("reading https://modal.example/v1/models"), "{}", screen(&p));

        let action =
            p.fetched(Fetch::Listed(vec![listed("org/a", Some(65_536), Some(8_192)), listed("org/b", None, None)]));
        assert_eq!(action, Action::Pending, "one read is enough");
        let text = screen(&p);
        assert!(text.contains("lists 2 models"), "{text}");
        assert!(text.contains("org/a") && text.contains("65536 ctx · 8192 out"), "{text}");
        assert!(text.contains("Type a model id"), "{text}");
        // The listing belongs to the base URL: another one is read again.
        p.answer(Answer::Back);
        type_into(&mut p, "Base URL", "https://other.example/v1");
        assert!(matches!(pick(&mut p, "Models"), Action::Fetch(p) if p.base_url == "https://other.example/v1"));
    }

    #[test]
    fn a_model_takes_its_limits_from_the_listing_and_they_can_be_changed() {
        let mut p = ready();
        pick(&mut p, "Models");
        p.fetched(Fetch::Listed(vec![listed("org/a", Some(65_536), Some(8_192))]));
        pick(&mut p, "org/a");
        let text = screen(&p);
        assert!(text.contains("Limits of org/a"), "{text}");
        assert!(text.contains("65536 tokens") && text.contains("8192 tokens"), "{text}");
        assert!(text.contains("Add org/a"), "not configured yet:\n{text}");
        assert!(!text.contains("Remove it"));

        // Adding keeps the limits the listing reported.
        let i = row(&p, "Add org/a");
        assert_eq!(p.answer(Answer::Pick(i)), Action::Pending);
        let saved = p.saved();
        assert_eq!(saved.provider.models["org/a"].context_window, Some(65_536));
        assert_eq!(saved.provider.models["org/a"].max_tokens, Some(8_192));

        // Changing them is a field, and the output never exceeds the window.
        pick(&mut p, "org/a");
        type_into(&mut p, "Context window", "32768");
        assert!(screen(&p).contains("32768 tokens"), "{}", screen(&p));
        assert!(screen(&p).contains("8192 tokens"), "output is kept when it still fits:\n{}", screen(&p));
        assert!(matches!(type_into(&mut p, "Output limit", "70000"), Action::Refused(_)));
        assert_eq!(p.answer(Answer::Text("4096".into())), Action::Pending);
        let i = row(&p, "Save org/a");
        p.answer(Answer::Pick(i));
        let saved = p.saved();
        assert_eq!(saved.provider.models["org/a"].context_window, Some(32_768));
        assert_eq!(saved.provider.models["org/a"].max_tokens, Some(4_096));

        // And removing it takes it out again.
        pick(&mut p, "org/a");
        let i = row(&p, "Remove it");
        p.answer(Answer::Pick(i));
        assert!(p.saved().provider.models.is_empty(), "{:?}", p.saved().provider.models);
    }

    #[test]
    fn a_typed_model_id_takes_the_defaults_and_must_not_be_blank() {
        let mut p = ready();
        pick(&mut p, "Models");
        assert_eq!(p.fetched(Fetch::Failed("unreachable".into())), Action::Pending);
        assert!(screen(&p).contains("unreachable"), "a failure is shown, not a dead end:\n{}", screen(&p));
        assert!(screen(&p).contains("Ask the endpoint again"), "{}", screen(&p));
        assert!(matches!(type_into(&mut p, "Type a model id", "  "), Action::Refused(_)));
        // The field stays open on what was typed, so the next value is typed straight in.
        assert!(matches!(p.answer(Answer::Text("org/typed".into())), Action::Pending));
        let text = screen(&p);
        assert!(text.contains("Limits of org/typed"), "{text}");
        assert!(text.contains(&format!("{DEFAULT_CONTEXT} tokens")), "the defaults fill in:\n{text}");
    }

    #[test]
    fn a_refused_key_returns_to_the_choice_that_saves_nothing() {
        let mut p = ready();
        pick(&mut p, "Key");
        pick(&mut p, "Paste a key");
        p.key_typed("0123456789").unwrap();
        pick(&mut p, "Models");
        let action = p.fetched(Fetch::Rejected("modal rejected the credential (HTTP 401)".into()));
        assert_eq!(action, Action::Pending);
        let text = screen(&p);
        assert!(text.contains("How does the endpoint take its key?"), "{text}");
        assert!(text.contains("HTTP 401"), "the reason is on the screen:\n{text}");
        assert!(!text.contains("(current)"), "a refused key is not the choice any more:\n{text}");
        assert!(p.take_notes()[0].contains("Nothing was saved"));
        assert_eq!(p.answer(Answer::Back), Action::Pending, "back to the fields");
    }

    #[test]
    fn save_needs_an_id_a_base_url_and_a_key_and_reports_what_is_missing() {
        let mut p = panel();
        assert!(matches!(pick(&mut p, "Save"), Action::Refused(m) if m.contains("id")));
        type_into(&mut p, "Provider id", "modal");
        assert!(matches!(pick(&mut p, "Save"), Action::Refused(m) if m.contains("base URL")));
        type_into(&mut p, "Base URL", "https://modal.example/v1");
        assert!(matches!(pick(&mut p, "Save"), Action::Refused(m) if m.contains("key")));
        pick(&mut p, "Key");
        pick(&mut p, "No authentication");
        let Action::Save(saved) = pick(&mut p, "Save") else { panic!("not saved") };
        assert_eq!(saved.id, "modal");
        assert_eq!(saved.provider.base_url, "https://modal.example/v1");
        assert_eq!(saved.provider.auth, AuthMode::None);
        assert!(saved.entry.is_none(), "a keyless endpoint stores no credential");
        assert!(saved.listed.is_none(), "nothing was read, so nothing is cached");
    }

    #[test]
    fn a_new_model_starts_with_the_effort_ladder_and_its_levels_toggle() {
        let mut p = ready();
        pick(&mut p, "Models");
        p.fetched(Fetch::Failed("unreachable".into()));
        type_into(&mut p, "Type a model id", "org/typed");
        assert!(screen(&p).contains("effort: low, medium, high"), "{}", screen(&p));

        let i = row(&p, "Add org/typed");
        assert_eq!(p.answer(Answer::Pick(i)), Action::Pending);
        let m = p.saved().provider.models["org/typed"].clone();
        assert_eq!((m.reasoning, m.efforts), (Reasoning::Effort, custom::default_efforts()));

        // The whole ladder is offered; picking a level adds it, picking it again removes it.
        type_into(&mut p, "Type a model id", "org/typed");
        pick(&mut p, "Reasoning");
        let text = screen(&p);
        assert!(text.contains("How does org/typed reason?"), "{text}");
        assert!(text.contains("minimal") && text.contains(EFFORT_LADDER[5]), "the whole ladder:\n{text}");
        assert!(text.contains("off; Enter adds it"), "{text}");
        pick(&mut p, "high");
        pick(&mut p, "xhigh");
        assert_eq!(p.limits.as_ref().unwrap().model.efforts, ["low", "medium", "xhigh"], "kept ascending");
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(screen(&p).contains("effort: low, medium, xhigh"), "{}", screen(&p));

        // A model with no level at all cannot be saved, and the panel says why.
        pick(&mut p, "Reasoning");
        for level in ["low", "medium", "xhigh"] {
            pick(&mut p, level);
        }
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(
            matches!(pick(&mut p, "Save org/typed"), Action::Refused(m) if m.contains("at least one effort level")),
            "{}",
            screen(&p)
        );

        // No reasoning drops the ladder.
        pick(&mut p, "Reasoning");
        pick(&mut p, "No reasoning");
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(screen(&p).contains("none; /thinking has nothing to set"), "{}", screen(&p));
        let i = row(&p, "Save org/typed");
        assert_eq!(p.answer(Answer::Pick(i)), Action::Pending);
        let m = p.saved().provider.models["org/typed"].clone();
        assert_eq!((m.reasoning, m.efforts), (Reasoning::None, Vec::new()));
    }

    #[test]
    fn template_reasoning_takes_its_switches_as_path_value_text() {
        let mut p = ready();
        pick(&mut p, "Models");
        p.fetched(Fetch::Failed("unreachable".into()));
        type_into(&mut p, "Type a model id", "org/typed");
        pick(&mut p, "Reasoning");
        pick(&mut p, "Request switches");
        let text = screen(&p);
        assert!(text.contains("While thinking") && text.contains("While not thinking"), "{text}");
        type_into(&mut p, "While thinking", "chat_template_kwargs.enable_thinking = true");
        assert!(screen(&p).contains("chat_template_kwargs.enable_thinking = true"), "{}", screen(&p));

        // A switch that would replace a field mu owns, missing `=` and a table clash are refused.
        assert!(matches!(type_into(&mut p, "While not thinking", "reasoning_effort = \"high\""), Action::Refused(m)
                if m.contains("reasoning_effort")));
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(matches!(type_into(&mut p, "While not thinking", "chat_template_kwargs"), Action::Refused(m)
                if m.contains("path = value")));
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(matches!(type_into(&mut p, "While not thinking", "a.b = 1, a = 2"), Action::Refused(m)
                if m.contains("table")));
        assert_eq!(p.answer(Answer::Back), Action::Pending);

        // Only one side set is not template reasoning yet, and Add says so.
        type_into(&mut p, "While thinking", "chat_template_kwargs.enable_thinking = true");
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        assert!(
            matches!(pick(&mut p, "Add org/typed"), Action::Refused(m) if m.contains("while it is off")),
            "{}",
            screen(&p)
        );
        pick(&mut p, "Reasoning");
        type_into(&mut p, "While not thinking", "chat_template_kwargs.enable_thinking = false");
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        let m = p.limits.as_ref().unwrap().model.clone();
        assert_eq!(m.reasoning, Reasoning::Template);
        assert!(m.efforts.is_empty(), "template sends no effort");
        assert_eq!(m.extra_body_on["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(m.extra_body_off["chat_template_kwargs"]["enable_thinking"], false);
        let i = row(&p, "Add org/typed");
        assert_eq!(p.answer(Answer::Pick(i)), Action::Pending);
        let saved = p.saved().provider.models["org/typed"].clone();
        assert_eq!(custom::switches_text(&saved.extra_body_off), "chat_template_kwargs.enable_thinking = false");
    }

    /// An endpoint that is already in the profile, with one model and no discovery.
    fn existing() -> CustomProvider {
        CustomProvider {
            id: "modal".into(),
            env_hint: custom::env_hint("modal"),
            label: Some("Modal".into()),
            base_url: "https://modal.example/v1".into(),
            auth: AuthMode::Bearer,
            discover: false,
            models: BTreeMap::from([(
                "org/quiet".to_string(),
                CustomModel { context_window: Some(8192), max_tokens: Some(1024), ..Default::default() },
            )]),
        }
    }

    fn edited(entry: Option<Entry>) -> Panel {
        let dests = Destinations { providers: "/p/providers.toml".into(), auth: "/p/auth.json".into() };
        Panel::edited(dests, &existing(), entry)
    }

    #[test]
    fn an_edit_opens_on_the_definition_with_its_key_and_keeps_what_it_is_not_shown_here() {
        let mut p = edited(Some(Entry::literal("stored-key-0123456789")));
        let text = screen(&p);
        assert!(text.contains("Edit custom endpoint Modal"), "{text}");
        assert!(text.contains("/p/providers.toml"), "{text}");
        assert!(!text.contains("stored-key-0123456789"), "{text}");
        assert!(!text.contains("Provider id"), "the id is fixed:\n{text}");
        assert!(text.contains("the stored key (not shown)"), "{text}");
        assert!(text.contains("the definition; its stored key stays"), "{text}");

        // Its model keeps the settings the file has; only its reasoning changes.
        assert!(matches!(pick(&mut p, "Models"), Action::Fetch(_)));
        p.fetched(Fetch::Failed("unreachable".into()));
        type_into(&mut p, "Type a model id", "org/quiet");
        assert!(screen(&p).contains("8192 tokens"), "the configured limits:\n{}", screen(&p));
        assert!(screen(&p).contains("none; /thinking has nothing to set"), "{}", screen(&p));
        pick(&mut p, "Reasoning");
        pick(&mut p, "Effort levels");
        assert_eq!(p.answer(Answer::Back), Action::Pending);
        let i = row(&p, "Save org/quiet");
        assert_eq!(p.answer(Answer::Pick(i)), Action::Pending);
        assert_eq!(p.answer(Answer::Back), Action::Pending, "the models screen goes back to the fields");

        let Action::Save(saved) = pick(&mut p, "Save") else { panic!("not saved:\n{}", screen(&p)) };
        assert!(saved.editing && !saved.key_changed, "an untouched key is not written again");
        assert_eq!((saved.id.as_str(), saved.provider.label()), ("modal", "Modal"));
        assert!(!saved.provider.discover, "the definition keeps its own discovery setting");
        assert_eq!(saved.provider.models["org/quiet"].efforts, ["low", "medium", "high"]);
        assert_eq!(saved.entry.and_then(|e| e.key).as_deref(), Some("stored-key-0123456789"));
    }

    #[test]
    fn an_edited_endpoint_names_its_reference_and_writes_the_key_only_when_it_is_chosen_again() {
        let entry = Entry::reference("$MODAL_API_KEY").unwrap();
        let mut p = edited(Some(entry));
        assert!(screen(&p).contains("$MODAL_API_KEY"), "{}", screen(&p));
        pick(&mut p, "Key");
        assert!(screen(&p).contains("(current)"), "{}", screen(&p));
        pick(&mut p, "Use a command");
        p.answer(Answer::Text("pass show modal/token".into()));
        assert!(screen(&p).contains("!pass …"), "{}", screen(&p));
        assert!(screen(&p).contains("the definition and the key"), "{}", screen(&p));
        let Action::Save(saved) = pick(&mut p, "Save") else { panic!("not saved") };
        assert!(saved.editing && saved.key_changed);
        assert_eq!(saved.entry.and_then(|e| e.reference).as_deref(), Some("!pass show modal/token"));

        // A bearer endpoint with no stored key has to be given one before it can be saved.
        let mut p = edited(None);
        assert!(screen(&p).contains("not chosen"), "{}", screen(&p));
        assert!(matches!(pick(&mut p, "Save"), Action::Refused(m) if m.contains("key")));
        pick(&mut p, "Key");
        pick(&mut p, "No authentication");
        assert!(screen(&p).contains("Save  the definition"), "{}", screen(&p));
        let Action::Save(saved) = pick(&mut p, "Save") else { panic!("not saved") };
        assert!(!saved.key_changed && saved.provider.auth == AuthMode::None);
    }

    #[test]
    fn a_command_is_shown_by_its_program_only() {
        let mut p = ready();
        pick(&mut p, "Key");
        pick(&mut p, "Use a command");
        assert!(typing(&p), "the command is typed in the field");
        p.answer(Answer::Text("pass show modal/token --secret-flag arg-123".into()));
        let text = screen(&p);
        assert!(text.contains("!pass …"), "{text}");
        assert!(!text.contains("arg-123") && !text.contains("secret-flag"), "{text}");
        let Action::Save(saved) = pick(&mut p, "Save") else { panic!("not saved") };
        assert_eq!(
            saved.entry.and_then(|e| e.reference).as_deref(),
            Some("!pass show modal/token --secret-flag arg-123")
        );
    }
}
