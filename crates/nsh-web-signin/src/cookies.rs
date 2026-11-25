//! Pure cross-platform RFC 6265 cookie parsing, formatting, domain/path
//! matching, and URL completion pattern helpers.

use std::collections::HashMap;

use url::Url;

/// Parses a `Cookie` header string (or newline-separated `k=v` pairs) into a
/// map of non-empty cookie names to non-empty trimmed values.
pub fn parse_cookie_header(raw: &str) -> HashMap<String, String> {
    let trimmed = raw.trim();
    let body = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("cookie:") {
        &trimmed[7..]
    } else {
        trimmed
    };

    let mut map = HashMap::new();
    for part in body.split([';', '\r', '\n']) {
        let entry = part.trim();
        if let Some((k, v)) = entry.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if !key.is_empty() && !val.is_empty() {
                map.insert(key.to_owned(), val.to_owned());
            }
        }
    }
    map
}

/// Formats `cookies` into a deterministic sorted `k=v; k2=v2` Cookie header string.
pub fn format_cookie_header(cookies: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&str, &str)> = cookies
        .iter()
        .filter_map(|(k, v)| {
            let kt = k.trim();
            let vt = v.trim();
            (!kt.is_empty() && !vt.is_empty()).then_some((kt, vt))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));

    let mut out = String::new();
    for (idx, (k, v)) in pairs.into_iter().enumerate() {
        if idx > 0 {
            out.push_str("; ");
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// Returns `true` when `until` is non-empty and every named cookie exists in
/// `cookies` with a non-empty trimmed value.
pub fn satisfies_until_cookies(cookies: &HashMap<String, String>, until: &[String]) -> bool {
    let required: Vec<&str> = until
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    !required.is_empty()
        && required
            .iter()
            .all(|name| cookies.get(*name).is_some_and(|v| !v.trim().is_empty()))
}

/// RFC 6265 domain, path, and `Secure` scheme matching against `target_url`.
pub fn cookie_matches_target(
    cookie_domain: &str,
    cookie_path: &str,
    is_secure: bool,
    target_url: &str,
) -> bool {
    let Some(parsed) = parse_url_lenient(target_url) else {
        return false;
    };
    if is_secure && !parsed.scheme().eq_ignore_ascii_case("https") {
        return false;
    }
    let Some(target_host_raw) = parsed.host_str() else {
        return false;
    };
    let target_host = normalize_host(target_host_raw);
    if target_host.is_empty() {
        return false;
    }

    let raw_dom = cookie_domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if raw_dom.is_empty() {
        return false;
    }

    let domain_ok = if let Some(suffix) = raw_dom.strip_prefix('.') {
        !suffix.is_empty() && host_matches_domain_suffix(&target_host, suffix)
    } else {
        target_host == raw_dom
    };
    if !domain_ok {
        return false;
    }

    let cpath = {
        let p = cookie_path.trim();
        if p.is_empty() { "/" } else { p }
    };
    let tpath = {
        let p = parsed.path();
        if p.is_empty() { "/" } else { p }
    };

    path_matches(tpath, cpath)
}

/// Returns `true` when `cookies_of` is `None`/empty, or when `page_url` matches
/// or shares the same registrable domain suffix as `cookies_of` (for example,
/// `music.youtube.com` and `www.youtube.com` match `https://music.youtube.com`,
/// while `accounts.google.com` does not).
pub fn url_matches_cookies_of(page_url: &str, cookies_of: Option<&str>) -> bool {
    let Some(target_raw) = cookies_of.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let Some(page_parsed) = parse_url_lenient(page_url) else {
        return false;
    };
    let Some(target_parsed) = parse_url_lenient(target_raw) else {
        return false;
    };
    let (Some(page_host_raw), Some(target_host_raw)) =
        (page_parsed.host_str(), target_parsed.host_str())
    else {
        return false;
    };
    let page_host = normalize_host(page_host_raw);
    let target_host = normalize_host(target_host_raw);
    if page_host.is_empty() || target_host.is_empty() {
        return false;
    }
    if page_host == target_host
        || host_matches_domain_suffix(&page_host, &target_host)
        || host_matches_domain_suffix(&target_host, &page_host)
    {
        return true;
    }
    registrable_domain(&page_host) == registrable_domain(&target_host)
}

/// Matches a navigated `url` against a completion `pattern` (supporting `*`
/// wildcards or case-insensitive substring/prefix matches).
pub fn url_matches_pattern(url: &str, pattern: &str) -> bool {
    let url_t = url.trim();
    let pat_t = pattern.trim();
    if url_t.is_empty() || pat_t.is_empty() {
        return false;
    }
    let url_lc = url_t.to_ascii_lowercase();
    let pat_lc = pat_t.to_ascii_lowercase();
    if pat_lc.contains('*') {
        glob_match(&pat_lc, &url_lc)
    } else {
        url_lc.contains(&pat_lc)
    }
}

/// Returns `(stripped_domain_len, host_only, path_len)` for RFC 6265 duplicate
/// cookie tie-breaking (more specific domain/path wins).
#[cfg_attr(
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos"))
    )),
    allow(dead_code)
)]
pub(crate) fn cookie_specificity(domain: &str, path: &str) -> (usize, bool, usize) {
    let trimmed_dom = domain.trim();
    let host_only = !trimmed_dom.is_empty() && !trimmed_dom.starts_with('.');
    let norm_dom_len = trimmed_dom.trim_matches('.').len();
    let path_len = path.trim().len();
    (norm_dom_len, host_only, path_len)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_idx, mut match_idx) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(s) = star_idx {
            pi = s + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn parse_url_lenient(raw: &str) -> Option<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Url::parse(trimmed).ok().or_else(|| {
        if !trimmed.contains("://") {
            Url::parse(&format!("https://{trimmed}")).ok()
        } else {
            None
        }
    })
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn host_matches_domain_suffix(host: &str, suffix: &str) -> bool {
    host == suffix
        || (host.len() > suffix.len()
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.')
}

fn path_matches(target_path: &str, cookie_path: &str) -> bool {
    if cookie_path == "/" || target_path == cookie_path {
        return true;
    }
    target_path.starts_with(cookie_path)
        && (cookie_path.ends_with('/')
            || target_path.as_bytes().get(cookie_path.len()) == Some(&b'/'))
}

fn registrable_domain(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    if labels.len() <= 2 {
        return host;
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let multi_part_tld = matches!(
        last_two.as_str(),
        "co.uk"
            | "org.uk"
            | "ac.uk"
            | "gov.uk"
            | "com.cn"
            | "net.cn"
            | "org.cn"
            | "gov.cn"
            | "co.jp"
            | "ne.jp"
            | "or.jp"
            | "com.au"
            | "com.hk"
            | "com.tw"
            | "com.sg"
            | "com.br"
            | "co.kr"
    );
    let keep = if multi_part_tld && labels.len() >= 3 {
        3
    } else {
        2
    };
    let drop_labels = labels.len() - keep;
    let mut byte_offset = 0usize;
    for label in labels.iter().take(drop_labels) {
        byte_offset += label.len() + 1;
    }
    host.get(byte_offset..).unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_cookie_header_is_deterministic() {
        let map = parse_cookie_header(
            "Cookie: __Secure-3PSID=sid==; SAPISID=abc ; SESSDATA=xyz; EMPTY=; =bad",
        );
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("SAPISID").map(String::as_str), Some("abc"));
        assert_eq!(map.get("__Secure-3PSID").map(String::as_str), Some("sid=="));
        assert_eq!(map.get("SESSDATA").map(String::as_str), Some("xyz"));
        assert_eq!(
            format_cookie_header(&map),
            "SAPISID=abc; SESSDATA=xyz; __Secure-3PSID=sid=="
        );
    }

    #[test]
    fn satisfies_until_cookies_ytm_and_bilibili() {
        let ytm = parse_cookie_header("SAPISID=sap1; __Secure-3PSID=psid2");
        assert!(satisfies_until_cookies(
            &ytm,
            &["SAPISID".into(), "__Secure-3PSID".into()]
        ));
        assert!(!satisfies_until_cookies(&ytm, &[]));
        assert!(!satisfies_until_cookies(
            &ytm,
            &["SAPISID".into(), "MISSING".into()]
        ));

        let bili = parse_cookie_header("SESSDATA=s1; bili_jct=j2");
        assert!(satisfies_until_cookies(
            &bili,
            &["SESSDATA".into(), "bili_jct".into()]
        ));
    }

    #[test]
    fn cookie_matches_target_youtube_music_and_google_isolation() {
        assert!(cookie_matches_target(
            ".youtube.com",
            "/",
            true,
            "https://music.youtube.com/"
        ));
        assert!(cookie_matches_target(
            "music.youtube.com",
            "/",
            true,
            "https://music.youtube.com/library"
        ));
        assert!(!cookie_matches_target(
            ".google.com",
            "/",
            true,
            "https://music.youtube.com/"
        ));
        assert!(!cookie_matches_target(
            "www.youtube.com",
            "/",
            true,
            "https://music.youtube.com/"
        ));
        assert!(!cookie_matches_target(
            ".youtube.com",
            "/",
            true,
            "http://music.youtube.com/"
        ));
    }

    #[test]
    fn cookie_matches_target_bilibili_and_path_boundaries() {
        assert!(cookie_matches_target(
            ".bilibili.com",
            "/",
            true,
            "https://api.bilibili.com/x/web-interface/nav"
        ));
        assert!(cookie_matches_target(
            "api.bilibili.com",
            "/x/web-interface",
            true,
            "https://api.bilibili.com/x/web-interface/nav"
        ));
        assert!(!cookie_matches_target(
            "api.bilibili.com",
            "/x/web",
            true,
            "https://api.bilibili.com/x/web-interface/nav"
        ));
        assert!(cookie_matches_target(
            ".bilibili.com",
            "/",
            false,
            "http://api.bilibili.com/"
        ));
        assert!(!cookie_matches_target(
            ".evil-bilibili.com",
            "/",
            false,
            "https://api.bilibili.com/"
        ));
    }

    #[test]
    fn url_matches_cookies_of_and_patterns() {
        assert!(url_matches_cookies_of("https://accounts.google.com/", None));
        assert!(url_matches_cookies_of(
            "https://music.youtube.com/",
            Some("https://music.youtube.com")
        ));
        assert!(url_matches_cookies_of(
            "https://www.youtube.com/watch?v=1",
            Some("https://music.youtube.com")
        ));
        assert!(!url_matches_cookies_of(
            "https://accounts.google.com/signin",
            Some("https://music.youtube.com")
        ));
        assert!(url_matches_cookies_of(
            "https://passport.bilibili.com/login",
            Some("https://api.bilibili.com")
        ));
        assert!(!url_matches_cookies_of(
            "https://evil-youtube.com/",
            Some("https://music.youtube.com")
        ));

        assert!(url_matches_pattern(
            "https://music.youtube.com/library",
            "https://music.youtube.com/*"
        ));
        assert!(url_matches_pattern(
            "https://music.youtube.com/library",
            "music.youtube.com"
        ));
        assert!(!url_matches_pattern(
            "https://accounts.google.com/ServiceLogin",
            "https://music.youtube.com/*"
        ));
        assert!(!url_matches_pattern("https://music.youtube.com/", ""));
    }
}
