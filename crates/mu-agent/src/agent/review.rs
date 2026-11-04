use super::*;
use crate::review;

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

fn short(s: &str) -> String {
    s.chars().take(7).collect()
}

fn ask_option(label: String, description: Option<String>) -> mu_base::events::AskOption {
    mu_base::events::AskOption { label, description, preview: None, kind: None }
}

impl Agent {
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
        self.emit(AgentEvent::TurnStart);
        let sent = self.review_run(focus, &cancel).await;
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
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
                    base_short: short(&scope.base),
                    cur_short: short(&scope.cur),
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
        self.emit(AgentEvent::ToolEnd { id: "review".into(), name: "review".into(), output: ToolOutput::ok(md) });
        let plan_uri = scope.plan_uri.as_deref();
        let Some(report) = report else {
            return self.ask_unstructured(&out.text, plan_uri, &report_uri, cancel).await;
        };
        if report.findings.is_empty() {
            self.notice("review: no findings");
            return None;
        }
        self.pick_findings(&report, plan_uri, &report_uri, cancel).await
    }

    /// The changes `/review` covers. `None`, after a notice, when there is nothing to review.
    async fn review_scope(&mut self, focus: Option<String>) -> Option<ReviewScope> {
        let base = match self.plan.review_base.as_ref().and_then(|b| b.tree.clone()) {
            Some(tree) => review::facts::Base::Tree { tree, label: "plan approval snapshot" },
            None => {
                self.notice("no plan base recorded; reviewing uncommitted changes against HEAD");
                review::facts::Base::Head { label: "HEAD (no plan approved in this session)" }
            }
        };
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
        let notes = review::facts::user_notes(&self.messages, since);
        let facts = match review::facts::gather(review::facts::Gather {
            cwd: &self.cwd,
            base,
            plan,
            notes: &notes,
            focus: focus.as_deref(),
        })
        .await
        {
            Ok(f) => f,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        if !facts.changed() {
            self.notice(format!("nothing to review: no changes since {}", short(&facts.base)));
            return None;
        }
        if facts.files.is_empty() {
            self.notice("nothing to review: only binary, generated or lock files changed");
            return None;
        }
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
            request: facts.request,
        })
    }

    /// Runs the review child over `scope`. `None` when it fails or the run is cancelled.
    async fn run_review(
        &mut self,
        scope: &ReviewScope,
        cancel: &CancellationToken,
    ) -> Option<mu_tools::tools::subagent::Outcome> {
        let exe = match mu_common::paths::self_exe() {
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
        });
        self.emit(AgentEvent::ToolStart {
            id: "review".into(),
            name: "review".into(),
            args: json!({"scope": format!(
                "{} files +{}/−{} vs {} · {}",
                scope.files,
                scope.added,
                scope.removed,
                short(&scope.base),
                scope.model
            )}),
        });
        let out = {
            let (events, id) = (self.ports.events.clone(), "review".to_string());
            mu_tools::tools::subagent::run_child(&exe, &scope.root, &payload, cancel, move |name, summary| {
                let _ = events.send(AgentEvent::ToolOutput { id: id.clone(), chunk: format!("{name} {summary}\n") });
            })
            .await
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
        use mu_base::events::{Answer, AskQuestion};
        let q = AskQuestion {
            question: "The reviewer returned an unstructured report.".into(),
            context: None,
            header: Some("Review".into()),
            options: vec![
                ask_option("Send report to the implementer".into(), None),
                ask_option("Dismiss".into(), None),
            ],
            multi: false,
            recommended: Some(1),
            confirm: false,
        };
        match self.review_ask(q, cancel).await {
            Answer::Options(o) | Answer::Noted { picks: o, .. } if o.first().is_some_and(|l| l.starts_with("Send")) => {
                Some(review::report::raw_message(text, plan_uri, report_uri))
            }
            _ => None,
        }
    }

    /// The user picks which findings of the report go to the implementer. `None` when none do.
    async fn pick_findings(
        &mut self,
        report: &review::report::Report,
        plan_uri: Option<&str>,
        report_uri: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        use mu_base::events::{Answer, AskQuestion};
        let all = &report.findings;
        let blocking: Vec<usize> = (0..all.len()).filter(|&i| review::report::is_blocking(&all[i])).collect();
        let (n, m) = (blocking.len(), all.len());
        let mut options = Vec::new();
        if n > 0 {
            options.push(ask_option(format!("Send {n} blocking findings"), None));
        }
        if m > n {
            options.push(ask_option(format!("Send all {m} findings"), None));
        }
        options.push(ask_option("Choose findings…".into(), None));
        options.push(ask_option("Dismiss".into(), None));
        let recommended = if n > 0 { 0 } else { options.len() - 1 };
        let q = AskQuestion {
            question: "Send findings to the implementer?".into(),
            context: None,
            header: Some("Review".into()),
            options,
            multi: false,
            recommended: Some(recommended),
            confirm: false,
        };
        let default_set = if n > 0 { blocking.clone() } else { (0..m).collect() };
        let (answer, typed) = match self.review_ask(q, cancel).await {
            Answer::Noted { picks, note } => (Answer::Options(picks), Some(note)),
            a => (a, None),
        };
        let (chosen, note): (Vec<usize>, Option<String>) = match answer {
            Answer::Options(o) => match o.first().map(String::as_str) {
                Some(l) if l.starts_with("Send all") => ((0..m).collect(), None),
                Some(l) if l.starts_with("Send ") => (blocking.clone(), None),
                Some(l) if l.starts_with("Choose") => {
                    let options = all
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let first = f.body.lines().next().unwrap_or("");
                            let at = match f.line {
                                Some(l) => format!("{}:{l}", f.file),
                                None => f.file.clone(),
                            };
                            ask_option(
                                format!("{}. [P{}] {}", i + 1, f.priority, f.title),
                                Some(format!("{at} — {first}")),
                            )
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
            },
            Answer::Custom(t) => (default_set, Some(t)),
            Answer::Cancelled | Answer::Noted { .. } => (Vec::new(), None),
        };
        let note = note.or(typed);
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
