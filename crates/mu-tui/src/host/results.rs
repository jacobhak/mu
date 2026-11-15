//! A launched worker's result, from the jobs panel (`d` shows the diff, `a` applies it with a
//! three-way merge). The git work is `mu_tools::tools::changes`; these calls only name the result by
//! the ref and base the job's snapshot carried, so a client cannot reach any other ref.

use super::local::{Local, Registry};
use super::{Answer, pending};
use mu_tools::tools::changes;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::rc::Rc;

pub const RESULT_DIFF: &str = "result_diff";
pub const RESULT_APPLY: &str = "result_apply";

/// The job's directory, and the ref and base of its result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultArgs {
    pub cwd: PathBuf,
    pub r#ref: String,
    pub base: String,
}

/// What `result_apply` did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub files: usize,
    pub conflicts: Vec<String>,
}

pub(super) fn register(r: &mut Registry) {
    r.add(RESULT_DIFF, result_diff);
    r.add(RESULT_APPLY, result_apply);
}

fn result_diff(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |a: ResultArgs| async move {
        let root = mu_base::git::repo_root(&a.cwd).await?;
        changes::diff(&root, &a.base, &a.r#ref).await
    })
}

fn result_apply(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |a: ResultArgs| async move {
        let root = mu_base::git::repo_root(&a.cwd).await?;
        let m = changes::apply_3way(&root, &a.base, &a.r#ref).await?;
        Ok(Applied { files: m.files, conflicts: m.conflicts })
    })
}
