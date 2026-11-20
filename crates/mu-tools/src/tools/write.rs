use super::uri::{Scheme, physical};
use super::{Tools, diff};
use mu_base::tool::{CHANGE_TEXT_MAX, FileChange, FileMeta, ToolMeta, ToolOutput};
use omp_edit::{path_policy::canonical_key, text};
use serde::Deserialize;
use std::borrow::Cow;
use std::io::Read;
use std::path::Path;

#[derive(Deserialize)]
pub struct Args {
    path: String,
    content: String,
}

impl Args {
    pub(crate) fn paths(&self) -> Vec<String> {
        vec![self.path.clone()]
    }
}

impl Tools {
    pub async fn write(&self, a: Args, plan: bool) -> ToolOutput {
        let res = match self.resolve(&a.path) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(s @ (Scheme::Artifact | Scheme::Skill | Scheme::Mu)) = res.scheme {
            return ToolOutput::err(format!("{}:// is read-only", s.name()));
        }
        if plan && res.scheme != Some(Scheme::Local) {
            return ToolOutput::err(
                "Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.",
            );
        }
        let abs = res.path;
        let disp = if res.scheme.is_some() { a.path.clone() } else { self.display(&abs) };
        // A symlink is written through, so the link survives the rename that ends the write; a
        // dangling link is written through to where it points. The gate judged this same file.
        let target = physical(&abs);
        // The filesystem work runs on a blocking thread; `content` moves in and comes back.
        let (path, content) = (abs.clone(), a.content);
        let done = tokio::task::spawn_blocking(move || -> Result<(Option<String>, bool, String), String> {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
            }
            let (old, too_big, crlf) = match std::fs::metadata(&target) {
                Ok(m) if m.len() <= CHANGE_TEXT_MAX as u64 => {
                    let bytes = std::fs::read(&target).ok();
                    let crlf = bytes.as_deref().is_some_and(starts_crlf);
                    (bytes.and_then(|b| String::from_utf8(b).ok()), false, crlf)
                }
                Ok(_) => (None, true, starts_crlf(&head(&target))),
                Err(_) => (None, false, false),
            };
            let on_disk: Cow<str> = if crlf && !content.contains('\r') {
                Cow::Owned(content.replace('\n', "\r\n"))
            } else {
                Cow::Borrowed(&content)
            };
            mu_common::fs::write_atomic(&target, on_disk.as_bytes())
                .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
            Ok((old, too_big, content))
        })
        .await;
        let (old, too_big, content) = match done {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::err(e.replace(&abs.display().to_string(), &disp)),
            Err(e) => return ToolOutput::err(format!("write failed: {e}")),
        };
        let a = Args { path: a.path, content };
        let norm = text::normalize_to_lf(text::strip_bom(&a.content).1).into_owned();
        let tag = self.edit_store.record(&canonical_key(&abs), &norm, Some(&[]));
        let mut out = ToolOutput::ok(format!("[{disp}#{tag}]\nSuccessfully wrote {} bytes to {disp}", a.content.len()));
        let plan = a.path.starts_with("local://") && a.path.ends_with("-plan.md");
        out.meta = Some(ToolMeta::File(FileMeta { path: disp.clone(), tag: Some(tag), plan }));
        // A large old file was not read, so there is no text to diff against.
        if !too_big {
            let d = diff::unified(old.as_deref().unwrap_or_default(), &a.content);
            if !d.is_empty() {
                out.display = Some(d);
            }
            if old.as_ref().is_none_or(|o| o.len() <= CHANGE_TEXT_MAX) && a.content.len() <= CHANGE_TEXT_MAX {
                out.changes = vec![FileChange { path: abs, old, new: Some(a.content) }];
            }
        }
        out
    }
}

/// Whether the first line of `bytes` ends in CRLF.
fn starts_crlf(bytes: &[u8]) -> bool {
    bytes.iter().position(|&b| b == b'\n').is_some_and(|i| i > 0 && bytes[i - 1] == b'\r')
}

/// The first 8 KiB of the file at `path` (empty if it cannot be read).
fn head(path: &Path) -> Vec<u8> {
    let mut head = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(8192).read_to_end(&mut head);
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_common::paths::Home;
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tools(d: &Path) -> Tools {
        let mut t = Tools::for_test();
        t.cwd = d.to_path_buf();
        t
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_crlf_file_stays_crlf() {
        let d = dir();
        let p = d.join("win.txt");
        std::fs::write(&p, "a\r\nb\r\n").unwrap();
        let t = tools(&d);
        let o = t.write(Args { path: p.display().to_string(), content: "x\ny\n".into() }, false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read(&p).unwrap(), b"x\r\ny\r\n");
        // The model's text, and the diff, stay LF.
        assert_eq!(o.changes[0].new.as_deref(), Some("x\ny\n"));
        // Content that brings its own CR is written as it is.
        let o = t.write(Args { path: p.display().to_string(), content: "p\r\nq\n".into() }, false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read(&p).unwrap(), b"p\r\nq\n");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn writing_through_a_symlink_keeps_the_link() {
        let d = dir();
        let real = d.join("real.txt");
        std::fs::write(&real, "old\n").unwrap();
        let link = d.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let t = tools(&d);
        let o = t.write(Args { path: link.display().to_string(), content: "new\n".into() }, false).await;
        assert!(!o.is_error, "{}", o.text);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn an_existing_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        let p = d.join("run.sh");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = tools(&d);
        let o = t.write(Args { path: p.display().to_string(), content: "#!/bin/sh\necho hi\n".into() }, false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "#!/bin/sh\necho hi\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_tilde_path_writes_under_home() {
        let d = dir();
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut t = tools(&d);
        t.home = Home::at(&home);
        let o = t.write(Args { path: "~/sub/note.txt".into(), content: "hi\n".into() }, false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read_to_string(home.join("sub/note.txt")).unwrap(), "hi\n");
    }
}
