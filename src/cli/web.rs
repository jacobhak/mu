//! `mu web`: choose where `web_search` sends queries (see `mu_provider::auth::web`).

use super::approved;
use anyhow::{Result, bail};
use mu_provider::auth::store::{Store, now};
use mu_provider::auth::web::{self, SearchBackend, WebSearch, WebStatus, parse_backend};
use mu_provider::provider::providers::KeyTarget;
use std::io::{IsTerminal, Write};

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Show the chosen search backend and what it sends where.
    Status,
    /// Choose the search backend: parallel, exa, anthropic, openai or off. Exactly one is used,
    /// never a fallback.
    Use {
        /// parallel, exa, anthropic, openai or off.
        backend: String,
        /// Parallel or Exa through their free endpoint, with no key.
        #[arg(long)]
        keyless: bool,
        /// Approve the keyless third party without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Forget the choice; mu asks again the next time the model searches.
    Reset,
}

/// What `mu web use` will do, decided before anything is written.
#[derive(Debug, PartialEq)]
enum Plan {
    Off,
    /// Needs the user's approval of a third party.
    Keyless(SearchBackend),
    Keyed(SearchBackend),
}

fn plan_use(backend: SearchBackend, keyless: bool, has_key: bool) -> Result<Plan> {
    if backend == SearchBackend::Off {
        if keyless {
            bail!("--keyless does not apply to off");
        }
        return Ok(Plan::Off);
    }
    if keyless {
        if !matches!(backend, SearchBackend::Parallel | SearchBackend::Exa) {
            bail!("--keyless is only valid for parallel and exa");
        }
        return Ok(Plan::Keyless(backend));
    }
    if !has_key {
        let id = web::key_id(backend).unwrap_or_default();
        let label = KeyTarget::builtin(id).map_or_else(|| id.to_string(), |t| t.label().to_string());
        let chat = if matches!(backend, SearchBackend::Anthropic | SearchBackend::Openai) { " --no-chat" } else { "" };
        bail!("no {label} key is stored; add one with `mu auth add {id}{chat}`");
    }
    Ok(Plan::Keyed(backend))
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let tty = std::io::stdin().is_terminal();
    let ask = || {
        if !tty {
            return None;
        }
        eprint!("Approve? [y/N] ");
        std::io::stderr().flush().ok()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok()?;
        Some(s)
    };
    exec(&Store::new(), cmd, &mut std::io::stdout(), &ask)
}

