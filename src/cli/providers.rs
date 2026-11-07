//! `mu providers`: custom OpenAI-compatible endpoints (`providers.toml`) and their setup, without the
//! TUI. Each command takes its registry and key store explicitly, so the tests use their own files.

use super::approved;
use super::auth::read_secret;
use anyhow::{Context, Result, anyhow, bail};
use mu_provider::auth::check::{self, Check};
use mu_provider::auth::store::{Entry, Store, resolve_entry};
use mu_provider::model::discovery::{self, CustomRow};
use mu_provider::provider::custom::{self, AuthMode, CustomModel, CustomProvider, Reasoning, Registry, SaveError};
use serde_json::Map;
use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};

/// `Add` carries every `--model` setting and is parsed once per run, so it is not worth boxing.
#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
pub enum Cmd {
    /// List the custom endpoints: base URL, credential state, discovery and manual models. Never shows a key.
    List,
    /// Add a custom OpenAI-compatible endpoint. Its key is read from stdin (a no-echo prompt on a
    /// terminal), or referenced with --env or --cmd; --no-auth for an endpoint that takes none.
    /// Each --model reasons with an effort ladder (low, medium, high) unless --reasoning or
    /// --effort say otherwise.
    Add {
        /// The provider id: models are then selected as `<id>/<model id>`.
        id: String,
        /// The endpoint's base URL, including any `/v1` it serves under; `/models` and `/chat/completions` are added.
        #[arg(long)]
        base_url: String,
        /// The name shown in pickers (default: the id).
        #[arg(long)]
        label: Option<String>,
        /// A model the endpoint serves, by its exact id. Repeat for each one.
        #[arg(long = "model", value_name = "ID")]
        models: Vec<String>,
        /// Context window of the --model ids.
        #[arg(long)]
        context_window: Option<u32>,
        /// Output limit of the --model ids.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// How the --model ids reason: none, effort (sends reasoning_effort) or template (request
        /// switches). Default effort, with low, medium and high.
        #[arg(long, value_name = "MODE")]
        reasoning: Option<String>,
        /// An effort level the --model ids take: minimal, low, medium, high, xhigh or max. Repeat,
        /// lowest first. Default low, medium and high; effort only.
        #[arg(long = "effort", value_name = "LEVEL")]
        efforts: Vec<String>,
        /// A request switch while thinking is on, as `path = value` (with reasoning = template).
        /// Repeat for each one.
        #[arg(long = "thinking-on", value_name = "PATH=VALUE")]
        thinking_on: Vec<String>,
        /// A request switch while thinking is off, as `path = value` (with reasoning = template).
        /// Repeat for each one.
        #[arg(long = "thinking-off", value_name = "PATH=VALUE")]
        thinking_off: Vec<String>,
        /// Read the key from this environment variable at request time.
        #[arg(long, conflicts_with_all = ["cmd", "no_auth"])]
        env: Option<String>,
        /// Run this shell command at request time; its output is the key.
        #[arg(long, conflicts_with = "no_auth")]
        cmd: Option<String>,
        /// The endpoint takes no key.
        #[arg(long)]
        no_auth: bool,
        /// Do not list the endpoint's models from `/models`; then --model is required.
        #[arg(long)]
        no_discover: bool,
        /// Save without checking the endpoint or the key first.
        #[arg(long)]
        no_validate: bool,
    },
    /// Print an endpoint's models: the --model ids and its last `/models` listing.
    Models {
        id: String,
        /// Fetch `/models` now; a failure is an error, not the old listing.
        #[arg(long)]
        refresh: bool,
    },
    /// Remove a custom endpoint and its stored key. Roles and the current model are not changed.
    Rm {
        id: String,
        /// Remove without asking.
        #[arg(long)]
        yes: bool,
    },
}

/// Where a new endpoint's key comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Read from stdin when `add` gets to it, after every check that needs no key.
    Prompt,
    Env(String),
    Cmd(String),
    Keyless,
}

