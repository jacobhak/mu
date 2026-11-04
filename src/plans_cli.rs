//! `mu plan ...`: the command line over the plan store in `mu_agent::plans`. `new`, `refine` and `approve`
//! run through headless mode (`plan_run`); `list`, `show` and `drop` print from the store (`run_cli`).
use mu_agent::plans::{drift, store_for};
use std::path::Path;

#[derive(clap::Subcommand)]
pub enum PlanCmd {
    /// List this repository's plans (pending only unless --all)
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a plan, its metadata and drift
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Plan a brief in plan mode without a user and park the result
    New { brief: String },
    /// Revise a parked plan headless and park it again
    Refine { id: String, text: Option<String> },
    /// Approve a parked plan and run it headless in this directory
    Approve {
        id: String,
        #[arg(long)]
        anyway: bool,
    },
    /// Drop a parked plan
    Drop { id: String },
}

/// How `mu plan new|refine|approve` runs through `headless::print_mode`.
pub struct PlanRun {
    pub first: mu_base::events::Command,
    pub expect_park: bool,
    /// Appended to the system prompt.
    pub note: Option<&'static str>,
}

/// `Err` gives back the store commands (`list`, `show`, `drop`) for `run_cli`.
pub fn plan_run(cmd: PlanCmd) -> Result<PlanRun, PlanCmd> {
    use mu_base::events::{Command, PlanDecision};
    match cmd {
        PlanCmd::New { brief } => Ok(PlanRun {
            first: Command::TogglePlan(Some(brief)),
            expect_park: true,
            note: Some(mu_agent::prompt::HEADLESS_NOTE),
        }),
        PlanCmd::Refine { id, text } => Ok(PlanRun {
            first: Command::RefinePlan { id, text },
            expect_park: true,
            note: Some(mu_agent::prompt::HEADLESS_NOTE),
        }),
        PlanCmd::Approve { id, anyway } => Ok(PlanRun {
            first: Command::StartPlan { id, decision: PlanDecision::Keep { model: None }, anyway },
            expect_park: false,
            note: None,
        }),
        other => Err(other),
    }
}

fn fail(e: String) -> i32 {
    eprintln!("mu plan: {e}");
    1
}

async fn cli(cmd: PlanCmd, cwd: &Path) -> i32 {
    let store = store_for(cwd).await;
    match cmd {
        PlanCmd::List { all, json } => {
            let list = store.list(all);
            if json {
                let arr: Vec<_> = list
                    .iter()
                    .map(|m| {
                        let mut v = serde_json::to_value(m).unwrap_or_default();
                        v["path"] = store.md_path(&m.id).to_string_lossy().into_owned().into();
                        v
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else {
                for m in &list {
                    println!("{}  {}  {}  {}", m.id, m.status.name(), &m.updated[..m.updated.len().min(10)], m.title);
                }
            }
            0
        }
        PlanCmd::Show { id, json } => {
            let (meta, text) = match store.load(&id) {
                Ok(v) => v,
                Err(e) => return fail(e),
            };
            let d = drift(&meta, cwd).await;
            if json {
                let mut v = serde_json::to_value(&meta).unwrap_or_default();
                v["path"] = store.md_path(&id).to_string_lossy().into_owned().into();
                v["content"] = text.into();
                v["drift"] = d.json();
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let base = if meta.base.is_none() {
                    "none".to_string()
                } else {
                    mu_agent::plans::base_label(&meta, cwd).await
                };
                let n = store.root.as_deref().map_or(meta.read.len(), |r| meta.files_read(r).len());
                println!("id: {}\ntitle: {}\nstatus: {}", meta.id, meta.title, meta.status.name());
                println!("base: {base}\nread: {n} files\ndrift: {}\n", d.describe());
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            0
        }
        PlanCmd::Drop { id } => store.drop(&id).map_or_else(fail, |()| 0),
        _ => fail("not a store command".into()),
    }
}

/// `mu plan list|show|drop`; returns the exit code.
pub fn run_cli(cmd: PlanCmd, cwd: &Path) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => return fail(e.to_string()),
    };
    tokio::task::LocalSet::new().block_on(&rt, cli(cmd, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_planning_expects_a_park() {
        for cmd in [PlanCmd::New { brief: "b".into() }, PlanCmd::Refine { id: "x".into(), text: None }] {
            let r = plan_run(cmd).ok().unwrap();
            let n = r.note.unwrap();
            assert!(r.expect_park && n.contains("parked for later approval") && n.contains("expected result"));
        }
        let r = plan_run(PlanCmd::Approve { id: "x".into(), anyway: false }).ok().unwrap();
        assert!(!r.expect_park && r.note.is_none());
        assert!(plan_run(PlanCmd::Drop { id: "x".into() }).is_err());
    }
}
