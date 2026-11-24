//! `mu models`: copy omp's or pi's model setup into mu's own files (`models.toml`, and for pi also
//! custom endpoints, keys, theme and display settings). Neither tool's files are written.

use super::approved;
use anyhow::{Result, anyhow, bail};
use mu_base::config::{self, Configs};
use mu_base::model_config::Import;
use mu_base::pi_import::PiPlan;
use mu_provider::auth::store::Store;
use mu_provider::pi_import;
use mu_provider::provider::custom::Registry;
use std::io::{IsTerminal, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Source {
    Omp,
    Pi,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Copy omp's or pi's model setup into mu's own files, after listing what will be copied and
    /// what will not. From omp: modelRoles as set `main`, modelPresets as sets, cycleOrder,
    /// models.yml limits, compaction thresholds. From pi: the default model and thinking level as
    /// role `default`, models.json openai-completions providers as custom endpoints, API keys of
    /// anthropic, openai and baseten (never OAuth logins), the theme and hideThinkingBlock.
    Import {
        /// Which tool to copy from; by default the one that has something (asks when both do).
        #[arg(long, value_enum)]
        from: Option<Source>,
        /// Copy without asking.
        #[arg(long)]
        yes: bool,
        /// Replace an existing models.toml, stored key, theme or display setting. A custom
        /// endpoint is never replaced.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let tty = std::io::stdin().is_terminal();
    let read = |prompt: &str| {
        if !tty {
            return None;
        }
        eprint!("{prompt}");
        std::io::stderr().flush().ok()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok()?;
        Some(s)
    };
    match cmd {
        Cmd::Import { from, yes, force } => {
            let (omp, pi) = (config::omp_import(), config::pi_import());
            let source = choose(from, omp.is_some(), pi.is_some(), &|| read("Import from omp or pi? [omp/pi] "))?;
            let ask = || read("Import? [y/N] ");
            let out = &mut std::io::stdout();
            match source {
                Source::Omp => exec(&Configs::default(), omp, yes, force, out, &ask),
                Source::Pi => exec_pi(&Configs::default(), pi, &Registry::new(), &Store::new(), yes, force, out, &ask),
            }
        }
    }
}

/// `--from`, or the only tool with something to copy; with both, the answer to `ask`.
fn choose(from: Option<Source>, omp: bool, pi: bool, ask: &dyn Fn() -> Option<String>) -> Result<Source> {
    if let Some(s) = from {
        return Ok(s);
    }
    match (omp, pi) {
        (true, false) => Ok(Source::Omp),
        (false, true) => Ok(Source::Pi),
        (false, false) => bail!(
            "nothing to import: neither omp ({}) nor pi ({}) has setup mu can use",
            mu_common::paths::omp_agent_dir().display(),
            mu_common::paths::pi_agent_dir().display()
        ),
        (true, true) => match ask().as_deref().map(|a| a.trim().to_ascii_lowercase()).as_deref() {
            Some("omp") => Ok(Source::Omp),
            Some("pi") => Ok(Source::Pi),
            _ => bail!("both omp and pi have setup to import: pass --from omp or --from pi"),
        },
    }
}

/// pi: lists what would be copied and skipped, asks, then writes each part on its own.
#[allow(clippy::too_many_arguments)]
fn exec_pi(
    cfg: &Configs,
    plan: Option<PiPlan>,
    registry: &Registry,
    store: &Store,
    yes: bool,
    force: bool,
    out: &mut impl Write,
    ask: &dyn Fn() -> Option<String>,
) -> Result<i32> {
    let plan = plan.ok_or_else(|| anyhow!(config::nothing_to_import_from_pi()))?;
    let resolved = pi_import::resolve(&plan, registry, store, force);
    writeln!(out, "Copy from pi into mu:")?;
    for l in &resolved.lines {
        writeln!(out, "  {l}")?;
    }
    out.flush()?;
    if resolved.is_empty() {
        bail!("nothing to import: everything pi has is already in mu or cannot be copied (see above)");
    }
    approved(yes, if yes { None } else { ask() })?;
    let done = resolved.apply(cfg, registry, store, force);
    for l in &done.done {
        writeln!(out, "  {l}")?;
    }
    if !done.failed.is_empty() {
        bail!("imported only part of pi's setup; not written: {}", done.failed.join("; "));
    }
    writeln!(out, "imported pi's setup")?;
    Ok(0)
}

fn exec(
    cfg: &Configs,
    import: Option<Import>,
    yes: bool,
    force: bool,
    out: &mut impl Write,
    ask: &dyn Fn() -> Option<String>,
) -> Result<i32> {
    let import = import.ok_or_else(|| anyhow!(config::nothing_to_import()))?;
    config::check_import_target(force).map_err(|e| anyhow!(e))?;
    writeln!(out, "Copy into {}:", config::models_path().display())?;
    for l in &import.lines {
        writeln!(out, "  {l}")?;
    }
    out.flush()?;
    approved(yes, if yes { None } else { ask() })?;
    if let Err(e) = cfg.write_import(&import, force) {
        bail!(e);
    }
    writeln!(out, "imported omp's model config into {}", config::models_path().display())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::model_config;
    use std::sync::Mutex;

    /// Tests that write `models.toml` and `config.toml` share one `MU_HOME`.
    static HOME: Mutex<()> = Mutex::new(());

    fn omp_import() -> Option<Import> {
        let (config, _) = model_config::parse(
            "set = \"main\"\n[sets.main]\ndefault = \"a/b\"\nsmol = \"c/d\"\n[sets.openai]\ndefault = \"o/p\"\n",
        );
        Some(Import { config, lines: vec!["set main: 2 roles".into(), "set openai: 1 roles".into()] })
    }

    fn run_in(cfg: &Configs, import: Option<Import>, force: bool, answer: Option<&str>) -> (Result<i32>, String) {
        let mut out = Vec::new();
        let a = answer.map(String::from);
        let r = exec(cfg, import, false, force, &mut out, &move || a.clone());
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn import_needs_approval_and_refuses_to_replace() {
        let _guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
        let path = {
            mu_testutil::home();
            config::models_path()
        };
        let _ = std::fs::remove_file(&path);
        let cfg = Configs::default();

        let (r, text) = run_in(&cfg, omp_import(), false, None);
        assert!(r.unwrap_err().to_string().starts_with("approval needed"));
        assert!(text.contains("set main: 2 roles") && text.contains("set openai: 1 roles"), "{text}");
        assert!(!path.exists());

        let (r, _) = run_in(&cfg, None, false, Some("y"));
        assert!(r.unwrap_err().to_string().starts_with("nothing to import"));

        let (r, text) = run_in(&cfg, omp_import(), false, Some("y"));
        assert_eq!(r.unwrap(), 0);
        assert!(text.contains("imported omp's model config into"), "{text}");
        let (parsed, warnings) = model_config::parse(&std::fs::read_to_string(&path).unwrap());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(parsed.sets.keys().collect::<Vec<_>>(), ["main", "openai"]);
        assert_eq!(parsed.set.as_deref(), Some("main"));
        assert_eq!(cfg.role("smol").as_deref(), Some("c/d"));

        let (r, _) = run_in(&cfg, omp_import(), false, Some("y"));
        assert!(r.unwrap_err().to_string().contains("already exists"));
        let (r, _) = run_in(&cfg, omp_import(), true, Some("y"));
        assert_eq!(r.unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    const PI_SETTINGS: &str = r#"{"defaultProvider":"anthropic","defaultModel":"claude-opus-5-5","defaultThinkingLevel":"high","theme":"light"}"#;
    const PI_MODELS: &str = r#"{"providers":{"vllm":{"baseUrl":"https://v.example/v1","api":"openai-completions",
        "apiKey":"sk-test-vllm-1234","models":[{"id":"a"}]}}}"#;
    const PI_AUTH: &str = r#"{"anthropic":{"type":"api_key","key":"sk-test-ant-1234"},
        "openai-codex":{"type":"oauth","access":"sk-test-oauth-1234"}}"#;

    struct PiEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: mu_testutil::TempDir,
        registry: Registry,
        store: Store,
    }

    fn pi_env() -> PiEnv {
        let guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
        let home = mu_testutil::home();
        for f in ["models.toml", "config.toml"] {
            let _ = std::fs::remove_file(home.join(f));
        }
        let dir = mu_testutil::tmp();
        std::fs::create_dir_all(dir.join("pi")).unwrap();
        for (name, text) in [("settings.json", PI_SETTINGS), ("models.json", PI_MODELS), ("auth.json", PI_AUTH)] {
            std::fs::write(dir.join("pi").join(name), text).unwrap();
        }
        PiEnv {
            registry: Registry::at(dir.join("providers.toml")),
            store: Store::at(dir.join("auth.json")),
            _guard: guard,
            dir,
        }
    }

    fn run_pi(e: &PiEnv, force: bool, answer: Option<&str>) -> (Result<i32>, String) {
        let mut out = Vec::new();
        let a = answer.map(String::from);
        let plan = Some(mu_base::pi_import::plan(&e.dir.join("pi")));
        let r = exec_pi(&Configs::default(), plan, &e.registry, &e.store, false, force, &mut out, &move || a.clone());
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn pi_import_lists_asks_writes_and_leaks_no_key() {
        let e = pi_env();
        let (r, text) = run_pi(&e, false, None);
        assert!(r.unwrap_err().to_string().starts_with("approval needed"));
        for want in [
            "set main: default = anthropic/claude-opus-5-5:high",
            "custom endpoint `vllm`: https://v.example/v1, 1 models, key: stored key",
            "API key `anthropic`: stored key",
            "theme: catppuccin-latte (pi: light)",
            "OAuth logins are never copied",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(!text.contains("sk-test"), "{text}");
        assert!(!config::models_path().exists() && !e.registry.path().exists() && !e.store.path().exists());

        let (r, text) = run_pi(&e, false, Some("y"));
        assert_eq!(r.unwrap(), 0);
        assert!(text.contains("imported pi's setup") && !text.contains("sk-test"), "{text}");
        assert_eq!(e.registry.snapshot().unwrap().keys().collect::<Vec<_>>(), ["vllm"]);
        assert_eq!(e.store.get("anthropic").unwrap().key.as_deref(), Some("sk-test-ant-1234"));
        assert_eq!(e.store.get("vllm").unwrap().key.as_deref(), Some("sk-test-vllm-1234"));
        assert!(!e.store.has("openai-codex"));
        let (models, _) = model_config::parse(&std::fs::read_to_string(config::models_path()).unwrap());
        assert_eq!(models.sets["main"]["default"], "anthropic/claude-opus-5-5:high");

        // Everything is in mu now: a second run has nothing to do, and --force still keeps the endpoint.
        let (r, text) = run_pi(&e, false, Some("y"));
        assert!(r.unwrap_err().to_string().starts_with("nothing to import"), "{text}");
        e.store.set("anthropic", mu_provider::auth::store::Entry::literal("sk-test-mine-1234")).unwrap();
        let (r, text) = run_pi(&e, true, Some("y"));
        assert_eq!(r.unwrap(), 0, "{text}");
        assert_eq!(e.store.get("anthropic").unwrap().key.as_deref(), Some("sk-test-ant-1234"));
        assert!(text.contains("skipped pi provider `vllm`") && !text.contains("sk-test"), "{text}");
    }

    #[test]
    fn pi_import_without_pi_setup_is_nothing() {
        let e = pi_env();
        let mut out = Vec::new();
        let r = exec_pi(&Configs::default(), None, &e.registry, &e.store, true, false, &mut out, &|| None);
        assert!(r.unwrap_err().to_string().starts_with("nothing to import: pi has no"));
    }

    #[test]
    fn source_choice() {
        let no = || None;
        let say = |s: &'static str| move || Some(format!("{s}\n"));
        assert_eq!(choose(Some(Source::Pi), true, true, &no).unwrap(), Source::Pi);
        assert_eq!(choose(None, true, false, &no).unwrap(), Source::Omp);
        assert_eq!(choose(None, false, true, &no).unwrap(), Source::Pi);
        assert!(choose(None, false, false, &no).unwrap_err().to_string().starts_with("nothing to import: neither omp"));
        assert_eq!(choose(None, true, true, &say("pi")).unwrap(), Source::Pi);
        assert_eq!(choose(None, true, true, &say(" OMP ")).unwrap(), Source::Omp);
        for ask in [&no as &dyn Fn() -> Option<String>, &say("maybe")] {
            assert!(choose(None, true, true, ask).unwrap_err().to_string().contains("--from omp or --from pi"));
        }
    }
}
