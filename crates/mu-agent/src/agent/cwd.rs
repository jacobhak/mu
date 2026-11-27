//! Changing the working directory mid-session: everything derived from the directory is derived
//! again here, by the same code that derives it at startup.

use super::*;
use mu_common::paths;

/// What brought a switch about.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    User,
    /// The `set_cwd` tool: its result already tells the transcript, and the turn lists new MCP tools itself.
    Model,
    /// Resuming a session that last worked elsewhere: the session already records that directory.
    Resume,
}

impl Agent {
    /// `/cd <dir>`: the user's own switch. The model is told, since its earlier tool results are
    /// relative to the old directory.
    pub(super) async fn user_set_cwd(&mut self, dir: PathBuf) {
        let s = dir.to_string_lossy();
        let dir = paths::expand_tilde(&s, &paths::home()).unwrap_or_else(|| dir.clone());
        let dir = if dir.is_absolute() { dir } else { self.cwd.join(dir) };
        let old = self.cwd.clone();
        if !self.set_cwd(dir, Why::User).await || self.messages.is_empty() {
            return;
        }
        let note = format!(
            "The user changed the working directory from {} to {}. Relative paths in earlier tool results refer to the old directory.",
            old.display(),
            self.cwd.display()
        );
        self.push(Message::synthetic_user(&note));
        self.turn_msgs.clear(); // not a model turn: no `turn_end` hook for it
    }

    fn restores_cwd(&self) -> bool {
        self.ports.providers.cfg.mu.cwd.restore && !self.tools.mirror && self.cwd != self.origin_cwd
    }

    /// `/new`: back to the directory the agent started in, unless `[cwd] restore` is off.
    pub(super) async fn restore_origin_cwd(&mut self) {
        if self.restores_cwd() {
            self.set_cwd(self.origin_cwd.clone(), Why::User).await;
        }
    }

    /// Quitting: the session's last directory becomes the one the agent started in, so a resume
    /// from there lands where the run began. Nothing is re-derived: the process is ending.
    pub(super) fn record_origin_cwd(&mut self) {
        if self.restores_cwd()
            && let Some(s) = self.session.as_mut()
        {
            s.record_cwd(&self.origin_cwd);
        }
    }

    /// Moves the session to `dir`. `false` when it did not move (an error or notice says why).
    pub(super) async fn set_cwd(&mut self, dir: PathBuf, why: Why) -> bool {
        let new = match std::fs::canonicalize(&dir) {
            Ok(p) if p.is_dir() => p,
            Ok(_) => {
                self.error(format!("{}: not a directory", dir.display()));
                return false;
            }
            Err(e) => {
                self.error(format!("{}: {e}", dir.display()));
                return false;
            }
        };
        if self.tools.mirror {
            self.error("the working directory cannot change in a session a parent mirrors");
            return false;
        }
        if new == self.cwd || std::fs::canonicalize(&self.cwd).is_ok_and(|c| c == new) {
            if why != Why::Resume {
                self.notice(format!("already in {}", new.display()));
            }
            return false;
        }
        let old = std::mem::replace(&mut self.cwd, new.clone());
        let old_repo = git::main_checkout(&old).await;
        *self.costs.watch_cwd.borrow_mut() = new.clone();
        self.tools.set_cwd(new.clone());
        let mut warns = Vec::new();
        self.tools.permissions.rebase_project(&new, &mut warns).await;
        for w in warns {
            self.notice(w);
        }

        let ctx = Context::load_for(&new, &self.ports.providers.cfg);
        for w in &ctx.warnings {
            self.notice(format!("warning: {w}"));
        }
        self.ctx = Rc::new(ctx);
        self.tools.set_skills(self.ctx.skills.iter().map(|s| (s.name.clone(), s.dir.clone())).collect());
        self.tools.set_context_page(prompt::context_page(&self.ctx));
        self.sync_mode_tables();
        if self.plan.mode.as_ref().is_some_and(|m| self.ctx.mode(&m.name).is_none()) {
            self.notice("the active mode is not defined here");
            self.exit_mode();
        }

        // Plugins read the directory from the session params; `mcp_sync` hashes configs with it.
        self.sync_plugin_session();
        let (servers, warnings) = mu_tools::mcp::config::load(&new, &paths::omp_agent_dir());
        let sync = self.ports.plugins.mcp_sync(servers).await;
        // A model-initiated switch is mid-turn: the loop lists missing tools, cancellably, before its next request.
        if why != Why::Model && (!sync.added.is_empty() || !sync.changed.is_empty()) {
            self.ports.plugins.list_missing_until(&CancellationToken::new()).await;
        }
        if !sync.is_empty() {
            let mut parts = Vec::new();
            for (label, names) in [("added", &sync.added), ("restarted", &sync.changed), ("stopped", &sync.removed)] {
                if !names.is_empty() {
                    parts.push(format!("{label} {}", names.join(", ")));
                }
            }
            self.notice(format!("mcp servers for {}: {}", new.display(), parts.join("; ")));
        }
        for w in warnings {
            self.notice(w);
        }
        self.refresh_tool_defs();

        // A review base is a tree of the repository it was taken in; another worktree of it still resolves.
        if git::main_checkout(&new).await != old_repo {
            self.plan.review_base = None;
        }
        *self.costs.git.borrow_mut() = git::git_status(&new).await;
        *self.memo.system.borrow_mut() = None;
        if why != Why::Resume
            && let Some(s) = self.session.as_mut()
        {
            s.record_cwd(&new);
        }
        self.emit(AgentEvent::Cwd(new.clone()));
        if why != Why::Model {
            self.notice(format!("working directory: {} -> {}", old.display(), new.display()));
        }
        self.emit_status();
        true
    }
}
