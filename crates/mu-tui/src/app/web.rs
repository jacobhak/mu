//! The `/web` setup wizard: the app shows each step of `web::wizard` and feeds the answers back.
//! It opens from `/web`, or when the model calls `web_search` with no backend chosen (then the
//! call waits for the outcome).

use super::keys::KeyThen;
use super::*;
use crate::host::keys::KeyInfo;
use crate::host::web::WebState;
use crate::web::wizard::{Answer, Step, Wizard};
use crate::widgets::select::{Item, Select};
use mu_provider::auth::web::{SearchBackend, WebSearch, label};

impl App {
    /// `reply` is the waiting `web_search` call, told whether web search is now ready.
    pub(super) fn start_web_wizard(&mut self, reply: Option<tokio::sync::oneshot::Sender<bool>>) {
        self.leave_custom_wizard();
        self.host_call(host::web::WEB_STATE, &(), move |app, r: Result<WebState, String>| {
            let state = r.unwrap_or_else(|e| {
                app.error(&format!("could not read the web search choice: {e}"));
                WebState::default()
            });
            let wizard = Wizard::new(state.current, move |id| state.keys.iter().any(|k| k == id));
            app.flows.begin(Active::Web(Box::new((wizard, reply))));
            app.web_wizard_show();
        });
    }

    fn web_wizard_show(&mut self) {
        let Some((w, _)) = self.flows.web() else { return };
        match w.step() {
            Step::Pick { title, context, items } => {
                let about = items.iter().map(|i| Some(i.about.clone()).filter(|a| !a.is_empty())).collect();
                let rows = items.into_iter().map(|i| Item { label: i.label, detail: i.detail }).collect();
                self.open_overlay(Overlay::Web { sel: Select::new(&title, rows).context(&context).details(about) });
            }
            Step::NeedKey(id) => {
                self.host_call(host::keys::KEY_TARGET, &id, |app, r: Result<Option<KeyInfo>, String>| match r {
                    // A key added here serves web search only; chat use is a separate, explicit choice.
                    Ok(Some(t)) => app.start_key_flow(t, Some(false), Some(KeyThen::WebWizard)),
                    _ => app.web_wizard_answer(Answer::KeyCancelled),
                })
            }
            Step::Done(w) => self.web_wizard_finish(Some(w)),
            Step::Cancelled => self.web_wizard_finish(None),
        }
    }

    fn web_wizard_answer(&mut self, a: Answer) {
        if let Some((w, _)) = self.flows.web_mut() {
            w.answer(a);
            self.web_wizard_show();
        }
    }

    pub(super) fn on_web_pick(&mut self, row: Option<usize>) {
        self.web_wizard_answer(row.map_or(Answer::Cancel, Answer::Pick));
    }

    /// The key flow the wizard started ended.
    pub(super) fn web_wizard_key_done(&mut self, saved: bool) {
        self.web_wizard_answer(if saved { Answer::KeySaved } else { Answer::KeyCancelled });
    }

    fn web_wizard_finish(&mut self, chosen: Option<WebSearch>) {
        let Some((_, reply)) = self.flows.end_web() else { return };
        let Some(w) = chosen else {
            self.notice("web search was not set up");
            if let Some(tx) = reply {
                let _ = tx.send(false);
            }
            return;
        };
        self.host_call(host::web::SET_WEB, &w, move |app, r: Result<(), String>| {
            let ready = match r {
                Ok(()) => {
                    if w.backend == SearchBackend::Off {
                        app.notice("web search is off");
                    } else {
                        app.notice(&format!("web search: {}", label(w.backend, w.keyless)));
                    }
                    w.backend != SearchBackend::Off
                }
                Err(e) => {
                    app.error(&format!("could not save the web search choice: {e}"));
                    false
                }
            };
            if let Some(tx) = reply {
                let _ = tx.send(ready);
            }
        });
    }
}
