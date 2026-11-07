//! `mu auth`: manage provider API keys and the ChatGPT sign-in without the TUI (see
//! `mu_provider::auth::store` and `mu_provider::auth::chatgpt`).

use anyhow::{Result, anyhow, bail};
use mu_provider::auth::chatgpt::{self, Account, PROVIDER_ID, USAGE_URL, oauth, signin};
use mu_provider::auth::check::{self, Check};
use mu_provider::auth::store::{Entry, Origin, Store};
use mu_provider::provider::custom::{AuthMode, CustomProvider, KeyState, Registry};
use mu_provider::provider::providers;
use mu_provider::provider::providers::{KeyTarget, SEARCH_KEYS};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use tokio_util::sync::CancellationToken;

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Show the keys and the ChatGPT sign-in: where each comes from and what it is used for.
    List,
    /// Add or replace an API key. The key is read from stdin (a no-echo prompt on a
    /// terminal), or referenced with --env / --cmd so it is never stored in the file.
    Add {
        /// anthropic, openai, baseten (chat models) or parallel, exa (web search).
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
        /// anthropic and openai: its chat models use this key, billed per token instead of your omp login.
        #[arg(long, conflicts_with = "no_chat")]
        chat: bool,
        /// anthropic and openai: keep chat on omp; the key is used only where you choose it, such as `mu web use`.
        #[arg(long)]
        no_chat: bool,
    },
    /// Remove a key; a provider's chat models go back through omp. `openai-chatgpt` signs out instead.
    Rm { provider: String },
    /// Sign in with ChatGPT: `openai-chatgpt/*` models then run on your ChatGPT plan. Opens a browser;
    /// over SSH, paste the final redirect URL back into the terminal.
    Login {
        /// `openai-chatgpt` (the default; `chatgpt` also works).
        provider: Option<String>,
    },
    /// Sign out of ChatGPT and revoke the sign-in. The registration is kept, so the next sign-in reuses it.
    Logout {
        /// `openai-chatgpt` (the default; `chatgpt` also works).
        provider: Option<String>,
        /// Also forget the registration, so the next sign-in may be another ChatGPT account. The old app
        /// stays listed in your ChatGPT settings until you disconnect it there.
        #[arg(long)]
        forget: bool,
    },
    /// Ask the provider whether the stored key works; for `openai-chatgpt`, whether the plan answers.
    Test { provider: String },
    /// Choose whether an anthropic or openai key serves chat models.
    Use {
        provider: String,
        #[arg(long, conflicts_with = "no_chat")]
        chat: bool,
        #[arg(long)]
        no_chat: bool,
    },
}

pub fn run(cmd: Cmd) -> Result<i32> {
    exec(&Store::new(), &Registry::new(), cmd, &mut std::io::stdout())
}

