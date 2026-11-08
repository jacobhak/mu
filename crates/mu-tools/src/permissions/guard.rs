//! The built-in deny no mode or rule can lift: a recursive `rm` of `/`, `~` or `$HOME`.

use super::bash::{BashAnalysis, analyze};

pub const REFUSAL: &str =
    "Not run: mu never runs a recursive rm of /, ~ or $HOME, whatever the permission mode or rules.";

/// Programs that run the command named in their arguments.
const WRAPPERS: &[&str] =
    &["sudo", "doas", "command", "builtin", "exec", "env", "nice", "nohup", "time", "timeout", "xargs"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
/// How deep `sh -c '…'` and `eval` strings are followed.
const MAX_DEPTH: usize = 3;

/// Some simple command of the line removes the filesystem root or the home directory recursively.
pub fn removes_root_or_home(a: &BashAnalysis, home: Option<&str>) -> bool {
    removes(a, home, 0)
}

fn removes(a: &BashAnalysis, home: Option<&str>, depth: usize) -> bool {
    a.cmds.iter().any(|c| command_removes(&c.words, home, depth))
}

fn command_removes(words: &[String], home: Option<&str>, depth: usize) -> bool {
    let Some(first) = words.first() else { return false };
    let mut prog = program(first);
    let mut at = 0;
    if WRAPPERS.contains(&prog.as_str()) {
        // Wrapper options can take values (`sudo -u root`), so look for the program by name.
        let Some(i) = words.iter().skip(1).position(|w| {
            let p = program(w);
            p == "rm" || p == "eval" || SHELLS.contains(&p.as_str())
        }) else {
            return false;
        };
        at = i + 1;
        prog = program(&words[at]);
    }
    let args = &words[at + 1..];
    match prog.as_str() {
        "rm" => rm_removes(args, home),
        "eval" if depth < MAX_DEPTH => {
            nested(&args.iter().map(|w| unquote(w)).collect::<Vec<_>>().join(" "), home, depth)
        }
        p if SHELLS.contains(&p) && depth < MAX_DEPTH => args
            .iter()
            .position(|w| w.len() > 1 && w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
            .and_then(|i| args.get(i + 1))
            .is_some_and(|script| nested(&unquote(script), home, depth)),
        _ => false,
    }
}

fn nested(script: &str, home: Option<&str>, depth: usize) -> bool {
    removes(&analyze(script), home, depth + 1)
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

/// A program word's name: quotes and a leading `\` dropped, directories stripped (`/bin/rm`).
fn program(word: &str) -> String {
    let w = unquote(word);
    let w = w.trim_start_matches('\\');
    w.rsplit('/').next().unwrap_or(w).to_string()
}

fn unquote(word: &str) -> String {
    word.chars().filter(|c| *c != '\'' && *c != '"').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
