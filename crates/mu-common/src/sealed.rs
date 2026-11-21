//! Sealing credential state at rest under a data key (DK) that is never stored in the clear.
//!
//! A random 256-bit DK seals each protected file with XChaCha20-Poly1305 (fresh random nonce, the file's
//! logical name as associated data) into the envelope `{"sealed":1,"nonce":…,"ct":…}`. The DK lives in
//! `<profile_home>/keys/state.key` only wrapped, and in process memory while unlocked.
//!
//! Wrapping to an SSH ed25519 recipient follows `age`'s `ssh-ed25519` recipient: the public key is
//! converted to its X25519 (Montgomery) form; the sealer makes an ephemeral X25519 key, does ECDH,
//! derives a key with HKDF-SHA256 over (shared secret || ephemeral public || recipient X25519 public),
//! info `mu state-key wrap v1`, and seals the DK with XChaCha20-Poly1305 under it. The blob is
//! `ephemeral public (32) || nonce (24) || ciphertext`. The recipient's X25519 secret is the clamped
//! first 32 bytes of SHA-512(ed25519 seed), as RFC 8032 derives it. A `raw` key (read from a file or
//! command) seals the DK directly. `state.key` also holds a check value, a fixed string sealed under the
//! DK, so a wrong DK is told from a damaged file.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use curve25519_dalek::constants::X25519_BASEPOINT;
use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::montgomery::MontgomeryPoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use ssh_key::{HashAlg, PrivateKey, PublicKey};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

const WRAP_INFO: &[u8] = b"mu state-key wrap v1";
const CHECK_NAME: &str = "state.key-check";
const CHECK_TEXT: &[u8] = b"mu state key";
const RAW_NAME: &str = "state.key-raw";
const VERSION: u32 = 1;
const NONCE: usize = 24;
const ONLY_ED25519: &str = "only ssh-ed25519 keys are supported";

/// The data key. Zeroed on drop; never printed or serialized.
pub struct Dk([u8; 32]);

impl Dk {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Clone for Dk {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for Dk {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Dk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dk(<hidden>)")
    }
}

#[derive(Debug)]
pub enum SealedError {
    Locked,
    NotInitialised,
    AlreadyInitialised,
    WrongKey,
    NotARecipient,
    Tampered,
    Io(std::io::Error),
    Format(String),
}

impl std::fmt::Display for SealedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked => f.write_str("locked: attach to unlock"),
            Self::NotInitialised => f.write_str("no state key is set up"),
            Self::AlreadyInitialised => f.write_str("a state key already exists"),
            Self::WrongKey => f.write_str("wrong key"),
            Self::NotARecipient => f.write_str("this key is not a recipient of the state key"),
            Self::Tampered => f.write_str("sealed data was modified or does not belong here"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Format(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SealedError {}

impl From<std::io::Error> for SealedError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn format(m: impl Into<String>) -> SealedError {
    SealedError::Format(m.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Recipients,
    Raw,
}

/// One recipient's entry: its public key line, the SHA256 fingerprint and the opaque wrapped DK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub fingerprint: String,
    pub blob: String,
}

/// What an agent sends a client to unlock it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wrapped {
    pub entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Blob {
    nonce: String,
    ct: String,
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u32,
    kind: Kind,
    check: Blob,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recipients: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw: Option<Blob>,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    sealed: u32,
    nonce: String,
    ct: String,
}

static UNLOCKED: Mutex<Option<Dk>> = Mutex::new(None);

/// Serialises the tests of this crate that touch the process-global unlocked state.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

fn global() -> std::sync::MutexGuard<'static, Option<Dk>> {
    UNLOCKED.lock().unwrap_or_else(|e| e.into_inner())
}

fn current() -> Result<Dk, SealedError> {
    global().clone().ok_or(SealedError::Locked)
}

fn store(dk: Dk) {
    *global() = Some(dk);
}

pub fn lock() {
    *global() = None;
}

pub fn is_unlocked() -> bool {
    global().is_some()
}

fn state_path(profile_home: &Path) -> PathBuf {
    profile_home.join("keys").join("state.key")
}

fn random<const N: usize>() -> Result<[u8; N], SealedError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| SealedError::Io(std::io::Error::other(e.to_string())))?;
    Ok(b)
}