fn exec(store: &Store, cmd: Cmd, out: &mut impl Write, ask: &dyn Fn() -> Option<String>) -> Result<i32> {
    match cmd {
        Cmd::Status => {
            match web::status(store) {
                WebStatus::Ready(w) => {
                    writeln!(out, "{}", web::label(w.backend, w.keyless))?;
                    if let Some(when) = w.approved.as_deref().filter(|_| w.keyless) {
                        writeln!(out, "approved {when}")?;
                    }
                    writeln!(out, "{}", web::about(w.backend, w.keyless))?;
                }
                WebStatus::Off => {
                    writeln!(out, "{}", web::label(SearchBackend::Off, false))?;
                    writeln!(out, "{}", web::about(SearchBackend::Off, false))?;
                }
                WebStatus::NotSetUp => {
                    writeln!(out, "not set up; run /web in mu or `mu web use <parallel|exa|anthropic|openai|off>`")?;
                }
            }
            Ok(0)
        }
        Cmd::Use { backend, keyless, yes } => {
            let Some(b) = parse_backend(&backend) else {
                bail!("unknown backend `{backend}`; use parallel, exa, anthropic, openai or off");
            };
            let has_key = web::key_id(b).is_some_and(|id| store.has(id));
            let settings = match plan_use(b, keyless, has_key)? {
                Plan::Off => WebSearch::off(),
                Plan::Keyed(b) => WebSearch::keyed(b),
                Plan::Keyless(b) => {
                    writeln!(out, "{}", web::label(b, true))?;
                    writeln!(out, "{}", web::about(b, true))?;
                    out.flush()?;
                    approved(yes, if yes { None } else { ask() })?;
                    WebSearch::keyless(b, now())
                }
            };
            store.set_web(Some(settings.clone()))?;
            writeln!(out, "web search: {}", web::label(settings.backend, settings.keyless))?;
            if !settings.keyless && settings.backend != SearchBackend::Off {
                writeln!(out, "{}", web::about(settings.backend, false))?;
            }
            Ok(0)
        }
        Cmd::Reset => {
            store.set_web(None)?;
            writeln!(out, "web search choice forgotten; mu asks again on the next search")?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SearchBackend::*;

    #[test]
    fn use_rules() {
        assert_eq!(plan_use(Off, false, false).unwrap(), Plan::Off);
        assert!(plan_use(Off, true, false).is_err());
        assert_eq!(plan_use(Parallel, true, false).unwrap(), Plan::Keyless(Parallel));
        assert_eq!(plan_use(Exa, false, true).unwrap(), Plan::Keyed(Exa));
        for b in [Anthropic, Openai] {
            let e = plan_use(b, true, true).unwrap_err().to_string();
            assert_eq!(e, "--keyless is only valid for parallel and exa");
        }
        let e = plan_use(Anthropic, false, false).unwrap_err().to_string();
        assert_eq!(e, "no Anthropic key is stored; add one with `mu auth add anthropic --no-chat`");
        let e = plan_use(Exa, false, false).unwrap_err().to_string();
        assert_eq!(e, "no Exa key is stored; add one with `mu auth add exa`");
    }

    #[test]
    fn approval_needs_yes_or_a_y_answer() {
        assert!(approved(true, None).is_ok());
        assert!(approved(false, Some("y\n".into())).is_ok());
        assert!(approved(false, Some("YES".into())).is_ok());
        for a in [None, Some("n".into()), Some("".into())] {
            let e = approved(false, a).unwrap_err().to_string();
            assert!(e.starts_with("approval needed: rerun with --yes"), "{e}");
        }
    }

    fn run_in(store: &Store, cmd: Cmd, answer: Option<&str>) -> (Result<i32>, String) {
        let mut out = Vec::new();
        let a = answer.map(String::from);
        let r = exec(store, cmd, &mut out, &move || a.clone());
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn use_status_and_reset_round_trip() {
        let d = std::env::temp_dir().join(format!("mu-cli-web-{}", uuid::Uuid::new_v4()));
        let s = Store::at(d.join("auth.json"));
        let (r, text) = run_in(&s, Cmd::Status, None);
        assert_eq!(r.unwrap(), 0);
        assert!(text.contains("not set up"), "{text}");

        let keyless = |yes| Cmd::Use { backend: "parallel".into(), keyless: true, yes };
        // Without approval nothing is stored, and the text was shown first.
        let (r, text) = run_in(&s, keyless(false), None);
        assert!(r.is_err());
        assert!(text.contains("Parallel Web Systems, Inc."), "{text}");
        assert_eq!(s.web(), None);
        let (r, _) = run_in(&s, keyless(false), Some("y"));
        assert_eq!(r.unwrap(), 0);
        let w = s.web().unwrap();
        assert!(w.keyless && w.approved.is_some() && w.backend == Parallel);
        let (_, text) = run_in(&s, Cmd::Status, None);
        assert!(text.starts_with("Parallel (no key)\napproved "), "{text}");

        let (r, _) = run_in(&s, Cmd::Use { backend: "off".into(), keyless: false, yes: false }, None);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(s.web(), Some(WebSearch::off()));
        let (r, _) = run_in(&s, Cmd::Use { backend: "bogus".into(), keyless: false, yes: false }, None);
        assert!(r.unwrap_err().to_string().contains("unknown backend"));
        let (r, _) = run_in(&s, Cmd::Reset, None);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(s.web(), None);
        let _ = std::fs::remove_dir_all(d);
    }
}
