//! What every front end needs before its agent exists: config, context, plugins and providers.

use mu_base::{config, secrets};
use mu_common::paths;
use mu_provider::provider::client::Providers;
use mu_tools::{context, plugin, tools};
use std::path::PathBuf;
use std::rc::Rc;

/// Everything a session needs that depends only on the working directory.
pub struct Setup {
    pub cfgs: Rc<config::Configs>,
    pub ctx: Rc<context::Context>,
    pub host: Rc<plugin::host::PluginHost>,
    pub providers: Rc<Providers>,
    pub notice_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pub notice_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// Prints notices to stderr until told to stop, then prints whatever is still queued.
pub fn spawn_notice_printer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (done, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                n = rx.recv() => match n {
                    Some(n) => eprintln!("{n}"),
                    None => break,
                },
                _ = &mut done_rx => {
                    while let Ok(n) = rx.try_recv() {
                        eprintln!("{n}");
                    }
                    break;
                }
            }
        }
    });
    (done, handle)
}

pub fn setup(cwd: &std::path::Path) -> Setup {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let cfgs = Rc::new(config::Configs::load());
    let ctx =
        Rc::new(context::Context::load_all(cwd, &home, &paths::omp_agent_dir(), cfgs.omp.skills.enable_claude_user));
    let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    secrets::configure(cfgs.mu.secrets.env.clone());
    if let Err(e) = tools::edit::set_mode(&cfgs.mu.edit.mode) {
        let _ = notice_tx.send(format!("warning: {e}"));
    }
    for w in cfgs.warnings.iter().chain(&ctx.warnings) {
        let _ = notice_tx.send(format!("warning: {w}"));
    }
    let (manifests, mut warnings) = plugin::manifest::discover(&paths::mu_home().join("plugins"));
    let (servers, mcp_warnings) = mu_tools::mcp::config::load(cwd, &paths::omp_agent_dir());
    warnings.extend(mcp_warnings);
    let mcp_lifetime = match plugin::manifest::Lifetime::parse(&cfgs.mu.mcp.lifetime) {
        Some(l) => l,
        None => {
            if !cfgs.mu.mcp.lifetime.is_empty() {
                warnings.push(format!("[mcp] lifetime `{}` is not valid; using session", cfgs.mu.mcp.lifetime));
            }
            plugin::manifest::Lifetime::Session
        }
    };
    let host = plugin::host::PluginHost::new_with_mcp(
        manifests,
        servers,
        mcp_lifetime,
        mu_tools::mcp::cache::dir(),
        warnings,
        notice_tx.clone(),
        plugin::host::InitParams { cwd: cwd.to_string_lossy().into(), ..Default::default() },
        tools::is_builtin,
    );
    let providers = Rc::new(Providers::new(
        cfgs.clone(),
        mu_provider::provider::endpoint::GatewaySource::new(host.gateway_start()),
    ));
    // A malformed providers.toml is reported here; model resolution fails on it until it is fixed.
    if let Err(e) = providers.custom_registry().snapshot() {
        let _ = notice_tx.send(format!("warning: {e:#}"));
    }
    host.set_providers(&providers);
    Setup { cfgs, ctx, host, providers, notice_tx, notice_rx }
}
