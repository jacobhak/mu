//! One-time import of the pi coding agent's setup (`~/.pi/agent`): reading and planning, with no
//! side effects. pi's files are only read. `plan` takes the agent dir, so tests use temp dirs.
//! Turning the plan into mu's files is `mu_provider::pi_import` (endpoints and keys) and
//! `Configs::apply_pi_import` (models.toml, `[theme]`, `[display]`).

use crate::model_config::{Import, MAIN_SET, ModelConfig};
use crate::models::Thinking;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

/// What applying part of an import did: a line for each thing written, and for each that failed.
/// Error text comes from mu's writers and holds paths and reasons, never a key.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Applied {
    pub done: Vec<String>,
    pub failed: Vec<String>,
}

impl Applied {
    pub fn record(&mut self, r: Result<String, String>) {
        match r {
            Ok(line) => self.done.push(line),
            Err(e) => self.failed.push(e),
        }
    }

    pub fn extend(&mut self, other: Applied) {
        self.done.extend(other.done);
        self.failed.extend(other.failed);
    }
}

/// pi's context window and output limit for a `models.json` model that sets none.
const PI_DEFAULT_CONTEXT: u32 = 128_000;
const PI_DEFAULT_OUTPUT: u32 = 16_384;

/// Where a key comes from. A literal never shows in `Debug` or in any line of a plan.
#[derive(Clone, PartialEq)]
pub enum KeySource {
    Literal(String),
    Env(String),
    Cmd(String),
}

impl fmt::Debug for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(…)"),
            Self::Env(n) => f.debug_tuple("Env").field(n).finish(),
            Self::Cmd(_) => f.write_str("Cmd(…)"),
        }
    }
}

impl KeySource {
    /// How the key is taken, for a listing: never the key, and for a command only its program.
    pub fn describe(&self) -> String {
        match self {
            Self::Literal(_) => "stored key".into(),
            Self::Env(n) => format!("env {n}"),
            Self::Cmd(c) => {
                format!("command {}", c.split_whitespace().next().unwrap_or_default())
            }
        }
    }
}

/// A `models.json` model of an `openai-completions` provider, with pi's defaults filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct PiModel {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: bool,
    pub images: bool,
    pub context_window: u32,
    pub max_tokens: u32,
    /// Dollars per million tokens: input, output, cache read, cache write.
    pub cost: Option<[f64; 4]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PiEndpoint {
    pub id: String,
    pub name: Option<String>,
    pub base_url: String,
    pub key: Option<KeySource>,
    pub models: Vec<PiModel>,
    /// Things of this provider mu cannot carry, shown beside it.
    pub notes: Vec<String>,
}

/// What pi's files say, in the parts mu can use, and a line for each part it cannot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PiPlan {
    /// `defaultProvider` and the role value (`provider/model[:level]`) it makes.
    pub default: Option<(String, String)>,
    /// `(endpoint id, model key, "fN")` for each `compaction.modelOverrides` entry that maps.
    pub thresholds: Vec<(String, String, String)>,
    /// pi's theme name and the mu theme it maps to.
    pub theme: Option<(String, &'static str)>,
    pub hide_thinking: Option<bool>,
    pub endpoints: Vec<PiEndpoint>,
    /// `api_key` logins of `auth.json`, by pi's provider id.
    pub keys: Vec<(String, KeySource)>,
    /// Providers pi is logged in to with OAuth, never copied.
    pub oauth: Vec<String>,
    /// Provider ids of `models.json` that were not imported.
    pub skipped_providers: BTreeSet<String>,
    pub skipped: Vec<String>,
}

impl PiPlan {
    /// Whether anything could be copied.
    pub fn has_content(&self) -> bool {
        self.default.is_some()
            || self.theme.is_some()
            || self.hide_thinking.is_some()
            || !self.endpoints.is_empty()
            || !self.keys.is_empty()
    }

