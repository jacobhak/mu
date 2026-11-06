//! The `/web` setup wizard: the app shows each step of `web::wizard` and feeds the answers back.
//! It opens from `/web`, or when the model calls `web_search` with no backend chosen (then the
//! call waits for the outcome).

use super::keys::KeyThen;
use super::*;
use crate::web::wizard::{Answer, Step, Wizard};
use crate::widgets::select::{Item, Select};
use mu_provider::auth::web::{SearchBackend, WebSearch, label};
use mu_provider::provider::providers::KeyTarget;

impl App {
    /// `reply` is the waiting `web_search` call, told whether web search is now ready.
    pub(super) fn start_web_wizard(&mut self, reply: Option<tokio::sync::oneshot::Sender<bool>>) {
        if let Some((_, Some(old))) = self.flows.web_wizard.take() {
            let _ = old.send(false);
        }
        let store = self.services.providers.store().clone();
        let wizard = Wizard::new(store.web(), move |id| store.has(id));
        self.flows.web_wizard = Some((wizard, reply));
        self.web_wizard_show();
    }

    fn web_wizard_show(&mut self) {
        let Some((w, _)) = self.flows.web_wizard.as_ref() else { return };
        match w.step() {
            Step::Pick { title, context, items } => {
                let about = items.iter().map(|i| Some(i.about.clone()).filter(|a| !a.is_empty())).collect();
                let rows = items.into_iter().map(|i| Item { label: i.label, detail: i.detail }).collect();
                self.overlay = Some(Overlay::Web { sel: Select::new(&title, rows).context(&context).details(about) });
            }
            Step::NeedKey(id) => match KeyTarget::find(id) {
                // A key added here serves web search only; chat use is a separate, explicit choice.
                Some(t) => self.start_key_flow(t, Some(false), Some(KeyThen::WebWizard)),
                None => self.web_wizard_answer(Answer::KeyCancelled),
            },
            Step::Done(w) => self.web_wizard_finish(Some(w)),
            Step::Cancelled => self.web_wizard_finish(None),
        }
    }

    fn web_wizard_answer(&mut self, a: Answer) {
        if let Some((w, _)) = self.flows.web_wizard.as_mut() {
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
        let Some((_, reply)) = self.flows.web_wizard.take() else { return };
        let ready = match chosen {
            Some(w) => match self.services.set_web(w.clone()) {
                Ok(()) => {
                    if w.backend == SearchBackend::Off {
                        self.notice("web search is off");
                    } else {
                        self.notice(&format!("web search: {}", label(w.backend, w.keyless)));
                    }
                    w.backend != SearchBackend::Off
                }
                Err(e) => {
                    self.error(&format!("could not save the web search choice: {e}"));
                    false
                }
            },
            None => {
                self.notice("web search was not set up");
                false
            }
        };
        if let Some(tx) = reply {
            let _ = tx.send(ready);
        }
    }
}
