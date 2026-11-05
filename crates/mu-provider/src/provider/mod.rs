//! Provider access: the contract (`types`), the client that routes and streams requests
//! (`client`), role-fallback completion (`complete`), the gateway's model list
//! (`gateway_models`), the providers mu calls with an API key (`providers`), and the wires
//! (`wire`).

pub mod client;
pub(crate) mod complete;
pub mod endpoint;
pub(crate) mod gateway_models;
pub mod providers;
pub(crate) mod retry;
pub(crate) mod sse;
pub mod types;
pub(crate) mod wire;
