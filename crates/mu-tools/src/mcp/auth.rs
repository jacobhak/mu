//! Resolves `env`/`headers` values (`!cmd`, env names, literals).

use mu_base::process::run_capture;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// omp's rule: `!cmd` runs in `dir` (10 s cap), else an env var of that name, else the literal.
/// An empty result is `None`, which drops the key.
pub async fn resolve_value(v: &str, dir: &Path) -> Option<String> {
    let out = if let Some(cmd) = v.strip_prefix('!') {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd).current_dir(dir);
        run_capture(c, Duration::from_secs(10)).await?
    } else if let Ok(e) = std::env::var(v) {
        e
    } else {
        v.to_string()
    };
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

pub async fn resolve_pairs(pairs: &[(String, String)], dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        if let Some(r) = resolve_value(v, dir).await {
            out.push((k.clone(), r));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::block_on as rt;

    #[test]
    fn values() {
        rt(async {
            let d = std::env::temp_dir();
            assert_eq!(resolve_value("!echo  x ", &d).await.as_deref(), Some("x"));
            assert_eq!(resolve_value("!true", &d).await, None, "empty drops the key");
            assert_eq!(resolve_value("!exit 3", &d).await, None);
            assert_eq!(resolve_value("literal-value", &d).await.as_deref(), Some("literal-value"));
            assert_eq!(resolve_value("PATH", &d).await, std::env::var("PATH").ok());
            assert_eq!(resolve_value("", &d).await, None);
        });
    }
}
