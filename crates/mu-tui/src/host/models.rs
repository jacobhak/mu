//! Models: `/switch`, the `/model` role editor, model sets and plan tiers (table row "/switch, /model
//! editor, model sets").

use super::local::{Local, Registry};
use super::startup::ImportOffer;
use super::{Answer, pending, ready};
use crate::panels::doc_review::Tier;
use mu_base::config::{self, Configs, models_path};
use mu_base::model_config::{MAIN_SET, RoleMeta};
use mu_base::roles::{self, RoleView};
use mu_provider::pi_import;
use mu_provider::provider::custom::KeyState;
use mu_tools::context::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::rc::Rc;

pub const MODEL_ROWS: &str = "model_rows";
pub const ROLE_TABLE: &str = "role_table";
pub const PLAN_TIERS: &str = "plan_tiers";
pub const REFRESH_SOURCES: &str = "refresh_sources";
pub const REFRESH_SOURCE: &str = "refresh_source";
pub const EDIT_ROLE: &str = "edit_role";
pub const EDIT_CUSTOM_ROLE: &str = "edit_custom_role";
pub const SET_TABLE: &str = "set_table";
pub const SWITCH_SET: &str = "switch_set";
pub const NEW_SET: &str = "new_set";
pub const DELETE_SET: &str = "delete_set";
pub const RENAME_SET: &str = "rename_set";
pub const IMPORT_MODELS: &str = "import_models";
pub const IMPORT_OFFERS: &str = "import_offers";

/// Where a listed model comes from, with what the picker says beside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelKind {
    /// A direct or custom provider's model; `key` is a custom endpoint's credential state.
    Direct { provider: String, key: Option<KeyState> },
    /// The gateway's; `native` names the provider when its models run on the user's own key or plan.
    Gateway { native: Option<String>, context_length: Option<u32> },
    /// Only named in `models.toml`.
    Listed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRow {
    pub id: String,
    pub kind: ModelKind,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelRows {
    pub roles: Vec<RoleView>,
    /// Direct and custom models, then the gateway's, then `models.toml`'s, each id once.
    pub rows: Vec<ModelRow>,
    /// `providers.toml` did not read: the rows that do not depend on it are still listed.
    pub error: Option<String>,
}

/// What the role editor opens on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoleTable {
    pub set: String,
    pub path: PathBuf,
    pub roles: Vec<RoleView>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditRole {
    pub set: String,
    pub role: String,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditCustomRole {
    pub set: String,
    pub role: String,
    pub model: Option<String>,
    pub meta: RoleMeta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetRow {
    pub name: String,
    pub default: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SetTable {
    pub sets: Vec<SetRow>,
    /// omp's model config can still be copied.
    pub import_offer: bool,
}

/// What the new active set assigns, for the session's next model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Switched {
    /// The active mode's first model selector the new set can serve.
    pub mode_model: Option<String>,
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Imported {
    pub lines: Vec<String>,
    pub path: PathBuf,
    pub has_default: bool,
    /// Parts of a pi import that could not be written.
    #[serde(default)]
    pub failed: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rename {
    pub old: String,
    pub new: String,
}

pub(crate) fn role_views(cfg: &Configs, ctx: &Context) -> Vec<RoleView> {
    let agents: Vec<(&str, &[String])> = ctx.agents.iter().map(|a| (a.name.as_str(), a.model.as_slice())).collect();
    roles::views(cfg, &agents)
}

pub(super) fn register(r: &mut Registry) {
    r.add(MODEL_ROWS, model_rows);
    r.add(ROLE_TABLE, role_table);
    r.add(PLAN_TIERS, plan_tiers);
    r.add(REFRESH_SOURCES, refresh_sources);
    r.add(REFRESH_SOURCE, refresh_source);
    r.add(EDIT_ROLE, edit_role);
    r.add(EDIT_CUSTOM_ROLE, edit_custom_role);
    r.add(SET_TABLE, set_table);
    r.add(SWITCH_SET, switch_set);
    r.add(NEW_SET, new_set);
    r.add(DELETE_SET, delete_set);
    r.add(RENAME_SET, rename_set);
    r.add(IMPORT_MODELS, import_models);
    r.add(IMPORT_OFFERS, import_offers);
}

/// `()`: the roles and every model the pickers list, from what is cached now (never the network).
fn model_rows(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let providers = &l.providers;
        let (direct, error) = match providers.direct_models() {
            Ok(d) => (d, None),
            Err(e) => (Vec::new(), Some(format!("{e:#}"))),
        };
        let mut rows: Vec<ModelRow> = Vec::new();
        let listed = |rows: &[ModelRow], id: &str| rows.iter().any(|r| r.id == id);
        for m in direct {
            if !listed(&rows, &m.id) {
                rows.push(ModelRow { id: m.id, kind: ModelKind::Direct { provider: m.provider, key: m.key } });
            }
        }
        for g in providers.cached_models() {
            if listed(&rows, &g.id) {
                continue;
            }
            let native = g.id.split_once('/').map(|(p, _)| p).filter(|p| providers.is_direct(p)).map(String::from);
            rows.push(ModelRow { id: g.id, kind: ModelKind::Gateway { native, context_length: g.context_length } });
        }
        for id in providers.cfg.listed_models() {
            if !listed(&rows, &id) {
                rows.push(ModelRow { id, kind: ModelKind::Listed });
            }
        }
        Ok(ModelRows { roles: role_views(&providers.cfg, &l.ctx()), rows, error })
    })
}

fn role_table(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let cfg = &l.providers.cfg;
        Ok(RoleTable {
            set: cfg.active_set().unwrap_or_else(|| MAIN_SET.into()),
            path: models_path(),
            roles: role_views(cfg, &l.ctx()),
        })
    })
}

/// `()`: the roles `cycle_order` offers when a plan is approved, those that resolve to a model.
fn plan_tiers(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let providers = &l.providers;
        let cfg = &providers.cfg;
        let mut out = Vec::new();
        for r in cfg.role_cycle() {
            match cfg.role(&r) {
                Some(v) if providers.resolve(&format!("@{r}")).is_ok() => {
                    out.push(Tier { model: Some(format!("@{r}")), detail: v, role: r });
                }
                None if r == "default" => out.push(Tier { role: r, detail: "current model".into(), model: None }),
                _ => {}
            }
        }
        Ok(out)
    })
}

