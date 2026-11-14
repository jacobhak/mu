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
//! [providers.modal.models."org/model".cost]    # dollars per million tokens
//! input = 0.30
//! output = 1.20
//! [providers.modal.models."org/model".extra_body_on.chat_template_kwargs]
//! enable_thinking = true
//! ```

mod registry;
mod workflow;

pub use registry::Registry;
pub use workflow::{KeyEdit, SaveError, save, update};

use crate::auth::store::{self, Store};
use crate::provider::providers::{PROVIDERS, SEARCH_KEYS};
use crate::provider::wire::openai_completions::REASONING_FIELDS;
use anyhow::{Context, Result, anyhow, bail};
use mu_base::models::{Api, CompletionsCompat, ModelSpec, ThinkingMode, ThinkingSpec};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

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

/// The effort names a model's ladder may use, lowest first.
pub const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The ladder a model gets when nobody chose one: the levels an effort-capable server is
/// expected to take.
pub fn default_efforts() -> Vec<String> {
    ["low", "medium", "high"].map(String::from).to_vec()
}

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

impl Reasoning {
    pub const NAMES: [&'static str; 3] = ["none", "effort", "template"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Effort => "effort",
            Self::Template => "template",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "none" => Self::None,
            "effort" => Self::Effort,
            "template" => Self::Template,
            _ => return None,
        })
    }
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

/// What a model's tokens cost: dollars per million tokens, as the catalog and the providers' own
/// listings give them. Every field is 0 unless configured.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write: f64,
}

fn is_zero(n: &f64) -> bool {
    *n == 0.0
}

impl Cost {
    fn validate(&self) -> Result<()> {
        for (name, rate) in [
            ("input", self.input),
            ("output", self.output),
            ("cache_read", self.cache_read),
            ("cache_write", self.cache_write),
        ] {
            if !rate.is_finite() || rate < 0.0 {
                bail!("cost.{name} must be a dollar rate per million tokens, zero or more");
            }
        }
        Ok(())
    }
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
    /// What a turn costs, per million tokens; `None` leaves the model unpriced, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
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

/// One request switch (`extra_body_on` / `extra_body_off`) as text: a dotted path and a JSON
/// value, e.g. `chat_template_kwargs.enable_thinking = true`. A value that is not JSON is a string.
pub fn set_switch(map: &mut Map<String, Value>, entry: &str) -> Result<(), String> {
    let (path, raw) = entry.split_once('=').ok_or_else(|| {
        format!(
            "`{}` is not a switch; write `path = value`, e.g. chat_template_kwargs.enable_thinking = true",
            entry.trim()
        )
    })?;
    let (path, raw) = (path.trim(), raw.trim());
    if path.is_empty() || raw.is_empty() {
        return Err("a switch is `path = value`, with both sides filled in".into());
    }
    let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let segs: Vec<&str> = path.split('.').map(str::trim).collect();
    if segs.iter().any(|s| s.is_empty()) {
        return Err(format!("`{path}` has an empty key"));
    }
    insert_switch(map, &segs, path, value)
}

fn insert_switch(map: &mut Map<String, Value>, segs: &[&str], path: &str, value: Value) -> Result<(), String> {
    let Some((head, rest)) = segs.split_first() else { return Ok(()) };
    if rest.is_empty() {
        if map.get(*head).is_some_and(Value::is_object) {
            return Err(format!("`{path}` collides with another switch that is a table"));
        }
        map.insert((*head).to_string(), value);
        return Ok(());
    }
    let slot = map.entry((*head).to_string()).or_insert_with(|| Value::Object(Map::new()));
    match slot.as_object_mut() {
        Some(t) => insert_switch(t, rest, path, value),
        None => Err(format!("`{path}` needs `{head}` to be a table")),
    }
}

/// The switches in `text` (comma- or newline-separated), as the nested maps a request takes.
pub fn parse_switches(text: &str) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for entry in split_switches(text) {
        set_switch(&mut map, &entry)?;
    }
    Ok(map)
}

