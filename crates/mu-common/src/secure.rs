//! The encrypted channel of the attach and relay streams: Noise `XX` (25519, ChaChaPoly, BLAKE2s) from the
//! pure-Rust `snow`, carried as lines over whatever pipe a remote's `connect` gives.
//!
//! Every wire line is one JSON string holding base64 of one Noise message, so a reader that knows only
//! JSON lines (`rpc::Framed`, `read_line_capped`) carries it. A plaintext line of any length is cut into
//! chunks that fit a Noise message (a flag byte, `1` while more follow, then the data) and put back
//! together by `Opener`; a line over `rpc::MAX_LINE` is refused.
//!
//! The handshake takes three lines (initiator, responder, initiator). The initiator sees the responder's
//! static key before it sends its own, so it can refuse a key it has pinned differently without telling
//! it anything. Agents keep a static key in the profile home and clients pin it (`check_pin`); a worker
//! and its parent make keys in memory per task.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"mu channel 1";
/// Plaintext bytes per Noise message; the limit is 65535 with the 16-byte tag.
const CHUNK: usize = 60_000;
const MORE: u8 = 1;
const LAST: u8 = 0;

/// A static key pair.
#[derive(Clone)]
pub struct Keypair {
    private: Vec<u8>,
    public: Vec<u8>,
}

impl Keypair {
    pub fn generate() -> Result<Self, String> {
        let builder = snow::Builder::new(PARAMS.parse().map_err(|e| format!("{e}"))?);
        let k = builder.generate_keypair().map_err(|e| format!("cannot make a key: {e}"))?;
        Ok(Self { private: k.private, public: k.public })
    }

    pub fn public(&self) -> &[u8] {
        &self.public
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }

    fn to_text(&self) -> String {
        format!("{}\n{}\n", STANDARD.encode(&self.private), STANDARD.encode(&self.public))
    }

    fn from_text(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let private = STANDARD.decode(lines.next()?.trim()).ok()?;
        let public = STANDARD.decode(lines.next()?.trim()).ok()?;
        (private.len() == 32 && public.len() == 32).then_some(Self { private, public })
    }
}

/// `SHA256:` and the unpadded base64 of the SHA-256 of a public key, as ssh shows host keys.
pub fn fingerprint(public: &[u8]) -> String {
    format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(public)))
}

/// The key at `path`, made (mode 0600, in a 0700 directory) when there is none. Two processes making it
/// at once end up with the same one.
pub fn load_or_create(path: &Path) -> Result<Keypair, String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    match std::fs::read_to_string(path) {
        Ok(text) => return Keypair::from_text(&text).ok_or_else(|| format!("{}: not a key file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fail(e)),
    }
    let key = Keypair::generate()?;
    let tmp = path.with_extension(format!("new.{}", std::process::id()));
    crate::fs::write_private(&tmp, key.to_text().as_bytes()).map_err(fail)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create(path),
        Err(e) => Err(fail(e)),
    }
}

fn keys_dir() -> PathBuf {
    crate::paths::profile_home().join("keys")
}

/// The static key of this profile's agents.
pub fn agent_key() -> Result<Keypair, String> {
    load_or_create(&keys_dir().join("agent.key"))
}

/// The static key of this profile as a client.
pub fn client_key() -> Result<Keypair, String> {
    load_or_create(&keys_dir().join("client.key"))
}

pub fn pins_path() -> PathBuf {
    keys_dir().join("pins.json")
}

/// What `check_pin` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Pinned {
    /// Never seen: it is pinned now.
    First,
    Known,
}

