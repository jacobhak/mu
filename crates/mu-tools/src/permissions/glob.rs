//! The one pattern matcher permission rules use.

/// `*` matches any run of characters (including `/` and spaces), `?` exactly one, everything
/// else is literal, and the match is anchored at both ends. A pattern ending in ` *` also
/// matches the text before it, so `cargo test *` matches `cargo test`.
pub fn glob_match(pattern: &str, subject: &str) -> bool {
    if let Some(head) = pattern.strip_suffix(" *")
        && subject == head
    {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = subject.chars().collect();
    let (mut pi, mut si) = (0, 0);
    // Where the last `*` was, and where its match had got to.
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        match p.get(pi) {
            Some('?') => {
                pi += 1;
                si += 1;
            }
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
            }
            Some(c) if *c == s[si] => {
                pi += 1;
                si += 1;
            }
            _ => match star {
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_cross_slashes_and_spaces() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "a/b c"));
        assert!(glob_match("*.env", "a/b/.env"));
        assert!(glob_match("mcp__github_*", "mcp__github_create_issue"));
        assert!(!glob_match("mcp__github_*", "mcp__gitlab_create"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn trailing_space_star_also_matches_its_head() {
        assert!(glob_match("cargo test *", "cargo test"));
        assert!(glob_match("cargo test *", "cargo test --release"));
        assert!(!glob_match("cargo test *", "cargo testx"));
        assert!(!glob_match("cargo test *", "cargo testing"));
    }

    #[test]
    fn question_is_one_character_and_matches_are_anchored() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("a?c", "aéc"), "one character, not one byte");
        assert!(!glob_match("a?c", "aécx"));
        assert!(!glob_match("bc", "abc"));
        assert!(!glob_match("ab", "abc"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }
}
