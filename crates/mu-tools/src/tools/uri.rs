use super::Tools;
use mu_common::paths;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Local,
    Artifact,
    Skill,
    /// mu's own documentation; see `docs`.
    Mu,
}

impl Scheme {
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Local => "local",
            Scheme::Artifact => "artifact",
            Scheme::Skill => "skill",
            Scheme::Mu => "mu",
        }
    }
}

pub struct Resolved {
    pub path: PathBuf,
    pub scheme: Option<Scheme>,
}

/// The first existing `rest` under an ancestor session's directory (`root_of` maps a session id to
/// it): the session's parent from its header, then that one's parent, up to a depth limit.
fn in_ancestors(session: Option<PathBuf>, rest: &str, root_of: impl Fn(&str) -> PathBuf) -> Option<PathBuf> {
    let mut cur = session;
    for _ in 0..8 {
        let parent = mu_session::session::owner::header_parent(&cur?)?;
        let id = mu_session::session::list::id_of(&parent);
        if mu_session::session::list::valid_id(&id) {
            let found = safe_join(&root_of(&id), rest).ok().filter(|f| f.exists());
            if found.is_some() {
                return found;
            }
        }
        cur = Some(parent);
    }
    None
}

fn split_scheme(p: &str) -> Option<(Scheme, &str)> {
    for s in [Scheme::Local, Scheme::Artifact, Scheme::Skill, Scheme::Mu] {
        if let Some(rest) = p.strip_prefix(s.name()).and_then(|r| r.strip_prefix("://")) {
            return Some((s, rest));
        }
    }
    None
}

pub(super) fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("path escapes its root: {}", rel.display()));
    }
    Ok(root.join(rel))
}

/// The id as a single safe path component; anything else shares one `unnamed` directory.
fn dir_id(session_id: &str) -> &str {
    if mu_session::session::list::valid_id(session_id) { session_id } else { "unnamed" }
}

pub fn local_root(session_id: &str) -> PathBuf {
    paths::profile_home().join("local").join(dir_id(session_id))
}

pub fn artifact_dir(session_id: &str) -> PathBuf {
    paths::profile_home().join("artifacts").join(dir_id(session_id))
}

/// Where `mu://state/` is written for this session.
pub(super) fn state_dir(session_id: &str) -> PathBuf {
    paths::profile_home().join("mu-state").join(dir_id(session_id))
}

impl Tools {
    pub fn set_session(&self, id: &str, path: Option<PathBuf>) {
        *self.session.id.borrow_mut() = id.into();
        *self.session.path.borrow_mut() = path;
    }

    pub fn set_skills(&self, skills: Vec<(String, PathBuf)>) {
        *self.session.skills.borrow_mut() = skills;
    }

    pub fn local_root(&self) -> PathBuf {
        local_root(&self.session.id.borrow())
    }

