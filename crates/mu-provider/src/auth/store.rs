//! mu's own store of provider API keys: `<profile cache dir>/auth.json`, mode 0600, one entry per
//! provider (see `provider::providers`). An entry holds a literal key or a reference to one:
//! `$NAME` (an environment variable) or `!command` (run through `sh -c`). A provider has an entry
//! exactly when mu routes its models natively; omp's own credentials are never read.
//!
//! Reads are cached per file stamp (inode, ctime, mtime and size), so a key added by one session reaches the
//! others without a restart. Writes hold a lock file, re-read, and replace the file atomically.

use super::chatgpt::ChatgptData;
use super::chatgpt::store::register_tokens;
use super::web::WebSearch;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 2 added `Entry::chat` and `web`; 3 added the `chatgpt` section. An older mu refuses a newer file
/// rather than rewriting it without them. A file with no sign-in is still written as 2.
const VERSION: u32 = 3;
const VERSION_WITHOUT_CHATGPT: u32 = 2;
/// A `!command` reference gets this long to print the key.
pub const CMD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `$NAME` or `!command`.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Overrides the provider's API URL (proxies, tests). https, or http to loopback only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    /// Whether the provider's chat models use this key. `None` is a version-1 entry, which did.
    /// Meaningless for search-only targets such as Parallel and Exa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<bool>,
}

/// Where an entry's key comes from.
#[derive(Clone, PartialEq)]
pub enum Origin {
    Stored,
    Env(String),
    Cmd(String),
}

impl Entry {
    pub fn literal(key: &str) -> Self {
        Self { key: Some(key.trim().to_string()), added: Some(now()), ..Self::default() }
    }

    /// `$NAME` or `!command`; anything else is rejected, so a typo cannot become a literal key.
    pub fn reference(r: &str) -> Result<Self> {
        let r = r.trim();
        match (r.strip_prefix('$'), r.strip_prefix('!')) {
            (Some(n), _) if valid_env_name(n) => {}
            (_, Some(c)) if !c.trim().is_empty() => {}
            _ => bail!("a reference is `$ENV_NAME` or `!command`"),
        }
        Ok(Self { reference: Some(r.to_string()), added: Some(now()), ..Self::default() })
    }

    pub fn routes_chat(&self) -> bool {
        self.chat.unwrap_or(true)
    }

    pub fn origin(&self) -> Option<Origin> {
        match (&self.reference, &self.key) {
            (Some(r), _) => match (r.strip_prefix('$'), r.strip_prefix('!')) {
                (Some(n), _) => Some(Origin::Env(n.to_string())),
                (_, Some(c)) => Some(Origin::Cmd(c.trim().to_string())),
                _ => None,
            },
            (None, Some(k)) if !k.is_empty() => Some(Origin::Stored),
            _ => None,
        }
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Stored => f.write_str("Stored"),
            Origin::Env(n) => f.debug_tuple("Env").field(n).finish(),
            Origin::Cmd(c) => f.debug_tuple("Cmd").field(&cmd_label(c)).finish(),
        }
    }
}

/// The key is never printed.
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("origin", &self.origin())
            .field("base_url", &self.base_url)
            .field("added", &self.added)
            .field("chat", &self.chat)
            .finish()
    }
}

/// A `!command` as shown to a user: its program name only, since the arguments can hold a key.
pub fn cmd_label(cmd: &str) -> String {
    let prog = cmd.split_whitespace().next().unwrap_or_default();
    mu_base::secrets::redact_keys(&format!("!{prog} …"))
}

/// A base URL a stored entry or `--base-url` may use: `net::validate_url`, and no credentials
/// (a password in the URL would sit in the file and show in `mu auth list`).
pub fn check_base_url(url: &str) -> Result<String> {
    let url = url.trim();
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    if rest.split(['/', '?', '#']).next().unwrap_or_default().contains('@') {
        bail!("a base URL must not contain a user name or password");
    }
    crate::net::validate_url(url)
}

fn valid_env_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn now() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileData {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    web: Option<WebSearch>,
    #[serde(default, skip_serializing_if = "chatgpt_unset")]
    chatgpt: ChatgptData,
}

fn chatgpt_unset(c: &ChatgptData) -> bool {
    *c == ChatgptData::default()
}

/// What `resolve` hands to a request.
pub struct Resolved {
    pub key: String,
    pub base_url: Option<String>,
}

type Stamp = Option<[i64; 7]>;

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

