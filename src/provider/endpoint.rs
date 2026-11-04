//! Where gateway requests go. The binary supplies the start function (it runs `MU_GATEWAY_URL` or
//! the gateway plugin), so the provider layer does not depend on the gateway or the plugin host.

use anyhow::{Result, anyhow};
use std::future::Future;
use std::pin::Pin;

/// A gateway's `(url, token)`, or why there is none.
pub type BoxEndpoint = Pin<Box<dyn Future<Output = Result<(String, String), String>>>>;
pub type StartFn = Box<dyn Fn() -> BoxEndpoint>;

/// Boxes an async start function for `GatewaySource::new`.
pub fn start_fn<F, Fut>(f: F) -> StartFn
where
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<(String, String), String>> + 'static,
{
    Box::new(move || -> BoxEndpoint { Box::pin(f()) })
}

pub struct GatewaySource {
    start: StartFn,
}

impl GatewaySource {
    pub fn new(start: StartFn) -> Self {
        Self { start }
    }

    pub async fn endpoint(&self) -> Result<(String, String)> {
        (self.start)().await.map_err(|e| anyhow!(e))
    }
}