/// `()`: what a refresh asks, one `REFRESH_SOURCE` call each: the gateway, then every custom
/// endpoint that discovers. A `providers.toml` that does not read is shown by the picker; there is
/// nothing to fetch from it.
fn refresh_sources(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let mut out = vec!["gateway".to_string()];
        if let Ok(custom) = l.providers.custom_registry().snapshot() {
            out.extend(custom.into_iter().filter(|(_, c)| c.discover).map(|(id, _)| format!("custom:{id}")));
        }
        Ok(out)
    })
}

/// One source's name from `REFRESH_SOURCES`: what failed, if anything. A failure keeps the rows
/// already listed.
fn refresh_source(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |source: String| {
        let providers = l.providers.clone();
        async move {
            let Some(id) = source.strip_prefix("custom:") else {
                return Ok(providers
                    .gateway_models()
                    .await
                    .err()
                    .map(|e| format!("gateway models unavailable: {e:#}")));
            };
            let Some(c) = providers.custom_registry().snapshot().ok().and_then(|mut all| all.remove(id)) else {
                return Ok(None);
            };
            Ok(c.refresh_models(providers.store())
                .await
                .err()
                .map(|e| format!("{}: its models were not refreshed: {e:#}", c.label())))
        }
    })
}

fn edit_role(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: EditRole| l.providers.cfg.edit_role(&a.set, &a.role, a.model.as_deref()))
}

fn edit_custom_role(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: EditCustomRole| l.providers.cfg.edit_custom_role(&a.set, &a.role, a.model.as_deref(), &a.meta))
}

fn set_table(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let cfg = &l.providers.cfg;
        let active = cfg.active_set();
        let models = cfg.models.borrow();
        let sets = cfg
            .set_names()
            .into_iter()
            .map(|name| SetRow {
                default: models.sets.get(&name).and_then(|r| r.get("default")).cloned(),
                active: active.as_deref() == Some(name.as_str()),
                name,
            })
            .collect();
        let import_offer = cfg.import_offer.borrow().is_some() || cfg.pi_import_offer.borrow().is_some();
        Ok(SetTable { sets, import_offer })
    })
}

/// `(name, mode)`: makes `name` the active set; `mode` is the session's active mode, if any.
fn switch_set(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(name, mode): (String, Option<String>)| {
        let cfg = &l.providers.cfg;
        cfg.switch_set(&name)?;
        let mode_model = mode.and_then(|m| {
            l.ctx()
                .mode(&m)?
                .model
                .iter()
                .find(|sel| match sel.strip_prefix('@') {
                    Some(role) => cfg.role(role).is_some(),
                    None => l.providers.resolve(sel).is_ok(),
                })
                .cloned()
        });
        Ok(Switched { mode_model, default: cfg.role("default") })
    })
}