/// Parsed files by path, with the stamp they were read at.
type Cached = HashMap<PathBuf, (Stamp, Arc<FileData>)>;
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
/// `!command` results by store file and provider, with the command that produced them. A store
/// only sees its own results; every real run has the one profile store.
type CmdKey = (PathBuf, String);
static CMD_CACHE: Mutex<Option<HashMap<CmdKey, (String, String)>>> = Mutex::new(None);

impl Store {
    /// The active profile's store.
    pub fn new() -> Self {
        Self { path: mu_common::paths::profile_home().join("auth.json") }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn stamp(&self) -> Stamp {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&self.path).ok()?;
        // Inode and ctime catch an atomic rename, or a rewrite within one mtime tick at the same size.
        Some([m.dev() as i64, m.ino() as i64, m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.len() as i64])
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        let mut n = self.path.file_name().unwrap_or_default().to_os_string();
        n.push(".lock");
        self.path.with_file_name(n)
    }

    /// Reads the file now. A missing file is an empty store; an unreadable or corrupt one is an error.
    fn read_fresh(&self) -> Result<FileData> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileData::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        if text.trim().is_empty() {
            return Ok(FileData::default());
        }
        let data: FileData = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; fix or delete it", self.path.display()))?;
        if data.version > VERSION {
            bail!("{} is version {}, newer than this mu understands ({VERSION})", self.path.display(), data.version);
        }
        Ok(data)
    }

    /// The parsed file, re-read when its stamp changed. A bad file reads as empty here; `check`
    /// reports why.
    fn snapshot(&self) -> Arc<FileData> {
        let stamp = self.stamp();
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some((s, d)) = cache.get(&self.path)
            && *s == stamp
        {
            return d.clone();
        }
        let data = Arc::new(self.read_fresh().unwrap_or_default());
        for (provider, e) in &data.providers {
            if let Some(k) = &e.key {
                mu_base::secrets::register_stored(&label(provider), k);
            }
        }
        register_tokens(&data.chatgpt);
        cache.insert(self.path.clone(), (stamp, data.clone()));
        data
    }

    /// `Ok` when the store is absent or well-formed. A corrupt file is an error, and so is an entry
    /// mu ignores (no usable key or reference, or a base URL it refuses), named by provider.
    pub fn check(&self) -> Result<()> {
        let data = self.read_fresh()?;
        if [data.chatgpt.auth_base.as_deref(), data.chatgpt.api_base.as_deref()]
            .into_iter()
            .flatten()
            .any(|u| check_base_url(u).is_err())
        {
            bail!(
                "{} has a ChatGPT base URL override mu refuses (https, or http to loopback; no user name or password)",
                self.path.display()
            );
        }
        let ignored: Vec<&str> = data
            .providers
            .iter()
            .filter(|(_, e)| e.origin().is_none() || e.base_url.as_deref().is_some_and(|u| check_base_url(u).is_err()))
            .map(|(p, _)| p.as_str())
            .collect();
        if ignored.is_empty() {
            return Ok(());
        }
        bail!(
            "{} has entries mu ignores (no usable key, reference or base URL): {}; fix them or remove them with `mu auth rm <provider>`",
            self.path.display(),
            ignored.join(", ")
        )
    }

    /// A warning when other users can read the file, as ssh does for private keys.
    pub fn permission_warning(&self) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&self.path).ok()?.permissions().mode();
        (mode & 0o077 != 0).then(|| {
            format!(
                "{} is readable by other users (mode {:o}); run `chmod 600 {}`",
                self.path.display(),
                mode & 0o777,
                self.path.display()
            )
        })
    }

    pub fn get(&self, provider: &str) -> Option<Entry> {
        self.snapshot().providers.get(provider).filter(|e| e.origin().is_some()).cloned()
    }

    pub fn has(&self, provider: &str) -> bool {
        self.get(provider).is_some()
    }

    /// Entries in provider order.
    pub fn list(&self) -> Vec<(String, Entry)> {
        self.snapshot()
            .providers
            .iter()
            .filter(|(_, e)| e.origin().is_some())
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect()
    }

    fn modify(&self, f: impl FnOnce(&mut FileData) -> bool) -> Result<bool> {
        let _lock = mu_common::fs::FileLock::exclusive(&self.lock_path())
            .with_context(|| format!("locking {}", self.lock_path().display()))?;
        self.modify_locked(f)
    }

    /// `modify` for a caller that already holds the lock file.
    fn modify_locked(&self, f: impl FnOnce(&mut FileData) -> bool) -> Result<bool> {
        let mut data = self.read_fresh()?;
        let changed = f(&mut data);
        if changed {
            data.version = if data.chatgpt == ChatgptData::default() { VERSION_WITHOUT_CHATGPT } else { VERSION };
            for (p, e) in data.providers.iter_mut() {
                if e.chat.is_none() && crate::provider::providers::native_def(p).is_some() {
                    e.chat = Some(true);
                }
            }
            let json = serde_json::to_string_pretty(&data)? + "\n";
            mu_common::fs::write_private(&self.path, json.as_bytes())
                .with_context(|| format!("writing {}", self.path.display()))?;
        }
        Ok(changed)
    }

    /// Adds or replaces a provider's entry.
    pub fn set(&self, provider: &str, entry: Entry) -> Result<()> {
        if entry.origin().is_none() {
            bail!("an entry needs a key or a reference");
        }
        if let Some(u) = &entry.base_url {
            check_base_url(u).with_context(|| format!("base URL for {provider}"))?;
        }
        if let Some(k) = &entry.key {
            mu_base::secrets::register_stored(&label(provider), k);
        }
        forget_cached(&self.path, provider);
        self.modify(|d| {
            d.providers.insert(provider.to_string(), entry);
            true
        })?;
        Ok(())
    }

    /// Decides whether `provider`'s chat models use its key. `false` when there is no entry.
    pub fn set_chat(&self, provider: &str, chat: bool) -> Result<bool> {
        self.modify(|d| match d.providers.get_mut(provider) {
            Some(e) => {
                e.chat = Some(chat);
                true
            }
            None => false,
        })
    }

    pub fn web(&self) -> Option<WebSearch> {
        self.snapshot().web.clone()
    }

    /// `None` forgets the choice, so the setup wizard runs again.
    pub fn set_web(&self, w: Option<WebSearch>) -> Result<()> {
        self.modify(|d| {
            d.web = w;
            true
        })?;
        Ok(())
    }

    /// Removes a provider's entry; `false` when there was none.
    pub fn remove(&self, provider: &str) -> Result<bool> {
        forget_cached(&self.path, provider);
        self.modify(|d| d.providers.remove(provider).is_some())
    }

    /// The key for `provider`: the stored literal, `$NAME`'s value, or the output of `!command`.
    /// A reference that cannot be resolved is an error: it never falls back to the reference text.
    pub async fn resolve(&self, provider: &str) -> Result<Resolved> {
        let entry = self.get(provider).ok_or_else(|| anyhow!("no API key stored for {provider}"))?;
        resolve_entry_in(&self.path, provider, &entry).await
    }
}

