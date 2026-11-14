//! Hosts of URLs: what `fetch` is asked per, and which a search result has already named.

/// The lowercase host of an http(s) URL, without userinfo, port or brackets.
pub(super) fn url_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']')?.0,
        None => host.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The hosts a `web_search` result lists: the lines that are one URL, as the tool prints them.
pub(super) fn result_hosts(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| (l.starts_with("http://") || l.starts_with("https://")) && !l.contains(char::is_whitespace))
        .filter_map(url_host)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_what_the_connection_goes_to() {
        assert_eq!(url_host("https://Example.com/a?b").as_deref(), Some("example.com"));
        assert_eq!(url_host("http://example.com:8080").as_deref(), Some("example.com"));
        assert_eq!(url_host("https://good.com@evil.com/x").as_deref(), Some("evil.com"), "userinfo is not the host");
        assert_eq!(url_host("https://good.com#@evil.com/").as_deref(), Some("good.com"));
        assert_eq!(url_host("http://[::1]:80/").as_deref(), Some("::1"));
        assert_eq!(url_host("example.com"), None);
        assert_eq!(url_host("https:///x"), None);
    }

    #[test]
    fn only_the_url_lines_of_a_result_name_hosts() {
        let text = "Note: x\n[1] Title\n    https://a.example/p\n    a snippet about https://b.example/\n[2] T\n    http://c.example/";
        assert_eq!(result_hosts(text), ["a.example", "c.example"]);
    }
}