    /// The `models.toml` this would write: the default model as role `default` of set `main`, and
    /// the thresholds of endpoints not in `dropped` (ones mu refused). Lines for what could not be
    /// written come second. `None` when there is nothing.
    pub fn models_import(&self, dropped: &BTreeSet<String>) -> (Option<Import>, Vec<String>) {
        let mut cfg = ModelConfig::default();
        let mut lines = Vec::new();
        let mut notes = Vec::new();
        if let Some((provider, role)) = &self.default {
            if dropped.contains(provider) {
                notes.push(format!("skipped pi's default model `{role}`: its provider `{provider}` was not imported"));
            } else {
                cfg.sets.insert(MAIN_SET.into(), [("default".to_string(), role.clone())].into());
                lines.push(format!("set {MAIN_SET}: default = {role}"));
            }
        }
        for (endpoint, key, value) in &self.thresholds {
            if dropped.contains(endpoint) {
                continue;
            }
            cfg.compaction.thresholds.insert(key.clone(), toml::Value::String(value.clone()));
            lines.push(format!("compaction threshold: {key} = {value}"));
        }
        if cfg.sets.is_empty() && cfg.compaction.thresholds.is_empty() {
            return (None, notes);
        }
        (Some(Import { config: cfg, lines }), notes)
    }
}

/// Reads the pi agent dir `dir`. A missing file is nothing; a file that is not JSON is one line
/// and otherwise ignored.
pub fn plan(dir: &Path) -> PiPlan {
    let mut p = PiPlan::default();
    let settings = read_json(dir, "settings.json", &mut p.skipped);
    let models = read_json(dir, "models.json", &mut p.skipped);
    let auth = read_json(dir, "auth.json", &mut p.skipped);
    read_auth(auth.as_ref(), &mut p);
    read_endpoints(models.as_ref(), &mut p);
    if let Some(s) = &settings {
        read_settings(s, &mut p);
    }
    if !p.oauth.is_empty() {
        p.skipped.push(format!(
            "skipped pi logins ({}): OAuth logins are never copied; the gateway-pi plugin serves them \
             (`[gateway] plugin = \"gateway-pi\"`)",
            p.oauth.join(", ")
        ));
    }
    p
}

fn read_json(dir: &Path, name: &str, skipped: &mut Vec<String>) -> Option<Value> {
    let text = match std::fs::read_to_string(dir.join(name)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            skipped.push(format!("skipped pi's {name}: {}", e.kind()));
            return None;
        }
    };
    // `from_str` into a `Value` only fails on syntax, and that error never quotes the text.
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => Some(v),
        Ok(_) => {
            skipped.push(format!("skipped pi's {name}: not a JSON object"));
            None
        }
        Err(e) => {
            skipped.push(format!("skipped pi's {name}: not valid JSON ({e})"));
            None
        }
    }
}

fn nonempty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// pi and mu name providers the same (both follow the pi-ai catalog's ids), so this is the
/// identity; a pi id that differs from mu's goes here.
pub fn mu_provider_id(pi: &str) -> &str {
    pi
}

fn read_settings(s: &Value, p: &mut PiPlan) {
    let provider = nonempty(s.get("defaultProvider"));
    let model = nonempty(s.get("defaultModel"));
    match (provider, model) {
        (Some(prov), Some(model)) => {
            let mu = mu_provider_id(prov);
            let level = match nonempty(s.get("defaultThinkingLevel")) {
                None => None,
                Some(l) => match Thinking::parse(l) {
                    Some(t) => Some(t),
                    None => {
                        p.skipped.push(format!(
                            "skipped pi's defaultThinkingLevel `{l}`: not one of off, minimal, low, medium, high, xhigh, max"
                        ));
                        None
                    }
                },
            };
            let role = match level {
                Some(t) => format!("{mu}/{model}:{}", t.as_str()),
                None => format!("{mu}/{model}"),
            };
            if p.skipped_providers.contains(prov) {
                p.skipped.push(format!(
                    "skipped pi's default model `{prov}/{model}`: provider `{prov}` is a custom provider mu did not import"
                ));
            } else {
                p.default = Some((mu.to_string(), role));
            }
        }
        (None, Some(_)) => p.skipped.push("skipped pi's defaultModel: no defaultProvider".into()),
        (Some(_), None) => p.skipped.push("skipped pi's defaultProvider: no defaultModel".into()),
        (None, None) => {
            if nonempty(s.get("defaultThinkingLevel")).is_some() {
                p.skipped.push("skipped pi's defaultThinkingLevel: no default model".into());
            }
        }
    }
    read_compaction(s, p);
    match nonempty(s.get("theme")) {
        Some("dark") => p.theme = Some(("dark".into(), "tokyo-night")),
        Some("light") => p.theme = Some(("light".into(), "catppuccin-latte")),
        Some("system") => p.theme = Some(("system".into(), "terminal")),
        Some(other) => {
            p.skipped.push(format!("skipped pi's theme `{other}`: only dark, light and system map to mu themes"))
        }
        None => {}
    }
    p.hide_thinking = s.get("hideThinkingBlock").and_then(Value::as_bool);
}