impl Store {
    /// The cached `chatgpt` section (see `read_fresh` for a re-read under the lock).
    pub(crate) fn chatgpt_cached(&self) -> ChatgptData {
        self.snapshot().chatgpt.clone()
    }

    /// The `chatgpt` section as the file holds it now. Callers that write must hold `lock_path`.
    pub(crate) fn chatgpt_fresh(&self) -> Result<ChatgptData> {
        Ok(self.read_fresh()?.chatgpt)
    }

    /// Applies `f` to the `chatgpt` section under the lock file; writes only when `f` reports a change.
    pub(crate) fn update_chatgpt(&self, f: impl FnOnce(&mut ChatgptData) -> bool) -> Result<bool> {
        self.modify(|d| f(&mut d.chatgpt))
    }

    /// `update_chatgpt` for a caller that holds the lock file (`lock_path`) already.
    pub(crate) fn update_chatgpt_locked(&self, f: impl FnOnce(&mut ChatgptData) -> bool) -> Result<bool> {
        self.modify_locked(|d| f(&mut d.chatgpt))
    }
}

/// `Store::resolve` for an entry that need not be saved yet, on the profile's store.
pub async fn resolve_entry(provider: &str, entry: &Entry) -> Result<Resolved> {
    resolve_entry_in(Store::new().path(), provider, entry).await
}

