//! Custom OpenAI-compatible (Chat Completions) endpoints: vLLM, Modal and the like. The profile's
//! `providers.toml` holds their non-secret definitions; credentials live in the auth store, under
//! the provider id, never in this file. Every read reflects the file as it is now, so a definition
//! added by `mu providers add` or another session is visible without a restart.
//!
//! ```toml
//! [providers.modal]
//! label = "Modal"            # optional
//! base_url = "https://host/v1"
//! auth = "bearer"            # or "none"
//! discover = true            # GET <base_url>/models
//!
//! [providers.modal.models."org/model"]
//! context_window = 32768
//! reasoning = "template"     # "none" | "effort" | "template"
//! [providers.modal.models."org/model".extra_body_on.chat_template_kwargs]
//! enable_thinking = true
//! ```

use crate::auth::store::{self, Entry, EntryExists, Store};
use crate::provider::providers::{PROVIDERS, SEARCH_KEYS};
use crate::provider::wire::openai_completions::REASONING_FIELDS;
use anyhow::{Context, Result, anyhow, bail};
use mu_base::models::{Api, CompletionsCompat, ModelSpec, ThinkingMode, ThinkingSpec};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Defaults of a custom model whose limits nobody configured or discovered.
pub const DEFAULT_CONTEXT: u32 = 32_768;
pub const DEFAULT_OUTPUT: u32 = 4_096;

/// Request fields mu owns; an extra-body map may not replace them at its root.
pub const RESERVED_BODY_KEYS: [&str; 11] = [
    "model",
    "messages",
    "tools",
    "tool_choice",
    "functions",
    "function_call",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "reasoning_effort",
];

