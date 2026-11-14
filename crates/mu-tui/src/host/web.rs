//! `/web` (table row "/web"): the stored search choice and which keys it could use.

use super::local::{Local, Registry};
use super::{Answer, ready};
use mu_provider::auth::web::WebSearch;
use serde::{Deserialize, Serialize};
use std::rc::Rc;

pub const WEB_STATE: &str = "web_state";
pub const SET_WEB: &str = "set_web";

/// What the wizard starts from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebState {
    pub current: Option<WebSearch>,
    /// The ids that have a stored key.
    pub keys: Vec<String>,
}

pub(super) fn register(r: &mut Registry) {
    r.add(WEB_STATE, web_state);
    r.add(SET_WEB, set_web);
}

fn web_state(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let store = l.providers.store();
        Ok(WebState { current: store.web(), keys: store.list().into_iter().map(|(p, _)| p).collect() })
    })
}

fn set_web(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |w: WebSearch| l.providers.store().set_web(Some(w)).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_provider::auth::web::SearchBackend;

    #[test]
    fn the_state_survives_the_wire() {
        let s = WebState {
            current: Some(WebSearch::keyless(SearchBackend::Exa, "2026-01-01T00:00:00Z".into())),
            keys: vec!["exa".into()],
        };
        let back: WebState = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