fn aead_seal(key: &[u8; 32], aad: &str, plain: &[u8]) -> Result<([u8; NONCE], Vec<u8>), SealedError> {
    let nonce = random::<NONCE>()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| format("bad key length"))?;
    let ct = cipher
        .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad: aad.as_bytes() })
        .map_err(|_| format("cannot encrypt"))?;
    Ok((nonce, ct))
}

fn aead_open(key: &[u8; 32], aad: &str, nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>, SealedError> {
    let nonce: [u8; NONCE] = nonce.try_into().map_err(|_| format("bad nonce length"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| format("bad key length"))?;
    cipher.decrypt(&XNonce::from(nonce), Payload { msg: ct, aad: aad.as_bytes() }).map_err(|_| SealedError::Tampered)
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Result<Vec<u8>, SealedError> {
    STANDARD.decode(text).map_err(|_| format("bad base64"))
}

fn seal_blob(key: &[u8; 32], aad: &str, plain: &[u8]) -> Result<Blob, SealedError> {
    let (nonce, ct) = aead_seal(key, aad, plain)?;
    Ok(Blob { nonce: b64(&nonce), ct: b64(&ct) })
}

fn open_blob(key: &[u8; 32], aad: &str, blob: &Blob) -> Result<Vec<u8>, SealedError> {
    aead_open(key, aad, &unb64(&blob.nonce)?, &unb64(&blob.ct)?)
}

fn parse_recipient(line: &str) -> Result<([u8; 32], Entry), SealedError> {
    let line = line.trim();
    if line.split_whitespace().next() != Some("ssh-ed25519") {
        return Err(format(ONLY_ED25519));
    }
    let key = PublicKey::from_openssh(line).map_err(|e| format(format!("not an OpenSSH public key: {e}")))?;
    let ed = key.key_data().ed25519().ok_or_else(|| format(ONLY_ED25519))?;
    let entry =
        Entry { key: line.to_string(), fingerprint: key.fingerprint(HashAlg::Sha256).to_string(), blob: String::new() };
    Ok((ed.0, entry))
}

fn montgomery_public(ed: &[u8; 32]) -> Result<[u8; 32], SealedError> {
    let point = CompressedEdwardsY(*ed).decompress().ok_or_else(|| format("invalid ed25519 public key"))?;
    Ok(point.to_montgomery().to_bytes())
}

fn wrap_key(shared: &[u8; 32], epub: &[u8; 32], rpub: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut ikm = Zeroizing::new([0u8; 96]);
    ikm[..32].copy_from_slice(shared);
    ikm[32..64].copy_from_slice(epub);
    ikm[64..].copy_from_slice(rpub);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2_10::Sha256>::new(None, ikm.as_slice())
        .expand(WRAP_INFO, okm.as_mut_slice())
        .expect("32 bytes is a valid HKDF-SHA256 length");
    okm
}

fn wrap_to(ed: &[u8; 32], dk: &Dk) -> Result<String, SealedError> {
    let rpub = montgomery_public(ed)?;
    let secret = Zeroizing::new(random::<32>()?);
    let epub = X25519_BASEPOINT.mul_clamped(*secret).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(rpub).mul_clamped(*secret).to_bytes());
    let key = wrap_key(&shared, &epub, &rpub);
    let (nonce, ct) = aead_seal(&key, "", dk.as_bytes())?;
    let mut blob = Vec::with_capacity(32 + NONCE + ct.len());
    blob.extend_from_slice(&epub);
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(b64(&blob))
}

fn unwrap_from(seed: &[u8; 32], blob: &str) -> Result<Dk, SealedError> {
    let blob = unb64(blob)?;
    if blob.len() < 32 + NONCE {
        return Err(format("bad wrapped key"));
    }
    let epub: [u8; 32] = blob[..32].try_into().expect("length checked");
    let digest = Sha512::digest(seed);
    let mut scalar = Zeroizing::new([0u8; 32]);
    scalar.copy_from_slice(&digest[..32]);
    let rpub = X25519_BASEPOINT.mul_clamped(*scalar).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(epub).mul_clamped(*scalar).to_bytes());
    if shared.iter().all(|b| *b == 0) {
        return Err(format("bad wrapped key"));
    }
    let key = wrap_key(&shared, &epub, &rpub);
    let plain =
        Zeroizing::new(aead_open(&key, "", &blob[32..32 + NONCE], &blob[32 + NONCE..]).map_err(|e| match e {
            SealedError::Tampered => SealedError::WrongKey,
            e => e,
        })?);
    let bytes: [u8; 32] = plain.as_slice().try_into().map_err(|_| format("bad wrapped key"))?;
    Ok(Dk(bytes))
}

fn load(profile_home: &Path) -> Result<StateFile, SealedError> {
    let text = match std::fs::read(state_path(profile_home)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SealedError::NotInitialised),
        Err(e) => return Err(e.into()),
    };
    let file: StateFile = serde_json::from_slice(&text).map_err(|e| format(format!("state.key: {e}")))?;
    if file.version != VERSION {
        return Err(format(format!("state.key: unsupported version {}", file.version)));
    }
    Ok(file)
}