const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The `compat` keys a definition may set (`CompletionsCompat`'s camelCase names, less the
/// request-extra maps, which are the model's own `extra_body*` tables).
const COMPAT_KEYS: [&str; 10] = [
    "maxTokensField",
    "reasoningContentField",
    "requiresReasoningContentForToolCalls",
    "requiresReasoningContentForAllAssistantTurns",
    "requiresAssistantContentForToolCalls",
    "allowsSyntheticReasoningContentForToolCalls",
    "syntheticReasoningContentFallback",
    "reasoningDisableMode",
    "thinkingFormat",
    "supportsDeveloperRole",
];
const COMPAT_KEYS_MORE: [&str; 3] = ["supportsMultipleSystemMessages", "supportsUsageInStreaming", "stripImageInput"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// A key from the auth store, sent as `Authorization: Bearer`.
    #[default]
    Bearer,
    /// No credential; no auth header is sent.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reasoning {
    #[default]
    None,
    /// `reasoning_effort` from the model's `efforts` ladder.
    Effort,
    /// Request-parameter switches (`extra_body_on` / `extra_body_off`).
    Template,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProvider {
    /// The `[providers.<id>]` key; injected on load.
    #[serde(skip)]
    pub id: String,
    /// The conventional environment variable, `<ID>_API_KEY`; injected on load.
    #[serde(skip)]
    pub env_hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Without a trailing slash; `/models` and `/chat/completions` are appended as they are.
    pub base_url: String,
    #[serde(default)]
    pub auth: AuthMode,
    #[serde(default = "yes")]
    pub discover: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, CustomModel>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub images: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub reasoning: Reasoning,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(
        default,
        deserialize_with = "de_compat",
        serialize_with = "ser_compat",
        skip_serializing_if = "compat_is_default"
    )]
    pub compat: CompletionsCompat,
    #[serde(default, deserialize_with = "de_json_map", skip_serializing_if = "Map::is_empty")]
    pub extra_body: Map<String, Value>,
    #[serde(default, deserialize_with = "de_json_map", skip_serializing_if = "Map::is_empty")]
    pub extra_body_on: Map<String, Value>,
    #[serde(default, deserialize_with = "de_json_map", skip_serializing_if = "Map::is_empty")]
    pub extra_body_off: Map<String, Value>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// A TOML value as JSON: dates, times and non-finite numbers have no JSON form.
fn json_from_toml(v: toml::Value, at: &str) -> Result<Value, String> {
    Ok(match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => {
            Value::Number(serde_json::Number::from_f64(f).ok_or_else(|| format!("{at}: non-finite number"))?)
        }
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(_) => return Err(format!("{at}: dates and times cannot be sent as JSON")),
        toml::Value::Array(a) => Value::Array(
            a.into_iter()
                .enumerate()
                .map(|(i, v)| json_from_toml(v, &format!("{at}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(t) => Value::Object(
            t.into_iter()
                .map(|(k, v)| {
                    let p = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                    Ok((k, json_from_toml(v, &p)?))
                })
                .collect::<Result<_, String>>()?,
        ),
    })
}

fn de_json_map<'de, D: Deserializer<'de>>(d: D) -> Result<Map<String, Value>, D::Error> {
    let t = toml::Table::deserialize(d)?;
    match json_from_toml(toml::Value::Table(t), "").map_err(D::Error::custom)? {
        Value::Object(m) => Ok(m),
        _ => Err(D::Error::custom("expected a table")),
    }
}

fn de_compat<'de, D: Deserializer<'de>>(d: D) -> Result<CompletionsCompat, D::Error> {
    let t = toml::Table::deserialize(d)?;
    for k in t.keys() {
        if k.starts_with("extraBody") {
            return Err(D::Error::custom(format!(
                "`compat.{k}` is not allowed; use the model's own `extra_body`, `extra_body_on` and `extra_body_off` tables"
            )));
        }
        if !COMPAT_KEYS.contains(&k.as_str()) && !COMPAT_KEYS_MORE.contains(&k.as_str()) {
            return Err(D::Error::custom(format!("unknown compat key `{k}`")));
        }
    }
    toml::Value::Table(t).try_into().map_err(D::Error::custom)
}

/// The compat flags that differ from the defaults, so a written file lists only what was chosen.
fn compat_diff(c: &CompletionsCompat) -> Map<String, Value> {
    let now = serde_json::to_value(c).unwrap_or_default();
    let base = serde_json::to_value(CompletionsCompat::default()).unwrap_or_default();
    let (Value::Object(now), Value::Object(base)) = (now, base) else { return Map::new() };
    now.into_iter().filter(|(k, v)| !k.starts_with("extraBody") && !v.is_null() && base.get(k) != Some(v)).collect()
}

fn compat_is_default(c: &CompletionsCompat) -> bool {
    compat_diff(c).is_empty()
}

fn ser_compat<S: Serializer>(c: &CompletionsCompat, s: S) -> Result<S::Ok, S::Error> {
    compat_diff(c).serialize(s)
}

/// An id for `[providers.<id>]` and for selectors: `[a-z][a-z0-9_-]*`, and not one mu already uses.
pub fn check_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        bail!("provider id `{id}` must start with a lowercase letter and use only a-z, 0-9, `-` and `_`");
    }
    if PROVIDERS.iter().any(|d| d.id == id)
        || SEARCH_KEYS.iter().any(|d| d.id == id)
        || id == "chatgpt"
        || id == "custom"
    {
        bail!("`{id}` is a name mu already uses; pick another provider id");
    }
    Ok(())
}

/// The base URL of an endpoint: https (or http to loopback), no credentials, query or fragment,
/// no trailing slash. `/v1` is never guessed or appended.
pub fn check_base_url(url: &str) -> Result<String> {
    let url = store::check_base_url(url)?;
    if url.contains(['?', '#']) {
        bail!("the base URL must not carry a query string or fragment");
    }
    Ok(url)
}

/// `reasoning_effort`-style names for the model's ladder, lowest first.
fn check_efforts(efforts: &[String]) -> Result<()> {
    let mut last = None;
    for e in efforts {
        let rank = EFFORT_LADDER
            .iter()
            .position(|n| n == e)
            .ok_or_else(|| anyhow!("unknown effort `{e}` (one of {})", EFFORT_LADDER.join(", ")))?;
        if last.is_some_and(|l| rank <= l) {
            bail!("efforts must be unique and ascending ({})", EFFORT_LADDER.join(", "));
        }
        last = Some(rank);
    }
    Ok(())
}

/// A request-extra map: the root may not replace anything mu owns.
pub fn check_extra(name: &str, map: &Map<String, Value>) -> Result<()> {
    if let Some(k) = map.keys().find(|k| RESERVED_BODY_KEYS.contains(&k.as_str())) {
        bail!("{name}.{k} would replace a field mu sets itself; use the typed settings instead");
    }
    Ok(())
}

impl CustomModel {
    pub fn validate(&self) -> Result<()> {
        for (label, v) in [("context_window", self.context_window), ("max_tokens", self.max_tokens)] {
            if v == Some(0) {
                bail!("{label} must be positive");
            }
        }
        if let (Some(w), Some(m)) = (self.context_window, self.max_tokens)
            && m > w
        {
            bail!("max_tokens ({m}) exceeds context_window ({w})");
        }
        match self.reasoning {
            Reasoning::Effort if self.efforts.is_empty() => {
                bail!("reasoning = \"effort\" needs a nonempty `efforts` list")
            }
            Reasoning::Effort => check_efforts(&self.efforts)?,
            _ if !self.efforts.is_empty() => bail!("`efforts` only applies to reasoning = \"effort\""),
            _ => {}
        }
        for (name, m) in [
            ("extra_body", &self.extra_body),
            ("extra_body_on", &self.extra_body_on),
            ("extra_body_off", &self.extra_body_off),
        ] {
            check_extra(name, m)?;
        }
        match self.reasoning {
            Reasoning::Template => {
                if self.extra_body_on.is_empty() || self.extra_body_off.is_empty() {
                    bail!("reasoning = \"template\" needs nonempty `extra_body_on` and `extra_body_off`");
                }
                if self.extra_body_on == self.extra_body_off {
                    bail!("`extra_body_on` and `extra_body_off` must differ");
                }
            }
            Reasoning::None if !self.extra_body_on.is_empty() || !self.extra_body_off.is_empty() => {
                bail!("`extra_body_on`/`extra_body_off` need reasoning = \"template\" or \"effort\"")
            }
            _ => {}
        }
        let c = &self.compat;
        if !matches!(c.max_tokens_field.as_str(), "max_tokens" | "max_completion_tokens") {
            bail!("compat.maxTokensField must be max_tokens or max_completion_tokens");
        }
        if let Some(f) = &c.reasoning_content_field
            && !REASONING_FIELDS.contains(&f.as_str())
        {
            bail!("compat.reasoningContentField must be one of {}", REASONING_FIELDS.join(", "));
        }
        if let Some(m) = &c.reasoning_disable_mode
            && !matches!(m.as_str(), "omit" | "lowest-effort" | "none-effort")
        {
            bail!("compat.reasoningDisableMode must be omit, lowest-effort or none-effort");
        }
        Ok(())
    }

    /// The Chat Completions flags this model's requests use: its `compat` plus its request-extra maps.
    pub fn completions_compat(&self) -> CompletionsCompat {
        let mut c = self.compat.clone();
        c.extra_body = self.extra_body.clone();
        c.extra_body_on = self.extra_body_on.clone();
        c.extra_body_off = self.extra_body_off.clone();
        c
    }
}

impl CustomProvider {
    /// A bearer-auth, discovering endpoint with no manual models, not yet validated (see `validated`).
    pub fn new(base_url: &str) -> Self {
        Self {
            id: String::new(),
            env_hint: String::new(),
            label: None,
            base_url: base_url.to_string(),
            auth: AuthMode::Bearer,
            discover: true,
            models: BTreeMap::new(),
        }
    }

    /// Checks every field, normalizes the base URL and injects `id` and the credential hint.
    pub fn validated(mut self, id: &str) -> Result<Self> {
        check_id(id)?;
        self.base_url = check_base_url(&self.base_url)?;
        if self.label.as_deref().is_some_and(|l| l.trim().is_empty()) {
            self.label = None;
        }
        for (key, m) in &self.models {
            if key.trim().is_empty() {
                bail!("a model id must not be blank");
            }
            m.validate().with_context(|| format!("models.\"{key}\""))?;
        }
        self.id = id.to_string();
        self.env_hint = env_hint(id);
        Ok(self)
    }

    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// The bearer key requests use; `None` for a keyless endpoint. A missing, disabled or
    /// unresolvable key is an error, never a fall back to omp or to another host.
    pub async fn credential(&self, store: &Store) -> Result<Option<String>> {
        if self.auth == AuthMode::None {
            return Ok(None);
        }
        let id = &self.id;
        let entry = store.get(id).ok_or_else(|| {
            anyhow!("no API key for custom provider `{id}`: add one with /login {id} or `mu auth add {id}`")
        })?;
        if !entry.routes_chat() {
            bail!("the stored key for `{id}` is not enabled for chat; replace it with `mu auth add {id}`");
        }
        if let Some(u) = &entry.base_url
            && check_base_url(u).ok().as_deref() != Some(self.base_url.as_str())
        {
            bail!(
                "the stored key for `{id}` carries a different base URL than providers.toml; replace it with `mu auth add {id}`"
            );
        }
        Ok(Some(store.resolve(id).await?.key))
    }

    /// Fetches this endpoint's `/models` and caches it, with its credential. The real error comes
    /// back (a [`DiscoverError`] when the endpoint answered or refused); the old cache stays on failure.
    pub async fn refresh_models(&self, store: &Store) -> Result<usize> {
        let key = self.credential(store).await?;
        Ok(crate::model::discovery::custom_refresh(self, key.as_deref()).await?)
    }

    /// Whether this endpoint's key is usable, for rows and status lines.
    pub fn key_state(&self, store: &Store) -> KeyState {
        if self.auth == AuthMode::None {
            return KeyState::NotNeeded;
        }
        match store.get(&self.id) {
            None => KeyState::Missing,
            Some(e) if !e.routes_chat() => KeyState::Disabled,
            Some(_) => KeyState::Saved,
        }
    }

    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// The spec of `model`, which need not be configured or listed: configured numbers, then
    /// `listed` (context, output) from this endpoint's own `/models`, then the defaults. An
    /// unconfigured model takes text and tools and requests no reasoning.
    pub fn spec(&self, model: &str, listed: (Option<u32>, Option<u32>)) -> Result<ModelSpec> {
        let m = self.models.get(model);
        let context_window = m.and_then(|m| m.context_window).or(listed.0).unwrap_or(DEFAULT_CONTEXT);
        let max_output = m.and_then(|m| m.max_tokens).or(listed.1).unwrap_or(DEFAULT_OUTPUT.min(context_window));
        let (mode, efforts) = match m.map(|m| m.reasoning) {
            Some(Reasoning::Effort) => (ThinkingMode::Effort, m.map(|m| m.efforts.clone()).unwrap_or_default()),
            Some(Reasoning::Template) => (ThinkingMode::Template, Vec::new()),
            _ => (ThinkingMode::None, Vec::new()),
        };
        let compat = m.map(CustomModel::completions_compat).unwrap_or_default();
        for (name, map) in [
            ("extra_body", &compat.extra_body),
            ("extra_body_on", &compat.extra_body_on),
            ("extra_body_off", &compat.extra_body_off),
        ] {
            check_extra(name, map).with_context(|| format!("model {}/{model}", self.id))?;
        }
        Ok(ModelSpec {
            id: format!("{}/{model}", self.id),
            provider: self.id.clone(),
            context_window,
            max_output,
            api: Api::OpenAiCompletions,
            native: true,
            wire_id: model.to_string(),
            thinking: ThinkingSpec { mode, efforts },
            images: m.is_some_and(|m| m.images),
            compat: Some(compat),
            anthropic: Default::default(),
        })
    }
}

/// The state of a custom endpoint's credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    NotNeeded,
    Saved,
    /// Stored but not enabled for chat.
    Disabled,
    Missing,
}

impl KeyState {
    /// The short word picker rows and status lines show.
    pub fn word(self) -> &'static str {
        match self {
            Self::NotNeeded => "no auth",
            Self::Saved => "key",
            Self::Disabled => "key not for chat",
            Self::Missing => "key missing",
        }
    }
}

