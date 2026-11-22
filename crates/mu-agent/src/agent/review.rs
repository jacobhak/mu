use super::*;
use crate::review;
use mu_base::events::{ActionChoice, ActionInput, ActionSpec, ReviewRequest};
use mu_base::git::short_sha;

/// What `/review` covers: the snapshots compared, the review model and its totals, and the request.
struct ReviewScope {
    root: PathBuf,
    base: String,
    cur: String,
    /// Changed files that are reviewed; excluded ones are not counted.
    files: usize,
    added: u32,
    removed: u32,
    model: String,
    plan_uri: Option<String>,
    request: String,
}

fn ask_option(label: String, description: Option<String>) -> mu_base::events::AskOption {
    mu_base::events::AskOption { label, description, preview: None, kind: None }
}

fn action(id: &str, label: String, input: ActionInput, approves: bool) -> ActionSpec {
    ActionSpec { id: id.into(), label, disabled: false, input, keeps_context: false, approves }
}

impl Agent {
    /// The shared document dialog; a cancel or a closed dialog is `None`.
    async fn review_dialog(&self, req: ReviewRequest, cancel: &CancellationToken) -> Option<ActionChoice> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(AgentEvent::Review { request: req, reply: tx });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = rx => r.ok(),
        }
    }

    /// One single-answer question to the user; a cancel or a closed dialog is `Cancelled`.
    async fn review_ask(&self, q: mu_base::events::AskQuestion, cancel: &CancellationToken) -> mu_base::events::Answer {
        use mu_base::events::Answer;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(AgentEvent::Ask { questions: vec![q], reply: tx });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Answer::Cancelled,
            r = rx => r.ok().flatten().and_then(|mut a| (!a.is_empty()).then(|| a.remove(0))).unwrap_or(Answer::Cancelled),
        }
    }

    /// `/review`: a fresh-context lead reviewer checks the changes since plan approval (or the
    /// uncommitted changes) and the user picks which findings go to the implementer.
    pub(super) async fn review(&mut self, focus: Option<String>) {
        if !self.ensure_session().await {
            return;
        }
        let cancel = CancellationToken::new();
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
        self.emit_turn_start();
        let sent = self.review_run(focus, &cancel).await;
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
        // Input typed while the reviewer ran goes back to the composer, not into the fix turn.
        report_dropped(&self.ports.inbox, &self.ports.events);
        if let Some(msg) = sent {
            self.turn(Some(msg.into())).await;
        }
    }

    /// Runs the review and the selection dialog. Returns the message to send to the implementer.
    async fn review_run(&mut self, focus: Option<String>, cancel: &CancellationToken) -> Option<String> {
        let scope = self.review_scope(focus).await?;
        let out = self.run_review(&scope, cancel).await?;
        let report = review::report::report_of(&out);
        let md = match &report {
            Some(r) => review::report::render_markdown(
                r,
                &review::report::Meta {
                    base_short: short_sha(&scope.base).to_string(),
                    cur_short: short_sha(&scope.cur).to_string(),
                    files: scope.files,
                    added: scope.added,
                    removed: scope.removed,
                    model: scope.model.clone(),
                },
            ),
            None => format!("reviewer returned an unstructured report\n\n{}", out.text),
        };
        let report_uri = review::report_uri();
        if let Err(e) = self.write_local_file(&report_uri, &md) {
            self.error(format!("cannot save the review to {report_uri}: {e}"));
        }
        let output = ToolOutput::ok(md.clone());
        self.emit(AgentEvent::ToolEnd { id: "review".into(), name: "review".into(), output });
        let plan_uri = scope.plan_uri.as_deref();
        let Some(report) = report else {
            return self.ask_unstructured(&out.text, plan_uri, &report_uri, cancel).await;
        };
        if report.findings.is_empty() {
            self.notice("review: no findings");
            return None;
        }
        self.pick_findings(&report, &md, plan_uri, &report_uri, cancel).await
    }

    /// The changes `/review` covers. `None`, after a notice, when there is nothing to review.
    async fn review_scope(&mut self, focus: Option<String>) -> Option<ReviewScope> {
        use review::facts::{Base, Worktree, repo_root};
        let root = match repo_root(&self.cwd).await {
            Ok(r) => r,
            Err(e) => {
                self.error(e);
                return None;
            }
        };
        let wt = match Worktree::snapshot(root).await {
            Ok(w) => w,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        let base = match self.plan.review_base.as_ref().and_then(|b| b.tree.clone()) {
            Some(tree) => Base::Tree { tree, label: "plan approval snapshot" },
            None => {
                self.notice("no plan base recorded; reviewing uncommitted changes against HEAD");
                Base::Head { label: "HEAD (no plan approved in this session)" }
            }
        };
        let facts = match wt.gather(base).await {
            Ok(f) => f,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        if !facts.changed() {
            self.notice(format!("nothing to review: no changes since {}", short_sha(&facts.base)));
            return None;
        }
        if facts.files.is_empty() {
            self.notice("nothing to review: only binary, generated or lock files changed");
            return None;
        }
        let plan_uri = self.plan.review_base.as_ref().and_then(|b| b.plan_uri.clone());
        let plan = match &plan_uri {
            Some(u) => match self.tools.resolve(u) {
                Ok(r) if r.path.exists() => Some(r.path),
                _ => {
                    self.notice(format!("plan file {u} not found; reviewing without it"));
                    None
                }
            },
            None => None,
        };
        let plan_uri = plan.as_ref().and(plan_uri);
        let since = self.plan.review_base.as_ref().map_or(0, |b| b.since);
        let notes = review::facts::user_notes(&self.messages.to_vec(), since);
        let request = facts.request(plan, &notes, focus.as_deref());
        let model = mu_base::models::role_selector("review", &self.ports.providers.cfg)
            .and_then(|s| self.ports.providers.resolve(&s).ok())
            .map_or_else(|| self.model.model.id.clone(), |(m, _)| m.id);
        Some(ReviewScope {
            root: facts.root,
            base: facts.base,
            cur: facts.cur,
            files: facts.files.len(),
            added: facts.added,
            removed: facts.removed,
            model,
            plan_uri,
            request,
        })
    }

    /// Runs the review child over `scope`. `None` when it fails or the run is cancelled.
    async fn run_review(
        &mut self,
        scope: &ReviewScope,
        cancel: &CancellationToken,
    ) -> Option<mu_tools::tools::subagent::Outcome> {
        #[cfg(test)]
        let exe = self.review_child.clone().map_or_else(mu_common::paths::self_exe, Ok);
        #[cfg(not(test))]
        let exe = mu_common::paths::self_exe();
        let exe = match exe {
            Ok(e) => e,
            Err(e) => {
                self.error(format!("cannot locate mu executable: {e}"));
                return None;
            }
        };
        let payload = json!({
            "task": scope.request,
            "agent": context::REVIEW_LEAD,
            "outputSchema": review::report::report_schema(),
            "parentSession": self.session.as_ref().map(|s| s.path.clone()),
            "permissions": self.tools.permissions.inherit_json(),
        });
        // A job when the UI keeps them: listed in the jobs panel, which opens its transcript and
        // cancels it with `x`.
        let job = self.tools.jobs.as_ref().map(|j| j.unique_id("review"));
        let hint = job.as_deref().map(|id| format!(" · job {id} (Alt+J)")).unwrap_or_default();
        self.emit(AgentEvent::ToolStart {
            id: "review".into(),
            name: "review".into(),
            args: json!({"scope": format!(
                "{} files +{}/−{} vs {} · {}{hint}",
                scope.files,
                scope.added,
                scope.removed,
                short_sha(&scope.base),
                scope.model
            )}),
        });
        let progress = {
            let (events, id) = (self.ports.events.clone(), "review".to_string());
            move |name: &str, summary: &str| {
                let _ = events.send(AgentEvent::ToolOutput { id: id.clone(), chunk: format!("{name} {summary}\n") });
            }
        };
        let out = match (&self.tools.jobs, &job) {
            (Some(jobs), Some(id)) => {
                jobs.run_owned(
                    id,
                    context::REVIEW_LEAD,
                    &exe,
                    &scope.root,
                    &payload,
                    cancel,
                    progress,
                    self.tools.permissions.clone(),
                    mu_tools::tools::subagent::Mirror::Off,
                )
                .await
            }
            _ => mu_tools::tools::subagent::run_child(&exe, &scope.root, &payload, cancel, progress).await,
        };
        if cancel.is_cancelled() {
            self.emit(AgentEvent::ToolEnd {
                id: "review".into(),
                name: "review".into(),
                output: ToolOutput::err("review cancelled"),
            });
            return None;
        }
        if !out.ok {
            self.emit(AgentEvent::ToolEnd {
                id: "review".into(),
                name: "review".into(),
                output: ToolOutput::err(out.text),
            });
            return None;
        }
        Some(out)
    }

    /// The reviewer's report did not parse: asks whether its raw text goes to the implementer.
    async fn ask_unstructured(
        &mut self,
        text: &str,
        plan_uri: Option<&str>,
        report_uri: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let req = ReviewRequest {
            header: "Review".into(),
            title: "unstructured report".into(),
            uri: Some(report_uri.to_string()),
            file: None,
            content: text.to_string(),
            note: None,
            actions: vec![
                action("send", "Send report to the implementer".into(), ActionInput::None, true),
                action("dismiss", "Dismiss".into(), ActionInput::None, false),
            ],
            default: 1,
            models: false,
            cancel_turn: false,
            cancel_notice: "review dismissed".into(),
        };
        match self.review_dialog(req, cancel).await {
            Some(c) if c.id == "send" => Some(review::report::raw_message(text, plan_uri, report_uri)),
            _ => None,
        }
    }

    /// The user picks which findings of the report go to the implementer. `None` when none do.
    async fn pick_findings(
        &mut self,
        report: &review::report::Report,
        md: &str,
        plan_uri: Option<&str>,
        report_uri: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        use mu_base::events::{Answer, AskQuestion};
        let all = &report.findings;
        let blocking: Vec<usize> = (0..all.len()).filter(|&i| review::report::is_blocking(&all[i])).collect();
        let (n, m) = (blocking.len(), all.len());
        let mut actions = Vec::new();
        if n > 0 {
            actions.push(action("blocking", format!("Send {n} blocking findings"), ActionInput::Optional, true));
        }
        if m > n {
            actions.push(action("all", format!("Send all {m} findings"), ActionInput::Optional, true));
        }
        actions.push(action("choose", "Choose findings…".into(), ActionInput::Optional, false));
        actions.push(action("dismiss", "Dismiss".into(), ActionInput::None, false));
        let default = if n > 0 { 0 } else { actions.len() - 1 };
        let req = ReviewRequest {
            header: "Review".into(),
            title: format!("{m} findings, {n} blocking"),
            uri: Some(report_uri.to_string()),
            file: None,
            content: md.to_string(),
            note: None,
            actions,
            default,
            models: false,
            cancel_turn: false,
            cancel_notice: "review dismissed".into(),
        };
        let choice = self.review_dialog(req, cancel).await?;
        let dialog_note = choice.text.filter(|t| !t.trim().is_empty());
        let (chosen, typed): (Vec<usize>, Option<String>) = match choice.id.as_str() {
            "all" => ((0..m).collect(), None),
            "blocking" => (blocking.clone(), None),
            "choose" => {
                let options = all
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let first = f.body.lines().next().unwrap_or("");
                        let at = match f.line {
                            Some(l) => format!("{}:{l}", f.file),
                            None => f.file.clone(),
                        };
                        let label = format!("{}. [P{}] {}", i + 1, f.priority, f.title);
                        ask_option(label, Some(format!("{at} — {first}")))
                    })
                    .collect();
                let q = AskQuestion {
                    question: "Which findings should be sent?".into(),
                    context: None,
                    header: Some("Review".into()),
                    options,
                    multi: true,
                    recommended: None,
                    confirm: false,
                    skippable: false,
                };
                let (sel, typed) = match self.review_ask(q, cancel).await {
                    Answer::Options(sel) => (sel, None),
                    Answer::Noted { picks, note } => (picks, Some(note)),
                    _ => (Vec::new(), None),
                };
                let idx = sel
                    .iter()
                    .filter_map(|l| l.split('.').next()?.trim().parse::<usize>().ok())
                    .filter(|i| (1..=m).contains(i))
                    .map(|i| i - 1)
                    .collect::<std::collections::BTreeSet<_>>();
                (idx.into_iter().collect(), typed)
            }
            _ => (Vec::new(), None),
        };
        let note = dialog_note.or(typed);
        if chosen.is_empty() {
            return None;
        }
        let picked: Vec<&review::report::Finding> = chosen.iter().map(|&i| &all[i]).collect();
        Some(review::report::fix_message(&picked, plan_uri, report_uri, note.as_deref()))
    }

    pub(super) fn set_review_base(&mut self, b: review::ReviewBase) {
        if let Some(s) = self.session.as_mut() {
            s.custom("review_base", serde_json::to_value(&b).unwrap_or_default());
        }
        self.plan.review_base = Some(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{agent_in, drain, git_repo};
    use mu_base::config::Configs;
    use mu_testutil::block_on_local as run;
    use std::os::unix::fs::PermissionsExt;

    const RESULT: &str =
        r#"{"type":"result","isError":false,"text":"","data":{"verdict":"pass","summary":"all good","findings":[]}}"#;

    async fn review_cmd(a: &mut Agent) {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let live = Rc::new(std::cell::Cell::new(Thinking::Off));
        let mut q = Queue {
            rx,
            inbox: new_inbox(),
            backlog: Default::default(),
            jobs: None,
            events: etx,
            live,
            perms: Rc::default(),
        };
        a.command(Command::Review(None), &mut q).await;
    }

    fn notices(ev: &[AgentEvent]) -> Vec<&str> {
        ev.iter()
            .filter_map(|e| match e {
                AgentEvent::Notice(m) | AgentEvent::Error(m) => Some(m.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn review_outside_a_repo_reports_only_the_bare_git_error() {
        run(async {
            let dir = crate::testing::tmp_dir();
            let (mut a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), dir);
            review_cmd(&mut a).await;
            let ev = drain(&mut rx);
            let n = notices(&ev);
            assert!(n.len() == 1 && n[0].starts_with("review needs a git repository: "), "{n:?}");
        });
    }

    #[test]
    fn review_checks_the_plan_file_only_when_there_is_something_to_review() {
        run(async {
            let dir = git_repo();
            let (mut a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), dir);
            a.plan.review_base =
                Some(review::ReviewBase { tree: None, plan_uri: Some("local://missing-plan.md".into()), since: 0 });
            review_cmd(&mut a).await;
            let ev = drain(&mut rx);
            let n = notices(&ev);
            assert_eq!(n.len(), 2, "{n:?}");
            assert!(n[0].starts_with("no plan base recorded") && n[1].starts_with("nothing to review"), "{n:?}");
        });
    }

    #[test]
    fn review_reports_a_child_that_cannot_start() {
        run(async {
            let dir = git_repo();
            std::fs::write(dir.join("a.txt"), "two\n").unwrap();
            let (mut a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), dir.clone());
            a.review_child = Some(dir.join("no-such-mu"));
            review_cmd(&mut a).await;
            let ev = drain(&mut rx);
            assert!(ev.iter().any(|e| matches!(e, AgentEvent::Notice(m) if m.contains("no plan base recorded"))));
            assert!(ev.iter().any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "review")));
            let end = ev.iter().find_map(|e| match e {
                AgentEvent::ToolEnd { name, output, .. } if name == "review" => {
                    Some((output.is_error, output.text.clone()))
                }
                _ => None,
            });
            let (is_error, text) = end.expect("the review ends");
            assert!(is_error && text.contains("cannot start subagent"), "{text}");
            assert!(!ev.iter().any(|e| matches!(e, AgentEvent::Ask { .. })));
            assert!(ev.iter().any(|e| matches!(e, AgentEvent::TurnEnd { aborted: false })));
        });
    }

    #[test]
    fn review_sends_the_gathered_request_to_the_child() {
        run(async {
            let dir = git_repo();
            std::fs::write(dir.join("a.txt"), "two\n").unwrap();
            let payload = dir.join("payload.json");
            let child = dir.join("fake-mu");
            // The child reads its task line and keeps stdin open for approval answers.
            let script = format!(
                "#!/bin/sh\nIFS= read -r line\nprintf '%s\\n' \"$line\" > '{}'\necho '{RESULT}'\n",
                payload.display()
            );
            std::fs::write(&child, script).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (mut a, mut rx) = agent_in(vec![], Configs::default(), RunConfig::default(), dir.clone());
            a.review_child = Some(child);
            review_cmd(&mut a).await;
            let sent: Value = serde_json::from_str(&std::fs::read_to_string(&payload).unwrap()).unwrap();
            assert_eq!(sent["agent"], context::REVIEW_LEAD);
            let task = sent["task"].as_str().unwrap();
            assert!(task.contains("HEAD (no plan approved in this session)"), "{task}");
            assert!(task.contains("| a.txt | +1/-1 |"), "{task}");
            let ev = drain(&mut rx);
            assert!(ev.iter().any(|e| matches!(e, AgentEvent::Notice(m) if m == "review: no findings")));
            assert!(ev.iter().any(|e| matches!(
                e,
                AgentEvent::ToolEnd { name, output, .. }
                    if name == "review" && !output.is_error && output.text.contains("## Review: PASS")
            )));
        });
    }

    #[test]
    fn input_typed_during_the_review_goes_back_before_the_fix_turn() {
        use crate::testing::assistant;
        use mu_base::message::{Block, StopReason};
        run(async {
            let dir = git_repo();
            std::fs::write(dir.join("a.txt"), "two\n").unwrap();
            let child = dir.join("fake-mu");
            let result = r#"{"type":"result","isError":false,"text":"","data":{"verdict":"fail","summary":"s","findings":[{"title":"t","body":"b","file":"a.txt","priority":1,"confidence":1}]}}"#;
            std::fs::write(&child, format!("#!/bin/sh\nIFS= read -r line\necho '{result}'\n")).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
            let fixed = Ok(assistant(vec![Block::text("fixed")], StopReason::Stop));
            let (mut a, mut rx) = agent_in(vec![fixed], Configs::default(), RunConfig::default(), dir.clone());
            a.review_child = Some(child);
            a.ports.inbox.borrow_mut().steer.push_back("typed".to_string().into());
            let mut ev = Vec::new();
            let answer = async {
                let mut ends = 0;
                while ends < 2 {
                    let Some(e) = rx.recv().await else { break };
                    match e {
                        AgentEvent::Review { reply, .. } => {
                            let _ = reply.send(ActionChoice { id: "blocking".into(), text: None, model: None });
                        }
                        e => {
                            ends += usize::from(matches!(e, AgentEvent::TurnEnd { .. }));
                            ev.push(e);
                        }
                    }
                }
            };
            tokio::join!(review_cmd(&mut a), answer);
            let first_end = ev.iter().position(|e| matches!(e, AgentEvent::TurnEnd { .. })).unwrap();
            let dropped = ev.iter().position(|e| matches!(e, AgentEvent::QueuedDropped(d) if d[0].text == "typed"));
            let fix_start = ev.iter().rposition(|e| matches!(e, AgentEvent::TurnStart)).unwrap();
            assert!(dropped.is_some_and(|d| first_end < d && d < fix_start), "returned between the two turns");
            assert!(!ev.iter().any(|e| matches!(e, AgentEvent::UserPrompt(t) if t == "typed")));
        });
    }

    #[test]
    fn sending_all_findings_carries_the_dialog_note_into_the_fix_message() {
        use crate::testing::assistant;
        use mu_base::message::{Block, StopReason};
        run(async {
            let dir = git_repo();
            std::fs::write(dir.join("a.txt"), "two\n").unwrap();
            let child = dir.join("fake-mu");
            let result = r#"{"type":"result","isError":false,"text":"","data":{"verdict":"fail","summary":"s","findings":[{"title":"t","body":"b","file":"a.txt","priority":1,"confidence":1},{"title":"u","body":"c","file":"a.txt","priority":3,"confidence":1}]}}"#;
            std::fs::write(&child, format!("#!/bin/sh\nIFS= read -r line\necho '{result}'\n")).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
            let fixed = Ok(assistant(vec![Block::text("fixed")], StopReason::Stop));
            let (mut a, mut rx) = agent_in(vec![fixed], Configs::default(), RunConfig::default(), dir.clone());
            a.review_child = Some(child);
            let mut ev = Vec::new();
            let answer = async {
                let mut ends = 0;
                while ends < 2 {
                    let Some(e) = rx.recv().await else { break };
                    match e {
                        AgentEvent::Review { request, reply } => {
                            assert!(request.actions.iter().any(|x| x.id == "all"), "{:?}", request.actions);
                            let text = Some("focus on API".to_string());
                            let _ = reply.send(ActionChoice { id: "all".into(), text, model: None });
                        }
                        e => {
                            ends += usize::from(matches!(e, AgentEvent::TurnEnd { .. }));
                            ev.push(e);
                        }
                    }
                }
            };
            tokio::join!(review_cmd(&mut a), answer);
            let fix = ev.iter().find_map(|e| match e {
                AgentEvent::UserPrompt(t) => Some(t.clone()),
                _ => None,
            });
            let fix = fix.expect("a fix message is sent");
            assert!(fix.contains("User note: focus on API"), "{fix}");
            assert!(fix.contains("1. [P1] t") && fix.contains("2. [P3] u"), "{fix}");
        });
    }
}
