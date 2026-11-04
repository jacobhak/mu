//! `mu auth`: manage provider API keys without the TUI (see `mu_provider::auth::store`).

use anyhow::{Result, anyhow, bail};
use mu_base::models::{self, PROVIDERS, ProviderDef};
use mu_provider::auth::store::{Entry, Origin, Store};
use mu_provider::provider::native::{self, Check};
use std::io::{IsTerminal, Read, Write};

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Show the providers that have a key, where it comes from and its last four characters.
    List,
    /// Add or replace a provider's API key. The key is read from stdin (a no-echo prompt on a
    /// terminal), or referenced with --env / --cmd so it is never stored in the file.
    Add {
        /// anthropic, openai or baseten.
        provider: String,
        /// Read the key from this environment variable at request time.
        #[arg(long, conflicts_with = "cmd")]
        env: Option<String>,
        /// Run this shell command at request time; its output is the key.
        #[arg(long)]
        cmd: Option<String>,
        /// Send requests to this URL instead of the provider's (a proxy). https, or http to loopback.
        #[arg(long)]
        base_url: Option<String>,
        /// Save without asking the provider whether the key works.
        #[arg(long)]
        no_validate: bool,
    },
    /// Remove a provider's key; its models go back through omp.
    Rm { provider: String },
    /// Ask the provider whether the stored key works.
    Test { provider: String },
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let store = Store::new();
    let mut out = std::io::stdout();
    match cmd {
        Cmd::List => list(&store, &mut out),
        Cmd::Add { provider, env, cmd, base_url, no_validate } => {
            let def = provider_def(&provider)?;
            let base_url = parse_base_url(base_url)?;
            let entry = match (env, cmd) {
                (Some(n), _) => Entry::reference(&format!("${n}"))?,
                (_, Some(c)) => Entry::reference(&format!("!{c}"))?,
                _ => {
                    let key = read_key(def)?;
                    if key.is_empty() {
                        bail!("no key given");
                    }
                    Entry::literal(&key)
                }
            };
            let entry = Entry { base_url, ..entry };
            if !no_validate {
                match runtime()?.block_on(check_entry(def, &entry)) {
                    Check::Valid => {}
                    Check::Rejected(m) => bail!("{m}; nothing saved"),
                    Check::Unreachable(m) => {
                        bail!("could not check the key: {m}; nothing saved (--no-validate saves anyway)")
                    }
                }
            }
            store.set(def.id, entry)?;
            writeln!(out, "saved the {} key to {}", def.label, store.path().display())?;
            writeln!(out, "{}", native::saved_notice(def))?;
            Ok(0)
        }
        Cmd::Rm { provider } => {
            let def = provider_def(&provider)?;
            if store.remove(def.id)? {
                writeln!(out, "removed the {} key; {} models go through omp again", def.label, def.id)?;
                Ok(0)
            } else {
                eprintln!("no {} key is stored", def.label);
                Ok(1)
            }
        }
        Cmd::Test { provider } => {
            let def = provider_def(&provider)?;
            let entry = store
                .get(def.id)
                .ok_or_else(|| anyhow!("no {} key is stored; run `mu auth add {}`", def.label, def.id))?;
            match runtime()?.block_on(check_entry(def, &entry)) {
                Check::Valid => {
                    writeln!(out, "{}: the key works", def.label)?;
                    Ok(0)
                }
                Check::Rejected(m) | Check::Unreachable(m) => {
                    eprintln!("{}: {m}", def.label);
                    Ok(1)
                }
            }
        }
    }
}

/// `--base-url` as stored: blank means none. Anything else must pass `mu_provider::auth::store::check_base_url`
/// before the key is sent anywhere.
fn parse_base_url(raw: Option<String>) -> Result<Option<String>> {
    raw.filter(|u| !u.trim().is_empty()).map(|u| mu_provider::auth::store::check_base_url(&u)).transpose()
}

fn provider_def(id: &str) -> Result<&'static ProviderDef> {
    models::native_def(id).ok_or_else(|| {
        let known: Vec<&str> = PROVIDERS.iter().map(|d| d.id).collect();
        anyhow!("unknown provider `{id}`; mu can use keys for: {}", known.join(", "))
    })
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

async fn check_entry(def: &ProviderDef, entry: &Entry) -> Check {
    match mu_provider::auth::store::resolve_entry(def.id, entry).await {
        Ok(r) => native::validate(def, &r.key, entry.base_url.as_deref()).await,
        Err(e) => Check::Rejected(e.to_string()),
    }
}

fn list(store: &Store, out: &mut impl Write) -> Result<i32> {
    if let Err(e) = store.check() {
        bail!("{e}");
    }
    if let Some(w) = store.permission_warning() {
        eprintln!("warning: {w}");
    }
    let rows = store.list();
    if rows.is_empty() {
        writeln!(out, "no API keys stored; add one with `mu auth add <provider>` ({})", names())?;
        return Ok(0);
    }
    writeln!(out, "{}", render(&rows))?;
    Ok(0)
}