/// The environment variable a provider's key conventionally comes from; also its redaction label.
pub fn env_hint(id: &str) -> String {
    format!("{}_API_KEY", id.to_ascii_uppercase().replace('-', "_"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(default)]
    providers: BTreeMap<String, CustomProvider>,
}

/// The profile's custom-provider definitions (`providers.toml`), read fresh on every call.
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn new() -> Self {
        Self { path: mu_common::paths::profile_home().join("providers.toml") }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut n = self.path.file_name().unwrap_or_default().to_os_string();
        n.push(".lock");
        self.path.with_file_name(n)
    }

    fn read_text(&self) -> Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn parse(&self, text: &str) -> Result<BTreeMap<String, CustomProvider>> {
        let doc: FileDoc = toml::from_str(text).with_context(|| {
            format!("{} is not valid custom-provider configuration; fix the file", self.path.display())
        })?;
        doc.providers
            .into_iter()
            .map(|(id, p)| {
                let p = p
                    .validated(&id)
                    .with_context(|| format!("{}: [providers.{id}] is invalid; fix the file", self.path.display()))?;
                Ok((id, p))
            })
            .collect()
    }

    /// Every definition as the file holds it now. A missing file is empty; one that cannot be read
    /// or does not validate is an error naming the file and the entry.
    pub fn snapshot(&self) -> Result<BTreeMap<String, CustomProvider>> {
        self.parse(&self.read_text()?)
    }

    pub fn get(&self, id: &str) -> Result<Option<CustomProvider>> {
        Ok(self.snapshot()?.remove(id))
    }

    /// Adds a definition, keeping the file's comments and other entries. Refuses an existing id
    /// and a file that does not parse.
    pub fn add(&self, id: &str, provider: CustomProvider) -> Result<()> {
        let provider = provider.validated(id)?;
        let _lock = mu_common::fs::FileLock::exclusive(&self.lock_path())
            .with_context(|| format!("locking {}", self.lock_path().display()))?;
        let text = self.read_text()?;
        if self.parse(&text)?.contains_key(id) {
            bail!("custom provider `{id}` already exists in {}", self.path.display());
        }
        let mut doc: toml_edit::DocumentMut =
            text.parse().with_context(|| format!("{} is not valid TOML", self.path.display()))?;
        let mut table = toml::to_string(&provider)
            .context("serializing the provider")?
            .parse::<toml_edit::DocumentMut>()
            .context("serializing the provider")?
            .as_table()
            .clone();
        clear_positions(&mut table);
        let providers = doc
            .as_table_mut()
            .entry("providers")
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_mut()
            .ok_or_else(|| anyhow!("`providers` in {} is not a table", self.path.display()))?;
        providers.insert(id, toml_edit::Item::Table(table));
        self.write(&doc.to_string())
    }

    /// Removes a definition; `false` when there was none. Other content is kept.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let _lock = mu_common::fs::FileLock::exclusive(&self.lock_path())
            .with_context(|| format!("locking {}", self.lock_path().display()))?;
        let text = self.read_text()?;
        if !self.parse(&text)?.contains_key(id) {
            return Ok(false);
        }
        let mut doc: toml_edit::DocumentMut =
            text.parse().with_context(|| format!("{} is not valid TOML", self.path.display()))?;
        let removed =
            doc.get_mut("providers").and_then(|p| p.as_table_like_mut()).is_some_and(|p| p.remove(id).is_some());
        if removed {
            self.write(&doc.to_string())?;
        }
        Ok(removed)
    }

    fn write(&self, text: &str) -> Result<()> {
        mu_common::fs::write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Tables built in another document carry its positions; without them they follow the file's own.
fn clear_positions(t: &mut toml_edit::Table) {
    t.set_position(None);
    for (_, item) in t.iter_mut() {
        if let Some(sub) = item.as_table_mut() {
            clear_positions(sub);
        }
    }
}

/// What a failed `save` left behind.
#[derive(Debug)]
pub enum SaveError {
    /// Nothing was written.
    Rejected(anyhow::Error),
    /// The definition is saved; its credential is not.
    KeyNotSaved { id: String, raced: bool, error: anyhow::Error },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "{e:#}"),
            Self::KeyNotSaved { id, raced: true, .. } => write!(
                f,
                "provider `{id}` saved; another credential for it appeared meanwhile and was kept (check it with `mu auth test {id}`)"
            ),
            Self::KeyNotSaved { id, error, .. } => write!(
                f,
                "provider `{id}` saved, but its credential was not saved: {error:#}; add one with /login {id} or `mu auth add {id}`"
            ),
        }
    }
}

