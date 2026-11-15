//! Plain `mu` with a remote in effect: which agent to join, starting one when needed, and the rows the
//! Sessions tab shows for the default remote's agents.

use super::adapter::{Adapter, AgentInfo, Start, StartAgent};
use super::{Selection, resolve};
use crate::run::attach::{self, Link};
use anyhow::{Result, bail};
use mu_base::config::Configs;
use mu_session::session::list::id_of;
use mu_session::session::owner::{ClientInfo, LiveSession};
use mu_tui::host::sessions::{LiveRow, RemoteRows};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// The adapter of the remote this run goes to, or `None` when it runs here.
pub fn in_effect(remote: Option<&str>, local: bool, cfgs: &Configs) -> Result<Option<Arc<Adapter>>> {
    for w in cfgs.warnings.iter().filter(|w| w.contains("remote")) {
        eprintln!("{w}");
    }
    let r = &cfgs.mu.remote;
    match resolve(remote, local, cfgs.mu.default_remote.as_deref(), r)? {
        Selection::Local => Ok(None),
        Selection::Remote(name) => Ok(Some(Arc::new(Adapter::from_config(&name, &r[&name])?))),
    }
}

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

#[derive(Debug, PartialEq)]
enum Choice {
    Agent(String),
    New,
    Quit,
}

fn agent_line(n: usize, a: &AgentInfo) -> String {
    let mut line = format!("{n}) {}  {}", a.id, a.cwd);
    if let Some(t) = &a.title {
        line.push_str(&format!("  {t}"));
    }
    if let Some(c) = &a.client {
        line.push_str(&format!("  [attached from {}]", c.host));
    }
    if a.waiting {
        line.push_str("  [waiting for you]");
    }
    line
}

fn parse_choice(input: &str, agents: &[AgentInfo]) -> Option<Choice> {
    let t = input.trim();
    match t {
        "" | "q" | "Q" => Some(Choice::Quit),
        "n" | "N" => Some(Choice::New),
        _ => t
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| agents.get(i))
            .map(|a| Choice::Agent(a.id.clone())),
    }
}

/// Asks, before the UI is up, which agent to join. Several agents or none: a new agent then opens the
/// picker over the saved sessions there.
fn choose(agents: &[AgentInfo]) -> Result<Choice> {
    if agents.is_empty() {
        return Ok(Choice::New);
    }
    if !std::io::stdin().is_terminal() {
        bail!("mu: --resume without a session needs a terminal to ask on; pass a session id");
    }
    for (i, a) in agents.iter().enumerate() {
        eprintln!("{}", agent_line(i + 1, a));
    }
    loop {
        eprint!("join which agent? number, [n]ew agent (pick a saved session), [q]uit: ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().lock().read_line(&mut input)? == 0 {
            return Ok(Choice::Quit);
        }
        if let Some(c) = parse_choice(&input, agents) {
            return Ok(c);
        }
    }
}

/// Plain `mu` on `adapter`'s remote: joins or starts an agent there and runs the UI over `connect`.
/// `resume` is the `--resume` flag (`Some(None)` picks). A remote that does not answer is refused with the
/// adapter's error; it never falls back to running here.
pub fn run(
    adapter: Arc<Adapter>,
    resume: Option<Option<PathBuf>>,
    continue_latest: bool,
    model: Option<String>,
) -> Result<i32> {
    if !adapter.trusted() {
        return Err(adapter.untrusted_agent());
    }
    let found = agents(&adapter)?;
    if !std::io::stdout().is_terminal() {
        bail!("mu: interactive mode needs a terminal; use -p");
    }
    let (want, pick) = match (continue_latest, resume) {
        (true, _) => (Want::Latest, false),
        (false, Some(Some(p))) => (Want::Resume(id_of(&p)), false),
        (false, Some(None)) => match choose(&found)? {
            Choice::Quit => return Ok(0),
            Choice::Agent(id) => (Want::Resume(id), false),
            Choice::New => (Want::New, true),
        },
        (false, None) => (Want::New, false),
    };
    let id = match plan(&want, &found, model.as_deref())? {
        Plan::Connect(id) => id,
        Plan::Start(s) => adapter.start(&Start::Agent(s), &mut |p| eprintln!("mu: {p}…"))?,
    };
    attach::run(Link::Remote { adapter, id }, pick)
}

fn row(remote: &str, a: AgentInfo) -> LiveRow {
    let started = jiff::Timestamp::from_second(a.started).map(|t| t.to_string()).unwrap_or_default();
    LiveRow {
        live: LiveSession {
            path: Path::new("remote").join(remote).join(format!("{}.jsonl", a.id)),
            pid: 0,
            print_run: false,
            inbox: false,
            started,
            kind: Some("agent".into()),
            agent: None,
            cwd: Some(a.cwd.into()),
            parent: None,
            abort: false,
            waiting: a.waiting,
            client: a.client.map(|c| ClientInfo { host: c.host, since: c.since }),
        },
        parent: None,
        modified: None,
        title: a.title,
        host: Some(remote.to_string()),
    }
}

/// What the Sessions tab lists beside this host's sessions: the agents of `default_remote`, when this
/// run is local (`--local`) and the remote is one a main agent may run on. A remote that does not answer
/// adds no rows.
pub fn tab_rows(cfgs: &Configs) -> Option<RemoteRows> {
    let name = cfgs.mu.default_remote.clone()?;
    let adapter = Adapter::from_config(&name, cfgs.mu.remote.get(&name)?).ok().filter(Adapter::trusted)?;
    Some(Box::new(move || {
        agents(&adapter).map(|found| found.into_iter().map(|a| row(&name, a)).collect()).unwrap_or_default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::adapter::ClientInfo as Attached;

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
    fn the_choice_is_a_number_new_or_quit() {
        let two = [agent("abc1", 1), agent("abd2", 2)];
        assert_eq!(parse_choice("2\n", &two), Some(Choice::Agent("abd2".into())));
        assert_eq!(parse_choice(" n ", &two), Some(Choice::New));
        assert_eq!(parse_choice("\n", &two), Some(Choice::Quit));
        assert_eq!(parse_choice("3", &two), None);
        assert_eq!(parse_choice("0", &two), None);
        assert_eq!(parse_choice("x", &two), None);
    }

    #[test]
    fn a_listed_agent_says_who_has_it_and_whether_it_waits() {
        let a = AgentInfo {
            title: Some("fix it".into()),
            client: Some(Attached { host: "laptop".into(), since: 5 }),
            waiting: true,
            ..agent("abc1", 1)
        };
        assert_eq!(agent_line(1, &a), "1) abc1  /w  fix it  [attached from laptop]  [waiting for you]");
    }

    #[test]
    fn the_sessions_tab_row_names_the_remote_and_keeps_the_agents_facts() {
        let a = AgentInfo {
            client: Some(Attached { host: "laptop".into(), since: 5 }),
            waiting: true,
            ..agent("abc1", 1_767_225_600)
        };
        let r = row("cluster", a);
        assert_eq!(r.host.as_deref(), Some("cluster"));
        assert_eq!(r.live.started, "2026-01-01T00:00:00Z");
        assert!(r.live.waiting && r.live.client.as_ref().is_some_and(|c| c.host == "laptop"));
        assert_eq!(id_of(&r.live.path), "abc1");
    }
}
