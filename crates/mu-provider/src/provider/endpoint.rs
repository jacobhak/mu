//! Where gateway requests go. The binary supplies the start function (it runs `MU_GATEWAY_URL` or
//! the gateway plugin), so the provider layer does not depend on the gateway or the plugin host.
//!
//! # The gateway plugin contract
//!
//! A gateway is a native plugin whose manifest has `provider = "gateway"`. It serves:
//! - `provider/endpoint {clientPid, start}`: `{baseUrl, token}`, or `null` when `start` is false and nothing
//!   is running. With `start` true it starts the gateway if needed and holds it up while `clientPid` lives.
//! - `provider/prices {}`: `{"models":[{provider,id,name?,reasoning?,pricingStatus?,
//!   cost:{input,output,cacheRead,cacheWrite,longContext?}}]}`.
//!
//! The base URL serves `/v1/messages`, `/v1/models` and `/v1/usage`. `MU_GATEWAY_URL` with `MU_GATEWAY_TOKEN`
//! points mu at a running gateway directly instead; that has no price list.

use anyhow::{Result, anyhow};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

/// A gateway's `(url, token)`, or why there is none.
pub type BoxEndpoint = Pin<Box<dyn Future<Output = Result<(String, String), String>>>>;
pub type StartFn = Box<dyn Fn() -> BoxEndpoint>;

/// The `(url, token)` of a gateway that is already running, or `None`. Never starts one.
pub type BoxPeek = Pin<Box<dyn Future<Output = Option<(String, String)>>>>;
/// The gateway's price list as JSON text, or why there is none.
pub type BoxPrices = Pin<Box<dyn Future<Output = Result<String, String>>>>;
pub type PeekFn = Rc<dyn Fn() -> BoxPeek>;
pub type PricesFn = Rc<dyn Fn() -> BoxPrices>;

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
    peek: PeekFn,
    prices: PricesFn,
}

impl GatewaySource {
    /// A source with no running-gateway lookup and no price list; `with_plugin` adds them.
    pub fn new(start: StartFn) -> Self {
        Self {
            start,
            peek: Rc::new(|| Box::pin(async { None })),
            prices: Rc::new(|| Box::pin(async { Err("no gateway price list".to_string()) })),
        }
    }

    pub fn with_plugin(mut self, peek: PeekFn, prices: PricesFn) -> Self {
        self.peek = peek;
        self.prices = prices;
        self
    }

    pub async fn endpoint(&self) -> Result<(String, String)> {
        (self.start)().await.map_err(|e| anyhow!(e))
    }

    pub fn peek_fn(&self) -> PeekFn {
        self.peek.clone()
    }

    pub fn prices(&self) -> BoxPrices {
        (self.prices)()
    }
}
