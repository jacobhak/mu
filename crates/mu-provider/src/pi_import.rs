//! Applying pi's setup (`mu_base::pi_import::PiPlan`): its `models.json` endpoints become custom
//! endpoints, its `auth.json` API keys become mu's keys, and the rest goes to mu's own settings
//! through the writers `mu providers add`, `mu auth add` and `/theme` use. `resolve` checks the
//! plan against what mu already has, without writing; `Resolved::apply` writes. Nothing here
//! reads or writes pi's files, and no key appears in any line.

use crate::auth::store::{Entry, Store};
use crate::provider::custom::{
    self, AuthMode, Cost, CustomModel, CustomProvider, Reasoning, Registry, default_efforts,
};
use crate::provider::providers::{api_key_defs, native_def};
use mu_base::config::{self, Configs};
use mu_base::model_config::Import;
use mu_base::pi_import::{Applied, KeySource, PiEndpoint, PiPlan, mu_provider_id};
use std::collections::BTreeSet;

/// What importing would do, checked against mu's files as they are now.
#[derive(Debug, Clone)]
pub struct Resolved {
    endpoints: Vec<(String, CustomProvider, Entry)>,
    keys: Vec<(String, Entry)>,
    models: Option<Import>,
    theme: Option<&'static str>,
    hide: Option<bool>,
    /// What will be copied, then what will not and why; one line each, no key.
    pub lines: Vec<String>,
}

fn entry(src: &KeySource) -> anyhow::Result<Entry> {
    match src {
        KeySource::Literal(k) => Ok(Entry::literal(k)),
        KeySource::Env(n) => Entry::reference(&format!("${n}")),
        KeySource::Cmd(c) => Entry::reference(&format!("!{c}")),
    }
}

fn provider(e: &PiEndpoint) -> CustomProvider {
    let mut p = CustomProvider::new(&e.base_url);
    p.label = e.name.clone();
    p.auth = AuthMode::Bearer;
    // pi lists the models it was given; mu does not ask the server for more.
    p.discover = false;
    for m in &e.models {
        let (reasoning, efforts) =
            if m.reasoning { (Reasoning::Effort, default_efforts()) } else { (Reasoning::None, Vec::new()) };
        p.models.insert(
            m.id.clone(),
            CustomModel {
                name: m.name.clone(),
                context_window: Some(m.context_window),
                max_tokens: Some(m.max_tokens),
                images: m.images,
                reasoning,
                efforts,
                cost: m.cost.map(|[input, output, cache_read, cache_write]| Cost {
                    input,
                    output,
                    cache_read,
                    cache_write,
                }),
                ..Default::default()
            },
        );
    }
    p
}

/// A validation message without the endpoint's URL, which may carry a token in its query.
fn without_url(msg: String, url: &str) -> String {
    msg.replace(url, "<url>").replace(url.trim().trim_end_matches('/'), "<url>")
}

