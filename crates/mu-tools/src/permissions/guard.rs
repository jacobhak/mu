//! The built-in deny no mode or rule can lift: a recursive `rm` of `/`, `~` or `$HOME`.

use super::bash::{BashAnalysis, unquote, unwrapped};

pub const REFUSAL: &str =
    "Not run: mu never runs a recursive rm of /, ~ or $HOME, whatever the permission mode or rules.";

/// Some simple command of the line removes the filesystem root or the home directory recursively.
pub fn removes_root_or_home(a: &BashAnalysis, home: Option<&str>) -> bool {
    a.cmds.iter().any(|c| unwrapped(&c.plain, 0).iter().any(|w| w[0] == "rm" && rm_removes(&w[1..], home)))
}

fn rm_removes(args: &[String], home: Option<&str>) -> bool {
    let mut recursive = false;
    let mut targets = Vec::new();
    let mut options = true;
    for w in args {
        let w = unquote(w);
        if options && w == "--" {
            options = false;
        } else if options && w.starts_with("--") {
            recursive |= w == "--recursive";
        } else if options && w.len() > 1 && w.starts_with('-') {
            recursive |= w.contains(['r', 'R']);
        } else {
            targets.push(w);
        }
    }
    recursive && targets.iter().any(|t| is_root_or_home(t, home))
}

/// `/`, `/*`, `~`, `~/`, `~/*`, `$HOME`, `${HOME}/.` and the home path itself, however written.
fn is_root_or_home(target: &str, home: Option<&str>) -> bool {
    if target.is_empty() {
        return false;
    }
    let mut t = target.to_string();
    for var in ["${HOME}", "$HOME"] {
        if let Some(rest) = t.strip_prefix(var) {
            t = format!("~{rest}");
            break;
        }
    }
    if let Some(h) = home.map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty())
        && let Some(rest) = t.strip_prefix(h)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        t = format!("~{rest}");
    }
    loop {
        let trimmed = t.strip_suffix("/*").or_else(|| t.strip_suffix("/.")).or_else(|| t.strip_suffix('/'));
        match trimmed {
            Some(s) => t = s.to_string(),
            None => break,
        }
    }
    t.is_empty() || t == "~"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::analyze;

    fn removes(cmd: &str) -> bool {
        removes_root_or_home(&analyze(cmd), Some("/home/u"))
    }

    #[test]
    fn recursive_removal_of_root_or_home_is_caught() {
        for cmd in [
            "rm -rf /",
            "rm -rf /*",
            "rm -fr ~",
            "rm -r -f ~/",
            "rm -Rf ~/*",
            "rm --recursive --force $HOME",
            "rm -rf \"$HOME\"",
            "rm -rf ${HOME}/",
            "rm -rf /home/u",
            "rm -rf /home/u/*",
            "rm -rf --no-preserve-root /",
            "rm -rf -- ~",
            "/bin/rm -rf ~",
            "\\rm -rf ~",
            "sudo rm -rf /",
            "sudo -u root rm -rf /",
            "cd /tmp && rm -rf ~",
            "echo $(rm -rf ~)",
            "bash -c 'rm -rf ~'",
            "sh -ec \"rm -rf /\"",
            "eval rm -rf '~'",
            "rm -rf build ~",
        ] {
            assert!(removes(cmd), "{cmd}");
        }
    }

    #[test]
    fn ordinary_removals_run() {
        for cmd in [
            "rm -rf target",
            "rm -rf ~/code/x/target",
            "rm -rf $HOME/tmp",
            "rm -rf /tmp/x",
            "rm -rf /home/u2",
            "rm -rf /home/uv",
            "rm ~",
            "rm -f /",
            "rm -rf \"\"",
            "echo rm -rf ~",
            "git commit -m 'rm -rf ~'",
            "bash scripts/x.sh /",
        ] {
            assert!(!removes(cmd), "{cmd}");
        }
    }
}
