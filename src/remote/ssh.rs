//! The built-in `ssh` adapter: the contract's verbs as `ssh -T -o BatchMode=yes HOST mu ...`.

use super::adapter::{Start, StartAgent};
use anyhow::{Result, bail};

/// The ssh program; `MU_SSH_PROGRAM` replaces it (tests use a fake).
pub fn program() -> String {
    std::env::var("MU_SSH_PROGRAM").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "ssh".to_string())
}

/// `s` as one word of the remote shell's command line.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// ssh's arguments for running `remote` (one command line for the remote shell) on `host`.
pub fn argv(host: &str, remote: &str) -> Vec<String> {
    ["-T", "-o", "BatchMode=yes", host, remote].map(String::from).to_vec()
}

fn start_agent(a: &StartAgent) -> Result<String> {
    if a.bundle.is_some() {
        bail!("the ssh adapter cannot take a repository bundle");
    }
    let mut cmd = String::new();
    if let Some(dir) = &a.dir {
        cmd.push_str(&format!("cd {} && ", quote(dir)));
    }
    cmd.push_str("mu agent --detach");
    if let Some(m) = &a.model {
        cmd.push_str(&format!(" --model {}", quote(m)));
    }
    if let Some(id) = &a.resume {
        cmd.push_str(&format!(" --resume {}", quote(id)));
    } else if a.continue_latest {
        cmd.push_str(" -c");
    }
    Ok(cmd)
}

pub fn start(s: &Start) -> Result<String> {
    match s {
        Start::Agent(a) => start_agent(a),
        Start::Worker { .. } => bail!("the ssh adapter cannot run workers; use another adapter for `[task] remote`"),
    }
}

pub fn list() -> String {
    "mu agent list --json".to_string()
}

pub fn connect(id: &str) -> String {
    format!("mu attach --stdio {}", quote(id))
}

/// A hint for ssh's stderr, when it says why the login failed.
pub fn hint(stderr: &str) -> Option<&'static str> {
    stderr.contains("Permission denied").then_some(
        "ssh runs with BatchMode=yes and cannot ask for a password: load your key into ssh-agent (ssh-add) or \
         set up key login for this host",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_the_remote_shell() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(connect("a;rm -rf /"), "mu attach --stdio 'a;rm -rf /'");
    }

    #[test]
    fn start_builds_one_remote_command() {
        let a = StartAgent {
            resume: Some("abc".into()),
            dir: Some("/w/my repo".into()),
            model: Some("p/m".into()),
            ..Default::default()
        };
        assert_eq!(
            start(&Start::Agent(a)).unwrap(),
            "cd '/w/my repo' && mu agent --detach --model 'p/m' --resume 'abc'"
        );
        let c = StartAgent { continue_latest: true, ..Default::default() };
        assert_eq!(start(&Start::Agent(c)).unwrap(), "mu agent --detach -c");
        let w = Start::Worker { launch: "l".into(), bundle: "/b".into(), dir: "/d".into() };
        assert!(start(&w).unwrap_err().to_string().contains("cannot run workers"));
    }

    #[test]
    fn argv_never_prompts() {
        assert_eq!(
            argv("devbox", "mu agent list --json"),
            ["-T", "-o", "BatchMode=yes", "devbox", "mu agent list --json"]
        );
    }
}
