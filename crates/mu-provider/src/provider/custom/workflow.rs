//! The save workflows: a definition in `providers.toml` and its credential in the auth store, checked
//! together and written in that order.

use super::{AuthMode, CustomProvider, Registry};
use crate::auth::store::{Entry, EntryExists, Store};
use anyhow::anyhow;

/// What a failed `save` left behind.
#[derive(Debug)]
pub enum SaveError {
    /// Nothing was written.
    Rejected(anyhow::Error),
    /// The definition is saved; its credential is not.
    KeyNotSaved { id: String, raced: bool, error: anyhow::Error },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "{e:#}"),
            Self::KeyNotSaved { id, raced: true, .. } => write!(
                f,
                "provider `{id}` saved; another credential for it appeared meanwhile and was kept (check it with `mu auth test {id}`)"
            ),
            Self::KeyNotSaved { id, error, .. } => write!(
                f,
                "provider `{id}` saved, but its credential was not saved: {error:#}; add one with /login {id} or `mu auth add {id}`"
            ),
        }
    }
}

impl std::error::Error for SaveError {}

/// Saves a new custom provider: preflight (schema, collisions with another definition or a stored
/// credential), the definition, then its credential. The two files are not one transaction: when
/// the credential write fails the definition stays, unrunnable without a key, and nothing another
/// writer stored is ever removed or replaced. A bearer provider needs `entry` (stored for chat,
/// never with a base URL of its own); a keyless one takes none.
pub fn save(
    registry: &Registry,
    store: &Store,
    id: &str,
    provider: CustomProvider,
    entry: Option<Entry>,
) -> Result<CustomProvider, SaveError> {
    let reject = |e: anyhow::Error| SaveError::Rejected(e);
    let provider = provider.validated(id).map_err(reject)?;
    let entry = match (provider.auth, entry) {
        (AuthMode::None, None) => None,
        (AuthMode::None, Some(_)) => return Err(reject(anyhow!("a keyless provider takes no credential"))),
        (AuthMode::Bearer, None) => return Err(reject(anyhow!("a bearer provider needs a credential"))),
        (AuthMode::Bearer, Some(mut e)) => {
            if e.origin().is_none() {
                return Err(reject(anyhow!("the credential needs a key or a reference")));
            }
            if e.base_url.is_some() {
                return Err(reject(anyhow!("a custom provider's base URL comes from its definition")));
            }
            e.chat = Some(true);
            Some(e)
        }
    };
    if registry.snapshot().map_err(reject)?.contains_key(id) {
        return Err(reject(anyhow!("custom provider `{id}` already exists")));
    }
    if provider.auth == AuthMode::Bearer && store.get(id).is_some() {
        return Err(reject(anyhow!(
            "a credential for `{id}` already exists; remove it with `mu auth rm {id}` or pick another id"
        )));
    }
    registry.add(id, provider.clone()).map_err(reject)?;
    if let Some(entry) = entry
        && let Err(error) = store.set_if_absent(id, entry)
    {
        let raced = error.downcast_ref::<EntryExists>().is_some();
        return Err(SaveError::KeyNotSaved { id: id.to_string(), raced, error });
    }
    Ok(provider)
}

/// What an edit does with the endpoint's credential.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyEdit {
    /// The endpoint keeps the credential the store holds (or none, for a keyless endpoint).
    Keep,
    /// The endpoint takes this newly chosen credential instead.
    Set(Entry),
}

/// Saves an edit of an existing definition: the same checks as [`save`], but the id must already be
/// there, the entry is replaced rather than added, and a stored credential is only written by
/// [`KeyEdit::Set`], so editing a model's reasoning never touches the key. A definition saved as
/// keyless leaves any stored credential in place; removing it stays `mu auth rm`.
pub fn update(
    registry: &Registry,
    store: &Store,
    id: &str,
    provider: CustomProvider,
    key: KeyEdit,
) -> Result<CustomProvider, SaveError> {
    let reject = |e: anyhow::Error| SaveError::Rejected(e);
    let provider = provider.validated(id).map_err(reject)?;
    let entry = match (provider.auth, key) {
        (AuthMode::None, KeyEdit::Set(_)) => {
            return Err(reject(anyhow!("a keyless provider takes no credential")));
        }
        (AuthMode::Bearer, KeyEdit::Set(mut e)) => {
            if e.origin().is_none() {
                return Err(reject(anyhow!("the credential needs a key or a reference")));
            }
            if e.base_url.is_some() {
                return Err(reject(anyhow!("a custom provider's base URL comes from its definition")));
            }
            e.chat = Some(true);
            Some(e)
        }
        _ => None,
    };
    if registry.get(id).map_err(reject)?.is_none() {
        return Err(reject(anyhow!(
            "custom provider `{id}` does not exist in {}; add it with /login custom or `mu providers add`",
            registry.path().display()
        )));
    }
    registry.set(id, provider.clone()).map_err(reject)?;
    if let Some(entry) = entry
        && let Err(error) = store.set(id, entry)
    {
        return Err(SaveError::KeyNotSaved { id: id.to_string(), raced: false, error });
    }
    Ok(provider)
}