pub fn resolve(plan: &PiPlan, registry: &Registry, store: &Store, force: bool) -> Resolved {
    let mut out = Resolved { endpoints: vec![], keys: vec![], models: None, theme: None, hide: None, lines: vec![] };
    let mut skipped = plan.skipped.clone();
    let mut dropped = BTreeSet::new();
    let existing = registry.snapshot();
    for e in &plan.endpoints {
        let refuse = |why: String| format!("skipped pi provider `{}`: {why}", e.id);
        let result: Result<(CustomProvider, Entry), String> = (|| {
            let existing = existing.as_ref().map_err(|err| format!("{err:#}"))?;
            let p = provider(e).validated(&e.id).map_err(|err| without_url(format!("{err:#}"), &e.base_url))?;
            if existing.contains_key(&e.id) {
                return Err("a custom endpoint of that name exists (never overwritten)".to_string());
            }
            if store.has(&e.id) {
                return Err("a stored key for that name exists (never overwritten)".to_string());
            }
            let key = e.key.as_ref().ok_or("no key")?;
            let entry = entry(key).map_err(|err| format!("{err:#}"))?;
            Ok((p, entry))
        })();
        match result {
            Ok((p, entry)) => {
                let mut line = format!(
                    "custom endpoint `{}`: {}, {} models, key: {}",
                    e.id,
                    p.base_url,
                    p.models.len(),
                    e.key.as_ref().map(KeySource::describe).unwrap_or_default()
                );
                for n in &e.notes {
                    line.push_str(&format!("; {n}"));
                }
                out.lines.push(line);
                out.endpoints.push((e.id.clone(), p, entry));
            }
            Err(why) => {
                skipped.push(refuse(why));
                dropped.insert(e.id.clone());
            }
        }
    }

    let native: BTreeSet<&str> = api_key_defs().map(|d| d.id).collect();
    for (pi_id, src) in &plan.keys {
        let id = mu_provider_id(pi_id);
        if out.endpoints.iter().any(|(e, ..)| e == pi_id) {
            continue; // an endpoint's own login; its line says how the key is taken
        }
        if !native.contains(id) {
            skipped.push(format!(
                "skipped pi key for `{pi_id}`: mu holds keys only for {}; the gateway-pi plugin serves other providers",
                native.iter().copied().collect::<Vec<_>>().join(", ")
            ));
            continue;
        }
        if store.get(id).is_some_and(|e| e.origin().is_some()) && !force {
            skipped.push(format!("skipped pi key for `{id}`: mu already has one (`--force` replaces it)"));
            continue;
        }
        match entry(src) {
            Ok(entry) => {
                // An importer must not move `anthropic/*` or `openai/*` from a subscription on the gateway to
                // per-token billing: those keys stay web-search-only (or keep the choice a replaced key had);
                // Baseten's key is only ever for chat.
                let optional = native_def(id).is_some_and(|d| matches!(d.id, "anthropic" | "openai"));
                let chat = !optional || store.get(id).and_then(|e| e.chat) == Some(true);
                let entry = Entry { chat: Some(chat), ..entry };
                let note = match (optional, chat) {
                    (true, false) => format!(
                        "; not used for chat, so `{id}/*` stays where it is (`mu auth use {id} --chat` bills it per token)"
                    ),
                    (true, true) => "; its chat models use it, as the key it replaces did".to_string(),
                    _ => String::new(),
                };
                out.lines.push(format!("API key `{id}`: {}{note}", src.describe()));
                out.keys.push((id.to_string(), entry));
            }
            Err(err) => skipped.push(format!("skipped pi key for `{id}`: {err:#}")),
        }
    }

    let (models, notes) = plan.models_import(&dropped);
    skipped.extend(notes);
    if let Some(import) = models {
        if config::models_path().exists() && !force {
            skipped.push(format!(
                "skipped pi's default model and compaction thresholds: {} exists (`--force` replaces it)",
                config::models_path().display()
            ));
        } else {
            out.lines.extend(import.lines.iter().cloned());
            out.models = Some(import);
        }
    }
    if let Some((pi, mu)) = &plan.theme {
        if config::mu_config_sets("theme", "source") && !force {
            skipped.push(format!("skipped pi's theme `{pi}`: `[theme] source` is already set (`--force` replaces it)"));
        } else {
            out.lines.push(format!("theme: {mu} (pi: {pi})"));
            out.theme = Some(mu);
        }
    }
    if let Some(hide) = plan.hide_thinking {
        if config::mu_config_sets("display", "hide_thinking_block") && !force {
            skipped.push("skipped pi's hideThinkingBlock: `[display] hide_thinking_block` is already set (`--force` replaces it)".into());
        } else {
            out.lines.push(format!("[display] hide_thinking_block = {hide}"));
            out.hide = Some(hide);
        }
    }
    out.lines.extend(skipped);
    out
}

