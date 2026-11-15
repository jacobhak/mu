//! Calling the host from the UI: the call goes out, and `then` runs with the reply, at once when the
//! host answered at once, otherwise from the event loop when the reply arrives.

use super::*;
use crate::host::{Answer, Pushed, decode, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;

impl App {
    pub(super) fn host_call<A, R>(
        &mut self,
        name: &'static str,
        args: &A,
        then: impl FnOnce(&mut App, Result<R, String>) + 'static,
    ) where
        A: Serialize,
        R: DeserializeOwned + 'static,
    {
        match self.host.call(name, encode(args)) {
            Answer::Ready(r) => then(self, decode(r)),
            Answer::Pending(fut) => {
                let tx = self.ui_tx.clone();
                tokio::task::spawn_local(async move {
                    let r = decode(fut.await);
                    let _ = tx.send(UiMsg::Host(Box::new(move |app| then(app, r))));
                });
            }
        }
    }
}

impl App {
    /// A value the host sent unasked. Each surface that pushes adds an arm for its name.
    pub(super) fn on_pushed(&mut self, p: Pushed) {
        self.screen.dirty = true;
        #[allow(clippy::match_single_binding)]
        match p.name.as_str() {
            crate::host::chatgpt::URL | crate::host::chatgpt::NOTICE | crate::host::chatgpt::DONE => {
                self.on_chatgpt_pushed(p)
            }
            crate::host::program::PROGRAM_OUTPUT => self.on_program_output(p),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use super::*;
    use crate::host::{HostInfo, Pushed};
    use serde_json::Value;
    use tokio::sync::oneshot;

    /// A host that answers every call later, when the test says so, as a remote one does.
    struct Slow {
        info: HostInfo,
        gate: std::cell::RefCell<Option<oneshot::Receiver<Value>>>,
    }

    impl Host for Slow {
        fn info(&self) -> &HostInfo {
            &self.info
        }

        fn call(&self, _: &str, _: Value) -> Answer {
            let gate = self.gate.borrow_mut().take();
            Answer::Pending(Box::pin(async move {
                match gate {
                    Some(rx) => rx.await.map_err(|e| e.to_string()),
                    None => Err("no gate".into()),
                }
            }))
        }

        fn pushed(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Pushed>> {
            None
        }
    }

    #[test]
    fn a_local_answer_runs_the_reply_at_once() {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-hostcall-a"));
        app.host_call(
            crate::host::completion::SKILLS,
            &(),
            |app, r: Result<Vec<crate::host::completion::SkillRow>, String>| {
                assert!(r.unwrap().is_empty());
                app.notice("answered");
            },
        );
        assert!(transcript_text(&app).contains("answered"));
    }

    #[test]
    fn a_pending_answer_comes_back_as_a_ui_message() {
        in_local(async {
            let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-hostcall-b"));
            let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
            app.ui_tx = ui_tx;
            let (gate, rx) = oneshot::channel();
            app.host = Rc::new(Slow { info: HostInfo::default(), gate: std::cell::RefCell::new(Some(rx)) });
            app.host_call("anything", &(), |app, r: Result<String, String>| app.notice(&format!("got {}", r.unwrap())));
            assert!(!transcript_text(&app).contains("got"), "nothing runs before the reply");
            gate.send(Value::String("it".into())).unwrap();
            let m = ui_rx.recv().await.unwrap();
            app.on_ui(m);
            assert!(transcript_text(&app).contains("got it"));
        });
    }

    #[test]
    fn an_unknown_call_is_the_error_of_the_reply() {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-hostcall-c"));
        app.host_call("no-such-call", &(), |app, r: Result<(), String>| app.error(&r.unwrap_err()));
        assert!(transcript_text(&app).contains("the host has no call no-such-call"));
    }

    #[test]
    fn an_unknown_slash_command_and_skill_are_reported() {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-hostcall-d"));
        app.submit("/nosuch".into(), false);
        app.submit("/skill:nope".into(), false);
        let t = transcript_text(&app);
        assert!(t.contains("unknown command /nosuch"), "{t}");
        assert!(t.contains("unknown skill nope"), "{t}");
    }

    #[test]
    fn skills_and_plugins_list_what_the_host_has() {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-hostcall-e"));
        app.cmd_skills("");
        app.cmd_plugins("");
        let t = transcript_text(&app);
        assert!(t.contains("no skills") && t.contains("no plugins"), "{t}");
    }
}