/// `run` on the given key store and custom-provider registry.
fn exec(store: &Store, registry: &Registry, cmd: Cmd, out: &mut impl Write) -> Result<i32> {
    match cmd {
        Cmd::List => list(store, registry, out),
        Cmd::Login { provider } => {
            chatgpt_only("login", provider.as_deref())?;
            login(store, out)
        }
        Cmd::Logout { provider, forget } => {
            chatgpt_only("logout", provider.as_deref())?;
            logout(store, out, forget)
        }
        Cmd::Add { provider, env, cmd, base_url, chat, no_chat, .. } if is_chatgpt(&provider) => {
            Err(chatgpt_key_refusal(env.is_some() || cmd.is_some() || base_url.is_some(), chat || no_chat))
        }
        Cmd::Add { provider, env, cmd, base_url, no_validate, chat, no_chat } => {
            let target = target(&provider, registry)?;
            if let KeyTarget::Custom(c) = &target {
                if base_url.is_some() {
                    bail!(
                        "--base-url does not apply to custom provider `{}`: its base URL is set in providers.toml",
                        c.id
                    );
                }
                if c.auth == AuthMode::None {
                    bail!(
                        "custom provider `{}` takes no key (auth = \"none\"); set auth = \"bearer\" in providers.toml before attaching a key",
                        c.id
                    );
                }
            }
            let chat = chat_flag(&target, chat, no_chat)?;
            let base_url = parse_base_url(base_url)?;
            let entry = match (env, cmd) {
                (Some(n), _) => Entry::reference(&format!("${n}"))?,
                (_, Some(c)) => Entry::reference(&format!("!{c}"))?,
                _ => {
                    let key = read_key(&target)?;
                    if key.is_empty() {
                        bail!("no key given");
                    }
                    Entry::literal(&key)
                }
            };
            let entry = Entry { base_url, chat, ..entry };
            if !no_validate {
                match mu_base::runtime::current_thread_runtime()?.block_on(check_entry(&target, &entry)) {
                    Check::Valid | Check::Unchecked => {}
                    Check::Rejected(m) => bail!("{m}; nothing saved"),
                    Check::Unreachable(m) => {
                        bail!("could not check the key: {m}; nothing saved (--no-validate saves anyway)")
                    }
                }
            }
            store.set(target.id(), entry)?;
            writeln!(out, "saved the {} key to {}", target.label(), store.path().display())?;
            match &target {
                KeyTarget::Provider(def) => writeln!(out, "{}", providers::saved_notice(def, chat == Some(true)))?,
                KeyTarget::Custom(c) => writeln!(out, "{}/* models use this key, sent to {}", c.id, c.base_url)?,
                KeyTarget::Search(_) => {}
            }
            Ok(0)
        }
        Cmd::Rm { provider } if is_chatgpt(&provider) => logout(store, out, false),
        Cmd::Rm { provider } => {
            let Some(target) = KeyTarget::find(&provider, registry)? else {
                // An orphan: a stored key whose custom definition is gone.
                if store.remove(&provider)? {
                    writeln!(
                        out,
                        "removed the stored key for `{provider}`; no custom provider `{provider}` is configured"
                    )?;
                    return Ok(0);
                }
                return Err(unknown(&provider));
            };
            if let KeyTarget::Custom(c) = &target
                && c.auth == AuthMode::None
            {
                eprintln!(
                    "{} takes no key (auth = \"none\"), so there is no key to remove; to remove the endpoint, run `mu providers rm {}`",
                    c.label(),
                    c.id
                );
                return Ok(1);
            }
            let was_chat =
                matches!(target, KeyTarget::Provider(_)) && store.get(target.id()).is_some_and(|e| e.routes_chat());
            if store.remove(target.id())? {
                match &target {
                    KeyTarget::Custom(c) => writeln!(
                        out,
                        "removed the {} key; {id}/* models now need a key: add one with `mu auth add {id}`, or remove the endpoint with `mu providers rm {id}`",
                        c.label(),
                        id = c.id
                    )?,
                    _ if was_chat => writeln!(
                        out,
                        "removed the {} key; {}/* models go through omp again",
                        target.label(),
                        target.id()
                    )?,
                    _ => writeln!(out, "removed the {} key", target.label())?,
                }
                Ok(0)
            } else {
                eprintln!("no {} key is stored", target.label());
                Ok(1)
            }
        }
        Cmd::Test { provider } if is_chatgpt(&provider) => chatgpt_test(store, out),
        Cmd::Test { provider } => {
            let target = target(&provider, registry)?;
            let keyless = matches!(&target, KeyTarget::Custom(c) if c.auth == AuthMode::None);
            let check = match &target {
                // A keyless endpoint has no stored entry to check: its `/models` is asked without a key.
                KeyTarget::Custom(c) if keyless => {
                    mu_base::runtime::current_thread_runtime()?.block_on(check::validate_custom(c, None))
                }
                _ => {
                    let entry = store.get(target.id()).ok_or_else(|| {
                        anyhow!("no {} key is stored; run `mu auth add {}`", target.label(), target.id())
                    })?;
                    mu_base::runtime::current_thread_runtime()?.block_on(check_entry(&target, &entry))
                }
            };
            match check {
                Check::Valid => {
                    let what = if keyless { "the endpoint answers without a key" } else { "the key works" };
                    writeln!(out, "{}: {what}", target.label())?;
                    Ok(0)
                }
                Check::Unchecked => {
                    writeln!(out, "{}: no free way to test the key; it resolves", target.label())?;
                    Ok(0)
                }
                Check::Rejected(m) | Check::Unreachable(m) => {
                    eprintln!("{}: {m}", target.label());
                    Ok(1)
                }
            }
        }
        Cmd::Use { provider, .. } if is_chatgpt(&provider) => {
            bail!("`mu auth use` does not apply to {PROVIDER_ID}: its models run on your ChatGPT plan once signed in")
        }
        Cmd::Use { provider, chat, no_chat } => {
            let target = target(&provider, registry)?;
            if !target.asks_chat() {
                bail!("`mu auth use` only applies to anthropic and openai");
            }
            let Some(chat) = chat_flag(&target, chat, no_chat)? else { unreachable!("asks_chat requires a flag") };
            if store.set_chat(target.id(), chat)? {
                if chat {
                    writeln!(out, "{}/* models now use your API key (billed per token) instead of omp.", target.id())?;
                } else {
                    writeln!(out, "{}/* chat models go through omp again; the key stays for web search", target.id())?;
                }
                Ok(0)
            } else {
                eprintln!("no {} key is stored", target.label());
                Ok(1)
            }
        }
    }
}

/// What `--chat` / `--no-chat` mean for a target: anthropic and openai need one of them,
/// Baseten's key is only for chat, and search keys have no chat use.
fn chat_flag(t: &KeyTarget, chat: bool, no_chat: bool) -> Result<Option<bool>> {
    match t {
        KeyTarget::Search(_) if chat || no_chat => bail!("--chat/--no-chat only apply to anthropic and openai"),
        KeyTarget::Search(_) => Ok(None),
        _ if !t.asks_chat() => {
            if no_chat {
                bail!("a {} key is only used for chat models", t.label());
            }
            Ok(Some(true))
        }
        _ if chat || no_chat => Ok(Some(chat)),
        _ => bail!(
            "say whether {id}/* chat models should use this key, billed per token, instead of your omp login: add --chat or --no-chat",
            id = t.id()
        ),
    }
}