/// Compares `key`, the key agent `id` of remote `remote` showed, with the one pinned in `file`; pins it
/// when there is none. A different key is an error that says so.
pub fn check_pin(file: &Path, remote: &str, id: &str, key: &[u8]) -> Result<Pinned, String> {
    let mut pins: BTreeMap<String, BTreeMap<String, String>> = match std::fs::read_to_string(file) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let now = STANDARD.encode(key);
    match pins.get(remote).and_then(|m| m.get(id)) {
        Some(old) if *old == now => Ok(Pinned::Known),
        Some(old) => {
            let was = STANDARD.decode(old).map(|k| fingerprint(&k)).unwrap_or_else(|_| old.clone());
            Err(format!(
                "the key of agent {id} on remote {remote} is not the one pinned: it was {was}, it now shows {}. \
                 Refusing to connect: someone may sit between you and the agent. If the agent's profile was \
                 reset on purpose, remove the entry from {}",
                fingerprint(key),
                file.display()
            ))
        }
        None => {
            pins.entry(remote.to_string()).or_default().insert(id.to_string(), now);
            let text = serde_json::to_string_pretty(&pins).map_err(|e| e.to_string())?;
            crate::fs::write_private(file, text.as_bytes()).map_err(|e| format!("{}: {e}", file.display()))?;
            Ok(Pinned::First)
        }
    }
}

pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    pub fn initiator(local: &Keypair) -> Result<Self, String> {
        Self::build(local, true)
    }

    pub fn responder(local: &Keypair) -> Result<Self, String> {
        Self::build(local, false)
    }

    fn build(local: &Keypair, initiator: bool) -> Result<Self, String> {
        let builder = snow::Builder::new(PARAMS.parse().map_err(|e| format!("{e}"))?)
            .local_private_key(&local.private)
            .map_err(|e| e.to_string())?
            .prologue(PROLOGUE)
            .map_err(|e| e.to_string())?;
        let state = if initiator { builder.build_initiator() } else { builder.build_responder() };
        Ok(Self { state: state.map_err(|e| format!("cannot start the handshake: {e}"))? })
    }

    pub fn finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// The next handshake line to send.
    pub fn write(&mut self) -> Result<String, String> {
        let mut out = vec![0u8; 65535];
        let n = self.state.write_message(&[], &mut out).map_err(|e| format!("handshake: {e}"))?;
        Ok(encode(&out[..n]))
    }

    /// Takes the peer's handshake line.
    pub fn read(&mut self, line: &Value) -> Result<(), String> {
        let bytes = decode(line)?;
        let mut out = vec![0u8; 65535];
        self.state.read_message(&bytes, &mut out).map_err(|e| format!("handshake: {e}"))?;
        Ok(())
    }

    /// The peer's static key, once its line that carries it has been read.
    pub fn remote_key(&self) -> Option<Vec<u8>> {
        self.state.get_remote_static().map(<[u8]>::to_vec)
    }

    /// The two ends of the channel, and the peer's static key.
    pub fn finish(self) -> Result<(Sealer, Opener, Vec<u8>), String> {
        let remote = self.remote_key().ok_or("the handshake did not finish")?;
        let state = Arc::new(self.state.into_stateless_transport_mode().map_err(|e| format!("handshake: {e}"))?);
        Ok((Sealer { state: state.clone(), nonce: 0 }, Opener { state, nonce: 0, partial: Vec::new() }, remote))
    }
}

fn encode(bytes: &[u8]) -> String {
    Value::String(STANDARD.encode(bytes)).to_string()
}

fn decode(line: &Value) -> Result<Vec<u8>, String> {
    let text = line.as_str().ok_or("not an encrypted line")?;
    STANDARD.decode(text).map_err(|_| "not an encrypted line".to_string())
}

/// The next plaintext line from `lines`, whole: `None` at the end of the stream. A line that fails to
/// decrypt, or is not an encrypted line, is an error.
pub async fn next_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut crate::rpc::Framed<R>,
    opener: &mut Opener,
) -> Result<Option<String>, String> {
    use crate::rpc::Line;
    loop {
        match lines.next_line().await {
            None => return Ok(None),
            Some(Line::Value(v)) => {
                if let Some(line) = opener.open(&v)? {
                    return Ok(Some(line));
                }
            }
            Some(Line::Bad(e)) => return Err(format!("a line of the stream is not encrypted: {e}")),
        }
    }
}

/// Writes `line` through `sealer`, as the wire lines it makes.
pub async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    sealer: &mut Sealer,
    line: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    for wire in sealer.seal(line).map_err(std::io::Error::other)? {
        w.write_all(wire.as_bytes()).await?;
        w.write_all(b"\n").await?;
    }
    Ok(())
}