/// What `mu providers add` was asked for.
#[derive(Debug, Clone)]
pub struct AddArgs {
    pub id: String,
    pub base_url: String,
    pub label: Option<String>,
    pub models: Vec<String>,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
    pub reasoning: Option<String>,
    pub efforts: Vec<String>,
    pub thinking_on: Vec<String>,
    pub thinking_off: Vec<String>,
    pub source: Source,
    pub no_discover: bool,
    pub no_validate: bool,
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let tty = std::io::stdin().is_terminal();
    let ask = || {
        if !tty {
            return None;
        }
        eprint!("Remove? [y/N] ");
        std::io::stderr().flush().ok()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok()?;
        Some(s)
    };
    exec(&Registry::new(), &Store::new(), cmd, &mut std::io::stdout(), &ask)
}

fn exec(
    registry: &Registry,
    store: &Store,
    command: Cmd,
    out: &mut impl Write,
    ask: &dyn Fn() -> Option<String>,
) -> Result<i32> {
    match command {
        Cmd::List => list(registry, store, out)?,
        Cmd::Add {
            id,
            base_url,
            label,
            models,
            context_window,
            max_tokens,
            reasoning,
            efforts,
            thinking_on,
            thinking_off,
            env,
            cmd,
            no_auth,
            no_discover,
            no_validate,
        } => {
            let source = match (env, cmd, no_auth) {
                (Some(name), _, _) => Source::Env(name),
                (_, Some(command), _) => Source::Cmd(command),
                (_, _, true) => Source::Keyless,
                _ => Source::Prompt,
            };
            let args = AddArgs {
                id,
                base_url,
                label,
                models,
                context_window,
                max_tokens,
                reasoning,
                efforts,
                thinking_on,
                thinking_off,
                source,
                no_discover,
                no_validate,
            };
            let prompt = format!("API key for {}", args.id);
            add(registry, store, &args, &|| read_secret("", &prompt), out)?
        }
        Cmd::Models { id, refresh } => models(registry, store, &id, refresh, out)?,
        Cmd::Rm { id, yes } => rm(registry, store, &id, yes, out, ask)?,
    }
    Ok(0)
}

/// The definition `add` would write, before any check.
fn draft(args: &AddArgs) -> Result<CustomProvider> {
    let mut p = CustomProvider::new(&args.base_url);
    p.label = args.label.clone();
    p.auth = if args.source == Source::Keyless { AuthMode::None } else { AuthMode::Bearer };
    p.discover = !args.no_discover;
    if args.models.is_empty() {
        if args.context_window.is_some()
            || args.max_tokens.is_some()
            || args.reasoning.is_some()
            || !args.efforts.is_empty()
            || !args.thinking_on.is_empty()
            || !args.thinking_off.is_empty()
        {
            bail!(
                "--context-window, --max-tokens, --reasoning, --effort and --thinking-on/--thinking-off \
                 apply to the --model ids; give at least one --model"
            );
        }
        if args.no_discover {
            bail!("--no-discover needs at least one --model: without discovery, the models are the ones you list");
        }
    }
    for m in &args.models {
        if m.trim().is_empty() {
            bail!("--model needs a model id");
        }
        let model = model_settings(args)?;
        if p.models.insert(m.clone(), model).is_some() {
            bail!("--model {m} is given more than once");
        }
    }
    Ok(p)
}

