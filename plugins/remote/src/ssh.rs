//! The built-in `ssh` adapter: the contract's verbs as `ssh -T -o BatchMode=yes HOST mu ...` (`connect` runs
//! `mu-remote bridge` there). Every call shares one multiplexed connection (ControlMaster), which also carries
//! the port forwards.

use crate::adapter::{Start, StartAgent};
use anyhow::{Context, Result, bail};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The ssh program; `MU_SSH_PROGRAM` replaces it (tests use a fake).
pub fn program() -> String {
    std::env::var("MU_SSH_PROGRAM").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "ssh".to_string())
}

/// `s` as one word of the remote shell's command line.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The directory of the ssh control sockets: `$XDG_RUNTIME_DIR/mu-remote` when that is set and absolute, else
/// `/tmp/mu-remote-<uid>` (not `$TMPDIR`: macOS's is long and a unix socket path holds about 100 bytes). It must
/// belong to this user and be closed to everyone else.
pub fn control_dir() -> Result<PathBuf> {
    control_dir_in(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

fn control_dir_in(runtime: Option<&str>) -> Result<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir = match runtime.map(Path::new).filter(|p| p.is_absolute()) {
        Some(r) => r.join("mu-remote"),
        None => PathBuf::from(format!("/tmp/mu-remote-{uid}")),
    };
    if std::fs::symlink_metadata(&dir).is_err() {
        crate::fs::create_private_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let m = std::fs::symlink_metadata(&dir).with_context(|| dir.display().to_string())?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o777 != 0o700 {
        bail!("{} is not private to you", dir.display());
    }
    Ok(dir)
}

/// ssh's options up to the host: no prompts, and one shared connection when there is a private directory for
/// its socket.
fn options() -> Vec<String> {
    let mut o: Vec<String> = ["-T", "-o", "BatchMode=yes"].map(String::from).to_vec();
    if let Ok(dir) = control_dir() {
        o.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}/%C", dir.display()),
            "-o".into(),
            "ControlPersist=10m".into(),
        ]);
    }
    o
}

/// `remote` run under the active mu profile there: agents keep their owner records in the profile's home, so
/// `start`, `list`, `connect`, `stop` and the rest must all name the same one.
fn under_profile(profile: Option<&str>, remote: &str) -> String {
    match profile {
        Some(p) => format!("export MU_PROFILE={}; {remote}", quote(p)),
        None => remote.to_string(),
    }
}

/// ssh's arguments for running `remote` (one command line for the remote shell) on `host`, under the active
/// mu profile.
pub fn argv(host: &str, remote: &str) -> Vec<String> {
    let mut a = options();
    a.extend([host.to_string(), under_profile(crate::paths::profile().as_deref(), remote)]);
    a
}

fn forward_spec(port: u16, local: u16) -> String {
    format!("127.0.0.1:{local}:localhost:{port}")
}

/// The ssh calls that forward `127.0.0.1:LOCAL` to `localhost:PORT` on `host`: one that makes sure the master
/// connection is up (it may have expired), then the forward itself.
pub fn forward(host: &str, port: u16, local: u16) -> Result<Vec<Vec<String>>> {
    let mut add = options();
    add.extend(["-O".into(), "forward".into(), "-L".into(), forward_spec(port, local), host.to_string()]);
    Ok(vec![argv(host, "true"), add])
}

pub fn unforward(host: &str, port: u16, local: u16) -> Result<Vec<String>> {
    let mut a = options();
    a.extend(["-O".into(), "cancel".into(), "-L".into(), forward_spec(port, local), host.to_string()]);
    Ok(a)
}

pub fn read(path: &str) -> String {
    format!("cat -- {}", quote(path))
}

pub fn write(path: &str) -> String {
    let q = quote(path);
    format!("umask 077; mkdir -p -- \"$(dirname -- {q})\" && [ ! -e {q} ] && cat > {q}")
}

fn start_agent(a: &StartAgent) -> Result<String> {
    if a.bundle.is_some() {
        bail!("the ssh adapter cannot take a repository bundle");
    }
    let mut cmd = String::new();
    if let Some(dir) = &a.dir {
        cmd.push_str(&format!("cd {} && ", quote(dir)));
    }
    cmd.push_str("mu agent --detach");
    if let Some(m) = &a.model {
        cmd.push_str(&format!(" --model {}", quote(m)));
    }
    if let Some(id) = &a.resume {
        cmd.push_str(&format!(" --resume {}", quote(id)));
    } else if a.continue_latest {
        cmd.push_str(" -c");
    }
    Ok(cmd)
}

