//! `web_search` runs only once the user has chosen a backend. Nothing is contacted, and the web
//! plugin is not started, until then.

use super::ToolCtx;
use mu_base::events::AgentEvent;
use mu_base::tool::ToolOutput;
use mu_provider::auth::store::Store;
use mu_provider::auth::web::{WebStatus, status};
use tokio::sync::oneshot;

pub(super) const OFF: &str =
    "Web search is turned off, so nothing was searched. Run /web or `mu web use <backend>` to turn it on.";
pub(super) const DECLINED: &str = "Web search was not set up, so nothing was searched.";
pub const HEADLESS: &str = "Web search is not set up, so nothing was searched. In interactive mu run /web to choose a search backend, or run `mu web use <backend>` (see `mu web --help`).";

fn refuse(text: &str) -> Box<ToolOutput> {
    Box::new(ToolOutput::err(text))
}

/// `Ok` when a search may run. An interactive host asks the front end to run the setup wizard.
pub(super) async fn gate(store: &Store, interactive: bool, ctx: &ToolCtx) -> Result<(), Box<ToolOutput>> {
    match status(store) {
        WebStatus::Ready(_) => Ok(()),
        WebStatus::Off => Err(refuse(OFF)),
        WebStatus::NotSetUp if !interactive => Err(refuse(HEADLESS)),
        WebStatus::NotSetUp => {
            let (reply, rx) = oneshot::channel();
            if ctx.events.send(AgentEvent::WebSetup { reply }).is_err() {
                return Err(refuse(DECLINED));
            }
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => Err(refuse("Tool call aborted by user")),
                r = rx => match r {
                    Ok(true) if matches!(status(store), WebStatus::Ready(_)) => Ok(()),
                    _ => Err(refuse(DECLINED)),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_provider::auth::store::Entry;
    use mu_provider::auth::web::{SearchBackend, WebSearch};
    use mu_testutil::block_on as rt;
    use tokio_util::sync::CancellationToken;

    fn store() -> (Store, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-gate-{}", uuid::Uuid::new_v4()));
        (Store::at(d.join("auth.json")), d)
    }

    fn ctx() -> (ToolCtx, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            ToolCtx { events, cancel: CancellationToken::new(), call_id: "c".into(), mode: None, wait_interrupt: None },
            rx,
        )
    }

    #[test]
    fn a_headless_host_with_no_settings_refuses_with_the_headless_text() {
        let (s, d) = store();
        let (c, mut rx) = ctx();
        let e = rt(gate(&s, false, &c)).unwrap_err();
        assert!(e.is_error && e.text == HEADLESS, "{}", e.text);
        assert!(rx.try_recv().is_err(), "no wizard is requested");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn off_refuses_and_a_ready_backend_passes() {
        let (s, d) = store();
        let (c, _rx) = ctx();
        s.set_web(Some(WebSearch::off())).unwrap();
        assert_eq!(rt(gate(&s, true, &c)).unwrap_err().text, OFF);
        s.set_web(Some(WebSearch::keyed(SearchBackend::Openai))).unwrap();
        s.set("openai", Entry::literal("sk-test-0123456789")).unwrap();
        assert!(rt(gate(&s, false, &c)).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_interactive_host_asks_the_front_end_and_a_no_means_declined() {
        let (s, d) = store();
        let (c, mut rx) = ctx();
        rt(async {
            let answer = async {
                let Some(AgentEvent::WebSetup { reply }) = rx.recv().await else { panic!("no WebSetup event") };
                let _ = reply.send(false);
            };
            let (r, ()) = tokio::join!(gate(&s, true, &c), answer);
            assert_eq!(r.unwrap_err().text, DECLINED);
        });
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_yes_without_a_ready_backend_is_still_declined_and_a_dropped_reply_too() {
        let (s, d) = store();
        let (c, mut rx) = ctx();
        rt(async {
            let answer = async {
                let Some(AgentEvent::WebSetup { reply }) = rx.recv().await else { panic!() };
                let _ = reply.send(true);
            };
            let (r, ()) = tokio::join!(gate(&s, true, &c), answer);
            assert_eq!(r.unwrap_err().text, DECLINED);
            let drop_it = async {
                let _ = rx.recv().await;
            };
            let (r, ()) = tokio::join!(gate(&s, true, &c), drop_it);
            assert_eq!(r.unwrap_err().text, DECLINED);
        });
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_refused_call_never_starts_the_plugin() {
        use crate::plugin::{host::PluginHost, manifest};
        mu_testutil::home();
        let dir = std::env::temp_dir().join(format!("mu-gate-plug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = manifest::Manifest {
            name: "web".into(),
            command: vec!["sh".into(), "-c".into(), "touch started; cat".into()],
            lifetime: manifest::Lifetime::parse("session").unwrap(),
            hooks: vec![],
            required_hooks: vec![],
            provider: None,
            tools: vec![manifest::ToolSpec::with_schema("web_search", serde_json::json!({}))],
            timeout: std::time::Duration::from_secs(300),
            dir: dir.clone(),
        };
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let tools = crate::tools::Tools::new(dir.clone(), PluginHost::new(vec![m], vec![], ntx, Default::default()));
        let (c, _rx) = ctx();
        let out = mu_testutil::block_on_local(async { tools.call("web_search", &serde_json::json!({}), &c).await });
        assert!(out.is_error && out.text == HEADLESS, "{}", out.text);
        assert!(!dir.join("started").exists(), "the plugin was started");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelling_while_the_wizard_is_open_aborts_the_call() {
        let (s, d) = store();
        let (c, _rx) = ctx();
        c.cancel.cancel();
        let e = rt(gate(&s, true, &c)).unwrap_err();
        assert!(e.text.contains("aborted"), "{}", e.text);
        let _ = std::fs::remove_dir_all(d);
    }
}
