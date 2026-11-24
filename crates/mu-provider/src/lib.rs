//! Model access for mu: provider wires, the model catalogue and pricing, key storage, and the
//! gateway endpoint. Depends on `mu-base` and `mu-common` only; the binary supplies the gateway
//! start function, so the plugin host stays outside.

pub mod auth;
pub mod gateway;
pub mod model;
pub mod net;
pub mod pi_import;
pub mod provider;
