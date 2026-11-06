//! `mu migrate-profile <name>`: moves the state older mu kept in `$MU_HOME` itself for every profile
//! (`paths::PROFILE_STATE`) under `$MU_HOME/profiles/<name>/`, where that profile now looks for it.
use mu_common::fs::write_atomic;
use mu_common::paths::{PROFILE_STATE, mu_home, profile_subdir, valid_profile_name};
use mu_session::session::owner::live_sessions_in;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Report {
    /// Per `PROFILE_STATE` entry found: how it was handled.
    lines: Vec<String>,
    /// Files left where they were because the target already holds a different file of that name.
    kept: Vec<PathBuf>,
    errors: Vec<String>,
    rewritten: usize,
}

pub fn run(name: &str, dry_run: bool) -> anyhow::Result<i32> {
    if !valid_profile_name(name) {
        eprintln!("mu migrate-profile: invalid profile name `{name}`");
        return Ok(2);
    }
    let from = mu_home();
    let live = live_sessions_in(&from.join("run"));
    if !live.is_empty() {
        let pids: Vec<String> = live.iter().map(|s| s.pid.to_string()).collect();
        let what = format!("{} session(s) in {} are still open (pid {})", live.len(), from.display(), pids.join(", "));
        if !dry_run {
            eprintln!("mu migrate-profile: {what}; close them first");
            return Ok(1);
        }
        eprintln!("note: {what}; the real run refuses until they are closed");
    }
    let to = profile_subdir(from.clone(), Some(name));
    let r = migrate(&from, &to, dry_run);
    let verb = if dry_run { "would move" } else { "moved" };
    if r.lines.is_empty() {
        println!("nothing to migrate in {}", from.display());
    } else {
        println!("{verb} into {}:", to.display());
        for l in &r.lines {
            println!("  {l}");
        }
        if r.rewritten > 0 {
            let verb = if dry_run { "would rewrite" } else { "rewrote" };
            println!("{verb} session paths in {} file(s)", r.rewritten);
        }
    }
    for k in &r.kept {
        eprintln!("left in place (a different file exists in the profile): {}", k.display());
    }
    for e in &r.errors {
        eprintln!("mu migrate-profile: {e}");
    }
    Ok(if r.kept.is_empty() && r.errors.is_empty() { 0 } else { 1 })
}

fn migrate(from: &Path, to: &Path, dry_run: bool) -> Report {
    let mut r = Report::default();
    for item in PROFILE_STATE {
        let src = from.join(item);
        if std::fs::symlink_metadata(&src).is_err() {
            continue;
        }
        let dst = to.join(item);
        let mut n = Counts::default();
        move_into(&src, &dst, dry_run, &mut n, &mut r);
        r.lines.push(if n.merged == 0 && n.moved == 1 {
            item.to_string()
        } else {
            format!("{item}: {} moved, {} merged", n.moved, n.merged)
        });
    }
    // Session headers and plan/delivery records name other sessions by absolute path (parentSession,
    // forkedFrom, session); point them at the sessions' new home so links survive the move.
    let old = format!("{}/", from.join("sessions").display());
    let new = format!("{}/", to.join("sessions").display());
    let roots: &[&Path] = if dry_run { &[from, to] } else { &[to] };
    for root in roots {
        for dir in ["sessions", "plans", "deliveries"] {
            r.rewritten += rewrite_paths(&root.join(dir), &old, &new, dry_run, &mut r.errors);
        }
    }
    r
}

#[derive(Default)]
struct Counts {
    moved: usize,
    merged: usize,
}

