//! The versions of the contracts a program that reaches other machines is built on (docs/remote-contracts.md).
//! A number goes up when a program written for the old one could break; `mu contracts` prints them.

pub use crate::wire::WIRE;

/// A: `[task] launcher`.
pub const LAUNCHER: u32 = 2;
/// B: `mu agent`, `mu agent list --json`, `mu attach --stdio`.
pub const AGENTS: u32 = 1;
/// C: `mu --attach COMMAND [--state-key PATH]`.
pub const ATTACH: u32 = 1;
/// D: `[sessions] sources` (2: argv `attach`, `helper` and `state_key` in rows).
pub const SOURCES: u32 = 2;
/// E: `MU_ATTACH_HELPER`.
pub const HELPER: u32 = 1;

/// The launcher's environment variable that carries `LAUNCHER`.
pub const LAUNCHER_ENV: &str = "MU_TASK_CONTRACT";
/// A session source's environment variable that carries `SOURCES`.
pub const SOURCES_ENV: &str = "MU_SOURCES_CONTRACT";
/// The attach helper's environment variable that carries `HELPER`.
pub const HELPER_ENV: &str = "MU_ATTACH_HELPER_CONTRACT";

/// Every number, in the shape `mu contracts` prints.
pub fn json() -> serde_json::Value {
    serde_json::json!({
        "wire": WIRE,
        "launcher": LAUNCHER,
        "agents": AGENTS,
        "attach": ATTACH,
        "sources": SOURCES,
        "helper": HELPER,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_doc_pins_every_number() {
        let doc = include_str!("../../../docs/remote-contracts.md");
        let rows = [
            format!("| A | launcher | {LAUNCHER} | `{LAUNCHER_ENV}` |"),
            format!("| B | agents | {AGENTS} | - |"),
            format!("| C | attach | {ATTACH} | - |"),
            format!("| D | sources | {SOURCES} | `{SOURCES_ENV}` |"),
            format!("| E | helper | {HELPER} | `{HELPER_ENV}` |"),
            format!("| - | wire | {WIRE} | - |"),
        ];
        for row in rows {
            assert!(doc.contains(&row), "docs/remote-contracts.md lacks the row `{row}`");
        }
    }

    #[test]
    fn the_json_line_is_stable() {
        assert_eq!(json().to_string(), r#"{"wire":7,"launcher":2,"agents":1,"attach":1,"sources":2,"helper":1}"#);
    }
}
