//! Protocol and foundation types for mu: messages, events, the tool contract and leaf helpers.
//! Nothing here starts a model request, a tool or a session.

pub mod ask_nudge;
pub mod catchup;
pub mod config;
pub mod events;
pub mod git;
pub mod image;
pub mod jsoncache;
pub mod jtd;
pub mod mem;
pub mod message;
pub mod model_config;
pub mod models;
pub mod permissions;
pub mod pi_import;
pub mod process;
pub mod pty;
pub mod roles;
pub mod runtime;
pub mod schema;
pub mod secrets;
pub mod text;
pub mod tool;
pub mod wire;
