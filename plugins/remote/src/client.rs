//! `mu-remote [NAME] [--resume ID | -c] [--model M]`: picks or starts an agent on the remote, then runs
//! `mu --attach 'mu-remote connect NAME ID' --state-key …`.

use crate::adapter::{Adapter, AgentInfo, Start, StartAgent};
use crate::config::Config;
use crate::ssh::quote;
use anyhow::{Context, Result, bail};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_WORK_DIR: &str = "/work";

/// Which session a run asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Want {
    New,
    /// The latest agent there, else the latest session for the directory `start` runs in.
    Latest,
    /// An agent whose id starts with this, else a session by that id.
    Resume(String),
}

#[derive(Debug, PartialEq)]
pub enum Plan {
    Connect(String),
    Start(StartAgent),
}

/// The agents there that a client can join, oldest first.
pub fn agents(adapter: &Adapter) -> Result<Vec<AgentInfo>> {
    let mut found: Vec<AgentInfo> = adapter.list(Some(LIST_TIMEOUT))?.into_iter().filter(AgentInfo::is_agent).collect();
    found.sort_by_key(|a| a.started);
    Ok(found)
}

pub fn plan(want: &Want, agents: &[AgentInfo], model: Option<&str>) -> Result<Plan> {
    let start = StartAgent { model: model.map(str::to_string), ..Default::default() };
    Ok(match want {
        Want::New => Plan::Start(start),
        Want::Latest => match agents.last() {
            Some(a) => Plan::Connect(a.id.clone()),
            None => Plan::Start(StartAgent { continue_latest: true, ..start }),
        },
        Want::Resume(id) => {
            let hits: Vec<&AgentInfo> = agents.iter().filter(|a| a.id.starts_with(id.as_str())).collect();
            match hits.as_slice() {
                [] => Plan::Start(StartAgent { resume: Some(id.clone()), ..start }),
                [a] => Plan::Connect(a.id.clone()),
                _ => bail!(
                    "several agents match {id}: {}",
                    hits.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    })
}

/// A session id from what `--resume` was given: an id, or a session file named `<timestamp>_<id>.jsonl`.
fn id_of(p: &Path) -> String {
    let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.rsplit_once('_').map_or(name.clone(), |(_, id)| id.to_string())
}

/// The `mu --attach` command line (program and arguments) that joins agent `id` on remote `name`.
fn attach_args(exe: &Path, name: &str, id: &str, state_key: &Path) -> Vec<String> {
    let connect = format!("{} connect {} {}", quote(&exe.to_string_lossy()), quote(name), quote(id));
    vec!["--attach".into(), connect, "--state-key".into(), state_key.to_string_lossy().into_owned()]
}

/// The shell command line mu runs (`MU_ATTACH_HELPER`) to forward ports and move files for agent `id`.
fn helper_command(exe: &Path, name: &str, id: &str) -> String {
    format!("{} helper {} {}", quote(&exe.to_string_lossy()), quote(name), quote(id))
}

/// Starts the agent `s` describes and returns its id. A new session gets a bundle of the repository in the
/// current directory when the adapter takes one.
fn start(adapter: &Adapter, mut s: StartAgent) -> Result<String> {
    let staged = if adapter.takes_bundle() && s.resume.is_none() && !s.continue_latest {
        let base = adapter.setting("work_dir").unwrap_or_else(|| DEFAULT_WORK_DIR.to_string());
        let cwd = std::env::current_dir()?;
        stage_bundle(&cwd, &base, &std::env::temp_dir()).context("could not bundle the repository")?
    } else {
        None
    };
    if let Some(b) = &staged {
        eprintln!("mu-remote: sending {} to {}…", b.repo, adapter.name());
        s.bundle = Some(b.file());
        s.dir = Some(b.dir.clone());
    }
    adapter.start(&Start::Agent(s), &mut |p| eprintln!("mu-remote: {p}…"))
}

/// `mu-remote new NAME [--model M]`: starts an agent on the remote and prints a session-source row for it
/// (sources contract 2: what `/remote new` attaches with). Progress goes to stderr.
pub fn new_agent(name: &str, model: Option<String>) -> Result<i32> {
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(Some(name))?;
    let adapter = Adapter::from_config(name, remote)?;
    if !adapter.trusted() {
        return Err(adapter.untrusted_agent());
    }
    let id = start(&adapter, StartAgent { model, ..Default::default() })?;
    println!("{}", crate::rows::started_row(name, remote, &adapter, &id)?);
    Ok(0)
}

/// Joins or starts an agent on the remote and hands the terminal to `mu --attach`. A remote that does not
/// answer is refused with the adapter's error; nothing falls back to running here.
pub fn run(name: Option<String>, resume: Option<PathBuf>, continue_latest: bool, model: Option<String>) -> Result<i32> {
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(name.as_deref())?;
    let adapter = Adapter::from_config(name, remote)?;
    if !adapter.trusted() {
        return Err(adapter.untrusted_agent());
    }
    let found = agents(&adapter)?;
    let want = match (continue_latest, resume) {
        (true, _) => Want::Latest,
        (false, Some(p)) => Want::Resume(id_of(&p)),
        (false, None) => Want::New,
    };
    let id = match plan(&want, &found, model.as_deref())? {
        Plan::Connect(id) => id,
        Plan::Start(s) => start(&adapter, s)?,
    };
    let exe = std::env::current_exe().context("cannot find this program's path")?;
    let args = attach_args(&exe, adapter.name(), &id, &adapter.state_key_path());
    let program = crate::paths::mu_program();
    let err = std::process::Command::new(&program)
        .args(args)
        .env("MU_ATTACH_HELPER", helper_command(&exe, adapter.name(), &id))
        .exec();
    bail!("cannot run {program}: {err}")
}

/// The repository's bundle in a private temp directory, removed when this is dropped.
#[derive(Debug)]
pub struct StagedBundle {
    tmp: PathBuf,
    pub repo: String,
    /// Where the remote clones it.
    pub dir: String,
}

impl StagedBundle {
    pub fn file(&self) -> PathBuf {
        self.tmp.join("repo.bundle")
    }
}

impl Drop for StagedBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A bundle of the repository `cwd` is in (full history: HEAD and its branch), for `start --dir
/// <base>/<repo>`. `None` outside a git repository or in one without a commit. The bundle holds no
/// working-tree changes and no config of the repository.
pub fn stage_bundle(cwd: &Path, base: &str, temp: &Path) -> Result<Option<StagedBundle>> {
    let Some(top) = git(cwd, &["rev-parse", "--show-toplevel"]) else { return Ok(None) };
    if git(cwd, &["rev-parse", "--verify", "-q", "HEAD"]).is_none() {
        return Ok(None);
    }
    let name: String = Path::new(&top)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let name = name.trim_matches(['.', '-']);
    let repo = if name.is_empty() { "repo" } else { name }.to_string();
    let tmp = temp.join(format!("mu-remote-bundle-{}", uuid::Uuid::new_v4().simple()));
    crate::fs::create_private_dir(&tmp)?;
    let staged = StagedBundle { tmp, dir: format!("{}/{repo}", base.trim_end_matches('/')), repo };
    let mut args = vec!["bundle", "create", "-q"];
    let file = staged.file();
    let file = file.to_string_lossy();
    args.push(&file);
    args.push("HEAD");
    let branch = git(cwd, &["symbolic-ref", "-q", "HEAD"]);
    if let Some(b) = &branch {
        args.push(b);
    }
    // No timeout: a long history takes as long as it takes.
    let out =
        std::process::Command::new("git").args(&args).current_dir(cwd).stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git bundle: {}", err.lines().next().unwrap_or("failed").trim());
    }
    Ok(Some(staged))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{sh, tmp};

    #[test]
    fn the_bundle_holds_head_and_branch_with_history_and_is_cleaned_up() {
        let t = tmp();
        let repo = t.join("my repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "1").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-q", "-m", "one"]);
        std::fs::write(repo.join("a"), "2").unwrap();
        sh(&repo, &["commit", "-q", "-am", "two"]);
        std::fs::write(repo.join("untracked"), "x").unwrap();
        let staged = stage_bundle(&repo, "/work/", &t).unwrap().unwrap();
        assert_eq!((staged.repo.as_str(), staged.dir.as_str()), ("my-repo", "/work/my-repo"));
        let heads = sh(&t, &["bundle", "list-heads", &staged.file().to_string_lossy()]);
        assert!(heads.contains("HEAD") && heads.contains("refs/heads/main"), "{heads}");
        let clone = t.join("clone");
        sh(&t, &["clone", "-q", &staged.file().to_string_lossy(), &clone.to_string_lossy()]);
        assert_eq!(std::fs::read_to_string(clone.join("a")).unwrap(), "2");
        assert!(!clone.join("untracked").exists());
        let tmp = staged.tmp.clone();
        drop(staged);
        assert!(!tmp.exists());
    }

    #[test]
    fn no_bundle_outside_a_repository_or_without_a_commit() {
        let t = tmp();
        assert!(stage_bundle(&t, "/work", &t).unwrap().is_none());
        sh(&t, &["init", "-q"]);
        assert!(stage_bundle(&t, "/work", &t).unwrap().is_none());
    }

    fn agent(id: &str, started: i64) -> AgentInfo {
        AgentInfo { id: id.into(), cwd: "/w".into(), started, ..Default::default() }
    }

    #[test]
    fn a_new_session_starts_an_agent_with_the_model() {
        let p = plan(&Want::New, &[agent("a1", 1)], Some("p/m")).unwrap();
        assert_eq!(p, Plan::Start(StartAgent { model: Some("p/m".into()), ..Default::default() }));
    }

    #[test]
    fn continue_joins_the_latest_agent_or_starts_one_that_continues() {
        let two = [agent("old", 1), agent("new", 9)];
        assert_eq!(plan(&Want::Latest, &two, None).unwrap(), Plan::Connect("new".into()));
        let p = plan(&Want::Latest, &[], None).unwrap();
        assert_eq!(p, Plan::Start(StartAgent { continue_latest: true, ..Default::default() }));
    }

    #[test]
    fn resume_joins_the_agent_that_has_the_session_else_starts_one() {
        let two = [agent("abc1", 1), agent("abd2", 2)];
        assert_eq!(plan(&Want::Resume("abd".into()), &two, None).unwrap(), Plan::Connect("abd2".into()));
        let e = plan(&Want::Resume("ab".into()), &two, None).unwrap_err().to_string();
        assert_eq!(e, "several agents match ab: abc1, abd2");
        let p = plan(&Want::Resume("zzz".into()), &two, None).unwrap();
        assert_eq!(p, Plan::Start(StartAgent { resume: Some("zzz".into()), ..Default::default() }));
    }

    #[test]
    fn a_session_file_name_gives_its_id() {
        assert_eq!(id_of(Path::new("abc1")), "abc1");
        assert_eq!(id_of(Path::new("/s/20260101T000000_abc1.jsonl")), "abc1");
    }

    #[test]
    fn the_attach_command_quotes_the_names_for_sh() {
        let a = attach_args(Path::new("/o pt/mu-remote"), "far", "a'b", Path::new("/k/id"));
        assert_eq!(a[0], "--attach");
        assert_eq!(a[1], r"'/o pt/mu-remote' connect 'far' 'a'\''b'");
        assert_eq!(&a[2..], ["--state-key", "/k/id"]);
    }

    #[test]
    fn the_helper_command_quotes_the_names_for_sh() {
        let h = helper_command(Path::new("/o pt/mu-remote"), "far", "a'b");
        assert_eq!(h, r"'/o pt/mu-remote' helper 'far' 'a'\''b'");
    }
}
