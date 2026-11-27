//! A mid-session working-directory switch: what it re-derives and what it records.

use super::*;
use crate::agent::RunConfig;
use crate::testing::{agent_in, assistant, drain, tmp_dir};
use mu_base::config::Configs;
use mu_session::session::format;
use mu_session::session::list::last_cwd;
use mu_testutil::block_on_local as run;

fn reply() -> Result<Message, ProviderError> {
    Ok(assistant(vec![Block::text("ok")], StopReason::Stop))
}

#[test]
fn a_user_switch_rederives_state_records_it_and_tells_the_model() {
    run(async {
        let (old, new) = (tmp_dir(), tmp_dir());
        std::fs::write(old.join(".env"), "CWD_TEST_SECRET=Zq-cwd-test-7431\n").unwrap();
        let (mut a, mut rx) = agent_in(vec![reply()], Configs::default(), RunConfig::default(), &old);
        a.turn(Some("hi".to_string().into())).await;
        let key = a.system_key();
        drain(&mut rx);

        a.user_set_cwd(new.clone()).await;

        assert_eq!(a.cwd, new);
        assert_eq!(a.tools.cwd, new);
        assert_eq!(*a.costs.watch_cwd.borrow(), new, "the git watcher follows");
        assert_ne!(a.system_key(), key, "the cached prompt size is measured again");
        let shown = new.to_string_lossy().into_owned();
        assert!(a.system().contains(&shown), "the prompt names the new directory");
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Cwd(p) if *p == new)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.starts_with("working directory:"))));
        match a.messages.last() {
            Some(Message::User { content, synthetic: true, .. }) => {
                let t = text_of(content);
                assert!(t.contains("changed the working directory") && t.contains(&shown));
            }
            m => panic!("{m:?}"),
        }
        let path = a.session.as_ref().unwrap().path.clone();
        assert_eq!(last_cwd(&path), Some(new.clone()));
        let loaded = format::load(&path).unwrap();
        assert!(
            matches!(loaded.messages.last(), Some(Message::User { content, .. }) if text_of(content).contains("changed the working directory")),
            "the note survives a resume"
        );
        let masked = secrets::redact("x Zq-cwd-test-7431 y", &new);
        assert!(!masked.text.contains("Zq-cwd-test-7431"), "the old directory's .env stays redacted");
    });
}

#[test]
fn the_cd_command_resolves_relative_paths_and_reports_failures() {
    run(async {
        let root = tmp_dir();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("file"), "").unwrap();
        let (a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &root);
        let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::task::spawn_local(a.run(crx));
        for p in [root.join("file"), PathBuf::from("nope"), PathBuf::from("sub"), root.join("sub")] {
            ctx.send(Command::SetCwd(p)).unwrap();
        }
        ctx.send(Command::Exit).unwrap();
        drop(ctx);
        task.await.unwrap();
        let events = drain(&mut rx);
        let moved: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Cwd(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(moved, [root.join("sub")], "relative paths resolve against the current directory");
        let errors: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("not a directory") && errors[1].contains("nope"));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.starts_with("already in"))));
    });
}

#[test]
fn a_mirrored_session_refuses_a_switch() {
    run(async {
        let (old, new) = (tmp_dir(), tmp_dir());
        let (mut a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &old);
        a.tools.mirror = true;
        a.user_set_cwd(new).await;
        assert_eq!(a.cwd, old);
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Error(m) if m.contains("mirror"))));
    });
}

fn cwd_entries(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().filter(|l| l.contains("\"cwd_change\"")).count()
}

#[test]
fn resuming_returns_to_where_the_session_last_worked_without_recording_it_again() {
    run(async {
        let (first, new, elsewhere) = (tmp_dir(), tmp_dir(), tmp_dir());
        let (mut a, _rx) = agent_in(vec![reply()], Configs::default(), RunConfig::default(), &first);
        a.turn(Some("hi".to_string().into())).await;
        a.user_set_cwd(new.clone()).await;
        let path = a.session.as_ref().unwrap().path.clone();
        let (len, entries) = (a.messages.len(), cwd_entries(&path));
        drop(a);

        let (mut b, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &elsewhere);
        assert!(b.load_session(&path).await);
        assert_eq!(b.cwd, new);
        assert_eq!(b.tools.cwd, new);
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Cwd(p) if *p == new)));
        assert_eq!(b.messages.len(), len, "no note for the model: the history already holds it");
        assert_eq!(cwd_entries(&path), entries, "the session already records that directory");
        assert_eq!(b.session.as_ref().unwrap().path, path);
    });
}

