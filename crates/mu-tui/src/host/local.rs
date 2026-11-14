//! The host that runs in the UI's own process: it answers from the provider registry, the context
//! and the plugin host, which are what the UI used to hold itself.

use super::{Answer, Host, HostInfo, Pushed};
use mu_common::paths;
use mu_provider::provider::client::Providers;
use mu_tools::context::Context;
use mu_tools::plugin::host::PluginHost;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// What the handlers work on.
pub struct Local {
    pub providers: Rc<Providers>,
    pub ctx: Rc<Context>,
    pub plugins: Rc<PluginHost>,
    pub cwd: PathBuf,
    /// Where a handler's background task sends values the UI did not ask for.
    pub push: UnboundedSender<Pushed>,
}

pub type Handler = fn(&Rc<Local>, Value) -> Answer;

/// The calls the local host answers, by name. Each surface module adds its own in `register`.
#[derive(Default)]
pub struct Registry(HashMap<&'static str, Handler>);

impl Registry {
    pub fn add(&mut self, name: &'static str, handler: Handler) {
        let was = self.0.insert(name, handler);
        debug_assert!(was.is_none(), "host call {name} is registered twice");
    }
}

pub struct LocalHost {
    local: Rc<Local>,
    info: HostInfo,
    handlers: HashMap<&'static str, Handler>,
    pushed: RefCell<Option<UnboundedReceiver<Pushed>>>,
}

impl LocalHost {
    pub fn new(providers: Rc<Providers>, ctx: Rc<Context>, plugins: Rc<PluginHost>, cwd: PathBuf) -> Self {
        let (push, pushed) = unbounded_channel();
        let info = HostInfo {
            home: paths::home(),
            profile_home: paths::profile_home(),
            tmp: std::env::temp_dir(),
            cwd: cwd.clone(),
            profile: std::env::var("OMP_PROFILE")
                .or_else(|_| std::env::var("PI_PROFILE"))
                .ok()
                .filter(|p| !p.is_empty()),
        };
        let mut registry = Registry::default();
        super::startup::register(&mut registry);
        super::completion::register(&mut registry);
        super::models::register(&mut registry);
        super::keys::register(&mut registry);
        super::web::register(&mut registry);
        super::chatgpt::register(&mut registry);
        super::custom::register(&mut registry);
        super::mcp::register(&mut registry);
        super::sessions::register(&mut registry);
        super::plans::register(&mut registry);
        super::memory::register(&mut registry);
        super::program::register(&mut registry);
        Self {
            local: Rc::new(Local { providers, ctx, plugins, cwd, push }),
            info,
            handlers: registry.0,
            pushed: RefCell::new(Some(pushed)),
        }
    }
}

impl Host for LocalHost {
    fn info(&self) -> &HostInfo {
        &self.info
    }

    fn call(&self, name: &str, args: Value) -> Answer {
        match self.handlers.get(name) {
            Some(h) => h(&self.local, args),
            None => Answer::Ready(Err(format!("the host has no call {name}"))),
        }
    }

    fn pushed(&self) -> Option<UnboundedReceiver<Pushed>> {
        self.pushed.borrow_mut().take()
    }
}
