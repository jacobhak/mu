//! The gateway's model list: its `/v1/models` rows, cached on disk per profile so the first status
//! line shows real context windows without starting the gateway.

use super::client::Providers;
use anyhow::{Result, anyhow};
use mu_base::models::GatewayModel;
use serde_json::Value;

impl Providers {
    /// The last known model list (from the on-disk cache or the latest fetch); never touches the network.
    pub fn cached_models(&self) -> Vec<GatewayModel> {
        self.gw_models.borrow().clone().unwrap_or_default()
    }

    /// Gateway `/v1/models` rows (context windows). `None` when the gateway is not running or unreachable.
    pub async fn gateway_models(&self) -> Result<Vec<GatewayModel>> {
        if self.gw_fresh.get()
            && let Some(m) = self.gw_models.borrow().as_ref()
        {
            return Ok(m.clone());
        }
        let ep = self.endpoint().await.map_err(|e| anyhow!("{e}"))?;
        let text = self
            .client()?
            .get(format!("{}/v1/models", ep.base))
            .bearer_auth(&ep.token)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let v: Value = serde_json::from_str(&text)?;
        let rows: Vec<GatewayModel> =
            v["data"].as_array().map(|a| a.iter().filter_map(model_row).collect()).unwrap_or_default();
        if rows.is_empty() {
            return Err(anyhow!("the gateway returned no models"));
        }
        save_cache(&rows);
        *self.gw_models.borrow_mut() = Some(rows.clone());
        self.gw_fresh.set(true);
        Ok(rows)
    }
}

/// One model as the listing and the cache file both write it: `id`, `context_length`, `max_output_tokens`.
fn model_row(m: &Value) -> Option<GatewayModel> {
    Some(GatewayModel {
        id: m["id"].as_str()?.into(),
        context_length: m["context_length"].as_u64().map(|n| n as u32),
        max_output_tokens: m["max_output_tokens"].as_u64().map(|n| n as u32),
    })
}

/// Per profile: each profile's gateway serves its own models.
fn cache() -> mu_base::jsoncache::JsonCache {
    mu_base::jsoncache::JsonCache::new(mu_common::paths::profile_home().join("gateway-models.json"))
}

pub(super) fn load_cache() -> Option<Vec<GatewayModel>> {
    cache().load(|text| {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(v.as_array()?.iter().filter_map(model_row).collect())
    })
}

fn save_cache(rows: &[GatewayModel]) {
    if rows.is_empty() {
        return;
    }
    let v: Vec<Value> = rows
        .iter()
        .map(|m| serde_json::json!({"id": m.id, "context_length": m.context_length, "max_output_tokens": m.max_output_tokens}))
        .collect();
    cache().save(&Value::Array(v).to_string());
}