impl std::error::Error for SaveError {}

/// Saves a new custom provider: preflight (schema, collisions with another definition or a stored
/// credential), the definition, then its credential. The two files are not one transaction: when
/// the credential write fails the definition stays, unrunnable without a key, and nothing another
/// writer stored is ever removed or replaced. A bearer provider needs `entry` (stored for chat,
/// never with a base URL of its own); a keyless one takes none.
pub fn save(
    registry: &Registry,
    store: &Store,
    id: &str,
    provider: CustomProvider,
    entry: Option<Entry>,
) -> Result<CustomProvider, SaveError> {
    let reject = |e: anyhow::Error| SaveError::Rejected(e);
    let provider = provider.validated(id).map_err(reject)?;
    let entry = match (provider.auth, entry) {
        (AuthMode::None, None) => None,
        (AuthMode::None, Some(_)) => return Err(reject(anyhow!("a keyless provider takes no credential"))),
        (AuthMode::Bearer, None) => return Err(reject(anyhow!("a bearer provider needs a credential"))),
        (AuthMode::Bearer, Some(mut e)) => {
            if e.origin().is_none() {
                return Err(reject(anyhow!("the credential needs a key or a reference")));
            }
            if e.base_url.is_some() {
                return Err(reject(anyhow!("a custom provider's base URL comes from its definition")));
            }
            e.chat = Some(true);
            Some(e)
        }
    };
    if registry.snapshot().map_err(reject)?.contains_key(id) {
        return Err(reject(anyhow!("custom provider `{id}` already exists")));
    }
    if provider.auth == AuthMode::Bearer && store.get(id).is_some() {
        return Err(reject(anyhow!(
            "a credential for `{id}` already exists; remove it with `mu auth rm {id}` or pick another id"
        )));
    }
    registry.add(id, provider.clone()).map_err(reject)?;
    if let Some(entry) = entry
        && let Err(error) = store.set_if_absent(id, entry)
    {
        let raced = error.downcast_ref::<EntryExists>().is_some();
        return Err(SaveError::KeyNotSaved { id: id.to_string(), raced, error });
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mu-custom-{tag}-{}", uuid::Uuid::new_v4()))
    }

    fn reg(tag: &str) -> (Registry, PathBuf) {
        let d = dir(tag);
        (Registry::at(d.join("providers.toml")), d)
    }

    const FULL: &str = r#"
# my endpoints
[providers.modal]
label = "Modal"
base_url = "https://endpoint.example/v1/"
discover = false

[providers.modal.models."Qwen/example"]
name = "Example"
context_window = 32768
max_tokens = 4096
reasoning = "template"

[providers.modal.models."Qwen/example".compat]
reasoningContentField = "reasoning_content"
requiresReasoningContentForToolCalls = true

[providers.modal.models."Qwen/example".extra_body]
temperature = 0.6
[providers.modal.models."Qwen/example".extra_body_on.chat_template_kwargs]
enable_thinking = true
[providers.modal.models."Qwen/example".extra_body_off.chat_template_kwargs]
enable_thinking = false
"#;

    fn err(text: &str) -> String {
        let (r, d) = reg("err");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(r.path(), text).unwrap();
        let e = format!("{:#}", r.snapshot().unwrap_err());
        let _ = std::fs::remove_dir_all(d);
        e
    }

    #[test]
    fn a_definition_parses_with_its_models_and_extras() {
        let (r, d) = reg("parse");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(r.path(), FULL).unwrap();
        let m = r.snapshot().unwrap();
        let p = &m["modal"];
        assert_eq!(
            (p.base_url.as_str(), p.label(), p.env_hint.as_str()),
            ("https://endpoint.example/v1", "Modal", "MODAL_API_KEY")
        );
        assert_eq!((p.auth, p.discover), (AuthMode::Bearer, false));
        let s = p.spec("Qwen/example", (None, None)).unwrap();
        assert_eq!(
            (s.wire_id.as_str(), s.id.as_str(), s.thinking.mode),
            ("Qwen/example", "modal/Qwen/example", ThinkingMode::Template)
        );
        assert!(s.native && !s.images && s.compat.as_ref().unwrap().requires_reasoning_content_for_tool_calls);
        let c = s.compat.unwrap();
        assert_eq!(c.extra_body_on["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(c.extra_body["temperature"], 0.6);
        assert_eq!(p.chat_url(), "https://endpoint.example/v1/chat/completions");
        // An unlisted id still resolves, with the defaults.
        let u = p.spec("other/x", (None, None)).unwrap();
        assert_eq!((u.context_window, u.max_output, u.thinking.mode), (32_768, 4_096, ThinkingMode::None));
        let listed = p.spec("other/x", (Some(1000), Some(500))).unwrap();
        assert_eq!((listed.context_window, listed.max_output), (1000, 500));
        let small = p.spec("other/x", (Some(2000), None)).unwrap();
        assert_eq!(small.max_output, 2000, "the fallback output never exceeds the window");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn missing_is_empty_and_bad_files_are_errors() {
        let (r, _) = reg("missing");
        assert!(r.snapshot().unwrap().is_empty());
        assert!(err("providers = [").contains("custom-provider configuration"));
        assert!(err("[providers.a]\nbase_url = \"https://x.test/v1\"\ncolour = 1\n").contains("colour"));
        assert!(err("[providers.Bad]\nbase_url = \"https://x.test\"\n").contains("providers.Bad"));
        assert!(err("[providers.openai]\nbase_url = \"https://x.test\"\n").contains("already uses"));
        assert!(err("[providers.custom]\nbase_url = \"https://x.test\"\n").contains("already uses"));
        assert!(err("[providers.a]\nbase_url = \"http://example.com/v1\"\n").contains("http"));
        assert!(err("[providers.a]\nbase_url = \"https://u:p@example.com/v1\"\n").contains("user name"));
        assert!(err("[providers.a]\nbase_url = \"https://example.com/v1?x=1\"\n").contains("query"));
        assert!(err("[providers.a]\nbase_url = \"https://x.test\"\nkey = \"sk\"\n").contains("key"));
        assert!(err("[providers.a]\nbase_url = \"https://x.test\"\nauth = \"header\"\n").contains("header"));
        let m = "[providers.a]\nbase_url = \"https://x.test\"\n[providers.a.models.m]\n";
        assert!(err(&format!("{m}ref = \"x\"\n")).contains("ref"));
        assert!(err(&format!("{m}context_window = 10\nmax_tokens = 20\n")).contains("exceeds"));
        assert!(err(&format!("{m}reasoning = \"effort\"\n")).contains("efforts"));
        assert!(err(&format!("{m}reasoning = \"effort\"\nefforts = [\"high\", \"low\"]\n")).contains("ascending"));
        assert!(err(&format!("{m}efforts = [\"low\"]\n")).contains("efforts"));
        assert!(err(&format!("{m}reasoning = \"template\"\n")).contains("extra_body_on"));
        assert!(err(&format!("{m}[providers.a.models.m.extra_body]\nmodel = \"x\"\n")).contains("extra_body.model"));
        assert!(
            err(&format!("{m}[providers.a.models.m.extra_body]\nreasoning_effort = \"x\"\n"))
                .contains("reasoning_effort")
        );
        assert!(err(&format!("{m}[providers.a.models.m.extra_body]\nwhen = 2026-01-01T00:00:00Z\n")).contains("date"));
        assert!(err(&format!("{m}[providers.a.models.m.extra_body]\nx = nan\n")).contains("non-finite"));
        assert!(err(&format!("{m}[providers.a.models.m.compat]\nextraBody = {{}}\n")).contains("extra_body"));
        assert!(err(&format!("{m}[providers.a.models.m.compat]\nbogus = 1\n")).contains("bogus"));
        assert!(
            err(&format!("{m}[providers.a.models.m.compat]\nmaxTokensField = \"tokens\"\n")).contains("maxTokensField")
        );
        assert!(
            err(&format!("{m}[providers.a.models.m.compat]\nreasoningContentField = \"thoughts\"\n"))
                .contains("reasoningContentField")
        );
        assert!(
            err(&format!("{m}[providers.a.models.m.compat]\nreasoningDisableMode = \"x\"\n"))
                .contains("reasoningDisableMode")
        );
        // A nested `model` key is server-specific, not the envelope.
        let ok = format!("{m}[providers.a.models.m.extra_body.opts]\nmodel = \"x\"\n");
        let (r, d) = reg("nested");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(r.path(), ok).unwrap();
        assert!(r.snapshot().is_ok());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn add_and_remove_keep_comments_and_round_trip() {
        let (r, d) = reg("roundtrip");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(r.path(), "# keep me\n[providers.first]\nbase_url = \"https://a.test/v1\"\n").unwrap();
        let mut p = CustomProvider::new("https://b.test/v1/");
        p.label = Some("Second".into());
        p.models.insert(
            "org/m".into(),
            CustomModel {
                context_window: Some(8192),
                max_tokens: Some(1024),
                reasoning: Reasoning::Template,
                compat: CompletionsCompat {
                    reasoning_content_field: Some("reasoning".into()),
                    requires_reasoning_content_for_tool_calls: true,
                    ..Default::default()
                },
                extra_body_on: serde_json::from_value(
                    serde_json::json!({"chat_template_kwargs": {"enable_thinking": true}}),
                )
                .unwrap(),
                extra_body_off: serde_json::from_value(
                    serde_json::json!({"chat_template_kwargs": {"enable_thinking": false}}),
                )
                .unwrap(),
                ..Default::default()
            },
        );
        r.add("second", p.clone()).unwrap();
        let text = std::fs::read_to_string(r.path()).unwrap();
        assert!(text.starts_with("# keep me\n"), "{text}");
        let all = r.snapshot().unwrap();
        assert_eq!(all.len(), 2, "{text}");
        assert_eq!(all["second"].models, p.validated("second").unwrap().models, "{text}");
        assert_eq!(all["second"].base_url, "https://b.test/v1");
        assert!(!text.contains("supportsDeveloperRole"), "only chosen compat flags are written: {text}");
        let dup = r.add("second", CustomProvider::new("https://c.test")).unwrap_err().to_string();
        assert!(dup.contains("already exists"), "{dup}");
        assert!(r.remove("second").unwrap());
        assert!(!r.remove("second").unwrap());
        let after = std::fs::read_to_string(r.path()).unwrap();
        assert!(after.contains("# keep me") && after.contains("a.test") && !after.contains("b.test"), "{after}");
        assert_eq!(r.snapshot().unwrap().len(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(r.path()).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_corrupt_file_is_never_overwritten() {
        let (r, d) = reg("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(r.path(), "providers = [").unwrap();
        assert!(r.add("x", CustomProvider::new("https://x.test")).is_err());
        assert!(r.remove("x").is_err());
        assert_eq!(std::fs::read_to_string(r.path()).unwrap(), "providers = [");
        // An unwritable destination reports the write, not a panic.
        let (r2, d2) = reg("nowrite");
        std::fs::create_dir_all(r2.path()).unwrap();
        assert!(r2.add("x", CustomProvider::new("https://x.test")).is_err());
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(d2);
    }

    #[test]
    fn concurrent_adds_all_land() {
        let (r, d) = reg("concurrent");
        std::fs::create_dir_all(&d).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let r = r.clone();
                std::thread::spawn(move || r.add(&format!("p{i}"), CustomProvider::new("https://x.test/v1")).unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(r.snapshot().unwrap().len(), 8);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn save_writes_the_definition_then_the_credential_and_reports_what_persisted() {
        let (r, d) = reg("save");
        let s = Store::at(d.join("auth.json"));
        let p = || CustomProvider::new("https://x.test/v1");
        let key = || Entry::literal("custom-key-0123456789");
        assert!(matches!(save(&r, &s, "x", p(), None), Err(SaveError::Rejected(_))), "bearer needs a key");
        assert!(!r.path().exists());
        save(&r, &s, "x", p(), Some(key())).unwrap();
        let e = s.get("x").unwrap();
        assert!(e.routes_chat() && e.base_url.is_none());
        let text = std::fs::read_to_string(r.path()).unwrap();
        assert!(!text.contains("custom-key"), "{text}");
        let again = save(&r, &s, "x", p(), Some(key())).unwrap_err();
        assert!(matches!(again, SaveError::Rejected(_)) && again.to_string().contains("already exists"));
        // A stored credential under a fresh id blocks the save before any file changes.
        s.set("y", Entry::literal("other-key-0123456789")).unwrap();
        let before = std::fs::read_to_string(r.path()).unwrap();
        assert!(matches!(save(&r, &s, "y", p(), Some(key())), Err(SaveError::Rejected(_))));
        assert_eq!(std::fs::read_to_string(r.path()).unwrap(), before);
        // A keyless provider writes no credential.
        let mut k = p();
        k.auth = AuthMode::None;
        save(&r, &s, "keyless", k, None).unwrap();
        assert!(s.get("keyless").is_none());
        // A credential write that fails leaves the definition and says so.
        let bad = Store::at(d.join("blocked").join("auth.json"));
        std::fs::create_dir_all(d.join("blocked").join("auth.json")).unwrap();
        let e = save(&r, &bad, "z", p(), Some(key())).unwrap_err();
        assert!(matches!(e, SaveError::KeyNotSaved { raced: false, .. }));
        assert!(
            e.to_string().contains("saved, but its credential was not saved")
                && e.to_string().contains("mu auth add z")
        );
        assert!(r.snapshot().unwrap().contains_key("z"));
        let _ = std::fs::remove_dir_all(d);
    }
}
