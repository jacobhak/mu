use crate::paths;

/// `modelRoles.default` from the active omp profile's config.yml, as (model, thinking level).
pub fn default_model() -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(paths::omp_agent_dir().join("config.yml")).ok()?;
    parse_default(&text)
}

fn parse_default(text: &str) -> Option<(String, Option<String>)> {
    let mut in_roles = false;
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) && !line.trim().is_empty() {
            in_roles = line.trim_end() == "modelRoles:";
            continue;
        }
        if in_roles {
            if let Some(v) = line.trim().strip_prefix("default:") {
                let v = v.trim().trim_matches(['"', '\'']);
                if v.is_empty() {
                    return None;
                }
                return Some(split_level(v));
            }
        }
    }
    None
}

fn split_level(v: &str) -> (String, Option<String>) {
    if let Some((m, l)) = v.rsplit_once(':') {
        let level = match l {
            "off" | "low" | "medium" | "high" => Some(l),
            "minimal" => Some("low"),
            "xhigh" => Some("high"),
            "auto" | "inherit" => None,
            _ => return (v.into(), None),
        };
        return (m.into(), level.map(String::from));
    }
    (v.into(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_role_and_level_suffix() {
        let y = "modelRoles:\n  commit: a/b\n  default: baseten/zai-org/GLM-5.3-Flash:high\nother:\n  default: x\n";
        assert_eq!(parse_default(y), Some(("baseten/zai-org/GLM-5.3-Flash".into(), Some("high".into()))));
        assert_eq!(parse_default("modelRoles:\n  default: anthropic/x:auto\n"), Some(("anthropic/x".into(), None)));
        assert_eq!(parse_default("other:\n  default: x\n"), None);
    }
}