/// The limits and reasoning `--model` ids get. A model nobody says otherwise about reasons with an
/// effort ladder, since a level only reaches the server while thinking is on.
fn model_settings(args: &AddArgs) -> Result<CustomModel> {
    let reasoning = match args.reasoning.as_deref() {
        None => Reasoning::Effort,
        Some(mode) => Reasoning::parse(mode)
            .ok_or_else(|| anyhow!("--reasoning must be one of {}", Reasoning::NAMES.join(", ")))?,
    };
    let mut on = Map::new();
    for entry in &args.thinking_on {
        custom::set_switch(&mut on, entry).map_err(|e| anyhow!("--thinking-on: {e}"))?;
    }
    let mut off = Map::new();
    for entry in &args.thinking_off {
        custom::set_switch(&mut off, entry).map_err(|e| anyhow!("--thinking-off: {e}"))?;
    }
    let efforts = match (reasoning, args.efforts.len()) {
        (Reasoning::Effort, 0) => custom::default_efforts(),
        (Reasoning::Effort, _) => args.efforts.clone(),
        (_, 0) => Vec::new(),
        (mode, _) => bail!("--effort only applies to --reasoning effort, not {}", mode.as_str()),
    };
    if reasoning == Reasoning::Template && (on.is_empty() || off.is_empty()) {
        bail!(
            "--reasoning template needs --thinking-on and --thinking-off, e.g. \
             --thinking-on 'chat_template_kwargs.enable_thinking = true' \
             --thinking-off 'chat_template_kwargs.enable_thinking = false'"
        );
    }
    if reasoning == Reasoning::None && (!on.is_empty() || !off.is_empty()) {
        bail!("--thinking-on and --thinking-off need --reasoning effort or template, not none");
    }
    Ok(CustomModel {
        context_window: args.context_window,
        max_tokens: args.max_tokens,
        reasoning,
        efforts,
        extra_body_on: on,
        extra_body_off: off,
        ..Default::default()
    })
}

/// `mu providers add`: checks the endpoint and the key, then saves the definition and the key. A
/// check that fails saves nothing; a key that cannot be saved leaves the definition, as `custom::save`
/// reports. Warnings and notices go to `out`.
fn add(
    registry: &Registry,
    store: &Store,
    args: &AddArgs,
    prompt: &dyn Fn() -> Result<String>,
    out: &mut impl Write,
) -> Result<()> {
    let provider = draft(args)?.validated(&args.id)?;
    if registry.get(&args.id)?.is_some() {
        bail!(
            "custom provider `{}` already exists in {}; edit it there, or remove it with `mu providers rm {}`",
            args.id,
            registry.path().display(),
            args.id
        );
    }
    let keyless = args.source == Source::Keyless;
    if !keyless && store.has(&args.id) {
        bail!(
            "a stored credential for `{id}` already exists; check it with `mu auth test {id}`, or remove it with `mu auth rm {id}` first",
            id = args.id
        );
    }
    let entry = match &args.source {
        Source::Prompt => {
            let key = prompt()?;
            if key.trim().is_empty() {
                bail!("no key given; nothing saved");
            }
            Some(Entry::literal(&key))
        }
        Source::Env(name) => Some(Entry::reference(&format!("${name}"))?),
        Source::Cmd(command) => Some(Entry::reference(&format!("!{command}"))?),
        Source::Keyless => None,
    };
    let runtime = mu_base::runtime::current_thread_runtime()?;
    let key = match &entry {
        Some(e) if !args.no_validate => Some(
            runtime
                .block_on(resolve_entry(&args.id, e))
                .map_err(|e| anyhow!("{e:#}; nothing saved (--no-validate saves anyway)"))?
                .key,
        ),
        _ => None,
    };
    let mut listed = None;
    if !args.no_validate {
        match runtime.block_on(check::validate_custom(&provider, key.as_deref())) {
            Check::Valid | Check::Unchecked => {}
            Check::Rejected(m) => bail!("{m}; nothing saved"),
            Check::Unreachable(m) => {
                bail!("could not check the endpoint: {m}; nothing saved (--no-validate saves anyway)")
            }
        }
        if provider.discover {
            match runtime.block_on(discovery::custom_fetch(&provider, key.as_deref())) {
                Ok(rows) => listed = Some(rows),
                Err(e) if args.models.is_empty() => bail!(
                    "could not list the models at {}: {e}; nothing saved (pass --model for each model, or --no-validate to save without a list)",
                    provider.models_url()
                ),
                Err(e) => writeln!(
                    out,
                    "warning: could not list the models at {}: {e}; saved with the --model ids",
                    provider.models_url()
                )?,
            }
        }
    }
    let listed_count = listed.as_ref().map(Vec::len);
    let key_err = match custom::save(registry, store, &args.id, provider.clone(), entry) {
        Ok(_) => None,
        Err(e @ SaveError::KeyNotSaved { .. }) => Some(e),
        Err(e) => return Err(e.into()),
    };
    // The definition is there now, so a listing fetched while checking is kept for it.
    if let Some(rows) = listed {
        discovery::custom_commit(&provider, rows);
    }
    if let Some(e) = key_err {
        return Err(e.into());
    }
    writeln!(out, "added custom provider `{}` to {}", args.id, registry.path().display())?;
    if keyless {
        writeln!(out, "`{}` sends no key (auth = \"none\")", args.id)?;
    } else {
        writeln!(out, "saved the key for `{}` to {}", args.id, store.path().display())?;
    }
    if let Some(n) = listed_count {
        writeln!(out, "listed {n} models from {}", provider.models_url())?;
    }
    if listed_count.is_none() && provider.models.is_empty() {
        writeln!(
            out,
            "notice: no model is listed for `{id}`; select one with /switch {id}/<exact model id>, spelled exactly as the server names it",
            id = args.id
        )?;
    }
    writeln!(out, "use /switch {}/<model> to try it, or /model to save a role; mu changed no model", args.id)?;
    Ok(())
}

