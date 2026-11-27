//! Finding session files: ids, lookup by id or prefix, and the list of recent sessions.
use super::format::slot_title;
use mu_base::message::Message;
use mu_common::paths;
use serde_json::Value;
use std::fs::File;
use std::path::{Path, PathBuf};

/// `--profile <name> ` (trailing space) when a profile is active, else empty: for resume hints.
pub fn profile_flag() -> String {
    mu_common::paths::profile().map(|p| format!("--profile {p} ")).unwrap_or_default()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub title: Option<String>,
    pub first_prompt: String,
    pub modified: std::time::SystemTime,
    /// Working directory recorded in the session header.
    pub cwd: Option<PathBuf>,
}

/// Session id from a file named `<timestamp>_<id>.jsonl`.
pub fn id_of(path: &Path) -> String {
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.rsplit_once('_').map_or(name.clone(), |(_, id)| id.to_string())
}

/// The newest session of `cwd` that has content and a readable header: a crash can leave a newer,
/// empty or torn file behind, and that one is not what `--continue` means.
pub fn latest_for(cwd: &Path) -> Option<PathBuf> {
    let mut files = jsonl_files(&paths::session_dir(cwd));
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files.into_iter().find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.len() > 0) && read_info(p.clone()).is_some_and(|s| s.cwd.is_some())
    })
}

/// A session file name is one path component ending in `.jsonl`, with characters from
/// `[A-Za-z0-9._-]` and no leading dot.
pub fn valid_file_name(name: &str) -> bool {
    name.strip_suffix(".jsonl").is_some_and(|stem| {
        !stem.is_empty()
            && !stem.starts_with('.')
            && stem.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    })
}

/// A session id is `[A-Za-z0-9-]+`: nothing that could name a path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Why no single session file matched.
#[derive(Debug)]
pub enum FindError {
    Empty,
    Invalid(String),
    NotFound(String),
    Ambiguous { id: String, matches: Vec<String> },
    Io(std::io::Error),
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty session argument"),
            Self::Invalid(id) => write!(f, "invalid session id `{id}`"),
            Self::NotFound(id) => write!(f, "session {id} not found"),
            Self::Ambiguous { id, matches } => {
                write!(f, "session id `{id}` is ambiguous; matches: {}", matches.join(", "))
            }
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for FindError {}

/// Session file for `arg`: an existing `.jsonl` file, else a session id (or id prefix).
pub fn find(cwd: &Path, arg: &Path) -> Result<PathBuf, FindError> {
    let Some(s) = arg.to_str().filter(|s| !s.trim().is_empty()) else {
        return Err(FindError::Empty);
    };
    if arg.is_file() && arg.extension().is_some_and(|x| x == "jsonl") {
        return Ok(arg.to_path_buf());
    }
    find_id(cwd, s)
}

/// Session file for an id or unique id prefix, searched in `cwd`'s sessions first and then in
/// every other project's. Never treats `id` as a path.
pub fn find_id(cwd: &Path, id: &str) -> Result<PathBuf, FindError> {
    if !valid_id(id) {
        return Err(FindError::Invalid(id.to_string()));
    }
    let own = paths::session_dir(cwd);
    let root = own.parent().ok_or_else(|| FindError::NotFound(id.to_string()))?;
    let matches_in = |d: &Path| -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(d) else { return vec![] };
        let mut v: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                p.extension().is_some_and(|x| x == "jsonl")
                    && stem.rsplit_once('_').is_some_and(|(_, sid)| sid.starts_with(id))
            })
            .collect();
        v.sort();
        v
    };
    let mut found = matches_in(&own);
    if found.is_empty() {
        let others = std::fs::read_dir(root).map_err(FindError::Io)?;
        for d in others.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| *p != own && p.is_dir()) {
            found.extend(matches_in(&d));
        }
    }
    match found.len() {
        0 => Err(FindError::NotFound(id.to_string())),
        1 => Ok(found.remove(0)),
        _ => Err(FindError::Ambiguous { id: id.to_string(), matches: found.iter().map(|p| id_of(p)).collect() }),
    }
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect()
}