fn read_compaction(s: &Value, p: &mut PiPlan) {
    let Some(over) = s.get("compaction").and_then(|c| c.get("modelOverrides")).and_then(Value::as_object) else {
        return;
    };
    let mut keep = Vec::new();
    let mut unknown_window = Vec::new();
    for (key, o) in over {
        if o.get("keepRecentTokens").is_some_and(|v| !v.is_null()) {
            keep.push(key.as_str());
        }
        let Some(reserve) = o.get("reserveTokens").and_then(Value::as_u64) else { continue };
        let Some((provider, model)) = key.split_once('/') else {
            p.skipped.push(format!("skipped compaction override `{key}`: not `provider/model`"));
            continue;
        };
        // pi compacts above `window - reserveTokens`; that is mu's `fN` once the window is known.
        let window = p
            .endpoints
            .iter()
            .find(|e| e.id == provider)
            .and_then(|e| e.models.iter().find(|m| m.id == model))
            .map(|m| u64::from(m.context_window));
        match window {
            Some(w) if reserve < w => {
                p.thresholds.push((provider.to_string(), key.clone(), format!("f{}", w - reserve)));
            }
            Some(_) => p
                .skipped
                .push(format!("skipped compaction override `{key}`: reserveTokens is not below the context window")),
            None => unknown_window.push(key.as_str()),
        }
    }
    if !unknown_window.is_empty() {
        p.skipped.push(format!(
            "skipped compaction reserveTokens of {}: mu needs the model's context window, which only a models.json \
             endpoint gives; set `[compaction.thresholds]` in models.toml by hand",
            unknown_window.join(", ")
        ));
    }
    if !keep.is_empty() {
        p.skipped.push(format!(
            "skipped compaction keepRecentTokens of {}: mu has no equivalent (it keeps a fixed tail)",
            keep.join(", ")
        ));
    }
}

fn read_auth(auth: Option<&Value>, p: &mut PiPlan) {
    let Some(map) = auth.and_then(Value::as_object) else { return };
    for (provider, entry) in map {
        match entry.get("type").and_then(Value::as_str) {
            Some("api_key") => match nonempty(entry.get("key")) {
                Some(key) => {
                    let src = match key.strip_prefix('!') {
                        Some(cmd) if !cmd.trim().is_empty() => KeySource::Cmd(cmd.trim().to_string()),
                        Some(_) => {
                            p.skipped.push(format!("skipped pi's key for `{provider}`: an empty !command"));
                            continue;
                        }
                        None => KeySource::Literal(key.to_string()),
                    };
                    p.keys.push((provider.clone(), src));
                }
                None => p.skipped.push(format!("skipped pi's key for `{provider}`: it has no key")),
            },
            Some("oauth") => p.oauth.push(provider.clone()),
            _ => p.skipped.push(format!("skipped pi's login for `{provider}`: unknown type")),
        }
    }
}

/// An API key as `models.json` writes it: `!command`, `$NAME` or `${NAME}` (the whole value), or a
/// literal in which `$$` is `$` and `$!` is `!`.
pub fn parse_api_key(raw: &str) -> Result<KeySource, &'static str> {
    if let Some(cmd) = raw.strip_prefix('!') {
        return if cmd.trim().is_empty() { Err("an empty !command") } else { Ok(KeySource::Cmd(cmd.trim().into())) };
    }
    let (mut lit, mut var) = (String::new(), None::<String>);
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        if c != '$' {
            lit.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let after = &rest[1..];
        if let Some(r) = after.strip_prefix('$') {
            lit.push('$');
            rest = r;
        } else if let Some(r) = after.strip_prefix('!') {
            lit.push('!');
            rest = r;
        } else if let Some((name, r)) = env_name(after) {
            if var.replace(name.to_string()).is_some() {
                return Err("it interpolates more than one variable");
            }
            rest = r;
        } else {
            lit.push('$');
            rest = after;
        }
    }
    match var {
        Some(name) if lit.is_empty() => Ok(KeySource::Env(name)),
        Some(_) => Err("it mixes $NAME interpolation with other text"),
        None if lit.trim().is_empty() => Err("it is empty"),
        None => Ok(KeySource::Literal(lit)),
    }
}

