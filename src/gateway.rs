use crate::plugin::PluginHost;
use anyhow::{Result, anyhow};
use mu_common::paths;
use std::rc::Rc;

/// Where gateway requests go: `MU_GATEWAY_URL` if set, else the gateway plugin.
pub struct GatewaySource {
    host: Rc<PluginHost>,
}

impl GatewaySource {
    pub fn new(host: Rc<PluginHost>) -> Self {
        Self { host }
    }

    pub async fn endpoint(&self) -> Result<(String, String)> {
        match std::env::var("MU_GATEWAY_URL") {
            Ok(u) if !u.is_empty() => {
                let token = match std::env::var("MU_GATEWAY_TOKEN") {
                    Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                    _ => {
                        let p = paths::omp_config_root().join("auth-gateway.token");
                        std::fs::read_to_string(&p).map(|s| s.trim().to_string()).map_err(|_| {
                            anyhow!("gateway token not found at {}; run `omp auth-gateway token`", p.display())
                        })?
                    }
                };
                Ok((u.trim_end_matches('/').into(), token))
            }
            _ => self.host.provider_endpoint("gateway").await.map_err(|e| anyhow!(e)),
        }
    }
}
