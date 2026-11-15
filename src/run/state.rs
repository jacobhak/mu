//! `mu agent`'s state key at start: an unattended source (`[agent] state_key_file` or
//! `state_key_command`) sets up or opens the key; with `state_key = "ssh"` alone the agent waits for a
//! client (`needs_init`, or locked). What runs once the state is open lives in `mu_tui::host::state`.

use mu_base::config::{MuAgent, StateKey};
use mu_common::sealed::{self, Kind, SealedError};
use std::path::Path;
use zeroize::Zeroizing;

const BAD_KEY: &str = "the state key must be 32 bytes, raw or as 64 hex characters";

fn hex(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let digit = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (i, pair) in text.chunks(2).enumerate() {
        out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

/// The key a file holds or a command prints: 64 hex characters (surrounding whitespace allowed), else
/// exactly 32 raw bytes.
fn parse_key(bytes: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
    if let Some(k) = hex(bytes.trim_ascii()) {
        return Ok(Zeroizing::new(k));
    }
    <[u8; 32]>::try_from(bytes).map(Zeroizing::new).map_err(|_| BAD_KEY.to_string())
}

fn read_source(config: &MuAgent) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    let bytes = if let Some(file) = &config.state_key_file {
        std::fs::read(file).map_err(|e| format!("[agent] state_key_file {file}: {e}"))?
    } else if let Some(command) = &config.state_key_command {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(|e| format!("[agent] state_key_command: {e}"))?;
        if !out.status.success() {
            return Err(format!("[agent] state_key_command failed ({})", out.status));
        }
        out.stdout
    } else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    parse_key(&bytes).map(Some).map_err(|e| format!("[agent] state key: {e}"))
}

/// Sets up or opens the state key from the configured source, and tells the host whether encryption is
/// wanted. Without a source the state stays as it is: locked when a key exists, else waiting for a client.
pub fn start(config: &MuAgent, profile_home: &Path) -> Result<(), String> {
    let source = read_source(config)?;
    mu_tui::host::state::set_wanted(config.state_key == StateKey::Ssh || source.is_some());
    let Some(key) = source else { return Ok(()) };
    if !sealed::is_initialised(profile_home) {
        sealed::init_raw(profile_home, &key).map_err(|e| format!("cannot set up the state key: {e}"))?;
    } else {
        match sealed::kind(profile_home).map_err(|e| e.to_string())? {
            Kind::Raw => sealed::unlock_raw(profile_home, &key).map_err(|e| match e {
                SealedError::WrongKey => {
                    "the key from [agent] state_key_file / state_key_command does not open this profile's state key"
                        .to_string()
                }
                e => e.to_string(),
            })?,
            Kind::Recipients => {
                return Err("this profile's state key is for SSH recipients; remove state_key_file and \
                            state_key_command from [agent], or keys/state.key to start over"
                    .to_string());
            }
        }
    }
    mu_tui::host::state::after_unlock(profile_home);
    Ok(())
}

/// The agent is ending: omp's credentials are sealed back and the memory copy removed, then the key goes.
pub fn shutdown() {
    mu_common::omp_state::shutdown();
    sealed::lock();
}

/// Held by every test in this binary that touches the process-wide state key (here and in `unlock`).
#[cfg(test)]
pub(super) static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> std::path::PathBuf {
        mu_testutil::tmp()
    }

    fn with_file(dir: &Path, bytes: &[u8]) -> MuAgent {
        let f = dir.join("source");
        std::fs::write(&f, bytes).unwrap();
        MuAgent { state_key_file: Some(f.to_string_lossy().into_owned()), ..Default::default() }
    }

    #[test]
    fn a_key_is_raw_bytes_or_hex_and_nothing_else() {
        assert_eq!(*parse_key(&[7u8; 32]).unwrap(), [7u8; 32]);
        let text = "0a".repeat(32);
        assert_eq!(*parse_key(format!("{text}\n").as_bytes()).unwrap(), [10u8; 32]);
        assert_eq!(*parse_key(text.to_uppercase().as_bytes()).unwrap(), [10u8; 32]);
        assert!(parse_key(&[7u8; 31]).is_err());
        assert!(parse_key(&[7u8; 33]).is_err());
        assert!(parse_key("zz".repeat(32).as_bytes()).is_err());
    }

    #[test]
    fn a_file_or_command_key_sets_up_then_opens_the_same_state_key_and_a_wrong_key_is_refused() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        sealed::lock();
        let h = home();
        let config = with_file(&h, &[3u8; 32]);
        start(&config, &h).unwrap();
        assert!(sealed::is_initialised(&h) && sealed::is_unlocked());
        assert_eq!(sealed::kind(&h).unwrap(), Kind::Raw);

        sealed::lock();
        start(&config, &h).unwrap();
        assert!(sealed::is_unlocked(), "the same key opens it again");

        sealed::lock();
        let wrong = with_file(&h, &[4u8; 32]);
        let e = start(&wrong, &h).unwrap_err();
        assert!(e.contains("does not open"), "{e}");
        assert!(!sealed::is_unlocked());

        let command =
            MuAgent { state_key_command: Some("printf %s ".to_string() + &"03".repeat(32)), ..Default::default() };
        start(&command, &h).unwrap();
        assert!(sealed::is_unlocked(), "a command's output is a source as well");

        let broken = MuAgent { state_key_command: Some("exit 3".into()), ..Default::default() };
        assert!(start(&broken, &h).unwrap_err().contains("failed"));
        sealed::lock();
        mu_tui::host::state::set_wanted(false);
    }

    #[test]
    fn an_ssh_agent_with_no_source_waits_and_a_source_does_not_take_over_an_ssh_state_key() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        sealed::lock();
        let h = home();
        let ssh = MuAgent { state_key: StateKey::Ssh, ..Default::default() };
        start(&ssh, &h).unwrap();
        assert!(!sealed::is_initialised(&h) && !sealed::is_unlocked());
        assert!(mu_tui::host::state::status(&h).needs_init());

        let key = ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]));
        sealed::init_recipients(&h, &[key.public_key().to_openssh().unwrap()]).unwrap();
        sealed::lock();
        let e = start(&with_file(&h, &[3u8; 32]), &h).unwrap_err();
        assert!(e.contains("SSH recipients"), "{e}");
        start(&ssh, &h).unwrap();
        assert!(mu_tui::host::state::status(&h).locked);
        mu_tui::host::state::set_wanted(false);
    }

    #[test]
    fn an_unreadable_source_is_a_start_error() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let config = MuAgent { state_key_file: Some("/nonexistent/key".into()), ..Default::default() };
        assert!(start(&config, &home()).unwrap_err().contains("state_key_file"));
    }
}