/// `name`: the set the new one was copied from.
fn new_set(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |name: String| l.providers.cfg.new_set(&name))
}

fn delete_set(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |name: String| l.providers.cfg.delete_set(&name))
}

fn rename_set(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |r: Rename| l.providers.cfg.rename_set(&r.old, &r.new))
}

/// `()`: what can be copied now, from omp and from pi, with the lines that list it.
fn import_offers(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| Ok(offers(l)))
}

fn offers(l: &Rc<Local>) -> Vec<ImportOffer> {
    let offer = |source: &str, lines| ImportOffer { source: source.into(), lines, models_path: models_path() };
    let mut out: Vec<ImportOffer> = config::omp_import().map(|i| offer("omp", i.lines)).into_iter().collect();
    if let Some(plan) = config::pi_import() {
        let r = pi_import::resolve(&plan, l.providers.custom_registry(), l.providers.store(), false);
        if !r.is_empty() {
            out.push(offer("pi", r.lines));
        }
    }
    out
}

/// `source`: `omp` or `pi` (`None`: whichever is the only one) is copied: omp's into models.toml,
/// never over an existing file; pi's into mu's files, leaving anything mu already has.
fn import_models(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |source: Option<String>| {
        let cfg = &l.providers.cfg;
        let source = match source.as_deref() {
            Some(s @ ("omp" | "pi")) => s.to_string(),
            Some(other) => return Err(format!("unknown import source `{other}`: omp or pi")),
            None => match offers(l).as_slice() {
                [one] => one.source.clone(),
                [] => return Err("nothing to import: neither omp nor pi has setup mu can use".into()),
                _ => {
                    return Err(
                        "both omp and pi have setup to import: /modelset import omp, or /modelset import pi".into()
                    );
                }
            },
        };
        if source == "omp" {
            let lines = cfg.import_models(false)?;
            return Ok(Imported {
                lines,
                path: models_path(),
                has_default: cfg.role("default").is_some(),
                failed: vec![],
            });
        }
        let plan = config::pi_import().ok_or_else(config::nothing_to_import_from_pi)?;
        let (registry, store) = (l.providers.custom_registry(), l.providers.store());
        let resolved = pi_import::resolve(&plan, registry, store, false);
        if resolved.is_empty() {
            return Err(format!("nothing to import from pi:\n{}", resolved.lines.join("\n")));
        }
        let done = resolved.apply(cfg, registry, store, false);
        Ok(Imported {
            lines: resolved.lines,
            path: models_path(),
            has_default: cfg.role("default").is_some(),
            failed: done.failed,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round<T: Serialize + serde::de::DeserializeOwned>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).unwrap()).unwrap()
    }

    #[test]
    fn replies_survive_a_round_trip() {
        let rows = ModelRows {
            roles: vec![roles::unsaved("writer")],
            rows: vec![
                ModelRow {
                    id: "modal/x".into(),
                    kind: ModelKind::Direct { provider: "modal".into(), key: Some(KeyState::Missing) },
                },
                ModelRow {
                    id: "a/b".into(),
                    kind: ModelKind::Gateway { native: Some("a".into()), context_length: Some(8000) },
                },
                ModelRow { id: "c/d".into(), kind: ModelKind::Listed },
            ],
            error: Some("bad".into()),
        };
        let back = round(&rows);
        assert_eq!((back.roles, back.rows, back.error), (rows.roles, rows.rows, rows.error));

        let table = RoleTable { set: "main".into(), path: "/m.toml".into(), roles: vec![roles::unsaved("x")] };
        let back = round(&table);
        assert_eq!((back.set, back.path, back.roles), (table.set, table.path, table.roles));

        let edit = EditCustomRole {
            set: "main".into(),
            role: "w".into(),
            model: None,
            meta: RoleMeta { description: Some("d".into()), fallback: vec!["default".into()], delegate: true },
        };
        assert_eq!(round(&edit).meta, edit.meta);

        let set =
            SetTable { sets: vec![SetRow { name: "main".into(), default: None, active: true }], import_offer: true };
        let back = round(&set);
        assert_eq!((back.sets, back.import_offer), (set.sets, true));

        let sw = Switched { mode_model: Some("@plan".into()), default: Some("a/b".into()) };
        assert_eq!(round(&sw), sw);
        let imp = Imported { lines: vec!["l".into()], path: "/m".into(), has_default: true, failed: vec!["f".into()] };
        assert_eq!(round(&imp), imp);
    }
}