/// `--base-url` as stored: blank means none. Anything else must pass `mu_provider::auth::store::check_base_url`
/// before the key is sent anywhere.
fn parse_base_url(raw: Option<String>) -> Result<Option<String>> {
    raw.filter(|u| !u.trim().is_empty()).map(|u| mu_provider::auth::store::check_base_url(&u)).transpose()
}

/// The key target `id` names: a built-in or search service, or a custom endpoint in `registry`.
fn target(id: &str, registry: &Registry) -> Result<KeyTarget> {
    KeyTarget::find(id, registry)?.ok_or_else(|| unknown(id))
}

fn unknown(id: &str) -> anyhow::Error {
    anyhow!(
        "unknown provider `{id}`; mu can use keys for: {}, a custom provider from `mu providers add`, or sign in with ChatGPT as {PROVIDER_ID}",
        names()
    )
}

async fn check_entry(t: &KeyTarget, entry: &Entry) -> Check {
    check::check_target(t, entry).await.unwrap_or_else(Check::Rejected)
}

fn list(store: &Store, registry: &Registry, out: &mut impl Write) -> Result<i32> {
    if let Err(e) = store.check() {
        bail!("{e}");
    }
    if let Some(w) = store.permission_warning() {
        eprintln!("warning: {w}");
    }
    let rows = store.list();
    let chatgpt = store.chatgpt_account();
    if rows.is_empty() && chatgpt.is_none() {
        writeln!(
            out,
            "no API keys stored; add one with `mu auth add <provider>` ({}), or sign in with `mu auth login {PROVIDER_ID}`",
            names()
        )?;
        return Ok(0);
    }
    let custom = registry.snapshot();
    if let Err(e) = &custom {
        eprintln!("warning: {e:#}");
    }
    let usage = uses(&rows, store, &custom);
    writeln!(out, "{}", render(&rows, &usage, chatgpt.as_ref()))?;
    Ok(0)
}

fn names() -> String {
    providers::api_key_defs().map(|d| d.id).chain(SEARCH_KEYS.iter().map(|d| d.id)).collect::<Vec<_>>().join(", ")
}

/// `openai-chatgpt`, or its `chatgpt` alias.
fn is_chatgpt(id: &str) -> bool {
    matches!(id, PROVIDER_ID | "chatgpt")
}

/// Why `mu auth add openai-chatgpt` is refused: the sign-in takes no key, and none of the flags that
/// shape a key.
fn chatgpt_key_refusal(key_flags: bool, chat_flags: bool) -> anyhow::Error {
    if key_flags || chat_flags {
        anyhow!(
            "--env, --cmd, --base-url and --chat/--no-chat do not apply to {PROVIDER_ID}, which signs in with ChatGPT"
        )
    } else {
        anyhow!("{PROVIDER_ID} takes no API key; sign in with `mu auth login {PROVIDER_ID}`")
    }
}

/// `mu auth login` and `mu auth logout` act on the ChatGPT sign-in only; its id is optional.
fn chatgpt_only(cmd: &str, provider: Option<&str>) -> Result<()> {
    match provider {
        Some(p) if !is_chatgpt(p) => bail!("`mu auth {cmd}` applies to {PROVIDER_ID} (ChatGPT sign-in), not `{p}`"),
        _ => Ok(()),
    }
}