fn save(profile_home: &Path, file: &StateFile) -> Result<(), SealedError> {
    let text = serde_json::to_vec_pretty(file).map_err(|e| format(e.to_string()))?;
    crate::fs::write_private(&state_path(profile_home), &text)?;
    Ok(())
}

fn check_dk(file: &StateFile, dk: &Dk) -> Result<(), SealedError> {
    match open_blob(dk.as_bytes(), CHECK_NAME, &file.check) {
        Ok(plain) if plain == CHECK_TEXT => Ok(()),
        Ok(_) | Err(SealedError::Tampered) => Err(SealedError::WrongKey),
        Err(e) => Err(e),
    }
}

fn require_new(profile_home: &Path) -> Result<(), SealedError> {
    if state_path(profile_home).exists() { Err(SealedError::AlreadyInitialised) } else { Ok(()) }
}

fn new_dk() -> Result<Dk, SealedError> {
    Ok(Dk(random::<32>()?))
}

/// Makes the state key for `recipients` (OpenSSH `ssh-ed25519` public key lines) and leaves the process unlocked.
pub fn init_recipients(profile_home: &Path, recipients: &[String]) -> Result<(), SealedError> {
    require_new(profile_home)?;
    if recipients.is_empty() {
        return Err(format("no recipients"));
    }
    let dk = new_dk()?;
    let mut entries: Vec<Entry> = Vec::new();
    for line in recipients {
        let (ed, mut entry) = parse_recipient(line)?;
        if entries.iter().any(|e| e.fingerprint == entry.fingerprint) {
            continue;
        }
        entry.blob = wrap_to(&ed, &dk)?;
        entries.push(entry);
    }
    let check = seal_blob(dk.as_bytes(), CHECK_NAME, CHECK_TEXT)?;
    save(profile_home, &StateFile { version: VERSION, kind: Kind::Recipients, check, recipients: entries, raw: None })?;
    store(dk);
    Ok(())
}

/// Makes the state key for an unattended source: the DK is sealed under `key` itself.
pub fn init_raw(profile_home: &Path, key: &[u8; 32]) -> Result<(), SealedError> {
    require_new(profile_home)?;
    let dk = new_dk()?;
    let raw = seal_blob(key, RAW_NAME, dk.as_bytes())?;
    let check = seal_blob(dk.as_bytes(), CHECK_NAME, CHECK_TEXT)?;
    save(
        profile_home,
        &StateFile { version: VERSION, kind: Kind::Raw, check, recipients: Vec::new(), raw: Some(raw) },
    )?;
    store(dk);
    Ok(())
}

