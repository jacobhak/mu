//! Encrypted credential state (`mu_common::sealed`): what a client asks of an agent to unlock it, set
//! its state key up, or change its recipients. The data key crosses only as base64 inside these calls,
//! which travel the encrypted channel; no reply carries it or a private key.

use super::local::{Local, Registry};
use super::{Answer, ready};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mu_common::sealed::{self, Dk, Kind, SealedError, Wrapped};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

pub const STATE_STATUS: &str = "state_status";
pub const STATE_WRAPPED: &str = "state_wrapped";
pub const STATE_UNLOCK: &str = "state_unlock";
pub const STATE_INIT: &str = "state_init";
pub const STATE_ADD_RECIPIENT: &str = "state_add_recipient";
pub const STATE_REMOVE_RECIPIENT: &str = "state_remove_recipient";

/// Wrong keys tolerated per window before `state_unlock` refuses to try.
const MAX_FAILURES: usize = 5;
const WINDOW: Duration = Duration::from_secs(60);

static WANTED: AtomicBool = AtomicBool::new(false);
static FAILURES: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());

/// The agent's configuration asks for state encryption (`[agent] state_key` or a key source).
pub fn set_wanted(wanted: bool) {
    WANTED.store(wanted, Ordering::SeqCst);
}

/// `enabled`: encryption is on or asked for. `initialised`: a state key exists. `locked`: it exists and
/// nothing has unlocked it. A client sets one up when `enabled && !initialised`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateStatus {
    pub enabled: bool,
    pub initialised: bool,
    pub locked: bool,
    pub kind: Option<Kind>,
}

impl StateStatus {
    pub fn needs_init(&self) -> bool {
        self.enabled && !self.initialised
    }
}

#[derive(Serialize, Deserialize)]
pub struct UnlockArgs {
    pub dk: String,
}

#[derive(Serialize, Deserialize)]
pub struct InitArgs {
    pub recipients: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct AddArgs {
    pub recipient: String,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveArgs {
    pub fingerprint: String,
}

/// A recipient of the state key: its public key and fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub key: String,
    pub fingerprint: String,
}

pub fn status(profile_home: &Path) -> StateStatus {
    let initialised = sealed::is_initialised(profile_home);
    StateStatus {
        enabled: initialised || WANTED.load(Ordering::SeqCst),
        initialised,
        locked: initialised && !sealed::is_unlocked(),
        kind: initialised.then(|| sealed::kind(profile_home).ok()).flatten(),
    }
}

/// Seals what was plaintext and unseals omp's directory, once the state key is open. A failure is logged:
/// the agent keeps running with what it has.
pub fn after_unlock(profile_home: &Path) {
    if let Err(e) = mu_provider::auth::sealing::seal_existing(profile_home) {
        eprintln!("mu agent: could not seal the stored credentials: {e:#}");
    }
    if let Err(e) = mu_common::omp_state::materialize(profile_home) {
        eprintln!("mu agent: could not prepare omp's credentials: {e}");
    }
}

pub(super) fn register(r: &mut Registry) {
    r.add(STATE_STATUS, state_status);
    r.add(STATE_WRAPPED, state_wrapped);
    r.add(STATE_UNLOCK, state_unlock);
    r.add(STATE_INIT, state_init);
    r.add(STATE_ADD_RECIPIENT, state_add_recipient);
    r.add(STATE_REMOVE_RECIPIENT, state_remove_recipient);
}

fn state_status(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |_: Value| Ok(status(&l.profile_home)))
}

/// The public keys and wrapped blobs: only while locked, and only for a recipients key.
fn state_wrapped(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |_: Value| {
        let s = status(&l.profile_home);
        if !s.locked || s.kind != Some(Kind::Recipients) {
            return Err("the state is not locked with SSH recipients".to_string());
        }
        sealed::wrapped(&l.profile_home).map_err(|e| e.to_string())
    })
}

fn too_many() -> Option<String> {
    let mut f = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    while f.front().is_some_and(|t| t.elapsed() >= WINDOW) {
        f.pop_front();
    }
    (f.len() >= MAX_FAILURES).then(|| "too many wrong keys: try again in a minute".to_string())
}

fn failed() {
    FAILURES.lock().unwrap_or_else(|e| e.into_inner()).push_back(Instant::now());
}

fn decode_dk(mut text: String) -> Result<Dk, String> {
    let decoded = STANDARD.decode(text.as_bytes()).map(Zeroizing::new);
    text.zeroize();
    let decoded = decoded.map_err(|_| "the data key is not base64".to_string())?;
    let bytes: Zeroizing<[u8; 32]> =
        Zeroizing::new(decoded.as_slice().try_into().map_err(|_| "the data key has the wrong length".to_string())?);
    Ok(Dk::from_bytes(*bytes))
}

fn state_unlock(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |a: UnlockArgs| {
        if let Some(e) = too_many() {
            return Err(e);
        }
        let dk = decode_dk(a.dk)?;
        match sealed::unlock_with_dk(&l.profile_home, &dk) {
            Ok(()) => {}
            Err(SealedError::WrongKey) => {
                failed();
                return Err("wrong key: this is not the key that seals the state".to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
        after_unlock(&l.profile_home);
        Ok(status(&l.profile_home))
    })
}

fn state_init(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |a: InitArgs| {
        let s = status(&l.profile_home);
        if s.initialised {
            return Err("the agent already has a state key".to_string());
        }
        if !s.enabled {
            return Err("state encryption is not turned on for this agent".to_string());
        }
        sealed::init_recipients(&l.profile_home, &a.recipients).map_err(|e| e.to_string())?;
        after_unlock(&l.profile_home);
        Ok(status(&l.profile_home))
    })
}

fn recipients(profile_home: &Path) -> Result<Vec<Recipient>, String> {
    let w: Wrapped = sealed::wrapped(profile_home).map_err(|e| e.to_string())?;
    Ok(w.entries.into_iter().map(|e| Recipient { key: e.key, fingerprint: e.fingerprint }).collect())
}

fn state_add_recipient(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |a: AddArgs| {
        sealed::add_recipient(&l.profile_home, &a.recipient).map_err(|e| e.to_string())?;
        recipients(&l.profile_home)
    })
}

fn state_remove_recipient(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |a: RemoveArgs| {
        sealed::remove_recipient(&l.profile_home, &a.fingerprint).map_err(|e| e.to_string())?;
        recipients(&l.profile_home)
    })
}

#[cfg(test)]
pub(super) fn reset_failures() {
    FAILURES.lock().unwrap_or_else(|e| e.into_inner()).clear();
}
