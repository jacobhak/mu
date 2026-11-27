//! Signing in to the providers a gateway plugin serves, from `/login`. The plugin names the program (omp's or
//! pi's own login), this runs it on the terminal, and the plugin is told when it exits so it can seal what the
//! program wrote. The program runs on this machine, so it is offered only beside a local host.

use super::child::ChildThen;
use super::*;
use crate::host::gateway_login::{self as host, LoginCommand, LoginRow};
use crate::host::models::REFRESH_SOURCE;
use crate::widgets::overlay::LoginTarget;
use crate::widgets::select::Item;

/// The `/login` rows for the gateway's sign-ins.
pub(super) fn login_items(rows: Vec<LoginRow>) -> Vec<(Item, LoginTarget)> {
    rows.into_iter()
        .map(|r| {
            let item = Item { label: r.label, detail: "signs in here, through the gateway".into() };
            (item, LoginTarget::Gateway(r.id))
        })
        .collect()
}

impl App {
    /// Runs the gateway plugin's sign-in for `provider` (`None`: the program asks which) on the terminal.
    pub(super) fn start_gateway_login(&mut self, provider: Option<String>) {
        if self.host.is_remote() {
            return self.notice("the gateway sign-in runs on the host's terminal: run it there (`mu-gateway login`)");
        }
        if self.screen.child_busy {
            return self.notice("another program already has the terminal");
        }
        self.host_call(host::GATEWAY_LOGIN, &provider, |app, r: Result<LoginCommand, String>| match r {
            Ok(c) => app.start_child(ChildThen::GatewayLogin, move || run(&c)),
            Err(e) => app.error(&format!("cannot start the sign-in: {e}")),
        });
    }

    /// The sign-in program exited: the plugin seals what it wrote, then the model list is read again.
    pub(super) fn finish_gateway_login(&mut self, status: Result<std::process::ExitStatus, String>) {
        let outcome = match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("the sign-in exited with {s}")),
            Err(e) => Err(format!("cannot run the sign-in: {e}")),
        };
        self.host_call(host::GATEWAY_LOGIN_DONE, &(), move |app, r: Result<(), String>| {
            if let Err(e) = r {
                app.error(&format!("the gateway could not keep the sign-in: {e}"));
            }
            match outcome {
                Ok(()) => app.refresh_after_gateway_login(),
                Err(e) => app.error(&e),
            }
        });
    }

    fn refresh_after_gateway_login(&mut self) {
        self.host_call(REFRESH_SOURCE, &"gateway".to_string(), |app, r: Result<Option<String>, String>| {
            match r.unwrap_or_else(|e| Some(format!("gateway models unavailable: {e}"))) {
                None => app.notice("signed in; /switch lists the gateway's models"),
                Some(why) => app.notice(&format!("signed in, but {why}")),
            }
        });
    }
}