pub fn is_initialised(profile_home: &Path) -> bool {
    state_path(profile_home).exists()
}

pub fn kind(profile_home: &Path) -> Result<Kind, SealedError> {
    Ok(load(profile_home)?.kind)
}

pub fn wrapped(profile_home: &Path) -> Result<Wrapped, SealedError> {
    let file = load(profile_home)?;
    if file.kind != Kind::Recipients {
        return Err(format("the state key has no recipients"));
    }
    Ok(Wrapped { entries: file.recipients })
}

/// Unlocks the process with `dk` if it opens this profile's check value; otherwise `WrongKey` and the
/// state stays as it was.
pub fn unlock_with_dk(profile_home: &Path, dk: &Dk) -> Result<(), SealedError> {
    check_dk(&load(profile_home)?, dk)?;
    store(dk.clone());
    Ok(())
}

pub fn unlock_raw(profile_home: &Path, key: &[u8; 32]) -> Result<(), SealedError> {
    let file = load(profile_home)?;
    let raw = match (&file.kind, &file.raw) {
        (Kind::Raw, Some(raw)) => raw,
        _ => return Err(format("the state key is not a raw key")),
    };
    let plain = Zeroizing::new(open_blob(key, RAW_NAME, raw).map_err(|e| match e {
        SealedError::Tampered => SealedError::WrongKey,
        e => e,
    })?);
    let bytes: [u8; 32] = plain.as_slice().try_into().map_err(|_| format("bad wrapped key"))?;
    let dk = Dk(bytes);
    check_dk(&file, &dk)?;
    store(dk);
    Ok(())
}

/// The recipient list of an unlocked profile, with the DK that opens it.
fn unlocked_recipients(profile_home: &Path) -> Result<(StateFile, Dk), SealedError> {
    let dk = current()?;
    let file = load(profile_home)?;
    if file.kind != Kind::Recipients {
        return Err(format("the state key has no recipients"));
    }
    check_dk(&file, &dk)?;
    Ok((file, dk))
}

/// Re-wraps the DK for one more recipient. Needs an unlocked process.
pub fn add_recipient(profile_home: &Path, recipient: &str) -> Result<(), SealedError> {
    let (mut file, dk) = unlocked_recipients(profile_home)?;
    let (ed, mut entry) = parse_recipient(recipient)?;
    if file.recipients.iter().any(|e| e.fingerprint == entry.fingerprint) {
        return Ok(());
    }
    entry.blob = wrap_to(&ed, &dk)?;
    file.recipients.push(entry);
    save(profile_home, &file)
}

/// Drops a recipient's entry. It does not revoke a DK that recipient already unwrapped: that needs a new
/// state key. Needs an unlocked process; the last recipient stays.
pub fn remove_recipient(profile_home: &Path, fingerprint: &str) -> Result<(), SealedError> {
    let (mut file, _dk) = unlocked_recipients(profile_home)?;
    let at = file.recipients.iter().position(|e| e.fingerprint == fingerprint).ok_or(SealedError::NotARecipient)?;
    if file.recipients.len() == 1 {
        return Err(format("cannot remove the last recipient"));
    }
    file.recipients.remove(at);
    save(profile_home, &file)
}

fn parse_identity(private_key: &str) -> Result<PrivateKey, SealedError> {
    PrivateKey::from_openssh(private_key).map_err(|e| format(format!("not an OpenSSH private key: {e}")))
}

/// Whether the private key is passphrase-protected.
pub fn identity_needs_passphrase(private_key: &str) -> bool {
    parse_identity(private_key).is_ok_and(|k| k.is_encrypted())
}

