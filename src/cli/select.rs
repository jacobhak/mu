//! Which model and thinking level a new agent gets.

use anyhow::{Result, anyhow};
use mu_base::{config, models};
use mu_provider::provider::client::Providers;
use mu_tools::context;

const NO_MODEL: &str =
    "no model chosen: pass --model provider/model, or set the `default` role (with /model, or in models.toml)";

/// The model and level a new agent asked for: `sel` (command line or headless role) beats the
/// `default` role. `None` when neither names one: mu never picks a model itself.
fn chosen(cfgs: &config::Configs, sel: Option<String>) -> (Option<String>, Option<models::Thinking>) {
    let (cfg_model, cfg_level) = cfgs.role("default").map(|v| models::split_level(&v)).unzip();
    (sel.or(cfg_model), cfg_level.flatten())
}

fn resolve_chosen(
    providers: &Providers,
    sel: &str,
    level: Option<models::Thinking>,
    cli_thinking: Option<models::Thinking>,
) -> Result<(models::ModelSpec, models::Thinking)> {
    let (spec, role_level) = providers.resolve(sel)?;
    Ok((spec, cli_thinking.or(role_level).or(level).unwrap_or(models::Thinking::Medium)))
}

/// Model and thinking level for a run that cannot ask: an error when none is named.
pub(crate) fn pick_model(
    cfgs: &config::Configs,
    providers: &Providers,
    sel: Option<String>,
    cli_thinking: Option<models::Thinking>,
) -> Result<(models::ModelSpec, models::Thinking)> {
    let (sel, level) = chosen(cfgs, sel);
    let sel = sel.ok_or_else(|| {
        let hint =
            if cfgs.import_offer.borrow().is_some() { "; copy omp's model config with `mu models import`" } else { "" };
        anyhow!("{NO_MODEL}{hint}")
    })?;
    resolve_chosen(providers, &sel, level, cli_thinking)
}

/// `pick_model` for the TUI: with no model named it starts unselected, so the user can `/login`
/// and `/model` first.
pub(crate) fn pick_model_or_none(
    cfgs: &config::Configs,
    providers: &Providers,
    sel: Option<String>,
    cli_thinking: Option<models::Thinking>,
) -> Result<(models::ModelSpec, models::Thinking)> {
    match chosen(cfgs, sel) {
        (Some(sel), level) => resolve_chosen(providers, &sel, level, cli_thinking),
        (None, level) => {
            Ok((models::ModelSpec::unselected(), cli_thinking.or(level).unwrap_or(models::Thinking::Medium)))
        }
    }
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
    fn a_model_comes_from_the_selector_or_the_default_role_and_never_from_credentials() {
        let cfgs = config::Configs::default();
        assert_eq!(chosen(&cfgs, None), (None, None));
        assert_eq!(chosen(&cfgs, Some("a/b".into())), (Some("a/b".into()), None));
        let cfgs = config::Configs::from_models_toml("[sets.main]\ndefault = \"a/d:high\"\n");
        assert_eq!(chosen(&cfgs, None), (Some("a/d".into()), Some(models::Thinking::High)));
        assert_eq!(chosen(&cfgs, Some("a/b".into())).0.as_deref(), Some("a/b"));
    }

    #[test]
    fn no_model_is_an_error_for_a_run_that_cannot_ask_and_unselected_in_the_tui() {
        let cfgs = config::Configs::default();
        let providers = Providers::for_tests(
            std::rc::Rc::new(config::Configs::default()),
            mu_provider::provider::endpoint::GatewaySource::new(mu_provider::provider::endpoint::start_fn(|| async {
                Err::<(String, String), String>("none".into())
            })),
        );
        let e = pick_model(&cfgs, &providers, None, None).unwrap_err().to_string();
        assert!(e.contains("--model") && e.contains("default"), "{e}");
        let (spec, level) = pick_model_or_none(&cfgs, &providers, None, None).unwrap();
        assert!(spec.is_unselected());
        assert_eq!(level, models::Thinking::Medium);
    }

    #[test]
    fn model_label_names_the_role_only_for_a_role_selector() {
        assert_eq!(model_label(Some("@smol:high"), "a/b"), "smol · a/b");
        assert_eq!(model_label(Some("a/b:low"), "a/b"), "a/b");
        assert_eq!(model_label(None, "a/b"), "a/b");
    }

    #[test]
    fn an_agent_with_no_usable_model_takes_the_role_of_its_kind() {
        let cfgs =
            config::Configs::from_models_toml("[sets.main]\ndefault = \"a/d\"\ntask = \"a/t\"\nscout = \"a/s\"\n");
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
