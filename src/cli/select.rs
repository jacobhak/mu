//! Which model and thinking level a new agent gets.

use anyhow::Result;
use mu_base::{config, models};
use mu_common::paths;
use mu_provider::provider::client::Providers;
use mu_provider::provider::providers::PROVIDERS;
use mu_tools::context;

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

/// Model and thinking level for a new agent: `sel` (command line or headless role) beats the `default` role.
pub(crate) fn pick_model(
    cfgs: &config::Configs,
    providers: &Providers,
    sel: Option<String>,
    cli_thinking: Option<models::Thinking>,
) -> Result<(models::ModelSpec, models::Thinking)> {
    let (cfg_model, cfg_level) = cfgs.role("default").map(|v| models::split_level(&v)).unzip();
    let sel = sel.or(cfg_model).unwrap_or_else(|| default_model(providers));
    let (spec, role_level) = providers.resolve(&sel)?;
    Ok((spec, cli_thinking.or(role_level).or(cfg_level.flatten()).unwrap_or(models::Thinking::Medium)))
}

/// The model used when nothing names one: omp's default while omp can run, else the first native
/// provider with a key, else omp's default (which then says what is missing).
fn default_model(providers: &Providers) -> String {
    default_model_for(omp_installed(), |p| providers.is_direct(p))
}

/// `default_model` with the two facts it depends on passed in.
fn default_model_for(omp: bool, native: impl Fn(&str) -> bool) -> String {
    if omp {
        return DEFAULT_MODEL.into();
    }
    PROVIDERS
        .iter()
        .find(|d| native(d.id))
        .map_or_else(|| DEFAULT_MODEL.into(), |d| format!("{}/{}", d.id, d.default_model))
}

/// Whether omp can run: `$MU_OMP`, else mu's own copy, else `omp` on `$PATH`.
fn omp_installed() -> bool {
    let bin = paths::omp_bin();
    if bin.components().count() > 1 {
        return bin.is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&bin).is_file()))
}

/// First `provider/model[:level]` or configured `@role` in the agent's model list, else the role chain of
/// `scout` for an agent of that name and of `task` for any other.
pub(crate) fn agent_selector(def: &context::AgentDef, cfgs: &config::Configs) -> Option<String> {
    def.model
        .iter()
        .find(|m| match m.strip_prefix('@') {
            Some(role) => cfgs.role(models::split_level(role).0.as_str()).is_some(),
            None => m.contains('/'),
        })
        .cloned()
        .or_else(|| models::role_selector(if def.name == "scout" { "scout" } else { "task" }, cfgs))
}

/// `role · provider/model` when the model came from a `@role` selector, else just the model.
pub(crate) fn model_label(sel: Option<&str>, model: &str) -> String {
    match sel.and_then(|s| s.strip_prefix('@')) {
        Some(role) => format!("{} · {model}", models::split_level(role).0),
        None => model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_omps_while_omp_can_run() {
        assert_eq!(default_model_for(true, |_| false), DEFAULT_MODEL);
        assert_eq!(default_model_for(true, |_| true), DEFAULT_MODEL);
    }

    #[test]
    fn without_omp_the_first_native_key_provides_the_default() {
        let first = &PROVIDERS[0];
        assert_eq!(default_model_for(false, |p| p == first.id), format!("{}/{}", first.id, first.default_model));
        assert_eq!(default_model_for(false, |_| false), DEFAULT_MODEL, "no key: omp's default says what is missing");
    }

    #[test]
    fn model_label_names_the_role_only_for_a_role_selector() {
        assert_eq!(model_label(Some("@smol:high"), "a/b"), "smol · a/b");
        assert_eq!(model_label(Some("a/b:low"), "a/b"), "a/b");
        assert_eq!(model_label(None, "a/b"), "a/b");
    }

    #[test]
    fn an_agent_with_no_usable_model_takes_the_role_of_its_kind() {
        let mut cfgs = config::Configs::default();
        for (role, model) in [("default", "a/d"), ("task", "a/t"), ("scout", "a/s")] {
            cfgs.omp.model_roles.insert(role.into(), model.into());
        }
        let def = |name: &str, model: &[&str]| context::AgentDef {
            name: name.into(),
            description: String::new(),
            tools: None,
            model: model.iter().map(|m| m.to_string()).collect(),
            thinking: None,
            prompt: String::new(),
            output: None,
        };
        assert_eq!(agent_selector(&def("scout", &[]), &cfgs).as_deref(), Some("@scout"));
        assert_eq!(agent_selector(&def("docs", &["@nope"]), &cfgs).as_deref(), Some("@task"));
        assert_eq!(agent_selector(&def("docs", &["@scout"]), &cfgs).as_deref(), Some("@scout"));
    }
}
