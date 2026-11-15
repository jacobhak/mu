//! The client's half of an agent's encrypted state: when it attaches to an agent whose state is locked,
//! it unwraps the data key with its SSH private key and returns it in `state_unlock`; to an agent that has
//! no state key yet, it offers to set one up for its public key. The calls and the terminal are traits so
//! the flow runs the same before the UI (a prompt), on a reconnect (no prompt) and in tests.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mu_common::sealed::{self, Kind, SealedError, Wrapped};
use mu_tui::host::state::{InitArgs, STATE_INIT, STATE_STATUS, STATE_UNLOCK, STATE_WRAPPED, StateStatus};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Passphrase attempts before the agent is left locked.
const TRIES: usize = 3;

/// Host calls on the agent.
pub(super) trait Calls {
    async fn call(&mut self, name: &str, args: Value) -> Result<Value, String>;
}

pub(super) trait Terminal {
    /// Whether the user can be asked something now.
    fn interactive(&self) -> bool;
    fn say(&mut self, text: &str);
    fn confirm(&mut self, question: &str) -> bool;
    /// A line typed without echo; `None` when none can be read.
    fn secret(&mut self, prompt: &str) -> Option<Zeroizing<String>>;
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Nothing to do: no state key, or it is open already.
    Open,
    Unlocked,
    Initialised,
    /// The agent is still locked, or still has no state key; the user was told why.
    Pending,
}