/// Signs in through the browser. On a terminal the pasted redirect URL is accepted too, for machines
/// the browser's callback cannot reach. The sign-in is saved only when it is the registered account.
fn login(store: &Store, out: &mut impl Write) -> Result<i32> {
    let host = store.chatgpt_host_id()?;
    let endpoints = store.chatgpt_endpoints();
    let req = oauth::LoginRequest {
        endpoints: &endpoints,
        host_id: &host,
        known: store.chatgpt_known(),
        cancel: CancellationToken::new(),
    };
    let tty = std::io::stdin().is_terminal();
    // The sender lives until the sign-in ends: a closed channel would fail the wait for the browser.
    let (paste_tx, mut paste_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if tty {
        let tx = paste_tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let mut on_url = |url: &str| {
        eprintln!("Sign in with ChatGPT. A browser should open; if it does not, open this link:\n\n{url}\n");
        if tty {
            eprintln!("If the browser cannot reach this machine, paste the final redirect URL here and press Enter.");
        }
        let _ = chatgpt::browser::open_browser(url);
    };
    let mut notice = |text: &str| eprintln!("{text}");
    let rt = mu_base::runtime::current_thread_runtime()?;
    let account = match rt.block_on(oauth::login(req, &mut on_url, &mut notice, &mut paste_rx)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sign-in did not finish: {e}; nothing saved");
            return Ok(1);
        }
    };
    let saved = match signin::save(store, account) {
        Ok(s) => s,
        Err(m) => {
            eprintln!("{m}");
            return Ok(1);
        }
    };
    writeln!(out, "signed in as {}", saved.label)?;
    if !saved.plan_usage {
        eprintln!(
            "plan usage is not enabled for this sign-in, so {PROVIDER_ID}/* models cannot run yet. Run `mu auth login {PROVIDER_ID}` again and approve the plan permission."
        );
        return Ok(1);
    }
    writeln!(out, "{PROVIDER_ID}/* models now run on your ChatGPT plan.")?;
    if saved.welcome {
        writeln!(out, "You're using your ChatGPT plan. Manage usage: {USAGE_URL}")?;
    }
    Ok(0)
}

/// Drops the tokens, then revokes the refresh token at the issuer. `forget` drops the registration too.
fn logout(store: &Store, out: &mut impl Write, forget: bool) -> Result<i32> {
    if forget {
        return forget_registration(store, out);
    }
    let Some(account) = signin::sign_out(store).map_err(|e| anyhow!("{e}"))? else {
        eprintln!("not signed in with ChatGPT");
        return Ok(1);
    };
    writeln!(
        out,
        "signed out of ChatGPT ({}); {PROVIDER_ID}/* models stop running until you sign in again",
        account.label()
    )?;
    if let Err(m) = mu_base::runtime::current_thread_runtime()?.block_on(signin::revoke(store, &account)) {
        eprintln!(
            "could not confirm that the sign-in was revoked ({m}). Disconnect mu under your ChatGPT settings, apps."
        );
    }
    Ok(0)
}

/// Drops the registration and its tokens, then revokes the refresh token at the issuer. The app stays
/// listed in ChatGPT settings, so that is said too.
fn forget_registration(store: &Store, out: &mut impl Write) -> Result<i32> {
    let Some(account) = signin::forget(store).map_err(|e| anyhow!("{e}"))? else {
        eprintln!("no ChatGPT sign-in is saved");
        return Ok(1);
    };
    writeln!(
        out,
        "forgot the ChatGPT registration ({}); {PROVIDER_ID}/* models stop running until you sign in again",
        account.label()
    )?;
    writeln!(
        out,
        "The next `mu auth login {PROVIDER_ID}` registers a new client and may sign in as a different ChatGPT account."
    )?;
    writeln!(out, "The old app stays listed under your ChatGPT settings, apps, until you disconnect it there.")?;
    let revoked = if account.signed_in() {
        mu_base::runtime::current_thread_runtime()?.block_on(signin::revoke(store, &account))
    } else {
        Ok(())
    };
    if let Err(m) = revoked {
        eprintln!(
            "could not confirm that the sign-in was revoked ({m}). Disconnect mu under your ChatGPT settings, apps."
        );
    }
    Ok(0)
}

/// Asks the ChatGPT plan whether the sign-in works: the model list is fetched with it.
fn chatgpt_test(store: &Store, out: &mut impl Write) -> Result<i32> {
    if !store.chatgpt_account().is_some_and(|a| a.signed_in()) {
        eprintln!("not signed in with ChatGPT; run `mu auth login {PROVIDER_ID}`");
        return Ok(1);
    }
    match mu_base::runtime::current_thread_runtime()?.block_on(chatgpt::store::check(store)) {
        Ok(models) => {
            writeln!(out, "{PROVIDER_ID}: the plan answers; {} models", models.len())?;
            Ok(0)
        }
        Err(m) => {
            eprintln!("{PROVIDER_ID}: {m}");
            Ok(1)
        }
    }
}

/// What each stored key is used for, from the store and the registry; `render` only formats it. A
/// custom id with no definition is an orphan, and an unreadable registry leaves custom rows unknown.
fn uses(
    rows: &[(String, Entry)],
    store: &Store,
    custom: &Result<BTreeMap<String, CustomProvider>>,
) -> BTreeMap<String, Usage> {
    rows.iter()
        .map(|(p, e)| {
            let usage = match (KeyTarget::builtin(p), custom) {
                (Some(KeyTarget::Search(_)), _) => Usage::Search,
                (Some(_), _) if e.routes_chat() => Usage::Chat,
                (Some(_), _) => Usage::NotChat,
                (None, Err(_)) => Usage::Unknown,
                (None, Ok(defs)) => match defs.get(p) {
                    None => Usage::Orphan,
                    Some(def) => match def.key_state(store) {
                        KeyState::NotNeeded => Usage::Keyless,
                        KeyState::Saved => Usage::Chat,
                        KeyState::Disabled => Usage::NotChat,
                        KeyState::Missing => Usage::Missing,
                    },
                },
            };
            (p.clone(), usage)
        })
        .collect()
}

/// What a stored key is used for; its row's last column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Search,
    Chat,
    NotChat,
    Keyless,
    Missing,
    Orphan,
    Unknown,
}

impl Usage {
    fn text(self) -> &'static str {
        match self {
            Self::Search => "web search",
            Self::Chat => "chat",
            Self::NotChat => "not for chat",
            Self::Keyless => "not used: the endpoint takes no key",
            Self::Missing => "key missing",
            Self::Orphan => "no provider definition",
            Self::Unknown => "",
        }
    }
}

