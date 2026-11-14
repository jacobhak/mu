//! Live model discovery for Baseten (`GET {base}/models`). It supplies context windows, output
//! limits, prices and capabilities for models the snapshot does not list or has stale numbers for.
//! The snapshot row still decides thinking and Chat Completions quirks; a discovered id without a
//! snapshot row gets reasoning off (an unknown family's reasoning shape is never guessed).
//! Cached per profile in `native-models.json`, refreshed at most daily.

use crate::provider::custom::CustomProvider;
use crate::provider::providers::{ProviderDef, native_def};
use crate::provider::types::Auth;
use mu_base::jsoncache::due;
use mu_base::models::{Api, CompletionsCompat, ModelSpec, ThinkingMode, ThinkingSpec};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// One model as the provider lists it. Prices are dollars per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovered {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_read_cost: f64,
    pub tools: bool,
    pub images: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    /// Unix milliseconds of the last successful fetch, per provider.
    fetched_at: std::collections::BTreeMap<String, i64>,
    /// Unix milliseconds of the last failed fetch, per provider.
    #[serde(default)]
    failed_at: std::collections::BTreeMap<String, i64>,
    models: std::collections::BTreeMap<String, Vec<Discovered>>,
    /// Custom endpoints' listings, by provider id and a hash of the base URL they came from.
    #[serde(default)]
    custom: std::collections::BTreeMap<String, CustomEntry>,
}

/// One custom endpoint's last listing.
#[derive(Default, Serialize, Deserialize)]
struct CustomEntry {
    fetched_at: Option<i64>,
    failed_at: Option<i64>,
    models: Vec<CustomRow>,
}

/// A model a custom endpoint lists. Missing metadata is unknown, never a limit or a price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomRow {
    pub id: String,
    pub context_length: Option<u32>,
    pub max_completion_tokens: Option<u32>,
}