fn list(registry: &Registry, store: &Store, out: &mut impl Write) -> Result<()> {
    let defs = registry.snapshot()?;
    if defs.is_empty() {
        writeln!(out, "no custom providers; add one with `mu providers add <id> --base-url <url>`")?;
        return Ok(());
    }
    let w = defs.keys().map(String::len).max().unwrap_or(0);
    for (id, c) in &defs {
        let credential = match c.auth {
            AuthMode::None => "no auth".to_string(),
            AuthMode::Bearer => format!("bearer, {}", c.key_state(store).word()),
        };
        let discovery = if c.discover { "discovery on" } else { "discovery off" };
        writeln!(
            out,
            "{id:<w$}  {}  {}  {credential}  {discovery}  {} manual models",
            c.label(),
            c.base_url,
            c.models.len()
        )?;
    }
    Ok(())
}

fn definition(registry: &Registry, id: &str) -> Result<CustomProvider> {
    registry.get(id)?.ok_or_else(|| {
        anyhow!(
            "no custom provider `{id}` in {}; `mu providers list` shows the configured ones",
            registry.path().display()
        )
    })
}

fn models(registry: &Registry, store: &Store, id: &str, refresh: bool, out: &mut impl Write) -> Result<()> {
    let c = definition(registry, id)?;
    if refresh {
        let n = mu_base::runtime::current_thread_runtime()?
            .block_on(c.refresh_models(store))
            .with_context(|| format!("refreshing the models of `{id}`"))?;
        writeln!(out, "listed {n} models from {}", c.models_url())?;
    }
    let rows = model_rows(&c, &discovery::custom_cached(&c));
    if rows.is_empty() {
        writeln!(
            out,
            "no models for `{id}`: add --model ids to providers.toml, or run `mu providers models {id} --refresh`"
        )?;
    }
    for row in rows {
        writeln!(out, "{row}")?;
    }
    Ok(())
}

/// The models an endpoint offers, once each: its configured ids and its cached listing, by id.
fn model_rows(c: &CustomProvider, listed: &[CustomRow]) -> Vec<String> {
    let manual: BTreeSet<&str> = c.models.keys().map(String::as_str).collect();
    let found: BTreeSet<&str> = listed.iter().map(|r| r.id.as_str()).collect();
    manual
        .union(&found)
        .map(|m| {
            let source = match (manual.contains(m), found.contains(m)) {
                (true, true) => "manual, listed",
                (true, false) => "manual",
                _ => "listed",
            };
            let context = c
                .models
                .get(*m)
                .and_then(|x| x.context_window)
                .or_else(|| listed.iter().find(|r| r.id == *m).and_then(|r| r.context_length));
            let context = context.map_or(String::new(), |n| format!("  context {n}"));
            let reasoning = c.models.get(*m).map_or(String::new(), |x| match x.reasoning {
                Reasoning::None => String::new(),
                Reasoning::Effort => format!("  reasoning effort {}", x.efforts.join("/")),
                Reasoning::Template => "  reasoning by request switches".to_string(),
            });
            format!("{}/{m}  {source}{context}{reasoning}", c.id)
        })
        .collect()
}

