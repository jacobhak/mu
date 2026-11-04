use super::*;
use crate::review;

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
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let sent = self.review_run(focus, &cancel).await;
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
        if let Some(msg) = sent {
            self.turn(Some(msg.into())).await;
        }
    }

    /// Runs the review and the selection dialog. Returns the message to send to the implementer.
    async fn review_run(&mut self, focus: Option<String>, cancel: &CancellationToken) -> Option<String> {
        use mu_base::events::{Answer, AskOption, AskQuestion};
        let short = |s: &str| s.chars().take(7).collect::<String>();
        let root = match review::repo_root(&self.cwd).await {
            Ok(r) => r,
            Err(e) => {
                self.error(e);
                return None;
            }
        };
        let cur = match review::snapshot_tree(&root).await {
            Ok(c) => c,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        let (base, base_label) = match self.review_base.as_ref().and_then(|b| b.tree.clone()) {
            Some(t) => (t, "plan approval snapshot"),
            None => {
                self.notice("no plan base recorded; reviewing uncommitted changes against HEAD");
                match review::head_tree(&root).await {
                    Ok(t) => (t, "HEAD (no plan approved in this session)"),
                    Err(e) => {
                        self.error(format!("review: {e}"));
                        return None;
                    }
                }
            }
        };
        let since = self.review_base.as_ref().map_or(0, |b| b.since);
        let stats = match review::numstat(&root, &base, &cur).await {
            Ok(s) => s,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        if stats.is_empty() {
            self.notice(format!("nothing to review: no changes since {}", short(&base)));
            return None;
        }
        let (files, excluded) = review::split_excluded(stats);
        if files.is_empty() {
            self.notice("nothing to review: only binary, generated or lock files changed");
            return None;
        }
        let plan_uri = self.review_base.as_ref().and_then(|b| b.plan_uri.clone());
        let plan_path = match &plan_uri {
            Some(u) => match self.tools.resolve(u) {
                Ok(r) if r.path.exists() => Some(r.path),
                _ => {
                    self.notice(format!("plan file {u} not found; reviewing without it"));
                    None
                }
            },
            None => None,
        };
        let plan_uri = plan_path.as_ref().and(plan_uri);
        let notes = review::user_notes(&self.messages, since);
        let request = review::request(&review::RequestInput {
            root: &root,
            base: &base,
            cur: &cur,
            base_label,
            plan_path,
            files: &files,
            excluded: &excluded,
            notes: &notes,
            focus: focus.as_deref(),
        });
        let model = mu_base::models::role_selector("review", &self.providers.cfg)
            .and_then(|s| self.providers.resolve(&s).ok())
            .map_or_else(|| self.model.id.clone(), |(m, _)| m.id);
        let (added, removed) =
            files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => {
                self.error(format!("cannot locate mu executable: {e}"));
                return None;
            }
        };
        let payload = json!({
            "task": request,
            "agent": context::REVIEW_LEAD,
            "outputSchema": review::report_schema(),
            "parentSession": self.session.as_ref().map(|s| s.path.clone()),
        });
        let id = "review".to_string();
        self.emit(AgentEvent::ToolStart {
            id: id.clone(),
            name: "review".into(),
            args: json!({"scope": format!("{} files +{added}/−{removed} vs {} · {model}", files.len(), short(&base))}),
        });
        let out = {
            let (events, id) = (self.events.clone(), id.clone());
            mu_tools::tools::subagent::run_child(&exe, &root, &payload, cancel, move |name, summary| {
                let _ = events.send(AgentEvent::ToolOutput { id: id.clone(), chunk: format!("{name} {summary}\n") });
            })
            .await
        };
        if cancel.is_cancelled() {
            self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::err("review cancelled") });
            return None;
        }
        if !out.ok {
            self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::err(out.text) });
            return None;
        }
        let report = review::report_of(&out);
        let md = match &report {
            Some(r) => review::render_markdown(
                r,
                &review::Meta {
                    base_short: short(&base),
                    cur_short: short(&cur),
                    files: files.len(),
                    added,
                    removed,
                    model,
                },
            ),
            None => format!("reviewer returned an unstructured report\n\n{}", out.text),
        };
        let report_uri = review::report_uri();
        match self.tools.resolve(&report_uri) {
            Ok(r) => {
                if let Err(e) = std::fs::create_dir_all(r.path.parent().unwrap_or(&r.path))
                    .and_then(|_| std::fs::write(&r.path, &md))
                {
                    self.error(format!("cannot save the review to {report_uri}: {e}"));
                }
            }
            Err(e) => self.error(format!("cannot save the review to {report_uri}: {e}")),
        }
        self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::ok(md) });

        let opt =
            |label: String, description: Option<String>| AskOption { label, description, preview: None, kind: None };
        let Some(report) = report else {
            let q = AskQuestion {
                question: "The reviewer returned an unstructured report.".into(),
                context: None,
                header: Some("Review".into()),
                options: vec![opt("Send report to the implementer".into(), None), opt("Dismiss".into(), None)],
                multi: false,
                recommended: Some(1),
                confirm: false,
            };
            return match self.review_ask(q, cancel).await {
                Answer::Options(o) | Answer::Noted { picks: o, .. }
                    if o.first().is_some_and(|l| l.starts_with("Send")) =>
                {
                    Some(review::raw_message(&out.text, plan_uri.as_deref(), &report_uri))
                }
                _ => None,
            };
        };
        if report.findings.is_empty() {
            self.notice("review: no findings");
            return None;
        }
        let all = &report.findings;
        let blocking: Vec<usize> = (0..all.len()).filter(|&i| review::is_blocking(&all[i])).collect();
        let (n, m) = (blocking.len(), all.len());
        let mut options = Vec::new();
        if n > 0 {
            options.push(opt(format!("Send {n} blocking findings"), None));
        }
        if m > n {
            options.push(opt(format!("Send all {m} findings"), None));
        }
        options.push(opt("Choose findings…".into(), None));
        options.push(opt("Dismiss".into(), None));
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
                            opt(format!("{}. [P{}] {}", i + 1, f.priority, f.title), Some(format!("{at} — {first}")))
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
        let picked: Vec<&review::Finding> = chosen.iter().map(|&i| &all[i]).collect();
        Some(review::fix_message(&picked, plan_uri.as_deref(), &report_uri, note.as_deref()))
    }

    pub(super) fn set_review_base(&mut self, b: review::ReviewBase) {
        if let Some(s) = self.session.as_mut() {
            s.custom("review_base", serde_json::to_value(&b).unwrap_or_default());
        }
        self.review_base = Some(b);
    }
}
