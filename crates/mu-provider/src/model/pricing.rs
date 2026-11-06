//! Per-model token prices, from omp's catalogue.
//!
//! Source: `omp models ls --json` (per-million rates, `pricingStatus`). It lists only models the active
//! profile can use, so its output is cached per profile (`paths::profile_home()/pricing.json`) and
//! refreshed at most once a day, off the draw path. A model missing from the cache forces one refresh.
use mu_base::message::{Cost, Usage};
use mu_common::paths;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 3600);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct LongContext {
    threshold: f64,
    rates: Rates,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    name: Option<String>,
    reasoning: bool,
    /// `None` unless omp reports the price as fixed.
    rates: Option<Rates>,
    long_context: Option<LongContext>,
}

#[derive(Debug, Default)]
pub struct Table {
    entries: HashMap<String, Entry>,
}

#[derive(Deserialize)]
struct Raw {
    models: Vec<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    provider: String,
    id: String,
    name: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(rename = "pricingStatus", default)]
    pricing_status: String,
    #[serde(default)]
    cost: RawCost,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawCost {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    long_context: Option<RawLong>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLong {
    input_threshold: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

impl Table {
    pub fn from_json(text: &str) -> Option<Self> {
        let raw: Raw = serde_json::from_str(text).ok()?;
        let entries = raw
            .models
            .into_iter()
            .map(|m| {
                let rates = |c: &RawCost| Rates {
                    input: c.input,
                    output: c.output,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                };
                let fixed = m.pricing_status == "fixed";
                let long_context = m.cost.long_context.as_ref().map(|l| LongContext {
                    threshold: l.input_threshold,
                    rates: Rates {
                        input: l.input,
                        output: l.output,
                        cache_read: l.cache_read,
                        cache_write: l.cache_write,
                    },
                });
                let entry = Entry {
                    name: m.name,
                    reasoning: m.reasoning,
                    rates: fixed.then(|| rates(&m.cost)),
                    long_context: if fixed { long_context } else { None },
                };
                (format!("{}/{}", m.provider, m.id), entry)
            })
            .collect();
        Some(Self { entries })
    }

    fn cost_with(&self, model_id: &str, usage: &Usage) -> Option<Cost> {
        price(self.entries.get(model_id)?, usage)
    }

    fn display_name_with(&self, model_id: &str) -> Option<String> {
        let name = self.entries.get(model_id)?.name.as_deref()?;
        Some(name.strip_prefix("Claude ").unwrap_or(name).to_string())
    }
}

fn price(entry: &Entry, usage: &Usage) -> Option<Cost> {
    let base = entry.rates?;
    // omp prices the whole prompt (fresh + cached) against the long-context threshold.
    let prompt = (usage.input + usage.cache_read + usage.cache_write) as f64;
    let r = match &entry.long_context {
        Some(l) if prompt > l.threshold => l.rates,
        _ => base,
    };
    let million = 1_000_000.0;
    let input = usage.input as f64 * r.input / million;
    let output = usage.output as f64 * r.output / million;
    let cache_read = usage.cache_read as f64 * r.cache_read / million;
    let cache_write = usage.cache_write as f64 * r.cache_write / million;
    Some(Cost { input, output, cache_read, cache_write, total: input + output + cache_read + cache_write })
}

static TABLE: RwLock<Option<Arc<Table>>> = RwLock::new(None);
/// A forced refresh (for a model missing from the cache) already ran or is running in this process.
static FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn cache() -> mu_base::jsoncache::JsonCache {
    mu_base::jsoncache::JsonCache::new(paths::profile_home().join("pricing.json"))
}

fn table() -> Arc<Table> {
    if let Some(t) = TABLE.read().ok().and_then(|g| g.clone()) {
        return t;
    }
    let t = Arc::new(cache().load(Table::from_json).unwrap_or_default());
    if let Ok(mut g) = TABLE.write() {
        *g = Some(t.clone());
    }
    t
}

/// The snapshot row of a provider mu drives natively: prices and names without omp.
fn snapshot_entry(model_id: &str) -> Option<Entry> {
    let (provider, id) = model_id.split_once('/')?;
    let Some(row) = crate::model::catalog::row(provider, id) else {
        // Not in the snapshot: a model the provider lists live (Baseten).
        let d = crate::model::discovery::find(provider, id)?;
        let priced = d.input_cost > 0.0 || d.output_cost > 0.0;
        return Some(Entry {
            name: d.name,
            reasoning: false,
            rates: priced.then_some(Rates {
                input: d.input_cost,
                output: d.output_cost,
                cache_read: d.cache_read_cost,
                cache_write: 0.0,
            }),
            long_context: None,
        });
    };
    let to_rates = |r: &crate::model::catalog::Cost| Rates {
        input: r.input,
        output: r.output,
        cache_read: r.cache_read,
        cache_write: r.cache_write,
    };
    let cost = row.cost.filter(|c| c.input != 0.0 || c.output != 0.0);
    let found = crate::model::discovery::find(provider, id);
    let (rates, listed) = with_listed_prices(cost.as_ref().map(to_rates), found.as_ref());
    Some(Entry {
        name: row.name.clone(),
        reasoning: row.reasoning,
        rates,
        long_context: if listed {
            None
        } else {
            cost.and_then(|c| c.long_context).map(|l| LongContext {
                threshold: l.input_threshold,
                rates: Rates { input: l.input, output: l.output, cache_read: l.cache_read, cache_write: l.cache_write },
            })
        },
    })
}

/// A discovered listing is the provider's current price list: its positive prices replace the
/// snapshot's field by field. Listings carry no long-context tier, so an override drops the snapshot's.
fn with_listed_prices(snap: Option<Rates>, d: Option<&crate::model::discovery::Discovered>) -> (Option<Rates>, bool) {
    let Some(d) = d.filter(|d| d.input_cost > 0.0 || d.output_cost > 0.0) else { return (snap, false) };
    let base = snap.unwrap_or(Rates { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 });
    let pick = |live: f64, snapshot: f64| if live > 0.0 { live } else { snapshot };
    let rates = Rates {
        input: pick(d.input_cost, base.input),
        output: pick(d.output_cost, base.output),
        cache_read: pick(d.cache_read_cost, base.cache_read),
        cache_write: base.cache_write,
    };
    (Some(rates), true)
}

/// Priced usage for `model_id` (`provider/model`), or `None` when there is no fixed price: omp's
/// catalogue first, else mu's snapshot (the providers it drives natively, which need no omp).
pub fn cost(model_id: &str, usage: &Usage) -> Option<Cost> {
    cost_in(&table(), model_id, usage)
}

fn cost_in(t: &Table, model_id: &str, usage: &Usage) -> Option<Cost> {
    // An omp entry without a fixed price falls through to the snapshot.
    if let Some(c) = t.cost_with(model_id, usage) {
        return Some(c);
    }
    if let Some(id) = openai_plan_model(model_id)
        && let Some(c) = t.cost_with(&id, usage)
    {
        return Some(c);
    }
    price(&snapshot_entry(model_id)?, usage)
}

fn openai_plan_model(model_id: &str) -> Option<String> {
    let (provider, model) = model_id.split_once('/')?;
    (provider == crate::auth::chatgpt::PROVIDER_ID).then(|| format!("openai/{model}"))
}

/// Whether the model is reasoning-capable; `None` when unknown.
pub fn reasoning(model_id: &str) -> Option<bool> {
    table().entries.get(model_id).map(|e| e.reasoning).or_else(|| snapshot_entry(model_id).map(|e| e.reasoning))
}

/// Whether a catalogue has an entry for the model, priced or not.
pub fn known(model_id: &str) -> bool {
    table().entries.contains_key(model_id)
        || openai_plan_model(model_id).is_some_and(|id| table().entries.contains_key(&id))
        || snapshot_entry(model_id).is_some()
}

/// Catalogue name without the `Claude ` prefix, else a name derived from the model id.
pub fn display_name(model_id: &str) -> String {
    table()
        .display_name_with(model_id)
        .or_else(|| {
            let n = snapshot_entry(model_id)?.name?;
            Some(n.strip_prefix("Claude ").unwrap_or(&n).to_string())
        })
        .unwrap_or_else(|| heuristic_name(model_id))
}

fn heuristic_name(model_id: &str) -> String {
    let id = model_id.rsplit('/').next().unwrap_or(model_id);
    let mut out = String::new();
    let mut prev_numeric = false;
    for w in id.split('-').filter(|w| !w.is_empty() && *w != "claude") {
        let numeric = w.chars().all(|c| c.is_ascii_digit() || c == '.');
        if !out.is_empty() {
            out.push(if numeric && prev_numeric { '.' } else { ' ' });
        }
        if numeric {
            out.push_str(w);
        } else if w.len() <= 3 {
            out.push_str(&w.to_ascii_uppercase());
        } else {
            let mut cs = w.chars();
            if let Some(c) = cs.next() {
                out.extend(c.to_uppercase());
                out.push_str(cs.as_str());
            }
        }
        prev_numeric = numeric;
    }
    if out.is_empty() { model_id.to_string() } else { out }
}

fn cache_fresh(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_CACHE_AGE)
}

/// Refreshes the cached catalogue when it is missing or older than a day. Failures keep the old cache.
/// Runs `omp models ls --json` at most once per day; never called from the draw path.
pub async fn refresh() {
    if !cache_fresh(&paths::profile_home().join("pricing.json")) {
        fetch().await;
    }
}

/// Refreshes regardless of the cache's age, once per process: for a model the cache does not list
/// (new model, or a cache written while its provider was not logged in).
pub async fn refresh_missing() {
    if !FORCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        fetch().await;
    }
}