/// `resolve_entry` with its command result cached for the store file `scope`.
async fn resolve_entry_in(scope: &Path, provider: &str, entry: &Entry) -> Result<Resolved> {
    let base_url = entry
        .base_url
        .as_deref()
        .map(check_base_url)
        .transpose()
        .with_context(|| format!("base URL for {provider}"))?;
    let key = match entry.origin() {
        Some(Origin::Stored) => entry.key.clone().unwrap_or_default(),
        Some(Origin::Env(n)) => match std::env::var(&n) {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => bail!("${n} (the key for {provider}) is not set"),
        },
        Some(Origin::Cmd(c)) => run_cmd(scope, provider, &c, CMD_TIMEOUT).await?,
        None => bail!("no API key stored for {provider}"),
    };
    mu_base::secrets::register_stored(&label(provider), &key);
    Ok(Resolved { key, base_url })
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// The name a provider's key is redacted under.
fn label(provider: &str) -> String {
    crate::provider::providers::native_def(provider).map_or_else(
        || format!("{}_API_KEY", provider.to_ascii_uppercase().replace('-', "_")),
        |d| d.env_hint.to_string(),
    )
}

async fn run_cmd(scope: &Path, provider: &str, cmd: &str, limit: Duration) -> Result<String> {
    let key = (scope.to_path_buf(), provider.to_string());
    let cached = CMD_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key).cloned());
    if let Some((c, v)) = cached
        && c == cmd
    {
        return Ok(v);
    }
    let label = cmd_label(cmd);
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmd);
    let out = mu_base::process::run_capture(c, limit)
        .await
        .ok_or_else(|| anyhow!("{label} (the key for {provider}) failed or took longer than {}s", limit.as_secs()))?;
    let out = out.trim().to_string();
    if out.is_empty() {
        bail!("{label} (the key for {provider}) printed nothing");
    }
    CMD_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, (cmd.to_string(), out.clone()));
    Ok(out)
}

/// Drops the cached `!command` result for `provider` in the store file `scope`. `true` when there
/// was one, so the caller knows a second try (after a 401) can produce a different key.
pub fn forget_cached(scope: &Path, provider: &str) -> bool {
    let key = (scope.to_path_buf(), provider.to_string());
    CMD_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|m| m.remove(&key).is_some())
}

/// Registers every stored key for redaction; re-reads the file at most once a second.
pub fn sync_redaction() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
        return;
    }
    *last = Some(Instant::now());
    drop(last);
    Store::new().snapshot();
}