/// The handshake as the responder, over a stream whose peer opens it: its static key comes back with the
/// channel. The caller bounds the time.
pub async fn accept<R, W>(
    key: &Keypair,
    lines: &mut crate::rpc::Framed<R>,
    w: &mut W,
) -> Result<(Sealer, Opener, Vec<u8>), String>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let mut hs = Handshake::responder(key)?;
    hs.read(&handshake_line(lines).await?)?;
    let reply = hs.write()?;
    let io = |e: std::io::Error| format!("handshake: {e}");
    w.write_all(reply.as_bytes()).await.map_err(io)?;
    w.write_all(b"\n").await.map_err(io)?;
    w.flush().await.map_err(io)?;
    hs.read(&handshake_line(lines).await?)?;
    hs.finish()
}

async fn handshake_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut crate::rpc::Framed<R>,
) -> Result<Value, String> {
    match lines.next_line().await {
        Some(crate::rpc::Line::Value(v)) => Ok(v),
        _ => Err("the stream ended during the handshake".into()),
    }
}

pub struct Sealer {
    state: Arc<snow::StatelessTransportState>,
    nonce: u64,
}

impl Sealer {
    /// `line` as the wire lines to send, in order.
    pub fn seal(&mut self, line: &str) -> Result<Vec<String>, String> {
        let bytes = line.as_bytes();
        let mut out = Vec::new();
        let mut chunks = bytes.chunks(CHUNK).peekable();
        let one = |data: &[u8], more: bool, nonce: &mut u64| -> Result<String, String> {
            let mut plain = Vec::with_capacity(data.len() + 1);
            plain.push(if more { MORE } else { LAST });
            plain.extend_from_slice(data);
            let mut sealed = vec![0u8; plain.len() + 16];
            let n = self.state.write_message(*nonce, &plain, &mut sealed).map_err(|e| format!("encrypt: {e}"))?;
            *nonce += 1;
            Ok(encode(&sealed[..n]))
        };
        if bytes.is_empty() {
            out.push(one(&[], false, &mut self.nonce)?);
        }
        while let Some(c) = chunks.next() {
            out.push(one(c, chunks.peek().is_some(), &mut self.nonce)?);
        }
        Ok(out)
    }
}

pub struct Opener {
    state: Arc<snow::StatelessTransportState>,
    nonce: u64,
    partial: Vec<u8>,
}