async fn fetch() {
    let out = tokio::process::Command::new(mu_common::paths::omp_bin())
        .args(["models", "ls", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(REFRESH_TIMEOUT, out).await else { return };
    if !out.status.success() {
        return;
    }
    let Ok(text) = String::from_utf8(out.stdout) else { return };
    let Some(parsed) = Table::from_json(&text) else { return };
    if parsed.entries.is_empty() {
        return;
    }
    if cache().save(&text)
        && let Ok(mut g) = TABLE.write()
    {
        *g = Some(Arc::new(parsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOGUE: &str = r#"{"models":[
      {"provider":"anthropic","id":"claude-haiku-5-5","name":"Claude Haiku 5.5","pricingStatus":"fixed",
       "cost":{"input":0.1,"output":0.5,"cacheRead":0.01,"cacheWrite":0.125,
               "longContext":{"inputThreshold":100000,"input":0.5,"output":2.5,"cacheRead":0.05,"cacheWrite":0.625}}},
      {"provider":"anthropic","id":"claude-x","name":"Claude X","pricingStatus":"unknown",
       "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}
    ]}"#;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage { input, output, cache_read, cache_write, ..Default::default() }
    }

    #[test]
    fn prices_base_and_long_context_tiers() {
        let t = Table::from_json(CATALOGUE).unwrap();
        let c = t.cost_with("anthropic/claude-haiku-5-5", &usage(2000, 500, 10_000, 0)).unwrap();
        assert!((c.input - 0.0002).abs() < 1e-12);
        assert!((c.output - 0.00025).abs() < 1e-12);
        assert!((c.cache_read - 0.0001).abs() < 1e-12);
        assert!((c.total - 0.00055).abs() < 1e-12);
        // 150k prompt tokens crosses the 100k threshold: long-context rates for every leg.
        let c = t.cost_with("anthropic/claude-haiku-5-5", &usage(150_000, 1000, 0, 0)).unwrap();
        assert!((c.input - 0.075).abs() < 1e-12);
        assert!((c.output - 0.0025).abs() < 1e-12);
    }

    #[test]
    fn unpriced_models_have_no_cost() {
        let t = Table::from_json(CATALOGUE).unwrap();
        assert!(t.cost_with("anthropic/claude-x", &usage(1, 1, 0, 0)).is_none());
        assert!(t.cost_with("unknown/model", &usage(1, 1, 0, 0)).is_none());
    }

    #[test]
    fn an_unpriced_omp_entry_falls_through_to_the_snapshot() {
        let snap = cost_in(&Table::default(), "anthropic/claude-sonnet-5-5", &usage(1000, 1000, 0, 0));
        assert!(snap.is_some(), "the snapshot prices sonnet");
        let t = Table::from_json(
            r#"{"models":[{"provider":"anthropic","id":"claude-sonnet-5-5","pricingStatus":"unknown"}]}"#,
        )
        .unwrap();
        let priced = |c: Option<Cost>| c.map(|c| c.total);
        assert_eq!(priced(cost_in(&t, "anthropic/claude-sonnet-5-5", &usage(1000, 1000, 0, 0))), priced(snap));
    }

    #[test]
    fn chatgpt_plan_uses_openai_catalogue_prices() {
        let t = Table::from_json(
            r#"{"models":[{"provider":"openai","id":"new-model","pricingStatus":"fixed","cost":{"input":2,"output":4}}]}"#,
        )
        .unwrap();
        let u = usage(1_000_000, 1_000_000, 0, 0);
        assert_eq!(cost_in(&t, "openai-chatgpt/new-model", &u).unwrap().total, 6.0);
    }

    #[test]
    fn a_listing_overrides_the_snapshot_field_by_field() {
        let snap = Rates { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: 0.5 };
        let listed = crate::model::discovery::Discovered {
            id: "moonshotai/Kimi-K3".into(),
            name: None,
            context_length: None,
            max_completion_tokens: None,
            input_cost: 3.0,
            output_cost: 0.0,
            cache_read_cost: 0.3,
            tools: true,
            images: false,
        };
        let (r, listed_ok) = with_listed_prices(Some(snap), Some(&listed));
        assert!(listed_ok);
        assert_eq!(r, Some(Rates { input: 3.0, output: 2.0, cache_read: 0.3, cache_write: 0.5 }));
        let (r, listed_ok) = with_listed_prices(Some(snap), None);
        assert!(!listed_ok && r == Some(snap), "no listing: the snapshot stands");
        let unpriced = crate::model::discovery::Discovered { input_cost: 0.0, output_cost: 0.0, ..listed };
        assert_eq!(with_listed_prices(Some(snap), Some(&unpriced)), (Some(snap), false));
    }

    #[test]
    fn display_names() {
        let t = Table::from_json(CATALOGUE).unwrap();
        assert_eq!(t.display_name_with("anthropic/claude-haiku-5-5").as_deref(), Some("Haiku 5.5"));
        assert_eq!(heuristic_name("anthropic/claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(heuristic_name("modal/zai-org/glm-5.3-flash"), "GLM 5.3 Flash");
    }

    #[test]
    #[ignore]
    fn contract_omp_models_ls() {
        let out = mu_common::paths::run_omp_blocking(&["models", "ls", "--json"], &[]);
        let t = Table::from_json(&out).expect("models ls --json did not parse");
        assert!(!t.entries.is_empty());
        assert!(
            t.entries
                .iter()
                .any(|(k, e)| k.starts_with("anthropic/") && e.rates.as_ref().is_some_and(|r| r.input > 0.0)),
            "no anthropic model with a non-zero input price"
        );
    }
}