    pub fn artifact_dir(&self) -> PathBuf {
        artifact_dir(&self.session.id.borrow())
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

    /// The directory or file `grep` and `glob` search: `path` (default `.`) resolved, plus the URL to
    /// show results under when it was an internal URL.
    pub(super) fn search_root(&self, path: Option<&str>) -> Result<(PathBuf, Option<String>), String> {
        let p = path.unwrap_or(".");
        let r = self.resolve_read(p)?;
        Ok((r.path, r.scheme.map(|_| p.to_string())))
    }

    /// `resolve` for reading: a `local://` file this session lacks is looked for in the local
    /// directories of its ancestors, nearest first, so a subagent reads what its parent wrote (a
    /// plan handed over by path). Writes use `resolve` and never leave the session's own directory.
    pub fn resolve_read(&self, p: &str) -> Result<Resolved, String> {
        let r = self.resolve(p)?;
        if r.scheme != Some(Scheme::Local) || r.path.exists() {
            return Ok(r);
        }
        let Some((_, rest)) = split_scheme(p) else { return Ok(r) };
        let session = self.session.path.borrow().clone();
        Ok(match in_ancestors(session, rest, local_root) {
            Some(path) => Resolved { path, scheme: r.scheme },
            None => r,
        })
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
            Scheme::Mu => self.mu_path(rest)?,
            Scheme::Skill => {
                let (name, rel) = rest.split_once('/').unwrap_or((rest, ""));
                let dir = self
                    .session
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

/// The file `p` reaches: symlinks in its existing part resolved (a dangling link to where it points),
/// and `.` and `..` folded in order, so a `..` after a missing directory cannot leave the place the
/// path reaches. A write goes here, and so does the gate's judgement of it.
pub fn physical(p: &Path) -> PathBuf {
    physical_in(p, 40)
}

fn physical_in(p: &Path, links: u32) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(n) => {
                out.push(n);
                match out.canonicalize() {
                    Ok(c) => out = c,
                    Err(_) if links > 0 => {
                        if let Ok(link) = std::fs::read_link(&out) {
                            let at = out.parent().map(|d| d.join(&link)).unwrap_or(link);
                            out = physical_in(&at, links - 1);
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
    out
}

/// `abs`, found under a search `root`, as shown to the model: under the URL `prefix` the search
/// started from when it was an internal URL, else relative to `cwd`.
pub(super) fn shown(cwd: &Path, root: &Path, prefix: Option<&str>, abs: &Path) -> String {
    let Some(p) = prefix else { return super::display_in(cwd, abs) };
    match abs.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{}", rel.display())
        }
        _ => p.to_string(),
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

/// A path whose trailing selector cannot be read as one, as the file it names and that selector.
/// `x.rs:285:345` is the common case: two numbers where the documented form takes one range. It is
/// returned only when the file exists, so a path that merely ends in digits stays "not found".
pub fn bad_selector(p: &str, exists: impl Fn(&str) -> bool) -> Option<(String, String)> {
    let selectorish = |s: &str| {
        !s.is_empty()
            && s.chars().any(|c| c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ','))
    };
    let mut head = p;
    let mut parts: Vec<&str> = Vec::new();
    while let Some((h, t)) = head.rsplit_once(':') {
        if !selectorish(t) {
            return None;
        }
        parts.insert(0, t);
        head = h;
        if exists(head) {
            return (parts.len() > 1).then(|| (head.to_string(), parts.join(":")));
        }
    }
    None
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
    fn physical_follows_a_dangling_link_and_folds_dots_after_a_missing_directory() {
        let d = std::env::temp_dir().join(format!("mu-physical-{}", std::process::id()));
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::create_dir_all(d.join("b")).unwrap();
        let d = d.canonicalize().unwrap();
        std::os::unix::fs::symlink("missing-target", d.join("link")).unwrap();
        assert_eq!(physical(&d.join("link/x")), d.join("missing-target/x"));
        assert_eq!(physical(&d.join("a/zz/../b")), d.join("a/b"));
        std::os::unix::fs::symlink("loop", d.join("loop")).unwrap();
        assert_eq!(physical(&d.join("loop")), d.join("loop"), "a link that points at itself stays put");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_local_file_is_found_in_an_ancestors_directory() {
        let d = std::env::temp_dir().join(format!("mu-anc-{}", std::process::id()));
        let sess = |name: &str, parent: Option<&str>| {
            let p = d.join(format!("{name}.jsonl"));
            let parent = parent.map(|x| format!(",\"parentSession\":\"{}\"", d.join(format!("{x}.jsonl")).display()));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(&p, format!("{{\"type\":\"session\",\"id\":\"{name}\"{}}}\n", parent.unwrap_or_default()))
                .unwrap();
            p
        };
        let root_of = |id: &str| d.join("local").join(id);
        sess("2026_aaaa1111", None);
        sess("2026_bbbb2222", Some("2026_aaaa1111"));
        let child = sess("2026_cccc3333", Some("2026_bbbb2222"));
        std::fs::create_dir_all(root_of("aaaa1111")).unwrap();
        std::fs::write(root_of("aaaa1111").join("plan.md"), "p").unwrap();
        let got = in_ancestors(Some(child.clone()), "plan.md", root_of);
        assert_eq!(got, Some(root_of("aaaa1111").join("plan.md")), "the grandparent's file");
        assert_eq!(in_ancestors(Some(child.clone()), "nope.md", root_of), None);
        assert_eq!(in_ancestors(Some(child), "../x", root_of), None, "never outside the root");
        assert_eq!(in_ancestors(None, "plan.md", root_of), None);
        std::fs::remove_dir_all(&d).ok();
    }

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

    /// `x.rs:285:345` names a real file and a selector no form accepts: that is a selector error,
    /// not a missing file. A path that is simply absent stays absent.
    #[test]
    fn two_numbers_after_a_file_are_a_bad_selector() {
        let ex = |q: &str| q == "src/x.rs";
        assert_eq!(bad_selector("src/x.rs:285:345", ex), Some(("src/x.rs".into(), "285:345".into())));
        assert_eq!(bad_selector("src/x.rs:12-20", ex), None, "a good selector is read elsewhere");
        assert_eq!(bad_selector("src/x.rs:1:2:3", ex), Some(("src/x.rs".into(), "1:2:3".into())));
        assert_eq!(bad_selector("gone.rs:285:345", ex), None);
        assert_eq!(bad_selector("src/x.rs", ex), None);
        assert_eq!(bad_selector("src/x.rs:abc:def", ex), None);
    }
}