impl Resolved {
    /// Nothing would be written.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
            && self.keys.is_empty()
            && self.models.is_none()
            && self.theme.is_none()
            && self.hide.is_none()
    }

    /// Whether the import sets role `default`.
    pub fn has_default(&self) -> bool {
        self.models.as_ref().is_some_and(|m| m.config.sets.values().any(|s| s.contains_key("default")))
    }

    /// Writes everything, each item on its own: one that fails does not stop the others.
    pub fn apply(&self, cfg: &Configs, registry: &Registry, store: &Store, force: bool) -> Applied {
        let mut out = Applied::default();
        for (id, p, entry) in &self.endpoints {
            let r = custom::save(registry, store, id, p.clone(), Some(entry.clone()));
            out.record(match r {
                Ok(_) => Ok(format!("added custom endpoint `{id}` to {}", registry.path().display())),
                Err(e) => Err(e.to_string()),
            });
        }
        for (id, entry) in &self.keys {
            let r = store.set(id, entry.clone());
            out.record(
                r.map(|()| format!("saved the `{id}` key to {}", store.path().display()))
                    .map_err(|e| format!("key for `{id}`: {e:#}")),
            );
        }
        out.extend(cfg.write_pi_settings(self.models.as_ref(), self.theme, self.hide, force));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::pi_import::plan;
    use std::sync::Mutex;

    /// The tests that write `models.toml` and `config.toml` share one `MU_HOME`.
    static HOME: Mutex<()> = Mutex::new(());

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: mu_testutil::TempDir,
        pi: std::path::PathBuf,
        registry: Registry,
        store: Store,
    }

    fn env(files: &[(&str, &str)]) -> Env {
        let guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
        let home = mu_testutil::home();
        for f in ["models.toml", "config.toml"] {
            let _ = std::fs::remove_file(home.join(f));
        }
        let dir = mu_testutil::tmp();
        let pi = dir.join("pi");
        std::fs::create_dir_all(&pi).unwrap();
        for (name, text) in files {
            std::fs::write(pi.join(name), text).unwrap();
        }
        Env {
            registry: Registry::at(dir.join("providers.toml")),
            store: Store::at(dir.join("auth.json")),
            _guard: guard,
            pi,
            _dir: dir,
        }
    }

    const SETTINGS: &str = r#"{"defaultProvider":"anthropic","defaultModel":"claude-opus-5-5","defaultThinkingLevel":"high",
        "theme":"dark","hideThinkingBlock":true,
        "compaction":{"modelOverrides":{"vllm/a":{"reserveTokens":2000}}}}"#;
    const MODELS: &str = r#"{"providers":{
        "vllm":{"name":"vLLM","baseUrl":"https://v.example/v1","api":"openai-completions","apiKey":"$VLLM_KEY",
                "models":[{"id":"a","reasoning":true,"input":["text","image"],"contextWindow":10000,"maxTokens":2000,
                           "cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0.2}}]},
        "lit":{"baseUrl":"http://127.0.0.1:9000/v1","api":"openai-completions","apiKey":"sk-test-lit-1234","models":[{"id":"m"}]},
        "cmd":{"baseUrl":"https://c.example/v1","api":"openai-completions","apiKey":"!pass show c","models":[{"id":"m"}]},
        "bad":{"baseUrl":"http://remote.example/v1?token=sk-test-url-9","api":"openai-completions","apiKey":"k","models":[{"id":"m"}]},
        "openai":{"baseUrl":"https://o.example/v1","api":"openai-completions","apiKey":"k","models":[{"id":"m"}]}}}"#;
    const AUTH: &str = r#"{"anthropic":{"type":"api_key","key":"sk-test-ant-1234"},"openai":{"type":"api_key","key":"!op read k"},
        "baseten":{"type":"api_key","key":"sk-test-bt-1234"},"groq":{"type":"api_key","key":"sk-test-groq-1234"},
        "openai-codex":{"type":"oauth","access":"sk-test-oauth-1234"}}"#;

    fn secret_free(text: &str) {
        assert!(!text.contains("sk-test"), "{text}");
    }

    #[test]
    fn listing_has_every_mapping_and_skip_and_no_secret() {
        let e = env(&[("settings.json", SETTINGS), ("models.json", MODELS), ("auth.json", AUTH)]);
        let r = resolve(&plan(&e.pi), &e.registry, &e.store, false);
        let text = r.lines.join("\n");
        for want in [
            "set main: default = anthropic/claude-opus-5-5:high",
            "compaction threshold: vllm/a = f8000",
            "custom endpoint `vllm`: https://v.example/v1, 1 models, key: env VLLM_KEY",
            "custom endpoint `lit`: http://127.0.0.1:9000/v1, 1 models, key: stored key",
            "custom endpoint `cmd`: https://c.example/v1, 1 models, key: command pass",
            "API key `anthropic`: stored key; not used for chat, so `anthropic/*` stays where it is",
            "(`mu auth use openai --chat` bills it per token)",
            "API key `baseten`: stored key",
            "theme: tokyo-night (pi: dark)",
            "[display] hide_thinking_block = true",
            "skipped pi provider `bad`: ",
            "skipped pi provider `openai`: `openai` is a name mu already uses",
            "skipped pi key for `groq`: mu holds keys only for",
            "OAuth logins are never copied",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        secret_free(&text);
        assert!(!text.contains("token="), "{text}");
        assert!(!e.store.path().exists() && !e.registry.path().exists(), "resolve writes nothing");
    }

    #[test]
    fn apply_writes_mu_files_and_never_prints_a_key() {
        let e = env(&[("settings.json", SETTINGS), ("models.json", MODELS), ("auth.json", AUTH)]);
        let cfg = Configs::default();
        let r = resolve(&plan(&e.pi), &e.registry, &e.store, false);
        let done = r.apply(&cfg, &e.registry, &e.store, false);
        assert!(done.failed.is_empty(), "{:?}", done.failed);
        secret_free(&done.done.join("\n"));

        let defs = e.registry.snapshot().unwrap();
        assert_eq!(defs.keys().collect::<Vec<_>>(), ["cmd", "lit", "vllm"]);
        let v = &defs["vllm"];
        assert_eq!((v.label.as_deref(), v.discover, v.auth), (Some("vLLM"), false, AuthMode::Bearer));
        let m = &v.models["a"];
        assert_eq!(
            (m.context_window, m.max_tokens, m.images, m.reasoning),
            (Some(10000), Some(2000), true, Reasoning::Effort)
        );
        assert_eq!(m.efforts, default_efforts());
        assert_eq!(m.cost, Some(Cost { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: 0.2 }));
        assert_eq!(defs["lit"].models["m"].reasoning, Reasoning::None);

        let origin = |id: &str| format!("{:?}", e.store.get(id).unwrap().origin());
        assert_eq!(origin("vllm"), r#"Some(Env("VLLM_KEY"))"#);
        assert_eq!(origin("lit"), "Some(Stored)");
        assert!(origin("cmd").starts_with("Some(Cmd("));
        assert_eq!(e.store.get("lit").unwrap().key.as_deref(), Some("sk-test-lit-1234"));
        assert_eq!(e.store.get("anthropic").unwrap().key.as_deref(), Some("sk-test-ant-1234"));
        assert_eq!(e.store.get("anthropic").unwrap().chat, Some(false), "a pi key never moves chat routing");
        assert_eq!(e.store.get("openai").unwrap().chat, Some(false));
        assert!(origin("openai").starts_with("Some(Cmd("));
        assert_eq!(e.store.get("baseten").unwrap().chat, Some(true));
        assert!(!e.store.has("groq") && !e.store.has("openai-codex"));

        let (models, w) = mu_base::model_config::parse(&std::fs::read_to_string(config::models_path()).unwrap());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(models.sets["main"]["default"], "anthropic/claude-opus-5-5:high");
        assert_eq!(models.compaction.thresholds["vllm/a"], toml::Value::String("f8000".into()));
        let c = std::fs::read_to_string(mu_testutil::home().join("config.toml")).unwrap();
        assert!(c.contains("source = \"tokyo-night\"") && c.contains("hide_thinking_block = true"), "{c}");
    }

    #[test]
    fn existing_mu_state_is_kept_unless_forced() {
        let e = env(&[("settings.json", SETTINGS), ("models.json", MODELS), ("auth.json", AUTH)]);
        let cfg = Configs::default();
        resolve(&plan(&e.pi), &e.registry, &e.store, false).apply(&cfg, &e.registry, &e.store, false);
        e.store.set("anthropic", Entry::literal("sk-test-mine-1234")).unwrap();

        let again = resolve(&plan(&e.pi), &e.registry, &e.store, false);
        let text = again.lines.join("\n");
        assert!(again.is_empty(), "{text}");
        for want in [
            "`vllm`: a custom endpoint of that name exists",
            "`lit`: a custom endpoint of that name exists",
            "pi key for `anthropic`: mu already has one",
            "models.toml",
            "`[theme] source` is already set",
            "`[display] hide_thinking_block` is already set",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        secret_free(&text);

        let forced = resolve(&plan(&e.pi), &e.registry, &e.store, true);
        assert!(forced.keys.len() == 3 && forced.models.is_some() && forced.theme.is_some() && forced.hide.is_some());
        assert!(forced.endpoints.is_empty(), "endpoints are never overwritten, even with --force");
        let done = forced.apply(&cfg, &e.registry, &e.store, true);
        assert!(done.failed.is_empty(), "{:?}", done.failed);
        assert_eq!(e.store.get("anthropic").unwrap().key.as_deref(), Some("sk-test-ant-1234"));

        std::fs::remove_file(e.registry.path()).unwrap();
        let orphan = resolve(&plan(&e.pi), &e.registry, &e.store, false).lines.join("\n");
        assert!(orphan.contains("`lit`: a stored key for that name exists"), "{orphan}");
    }

    #[test]
    fn a_default_model_of_a_refused_endpoint_is_not_imported() {
        let e = env(&[("settings.json", r#"{"defaultProvider":"bad","defaultModel":"m"}"#), ("models.json", MODELS)]);
        let r = resolve(&plan(&e.pi), &e.registry, &e.store, false);
        assert!(r.models.is_none() && !r.has_default());
        let text = r.lines.join("\n");
        assert!(text.contains("skipped pi's default model") && text.contains("`bad` was not imported"), "{text}");
    }

    #[test]
    fn an_unreadable_registry_skips_endpoints_with_its_error() {
        let e = env(&[("models.json", MODELS)]);
        std::fs::write(e.registry.path(), "not = [toml").unwrap();
        let r = resolve(&plan(&e.pi), &e.registry, &e.store, false);
        assert!(r.endpoints.is_empty());
        assert!(
            r.lines.iter().any(|l| l.starts_with("skipped pi provider `vllm`: ") && l.contains("providers.toml")),
            "{:?}",
            r.lines
        );
    }

    #[test]
    fn nothing_to_write_is_empty() {
        let e = env(&[]);
        assert!(resolve(&plan(&e.pi), &e.registry, &e.store, false).is_empty());
    }
}
