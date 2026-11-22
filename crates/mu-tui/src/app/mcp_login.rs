//! Signing in to an OAuth MCP server in the TUI: the browser flow with its pasted-redirect fallback.
//! The flow runs on the host, in the `mu-mcp-http` bridge; this file drives it and shows the results.

use super::*;
use crate::host::mcp_login::{self as host, Done, Flow, Notice, Paste, Start, Url};
use crate::keymap;
use crossterm::event::KeyEvent;

/// A sign-in waiting for the browser's callback or for the redirect URL typed into the composer.
/// The composer is locked to that input until the flow ends.
pub(super) struct McpLoginFlow {
    /// Tells a late result from the current flow's.
    pub(super) id: u64,
    pub(super) name: String,
}

/// Whether the client may open `url`: `https`, with nothing in it a shell or terminal could misread.
/// The URL comes from the host, which may be a remote agent.
fn openable(url: &str) -> bool {
    url.starts_with("https://") && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

impl App {
    /// Whether a sign-in waits; the composer is locked to it meanwhile.
    pub(super) fn mcp_login_typing(&self) -> bool {
        self.flows.mcp_login().is_some()
    }

    /// `/login mcp:<name>`: starts the sign-in of a configured server, here or on a remote host.
    pub(super) fn run_mcp_login(&mut self, name: &str) {
        let name = name.to_string();
        self.host_call(crate::host::mcp::MCP_NAMES, &(), move |app, r: Result<Vec<String>, String>| {
            if !r.unwrap_or_default().contains(&name) {
                return app.error(&format!("no MCP server `{name}` is configured"));
            }
            app.start_mcp_login(name);
        });
    }

    fn start_mcp_login(&mut self, name: String) {
        self.leave_custom_wizard();
        if let Some(f) = self.flows.mcp_login() {
            let waiting = f.name.clone();
            return self.notice(&format!(
                "a sign-in to mcp:{waiting} is already waiting: paste its redirect URL, or press {}",
                keymap::WIZARD_CANCEL.label()
            ));
        }
        self.flows.mcp_login_seq += 1;
        let id = self.flows.mcp_login_seq;
        // The flow is live before the host answers: its URL can arrive ahead of the reply.
        self.flows.begin(Active::McpLogin(McpLoginFlow { id, name: name.clone() }));
        self.prompt.composer.set_private(true);
        self.prompt.composer.set_closed(true);
        self.host_call(
            host::MCP_LOGIN_START,
            &Start { flow: id, name: name.clone() },
            move |app, r: Result<(), String>| match r {
                Ok(()) => app.notice(&format!("signing in to mcp:{name}…")),
                Err(e) => {
                    if app.flows.mcp_login().is_some_and(|f| f.id == id) {
                        app.flows.end_mcp_login();
                        app.unlock_composer();
                    }
                    app.error(&format!("mcp:{name}: could not start the sign-in: {e}"));
                }
            },
        );
    }

    /// A value the host pushed for the sign-in: its URL, a line it reports, or its end.
    pub(super) fn on_mcp_login_pushed(&mut self, p: crate::host::Pushed) {
        match p.name.as_str() {
            host::URL => {
                if let Ok(u) = serde_json::from_value::<Url>(p.data) {
                    self.on_mcp_login_url(u.flow, &u.url);
                }
            }
            host::NOTICE => {
                if let Ok(n) = serde_json::from_value::<Notice>(p.data)
                    && self.flows.mcp_login().is_some_and(|f| f.id == n.flow)
                {
                    self.notice(&n.text);
                }
            }
            host::DONE => {
                if let Ok(d) = serde_json::from_value::<Done>(p.data) {
                    self.on_mcp_login_done(d.flow, d.error.map_or(Ok(()), Err));
                }
            }
            _ => {}
        }
    }

    /// Opens the sign-in link when it is safe to, and shows it with how to finish by hand.
    fn on_mcp_login_url(&mut self, flow: u64, url: &str) {
        let Some(name) = self.flows.mcp_login().filter(|f| f.id == flow).map(|f| f.name.clone()) else { return };
        if !cfg!(test) {
            if openable(url) {
                let _ = mu_provider::auth::chatgpt::browser::open_browser(url);
            } else {
                self.notice("the sign-in link is not https, so no browser was opened");
            }
        }
        self.notice(&format!(
            "A browser should open to log in to mcp:{name}. If it does not, open this link:\n{url}\n\
             If the browser can't reach this machine, paste the final redirect URL here and press {} ({} cancels).",
            keymap::SUBMIT.label(),
            keymap::WIZARD_CANCEL.label()
        ));
    }

    /// Keys the redirect-URL input takes before the composer. True when handled.
    pub(super) fn mcp_login_key(&mut self, k: KeyEvent) -> bool {
        if keymap::WIZARD_CANCEL.hit(&k) {
            self.cancel_mcp_login();
            return true;
        }
        if keymap::SUBMIT.hit(&k) {
            self.send_mcp_redirect();
            return true;
        }
        false
    }

    /// Enter in the redirect-URL input: the bridge takes the text. The composer is cleared and stays
    /// locked until the sign-in ends.
    fn send_mcp_redirect(&mut self) {
        let text = self.prompt.composer.secret_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.prompt.composer.take_secret();
        if let Some(flow) = self.flows.mcp_login().map(|f| f.id) {
            self.host_call(host::MCP_LOGIN_PASTE, &Paste { flow, text }, |app, r: Result<(), String>| {
                if let Err(e) = r {
                    app.error(&e);
                }
            });
        }
        self.notice("checking the redirect URL…");
    }

    /// Esc while a sign-in waits: the flow ends, and its late result is ignored.
    fn cancel_mcp_login(&mut self) {
        let Some(f) = self.flows.end_mcp_login() else { return };
        self.host_call(host::MCP_LOGIN_CANCEL, &Flow { flow: f.id }, |_, _: Result<(), String>| {});
        self.unlock_composer();
        self.notice("sign-in cancelled; nothing changed");
    }

    pub(super) fn on_mcp_login_done(&mut self, flow: u64, r: Result<(), String>) {
        let Some(name) = self.flows.mcp_login().filter(|f| f.id == flow).map(|f| f.name.clone()) else { return };
        self.flows.end_mcp_login();
        self.unlock_composer();
        match r {
            Ok(()) => self.after_mcp_login(name),
            Err(e) => self.error(&format!("mcp:{name}: login did not finish: {e}")),
        }
    }

    /// Makes the server usable in this session: restarts it on the new credential and lists its tools.
    fn after_mcp_login(&mut self, name: String) {
        self.notice(&format!("mcp:{name}: logged in; loading tools…"));
        self.host_call(
            crate::host::mcp::MCP_AFTER_LOGIN,
            &name.clone(),
            move |app, r: Result<(usize, bool), String>| match r {
                Ok((n, changed)) => {
                    app.notice(&format!("mcp:{name}: logged in, {n} tools"));
                    if changed {
                        app.send(Command::RefreshTools);
                    }
                }
                Err(e) => app.error(&format!("mcp:{name}: logged in, but its tools could not be listed: {e}")),
            },
        );
    }
}
