//! `/memory`: the memory plugin's files, log, commits and reverts (table row "/memory"). Editing a
//! memory file goes through `read_file` and `write_file` (see `plans`).

use super::local::{Local, Registry};
use super::{Answer, pending};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::rc::Rc;
use std::time::Duration;

pub const PLUGIN_REQUEST: &str = "plugin_request";

/// The only plugin the UI may ask by name.
const PLUGIN: &str = "memory";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginRequest {
    pub plugin: String,
    pub method: String,
    pub params: Value,
    pub timeout_secs: u64,
}

pub(super) fn register(r: &mut Registry) {
    r.add(PLUGIN_REQUEST, plugin_request);
}

fn plugin_request(l: &Rc<Local>, args: Value) -> Answer {
    let l = l.clone();
    pending(args, move |a: PluginRequest| async move {
        if a.plugin != PLUGIN {
            return Err(format!("the host answers requests for the {PLUGIN} plugin only, not {}", a.plugin));
        }
        let reply = l.plugins.request(&a.plugin, &a.method, a.params, Duration::from_secs(a.timeout_secs)).await?;
        if a.method == "memory/files" {
            for key in ["files", "notes"] {
                for f in reply[key].as_array().into_iter().flatten() {
                    if let Some(path) = f["path"].as_str() {
                        super::plans::hand_out(std::path::Path::new(path));
                    }
                }
            }
        }
        Ok(reply)
    })
}