/// Why a custom endpoint's listing could not be fetched.
#[derive(Debug)]
pub enum DiscoverError {
    /// 401/403: the endpoint refuses the credential.
    Rejected(String),
    /// Anything else: unreachable, another status, or an unusable body.
    Failed(String),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// Providers that list their models over the API.
pub fn discovers(def: &ProviderDef) -> bool {
    def.id == "baseten"
}

fn cache() -> mu_base::jsoncache::JsonCache {
    mu_base::jsoncache::JsonCache::new(mu_common::paths::profile_home().join("native-models.json"))
}

fn num(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
    .max(0.0)
}

fn positive(v: &serde_json::Value) -> Option<u32> {
    let n = num(v);
    (n >= 1.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

/// Maps a `/models` body. Prices arrive as dollars per token (strings or numbers).
pub fn parse(body: &str) -> Vec<Discovered> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    let strs = |v: &serde_json::Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let features = strs(&m["supported_features"]);
                    let price = |k: &str| num(&m["pricing"][k]) * 1_000_000.0;
                    Some(Discovered {
                        id: m["id"].as_str()?.to_string(),
                        name: m["name"].as_str().map(String::from),
                        context_length: positive(&m["context_length"]),
                        max_completion_tokens: positive(&m["max_completion_tokens"]),
                        input_cost: price("prompt"),
                        output_cost: price("completion"),
                        cache_read_cost: price("input_cache_read"),
                        tools: features.iter().any(|f| f == "tools"),
                        images: strs(&m["input_modalities"]).iter().any(|i| i == "image"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

type Loaded = (Option<(SystemTime, u64)>, Arc<File>);
static LOADED: Mutex<Option<(std::path::PathBuf, Loaded)>> = Mutex::new(None);

fn load() -> Arc<File> {
    let path = mu_common::paths::profile_home().join("native-models.json");
    let stamp = std::fs::metadata(&path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
    let mut g = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, (s, f))) = g.as_ref()
        && *p == path
        && *s == stamp
    {
        return f.clone();
    }
    let file = Arc::new(cache().load(|t| serde_json::from_str::<File>(t).ok()).unwrap_or_default());
    *g = Some((path, (stamp, file.clone())));
    file
}

/// The last discovered models of `provider`; empty when never fetched.
pub fn cached(provider: &str) -> Vec<Discovered> {
    load().models.get(provider).cloned().unwrap_or_default()
}

pub fn find(provider: &str, id: &str) -> Option<Discovered> {
    load().models.get(provider)?.iter().find(|d| d.id == id).cloned()
}

/// No fetch yet, or the last one is over a day old. A failed fetch is due again after an hour.
pub fn stale(provider: &str) -> bool {
    let f = load();
    due(f.failed_at.get(provider).copied(), f.fetched_at.get(provider).copied())
}

/// The cache key of a custom endpoint: its id and a hash of its base URL, so a changed endpoint
/// never inherits the old server's models and limits.
fn custom_key(c: &CustomProvider) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(c.base_url.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}@{hash}", c.id)
}

/// Maps a generic OpenAI `/models` body: nonblank ids, deduplicated and sorted, with positive
/// context and output limits when the server gives them (`max_model_len` is vLLM's context length).
pub fn parse_generic(body: &str) -> Vec<CustomRow> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    let mut rows: Vec<CustomRow> = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str().filter(|s| !s.trim().is_empty())?;
                    Some(CustomRow {
                        id: id.to_string(),
                        context_length: positive(&m["context_length"]).or_else(|| positive(&m["max_model_len"])),
                        max_completion_tokens: positive(&m["max_completion_tokens"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows.dedup_by(|a, b| a.id == b.id);
    rows
}

/// The last listing of this endpoint (same id and base URL); empty when never fetched.
pub fn custom_cached(c: &CustomProvider) -> Vec<CustomRow> {
    load().custom.get(&custom_key(c)).map(|e| e.models.clone()).unwrap_or_default()
}

pub fn custom_find(c: &CustomProvider, model: &str) -> Option<CustomRow> {
    load().custom.get(&custom_key(c))?.models.iter().find(|r| r.id == model).cloned()
}

pub fn custom_stale(c: &CustomProvider) -> bool {
    let f = load();
    match f.custom.get(&custom_key(c)) {
        Some(e) => due(e.failed_at, e.fetched_at),
        None => true,
    }
}

/// `GET {base}/models` with `key` as the bearer (none for a keyless endpoint). Writes nothing;
/// the base URL is checked again first and redirects are never followed.
pub async fn custom_fetch(c: &CustomProvider, key: Option<&str>) -> Result<Vec<CustomRow>, DiscoverError> {
    let failed = |m: String| DiscoverError::Failed(mu_base::secrets::redact_keys(&m));
    let base = crate::provider::custom::check_base_url(&c.base_url).map_err(|e| failed(e.to_string()))?;
    let client = crate::net::http_client().map_err(|e| failed(e.to_string()))?;
    let auth = key.map_or(Auth::None, |k| Auth::Bearer(k.to_string()));
    let rb = auth
        .apply(client.get(format!("{base}/models")).timeout(Duration::from_secs(20)))
        .map_err(|e| DiscoverError::Rejected(e.to_string()))?;
    let resp = rb.send().await.map_err(|e| failed(format!("cannot reach {}: {e}", crate::net::host_of(&base))))?;
    let status = resp.status().as_u16();
    if matches!(status, 401 | 403) {
        return Err(DiscoverError::Rejected(format!("{} rejected the credential (HTTP {status})", c.label())));
    }
    if !resp.status().is_success() {
        return Err(failed(format!("{} answered HTTP {status} to GET /models", c.label())));
    }
    let body = resp.text().await.map_err(|e| failed(e.to_string()))?;
    let rows = parse_generic(&body);
    if rows.is_empty() {
        return Err(failed(format!("{} listed no usable models at /models", c.label())));
    }
    Ok(rows)
}

/// Stores `rows` as this endpoint's listing, as of now.
pub fn custom_commit(c: &CustomProvider, rows: Vec<CustomRow>) -> bool {
    update(|f| {
        let e = f.custom.entry(custom_key(c)).or_default();
        e.fetched_at = Some(mu_base::message::now_ms());
        e.failed_at = None;
        e.models = rows;
    })
}

fn custom_record_failure(c: &CustomProvider) {
    update(|f| {
        f.custom.entry(custom_key(c)).or_default().failed_at = Some(mu_base::message::now_ms());
    });
}

/// Fetches and caches this endpoint's listing. A failure keeps the old same-endpoint cache and
/// delays the next automatic try by an hour.
pub async fn custom_refresh(c: &CustomProvider, key: Option<&str>) -> Result<usize, DiscoverError> {
    match custom_fetch(c, key).await {
        Ok(rows) => {
            let n = rows.len();
            custom_commit(c, rows);
            Ok(n)
        }
        Err(e) => {
            custom_record_failure(c);
            Err(e)
        }
    }
}

/// Read-modify-writes the cache file privately, then drops the in-memory copy.
fn update(change: impl FnOnce(&mut File)) -> bool {
    // Concurrent refreshes in one process must not lose each other's rows.
    static WRITING: Mutex<()> = Mutex::new(());
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = cache().load(|t| serde_json::from_str::<File>(t).ok()).unwrap_or_default();
    change(&mut file);
    let saved = serde_json::to_string(&file).map(|t| cache().save_private(&t)).unwrap_or(false);
    *LOADED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    saved
}

fn store(provider: &str, rows: Vec<Discovered>) -> bool {
    update(|f| {
        f.fetched_at.insert(provider.to_string(), mu_base::message::now_ms());
        f.failed_at.remove(provider);
        f.models.insert(provider.to_string(), rows);
    })
}

fn record_failure(provider: &str) {
    update(|f| {
        f.failed_at.insert(provider.to_string(), mu_base::message::now_ms());
    });
}

/// Fetches the provider's model list and caches it. A failure, or an empty answer, keeps the old
/// cache and is retried after an hour.
pub async fn refresh(def: &ProviderDef, key: &str, base_url: Option<&str>) -> anyhow::Result<usize> {
    match fetch(def, key, base_url).await {
        Ok(rows) => {
            let n = rows.len();
            store(def.id, rows);
            Ok(n)
        }
        Err(e) => {
            record_failure(def.id);
            Err(e)
        }
    }
}

async fn fetch(def: &ProviderDef, key: &str, base_url: Option<&str>) -> anyhow::Result<Vec<Discovered>> {
    let base = base_url.unwrap_or(def.base_url);
    let client = crate::net::http_client()?;
    let rb = crate::provider::types::Auth::for_def(def, key)
        .apply(client.get(format!("{base}/models")).timeout(Duration::from_secs(20)))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let resp = rb.send().await.map_err(|e| anyhow::anyhow!("{}", mu_base::secrets::redact_keys(&e.to_string())))?;
    if !resp.status().is_success() {
        anyhow::bail!("{} answered HTTP {}", def.label, resp.status().as_u16());
    }
    let rows = parse(&resp.text().await?);
    if rows.is_empty() {
        anyhow::bail!("{} listed no models", def.label);
    }
    Ok(rows)
}

/// A Chat Completions spec for a model the snapshot does not list: no reasoning, the snapshot's
/// provider-wide request quirks, limits from the listing.
pub fn spec(provider: &str, d: &Discovered, default_window: u32, default_max: u32) -> Option<ModelSpec> {
    let def = native_def(provider)?;
    let mut compat =
        super::catalog::rows(provider).iter().find_map(|r| r.compat.clone()).unwrap_or_else(CompletionsCompat::default);
    compat.reasoning_content_field = None;
    compat.requires_reasoning_content_for_tool_calls = false;
    compat.requires_reasoning_content_for_all_assistant_turns = false;
    compat.reasoning_disable_mode = None;
    compat.strip_image_input = !d.images;
    Some(ModelSpec {
        id: format!("{provider}/{}", d.id),
        provider: provider.into(),
        context_window: d.context_length.unwrap_or(default_window),
        max_output: d.max_completion_tokens.unwrap_or(default_max),
        api: def.api,
        native: true,
        wire_id: d.id.clone(),
        thinking: ThinkingSpec { mode: ThinkingMode::None, efforts: Vec::new() },
        images: d.images && def.api != Api::AnthropicMessages,
        compat: (def.api == Api::OpenAiCompletions).then_some(compat),
        anthropic: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `/v1/models` body in the shape Baseten answers. The first two rows are copied from
    /// omp's inline mock in `packages/catalog/test/baseten-provider.test.ts` (v18.15.0); the rest are
    /// synthetic edge cases.
    const BODY: &str = r#"{"data":[
      {"id":"moonshotai/Kimi-K3","object":"model","name":"Kimi K3","context_length":1048576,"max_completion_tokens":262144,
       "supported_features":["tools","json_mode","structured_outputs","reasoning_effort"],"input_modalities":["text","image"],
       "pricing":{"prompt":"0.000003","completion":"0.000015","input_cache_read":"0.0000003"}},
      {"id":"zai-org/GLM-4.7","object":"model","name":"GLM 4.7","supported_features":["tools","reasoning"],"input_modalities":["text"]},
      {"id":"acme/Chat-1","object":"model","context_length":0,"pricing":{"prompt":0.000001,"completion":2e-6}},
      {"object":"model"}]}"#;

    #[test]
    fn the_listing_maps_to_limits_prices_and_capabilities() {
        let rows = parse(BODY);
        assert_eq!(rows.len(), 3, "a row without an id is skipped");
        let k = &rows[0];
        assert_eq!((k.context_length, k.max_completion_tokens), (Some(1_048_576), Some(262_144)));
        assert!(
            (k.input_cost - 3.0).abs() < 1e-9
                && (k.output_cost - 15.0).abs() < 1e-9
                && (k.cache_read_cost - 0.3).abs() < 1e-9
        );
        assert!(k.tools && k.images);
        let glm = &rows[1];
        assert_eq!((glm.context_length, glm.input_cost), (None, 0.0));
        assert!(glm.tools && !glm.images);
        let acme = &rows[2];
        assert_eq!(acme.context_length, None, "a zero window is unknown");
        assert!(!acme.tools, "no supported_features means no tools");
        assert!((acme.output_cost - 2.0).abs() < 1e-9, "numeric prices work too");
        assert!(parse("not json").is_empty() && parse("{}").is_empty());
    }

    #[test]
    fn an_unlisted_model_gets_reasoning_off_and_the_providers_quirks() {
        let d = Discovered {
            id: "acme/Chat-1".into(),
            name: None,
            context_length: Some(50_000),
            max_completion_tokens: None,
            input_cost: 1.0,
            output_cost: 2.0,
            cache_read_cost: 0.0,
            tools: true,
            images: false,
        };
        let s = spec("baseten", &d, 200_000, 32_000).unwrap();
        assert_eq!((s.context_window, s.max_output), (50_000, 32_000));
        assert_eq!(s.thinking.mode, ThinkingMode::None);
        assert_eq!((s.wire_id.as_str(), s.id.as_str()), ("acme/Chat-1", "baseten/acme/Chat-1"));
        let c = s.compat.unwrap();
        assert!(!c.requires_reasoning_content_for_tool_calls && c.reasoning_content_field.is_none());
        assert_eq!(c.max_tokens_field, "max_completion_tokens", "the provider's usual field, from the snapshot");
        assert!(!s.images);
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use mu_testutil::{Reply, fake_server};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_refresh_is_cached_and_reaches_resolution_pricing_and_the_picker() {
        mu_testutil::home();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let body = r#"{"data":[
              {"id":"acme/Chat-9","name":"Chat 9","context_length":64000,"max_completion_tokens":8000,
               "supported_features":["tools"],"pricing":{"prompt":"0.000002","completion":"0.000004"}},
              {"id":"acme/NoTools","supported_features":["json_mode"]},
              {"id":"moonshotai/Kimi-K3","context_length":999000,"supported_features":["tools","reasoning_effort"]}]}"#;
            let (base, seen) = fake_server(vec![Reply::new(200, body)]).await;
            let def = native_def("baseten").unwrap();
            assert_eq!(refresh(def, "bt-key-0123456789", Some(&base)).await.unwrap(), 3);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(req.starts_with("get /models "), "{req}");
            assert!(req.contains("authorization: bearer bt-key-0123456789"), "{req}");
            assert!(!stale("baseten"));

            // A discovered id resolves natively with its limits and no reasoning.
            let (m, _) = mu_base::models::resolve(
                "baseten/acme/Chat-9",
                &Default::default(),
                None,
                &crate::provider::providers::direct_spec,
            )
            .unwrap();
            assert_eq!((m.context_window, m.max_output, m.thinking.mode), (64_000, 8_000, ThinkingMode::None));
            // A snapshot row keeps its thinking and quirks but takes the fresher limits.
            let (k, _) = mu_base::models::resolve(
                "baseten/moonshotai/Kimi-K3",
                &Default::default(),
                None,
                &crate::provider::providers::direct_spec,
            )
            .unwrap();
            assert_eq!((k.context_window, k.thinking.mode), (999_000, ThinkingMode::Effort));
            assert!(k.compat.unwrap().requires_reasoning_content_for_tool_calls);
            // Priced from the listing.
            let u = mu_base::message::Usage { input: 1_000_000, output: 1_000_000, ..Default::default() };
            let c = crate::model::pricing::cost("baseten/acme/Chat-9", &u).unwrap();
            assert!((c.total - 6.0).abs() < 1e-9, "{c:?}");

            // The picker lists tools-capable discovered ids that the snapshot lacks.
            let ids: Vec<String> = cached("baseten")
                .into_iter()
                .filter(|d| d.tools && super::super::catalog::row("baseten", &d.id).is_none())
                .map(|d| d.id)
                .collect();
            assert_eq!(ids, ["acme/Chat-9"]);

            // An empty or failing answer keeps the good cache.
            let (bad, _) = fake_server(vec![Reply::new(200, r#"{"data":[]}"#)]).await;
            assert!(refresh(def, "bt-key-0123456789", Some(&bad)).await.is_err());
            let (down, _) = fake_server(vec![Reply::new(503, "x")]).await;
            assert!(refresh(def, "bt-key-0123456789", Some(&down)).await.is_err());
            assert_eq!(cached("baseten").len(), 3);

            // A failure is retried after an hour, not on every start; the cache file is private.
            assert!(!stale("baseten"), "a failure just now is not retried yet");
            update(|f| {
                let now = mu_base::message::now_ms();
                f.fetched_at.insert("baseten".into(), now - 3 * 3_600_000);
                f.failed_at.insert("baseten".into(), now - 2 * 3_600_000);
            });
            assert!(stale("baseten"));
            update(|f| {
                f.fetched_at.insert("baseten".into(), mu_base::message::now_ms() - 25 * 3_600_000);
                f.failed_at.clear();
            });
            assert!(stale("baseten"));
            let path = mu_common::paths::profile_home().join("native-models.json");
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        });
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;
    use mu_testutil::{Reply, block_on, fake_server};

    fn provider(id: &str, base: &str) -> CustomProvider {
        CustomProvider::new(base).validated(id).unwrap()
    }

    #[test]
    fn a_bare_openai_listing_parses_without_baseten_features() {
        let rows = parse_generic(
            r#"{"data":[{"id":"org/b"},{"id":"org/a","context_length":32768,"max_completion_tokens":4096},
               {"id":"org/a"},{"id":"  "},{"id":"org/vllm","max_model_len":131072},{"id":"org/zero","context_length":0},{"object":"model"}]}"#,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["org/a", "org/b", "org/vllm", "org/zero"], "sorted, deduplicated, blank ids dropped");
        assert_eq!((rows[0].context_length, rows[0].max_completion_tokens), (Some(32768), Some(4096)));
        assert_eq!((rows[1].context_length, rows[1].max_completion_tokens), (None, None));
        assert_eq!(rows[2].context_length, Some(131_072));
        assert_eq!(rows[3].context_length, None, "a zero window is unknown");
        assert!(parse_generic("nope").is_empty() && parse_generic("{}").is_empty());
    }

    #[test]
    fn a_fetch_is_draft_only_and_a_refresh_keeps_the_same_endpoints_cache_on_failure() {
        mu_testutil::home();
        block_on(async {
            let body = r#"{"data":[{"id":"org/m","context_length":4096},{"id":"org/n"}]}"#;
            let (base, seen) = fake_server(vec![
                Reply::new(200, body),
                Reply::new(200, body),
                Reply::new(200, r#"{"data":[]}"#),
                Reply::new(200, "<html>"),
                Reply::new(404, "x"),
                Reply::new(401, "x"),
                Reply::new(503, "x"),
            ])
            .await;
            let c = provider("disc-a", &base);
            let rows = custom_fetch(&c, Some("tok-0123456789")).await.unwrap();
            assert_eq!(rows.len(), 2);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(req.starts_with("get /models ") && req.contains("authorization: bearer tok-0123456789"), "{req}");
            assert!(custom_cached(&c).is_empty(), "a draft fetch writes nothing");

            assert_eq!(custom_refresh(&c, None).await.unwrap(), 2);
            assert!(!seen.lock().unwrap()[1].to_lowercase().contains("authorization"));
            assert_eq!(custom_cached(&c).len(), 2);
            assert_eq!(custom_find(&c, "org/m").unwrap().context_length, Some(4096));
            assert!(!custom_stale(&c));

            // Empty, malformed, 404, 401 and 5xx answers all keep the cache and say why.
            for want_rejected in [false, false, false, true, false] {
                let e = custom_refresh(&c, None).await.unwrap_err();
                assert_eq!(matches!(e, DiscoverError::Rejected(_)), want_rejected, "{e}");
                assert_eq!(custom_cached(&c).len(), 2);
            }
            assert!(!custom_stale(&c), "a failure just now is retried only after an hour");

            // The same id at another base URL inherits nothing.
            let moved = provider("disc-a", "https://moved.example/v1");
            assert!(custom_cached(&moved).is_empty() && custom_find(&moved, "org/m").is_none());
            assert!(custom_stale(&moved));
            // Returning to the first base finds its cache again.
            assert_eq!(custom_cached(&provider("disc-a", &base)).len(), 2);
        });
    }
}