pub fn start(s: &Start) -> Result<String> {
    match s {
        Start::Agent(a) => start_agent(a),
        Start::Worker { .. } => bail!("the ssh adapter cannot run workers; use another adapter for `[task] remote`"),
    }
}

pub fn list() -> String {
    "mu agent list --json".to_string()
}

pub fn connect(id: &str) -> String {
    format!("mu-remote bridge {}", quote(id))
}

pub fn stop(id: &str) -> String {
    format!("mu agent stop {}", quote(id))
}

pub fn cancel(id: &str) -> String {
    format!("mu agent cancel {}", quote(id))
}

pub fn notify(id: &str) -> String {
    format!("mu notify {} -", quote(id))
}

/// A hint for ssh's stderr, when it says why the login failed.
pub fn hint(stderr: &str) -> Option<&'static str> {
    stderr.contains("Permission denied").then_some(
        "ssh runs with BatchMode=yes and cannot ask for a password: load your key into ssh-agent (ssh-add) or \
         set up key login for this host",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_the_remote_shell() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(connect("a;rm -rf /"), "mu-remote bridge 'a;rm -rf /'");
    }

    #[test]
    fn start_builds_one_remote_command() {
        let a = StartAgent {
            resume: Some("abc".into()),
            dir: Some("/w/my repo".into()),
            model: Some("p/m".into()),
            ..Default::default()
        };
        assert_eq!(
            start(&Start::Agent(a)).unwrap(),
            "cd '/w/my repo' && mu agent --detach --model 'p/m' --resume 'abc'"
        );
        let c = StartAgent { continue_latest: true, ..Default::default() };
        assert_eq!(start(&Start::Agent(c)).unwrap(), "mu agent --detach -c");
        let w = Start::Worker { launch: "l".into(), bundle: "/b".into(), dir: "/d".into() };
        assert!(start(&w).unwrap_err().to_string().contains("cannot run workers"));
    }

    #[test]
    fn the_active_profile_goes_to_the_remote_mu() {
        assert_eq!(under_profile(None, "mu agent list --json"), "mu agent list --json");
        assert_eq!(
            under_profile(Some("wo rk"), "cd '/w' && mu agent --detach"),
            "export MU_PROFILE='wo rk'; cd '/w' && mu agent --detach"
        );
    }

    #[test]
    fn argv_never_prompts_and_shares_one_connection() {
        let a = argv("devbox", "mu agent list --json");
        assert_eq!(a[..3], ["-T", "-o", "BatchMode=yes"]);
        assert_eq!(a[a.len() - 2..], ["devbox", "mu agent list --json"]);
        let dir = control_dir().unwrap();
        assert_eq!(
            a[3..a.len() - 2],
            [
                "-o".to_string(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}/%C", dir.display()),
                "-o".into(),
                "ControlPersist=10m".into(),
            ]
        );
    }

    #[test]
    fn forwards_go_through_the_master_and_name_localhost_as_the_far_end() {
        let calls = forward("devbox", 3000, 4001).unwrap();
        assert_eq!(calls[0], argv("devbox", "true"));
        let add = &calls[1];
        assert_eq!(add[add.len() - 5..], ["-O", "forward", "-L", "127.0.0.1:4001:localhost:3000", "devbox"]);
        let cancel = unforward("devbox", 3000, 4001).unwrap();
        assert_eq!(cancel[cancel.len() - 5..], ["-O", "cancel", "-L", "127.0.0.1:4001:localhost:3000", "devbox"]);
    }

    #[test]
    fn file_commands_quote_every_path() {
        assert_eq!(read("/a b/it's"), r"cat -- '/a b/it'\''s'");
        assert_eq!(stop("a'b"), r"mu agent stop 'a'\''b'");
        assert_eq!(cancel("a'b"), r"mu agent cancel 'a'\''b'");
        assert_eq!(notify("a'b"), r"mu notify 'a'\''b' -");
        assert_eq!(write("/a b"), "umask 077; mkdir -p -- \"$(dirname -- '/a b')\" && [ ! -e '/a b' ] && cat > '/a b'");
    }

    #[test]
    fn the_control_directory_is_private_or_refused() {
        use std::os::unix::fs::PermissionsExt;
        let t = crate::testutil::tmp();
        let root = t.to_string_lossy().into_owned();
        let dir = control_dir_in(Some(root.as_str())).unwrap();
        assert_eq!(dir, t.join("mu-remote"));
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = control_dir_in(Some(root.as_str())).unwrap_err().to_string();
        assert!(e.contains("is not private to you"), "{e}");
    }
}
