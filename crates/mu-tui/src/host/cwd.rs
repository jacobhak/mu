//! A working-directory switch: the host moves its own copy of the directory and reloads the context
//! (AGENTS.md, rules, skills, commands, agents) for it.

use super::local::{Local, Registry};
use super::{Answer, pending};
use mu_common::paths;
use mu_tools::context::Context;
use mu_tools::mcp::config;
use std::path::PathBuf;
use std::rc::Rc;

/// `cwd`: the new directory. Answers `Switched` once the context is loaded.
pub const SET_CWD: &str = "set_cwd";

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Switched {
    /// False when another switch came while this one's context loaded: the newer one wins.
    pub applied: bool,
    /// Project MCP files in the new directory that stay off until trusted.
    pub untrusted_mcp: usize,
}

pub(super) fn register(r: &mut Registry) {
    r.add(SET_CWD, set_cwd);
}

fn set_cwd(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |cwd: PathBuf| {
        let seq = l.set_cwd(cwd.clone());
        let l = l.clone();
        let skills_user = l.providers.cfg.omp.skills.enable_claude_user;
        async move {
            let loaded = tokio::task::spawn_blocking(move || {
                // `Configs` is not `Sync`; the loader reads only this flag from it.
                let mut cfg = mu_base::config::Configs::default();
                cfg.omp.skills.enable_claude_user = skills_user;
                let untrusted = config::untrusted_files(&cwd, &paths::omp_agent_dir()).len();
                (Context::load_for(&cwd, &cfg), untrusted)
            })
            .await
            .map_err(|e| format!("cannot reload the context: {e}"))?;
            Ok(Switched { applied: l.set_ctx(seq, loaded.0), untrusted_mcp: loaded.1 })
        }
    })
}