/// One line per provider: its source, for a stored key the last four characters, and what the key
/// is used for. A `!command` is not run to show anything. The ChatGPT sign-in, when there is one, is last.
pub fn render(rows: &[(String, Entry)], uses: &BTreeMap<String, Usage>, chatgpt: Option<&Account>) -> String {
    let w = rows.iter().map(|(p, _)| p.len()).chain(chatgpt.map(|_| PROVIDER_ID.len())).max().unwrap_or(0);
    let mut lines: Vec<String> = rows
        .iter()
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
            let used = uses.get(p).copied().unwrap_or(Usage::Unknown).text();
            let url = e.base_url.as_deref().map(|u| format!("  {u}")).unwrap_or_default();
            Some(format!("{}{url}", columns(w, p, &[src.as_str(), detail.as_str(), used])))
        })
        .collect();
    lines.extend(chatgpt.map(|a| chatgpt_line(w, a)));
    lines.join("\n")
}

/// `id` padded to `w`, then each non-empty cell two spaces after the last.
fn columns(w: usize, id: &str, cells: &[&str]) -> String {
    cells.iter().filter(|s| !s.is_empty()).fold(format!("{id:<w$}"), |mut l, s| {
        l.push_str("  ");
        l.push_str(s);
        l
    })
}

/// The ChatGPT sign-in's row: where it comes from, the account, and what it is used for.
fn chatgpt_line(w: usize, a: &Account) -> String {
    let used = if !a.signed_in() {
        "signed out"
    } else if a.plan_usage() {
        "chat · plan"
    } else {
        "plan usage not enabled"
    };
    let label = a.label();
    columns(w, PROVIDER_ID, &["ChatGPT sign-in", label.as_str(), used])
}

/// The key `mu auth add` reads from stdin (see `read_secret`).
fn read_key(t: &KeyTarget) -> Result<String> {
    read_secret(&key_hint(t), &format!("{} API key", t.label()))
}

/// Where a provider's key is made; a custom endpoint has no such page, so no hint.
fn key_hint(t: &KeyTarget) -> String {
    match t.key_url() {
        "" => String::new(),
        url => format!("Create a key at {url}"),
    }
}

