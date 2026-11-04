use super::Tools;
use mu_common::paths;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Local,
    Artifact,
    Skill,
}

impl Scheme {
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Local => "local",
            Scheme::Artifact => "artifact",
            Scheme::Skill => "skill",
        }
    }
}

pub struct Resolved {
    pub path: PathBuf,
    pub scheme: Option<Scheme>,
}

fn split_scheme(p: &str) -> Option<(Scheme, &str)> {
    for s in [Scheme::Local, Scheme::Artifact, Scheme::Skill] {
        if let Some(rest) = p.strip_prefix(s.name()).and_then(|r| r.strip_prefix("://")) {
            return Some((s, rest));
        }
    }
    None
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("path escapes its root: {}", rel.display()));
    }
    Ok(root.join(rel))
}

/// The id as a single safe path component; anything else shares one `unnamed` directory.
fn dir_id(session_id: &str) -> &str {
    if mu_session::session::valid_id(session_id) { session_id } else { "unnamed" }
}

pub fn local_root(session_id: &str) -> PathBuf {
    paths::mu_home().join("local").join(dir_id(session_id))
}

pub fn artifact_dir(session_id: &str) -> PathBuf {
    paths::mu_home().join("artifacts").join(dir_id(session_id))
}

impl Tools {
    pub fn set_session(&self, id: &str, path: Option<PathBuf>) {
        *self.session_id.borrow_mut() = id.into();
        *self.session_path.borrow_mut() = path;
    }

    pub fn set_skills(&self, skills: Vec<(String, PathBuf)>) {
        *self.skills.borrow_mut() = skills;
    }

    pub fn local_root(&self) -> PathBuf {
        local_root(&self.session_id.borrow())
    }

    pub fn artifact_dir(&self) -> PathBuf {
        artifact_dir(&self.session_id.borrow())
    }

    /// Next unused artifact number for this session.
    pub fn next_artifact(&self) -> (u32, PathBuf) {
        Self::next_artifact_in(&self.artifact_dir())
    }

    /// Next unused artifact number in `dir`, for jobs that outlive a borrow of `self`.
    pub fn next_artifact_in(dir: &Path) -> (u32, PathBuf) {
        let max = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.path().file_stem()?.to_str()?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        (max + 1, dir.join(format!("{}.log", max + 1)))
    }

    /// Maps a user-supplied path or internal URL to a filesystem path.
    pub fn resolve(&self, p: &str) -> Result<Resolved, String> {
        let Some((scheme, rest)) = split_scheme(p) else {
            return Ok(Resolved { path: self.abs(p), scheme: None });
        };
        let path = match scheme {
            Scheme::Local => safe_join(&self.local_root(), rest)?,
            Scheme::Artifact => {
                let n = rest.trim_matches('/');
                if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("artifact://{rest} not found"));
                }
                self.artifact_dir().join(format!("{n}.log"))
            }
            Scheme::Skill => {
                let (name, rel) = rest.split_once('/').unwrap_or((rest, ""));
                let dir = self
                    .skills
                    .borrow()
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| format!("skill://{name} not found"))?;
                if rel.is_empty() { dir.join("SKILL.md") } else { safe_join(&dir, rel)? }
            }
        };
        Ok(Resolved { path, scheme: Some(scheme) })
    }
}

/// Splits a trailing `:selector` (digits, `-`, `+`) off `p`.
pub fn split_selector(p: &str) -> Option<(&str, &str)> {
    let (head, tail) = p.rsplit_once(':')?;
    (!tail.is_empty()
        && !head.ends_with('/')
        && tail.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ','))
        && tail.chars().any(|c| c.is_ascii_digit()))
    .then_some((head, tail))
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReadTarget {
    pub path: String,
    pub sel: Option<String>,
    pub raw: bool,
}

/// Where a `read` path points. The first candidate that `exists` wins: the whole path, the path
/// without `:raw`, then the path before a line selector (`p:SEL`, `p:SEL:raw`, `p:raw:SEL`).
/// When none exists: the whole path, no selector, not raw.
pub fn read_target(p: &str, exists: impl Fn(&str) -> bool) -> ReadTarget {
    if exists(p) {
        return ReadTarget { path: p.to_string(), sel: None, raw: false };
    }
    // `:raw` (after a selector, or the reversed `path:raw:SEL`) asks for lines uncut by the column limit.
    let (stem, raw) = match p.strip_suffix(":raw") {
        Some(s) if !s.is_empty() => (s.to_string(), true),
        _ => match p.rsplit_once(":raw:") {
            Some((h, t))
                if !h.is_empty()
                    && t.chars().any(|c| c.is_ascii_digit())
                    && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ',')) =>
            {
                (format!("{h}:{t}"), true)
            }
            _ => (p.to_string(), false),
        },
    };
    if raw && exists(&stem) {
        return ReadTarget { path: stem, sel: None, raw: true };
    }
    if let Some((h, s)) = split_selector(&stem)
        && exists(h)
    {
        return ReadTarget { path: h.to_string(), sel: Some(s.to_string()), raw };
    }
    ReadTarget { path: p.to_string(), sel: None, raw: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_and_schemes() {
        assert_eq!(split_selector("artifact://1:-3"), Some(("artifact://1", "-3")));
        assert_eq!(split_selector("src/a.rs:10-20"), Some(("src/a.rs", "10-20")));
        assert_eq!(split_selector("local://"), None);
        assert_eq!(split_selector("a.rs"), None);
        let t = Tools::for_test();
        t.set_session("sid", None);
        assert!(t.resolve("local://../x").is_err());
        let r = t.resolve("local://a/plan.md").unwrap();
        assert_eq!(r.scheme, Some(Scheme::Local));
        assert!(r.path.ends_with("local/sid/a/plan.md"));
        assert!(t.resolve("skill://nope").is_err());
        assert!(t.resolve("artifact://x").is_err());
    }
}

#[cfg(test)]
mod read_target_tests {
    use super::*;

    #[test]
    fn read_targets_strip_selectors_and_raw() {
        let ex = |q: &str| q == "a.rs" || q == "odd:raw";
        let t = |p: &str, s: Option<&str>, raw: bool| ReadTarget { path: p.to_string(), sel: s.map(String::from), raw };
        assert_eq!(read_target("a.rs", ex), t("a.rs", None, false));
        assert_eq!(read_target("a.rs:raw", ex), t("a.rs", None, true));
        assert_eq!(read_target("a.rs:10-20", ex), t("a.rs", Some("10-20"), false));
        assert_eq!(read_target("a.rs:10-20:raw", ex), t("a.rs", Some("10-20"), true));
        assert_eq!(read_target("a.rs:raw:10-20", ex), t("a.rs", Some("10-20"), true));
        assert_eq!(read_target("a.rs:3,10-12:raw", ex), t("a.rs", Some("3,10-12"), true));
        assert_eq!(read_target("odd:raw", ex), t("odd:raw", None, false));
        assert_eq!(read_target("gone.rs:raw", ex), t("gone.rs:raw", None, false));
    }
}