fn names() -> String {
    PROVIDERS.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
}

/// One line per provider: its source, and for a stored key the last four characters. A `!command`
/// is not run to show anything.
pub fn render(rows: &[(String, Entry)]) -> String {
    let w = rows.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
    rows.iter()
        .filter_map(|(p, e)| {
            let (src, detail) = match e.origin()? {
                Origin::Stored => {
                    ("stored".to_string(), mu_provider::auth::store::mask(e.key.as_deref().unwrap_or("")))
                }
                Origin::Env(n) => {
                    let set = std::env::var(&n).is_ok_and(|v| !v.trim().is_empty());
                    (format!("${n}"), if set { "set".into() } else { "not set".into() })
                }
                Origin::Cmd(c) => (mu_provider::auth::store::cmd_label(&c), String::new()),
            };
            let url = e.base_url.as_deref().map(|u| format!("  {u}")).unwrap_or_default();
            Some(format!("{p:<w$}  {src}  {detail}{url}").trim_end().to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The key from stdin: a no-echo prompt on a terminal, else the piped text.
fn read_key(def: &ProviderDef) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        return Ok(s.trim().to_string());
    }
    eprintln!("Create a key at {}", def.key_url);
    eprint!("{} API key (input is hidden): ", def.label);
    std::io::stderr().flush()?;
    let key = with_echo_off(|| {
        let mut lock = stdin.lock();
        Ok(collect_key(|| {
            let mut b = [0u8];
            loop {
                match lock.read(&mut b) {
                    Ok(1) => return Some(b[0]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => return None,
                }
            }
        }))
    })?;
    eprintln!();
    key.ok_or_else(|| anyhow!("cancelled; nothing saved"))
}

/// Reads a typed key up to a newline, one byte at a time (echo and line editing are off). `None`
/// when cancelled: Ctrl-C, or Ctrl-D on an empty line.
fn collect_key(mut next: impl FnMut() -> Option<u8>) -> Option<String> {
    let mut key = Vec::new();
    while let Some(b) = next() {
        match b {
            b'\n' | b'\r' => break,
            0x03 => return None,
            0x04 if key.is_empty() => return None,
            0x04 => {}
            0x08 | 0x7f => {
                key.pop();
            }
            b => key.push(b),
        }
    }
    Some(String::from_utf8_lossy(&key).trim().to_string())
}

/// The termios state to put back if a signal ends the process while echo is off.
static SAVED_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// Signals that would otherwise leave the terminal without echo.
const RESTORE_ON: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn restore_and_reraise(sig: libc::c_int) {
    if let Some(t) = SAVED_TERMIOS.get() {
        // SAFETY: tcsetattr is async-signal-safe, and the saved state is never changed once set.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    // SAFETY: default action, then re-raise so the exit status still reports the signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Echo and signal keys are off on stdin while this lives. Dropping it, or one of `RESTORE_ON`
/// arriving, puts the terminal back.
struct NoEcho {
    saved: libc::termios,
    handlers: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl NoEcho {
    fn start() -> std::io::Result<Self> {
        // SAFETY: tcgetattr fills the zeroed struct when it succeeds.
        let saved: libc::termios = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            t
        };
        let _ = SAVED_TERMIOS.set(saved);
        let mut quiet = saved;
        // Byte at a time with no signals from keys: Ctrl-C and Ctrl-D reach `collect_key` as bytes.
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        quiet.c_cc[libc::VMIN] = 1;
        quiet.c_cc[libc::VTIME] = 0;
        // SAFETY: `quiet` is `saved` with these flags changed.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut guard = Self { saved, handlers: Vec::new() };
        let handler = restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in RESTORE_ON {
            // SAFETY: the handler only makes async-signal-safe calls.
            let prev = unsafe { libc::signal(sig, handler) };
            if prev != libc::SIG_ERR {
                guard.handlers.push((sig, prev));
            }
        }
        Ok(guard)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        for (sig, prev) in self.handlers.drain(..) {
            // SAFETY: reinstalls the handler that was there before.
            unsafe { libc::signal(sig, prev) };
        }
        // SAFETY: the state tcgetattr returned at start.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) } != 0 {
            eprintln!("warning: could not restore terminal echo; run `stty echo`");
        }
    }
}

/// Runs `f` with terminal echo off on stdin, restored afterwards (also on error and on signals).
fn with_echo_off<T>(f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let _guard = NoEcho::start()?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_shows_at_most_the_last_four_characters() {
        let key = "sk-ant-api03-SECRETPARTabcdefWXYZ";
        let rows = vec![
            ("anthropic".to_string(), Entry::literal(key)),
            ("baseten".to_string(), Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap()),
            ("openai".to_string(), Entry::reference("!op read op://dev/openai/key").unwrap()),
        ];
        let text = render(&rows);
        assert!(text.contains("…WXYZ"), "{text}");
        assert!(!text.contains("SECRETPART") && !text.contains("sk-ant"), "{text}");
        assert!(text.contains("$MU_TEST_SURELY_UNSET_VAR  not set"), "{text}");
        assert!(text.contains("!op …") && !text.contains("read op://"), "{text}");
        for line in text.lines() {
            assert!(!line.contains(&key[..key.len() - 3]));
        }
    }

    #[test]
    fn short_keys_show_nothing() {
        let text = render(&[("anthropic".to_string(), Entry::literal("tiny-key"))]);
        assert!(!text.contains("tiny"), "{text}");
        assert!(!text.contains("-key"), "{text}");
    }

    #[test]
    fn unknown_providers_are_refused_with_the_known_ones() {
        let e = provider_def("gemini").unwrap_err().to_string();
        assert!(e.contains("anthropic") && e.contains("openai") && e.contains("baseten"), "{e}");
        assert_eq!(provider_def("baseten").unwrap().env_hint, "BASETEN_API_KEY");
    }

    fn feed(s: &'static [u8]) -> impl FnMut() -> Option<u8> {
        let mut it = s.iter().copied();
        move || it.next()
    }

    #[test]
    fn typed_keys_are_read_bytewise_and_ctrl_c_cancels() {
        assert_eq!(collect_key(feed(b"sk-abc\r")).as_deref(), Some("sk-abc"));
        assert_eq!(collect_key(feed(b"sk\x7f\x7fx-1\n")).as_deref(), Some("x-1"));
        assert_eq!(collect_key(feed(b"ab\x04\n")).as_deref(), Some("ab"));
        assert_eq!(collect_key(feed(b"sk-ab\x03\n")), None);
        assert_eq!(collect_key(feed(b"\x04")), None);
    }

    #[test]
    fn a_base_url_is_checked_before_a_key_is_sent() {
        assert!(parse_base_url(Some("http://example.com".into())).is_err());
        assert!(parse_base_url(Some("https://user:pw@example.com".into())).is_err());
        assert_eq!(parse_base_url(Some("  ".into())).unwrap(), None);
        assert_eq!(parse_base_url(None).unwrap(), None);
        assert_eq!(
            parse_base_url(Some("https://p.example/v1/".into())).unwrap().as_deref(),
            Some("https://p.example/v1")
        );
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use mu_testutil::block_on as rt;
    use mu_testutil::{Reply, fake_server};

    fn entry(key: &str, base: &str) -> Entry {
        Entry { base_url: Some(base.into()), ..Entry::literal(key) }
    }

    #[test]
    fn a_good_key_is_validated_with_the_providers_auth_header() {
        rt(async {
            let (base, seen) = fake_server(vec![Reply::new(200, r#"{"data":[]}"#)]).await;
            let anthropic = models::native_def("anthropic").unwrap();
            let r = check_entry(anthropic, &entry("ant-key-0123456789", &base)).await;
            assert_eq!(r, Check::Valid);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(req.starts_with("get /v1/models?limit=1 "), "{req}");
            assert!(req.contains("x-api-key: ant-key-0123456789"), "{req}");
            assert!(!req.contains("authorization"), "{req}");

            let (base, seen) = fake_server(vec![Reply::new(200, "{}")]).await;
            let baseten = models::native_def("baseten").unwrap();
            assert_eq!(check_entry(baseten, &entry("bt-key-0123456789", &base)).await, Check::Valid);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(req.starts_with("get /models "), "{req}");
            assert!(req.contains("authorization: bearer bt-key-0123456789"), "{req}");
            assert!(!req.contains("x-api-key"), "{req}");
        });
    }

    #[test]
    fn rejection_and_outages_are_told_apart() {
        rt(async {
            let (base, _) = fake_server(vec![Reply::new(401, "nope")]).await;
            let openai = models::native_def("openai").unwrap();
            assert!(
                matches!(check_entry(openai, &entry("k-0123456789", &base)).await, Check::Rejected(m) if m.contains("rejected"))
            );
            let (base, _) = fake_server(vec![Reply::new(503, "down")]).await;
            assert!(matches!(check_entry(openai, &entry("k-0123456789", &base)).await, Check::Unreachable(_)));
            // nothing listening
            let dead = "http://127.0.0.1:1";
            assert!(matches!(check_entry(openai, &entry("k-0123456789", dead)).await, Check::Unreachable(_)));
        });
    }

    #[test]
    fn an_unset_reference_is_not_validated_as_text() {
        rt(async {
            let openai = models::native_def("openai").unwrap();
            let e = Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap();
            assert!(matches!(check_entry(openai, &e).await, Check::Rejected(m) if m.contains("not set")));
        });
    }
}