/// A secret from stdin: a no-echo prompt on a terminal (after `hint`, when given), else the piped
/// text. Shared by `mu auth add` and `mu providers add`; a key is never a command argument.
pub(crate) fn read_secret(hint: &str, prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        return Ok(s.trim().to_string());
    }
    if !hint.is_empty() {
        eprintln!("{hint}");
    }
    eprint!("{prompt} (input is hidden): ");
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

    fn nowhere() -> Registry {
        Registry::at(std::env::temp_dir().join(format!("mu-cli-none-{}", uuid::Uuid::new_v4())).join("providers.toml"))
    }

    #[test]
    fn list_shows_at_most_the_last_four_characters() {
        let key = "sk-ant-api03-SECRETPARTabcdefWXYZ";
        let rows = vec![
            ("anthropic".to_string(), Entry::literal(key)),
            ("baseten".to_string(), Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap()),
            ("openai".to_string(), Entry::reference("!op read op://dev/openai/key").unwrap()),
        ];
        let text = render(&rows, &BTreeMap::new(), None);
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
        let text = render(&[("anthropic".to_string(), Entry::literal("tiny-key"))], &BTreeMap::new(), None);
        assert!(!text.contains("tiny"), "{text}");
        assert!(!text.contains("-key"), "{text}");
    }

    #[test]
    fn unknown_providers_are_refused_with_the_known_ones() {
        let e = target("gemini", &nowhere()).unwrap_err().to_string();
        for id in ["anthropic", "openai", "baseten", "parallel", "exa"] {
            assert!(e.contains(id), "{e}");
        }
        assert_eq!(target("baseten", &nowhere()).unwrap().env_hint(), "BASETEN_API_KEY");
        assert_eq!(target("exa", &nowhere()).unwrap().env_hint(), "EXA_API_KEY");
    }

    #[test]
    fn chat_flags_follow_the_target() {
        let t = |id| target(id, &nowhere()).unwrap();
        for id in ["anthropic", "openai"] {
            assert_eq!(chat_flag(&t(id), true, false).unwrap(), Some(true));
            assert_eq!(chat_flag(&t(id), false, true).unwrap(), Some(false));
            let e = chat_flag(&t(id), false, false).unwrap_err().to_string();
            assert!(e.contains("add --chat or --no-chat") && e.contains(&format!("{id}/*")), "{e}");
        }
        assert_eq!(chat_flag(&t("baseten"), false, false).unwrap(), Some(true));
        assert_eq!(chat_flag(&t("baseten"), true, false).unwrap(), Some(true));
        assert_eq!(
            chat_flag(&t("baseten"), false, true).unwrap_err().to_string(),
            "a Baseten key is only used for chat models"
        );
        for id in ["parallel", "exa"] {
            assert_eq!(chat_flag(&t(id), false, false).unwrap(), None);
            for (c, n) in [(true, false), (false, true)] {
                let e = chat_flag(&t(id), c, n).unwrap_err().to_string();
                assert!(e.contains("only apply to anthropic and openai"), "{e}");
            }
        }
    }

    #[test]
    fn list_says_what_each_key_is_for() {
        let off = Entry { chat: Some(false), ..Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap() };
        let rows = vec![
            ("anthropic".to_string(), off),
            ("baseten".to_string(), Entry { chat: Some(true), ..Entry::literal("bt-test-0123456789") }),
            ("exa".to_string(), Entry::literal("exa-test-0123456789")),
        ];
        let usage = BTreeMap::from([
            ("anthropic".to_string(), Usage::NotChat),
            ("baseten".to_string(), Usage::Chat),
            ("exa".to_string(), Usage::Search),
        ]);
        let lines: Vec<String> = render(&rows, &usage, None).lines().map(String::from).collect();
        assert!(lines[0].ends_with("not set  not for chat"), "{lines:?}");
        assert!(lines[1].ends_with("chat") && !lines[1].contains("not for"), "{lines:?}");
        assert!(lines[2].ends_with("web search"), "{lines:?}");
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
mod chatgpt_tests {
    use super::*;
    use clap::Parser;
    use mu_provider::auth::chatgpt::{DIRECT_SCOPE, Tokens};

    fn account(signed_in: bool, plan: bool) -> Account {
        let tokens = signed_in.then(|| Tokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            refresh_at: 0,
            scopes: if plan { vec![DIRECT_SCOPE.into()] } else { vec![] },
        });
        Account {
            client_id: "oaiapp_test".into(),
            subject: "user-test".into(),
            email: Some("me@example.com".into()),
            tokens,
            added: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn the_sign_in_refuses_key_flags_with_a_clear_error() {
        let key = chatgpt_key_refusal(true, false).to_string();
        assert!(key.contains("--env, --cmd, --base-url") && key.contains("openai-chatgpt"), "{key}");
        let chat = chatgpt_key_refusal(false, true).to_string();
        assert!(chat.contains("--chat/--no-chat"), "{chat}");
        let bare = chatgpt_key_refusal(false, false).to_string();
        assert!(bare.contains("takes no API key") && bare.contains("mu auth login openai-chatgpt"), "{bare}");
    }

    #[test]
    fn the_sign_in_takes_its_provider_id_and_alias_only() {
        assert!(chatgpt_only("login", None).is_ok());
        assert!(chatgpt_only("login", Some("openai-chatgpt")).is_ok());
        assert!(chatgpt_only("logout", Some("chatgpt")).is_ok());
        let e = chatgpt_only("login", Some("openai")).unwrap_err().to_string();
        assert!(e.contains("`mu auth login`") && e.contains("openai-chatgpt"), "{e}");
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: Cmd,
    }

    #[test]
    fn login_and_logout_parse_with_an_optional_provider() {
        let parse =
            |args: &[&str]| Wrap::try_parse_from(std::iter::once("mu").chain(args.iter().copied())).map(|w| w.cmd);
        assert!(matches!(parse(&["login"]), Ok(Cmd::Login { provider: None })));
        assert!(matches!(parse(&["login", "chatgpt"]), Ok(Cmd::Login { provider: Some(p) }) if p == "chatgpt"));
        assert!(matches!(parse(&["logout", "openai-chatgpt"]), Ok(Cmd::Logout { provider: Some(_), forget: false })));
        assert!(matches!(parse(&["logout", "--forget"]), Ok(Cmd::Logout { provider: None, forget: true })));
        assert!(matches!(
            parse(&["logout", "chatgpt", "--forget"]),
            Ok(Cmd::Logout { provider: Some(_), forget: true })
        ));
        assert!(parse(&["rm", "openai-chatgpt", "--forget"]).is_err(), "rm stays a plain sign-out");
        assert!(matches!(parse(&["rm", "openai-chatgpt"]), Ok(Cmd::Rm { .. })));
        assert!(parse(&["login", "--chat"]).is_err(), "the sign-in takes no chat flag");
        assert!(parse(&["logout", "--no-chat"]).is_err(), "the sign-in takes no chat flag");
    }

    #[test]
    fn forget_says_what_is_left_and_what_comes_next() {
        let d = std::env::temp_dir().join(format!("mu-cli-forget-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.chatgpt_save(account(false, false)).unwrap();
        let mut out = Vec::new();
        assert_eq!(forget_registration(&store, &mut out).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("forgot the ChatGPT registration (me@example.com)"), "{text}");
        assert!(text.contains("registers a new client") && text.contains("apps, until"), "{text}");
        assert!(store.chatgpt_account().is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn list_adds_the_sign_in_row_with_its_state() {
        let rows = vec![("anthropic".to_string(), Entry::literal("ant-test-0123456789"))];
        let text = render(&rows, &BTreeMap::new(), Some(&account(true, true)));
        let last = text.lines().last().unwrap();
        assert!(last.starts_with("openai-chatgpt") && last.contains("ChatGPT sign-in"), "{text}");
        assert!(last.contains("me@example.com") && last.ends_with("chat · plan"), "{text}");
        assert!(text.lines().next().unwrap().starts_with(&format!("{:<14}", "anthropic")), "{text}");
        assert!(render(&rows, &BTreeMap::new(), Some(&account(true, false))).ends_with("plan usage not enabled"));
        assert!(render(&[], &BTreeMap::new(), Some(&account(false, false))).ends_with("signed out"));
        assert!(!render(&rows, &BTreeMap::new(), None).contains("ChatGPT"));
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use mu_provider::provider::providers::native_def;
    use mu_testutil::block_on as rt;
    use mu_testutil::{Reply, fake_server};

    fn entry(key: &str, base: &str) -> Entry {
        Entry { base_url: Some(base.into()), ..Entry::literal(key) }
    }

    #[test]
    fn a_good_key_is_validated_with_the_providers_auth_header() {
        rt(async {
            let (base, seen) = fake_server(vec![Reply::new(200, r#"{"data":[]}"#)]).await;
            let anthropic = KeyTarget::Provider(native_def("anthropic").unwrap());
            let r = check_entry(&anthropic, &entry("ant-key-0123456789", &base)).await;
            assert_eq!(r, Check::Valid);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(req.starts_with("get /v1/models?limit=1 "), "{req}");
            assert!(req.contains("x-api-key: ant-key-0123456789"), "{req}");
            assert!(!req.contains("authorization"), "{req}");

            let (base, seen) = fake_server(vec![Reply::new(200, "{}")]).await;
            let baseten = KeyTarget::Provider(native_def("baseten").unwrap());
            assert_eq!(check_entry(&baseten, &entry("bt-key-0123456789", &base)).await, Check::Valid);
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
            let openai = KeyTarget::Provider(native_def("openai").unwrap());
            assert!(
                matches!(check_entry(&openai, &entry("k-0123456789", &base)).await, Check::Rejected(m) if m.contains("rejected"))
            );
            let (base, _) = fake_server(vec![Reply::new(503, "down")]).await;
            assert!(matches!(check_entry(&openai, &entry("k-0123456789", &base)).await, Check::Unreachable(_)));
            // nothing listening
            let dead = "http://127.0.0.1:1";
            assert!(matches!(check_entry(&openai, &entry("k-0123456789", dead)).await, Check::Unreachable(_)));
        });
    }

    #[test]
    fn an_unset_reference_is_not_validated_as_text() {
        rt(async {
            let openai = KeyTarget::Provider(native_def("openai").unwrap());
            let e = Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap();
            assert!(matches!(check_entry(&openai, &e).await, Check::Rejected(m) if m.contains("not set")));
            // A search key is only resolved, never sent anywhere.
            let exa = KeyTarget::builtin("exa").unwrap();
            assert!(matches!(check_entry(&exa, &e).await, Check::Rejected(m) if m.contains("not set")));
            assert_eq!(check_entry(&exa, &Entry::literal("exa-test-0123456789")).await, Check::Unchecked);
        });
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;
    use clap::Parser as _;
    use mu_provider::provider::custom::CustomProvider;
    use mu_testutil::{Reply, fake_server};

    /// A registry and store in a fresh temp dir, so no test reads or writes the profile.
    fn files(tag: &str) -> (Registry, Store, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-cli-auth-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        (Registry::at(d.join("providers.toml")), Store::at(d.join("auth.json")), d)
    }

    /// A fake endpoint that answers from a runtime of its own, so a blocking command can call it.
    fn server(replies: Vec<Reply>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (base, seen) = rt.block_on(fake_server(replies));
        (base, seen, rt)
    }

    fn add_def(reg: &Registry, id: &str, base: &str, auth: AuthMode) {
        let mut c = CustomProvider::new(base);
        c.auth = auth;
        reg.add(id, c).unwrap();
    }

    fn add_cmd(provider: &str, env: Option<&str>, base_url: Option<&str>, no_chat: bool) -> Cmd {
        Cmd::Add {
            provider: provider.into(),
            env: env.map(Into::into),
            cmd: None,
            base_url: base_url.map(Into::into),
            no_validate: true,
            chat: false,
            no_chat,
        }
    }

    fn rm_cmd(provider: &str) -> Cmd {
        Cmd::Rm { provider: provider.into() }
    }

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn custom_targets_refuse_flags_that_do_not_apply() {
        let (reg, store, d) = files("flags");
        add_def(&reg, "bearer", "https://x.test/v1", AuthMode::Bearer);
        add_def(&reg, "open", "https://x.test/v1", AuthMode::None);
        let e = exec(&store, &reg, add_cmd("bearer", Some("MU_UNSET"), Some("https://y.test"), false), &mut Vec::new());
        assert!(e.unwrap_err().to_string().contains("--base-url does not apply"));
        let e = exec(&store, &reg, add_cmd("open", Some("MU_UNSET"), None, false), &mut Vec::new());
        assert!(e.unwrap_err().to_string().contains("auth = \"bearer\""));
        // `--no-chat` is for Anthropic and OpenAI: a custom key only ever serves chat.
        let e = exec(&store, &reg, add_cmd("bearer", Some("MU_UNSET"), None, true), &mut Vec::new());
        assert!(e.unwrap_err().to_string().contains("only used for chat"));
        assert!(store.get("bearer").is_none(), "refusals save nothing");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_custom_key_is_stored_for_chat_under_its_definition() {
        let (reg, store, d) = files("add");
        add_def(&reg, "bearer", "https://x.test/v1", AuthMode::Bearer);
        let mut out = Vec::new();
        assert_eq!(exec(&store, &reg, add_cmd("bearer", Some("MU_UNSET"), None, false), &mut out).unwrap(), 0);
        let e = store.get("bearer").unwrap();
        assert!(e.routes_chat() && e.base_url.is_none());
        assert_eq!(e.reference.as_deref(), Some("$MU_UNSET"));
        assert!(text(out).contains("sent to https://x.test/v1"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_keyless_endpoint_is_tested_without_a_stored_entry() {
        let (reg, store, d) = files("keyless-test");
        let (base, seen, _rt) = server(vec![Reply::new(200, r#"{"data":[]}"#)]);
        add_def(&reg, "open", &base, AuthMode::None);
        let mut out = Vec::new();
        let test = Cmd::Test { provider: "open".into() };
        assert_eq!(exec(&store, &reg, test, &mut out).unwrap(), 0);
        assert!(text(out).contains("the endpoint answers without a key"));
        let req = seen.lock().unwrap()[0].to_lowercase();
        assert!(req.starts_with("get /models ") && !req.contains("authorization"), "{req}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_bearer_endpoint_is_tested_with_its_stored_key() {
        let (reg, store, d) = files("bearer-test");
        let (base, seen, _rt) = server(vec![Reply::new(401, "no")]);
        add_def(&reg, "bearer", &base, AuthMode::Bearer);
        store.set("bearer", Entry::literal("bt-test-0123456789")).unwrap();
        let test = Cmd::Test { provider: "bearer".into() };
        assert_eq!(exec(&store, &reg, test, &mut Vec::new()).unwrap(), 1, "a 401 is a failed test");
        let req = seen.lock().unwrap()[0].to_lowercase();
        assert!(req.contains("authorization: bearer bt-test-0123456789"), "{req}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rm_says_a_custom_provider_needs_its_key_and_never_falls_back() {
        let (reg, store, d) = files("rm");
        add_def(&reg, "bearer", "https://x.test/v1", AuthMode::Bearer);
        add_def(&reg, "open", "https://x.test/v1", AuthMode::None);
        store.set("bearer", Entry::literal("bt-test-0123456789")).unwrap();
        let mut out = Vec::new();
        assert_eq!(exec(&store, &reg, rm_cmd("bearer"), &mut out).unwrap(), 0);
        let t = text(out);
        assert!(t.contains("now need a key") && t.contains("mu auth add bearer"), "{t}");
        assert!(!t.contains("omp"), "{t}");
        assert!(store.get("bearer").is_none());
        assert!(reg.get("bearer").unwrap().is_some(), "`mu auth rm` keeps the definition");
        // A keyless endpoint has no key to remove: nothing is changed and the exit is 1.
        assert_eq!(exec(&store, &reg, rm_cmd("open"), &mut Vec::new()).unwrap(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rm_removes_an_orphan_key_without_a_definition() {
        let (reg, store, d) = files("orphan");
        store.set("ghost", Entry::literal("gh-test-0123456789")).unwrap();
        let mut out = Vec::new();
        assert_eq!(exec(&store, &reg, rm_cmd("ghost"), &mut out).unwrap(), 0);
        assert!(text(out).contains("no custom provider `ghost` is configured"));
        assert!(!store.has("ghost"));
        let e = exec(&store, &reg, rm_cmd("ghost"), &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("unknown provider `ghost`"), "{e}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_disabled_key_is_told_apart_from_a_missing_one() {
        let (reg, store, d) = files("uses");
        add_def(&reg, "bearer", "https://x.test/v1", AuthMode::Bearer);
        store.set("bearer", Entry { chat: Some(false), ..Entry::literal("bt-test-0123456789") }).unwrap();
        store.set("ghost", Entry::literal("gh-test-0123456789")).unwrap();
        let rows = store.list();
        let usage = uses(&rows, &store, &reg.snapshot());
        assert_eq!(usage["bearer"], Usage::NotChat);
        assert_eq!(usage["ghost"], Usage::Orphan);
        let t = render(&rows, &usage, None);
        assert!(t.contains("not for chat") && t.contains("no provider definition"), "{t}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_custom_target_has_no_key_page_to_point_at() {
        let custom = KeyTarget::Custom(CustomProvider::new("https://x.test/v1"));
        assert_eq!(key_hint(&custom), "");
        assert!(key_hint(&KeyTarget::builtin("anthropic").unwrap()).starts_with("Create a key at https://"));
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: Cmd,
    }

    #[test]
    fn custom_add_flags_parse_with_the_others() {
        let parse =
            |args: &[&str]| Wrap::try_parse_from(std::iter::once("mu").chain(args.iter().copied())).map(|w| w.cmd);
        assert!(parse(&["add", "bearer", "--env", "X", "--base-url", "https://x.test/v1"]).is_ok());
        assert!(parse(&["add", "bearer", "--env", "X", "--chat"]).is_ok());
    }
}
