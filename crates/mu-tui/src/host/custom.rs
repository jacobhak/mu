//! The custom providers panel (table row "Custom providers panel"): the registry and key store it
//! reads and writes, and the one `/models` check it runs.

use super::local::{Local, Registry};
use super::{Answer, pending, ready};
use crate::custom::panel::{Destinations, Fetch, Probe, Saved};
use mu_provider::auth::store::{Entry, resolve_entry};
use mu_provider::model::discovery::{DiscoverError, custom_commit, custom_fetch};
use mu_provider::provider::custom::{self, CustomProvider, KeyEdit, SaveError};
use serde::{Deserialize, Serialize};
use std::rc::Rc;

pub const CUSTOM_OPEN: &str = "custom_open";
pub const CUSTOM_GET: &str = "custom_get";
pub const CUSTOM_ENV: &str = "custom_env";
pub const CUSTOM_FETCH: &str = "custom_fetch";
pub const CUSTOM_SAVE: &str = "custom_save";

/// Stands in for a stored literal key in `custom_get`'s reply: the key never leaves the host. A
/// probe carrying it is checked with the stored key, and a save carrying it keeps the stored key.
pub const KEY_MARKER: &str = "mu:stored-key";

/// What a new endpoint's panel needs: which ids are taken, and the files a save writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Open {
    /// Ids of the custom definitions.
    pub names: Vec<String>,
    /// Ids that hold a stored credential.
    pub keys: Vec<String>,
    pub dests: Destinations,
}

/// An existing endpoint, as its panel starts: `def` is `None` when there is none by that id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edit {
    pub def: Option<CustomProvider>,
    pub entry: Option<Entry>,
    pub dests: Destinations,
}

/// What a Save left behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Saved,
    Failed {
        message: String,
        /// The definition is saved though the credential is not.
        written: bool,
    },
}

pub(super) fn register(r: &mut Registry) {
    r.add(CUSTOM_OPEN, open);
    r.add(CUSTOM_GET, get);
    r.add(CUSTOM_ENV, env);
    r.add(CUSTOM_FETCH, fetch);
    r.add(CUSTOM_SAVE, save);
}

fn dests(l: &Local) -> Destinations {
    Destinations {
        providers: l.providers.custom_registry().path().display().to_string(),
        auth: l.providers.store().path().display().to_string(),
    }
}

fn open(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let names = l.providers.custom_registry().snapshot().map_err(|e| format!("{e:#}"))?.into_keys().collect();
        let keys = l.providers.store().list().into_iter().map(|(id, _)| id).collect();
        Ok(Open { names, keys, dests: dests(l) })
    })
}

/// `id`: the definition and the credential the store holds for it.
fn get(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |id: String| {
        let def = l.providers.custom_registry().get(&id).map_err(|e| format!("{e:#}"))?;
        let entry = def.as_ref().and_then(|_| l.providers.store().get(&id)).map(|mut e| {
            if e.key.is_some() {
                e.key = Some(KEY_MARKER.into());
            }
            e
        });
        Ok(Edit { entry, def, dests: dests(l) })
    })
}

/// `name`: whether the environment variable is set to something other than blanks.
fn env(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |name: String| Ok(std::env::var(name).is_ok_and(|v| !v.trim().is_empty())))
}

/// The endpoint's `/models` with the draft's credential. Nothing is written.
fn fetch(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |probe: Probe| {
        let probe = pin_probe(l, probe);
        async move {
            let probe = match probe {
                Ok(p) => p,
                Err(f) => return Ok(f),
            };
            if let Some(key) = probe.credential.as_ref().and_then(|e| e.key.as_deref()) {
                mu_base::secrets::register_stored(&custom::env_hint(&probe.id), key);
            }
            Ok(read_listing(&probe).await)
        }
    })
}

/// Swaps the marker for the stored key, and keeps a stored key to its stored `base_url`: the
/// client's `base_url` counts only for an endpoint that has no stored key.
pub(super) fn pin_probe(l: &Local, mut probe: Probe) -> Result<Probe, Fetch> {
    let stored = l.providers.store().get(&probe.id);
    if probe.credential.as_ref().is_some_and(|e| e.key.as_deref() == Some(KEY_MARKER)) {
        match &stored {
            Some(s) => probe.credential = Some(s.clone()),
            None => return Err(Fetch::Credential(format!("no key is stored for {}", probe.id))),
        }
    }
    if stored.is_some() {
        let saved = l.providers.custom_registry().get(&probe.id).ok().flatten();
        let trim = |u: &str| u.trim_end_matches('/').to_string();
        match saved {
            Some(def) if trim(&def.base_url) == trim(&probe.base_url) => {}
            Some(def) => {
                return Err(Fetch::Failed(format!(
                    "the stored key for {} goes only to {}; save the endpoint with the new URL first",
                    probe.id, def.base_url
                )));
            }
            None => return Err(Fetch::Failed(format!("the stored key for {} has no saved endpoint", probe.id))),
        }
    }
    Ok(probe)
}

async fn read_listing(probe: &Probe) -> Fetch {
    let provider = match CustomProvider::new(&probe.base_url).validated(&probe.id) {
        Ok(p) => p,
        Err(e) => return Fetch::Failed(mu_base::secrets::redact_keys(&format!("{e:#}"))),
    };
    let key = match &probe.credential {
        Some(entry) => match resolve_entry(&probe.id, entry).await {
            Ok(r) => Some(r.key),
            Err(e) => return Fetch::Credential(mu_base::secrets::redact_keys(&format!("{e:#}"))),
        },
        None => None,
    };
    match custom_fetch(&provider, key.as_deref()).await {
        Ok(rows) => Fetch::Listed(rows),
        Err(DiscoverError::Rejected(m)) => Fetch::Rejected(m),
        Err(DiscoverError::Failed(m)) => Fetch::Failed(m),
    }
}

/// Writes the definition, then the key, then caches the listing that was read.
fn save(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |saved: Saved| {
        let Saved { id, provider, entry, editing, key_changed, listed } = saved;
        if entry.as_ref().is_some_and(|e| e.key.as_deref() == Some(KEY_MARKER)) && !(editing && !key_changed) {
            return Err("the key shown as stored was not entered; enter the key again".into());
        }
        let (registry, store) = (l.providers.custom_registry(), l.providers.store());
        let written = if editing {
            // An edit only writes the key it was given; otherwise the stored one stays.
            let key = match (key_changed, entry) {
                (true, Some(entry)) => KeyEdit::Set(entry),
                _ => KeyEdit::Keep,
            };
            custom::update(registry, store, &id, provider, key)
        } else {
            custom::save(registry, store, &id, provider, entry)
        };
        Ok(match written {
            Ok(p) => {
                if let Some(rows) = listed {
                    custom_commit(&p, rows);
                }
                Outcome::Saved
            }
            Err(e) => Outcome::Failed { written: matches!(e, SaveError::KeyNotSaved { .. }), message: e.to_string() },
        })
    })
}