/// Client side: decrypts the entry of `w` that belongs to this key.
pub fn unwrap_with_identity(w: &Wrapped, private_key: &str, passphrase: Option<&str>) -> Result<Dk, SealedError> {
    let key = parse_identity(private_key)?;
    if key.public_key().key_data().ed25519().is_none() {
        return Err(format(ONLY_ED25519));
    }
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let entry = w.entries.iter().find(|e| e.fingerprint == fingerprint).ok_or(SealedError::NotARecipient)?;
    let key = if key.is_encrypted() {
        let pass = passphrase.ok_or_else(|| format("passphrase needed"))?;
        key.decrypt(pass).map_err(|_| SealedError::WrongKey)?
    } else {
        key
    };
    let pair = key.key_data().ed25519().ok_or_else(|| format(ONLY_ED25519))?;
    let seed = Zeroizing::new(pair.private.to_bytes());
    unwrap_from(&seed, &entry.blob)
}

/// Seals `plaintext` under the unlocked DK, bound to the logical file `name`.
pub fn seal(name: &str, plaintext: &[u8]) -> Result<Vec<u8>, SealedError> {
    let dk = current()?;
    let (nonce, ct) = aead_seal(dk.as_bytes(), name, plaintext)?;
    serde_json::to_vec(&Envelope { sealed: 1, nonce: b64(&nonce), ct: b64(&ct) }).map_err(|e| format(e.to_string()))
}

pub fn open(name: &str, envelope: &[u8]) -> Result<Vec<u8>, SealedError> {
    let dk = current()?;
    let env: Envelope = serde_json::from_slice(envelope).map_err(|_| format("not a sealed file"))?;
    if env.sealed != 1 {
        return Err(format(format!("unsupported sealed version {}", env.sealed)));
    }
    aead_open(dk.as_bytes(), name, &unb64(&env.nonce)?, &unb64(&env.ct)?)
}

/// Whether `bytes` start like an envelope, so legacy plaintext can be told from sealed data.
pub fn is_sealed(bytes: &[u8]) -> bool {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
    bytes[start..].starts_with(br#"{"sealed":"#)
}

pub fn write_sealed(path: &Path, name: &str, plaintext: &[u8]) -> Result<(), SealedError> {
    let envelope = seal(name, plaintext)?;
    crate::fs::write_private(path, &envelope)?;
    Ok(())
}

pub fn read_sealed(path: &Path, name: &str) -> Result<Option<Vec<u8>>, SealedError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !is_sealed(&bytes) {
        return Err(format(format!("{}: not a sealed file", path.display())));
    }
    open(name, &bytes).map(Some)
}

#[cfg(test)]
mod tests {
    use super::TEST_SERIAL as SERIAL;
    use super::*;
    use ssh_key::LineEnding;
    use ssh_key::private::Ed25519Keypair;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::MutexGuard;

