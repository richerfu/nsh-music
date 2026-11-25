use std::collections::HashMap;

use nsh_music_core::source::RequestKind;
use serde::{Deserialize, Serialize};
use url::Url;

/// Resource category for `requestHeaders` matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HeaderRequestKind {
    Image,
    Audio,
    Api,
}

impl From<RequestKind> for HeaderRequestKind {
    fn from(kind: RequestKind) -> Self {
        match kind {
            RequestKind::Image => Self::Image,
            RequestKind::Audio => Self::Audio,
        }
    }
}

/// One `requestHeaders` matching rule declared in `plugin.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeaderRule {
    /// Single domain/URL glob pattern (e.g., `*.hdslb.com`, `https://api.example.com/*`).
    #[serde(
        rename = "match",
        alias = "domain",
        alias = "host",
        alias = "pattern",
        default
    )]
    pub match_pattern: String,
    /// Optional list of domain/URL globs.
    #[serde(default, alias = "domains")]
    pub hosts: Vec<String>,
    /// Resource kinds this rule applies to (empty = applies to `Image`, `Audio`, and `Api`).
    #[serde(default, alias = "types", alias = "kind")]
    pub kinds: Vec<HeaderRequestKind>,
    /// Headers to inject (values may contain `{{config.xxx}}` or `{{cookie}}` templates).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl HeaderRule {
    /// Returns all non-empty glob patterns configured on this rule.
    pub fn all_patterns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if !self.match_pattern.trim().is_empty() {
            out.push(self.match_pattern.trim());
        }
        for host in &self.hosts {
            let trimmed = host.trim();
            if !trimmed.is_empty() {
                out.push(trimmed);
            }
        }
        out
    }

    fn matches_kind(&self, kind: HeaderRequestKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }

    fn matches_url(&self, raw_url: &str, host: Option<&str>) -> bool {
        let patterns = self.all_patterns();
        if patterns.is_empty() {
            return true;
        }
        patterns
            .into_iter()
            .any(|pattern| pattern_matches_url(pattern, raw_url, host))
    }
}

/// Resolves plugin `requestHeaders` rules for a target URL and request kind.
#[derive(Debug, Clone, Default)]
pub struct RequestHeadersResolver {
    rules: Vec<HeaderRule>,
}

impl RequestHeadersResolver {
    /// Creates a resolver from parsed manifest rules.
    pub fn new(rules: Vec<HeaderRule>) -> Self {
        Self { rules }
    }

    /// Returns the underlying header rules.
    pub fn rules(&self) -> &[HeaderRule] {
        &self.rules
    }

    /// Resolves headers for `url` and `kind`, expanding `{{config.xxx}}` and `{{cookie}}` templates.
    pub fn resolve(
        &self,
        url: &str,
        kind: HeaderRequestKind,
        config: &HashMap<String, String>,
        cookie: Option<&str>,
    ) -> Vec<(String, String)> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let parsed = Url::parse(url).ok();
        let host = parsed.as_ref().and_then(|u| u.host_str());
        let origin = parsed
            .as_ref()
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_default();

        let effective_cookie = cookie
            .filter(|c| !c.is_empty())
            .or_else(|| config.get("cookie").map(String::as_str))
            .unwrap_or("");

        // Preserve insertion order while allowing later rules to override earlier headers case-insensitively.
        let mut ordered: Vec<(String, String)> = Vec::new();

        for rule in &self.rules {
            if !rule.matches_kind(kind) || !rule.matches_url(url, host) {
                continue;
            }
            let mut keys: Vec<&String> = rule.headers.keys().collect();
            keys.sort();
            for key in keys {
                let raw_val = &rule.headers[key];
                let expanded = expand_template(
                    raw_val,
                    config,
                    effective_cookie,
                    url,
                    host.unwrap_or(""),
                    &origin,
                );
                if raw_val.contains("{{") && expanded.trim().is_empty() {
                    continue;
                }
                if let Some(existing) = ordered
                    .iter_mut()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                {
                    existing.0 = key.clone();
                    existing.1 = expanded;
                } else {
                    ordered.push((key.clone(), expanded));
                }
            }
        }
        ordered
    }

    /// Convenience adapter for `nsh_music_core::source::RequestKind`.
    pub fn resolve_for_core(
        &self,
        url: &str,
        kind: RequestKind,
        config: &HashMap<String, String>,
        cookie: Option<&str>,
    ) -> Vec<(String, String)> {
        self.resolve(url, HeaderRequestKind::from(kind), config, cookie)
    }
}

