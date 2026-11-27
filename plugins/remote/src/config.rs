//! `config.toml` (see `paths::config_file`): the named remotes.
//!
//! ```toml
//! default_remote = "cluster"        # optional: `mu-remote` with no NAME uses it
//!
//! [remote.cluster]
//! adapter = "pod"                   # "ssh" is built in; anything else is a program on PATH or a path
//! trusted = true
//! [remote.cluster.settings]         # passed to the adapter as JSON; mu-remote does not read it
//! namespace = "…"
//!
//! [remote.devbox]
//! adapter = "ssh"
//! host = "devbox"
//! ```
//!
//! No secret goes in this file: a `settings` key that looks like one makes the remote unusable.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_remote: Option<String>,
    pub remote: BTreeMap<String, Remote>,
}

/// `[remote.NAME]`: where agents run. `adapter` is `ssh` (built in, needs `host`) or a program on PATH or a
/// path (`pod` is the installed Kubernetes adapter). `settings` goes to the adapter as JSON. `trusted` and
/// `repos` are ours: whether the main agent may run there, and the repositories (paths or origin URLs) whose
/// workers may run on an untrusted remote. `state_key` is the SSH ed25519 private key that unlocks the agent's
/// sealed state (default `~/.ssh/id_ed25519`). `reconnect_timeout` is how many seconds a launched task's lost
/// stream is retried before the launcher fetches what the worker has and finishes (default 600).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Remote {
    pub adapter: String,
    pub host: Option<String>,
    pub state_key: Option<String>,
    pub settings: toml::Table,
    pub trusted: bool,
    pub repos: Vec<String>,
    pub reconnect_timeout: Option<u64>,
}

const DEFAULT_RECONNECT_TIMEOUT: u64 = 600;

const SECRET_WORDS: [&str; 8] = ["token", "password", "passwd", "secret", "key", "apikey", "credential", "credentials"];
/// A trailing word that makes the key a reference to a secret rather than the secret itself.
const REFERENCE_WORDS: [&str; 6] = ["name", "ref", "file", "path", "env", "var"];

fn secret_looking(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|p| !p.is_empty()).collect();
    parts.iter().any(|p| SECRET_WORDS.contains(p)) && !parts.last().is_some_and(|p| REFERENCE_WORDS.contains(p))
}

fn find_secret_key(t: &toml::Table, path: &str) -> Option<String> {
    t.iter().find_map(|(k, v)| {
        let here = format!("{path}.{k}");
        if secret_looking(k) {
            return Some(here);
        }
        match v {
            toml::Value::Table(inner) => find_secret_key(inner, &here),
            _ => None,
        }
    })
}

impl Remote {
    /// Why `[remote.NAME]` cannot be used, if it cannot.
    pub fn validate(&self, name: &str) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("a remote needs a name".to_string());
        }
        let adapter = self.adapter.trim();
        if adapter.is_empty() {
            return Err(format!("[remote.{name}] needs an adapter (`ssh`, or a program)"));
        }
        if adapter == "ssh" {
            match self.host.as_deref().map(str::trim) {
                None | Some("") => return Err(format!("[remote.{name}] uses the ssh adapter and needs a host")),
                Some(h) if h.starts_with('-') => {
                    return Err(format!("[remote.{name}] host {h:?} must not start with -"));
                }
                Some(_) => {}
            }
        }
        if let Some(k) = find_secret_key(&self.settings, &format!("remote.{name}.settings")) {
            return Err(format!(
                "[{k}] looks like a secret; the config never holds credentials: the adapter reads them from where \
                 its tool keeps them (ssh-agent, kubeconfig). A reference ending in _name, _ref, _file, _path, \
                 _env or _var is fine"
            ));
        }
        Ok(())
    }

    pub fn reconnect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.reconnect_timeout.unwrap_or(DEFAULT_RECONNECT_TIMEOUT))
    }

    /// `settings` as the JSON the adapter gets in `MU_REMOTE_SETTINGS`.
    pub fn settings_json(&self) -> String {
        serde_json::to_string(&self.settings).unwrap_or_else(|_| "{}".to_string())
    }
}