fn public_key_path(private: &Path) -> PathBuf {
    let mut s = private.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

pub(super) async fn settle(calls: &mut impl Calls, key: Option<&Path>, term: &mut impl Terminal) -> Outcome {
    // An agent from before state encryption has no such call: nothing to settle.
    let Ok(status) = calls.call(STATE_STATUS, Value::Null).await else { return Outcome::Open };
    let Ok(status) = serde_json::from_value::<StateStatus>(status) else { return Outcome::Open };
    if status.locked {
        return match status.kind {
            Some(Kind::Recipients) => unlock(calls, key, term).await,
            Some(Kind::Raw) => {
                term.say(
                    "the agent's state is locked, and its key comes from the agent's own configuration: \
                     it cannot be unlocked from here",
                );
                Outcome::Pending
            }
            None => Outcome::Open,
        };
    }
    if status.needs_init() {
        return init(calls, key, term).await;
    }
    Outcome::Open
}

async fn unlock(calls: &mut impl Calls, key: Option<&Path>, term: &mut impl Terminal) -> Outcome {
    const LOCKED: &str = "the agent's state is locked";
    let Some(path) = key else {
        term.say(&format!("{LOCKED} and no SSH key is set for it: attach with --state-key to unlock"));
        return Outcome::Pending;
    };
    let private = match std::fs::read_to_string(path) {
        Ok(t) => Zeroizing::new(t),
        Err(e) => {
            term.say(&format!(
                "{LOCKED}, and {} cannot be read ({e}): pass --state-key with the key that unlocks it",
                path.display()
            ));
            return Outcome::Pending;
        }
    };
    let wrapped = match calls.call(STATE_WRAPPED, Value::Null).await {
        Ok(v) => match serde_json::from_value::<Wrapped>(v) {
            Ok(w) => w,
            Err(e) => {
                term.say(&format!("{LOCKED}: the agent's wrapped key is unreadable: {e}"));
                return Outcome::Pending;
            }
        },
        Err(e) => {
            term.say(&format!("{LOCKED}: {e}"));
            return Outcome::Pending;
        }
    };
    let needs_passphrase = sealed::identity_needs_passphrase(&private);
    let mut passphrase: Option<Zeroizing<String>> = None;
    let mut wrong = 0;
    let dk = loop {
        match sealed::unwrap_with_identity(&wrapped, &private, passphrase.as_deref().map(String::as_str)) {
            Ok(dk) => break dk,
            Err(SealedError::NotARecipient) => {
                let accepted: Vec<&str> = wrapped.entries.iter().map(|e| e.fingerprint.as_str()).collect();
                term.say(&format!(
                    "{LOCKED}, and {} is not one of the keys that unlock it: the agent accepts {}",
                    path.display(),
                    accepted.join(", ")
                ));
                return Outcome::Pending;
            }
            // The key is a recipient but encrypted: ask once, or again after a wrong passphrase.
            Err(SealedError::Format(_)) if needs_passphrase && passphrase.is_none() => {
                if !term.interactive() {
                    term.say(&format!(
                        "{LOCKED}: attach again with a terminal to unlock it ({} needs its passphrase)",
                        path.display()
                    ));
                    return Outcome::Pending;
                }
                match term.secret(&format!("passphrase for {}: ", path.display())) {
                    Some(p) => passphrase = Some(p),
                    None => {
                        term.say(&format!("{LOCKED}: no passphrase could be read"));
                        return Outcome::Pending;
                    }
                }
            }
            Err(SealedError::WrongKey) if needs_passphrase => {
                wrong += 1;
                term.say("wrong passphrase");
                if wrong >= TRIES {
                    term.say(&format!("{LOCKED}: too many wrong passphrases"));
                    return Outcome::Pending;
                }
                match term.secret(&format!("passphrase for {}: ", path.display())) {
                    Some(p) => passphrase = Some(p),
                    None => return Outcome::Pending,
                }
            }
            Err(e) => {
                term.say(&format!("{LOCKED}: {}: {e}", path.display()));
                return Outcome::Pending;
            }
        }
    };
    let text = Zeroizing::new(STANDARD.encode(dk.as_bytes()));
    drop(dk);
    let args = json!({ "dk": text.as_str() });
    drop(text);
    match calls.call(STATE_UNLOCK, args).await {
        Ok(_) => {
            term.say("the agent's state is unlocked");
            Outcome::Unlocked
        }
        Err(e) => {
            term.say(&format!("{LOCKED}: the agent refused the key: {e}"));
            Outcome::Pending
        }
    }
}

async fn init(calls: &mut impl Calls, key: Option<&Path>, term: &mut impl Terminal) -> Outcome {
    const NONE: &str = "the agent has no state key yet, so its credentials are not encrypted";
    if !term.interactive() {
        term.say(&format!("{NONE}: attach with a terminal to set one up"));
        return Outcome::Pending;
    }
    let Some(private) = key else {
        term.say(&format!("{NONE}: no SSH key is set for it (--state-key)"));
        return Outcome::Pending;
    };
    let public = public_key_path(private);
    let line = match std::fs::read_to_string(&public) {
        Ok(t) => t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string(),
        Err(e) => {
            term.say(&format!("{NONE}, and {} cannot be read ({e})", public.display()));
            return Outcome::Pending;
        }
    };
    if !line.starts_with("ssh-ed25519 ") {
        term.say(&format!(
            "{NONE}: {} is not an ssh-ed25519 key (make one with `ssh-keygen -t ed25519`)",
            public.display()
        ));
        return Outcome::Pending;
    }
    term.say(&format!("{NONE}.\nThe key {} would be the one that unlocks it:\n  {line}", public.display()));
    if !term.confirm("Encrypt the agent's credentials so only that key unlocks them? [y/N] ") {
        term.say("left unencrypted");
        return Outcome::Pending;
    }
    match calls.call(STATE_INIT, json!(InitArgs { recipients: vec![line] })).await {
        Ok(_) => {
            term.say(&format!("the agent's credentials are now encrypted; {} unlocks them", private.display()));
            Outcome::Initialised
        }
        Err(e) => {
            term.say(&format!("{NONE}: it refused: {e}"));
            Outcome::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_common::sealed::{init_recipients, lock};
    use mu_tui::host::state::UnlockArgs;
    use ssh_key::LineEnding;
    use ssh_key::private::Ed25519Keypair;
    use std::sync::MutexGuard;

    fn serial() -> MutexGuard<'static, ()> {
        let g = crate::run::state::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        lock();
        mu_tui::host::state::set_wanted(false);
        g
    }

    /// An agent's side over the sealed state of `home`, as the host calls answer.
    struct Agent {
        home: PathBuf,
        calls: Vec<String>,
    }

    impl Calls for Agent {
        async fn call(&mut self, name: &str, args: Value) -> Result<Value, String> {
            self.calls.push(name.to_string());
            match name {
                STATE_STATUS => Ok(json!(mu_tui::host::state::status(&self.home))),
                STATE_WRAPPED => sealed::wrapped(&self.home).map(|w| json!(w)).map_err(|e| e.to_string()),
                STATE_UNLOCK => {
                    let a: UnlockArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
                    let bytes: [u8; 32] = STANDARD.decode(a.dk).unwrap().try_into().unwrap();
                    sealed::unlock_with_dk(&self.home, &sealed::Dk::from_bytes(bytes)).map_err(|e| e.to_string())?;
                    Ok(Value::Null)
                }
                STATE_INIT => {
                    let a: InitArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
                    sealed::init_recipients(&self.home, &a.recipients).map_err(|e| e.to_string())?;
                    Ok(Value::Null)
                }
                _ => Err(format!("the host has no call {name}")),
            }
        }
    }

    #[derive(Default)]
    struct Script {
        interactive: bool,
        said: Vec<String>,
        yes: bool,
        passphrases: Vec<&'static str>,
        asked: usize,
    }

    impl Terminal for Script {
        fn interactive(&self) -> bool {
            self.interactive
        }

        fn say(&mut self, text: &str) {
            self.said.push(text.to_string());
        }

        fn confirm(&mut self, _: &str) -> bool {
            self.yes
        }

        fn secret(&mut self, _: &str) -> Option<Zeroizing<String>> {
            self.asked += 1;
            (!self.passphrases.is_empty()).then(|| Zeroizing::new(self.passphrases.remove(0).to_string()))
        }
    }

    struct Keys {
        dir: PathBuf,
        private: PathBuf,
        public: String,
    }

    fn keys(seed: u8, passphrase: Option<&str>) -> Keys {
        let dir = mu_testutil::tmp();
        let key = ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        let public = key.public_key().to_openssh().unwrap();
        let stored = match passphrase {
            Some(p) => key.encrypt(&mut rand_core::OsRng, p).unwrap(),
            None => key,
        };
        let private = dir.join("id_ed25519");
        std::fs::write(&private, stored.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        std::fs::write(public_key_path(&private), format!("{public} test\n")).unwrap();
        Keys { dir, private, public }
    }

    fn agent(public: &str) -> Agent {
        let home = mu_testutil::tmp();
        mu_tui::host::state::set_wanted(true);
        init_recipients(&home, &[public.to_string()]).unwrap();
        lock();
        Agent { home, calls: Vec::new() }
    }

    fn run(agent: &mut Agent, key: Option<&Path>, term: &mut Script) -> Outcome {
        mu_testutil::block_on(settle(agent, key, term))
    }

    #[test]
    fn a_locked_agent_is_unlocked_with_the_clients_key_without_a_prompt_when_the_key_has_no_passphrase() {
        let _g = serial();
        let k = keys(1, None);
        let mut a = agent(&k.public);
        let mut t = Script::default();
        assert_eq!(run(&mut a, Some(&k.private), &mut t), Outcome::Unlocked);
        assert!(sealed::is_unlocked());
        assert_eq!(t.asked, 0);
        assert_eq!(a.calls, [STATE_STATUS, STATE_WRAPPED, STATE_UNLOCK]);
        // An agent that is open asks nothing more.
        let mut again = Script::default();
        assert_eq!(run(&mut a, Some(&k.private), &mut again), Outcome::Open);
        let _ = std::fs::remove_dir_all(&k.dir);
    }

    #[test]
    fn a_key_that_is_not_a_recipient_is_refused_naming_what_the_agent_accepts() {
        let _g = serial();
        let (good, other) = (keys(1, None), keys(2, None));
        let mut a = agent(&good.public);
        let mut t = Script::default();
        assert_eq!(run(&mut a, Some(&other.private), &mut t), Outcome::Pending);
        assert!(!sealed::is_unlocked());
        let said = t.said.join("\n");
        let fingerprint = &sealed::wrapped(&a.home).unwrap().entries[0].fingerprint;
        assert!(said.contains("not one of the keys") && said.contains(fingerprint.as_str()), "{said}");
        assert!(!a.calls.contains(&STATE_UNLOCK.to_string()));
    }

    #[test]
    fn an_encrypted_key_asks_for_its_passphrase_and_a_wrong_one_is_refused() {
        let _g = serial();
        let k = keys(3, Some("hunter2"));
        let mut a = agent(&k.public);

        let mut wrong = Script { interactive: true, passphrases: vec!["a", "b", "c"], ..Default::default() };
        assert_eq!(run(&mut a, Some(&k.private), &mut wrong), Outcome::Pending);
        assert!(!sealed::is_unlocked());
        assert_eq!(wrong.asked, 3);
        assert!(wrong.said.iter().any(|s| s.contains("wrong passphrase")), "{:?}", wrong.said);

        let mut right = Script { interactive: true, passphrases: vec!["nope", "hunter2"], ..Default::default() };
        assert_eq!(run(&mut a, Some(&k.private), &mut right), Outcome::Unlocked);
        assert!(sealed::is_unlocked());

        lock();
        let mut quiet = Script::default();
        assert_eq!(run(&mut a, Some(&k.private), &mut quiet), Outcome::Pending, "a reconnect cannot ask");
        assert_eq!(quiet.asked, 0);
        assert!(quiet.said.join("\n").contains("terminal"), "{:?}", quiet.said);
    }

    #[test]
    fn a_missing_key_file_leaves_the_agent_locked_with_a_message() {
        let _g = serial();
        let k = keys(1, None);
        let mut a = agent(&k.public);
        let mut t = Script { interactive: true, ..Default::default() };
        assert_eq!(run(&mut a, Some(Path::new("/nonexistent/id")), &mut t), Outcome::Pending);
        assert_eq!(run(&mut a, None, &mut Script::default()), Outcome::Pending);
        assert!(!sealed::is_unlocked());
    }

    #[test]
    fn an_agent_with_no_state_key_is_set_up_for_the_clients_public_key_only_when_the_user_agrees() {
        let _g = serial();
        let k = keys(4, None);
        let home = mu_testutil::tmp();
        mu_tui::host::state::set_wanted(true);
        let mut a = Agent { home: home.clone(), calls: Vec::new() };

        let mut no = Script { interactive: true, ..Default::default() };
        assert_eq!(run(&mut a, Some(&k.private), &mut no), Outcome::Pending);
        assert!(!sealed::is_initialised(&home));
        assert!(no.said.join("\n").contains(&k.public), "the public key is shown");

        let mut quiet = Script::default();
        assert_eq!(run(&mut a, Some(&k.private), &mut quiet), Outcome::Pending, "a reconnect does not set up");
        assert!(!sealed::is_initialised(&home));

        let mut yes = Script { interactive: true, yes: true, ..Default::default() };
        assert_eq!(run(&mut a, Some(&k.private), &mut yes), Outcome::Initialised);
        assert!(sealed::is_initialised(&home) && sealed::is_unlocked());
        assert_eq!(sealed::wrapped(&home).unwrap().entries[0].key, format!("{} test", k.public).trim());
    }

    #[test]
    fn an_agent_without_the_state_calls_or_with_no_encryption_asks_nothing() {
        let _g = serial();
        struct Old;
        impl Calls for Old {
            async fn call(&mut self, name: &str, _: Value) -> Result<Value, String> {
                Err(format!("the host has no call {name}"))
            }
        }
        let mut t = Script { interactive: true, ..Default::default() };
        assert_eq!(mu_testutil::block_on(settle(&mut Old, None, &mut t)), Outcome::Open);
        let mut plain = Agent { home: mu_testutil::tmp(), calls: Vec::new() };
        assert_eq!(run(&mut plain, None, &mut t), Outcome::Open);
        assert!(t.said.is_empty());
    }
}