/// `read_info` for other modules: title, working directory, first prompt and last write.
pub fn info(path: &Path) -> Option<SessionInfo> {
    read_info(path.to_path_buf())
}

/// Title, working directory and first prompt of one session file; reads only its first lines.
fn read_info(path: PathBuf) -> Option<SessionInfo> {
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let f = std::io::BufReader::new(File::open(&path).ok()?);
    let mut title = None;
    let mut cwd = None;
    let mut first_prompt = String::new();
    for (i, l) in std::io::BufRead::lines(f).map_while(Result::ok).enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        match v["type"].as_str() {
            Some("title") if i == 0 => title = slot_title(&v),
            Some("session") => cwd = v["cwd"].as_str().map(PathBuf::from),
            Some("title_change") => title = v["title"].as_str().filter(|t| !t.is_empty()).map(String::from),
            Some("message") if first_prompt.is_empty() && v["message"]["role"] == "user" => {
                if let Ok(Message::User { content, .. }) = serde_json::from_value(v["message"].clone()) {
                    first_prompt = content
                        .iter()
                        .find_map(|b| match b {
                            mu_base::message::Block::Text { text } => Some(text.replace('\n', " ")),
                            _ => None,
                        })
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect();
                }
            }
            _ => {}
        }
        if title.is_some() && !first_prompt.is_empty() {
            break;
        }
    }
    Some(SessionInfo { path, title, first_prompt, modified, cwd })
}

/// Working directory recorded in a session's header.
pub fn header_cwd(path: &Path) -> Option<PathBuf> {
    read_info(path.to_path_buf())?.cwd
}

/// The directory the session last worked in: the newest `cwd_change` on its active branch (a switch
/// on a rewound branch does not count), else `None` (it never switched, so the header's is the one).
/// Loads the session, so call it once on resume, not per listing.
pub fn last_cwd(path: &Path) -> Option<PathBuf> {
    super::format::load(path).ok()?.cwd
}

