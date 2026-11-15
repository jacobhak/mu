//! Named remotes (`[remote.NAME]`): where agents run besides here. `adapter` speaks the adapter contract.

pub mod adapter;
pub mod ssh;

use anyhow::{Result, bail};
use mu_base::config::MuRemote;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Local,
    Remote(String),
}

fn known(remotes: &BTreeMap<String, MuRemote>) -> String {
    if remotes.is_empty() {
        "none is configured; add a [remote.NAME] section to config.toml".to_string()
    } else {
        format!("configured: {}", remotes.keys().cloned().collect::<Vec<_>>().join(", "))
    }
}

/// Where this run goes: `--local` here, `-r NAME` on that remote, else `default_remote`, else here.
pub fn resolve(
    remote: Option<&str>,
    local: bool,
    default_remote: Option<&str>,
    remotes: &BTreeMap<String, MuRemote>,
) -> Result<Selection> {
    if local {
        return Ok(Selection::Local);
    }
    if let Some(name) = remote {
        if !remotes.contains_key(name) {
            bail!("unknown remote `{name}` ({})", known(remotes));
        }
        return Ok(Selection::Remote(name.to_string()));
    }
    match default_remote {
        Some(name) if remotes.contains_key(name) => Ok(Selection::Remote(name.to_string())),
        Some(name) => {
            bail!("default_remote `{name}` is not a configured remote ({}); `mu --local` runs here", known(remotes))
        }
        None => Ok(Selection::Local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes() -> BTreeMap<String, MuRemote> {
        ["a", "b"]
            .into_iter()
            .map(|n| (n.to_string(), MuRemote { adapter: "ssh".into(), host: Some(n.into()), ..Default::default() }))
            .collect()
    }

    #[test]
    fn selection_order_is_local_then_flag_then_default() {
        let r = remotes();
        let remote = |n: &str| Selection::Remote(n.to_string());
        assert_eq!(resolve(None, false, None, &r).unwrap(), Selection::Local);
        assert_eq!(resolve(None, false, Some("a"), &r).unwrap(), remote("a"));
        assert_eq!(resolve(Some("b"), false, Some("a"), &r).unwrap(), remote("b"));
        assert_eq!(resolve(None, true, Some("a"), &r).unwrap(), Selection::Local);
        assert_eq!(resolve(None, true, Some("gone"), &r).unwrap(), Selection::Local);
    }

    #[test]
    fn unknown_names_say_what_is_configured() {
        let r = remotes();
        let e = resolve(Some("zz"), false, None, &r).unwrap_err().to_string();
        assert_eq!(e, "unknown remote `zz` (configured: a, b)");
        let e = resolve(None, false, Some("gone"), &r).unwrap_err().to_string();
        assert!(e.contains("default_remote `gone`") && e.contains("mu --local"), "{e}");
        let e = resolve(Some("zz"), false, None, &BTreeMap::new()).unwrap_err().to_string();
        assert!(e.contains("none is configured"), "{e}");
    }
}