    /// The unlocked state is process-global: tests that touch it run one at a time, starting locked.
    fn serial() -> MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        lock();
        g
    }

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-sealed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    struct Id {
        private: String,
        public: String,
    }

    fn identity(seed: u8, passphrase: Option<&str>) -> Id {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        let public = key.public_key().to_openssh().unwrap();
        let key = match passphrase {
            Some(p) => key.encrypt(&mut rand_core::OsRng, p).unwrap(),
            None => key,
        };
        Id { private: key.to_openssh(LineEnding::LF).unwrap().to_string(), public }
    }

    fn unlock_via_client(h: &Path, id: &Id, pass: Option<&str>) -> Result<(), SealedError> {
        let w: Wrapped = serde_json::from_str(&serde_json::to_string(&wrapped(h)?).unwrap()).unwrap();
        let dk = unwrap_with_identity(&w, &id.private, pass)?;
        unlock_with_dk(h, &dk)
    }

    #[test]
    fn an_agent_and_a_client_unlock_through_the_wrapped_key() {
        let _g = serial();
        let h = home("round");
        let id = identity(1, None);
        init_recipients(&h, std::slice::from_ref(&id.public)).unwrap();
        assert!(is_unlocked() && is_initialised(&h));
        assert_eq!(kind(&h).unwrap(), Kind::Recipients);
        let sealed = seal("auth.json", b"secret").unwrap();
        lock();
        assert!(!is_unlocked());
        unlock_via_client(&h, &id, None).unwrap();
        assert_eq!(open("auth.json", &sealed).unwrap(), b"secret");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_second_init_is_refused_and_an_uninitialised_profile_says_so() {
        let _g = serial();
        let h = home("twice");
        assert!(matches!(wrapped(&h), Err(SealedError::NotInitialised)));
        assert!(matches!(kind(&h), Err(SealedError::NotInitialised)));
        let id = identity(1, None);
        init_recipients(&h, std::slice::from_ref(&id.public)).unwrap();
        assert!(matches!(init_recipients(&h, &[id.public]), Err(SealedError::AlreadyInitialised)));
        assert!(matches!(init_raw(&h, &[1; 32]), Err(SealedError::AlreadyInitialised)));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_key_that_is_not_a_recipient_is_refused() {
        let _g = serial();
        let h = home("notrecipient");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        lock();
        let other = identity(2, None);
        assert!(matches!(unlock_via_client(&h, &other, None), Err(SealedError::NotARecipient)));
        assert!(!is_unlocked());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_wrong_dk_is_refused_and_the_process_stays_locked() {
        let _g = serial();
        let h = home("wrongdk");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        lock();
        assert!(matches!(unlock_with_dk(&h, &Dk::from_bytes([9; 32])), Err(SealedError::WrongKey)));
        assert!(!is_unlocked());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn locking_makes_seal_and_open_fail() {
        let _g = serial();
        let h = home("locked");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        let sealed = seal("a", b"x").unwrap();
        lock();
        let (s, o) = (seal("a", b"x").unwrap_err(), open("a", &sealed).unwrap_err());
        assert!(matches!(s, SealedError::Locked) && matches!(o, SealedError::Locked));
        assert_eq!(s.to_string(), "locked: attach to unlock");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn tampering_and_a_different_name_are_detected_and_nonces_differ() {
        let _g = serial();
        let h = home("tamper");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        let a = seal("auth.json", b"same").unwrap();
        let b = seal("auth.json", b"same").unwrap();
        assert_ne!(a, b);
        assert!(is_sealed(&a) && !is_sealed(b"{\"openai\":{}}") && !is_sealed(b""));
        assert!(matches!(open("other.json", &a), Err(SealedError::Tampered)));
        let mut env: Envelope = serde_json::from_slice(&a).unwrap();
        let mut ct = STANDARD.decode(&env.ct).unwrap();
        ct[0] ^= 1;
        env.ct = STANDARD.encode(ct);
        assert!(matches!(open("auth.json", &serde_json::to_vec(&env).unwrap()), Err(SealedError::Tampered)));
        assert!(matches!(open("auth.json", b"not json"), Err(SealedError::Format(_))));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn recipients_are_added_and_removed_only_when_unlocked_and_the_last_stays() {
        let _g = serial();
        let h = home("recipients");
        let (one, two) = (identity(1, None), identity(2, None));
        init_recipients(&h, std::slice::from_ref(&one.public)).unwrap();
        lock();
        assert!(matches!(add_recipient(&h, &two.public), Err(SealedError::Locked)));
        assert!(matches!(remove_recipient(&h, "SHA256:x"), Err(SealedError::Locked)));
        unlock_via_client(&h, &one, None).unwrap();
        add_recipient(&h, &two.public).unwrap();
        add_recipient(&h, &two.public).unwrap();
        assert_eq!(wrapped(&h).unwrap().entries.len(), 2);
        lock();
        unlock_via_client(&h, &two, None).unwrap();
        let fp = wrapped(&h).unwrap().entries[0].fingerprint.clone();
        assert!(matches!(remove_recipient(&h, "SHA256:nope"), Err(SealedError::NotARecipient)));
        remove_recipient(&h, &fp).unwrap();
        let left = wrapped(&h).unwrap().entries;
        assert_eq!(left.len(), 1);
        assert!(matches!(remove_recipient(&h, &left[0].fingerprint), Err(SealedError::Format(_))));
        lock();
        assert!(matches!(unlock_via_client(&h, &one, None), Err(SealedError::NotARecipient)));
        unlock_via_client(&h, &two, None).unwrap();
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn an_encrypted_private_key_needs_its_passphrase() {
        let _g = serial();
        let h = home("passphrase");
        let id = identity(3, Some("hunter2"));
        assert!(identity_needs_passphrase(&id.private));
        assert!(!identity_needs_passphrase(&identity(4, None).private));
        assert!(!identity_needs_passphrase("garbage"));
        init_recipients(&h, std::slice::from_ref(&id.public)).unwrap();
        lock();
        let w = wrapped(&h).unwrap();
        let needed = unwrap_with_identity(&w, &id.private, None).unwrap_err();
        assert!(matches!(&needed, SealedError::Format(m) if m == "passphrase needed"));
        assert!(matches!(unwrap_with_identity(&w, &id.private, Some("nope")), Err(SealedError::WrongKey)));
        unlock_via_client(&h, &id, Some("hunter2")).unwrap();
        assert!(is_unlocked());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn only_ed25519_public_keys_are_accepted() {
        let _g = serial();
        let h = home("rsa");
        let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7 someone@host".to_string();
        let e = init_recipients(&h, &[rsa]).unwrap_err();
        assert!(matches!(&e, SealedError::Format(m) if m == "only ssh-ed25519 keys are supported"));
        assert!(matches!(init_recipients(&h, &["ssh-ed25519 !!!".into()]), Err(SealedError::Format(_))));
        assert!(matches!(init_recipients(&h, &[]), Err(SealedError::Format(_))));
        assert!(!is_initialised(&h));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_raw_key_unlocks_and_a_wrong_one_is_refused() {
        let _g = serial();
        let h = home("raw");
        init_raw(&h, &[7; 32]).unwrap();
        assert_eq!(kind(&h).unwrap(), Kind::Raw);
        assert!(matches!(wrapped(&h), Err(SealedError::Format(_))));
        let sealed = seal("a", b"x").unwrap();
        lock();
        assert!(matches!(unlock_raw(&h, &[8; 32]), Err(SealedError::WrongKey)));
        assert!(!is_unlocked());
        unlock_raw(&h, &[7; 32]).unwrap();
        assert_eq!(open("a", &sealed).unwrap(), b"x");
        assert!(matches!(add_recipient(&h, &identity(1, None).public), Err(SealedError::Format(_))));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn state_key_is_private_and_holds_no_dk() {
        let _g = serial();
        let h = home("file");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        let path = state_path(&h);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let dk = current().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!bytes.windows(32).any(|w| w == dk.as_bytes()));
        assert!(!text.contains(&b64(dk.as_bytes())));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn sealed_files_round_trip_and_leave_no_temp_file() {
        let _g = serial();
        let h = home("files");
        init_recipients(&h, &[identity(1, None).public]).unwrap();
        let p = h.join("data").join("auth.json");
        assert!(read_sealed(&p, "auth.json").unwrap().is_none());
        write_sealed(&p, "auth.json", b"{\"k\":\"v\"}").unwrap();
        write_sealed(&p, "auth.json", b"{\"k\":\"w\"}").unwrap();
        assert_eq!(read_sealed(&p, "auth.json").unwrap().unwrap(), b"{\"k\":\"w\"}");
        assert!(!std::fs::read_to_string(&p).unwrap().contains("\"k\""));
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
        assert!(matches!(read_sealed(&p, "other.json"), Err(SealedError::Tampered)));
        std::fs::write(&p, b"{\"plain\":true}").unwrap();
        assert!(matches!(read_sealed(&p, "auth.json"), Err(SealedError::Format(_))));
        lock();
        assert!(matches!(write_sealed(&p, "auth.json", b"x"), Err(SealedError::Locked)));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn dk_debug_hides_the_key() {
        let dk = Dk::from_bytes([0xab; 32]);
        let shown = format!("{dk:?}");
        assert!(!shown.contains("171") && !shown.contains("ab"));
    }
}