/// Moves `src` to `dst`; when `dst` exists, merges directories entry by entry. A file already in the
/// profile wins: an identical copy is dropped, prompt history is joined (older lines first), and any
/// other clash is left in place and reported.
fn move_into(src: &Path, dst: &Path, dry_run: bool, n: &mut Counts, r: &mut Report) {
    let Ok(dst_meta) = std::fs::symlink_metadata(dst) else {
        n.moved += 1;
        if !dry_run {
            let res = dst.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::rename(src, dst));
            if let Err(e) = res {
                r.errors.push(format!("cannot move {} to {}: {e}", src.display(), dst.display()));
            }
        }
        return;
    };
    let src_dir = std::fs::symlink_metadata(src).is_ok_and(|m| m.is_dir());
    if src_dir && dst_meta.is_dir() {
        let Ok(rd) = std::fs::read_dir(src) else {
            r.errors.push(format!("cannot read {}", src.display()));
            return;
        };
        for e in rd.flatten() {
            move_into(&e.path(), &dst.join(e.file_name()), dry_run, n, r);
        }
        if !dry_run {
            let _ = std::fs::remove_dir(src);
        }
        return;
    }
    if src_dir || dst_meta.is_dir() {
        r.kept.push(src.to_path_buf());
        return;
    }
    let (Ok(a), Ok(b)) = (std::fs::read(src), std::fs::read(dst)) else {
        r.errors.push(format!("cannot read {} or {}", src.display(), dst.display()));
        return;
    };
    let joined = if a == b {
        None
    } else if src.file_name().is_some_and(|f| f == "history.jsonl") {
        let mut v = a;
        if !v.is_empty() && !v.ends_with(b"\n") {
            v.push(b'\n');
        }
        v.extend_from_slice(&b);
        Some(v)
    } else {
        r.kept.push(src.to_path_buf());
        return;
    };
    n.merged += 1;
    if dry_run {
        return;
    }
    if let Some(v) = joined
        && let Err(e) = write_atomic(dst, &v)
    {
        r.errors.push(format!("cannot write {}: {e}", dst.display()));
        return;
    }
    if let Err(e) = std::fs::remove_file(src) {
        r.errors.push(format!("cannot remove {}: {e}", src.display()));
    }
}

/// Replaces `old` with `new` in every `.jsonl`/`.json` file under `dir`; returns how many files hold `old`.
fn rewrite_paths(dir: &Path, old: &str, new: &str, dry_run: bool, errors: &mut Vec<String>) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if m.is_dir() {
            n += rewrite_paths(&p, old, new, dry_run, errors);
            continue;
        }
        if !p.extension().is_some_and(|x| x == "jsonl" || x == "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        if !text.contains(old) {
            continue;
        }
        n += 1;
        if !dry_run && let Err(e) = write_atomic(&p, text.replace(old, new).as_bytes()) {
            errors.push(format!("cannot rewrite {}: {e}", p.display()));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn moves_merges_and_rewrites_session_links() {
        let home = mu_testutil::tmp();
        let to = home.join("profiles/private");
        let parent = home.join("sessions/-code-x/1_parent.jsonl");
        write(&parent, "{\"type\":\"session\",\"version\":3,\"id\":\"parent\"}\n");
        let child = home.join("sessions/-code-x/2_child.jsonl");
        write(&child, &format!("{{\"type\":\"session\",\"parentSession\":\"{}\"}}\n", parent.display()));
        write(&home.join("plans/-code-x/p.json"), &format!("{{\"session\":\"{}\"}}", child.display()));
        write(&home.join("history.jsonl"), "old\n");
        write(&home.join("blobs/aa"), "same");
        write(&home.join("inbox/s/m.json"), "theirs");
        write(&home.join("config.toml"), "x = 1\n");
        // state a profile already has
        write(&to.join("history.jsonl"), "new\n");
        write(&to.join("blobs/aa"), "same");
        write(&to.join("inbox/s/m.json"), "mine");
        write(&to.join("sessions/-code-x/3_newer.jsonl"), "{}\n");

        let dry = migrate(&home, &to, true);
        assert!(child.is_file() && !to.join("plans").exists(), "a dry run changes nothing");
        assert_eq!(dry.rewritten, 2);

        let r = migrate(&home, &to, false);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.kept, vec![home.join("inbox/s/m.json")]);
        assert_eq!(r.rewritten, 2);
        let moved_parent = to.join("sessions/-code-x/1_parent.jsonl");
        let moved_child = read(&to.join("sessions/-code-x/2_child.jsonl"));
        assert!(moved_child.contains(&moved_parent.display().to_string()), "{moved_child}");
        assert!(read(&to.join("plans/-code-x/p.json")).contains(&to.join("sessions").display().to_string()));
        assert!(to.join("sessions/-code-x/3_newer.jsonl").is_file());
        assert_eq!(read(&to.join("history.jsonl")), "old\nnew\n");
        assert_eq!(read(&to.join("blobs/aa")), "same");
        assert!(
            !home.join("blobs").exists() && !home.join("sessions").exists() && !home.join("history.jsonl").exists()
        );
        assert_eq!(read(&to.join("inbox/s/m.json")), "mine");
        assert_eq!(read(&home.join("inbox/s/m.json")), "theirs");
        assert!(home.join("config.toml").is_file(), "install-wide files stay");

        let again = migrate(&home, &to, false);
        assert_eq!(again.rewritten, 0, "a second run finds nothing to rewrite");
    }
}
