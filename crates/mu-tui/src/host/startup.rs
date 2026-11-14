//! Start-up: what the UI shows or asks when it opens, and the handful of settings it reads from the
//! host's config.

use super::local::{Local, Registry};
use super::{Answer, ready};
use mu_base::config::models_path;
use mu_common::paths;
use mu_tools::mcp::config::{self, Kind, McpServer};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::rc::Rc;

pub const STARTUP: &str = "startup";
/// `/detach`: leave the agent running and end this client. Only an attached client has anyone to leave.
pub const DETACH: &str = "detach";

/// One MCP server of an untrusted project file, with its definition as written.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UntrustedServer {
    pub name: String,
    pub written: String,
}

/// A project MCP file that is off until the user trusts it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UntrustedFile {
    pub file: PathBuf,
    pub digest: String,
    pub servers: Vec<UntrustedServer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Startup {
    /// Warnings to show above the first prompt: the key store, plugins that did not start.
    pub warnings: Vec<String>,
    /// omp's model config can be copied: the lines that list what would be, and where it goes.
    pub import_offer: Option<ImportOffer>,
    pub untrusted_mcp: Vec<UntrustedFile>,
    /// Ghost-text prediction is on.
    pub predict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportOffer {
    pub lines: Vec<String>,
    pub models_path: PathBuf,
}

/// A server as its file writes it (an untrusted entry is shown unexpanded): a command with its arguments, or a url.
fn as_written(s: &McpServer) -> String {
    match &s.kind {
        Kind::Stdio { command, args, .. } => format!("{command} {}", args.join(" ")).trim_end().to_string(),
        Kind::Http { url, .. } => url.clone(),
    }
}

pub(super) fn register(r: &mut Registry) {
    r.add(STARTUP, startup);
    r.add(DETACH, |_, _| Answer::Ready(Err("not attached: /detach applies to a client of `mu agent`".into())));
}

fn startup(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let store = l.providers.store();
        let mut warnings: Vec<String> = l.plugins.warnings.clone();
        warnings.extend(store.permission_warning());
        warnings.extend(store.check().err().map(|e| e.to_string()));
        let import_offer = l
            .providers
            .cfg
            .import_offer
            .borrow()
            .as_ref()
            .map(|o| ImportOffer { lines: o.lines.clone(), models_path: models_path() });
        let all = config::load_all_with(&l.cwd, &paths::omp_agent_dir(), &|_| None).0;
        let untrusted_mcp = config::untrusted_files(&l.cwd, &paths::omp_agent_dir())
            .into_iter()
            .map(|(file, digest, names)| UntrustedFile {
                servers: names
                    .into_iter()
                    .map(|name| {
                        let written = all
                            .iter()
                            .find(|e| !e.trusted && e.file == file && e.server.name == name)
                            .map(|e| as_written(&e.server))
                            .unwrap_or_default();
                        UntrustedServer { name, written }
                    })
                    .collect(),
                file,
                digest,
            })
            .collect();
        Ok(Startup { warnings, import_offer, untrusted_mcp, predict: l.providers.cfg.mu.predict.enabled })
    })
}