impl Opener {
    /// Takes one wire line: the plaintext line it completes, if it does. Any failure (a forged,
    /// repeated, dropped or reordered line) is an error and ends the channel.
    pub fn open(&mut self, wire: &Value) -> Result<Option<String>, String> {
        let sealed = decode(wire)?;
        let mut plain = vec![0u8; sealed.len()];
        let n = self
            .state
            .read_message(self.nonce, &sealed, &mut plain)
            .map_err(|_| "a line of the stream did not decrypt: it was changed, repeated or lost".to_string())?;
        self.nonce += 1;
        let Some((flag, data)) = plain[..n].split_first() else { return Err("an empty message".into()) };
        if self.partial.len() + data.len() > crate::rpc::MAX_LINE {
            return Err(format!("a line longer than {} MiB", crate::rpc::MAX_LINE >> 20));
        }
        self.partial.extend_from_slice(data);
        match *flag {
            MORE => Ok(None),
            LAST => String::from_utf8(std::mem::take(&mut self.partial))
                .map(Some)
                .map_err(|_| "a line that is not text".to_string()),
            _ => Err("a message of an unknown kind".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Ends = (Sealer, Opener, Vec<u8>);

    fn pair() -> (Ends, Ends, Keypair, Keypair) {
        let (a, b) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
        let mut i = Handshake::initiator(&a).unwrap();
        let mut r = Handshake::responder(&b).unwrap();
        let wire = |s: String| serde_json::from_str::<Value>(&s).unwrap();
        r.read(&wire(i.write().unwrap())).unwrap();
        i.read(&wire(r.write().unwrap())).unwrap();
        assert_eq!(i.remote_key().as_deref(), Some(b.public()), "the initiator sees the responder's key early");
        r.read(&wire(i.write().unwrap())).unwrap();
        assert!(i.finished() && r.finished());
        (i.finish().unwrap(), r.finish().unwrap(), a, b)
    }

    fn deliver(s: &mut Sealer, o: &mut Opener, line: &str) -> String {
        let mut got = None;
        for w in s.seal(line).unwrap() {
            let v: Value = serde_json::from_str(&w).unwrap();
            assert!(w.len() < 90_000, "a wire line stays modest");
            got = o.open(&v).unwrap();
        }
        got.expect("the last chunk completes the line")
    }

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir =
            std::env::temp_dir().join(format!("mu-secure-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lines_cross_both_ways_whole_whatever_their_length() {
        let ((mut is, mut io, seen_by_i), (mut rs, mut ro, seen_by_r), a, b) = pair();
        assert_eq!((seen_by_i.as_slice(), seen_by_r.as_slice()), (b.public(), a.public()));
        let long = "x".repeat(300_000);
        for line in ["", r#"{"a":1}"#, long.as_str(), "ünïcode"] {
            assert_eq!(deliver(&mut is, &mut ro, line), line);
            assert_eq!(deliver(&mut rs, &mut io, line), line);
        }
    }

    #[test]
    fn the_wire_holds_no_plaintext() {
        let ((mut is, ..), ..) = pair();
        let secret = "a-very-recognisable-event-text";
        let wire = is.seal(&format!(r#"{{"event":"{secret}"}}"#)).unwrap().join("\n");
        assert!(!wire.contains(secret));
        assert!(!wire.contains("event"));
    }

    #[test]
    fn a_changed_repeated_or_dropped_line_ends_the_channel() {
        let ((mut is, ..), (_, mut ro, ..), ..) = pair();
        let lines: Vec<Value> =
            ["one", "two", "three"].iter().map(|l| serde_json::from_str(&is.seal(l).unwrap()[0]).unwrap()).collect();
        // A flipped byte.
        let mut bad = decode(&lines[0]).unwrap();
        bad[3] ^= 1;
        assert!(ro.open(&Value::String(STANDARD.encode(bad))).is_err());
        // The nonce did not move on failure's account: the real line still opens.
        assert_eq!(ro.open(&lines[0]).unwrap().as_deref(), Some("one"));
        // Repeated, then skipped ahead.
        assert!(ro.open(&lines[0]).is_err());
        assert!(ro.open(&lines[2]).is_err());
        assert_eq!(ro.open(&lines[1]).unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn a_third_party_with_another_key_does_not_pass_for_a_peer() {
        let (a, b) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
        let mut i = Handshake::initiator(&a).unwrap();
        let mut r = Handshake::responder(&b).unwrap();
        let one: Value = serde_json::from_str(&i.write().unwrap()).unwrap();
        r.read(&one).unwrap();
        let two: Value = serde_json::from_str(&r.write().unwrap()).unwrap();
        // A mangled second message does not read.
        let mut raw = decode(&two).unwrap();
        raw[40] ^= 1;
        assert!(i.read(&Value::String(STANDARD.encode(raw))).is_err());
    }

    #[test]
    fn a_key_file_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp().join("keys").join("agent.key");
        let first = load_or_create(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(load_or_create(&path).unwrap().public(), first.public());
        assert!(first.fingerprint().starts_with("SHA256:"));
    }

    #[test]
    fn a_pin_is_made_once_per_remote_and_agent_and_a_change_is_refused() {
        let file = tmp().join("keys").join("pins.json");
        let (a, b) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
        assert_eq!(check_pin(&file, "cluster", "s1", a.public()), Ok(Pinned::First));
        assert_eq!(check_pin(&file, "cluster", "s1", a.public()), Ok(Pinned::Known));
        let refused = check_pin(&file, "cluster", "s1", b.public()).unwrap_err();
        assert!(refused.contains(&a.fingerprint()) && refused.contains(&b.fingerprint()), "{refused}");
        assert!(refused.contains("Refusing to connect"));
        // Another agent, and another remote, have their own pin.
        assert_eq!(check_pin(&file, "cluster", "s2", b.public()), Ok(Pinned::First));
        assert_eq!(check_pin(&file, "devbox", "s1", b.public()), Ok(Pinned::First));
    }
}
