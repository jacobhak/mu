//! `mu models`: copy omp's model config into mu's own `models.toml`.

use super::approved;
use anyhow::{Result, anyhow, bail};
use mu_base::config::{self, Configs};
use mu_base::model_config::Import;
use std::io::{IsTerminal, Write};

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Copy omp's model config into models.toml: modelRoles as set `main`, modelPresets as sets,
    /// cycleOrder, models.yml limits, compaction thresholds.
    Import {
        /// Copy without asking.
        #[arg(long)]
        yes: bool,
        /// Replace an existing models.toml.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let tty = std::io::stdin().is_terminal();
    let ask = || {
        if !tty {
            return None;
        }
        eprint!("Import? [y/N] ");
        std::io::stderr().flush().ok()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok()?;
        Some(s)
    };
    match cmd {
        Cmd::Import { yes, force } => {
            exec(&Configs::default(), config::omp_import(), yes, force, &mut std::io::stdout(), &ask)
        }
    }
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
}