impl Config {
    /// Reads `path`; a missing file is an empty config. A remote that cannot be used is dropped, its reason
    /// pushed on `warn`.
    pub fn read(path: &Path, warn: &mut Vec<String>) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let mut cfg: Config = toml::from_str(&text).with_context(|| path.display().to_string())?;
        cfg.remote.retain(|name, r| match r.validate(name) {
            Ok(()) => true,
            Err(e) => {
                warn.push(format!("{}: ignoring remote: {e}", path.display()));
                false
            }
        });
        Ok(cfg)
    }

    /// `over` on top of `self`: its remotes replace the ones of the same name (whole), its `default_remote`
    /// wins when set.
    pub fn overlay(&mut self, over: Config) {
        self.remote.extend(over.remote);
        if over.default_remote.is_some() {
            self.default_remote = over.default_remote;
        }
    }

    /// The configured remotes, `~/.config/mu-remote/config.toml` with the active profile's own file over it
    /// (warnings go to stderr).
    pub fn load() -> Result<Config> {
        let mut warn = Vec::new();
        let mut cfg = Config::read(&crate::paths::config_file(), &mut warn)?;
        if let Some(own) = crate::paths::profile_config_file() {
            cfg.overlay(Config::read(&own, &mut warn)?);
        }
        for w in warn {
            eprintln!("mu-remote: {w}");
        }
        Ok(cfg)
    }

    fn known(&self) -> String {
        if self.remote.is_empty() {
            format!("none is configured; add a [remote.NAME] section to {}", crate::paths::config_file().display())
        } else {
            format!("configured: {}", self.remote.keys().cloned().collect::<Vec<_>>().join(", "))
        }
    }

    /// The remote `name` names, else `default_remote`.
    pub fn pick(&self, name: Option<&str>) -> Result<(&str, &Remote)> {
        let name = match name.or(self.default_remote.as_deref()) {
            Some(n) => n,
            None => bail!("no remote named and no default_remote set ({})", self.known()),
        };
        match self.remote.get_key_value(name) {
            Some((k, r)) => Ok((k.as_str(), r)),
            None => bail!("unknown remote `{name}` ({})", self.known()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Config, Vec<String>) {
        let dir = crate::testutil::tmp();
        let file = dir.join("config.toml");
        std::fs::write(&file, text).unwrap();
        let mut w = Vec::new();
        let c = Config::read(&file, &mut w).unwrap();
        (c, w)
    }

    #[test]
    fn remotes_parse_with_defaults() {
        let (c, w) = parse(
            "default_remote = \"cluster\"\n\
             [remote.cluster]\nadapter = \"pod\"\ntrusted = true\n\
             [remote.cluster.settings]\nnamespace = \"ns\"\nsccache_secret_name = \"s\"\n\
             [remote.devbox]\nadapter = \"ssh\"\nhost = \"devbox\"\nrepos = [\"/src/a\"]\nstate_key = \"~/.ssh/other\"\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.default_remote.as_deref(), Some("cluster"));
        assert!(c.remote["cluster"].trusted);
        assert_eq!(c.remote["cluster"].settings_json(), r#"{"namespace":"ns","sccache_secret_name":"s"}"#);
        let devbox = &c.remote["devbox"];
        assert!(!devbox.trusted);
        assert_eq!(devbox.host.as_deref(), Some("devbox"));
        assert_eq!(devbox.repos, ["/src/a"]);
        assert_eq!(devbox.state_key.as_deref(), Some("~/.ssh/other"));
        assert_eq!(devbox.settings_json(), "{}");
    }

    #[test]
    fn invalid_remotes_are_dropped_with_a_reason() {
        let (c, w) = parse(
            "[remote.a]\n[remote.b]\nadapter = \"ssh\"\n[remote.c]\nadapter = \"ssh\"\nhost = \"-oProxyCommand=x\"\n\
             [remote.d]\nadapter = \"pod\"\n[remote.d.settings.auth]\nApi_Key = \"x\"\n\
             [remote.e]\nadapter = \"pod\"\n[remote.e.settings]\ntoken = \"x\"\n\
             [remote.f]\nadapter = \"pod\"\n[remote.f.settings]\nkubeconfig_path = \"/x\"\n",
        );
        assert_eq!(c.remote.keys().collect::<Vec<_>>(), ["f"]);
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w[0].contains("needs an adapter"), "{w:?}");
        assert!(w[1].contains("needs a host"), "{w:?}");
        assert!(w[2].contains("must not start with -"), "{w:?}");
        assert!(w[3].contains("remote.d.settings.auth.Api_Key") && w[3].contains("secret"), "{w:?}");
        assert!(w[4].contains("remote.e.settings.token"), "{w:?}");
    }

    #[test]
    fn the_reconnect_timeout_defaults_to_ten_minutes() {
        let (c, w) = parse("[remote.a]\nadapter = \"x\"\n[remote.b]\nadapter = \"x\"\nreconnect_timeout = 5\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.remote["a"].reconnect_timeout(), std::time::Duration::from_secs(600));
        assert_eq!(c.remote["b"].reconnect_timeout(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_missing_file_is_an_empty_config() {
        let mut w = Vec::new();
        let c = Config::read(&crate::testutil::tmp().join("nothing.toml"), &mut w).unwrap();
        assert!(c.remote.is_empty() && c.default_remote.is_none() && w.is_empty());
    }

    #[test]
    fn a_profile_file_replaces_remotes_by_name_and_adds_the_rest() {
        let (mut shared, _) = parse(
            "default_remote = \"a\"\n[remote.a]\nadapter = \"ssh\"\nhost = \"a\"\n[remote.b]\nadapter = \"ssh\"\nhost = \"b\"\n",
        );
        let (own, _) =
            parse("[remote.b]\nadapter = \"ssh\"\nhost = \"b2\"\n[remote.c]\nadapter = \"ssh\"\nhost = \"c\"\n");
        shared.overlay(own);
        assert_eq!(shared.default_remote.as_deref(), Some("a"));
        assert_eq!(shared.remote.keys().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(shared.remote["b"].host.as_deref(), Some("b2"));
        let (own, _) = parse("default_remote = \"c\"\n[remote.c]\nadapter = \"ssh\"\nhost = \"c\"\n");
        shared.overlay(own);
        assert_eq!(shared.default_remote.as_deref(), Some("c"));
    }

    #[test]
    fn picking_a_remote_says_what_is_configured() {
        let (c, _) = parse(
            "default_remote = \"a\"\n[remote.a]\nadapter = \"ssh\"\nhost = \"a\"\n[remote.b]\nadapter = \"ssh\"\nhost = \"b\"\n",
        );
        assert_eq!(c.pick(None).unwrap().0, "a");
        assert_eq!(c.pick(Some("b")).unwrap().0, "b");
        assert_eq!(c.pick(Some("zz")).unwrap_err().to_string(), "unknown remote `zz` (configured: a, b)");
        let none = Config::default();
        assert!(none.pick(None).unwrap_err().to_string().contains("no remote named"));
    }
}