/// The newest `limit` sessions (of `only_cwd`'s project, or of every project), newest first.
/// File names start with a timestamp, so they are sorted without opening anything; only the
/// files that make the cut are read.
pub fn list_recent(cwd: &Path, only_cwd: Option<&Path>, limit: usize) -> Vec<SessionInfo> {
    let own = paths::session_dir(cwd);
    let dirs: Vec<PathBuf> = match only_cwd {
        Some(c) => vec![paths::session_dir(c)],
        None => match own.parent().and_then(|r| std::fs::read_dir(r).ok()) {
            Some(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect(),
            None => return vec![],
        },
    };
    let mut files: Vec<PathBuf> = dirs.iter().flat_map(|d| jsonl_files(d)).collect();
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files
        .into_iter()
        .filter_map(read_info)
        .filter(|s| only_cwd.is_none_or(|w| s.cwd.as_deref() == Some(w)))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::file::SessionFile;
    use mu_testutil::home;

    #[test]
    fn a_session_file_name_is_one_plain_jsonl_component() {
        assert!(valid_file_name("2026-11-14T10-00-00-000Z_0192abcd-ef.jsonl"));
        for bad in ["", ".jsonl", ".hidden.jsonl", "a.txt", "a/b.jsonl", "../a.jsonl", "a b.jsonl", "a\\b.jsonl"] {
            assert!(!valid_file_name(bad), "{bad}");
        }
    }

    #[test]
    fn id_validation_and_lookup() {
        assert!(valid_id("0198-abc"));
        for bad in ["", ".", "..", "a/b", "a.b", "a_b", "a b"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        home();
        let cwd = Path::new("/opt/find");
        let a = SessionFile::create(cwd, None).unwrap();
        let id = id_of(&a.path);
        assert_eq!(find(cwd, Path::new(&id)).unwrap(), a.path);
        assert!(find(cwd, Path::new("")).is_err());
        assert!(find(cwd, Path::new("..")).is_err());
        assert!(find(cwd, Path::new(&a.path.parent().unwrap().to_path_buf())).is_err(), "a directory is not a session");
        assert_eq!(find(cwd, &a.path).unwrap(), a.path);
        // Two sessions sharing a prefix are ambiguous.
        let b = SessionFile::create(cwd, None).unwrap();
        let (ia, ib) = (id_of(&a.path), id_of(&b.path));
        let common = ia.chars().zip(ib.chars()).take_while(|(x, y)| x == y).count();
        if common > 0 {
            let e = find_id(cwd, &ia[..common]).unwrap_err();
            assert!(matches!(e, FindError::Ambiguous { .. }), "{e}");
        }
    }

    #[test]
    fn latest_skips_newer_files_that_are_empty_or_headerless() {
        home();
        let cwd = Path::new("/opt/latest-empty");
        let mut s = SessionFile::create(cwd, None).unwrap();
        s.message(&Message::user("hello"));
        let dir = s.path.parent().unwrap().to_path_buf();
        std::fs::write(dir.join("9999_empty.jsonl"), "").unwrap();
        std::fs::write(dir.join("9998_torn.jsonl"), "{\"type\":\"sess").unwrap();
        assert_eq!(latest_for(cwd), Some(s.path.clone()));
    }

    #[test]
    fn a_switch_is_recorded_and_the_session_stays_listed_where_it_was_made() {
        home();
        let (a, b) = (Path::new("/opt/switch-a"), Path::new("/opt/switch-b"));
        let mut s = SessionFile::create(a, None).unwrap();
        s.message(&Message::user("hello"));
        assert_eq!(last_cwd(&s.path), None);
        s.record_cwd(b);
        s.message(&Message::user("again"));
        s.record_cwd(Path::new("/opt/switch-c"));
        assert_eq!(last_cwd(&s.path).as_deref(), Some(Path::new("/opt/switch-c")));
        assert_eq!(header_cwd(&s.path).as_deref(), Some(a));
        let path = s.path.clone();
        assert_eq!(list_recent(a, Some(a), 10).iter().filter(|i| i.path == path).count(), 1);
        assert!(list_recent(b, Some(b), 10).iter().all(|i| i.path != path));
        // The entry is tolerated on load and does not become a message.
        let loaded = crate::session::format::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        // A reopened session keeps appending after it.
        drop(s);
        let mut s = SessionFile::open_append(&path).unwrap();
        s.record_cwd(b);
        assert_eq!(last_cwd(&path).as_deref(), Some(b));
    }

    #[test]
    fn a_switch_on_a_rewound_branch_is_not_where_the_session_last_worked() {
        home();
        let (a, b) = (Path::new("/opt/rewind-a"), Path::new("/opt/rewind-b"));
        let mut s = SessionFile::create(a, None).unwrap();
        let first = s.message(&Message::user("hello")).unwrap();
        s.record_cwd(b);
        s.message(&Message::user("in b"));
        assert_eq!(last_cwd(&s.path).as_deref(), Some(b));
        s.rewind_after(&first).unwrap();
        assert_eq!(last_cwd(&s.path), None);
    }

    #[test]
    fn list_recent_reads_only_what_it_returns() {
        home();
        let cwd = Path::new("/opt/recent");
        let mut made = Vec::new();
        for _ in 0..6 {
            let mut s = SessionFile::create(cwd, None).unwrap();
            s.message(&Message::user("hello"));
            made.push(s.path.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let got = list_recent(cwd, Some(cwd), 3);
        assert_eq!(got.len(), 3);
        // Newest first, by file name.
        assert_eq!(got[0].path, made[5]);
        assert_eq!(got[2].path, made[3]);
        assert_eq!(got[0].cwd.as_deref(), Some(cwd));
        assert!(list_recent(cwd, Some(Path::new("/opt/other")), 3).is_empty());
        assert!(list_recent(cwd, None, 100).len() >= 6);
    }
}