/// `…` and the last four characters; shorter keys show nothing.
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 { "…".into() } else { format!("…{}", chars[chars.len() - 4..].iter().collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::block_on as rt;
    use std::os::unix::fs::PermissionsExt;

    fn store(tag: &str) -> (Store, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-auth-{tag}-{}", uuid::Uuid::new_v4()));
        (Store::at(d.join("auth.json")), d)
    }

    /// Replaces the file the way another session does: a new inode, so the cache notices.
    fn replace_file(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn the_file_is_0600_after_create_and_rewrite() {
        let (s, d) = store("mode");
        s.set("anthropic", Entry::literal("sk-ant-test-0123456789")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(s.path()), 0o600);
        s.set("openai", Entry::reference("$OPENAI_API_KEY").unwrap()).unwrap();
        assert_eq!(mode(s.path()), 0o600);
        assert_eq!(mode(&d), 0o700);
        assert!(s.permission_warning().is_none());
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(s.permission_warning().unwrap().contains("chmod 600"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_format_is_the_documented_one() {
        let (s, d) = store("fmt");
        s.set("anthropic", Entry::literal("sk-ant-test-0123456789")).unwrap();
        let mut e = Entry::reference("!op read op://dev/openai/key").unwrap();
        e.base_url = Some("http://127.0.0.1:9".into());
        s.set("openai", e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(s.path()).unwrap()).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["providers"]["anthropic"]["key"], "sk-ant-test-0123456789");
        assert_eq!(v["providers"]["anthropic"]["chat"], true);
        assert!(v["providers"]["anthropic"]["added"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["providers"]["openai"]["ref"], "!op read op://dev/openai/key");
        assert_eq!(v["providers"]["openai"]["baseUrl"], "http://127.0.0.1:9");
        assert!(v["providers"]["openai"].get("key").is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_version_1_entry_routes_chat_and_is_made_explicit_on_rewrite() {
        let (s, d) = store("v1");
        replace_file(s.path(), r#"{"version":1,"providers":{"openai":{"key":"sk-test-0123456789"}}}"#);
        let e = s.get("openai").unwrap();
        assert!(e.chat.is_none() && e.routes_chat());
        s.set("parallel", Entry::literal("par-test-0123456789")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(s.path()).unwrap()).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["providers"]["openai"]["chat"], true);
        assert!(v["providers"]["parallel"].get("chat").is_none(), "search keys have no chat flag");
        assert!(s.set_chat("openai", false).unwrap());
        assert!(!s.get("openai").unwrap().routes_chat());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_newer_file_is_refused() {
        let (s, d) = store("v3");
        replace_file(s.path(), r#"{"version":4,"providers":{}}"#);
        assert!(s.check().is_err());
        assert!(s.set("openai", Entry::literal("sk-test-0123456789")).is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let (s, d) = store("conc");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        s.set(&format!("p{i}-{j}"), Entry::literal(&format!("key-{i}-{j}-xxxxxxxx"))).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(s.list().len(), 40);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn running_sessions_see_changes_without_a_restart() {
        let (s, d) = store("reread");
        assert!(!s.has("baseten"));
        let other = s.clone(); // another session's handle
        other.set("baseten", Entry::literal("bt-0123456789abcdef")).unwrap();
        assert!(s.has("baseten"));
        assert!(other.remove("baseten").unwrap());
        assert!(!s.has("baseten"));
        assert!(!other.remove("baseten").unwrap());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_unset_variable_is_an_error_never_a_literal() {
        let (s, d) = store("unset");
        s.set("openai", Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap()).unwrap();
        let e = rt(s.resolve("openai")).err().unwrap().to_string();
        assert!(e.contains("MU_TEST_SURELY_UNSET_VAR") && e.contains("not set"), "{e}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn references_resolve() {
        let (s, d) = store("refs");
        s.set("anthropic", Entry::literal("  sk-ant-literal-0123  ")).unwrap();
        assert_eq!(rt(s.resolve("anthropic")).unwrap().key, "sk-ant-literal-0123");
        s.set("openai", Entry::reference("$PATH").unwrap()).unwrap();
        assert_eq!(rt(s.resolve("openai")).unwrap().key, std::env::var("PATH").unwrap().trim());
        s.set("baseten", Entry::reference("!echo  bt-from-cmd-0123 ").unwrap()).unwrap();
        assert_eq!(rt(s.resolve("baseten")).unwrap().key, "bt-from-cmd-0123");
        assert!(rt(s.resolve("nobody")).is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_command_is_capped_and_cached_until_forgotten() {
        let (s, d) = store("cmd");
        let counter = d.join("count");
        std::fs::create_dir_all(&d).unwrap();
        let cmd = format!("!echo x >> {}; echo key-from-cmd-0123456", counter.display());
        s.set("p-cmd", Entry::reference(&cmd).unwrap()).unwrap();
        let runs = || std::fs::read_to_string(&counter).map_or(0, |t| t.lines().count());
        assert_eq!(rt(s.resolve("p-cmd")).unwrap().key, "key-from-cmd-0123456");
        assert_eq!(rt(s.resolve("p-cmd")).unwrap().key, "key-from-cmd-0123456");
        assert_eq!(runs(), 1, "second resolve is cached");
        assert!(forget_cached(s.path(), "p-cmd"));
        rt(s.resolve("p-cmd")).unwrap();
        assert_eq!(runs(), 2, "runs again after a rejection");
        // failing and silent commands are errors
        s.set("p-fail", Entry::reference("!exit 3").unwrap()).unwrap();
        assert!(rt(s.resolve("p-fail")).is_err());
        s.set("p-empty", Entry::reference("!true").unwrap()).unwrap();
        assert!(rt(s.resolve("p-empty")).err().unwrap().to_string().contains("printed nothing"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_hanging_command_times_out() {
        let e =
            rt(run_cmd(Path::new("hang"), "p-hang", "sleep 5", Duration::from_millis(200))).err().unwrap().to_string();
        assert!(e.contains("failed or took longer than"), "{e}");
    }

    #[test]
    fn bad_references_and_urls_are_rejected() {
        assert!(Entry::reference("sk-ant-oops").is_err());
        assert!(Entry::reference("$").is_err());
        assert!(Entry::reference("$1BAD").is_err());
        assert!(Entry::reference("!  ").is_err());
        let (s, d) = store("bad");
        let mut e = Entry::literal("sk-ant-test-0123456789");
        e.base_url = Some("http://example.com".into());
        assert!(s.set("anthropic", e).is_err());
        assert!(!s.path().exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_corrupt_file_is_never_overwritten() {
        let (s, d) = store("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(s.path(), "{not json").unwrap();
        assert!(s.check().is_err());
        assert!(!s.has("anthropic"));
        assert!(s.set("anthropic", Entry::literal("sk-ant-test-0123456789")).is_err());
        assert_eq!(std::fs::read_to_string(s.path()).unwrap(), "{not json");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_stored_base_url_is_checked_on_resolve() {
        let (s, d) = store("baseurl");
        for bad in ["http://example.com", "https://user:pw@example.com", "ftp://example.com"] {
            let json =
                format!(r#"{{"version":1,"providers":{{"openai":{{"key":"sk-test-0123456789","baseUrl":"{bad}"}}}}}}"#);
            replace_file(s.path(), &json);
            let e = rt(s.resolve("openai")).err().unwrap().to_string();
            assert!(e.contains("base URL for openai") && !e.contains("pw@"), "{e}");
            assert!(s.check().unwrap_err().to_string().contains("openai"));
        }
        replace_file(
            s.path(),
            r#"{"version":1,"providers":{"openai":{"key":"sk-test-0123456789","baseUrl":"http://127.0.0.1:9"}}}"#,
        );
        assert_eq!(rt(s.resolve("openai")).unwrap().base_url.as_deref(), Some("http://127.0.0.1:9"));
        s.check().unwrap();
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_same_size_rewrite_is_seen_at_once() {
        let (s, d) = store("stamp");
        s.set("openai", Entry::literal("sk-test-aaaaaaaaaa")).unwrap();
        assert_eq!(s.get("openai").unwrap().key.as_deref(), Some("sk-test-aaaaaaaaaa"));
        let text = std::fs::read_to_string(s.path()).unwrap().replace("aaaaaaaaaa", "bbbbbbbbbb");
        replace_file(s.path(), &text);
        assert_eq!(s.get("openai").unwrap().key.as_deref(), Some("sk-test-bbbbbbbbbb"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn base_urls_must_not_carry_credentials() {
        assert!(check_base_url("https://user:pw@api.example.com").is_err());
        assert!(check_base_url("http://user:pw@example.com").is_err());
        assert_eq!(check_base_url(" https://api.example.com/v1/ ").unwrap(), "https://api.example.com/v1");
        assert!(check_base_url("http://127.0.0.1:9").is_ok());
    }

    #[test]
    fn forgetting_a_command_result_reports_whether_there_was_one() {
        let (s, d) = store("forget");
        s.set("p-forget", Entry::reference("!echo key-forget-0123456").unwrap()).unwrap();
        assert!(!forget_cached(s.path(), "p-forget"));
        assert_eq!(rt(s.resolve("p-forget")).unwrap().key, "key-forget-0123456");
        assert!(forget_cached(s.path(), "p-forget"));
        assert!(!forget_cached(s.path(), "p-forget"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn errors_and_debug_do_not_echo_keys_or_command_lines() {
        let e = Entry::reference("sk-ant-oops-0123456789").unwrap_err().to_string();
        assert!(!e.contains("sk-ant"), "{e}");
        let k = Entry::literal("sk-ant-test-0123456789");
        let dbg = format!("{k:?}");
        assert!(!dbg.contains("0123456789") && !dbg.contains("sk-ant"), "{dbg}");
        let c = Entry::reference("!op read --token sk-abc-0123456789").unwrap();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("!op …") && !dbg.contains("sk-abc"), "{dbg}");
        let err = rt(run_cmd(
            Path::new("quiet"),
            "p-quiet",
            "sh -c 'echo sk-leak-0123456789; exit 3'",
            Duration::from_secs(5),
        ))
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("!sh …") && !err.contains("sk-leak"), "{err}");
    }

    #[test]
    fn masking_shows_at_most_four_characters() {
        assert_eq!(mask("sk-ant-api03-abcdefghijWXYZ"), "…WXYZ");
        assert_eq!(mask("short"), "…");
        assert_eq!(mask(""), "…");
    }

    #[test]
    fn a_stored_key_is_redacted_from_tool_output() {
        let (s, d) = store("redact");
        // Not a recognisable key format: only the stored-key registry can catch it.
        let key = format!("bt-{}", uuid::Uuid::new_v4().simple());
        s.set("baseten", Entry::literal(&key)).unwrap();
        let out =
            mu_base::secrets::redact(&format!("cat auth.json -> {{\"key\": \"{key}\"}}"), Path::new("/nonexistent"));
        assert!(!out.text.contains(&key), "{}", out.text);
        assert!(out.text.contains("[REDACTED:BASETEN_API_KEY]"), "{}", out.text);
        assert!(out.hits.iter().any(|h| h.source == mu_base::secrets::Source::Stored));
        let _ = std::fs::remove_dir_all(d);
    }
}
