//! Signing in to the providers a gateway plugin serves (omp's or pi's): the plugin names the program, the UI
//! runs it on its terminal (so it must be the plugin's machine), and the plugin is told when it has exited.

use super::local::{Local, Registry};
use super::{pending, ready};
use serde::{Deserialize, Serialize};
use std::rc::Rc;

pub const GATEWAY_LOGINS: &str = "gateway_logins";
pub const GATEWAY_LOGIN: &str = "gateway_login";
pub const GATEWAY_LOGIN_DONE: &str = "gateway_login_done";

/// A provider the gateway plugin signs in to; without an `id`, the plugin's tool asks which.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginRow {
    pub id: Option<String>,
    pub label: String,
}

/// The program to run on the terminal, and the environment it needs besides its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(super) fn register(r: &mut Registry) {
    r.add(GATEWAY_LOGINS, logins);
    r.add(GATEWAY_LOGIN, login);
    r.add(GATEWAY_LOGIN_DONE, login_done);
}

/// `()`: the providers to offer. A plugin that does not answer is no reason to break `/login`, so a
/// failure is an empty list.
fn logins(l: &Rc<Local>, args: serde_json::Value) -> super::Answer {
    if !l.plugins.has_gateway_plugin() {
        return ready(args, |(): ()| Ok(Vec::<LoginRow>::new()));
    }
    pending(args, |(): ()| {
        let plugins = l.plugins.clone();
        async move {
            let rows = plugins.gateway_logins().await.unwrap_or_default();
            Ok(rows.into_iter().map(|r| LoginRow { id: r.id, label: r.label }).collect::<Vec<_>>())
        }
    })
}

/// `provider`: the program that signs in to it, or to the plugin's own choice for `None`.
fn login(l: &Rc<Local>, args: serde_json::Value) -> super::Answer {
    pending(args, |provider: Option<String>| {
        let plugins = l.plugins.clone();
        async move {
            let c = plugins.gateway_login(provider.as_deref()).await?;
            Ok(LoginCommand { argv: c.argv, env: c.env })
        }
    })
}

/// `()`: the program has exited. The plugin seals what it wrote, and the model list is read again.
fn login_done(l: &Rc<Local>, args: serde_json::Value) -> super::Answer {
    pending(args, |(): ()| {
        let (plugins, providers) = (l.plugins.clone(), l.providers.clone());
        async move {
            let sealed = plugins.gateway_login_done().await;
            providers.invalidate_gateway_models();
            sealed
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_survive_a_round_trip() {
        let c = LoginCommand { argv: vec!["omp".into(), "login".into()], env: vec![("K".into(), "v".into())] };
        let back: LoginCommand = serde_json::from_value(super::super::encode(&c)).unwrap();
        assert_eq!(back, c);
        let r = LoginRow { id: None, label: "Other".into() };
        let back: LoginRow = serde_json::from_value(super::super::encode(&r)).unwrap();
        assert_eq!(back, r);
    }
}