/// `NAME` or `{NAME}` at the start of `s`, and the rest.
fn env_name(s: &str) -> Option<(&str, &str)> {
    let valid = |n: &str| {
        let mut c = n.chars();
        c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
            && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if let Some(inner) = s.strip_prefix('{') {
        let (name, rest) = inner.split_once('}')?;
        return valid(name).then_some((name, rest));
    }
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    let (name, rest) = s.split_at(end);
    valid(name).then_some((name, rest))
}

fn read_endpoints(models: Option<&Value>, p: &mut PiPlan) {
    let Some(providers) = models.and_then(|m| m.get("providers")).and_then(Value::as_object) else { return };
    for (id, def) in providers {
        match endpoint(id, def, &p.keys) {
            Ok(e) => p.endpoints.push(e),
            Err(why) => {
                p.skipped.push(format!("skipped pi provider `{id}`: {why}"));
                // One without models of its own is a built-in provider's setting, still a provider.
                if def.get("models").and_then(Value::as_array).is_some_and(|m| !m.is_empty()) {
                    p.skipped_providers.insert(id.clone());
                }
            }
        }
    }
}

fn endpoint(id: &str, def: &Value, logins: &[(String, KeySource)]) -> Result<PiEndpoint, String> {
    let models = def.get("models").and_then(Value::as_array).filter(|m| !m.is_empty());
    let Some(models) = models else {
        return Err("it sets no models, only baseUrl or modelOverrides of a built-in provider".into());
    };
    let provider_api = nonempty(def.get("api"));
    let provider_url = nonempty(def.get("baseUrl"));
    let mut notes = Vec::new();
    let mut out = Vec::new();
    let mut other_api = None;
    for m in models {
        let Some(mid) = nonempty(m.get("id")) else { continue };
        let api = nonempty(m.get("api")).or(provider_api);
        if api != Some("openai-completions") {
            other_api.get_or_insert(api.unwrap_or("none"));
            notes.push(format!("model `{mid}` not copied: api `{}`", api.unwrap_or("none")));
            continue;
        }
        if nonempty(m.get("baseUrl")).is_some_and(|u| Some(u) != provider_url) {
            notes.push(format!("model `{mid}` not copied: it has its own baseUrl"));
            continue;
        }
        out.push(model(mid, m));
    }
    if out.is_empty() {
        return Err(match other_api {
            Some("none") | None => "no api (mu's custom endpoints speak openai-completions)".to_string(),
            Some(api) => {
                format!("api `{api}` is not openai-completions (mu's custom endpoints speak Chat Completions)")
            }
        });
    }
    let base_url = provider_url.ok_or("no baseUrl")?.to_string();
    let key = match def.get("apiKey") {
        Some(Value::String(raw)) => Some(parse_api_key(raw).map_err(|why| format!("apiKey: {why}"))?),
        Some(Value::Null) | None => logins.iter().find(|(p, _)| p == id).map(|(_, k)| k.clone()),
        Some(_) => return Err("apiKey is not a string".into()),
    };
    if key.is_none() {
        return Err("no apiKey, and no pi login for it (`mu providers add --no-auth` adds a keyless endpoint)".into());
    }
    if def.get("headers").and_then(Value::as_object).is_some_and(|h| !h.is_empty()) {
        notes.push("headers not copied".into());
    }
    Ok(PiEndpoint {
        id: id.to_string(),
        name: nonempty(def.get("name")).map(String::from),
        base_url,
        key,
        models: out,
        notes,
    })
}

fn model(id: &str, m: &Value) -> PiModel {
    let window = m.get("contextWindow").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    let context_window = window.unwrap_or(PI_DEFAULT_CONTEXT);
    let max = m.get("maxTokens").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    let cost = m.get("cost").and_then(Value::as_object).filter(|c| !c.is_empty()).map(|c: &Map<String, Value>| {
        let rate = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        [rate("input"), rate("output"), rate("cacheRead"), rate("cacheWrite")]
    });
    PiModel {
        id: id.to_string(),
        name: nonempty(m.get("name")).filter(|n| *n != id).map(String::from),
        reasoning: m.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
        images: m.get("input").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some("image"))),
        context_window,
        max_tokens: max.unwrap_or(PI_DEFAULT_OUTPUT.min(context_window)),
        cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> mu_testutil::TempDir {
        let d = mu_testutil::tmp();
        for (name, text) in files {
            std::fs::write(d.join(name), text).unwrap();
        }
        d
    }

    #[test]
    fn api_key_forms() {
        let ok = |s| parse_api_key(s).unwrap();
        assert_eq!(ok("sk-test-1234"), KeySource::Literal("sk-test-1234".into()));
        assert_eq!(ok("$MY_KEY"), KeySource::Env("MY_KEY".into()));
        assert_eq!(ok("${MY_KEY}"), KeySource::Env("MY_KEY".into()));
        assert_eq!(ok("!pass show x"), KeySource::Cmd("pass show x".into()));
        assert_eq!(ok("$$abc"), KeySource::Literal("$abc".into()));
        assert_eq!(ok("$!abc"), KeySource::Literal("!abc".into()));
        assert_eq!(ok("a$"), KeySource::Literal("a$".into()));
        assert!(parse_api_key("Bearer $TOKEN").unwrap_err().contains("mixes"));
        assert!(parse_api_key("$A$B").unwrap_err().contains("more than one"));
        assert!(parse_api_key("!  ").is_err() && parse_api_key("  ").is_err());
    }

    #[test]
    fn settings_map_to_default_thresholds_theme_and_display() {
        let d = dir_with(&[
            (
                "settings.json",
                r#"{"defaultProvider":"anthropic","defaultModel":"claude-opus-5-5","defaultThinkingLevel":"minimal",
                    "theme":"light","hideThinkingBlock":true,"enabledModels":["x/y"],
                    "compaction":{"modelOverrides":{
                        "local/m1":{"reserveTokens":8000,"keepRecentTokens":5000},
                        "anthropic/claude-opus-5-5":{"reserveTokens":1000},
                        "local/missing":{"reserveTokens":1}}}}"#,
            ),
            (
                "models.json",
                r#"{"providers":{"local":{"baseUrl":"http://127.0.0.1:8000/v1","api":"openai-completions","apiKey":"$LOCAL_KEY",
                    "models":[{"id":"m1","contextWindow":32000}]}}}"#,
            ),
        ]);
        let p = plan(&d);
        assert_eq!(p.default, Some(("anthropic".into(), "anthropic/claude-opus-5-5:low".into())));
        assert_eq!(p.thresholds, [("local".into(), "local/m1".into(), "f24000".into())]);
        assert_eq!(p.theme, Some(("light".into(), "catppuccin-latte")));
        assert_eq!(p.hide_thinking, Some(true));
        let text = p.skipped.join("\n");
        assert!(text.contains("anthropic/claude-opus-5-5, local/missing"), "{text}");
        assert!(text.contains("keepRecentTokens of local/m1"), "{text}");
        let (imp, notes) = p.models_import(&BTreeSet::new());
        let imp = imp.unwrap();
        assert!(notes.is_empty());
        assert_eq!(imp.config.sets["main"]["default"], "anthropic/claude-opus-5-5:low");
        assert_eq!(imp.config.compaction.thresholds["local/m1"], toml::Value::String("f24000".into()));
        let (imp, notes) = p.models_import(&["anthropic".to_string(), "local".to_string()].into());
        assert!(imp.is_none() && notes.len() == 1, "{notes:?}");
    }

    #[test]
    fn theme_names_and_bad_values() {
        for (pi, mu) in [("dark", "tokyo-night"), ("light", "catppuccin-latte"), ("system", "terminal")] {
            let d = dir_with(&[("settings.json", &format!(r#"{{"theme":"{pi}"}}"#))]);
            assert_eq!(plan(&d).theme.map(|t| t.1), Some(mu));
        }
        let d = dir_with(&[("settings.json", r#"{"theme":"mine","defaultThinkingLevel":"high","defaultModel":"x"}"#)]);
        let p = plan(&d);
        assert!(p.theme.is_none() && !p.has_content());
        let text = p.skipped.join("\n");
        assert!(text.contains("theme `mine`") && text.contains("no defaultProvider"), "{text}");
        let d = dir_with(&[(
            "settings.json",
            r#"{"defaultProvider":"a","defaultModel":"b","defaultThinkingLevel":"turbo"}"#,
        )]);
        let p = plan(&d);
        assert_eq!(p.default, Some(("a".into(), "a/b".into())));
        assert!(p.skipped[0].contains("turbo"));
    }

    #[test]
    fn endpoints_models_and_skips() {
        let d = dir_with(&[
            (
                "models.json",
                r#"{"providers":{
                  "vllm":{"name":"My vLLM","baseUrl":"https://v.example/v1","api":"openai-completions","apiKey":"!pass show vllm",
                    "headers":{"x":"y"},
                    "models":[{"id":"a/b","name":"A B","reasoning":true,"input":["text","image"],"contextWindow":65536,"maxTokens":8192,
                              "cost":{"input":1,"output":2,"cacheRead":0.5,"cacheWrite":3}},
                             {"id":"plain"},
                             {"id":"anth","api":"anthropic-messages"}]},
                  "anthropic":{"baseUrl":"https://proxy.example"},
                  "gemini":{"baseUrl":"https://g.example","api":"google-generative-ai","apiKey":"k","models":[{"id":"g"}]},
                  "nokey":{"baseUrl":"https://n.example","api":"openai-completions","models":[{"id":"m"}]},
                  "mixed":{"baseUrl":"https://n.example","api":"openai-completions","apiKey":"Bearer $T","models":[{"id":"m"}]},
                  "viaauth":{"baseUrl":"https://n.example","api":"openai-completions","models":[{"id":"m"}]}
                }}"#,
            ),
            (
                "auth.json",
                r#"{"viaauth":{"type":"api_key","key":"sk-test-1234"},"anthropic":{"type":"api_key","key":"sk-test-5678"},
                    "openai":{"type":"api_key","key":"!op read x"},"openai-codex":{"type":"oauth","access":"tok"},
                    "bad":{"type":"api_key"}}"#,
            ),
        ]);
        let p = plan(&d);
        let ids: Vec<&str> = p.endpoints.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["vllm", "viaauth"]);
        let v = &p.endpoints[0];
        assert_eq!(v.name.as_deref(), Some("My vLLM"));
        assert_eq!(v.key, Some(KeySource::Cmd("pass show vllm".into())));
        assert_eq!(v.models.len(), 2);
        assert_eq!(
            v.models[0],
            PiModel {
                id: "a/b".into(),
                name: Some("A B".into()),
                reasoning: true,
                images: true,
                context_window: 65536,
                max_tokens: 8192,
                cost: Some([1.0, 2.0, 0.5, 3.0]),
            }
        );
        assert_eq!(
            (v.models[1].context_window, v.models[1].max_tokens, v.models[1].reasoning),
            (128_000, 16_384, false)
        );
        assert!(
            v.notes.iter().any(|n| n.contains("`anth`")) && v.notes.iter().any(|n| n.contains("headers")),
            "{:?}",
            v.notes
        );
        assert_eq!(p.endpoints[1].key, Some(KeySource::Literal("sk-test-1234".into())));
        let text = p.skipped.join("\n");
        for want in [
            "`anthropic`: it sets no models",
            "`gemini`: api `google-generative-ai`",
            "`nokey`: no apiKey",
            "`mixed`: apiKey: it mixes",
            "OAuth logins are never copied",
            "openai-codex",
            "`bad`",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        assert_eq!(p.keys.len(), 3);
        assert!(!text.contains("sk-test") && !format!("{p:?}").contains("sk-test"));
    }

    #[test]
    fn default_model_of_a_skipped_custom_provider_is_unmappable() {
        let d = dir_with(&[
            ("settings.json", r#"{"defaultProvider":"gemini","defaultModel":"g"}"#),
            (
                "models.json",
                r#"{"providers":{"gemini":{"baseUrl":"https://g.example","api":"google-generative-ai","apiKey":"k","models":[{"id":"g"}]}}}"#,
            ),
        ]);
        let p = plan(&d);
        assert!(
            p.default.is_none() && p.skipped.iter().any(|s| s.contains("default model `gemini/g`")),
            "{:?}",
            p.skipped
        );
    }

    #[test]
    fn bad_files_are_one_line_each_and_nothing_is_nothing() {
        let d = dir_with(&[("settings.json", "{not json"), ("models.json", "[]")]);
        let p = plan(&d);
        assert!(!p.has_content() && p.skipped.len() == 2, "{:?}", p.skipped);
        assert_eq!(plan(&mu_testutil::tmp()), PiPlan::default());
    }
}
