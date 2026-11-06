//! Forking a session: a new file holding a copy of the active chain, so the fork carries on from
//! where the original is while the original stays as it was.
use super::file::SessionFile;
use super::format::{TitleSource, chain_lines, load};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Copies the active chain of `src` into a new session file for `cwd` and returns its path. The
/// new file is closed (and unlocked) again, ready to be opened. Nothing is left behind on failure.
pub fn fork(src: &Path, cwd: &Path) -> Result<PathBuf> {
    let chain = chain_lines(src)?;
    if chain.is_empty() {
        bail!("nothing to fork: the session has no entries yet");
    }
    let title = load(src)?.title;
    let mut out = SessionFile::create_fork(cwd, src)?;
    let path = out.path.clone();
    let copied = (|| -> Result<()> {
        let mut rd = BufReader::new(File::open(src).with_context(|| format!("read {}", src.display()))?);
        let mut line = Vec::new();
        for (id, parent, offset) in &chain {
            rd.seek(SeekFrom::Start(*offset))?;
            line.clear();
            rd.read_until(b'\n', &mut line)?;
            out.copy_entry(&line, id, parent.as_deref()).with_context(|| format!("write {}", path.display()))?;
        }
        Ok(())
    })();
    if let Err(e) = copied {
        drop(out);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    if let Some(t) = title {
        out.set_title(&t, TitleSource::User);
    }
    out.sync();
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::message::Message;
    use mu_testutil::home;
    use serde_json::json;

    fn texts(path: &Path) -> Vec<String> {
        load(path)
            .unwrap()
            .messages
            .iter()
            .map(|m| match m {
                Message::User { content, .. } => mu_base::message::text_of(content),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn a_fork_copies_the_active_chain_and_not_the_abandoned_branch() {
        home();
        let cwd = Path::new("/opt/fork");
        let mut s = SessionFile::create(cwd, None).unwrap();
        s.message(&Message::user("one"));
        let two = s.message(&Message::user("two")).unwrap();
        s.message(&Message::user("three"));
        s.rewind_before(&two).unwrap();
        s.message(&Message::user("two again"));
        s.set_title("Plan", TitleSource::User);
        let src = s.path.clone();

        let forked = fork(&src, cwd).unwrap();
        assert_ne!(forked, src);
        assert_eq!(texts(&forked), ["one", "two again"]);
        assert_eq!(load(&forked).unwrap().title.as_deref(), Some("Plan"));
        let header: serde_json::Value =
            std::fs::read_to_string(&forked).unwrap().lines().nth(1).map(|l| serde_json::from_str(l).unwrap()).unwrap();
        assert_eq!(header["forkedFrom"], json!(src.to_string_lossy()));
        assert!(header.get("parentSession").is_none(), "a fork is not a subagent");

        // The original is untouched, and the fork opens and grows on its own.
        assert_eq!(texts(&src), ["one", "two again"]);
        let mut f = SessionFile::open_append(&forked).unwrap();
        f.message(&Message::user("only in the fork"));
        assert_eq!(texts(&forked), ["one", "two again", "only in the fork"]);
        assert_eq!(texts(&src), ["one", "two again"]);
    }

    #[test]
    fn forking_an_empty_session_is_refused() {
        home();
        let s = SessionFile::create(Path::new("/opt/fork-empty"), None).unwrap();
        let e = fork(&s.path, Path::new("/opt/fork-empty")).unwrap_err().to_string();
        assert!(e.contains("nothing to fork"), "{e}");
    }
}