/// The sign-in program on the terminal, with Ctrl-C going to it and not to this mu.
fn run(c: &LoginCommand) -> std::io::Result<std::process::ExitStatus> {
    extern "C" fn noop(_: libc::c_int) {}
    let Some((program, args)) = c.argv.split_first() else {
        return Err(std::io::Error::other("the gateway plugin named no program"));
    };
    // SAFETY: installs a no-op handler; restored to the default below.
    unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    let status = std::process::Command::new(program).args(args).envs(c.env.iter().cloned()).status();
    // SAFETY: back to the default disposition.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use crate::host::{Answer, Host, HostInfo, Pushed};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    type Calls = Rc<RefCell<Vec<(String, Value)>>>;

    /// A host that answers each call from a table and records the calls.
    struct Fake {
        info: HostInfo,
        remote: bool,
        calls: Calls,
        gateway: Result<Value, String>,
    }

    impl Host for Fake {
        fn info(&self) -> &HostInfo {
            &self.info
        }

        fn is_remote(&self) -> bool {
            self.remote
        }

        fn call(&self, name: &str, args: Value) -> Answer {
            self.calls.borrow_mut().push((name.to_string(), args));
            Answer::Ready(match name {
                "key_rows" => Ok(json!({"stored": [], "env_set": [], "chatgpt": null, "customs": [], "error": null})),
                host::GATEWAY_LOGINS => Ok(json!([
                    {"id": "anthropic", "label": "Claude Pro/Max (omp)"},
                    {"id": null, "label": "Another provider (omp)"}
                ])),
                host::GATEWAY_LOGIN => self.gateway.clone(),
                host::GATEWAY_LOGIN_DONE => Ok(Value::Null),
                REFRESH_SOURCE => Ok(Value::Null),
                other => Err(format!("unexpected call {other}")),
            })
        }

        fn pushed(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Pushed>> {
            None
        }
    }

    fn app_with(remote: bool, gateway: Result<Value, String>) -> (App, Calls) {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-gateway-login-history"));
        let calls = Rc::new(RefCell::new(Vec::new()));
        app.host = Rc::new(Fake { info: HostInfo::default(), remote, calls: calls.clone(), gateway });
        (app, calls)
    }

    fn names(calls: &Calls) -> Vec<String> {
        calls.borrow().iter().map(|(n, _)| n.clone()).collect()
    }

    #[test]
    fn the_gateways_providers_are_login_rows_after_the_chatgpt_row() {
        let (mut app, _) = app_with(false, Err(String::new()));
        let rows = app.api_key_rows();
        let labels: Vec<&str> = rows.iter().map(|(i, _)| i.label.as_str()).collect();
        assert_eq!(labels[0], "Continue with ChatGPT");
        assert_eq!(&labels[1..3], ["Claude Pro/Max (omp)", "Another provider (omp)"]);
        assert_eq!(rows[1].1, LoginTarget::Gateway(Some("anthropic".into())));
        assert_eq!(rows[2].1, LoginTarget::Gateway(None));
        assert!(matches!(rows[3].1, LoginTarget::ApiKey(_)), "the key rows follow");
    }

    #[test]
    fn a_remote_host_offers_no_gateway_sign_in_and_is_not_asked() {
        let (mut app, calls) = app_with(true, Err(String::new()));
        let rows = app.api_key_rows();
        assert!(rows.iter().all(|(_, t)| !matches!(t, LoginTarget::Gateway(_))));
        assert!(!names(&calls).contains(&host::GATEWAY_LOGINS.to_string()));
        app.start_gateway_login(None);
        assert!(!names(&calls).contains(&host::GATEWAY_LOGIN.to_string()));
        assert!(transcript_text(&app).contains("run it there"), "{}", transcript_text(&app));
    }

    #[test]
    fn a_plugin_that_cannot_name_the_program_is_reported_and_the_terminal_stays() {
        let (mut app, calls) = app_with(false, Err("no gateway: nothing installed".into()));
        in_local(async {
            app.start_gateway_login(Some("anthropic".into()));
            assert_eq!(calls.borrow().last().unwrap().1, json!("anthropic"));
            assert!(!app.screen.child_busy);
            assert!(transcript_text(&app).contains("cannot start the sign-in: no gateway: nothing installed"));
        });
    }

    #[test]
    fn a_finished_sign_in_tells_the_plugin_and_refreshes_the_models() {
        let (mut app, calls) = app_with(false, Err(String::new()));
        app.finish_gateway_login(Ok(std::process::ExitStatus::from_raw(0)));
        assert_eq!(names(&calls), [host::GATEWAY_LOGIN_DONE, REFRESH_SOURCE]);
        assert_eq!(calls.borrow()[1].1, json!("gateway"));
        assert!(transcript_text(&app).contains("signed in; /switch lists the gateway's models"));
    }

    #[test]
    fn a_failed_sign_in_still_tells_the_plugin_but_refreshes_nothing() {
        let (mut app, calls) = app_with(false, Err(String::new()));
        app.finish_gateway_login(Ok(std::process::ExitStatus::from_raw(1 << 8)));
        assert_eq!(names(&calls), [host::GATEWAY_LOGIN_DONE], "what it wrote before failing is still sealed");
        assert!(transcript_text(&app).contains("the sign-in exited with"));
        app.finish_gateway_login(Err("no such file".into()));
        assert!(transcript_text(&app).contains("cannot run the sign-in: no such file"));
    }

    #[test]
    fn the_sign_in_program_runs_with_the_plugins_environment() {
        let c = LoginCommand {
            argv: vec!["sh".into(), "-c".into(), "test \"$MU_LOGIN_TEST\" = yes".into()],
            env: vec![("MU_LOGIN_TEST".into(), "yes".into())],
        };
        assert!(run(&c).unwrap().success());
        assert!(run(&LoginCommand { argv: vec![], env: vec![] }).is_err());
    }
}