#[test]
fn resuming_a_session_whose_last_directory_is_gone_says_so_and_stays_put() {
    run(async {
        let (first, gone, elsewhere) = (tmp_dir(), tmp_dir(), tmp_dir());
        let (mut a, _rx) = agent_in(vec![reply()], Configs::default(), RunConfig::default(), &first);
        a.turn(Some("hi".to_string().into())).await;
        a.user_set_cwd(gone.clone()).await;
        let path = a.session.as_ref().unwrap().path.clone();
        drop(a);
        std::fs::remove_dir_all(&gone).unwrap();

        let (mut b, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &elsewhere);
        assert!(b.load_session(&path).await);
        assert_eq!(b.cwd, elsewhere);
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("no longer exists"))));
    });
}

#[test]
fn a_review_base_survives_a_move_to_another_worktree_but_not_to_another_repository() {
    run(async {
        let repo = crate::testing::git_repo();
        let wt = repo.with_file_name(format!("{}-wt", repo.file_name().unwrap().to_string_lossy()));
        let o = std::process::Command::new("git")
            .args(["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(o.status.success());
        let wt = wt.canonicalize().unwrap();
        let other = crate::testing::git_repo();
        let (mut a, _rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &repo);
        a.plan.review_base = Some(crate::review::ReviewBase { tree: Some("t".into()), ..Default::default() });

        a.user_set_cwd(wt.clone()).await;
        assert_eq!(a.cwd, wt);
        assert!(a.plan.review_base.is_some(), "a tree of this repository still resolves in its other worktree");

        a.user_set_cwd(other.clone()).await;
        assert_eq!(a.cwd, other);
        assert!(a.plan.review_base.is_none(), "another repository does not hold the tree");
        std::fs::remove_dir_all(&wt).ok();
    });
}

fn restore_cfg(restore: bool) -> Configs {
    let mut cfg = Configs::default();
    cfg.mu.cwd.restore = restore;
    cfg
}

/// Runs the agent through `cmds` and `Exit`; the directories its `Cwd` events named, in order.
async fn moves(restore: bool, start: &Path, cmds: Vec<Command>) -> Vec<PathBuf> {
    let (a, mut rx) = agent_in(vec![], restore_cfg(restore), RunConfig::default(), start);
    let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::task::spawn_local(a.run(crx));
    for c in cmds {
        ctx.send(c).unwrap();
    }
    ctx.send(Command::Exit).unwrap();
    drop(ctx);
    task.await.unwrap();
    drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::Cwd(p) => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_session_goes_back_to_the_starting_directory() {
    run(async {
        let (old, new) = (tmp_dir(), tmp_dir());
        let moved = moves(true, &old, vec![Command::SetCwd(new.clone()), Command::NewSession]).await;
        assert_eq!(moved, [new, old]);
    });
}

#[test]
fn a_new_session_stays_put_when_restoring_is_off() {
    run(async {
        let (old, new) = (tmp_dir(), tmp_dir());
        let moved = moves(false, &old, vec![Command::SetCwd(new.clone()), Command::NewSession]).await;
        assert_eq!(moved, [new]);
    });
}

#[test]
fn quitting_records_the_starting_directory_unless_restoring_is_off() {
    run(async {
        for (restore, expect_origin) in [(true, true), (false, false)] {
            let (old, new) = (tmp_dir(), tmp_dir());
            let (mut a, _rx) = agent_in(vec![reply()], restore_cfg(restore), RunConfig::default(), &old);
            a.turn(Some("hi".to_string().into())).await;
            a.user_set_cwd(new.clone()).await;
            let path = a.session.as_ref().unwrap().path.clone();
            let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
            ctx.send(Command::Exit).unwrap();
            a.run(crx).await;
            assert_eq!(last_cwd(&path), Some(if expect_origin { old } else { new }));
        }
    });
}

#[test]
fn a_switch_before_any_session_exists_leaves_the_history_alone() {
    run(async {
        let (old, new) = (tmp_dir(), tmp_dir());
        let (mut a, _rx) = agent_in(vec![], Configs::default(), RunConfig::default(), &old);
        a.user_set_cwd(new.clone()).await;
        assert_eq!(a.cwd, new);
        assert!(a.messages.is_empty(), "there is nothing earlier for the model to misread");
        assert!(a.session.is_none());
    });
}