fn pattern_matches_url(pattern: &str, raw_url: &str, host: Option<&str>) -> bool {
    let pat = pattern.trim();
    if pat == "*" || pat.is_empty() {
        return true;
    }
    let pat_lower = pat.to_ascii_lowercase();
    if pat_lower.contains("://") || pat_lower.contains('/') {
        return glob_match(&pat_lower, &raw_url.to_ascii_lowercase());
    }
    let Some(host_str) = host else {
        return glob_match(&pat_lower, &raw_url.to_ascii_lowercase());
    };
    let host_lower = host_str.to_ascii_lowercase();
    if let Some(suffix) = pat_lower.strip_prefix("*.") {
        // `*.example.com` matches subdomains (`a.example.com`) and apex (`example.com`).
        return host_lower == suffix || host_lower.ends_with(&format!(".{suffix}"));
    }
    if let Some(suffix) = pat_lower.strip_prefix('.') {
        return host_lower == suffix || host_lower.ends_with(&format!(".{suffix}"));
    }
    glob_match(&pat_lower, &host_lower)
}

/// Expands `{{config.xxx}}`, `{{setting.xxx}}`, `{{cookie}}`, `{{host}}`, `{{origin}}`, and `{{url}}`.
pub fn expand_template(
    template: &str,
    config: &HashMap<String, String>,
    cookie: &str,
    url: &str,
    host: &str,
    origin: &str,
) -> String {
    if !template.contains("{{") {
        return template.to_owned();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = after_open[..end].trim();
        let replacement = if expr == "cookie" {
            cookie
        } else if expr == "host" {
            host
        } else if expr == "origin" {
            origin
        } else if expr == "url" {
            url
        } else if let Some(key) = expr
            .strip_prefix("config.")
            .or_else(|| expr.strip_prefix("setting."))
            .or_else(|| expr.strip_prefix("settings."))
        {
            config.get(key).map(String::as_str).unwrap_or("")
        } else {
            config.get(expr).map(String::as_str).unwrap_or("")
        };
        out.push_str(replacement);
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Wildcard glob matcher supporting `*` (0+ chars) and `?` (single char).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_domains_kinds_and_expands_templates() {
        let resolver = RequestHeadersResolver::new(vec![
            HeaderRule {
                match_pattern: "*.hdslb.com".to_owned(),
                hosts: vec![],
                kinds: vec![HeaderRequestKind::Image],
                headers: HashMap::from([(
                    "Referer".to_owned(),
                    "https://www.bilibili.com/".to_owned(),
                )]),
            },
            HeaderRule {
                match_pattern: "*.bilivideo.com".to_owned(),
                hosts: vec![],
                kinds: vec![HeaderRequestKind::Audio],
                headers: HashMap::from([
                    ("Referer".to_owned(), "https://www.bilibili.com/".to_owned()),
                    ("Cookie".to_owned(), "{{cookie}}".to_owned()),
                    (
                        "X-Token".to_owned(),
                        "Bearer {{config.apiToken}}".to_owned(),
                    ),
                ]),
            },
        ]);

        let cfg = HashMap::from([("apiToken".to_owned(), "secret-123".to_owned())]);

        let img_headers = resolver.resolve(
            "https://i0.hdslb.com/bfs/archive/cover.jpg",
            HeaderRequestKind::Image,
            &cfg,
            None,
        );
        assert_eq!(
            img_headers,
            vec![("Referer".to_owned(), "https://www.bilibili.com/".to_owned())]
        );

        let wrong_kind = resolver.resolve(
            "https://i0.hdslb.com/bfs/archive/cover.jpg",
            HeaderRequestKind::Audio,
            &cfg,
            None,
        );
        assert!(wrong_kind.is_empty());

        let audio_headers = resolver.resolve(
            "https://upos-sz-mirrorcos.bilivideo.com/track.m4s",
            HeaderRequestKind::Audio,
            &cfg,
            Some("SESSDATA=abc"),
        );
        assert!(audio_headers.contains(&("Cookie".to_owned(), "SESSDATA=abc".to_owned())));
        assert!(audio_headers.contains(&("X-Token".to_owned(), "Bearer secret-123".to_owned())));

        // Empty cookie template omits the Cookie header instead of emitting empty value.
        let no_cookie_headers = resolver.resolve(
            "https://upos-sz-mirrorcos.bilivideo.com/track.m4s",
            HeaderRequestKind::Audio,
            &cfg,
            None,
        );
        assert!(!no_cookie_headers.iter().any(|(k, _)| k == "Cookie"));
    }

    #[test]
    fn glob_match_edge_cases() {
        assert!(glob_match("*.example.com", "a.b.example.com"));
        assert!(glob_match("api-?.music.com", "api-2.music.com"));
        assert!(!glob_match("api-?.music.com", "api-12.music.com"));
        assert!(glob_match(
            "https://cdn.*.net/audio/*",
            "https://cdn.jp.net/audio/123.flac"
        ));
    }
}