/// The nested maps back as `path = value` switches, one line for a whole map.
pub fn switches_text(map: &Map<String, Value>) -> String {
    fn flatten(map: &Map<String, Value>, prefix: &str, out: &mut Vec<String>) {
        for (k, v) in map {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match v {
                Value::Object(m) if !m.is_empty() => flatten(m, &path, out),
                other => out.push(format!("{path} = {other}")),
            }
        }
    }
    let mut out = Vec::new();
    flatten(map, "", &mut out);
    out.join(", ")
}

/// Splits at commas and newlines that are outside a JSON string or bracket.
fn split_switches(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quote, mut depth, mut escaped) = (false, 0i32, false);
    for c in text.chars() {
        if escaped {
            escaped = false;
            cur.push(c);
            continue;
        }
        match c {
            '\\' if quote => {
                escaped = true;
                cur.push(c);
            }
            '"' => {
                quote = !quote;
                cur.push(c);
            }
            '[' | '{' if !quote => {
                depth += 1;
                cur.push(c);
            }
            ']' | '}' if !quote => {
                depth -= 1;
                cur.push(c);
            }
            ',' | '\n' if !quote && depth <= 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

impl CustomModel {
    /// A model nobody has configured yet: an effort ladder the panel and `mu providers add` start
    /// from, so `/thinking` has levels to choose. Only a thinking level above `off` sends anything.
    pub fn with_effort_defaults() -> Self {
        Self { reasoning: Reasoning::Effort, efforts: default_efforts(), ..Self::default() }
    }

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
        if let Some(c) = &self.cost {
            c.validate()?;
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

/// The environment variable a provider's key conventionally comes from; also its redaction label.
pub fn env_hint(id: &str) -> String {
    format!("{}_API_KEY", id.to_ascii_uppercase().replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Entry;
    use std::path::PathBuf;

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

[providers.modal.models."Qwen/example".cost]
input = 0.3
output = 1.2
cache_read = 0.05

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
        let cost = p.models["Qwen/example"].cost.unwrap();
        assert_eq!((cost.input, cost.output, cost.cache_read, cost.cache_write), (0.3, 1.2, 0.05, 0.0));
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
        assert!(err(&format!("{m}cost = {{ input = -1.0 }}\n")).contains("cost.input"));
        assert!(err(&format!("{m}cost = {{ output = inf }}\n")).contains("cost.output"));
        assert!(err(&format!("{m}cost = {{ tokens = 1.0 }}\n")).contains("tokens"));
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
                cost: Some(Cost { input: 0.3, output: 1.2, ..Default::default() }),
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

    #[test]
    fn request_switches_round_trip_as_text() {
        let text = "chat_template_kwargs.enable_thinking = true, \
                    chat_template_kwargs.thinking_budget = 512, note = \"a, b = c\"";
        let m = parse_switches(text).unwrap();
        assert_eq!(m["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(m["chat_template_kwargs"]["thinking_budget"], 512);
        assert_eq!(m["note"], "a, b = c", "quoted text is one value, commas and all");
        assert_eq!(
            switches_text(&m),
            "chat_template_kwargs.enable_thinking = true, \
             chat_template_kwargs.thinking_budget = 512, note = \"a, b = c\""
        );
        // A bare word is a string; JSON values keep their type.
        assert_eq!(parse_switches("a = on").unwrap()["a"], "on");
        assert!(parse_switches("a = {b: 1}").unwrap()["a"].is_string(), "not JSON, so a string");
        assert!(parse_switches("").unwrap().is_empty());
        // Entries say what they should look like.
        assert!(parse_switches("a").unwrap_err().contains("path = value"));
        assert!(parse_switches("a = ").unwrap_err().contains("both sides"));
        assert!(parse_switches("a..b = 1").unwrap_err().contains("empty key"));
        assert!(parse_switches("a.b = 1, a = 2").unwrap_err().contains("table"));
        assert!(parse_switches("a = 2, a.b = 1").unwrap_err().contains("table"));
        let mut m = Map::new();
        set_switch(&mut m, "nested.on = false").unwrap();
        assert_eq!(switches_text(&m), "nested.on = false");
    }

    #[test]
    fn a_new_model_starts_with_the_effort_ladder() {
        let m = CustomModel::with_effort_defaults();
        assert_eq!(m.reasoning, Reasoning::Effort);
        assert_eq!(m.efforts, ["low", "medium", "high"]);
        m.validate().unwrap();
    }

    #[test]
    fn set_replaces_one_entry_and_keeps_the_others() {
        let (r, d) = reg("set");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            r.path(),
            "# keep me\n[providers.a]\nbase_url = \"https://a.test/v1\"\n\n[providers.b]\nbase_url = \"https://b.test/v1\"\n",
        )
        .unwrap();
        let mut p = CustomProvider::new("https://a2.test/v1");
        p.models.insert("org/m".into(), CustomModel::with_effort_defaults());
        r.set("a", p).unwrap();
        let all = r.snapshot().unwrap();
        assert_eq!(all["a"].base_url, "https://a2.test/v1");
        assert_eq!(all["a"].models["org/m"].efforts, ["low", "medium", "high"]);
        assert_eq!(all["b"].base_url, "https://b.test/v1");
        let text = std::fs::read_to_string(r.path()).unwrap();
        assert!(text.starts_with("# keep me\n"), "{text}");
        let missing = r.set("nope", CustomProvider::new("https://x.test/v1")).unwrap_err().to_string();
        assert!(missing.contains("is not in"), "{missing}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn update_keeps_the_stored_key_unless_a_new_one_is_given() {
        let (r, d) = reg("update");
        let s = Store::at(d.join("auth.json"));
        let def = || CustomProvider::new("https://x.test/v1");
        let key = |k: &str| Entry::literal(k);
        save(&r, &s, "x", def(), Some(key("first-key-0123456789"))).unwrap();
        let shown = s.get("x").unwrap();

        // An edit of a model keeps the credential exactly as it was.
        let mut p = def();
        p.models.insert("org/m".into(), CustomModel::with_effort_defaults());
        update(&r, &s, "x", p, KeyEdit::Keep).unwrap();
        assert_eq!(s.get("x").unwrap().key, shown.key);
        assert_eq!(r.get("x").unwrap().unwrap().models["org/m"].efforts, ["low", "medium", "high"]);

        // A newly chosen credential replaces it and is used for chat.
        update(&r, &s, "x", def(), KeyEdit::Set(key("second-key-0123456789"))).unwrap();
        let e = s.get("x").unwrap();
        assert_eq!(e.key.as_deref(), Some("second-key-0123456789"));
        assert!(e.routes_chat() && e.base_url.is_none());

        // Keyless: an entry edits it without touching the key, and a key is refused.
        let mut k = def();
        k.auth = AuthMode::None;
        update(&r, &s, "x", k.clone(), KeyEdit::Keep).unwrap();
        assert_eq!(s.get("x").unwrap().key.as_deref(), Some("second-key-0123456789"), "keys are not deleted here");
        let refused = update(&r, &s, "x", k, KeyEdit::Set(key("third-key-0123456789"))).unwrap_err();
        assert!(refused.to_string().contains("keyless"), "{refused}");

        // An id that is not there, and a credential write that fails, both say so.
        let missing = update(&r, &s, "nope", def(), KeyEdit::Keep).unwrap_err();
        assert!(missing.to_string().contains("does not exist"), "{missing}");
        std::fs::create_dir_all(d.join("blocked").join("auth.json")).unwrap();
        let bad = Store::at(d.join("blocked").join("auth.json"));
        let e = update(&r, &bad, "x", def(), KeyEdit::Set(key("fourth-key-0123456789"))).unwrap_err();
        assert!(matches!(e, SaveError::KeyNotSaved { raced: false, .. }), "{e}");
        assert!(r.get("x").unwrap().is_some(), "the definition was written");
        let _ = std::fs::remove_dir_all(d);
    }
}