/// `mu providers rm`: removes the definition, then its stored key. Roles and the current model stay.
fn rm(
    registry: &Registry,
    store: &Store,
    id: &str,
    yes: bool,
    out: &mut impl Write,
    ask: &dyn Fn() -> Option<String>,
) -> Result<()> {
    let Some(c) = registry.get(id)? else {
        let left = if store.has(id) {
            format!("; a stored key is left for it: remove it with `mu auth rm {id}`")
        } else {
            String::new()
        };
        bail!("no custom provider `{id}` in {}{left}", registry.path().display());
    };
    writeln!(
        out,
        "This removes the custom provider `{id}` ({}) from {} and its stored key. Roles and the current model are not changed; a role naming `{id}/…` fails until you change it.",
        c.base_url,
        registry.path().display()
    )?;
    out.flush()?;
    approved(yes, if yes { None } else { ask() })?;
    if !registry.remove(id)? {
        bail!("no custom provider `{id}` in {}", registry.path().display());
    }
    writeln!(out, "removed the custom provider `{id}`")?;
    match store.remove(id) {
        Ok(true) => writeln!(out, "removed its stored key")?,
        Ok(false) => {}
        Err(e) => bail!(
            "removed the custom provider `{id}`, but its stored key remains ({e:#}): remove it with `mu auth rm {id}`"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use mu_testutil::{Reply, fake_server};
    use std::path::{Path, PathBuf};

    /// A registry and store in a fresh temp dir, so no test reads or writes the profile.
    fn files(tag: &str) -> (Registry, Store, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-cli-providers-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        (Registry::at(d.join("providers.toml")), Store::at(d.join("auth.json")), d)
    }

    /// A fake endpoint that answers from a runtime of its own, so the blocking commands can call it.
    fn server(replies: Vec<Reply>) -> (tokio::runtime::Runtime, String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (base, seen) = rt.block_on(fake_server(replies));
        (rt, base, seen)
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    fn refuse_prompt() -> Result<String> {
        Err(anyhow!("no prompt expected"))
    }

    fn args(id: &str, base: &str, source: Source) -> AddArgs {
        AddArgs {
            id: id.into(),
            base_url: base.into(),
            label: None,
            models: Vec::new(),
            context_window: None,
            max_tokens: None,
            reasoning: None,
            efforts: Vec::new(),
            thinking_on: Vec::new(),
            thinking_off: Vec::new(),
            source,
            no_discover: false,
            no_validate: false,
        }
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: Cmd,
    }

    fn parse(args: &[&str]) -> std::result::Result<Cmd, clap::Error> {
        Wrap::try_parse_from(std::iter::once("mu").chain(args.iter().copied())).map(|w| w.cmd)
    }

    #[test]
    fn the_key_sources_exclude_each_other() {
        let base = ["add", "modal", "--base-url", "https://x.test/v1"];
        let with = |extra: &[&str]| {
            let mut a = base.to_vec();
            a.extend_from_slice(extra);
            parse(&a)
        };
        assert!(with(&["--env", "K"]).is_ok());
        assert!(with(&["--cmd", "pass show k"]).is_ok());
        assert!(with(&["--no-auth"]).is_ok());
        assert!(with(&["--env", "K", "--cmd", "x"]).is_err());
        assert!(with(&["--env", "K", "--no-auth"]).is_err());
        assert!(with(&["--cmd", "x", "--no-auth"]).is_err());
        assert!(parse(&["add", "modal"]).is_err(), "--base-url is required");
        assert!(
            with(&[
                "--model",
                "a",
                "--context-window",
                "8192",
                "--max-tokens",
                "1024",
                "--no-discover",
                "--no-validate"
            ])
            .is_ok()
        );
    }

    #[test]
    fn limits_and_discovery_need_manual_models() {
        let mut a = args("modal", "https://x.test/v1", Source::Keyless);
        a.context_window = Some(8192);
        assert!(draft(&a).unwrap_err().to_string().contains("--model"));
        let mut a = args("modal", "https://x.test/v1", Source::Keyless);
        a.no_discover = true;
        assert!(draft(&a).unwrap_err().to_string().contains("--no-discover needs"));
        let mut a = args("modal", "https://x.test/v1", Source::Keyless);
        a.models = vec!["m".into(), "m".into()];
        assert!(draft(&a).unwrap_err().to_string().contains("more than once"));
        let mut a = args("modal", "https://x.test/v1", Source::Keyless);
        a.models = vec!["m".into()];
        a.context_window = Some(100);
        a.max_tokens = Some(200);
        let e = draft(&a).unwrap().validated("modal").unwrap_err();
        assert!(format!("{e:#}").contains("exceeds"), "{e:#}");
    }

    #[test]
    fn models_get_an_effort_ladder_by_default_and_the_flags_can_change_it() {
        let mut a = args("modal", "https://x.test/v1", Source::Keyless);
        a.models = vec!["org/m".into()];
        let saved = |a: &AddArgs| draft(a).unwrap().validated("modal");
        let p = saved(&a).unwrap();
        assert_eq!(p.models["org/m"].reasoning, Reasoning::Effort);
        assert_eq!(p.models["org/m"].efforts, ["low", "medium", "high"], "a new model reasons by default");

        // A chosen ladder; the whole definition still refuses one out of order or unknown.
        a.efforts = vec!["minimal".into(), "high".into()];
        assert_eq!(saved(&a).unwrap().models["org/m"].efforts, ["minimal", "high"]);
        a.efforts = vec!["high".into(), "low".into()];
        assert!(format!("{:#}", saved(&a).unwrap_err()).contains("ascending"));
        a.efforts = vec!["huge".into()];
        assert!(format!("{:#}", saved(&a).unwrap_err()).contains("unknown effort"));

        // None sends nothing at all.
        a.reasoning = Some("none".into());
        a.efforts.clear();
        let p = saved(&a).unwrap();
        assert_eq!(p.models["org/m"].reasoning, Reasoning::None);
        assert!(p.models["org/m"].efforts.is_empty());

        // Template takes its switches as `path = value`, one flag each, and needs both sides.
        a.reasoning = Some("template".into());
        a.thinking_on = vec!["chat_template_kwargs.enable_thinking = true".into()];
        let e = draft(&a).unwrap_err().to_string();
        assert!(e.contains("--thinking-off"), "{e}");
        a.thinking_off = vec!["chat_template_kwargs.enable_thinking = false".into()];
        let p = saved(&a).unwrap();
        let m = &p.models["org/m"];
        assert_eq!(m.reasoning, Reasoning::Template);
        assert_eq!(m.extra_body_on["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(custom::switches_text(&m.extra_body_off), "chat_template_kwargs.enable_thinking = false");

        // Effort keeps a switch too, and the flags that need a model without one say so.
        a.reasoning = Some("effort".into());
        assert!(saved(&a).is_ok(), "effort with request switches is valid");
        a.reasoning = Some("none".into());
        a.efforts = vec!["low".into()];
        assert!(draft(&a).unwrap_err().to_string().contains("--effort only applies"));
        a.reasoning = Some("maybe".into());
        assert!(draft(&a).unwrap_err().to_string().contains("none, effort, template"));
        a.reasoning = None;
        a.thinking_on.clear();
        a.thinking_off.clear();
        a.models.clear();
        assert!(draft(&a).unwrap_err().to_string().contains("apply to the --model ids"));
    }

    #[test]
    fn discovery_uses_the_key_and_the_file_holds_no_token() {
        mu_testutil::home();
        let (reg, store, d) = files("discover");
        let token = "pv-token-0123456789";
        let (_rt, base, seen) = server(vec![Reply::new(200, r#"{"data":[{"id":"org/model","context_length":8192}]}"#)]);
        let mut a = args("modal", &format!("{base}/v1"), Source::Prompt);
        a.label = Some("Modal".into());
        let mut out = Vec::new();
        add(&reg, &store, &a, &|| Ok(token.into()), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("listed 1 models from"), "{text}");
        // Validation and discovery each asked `/v1/models` with the bearer key.
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "{reqs:?}");
        for r in &reqs {
            assert!(r.starts_with("GET /v1/models "), "{r}");
            assert!(r.to_lowercase().contains(&format!("authorization: bearer {token}")), "{r}");
        }
        let toml = read(reg.path());
        assert!(!toml.contains(token), "{toml}");
        assert_eq!(store.get("modal").unwrap().key.as_deref(), Some(token));
        let def = reg.get("modal").unwrap().unwrap();
        assert_eq!(def.label(), "Modal");
        assert!(discovery::custom_cached(&def).iter().any(|r| r.id == "org/model"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_rejected_key_saves_nothing() {
        let (reg, store, d) = files("reject");
        let (_rt, base, _seen) = server(vec![Reply::new(401, "no")]);
        let a = args("modal", &format!("{base}/v1"), Source::Prompt);
        let e = add(&reg, &store, &a, &|| Ok("pv-bad-0123456789".into()), &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("rejected") && e.contains("nothing saved"), "{e}");
        assert!(!reg.path().exists() && !store.path().exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_unreachable_endpoint_needs_the_explicit_flag() {
        let (reg, store, d) = files("unreachable");
        let (_rt, base, _seen) = server(vec![Reply::new(503, "down")]);
        let a = args("modal", &format!("{base}/v1"), Source::Keyless);
        let e = add(&reg, &store, &a, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("--no-validate") && e.contains("nothing saved"), "{e}");
        assert!(!reg.path().exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_failed_listing_is_a_warning_with_manual_models_and_an_error_without() {
        let (reg, store, d) = files("listing");
        // `/models` answers, but lists nothing usable.
        let (_rt, base, _seen) = server(vec![Reply::new(200, r#"{"data":[]}"#)]);
        let mut a = args("modal", &format!("{base}/v1"), Source::Keyless);
        let e = add(&reg, &store, &a, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("could not list the models") && e.contains("nothing saved"), "{e}");
        assert!(!reg.path().exists());
        a.models = vec!["org/x".into()];
        let mut out = Vec::new();
        add(&reg, &store, &a, &refuse_prompt, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("warning: could not list the models"));
        assert!(reg.get("modal").unwrap().unwrap().models.contains_key("org/x"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_collision_refuses_before_either_file_changes() {
        let (reg, store, d) = files("collide");
        reg.add("modal", CustomProvider::new("https://a.test/v1")).unwrap();
        let before = read(reg.path());
        let a = args("modal", "https://b.test/v1", Source::Env("K".into()));
        let mut a_novalidate = a.clone();
        a_novalidate.no_validate = true;
        let e = add(&reg, &store, &a_novalidate, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("already exists") && e.contains("mu providers rm modal"), "{e}");
        assert_eq!(read(reg.path()), before);
        assert!(!store.path().exists());

        store.set("fresh", Entry::literal("fr-test-0123456789")).unwrap();
        let before_store = read(store.path());
        let a = AddArgs { no_validate: true, ..args("fresh", "https://c.test/v1", Source::Env("K".into())) };
        let e = add(&reg, &store, &a, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("already exists") && e.contains("mu auth rm fresh"), "{e}");
        assert_eq!(read(store.path()), before_store);
        assert!(reg.get("fresh").unwrap().is_none());

        let a = AddArgs { no_validate: true, ..args("anthropic", "https://c.test/v1", Source::Env("K".into())) };
        let e = add(&reg, &store, &a, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("already uses"), "{e}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_failed_key_write_leaves_the_definition_and_says_how_to_recover() {
        let (reg, _store, d) = files("write-fail");
        let blocked = Store::at(d.join("blocked").join("auth.json"));
        std::fs::create_dir_all(d.join("blocked").join("auth.json")).unwrap();
        let a = AddArgs { no_validate: true, ..args("fresh", "https://c.test/v1", Source::Env("K".into())) };
        let e = add(&reg, &blocked, &a, &refuse_prompt, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("saved, but its credential was not saved") && e.contains("mu auth add fresh"), "{e}");
        assert!(reg.get("fresh").unwrap().is_some(), "the definition stays");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn without_models_or_a_listing_the_notice_names_the_exact_id() {
        let (reg, store, d) = files("nolist");
        let a = AddArgs { no_validate: true, ..args("modal", "https://x.test/v1", Source::Keyless) };
        let mut out = Vec::new();
        add(&reg, &store, &a, &refuse_prompt, &mut out).unwrap();
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("/switch modal/<exact model id>"), "{t}");
        assert!(t.contains("sends no key"), "{t}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rm_needs_confirmation_and_leaves_the_others_alone() {
        let (reg, store, d) = files("rm");
        reg.add("modal", CustomProvider::new("https://a.test/v1")).unwrap();
        reg.add("other", CustomProvider::new("https://b.test/v1")).unwrap();
        store.set("modal", Entry::literal("md-test-0123456789")).unwrap();
        store.set("other", Entry::literal("ot-test-0123456789")).unwrap();
        let rm = |id: &str, yes| Cmd::Rm { id: id.into(), yes };
        let e = exec(&reg, &store, rm("modal", false), &mut Vec::new(), &|| None).unwrap_err().to_string();
        assert!(e.starts_with("approval needed"), "{e}");
        assert!(reg.get("modal").unwrap().is_some(), "nothing is removed without approval");
        let mut out = Vec::new();
        assert_eq!(exec(&reg, &store, rm("modal", false), &mut out, &|| Some("y".into())).unwrap(), 0);
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("Roles and the current model are not changed"), "{t}");
        assert!(reg.get("modal").unwrap().is_none() && !store.has("modal"));
        assert!(store.has("other") && reg.get("other").unwrap().is_some());
        let e = exec(&reg, &store, rm("modal", true), &mut Vec::new(), &|| None).unwrap_err().to_string();
        assert!(e.contains("no custom provider `modal`"), "{e}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_failed_key_removal_names_the_recovery() {
        let (reg, _store, d) = files("rm-fail");
        reg.add("modal", CustomProvider::new("https://a.test/v1")).unwrap();
        let blocked = Store::at(d.join("blocked").join("auth.json"));
        std::fs::create_dir_all(d.join("blocked").join("auth.json")).unwrap();
        let cmd = Cmd::Rm { id: "modal".into(), yes: true };
        let e = exec(&reg, &blocked, cmd, &mut Vec::new(), &|| None).unwrap_err().to_string();
        assert!(e.contains("removed the custom provider `modal`") && e.contains("mu auth rm modal"), "{e}");
        assert!(reg.get("modal").unwrap().is_none(), "the definition is gone");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn list_shows_the_credential_state_but_never_a_key() {
        let (reg, store, d) = files("list");
        let mut c = CustomProvider::new("https://a.test/v1");
        c.label = Some("Modal".into());
        c.models.insert("org/m".into(), CustomModel::default());
        reg.add("modal", c).unwrap();
        store.set("modal", Entry::literal("md-secret-0123456789")).unwrap();
        let mut out = Vec::new();
        exec(&reg, &store, Cmd::List, &mut out, &|| None).unwrap();
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("modal") && t.contains("Modal") && t.contains("bearer, key"), "{t}");
        assert!(t.contains("1 manual models") && t.contains("discovery on"), "{t}");
        assert!(!t.contains("secret") && !t.contains("md-"), "{t}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn models_merge_the_configured_and_the_listed_ids_once_each() {
        let mut c = CustomProvider::new("https://x.test/v1").validated("modal").unwrap();
        c.models.insert("org/b".into(), CustomModel { context_window: Some(4096), ..Default::default() });
        let listed = vec![
            CustomRow { id: "org/a".into(), context_length: Some(8192), max_completion_tokens: None },
            CustomRow { id: "org/b".into(), context_length: None, max_completion_tokens: None },
        ];
        assert_eq!(
            model_rows(&c, &listed),
            ["modal/org/a  listed  context 8192", "modal/org/b  manual, listed  context 4096"]
        );
    }
}
