//! Complete upstream `yt-dlp/yt-dlp/master` InnerTube client registry (`INNERTUBE_CLIENTS`)
//! and multi-scheme YouTube cookie authorization (`SAPISIDHASH` / `SAPISID1PHASH` / `SAPISID3PHASH`).

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::virtual_ytdlp::{extract_cookie_value, sha1_hex};

pub const WEB_REMIX_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
pub const WEB_SAFARI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.5 Safari/605.1.15,gzip(gfe)";
pub const MWEB_UA: &str = "Mozilla/5.0 (iPad; CPU OS 16_7_10 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1,gzip(gfe)";
pub const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)";
pub const TV_SIMPLY_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version";
pub const TV_SAMSUNG_UA: &str = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";
pub const VISION_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
pub const ANDROID_UA: &str = "com.google.android.youtube/21.26.364 (Linux; U; Android 11) gzip";
pub const ANDROID_VR_UA: &str = "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
pub const IOS_UA: &str =
    "com.google.ios.youtube/21.26.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)";
pub const IOS_PRODUCER_UA: &str =
    "com.google.ios.youtubeproducer/0.147.945 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)";

/// Official upstream `yt-dlp` InnerTube client specification.
#[derive(Debug, Clone, Copy)]
pub struct InnerTubeClientSpec {
    /// Upstream `yt-dlp` client key (e.g. `"web_safari"`, `"web_music"`, `"visionos"`).
    pub key: &'static str,
    /// InnerTube `context.client.clientName`.
    pub client_name: &'static str,
    /// Numeric `X-YouTube-Client-Name` header ID.
    pub client_id: &'static str,
    /// `context.client.clientVersion` and `X-YouTube-Client-Version`.
    pub client_version: &'static str,
    /// HTTP `User-Agent` header.
    pub user_agent: &'static str,
    /// Default InnerTube API host (`"www.youtube.com"` or `"music.youtube.com"`).
    pub host: &'static str,
    /// Whether this client accepts browser `Cookie` + `SAPISIDHASH` headers.
    pub supports_cookies: bool,
    /// Whether this client typically returns direct non-ciphered URLs without player JS solving.
    pub jsless: bool,
    /// Optional `context.client.userAgent` field override.
    pub include_context_ua: bool,
    /// Optional `context.client.clientScreen` (e.g. `"EMBED"` for `web_embedded`).
    pub client_screen: Option<&'static str>,
    /// Optional top-level `thirdParty.embedUrl` (for `web_embedded`).
    pub embed_url: Option<&'static str>,
    pub device_make: Option<&'static str>,
    pub device_model: Option<&'static str>,
    pub os_name: Option<&'static str>,
    pub os_version: Option<&'static str>,
    pub android_sdk_version: Option<u32>,
}

impl InnerTubeClientSpec {
    /// Default HTTPS origin derived from [`Self::host`].
    pub fn default_origin(&self) -> String {
        format!("https://{}", self.host)
    }

    /// Builds the InnerTube `context` JSON object for this client with dynamic `visitor`, `hl`, and `gl`.
    pub fn build_context(&self, visitor: &str, hl: &str, gl: &str) -> Value {
        let mut client = json!({
            "clientName": self.client_name,
            "clientVersion": self.client_version,
            "hl": if hl.is_empty() { "en" } else { hl },
            "gl": if gl.is_empty() { "US" } else { gl },
        });
        if let Some(m) = client.as_object_mut() {
            if self.include_context_ua {
                m.insert("userAgent".to_owned(), json!(self.user_agent));
            }
            if let Some(screen) = self.client_screen {
                m.insert("clientScreen".to_owned(), json!(screen));
            }
            if let Some(make) = self.device_make {
                m.insert("deviceMake".to_owned(), json!(make));
            }
            if let Some(model) = self.device_model {
                m.insert("deviceModel".to_owned(), json!(model));
            }
            if let Some(os) = self.os_name {
                m.insert("osName".to_owned(), json!(os));
            }
            if let Some(os_ver) = self.os_version {
                m.insert("osVersion".to_owned(), json!(os_ver));
            }
            if let Some(sdk) = self.android_sdk_version {
                m.insert("androidSdkVersion".to_owned(), json!(sdk));
            }
            if !visitor.is_empty() {
                m.insert("visitorData".to_owned(), json!(visitor));
            }
        }
        let mut ctx = json!({ "client": client });
        if let Some(embed_url) = self.embed_url
            && let Some(m) = ctx.as_object_mut()
        {
            m.insert("thirdParty".to_owned(), json!({ "embedUrl": embed_url }));
        }
        ctx
    }
}

/// Complete registry of all 15 official `yt-dlp/yt-dlp/master` InnerTube clients.
pub const INNERTUBE_CLIENTS: &[InnerTubeClientSpec] = &[
    InnerTubeClientSpec {
        key: "web",
        client_name: "WEB",
        client_id: "1",
        client_version: "2.20260708.00.00",
        user_agent: WEB_REMIX_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "web_safari",
        client_name: "WEB",
        client_id: "1",
        client_version: "2.20260708.00.00",
        user_agent: WEB_SAFARI_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "web_embedded",
        client_name: "WEB_EMBEDDED_PLAYER",
        client_id: "56",
        client_version: "2.20260708.00.00",
        user_agent: WEB_SAFARI_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: Some("EMBED"),
        embed_url: Some("https://www.youtube.com/"),
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "web_music",
        client_name: "WEB_REMIX",
        client_id: "67",
        client_version: "1.20260707.12.00",
        user_agent: WEB_REMIX_UA,
        host: "music.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "web_creator",
        client_name: "WEB_CREATOR",
        client_id: "62",
        client_version: "1.20260708.06.00",
        user_agent: WEB_REMIX_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "mweb",
        client_name: "MWEB",
        client_id: "2",
        client_version: "2.20260708.05.00",
        user_agent: MWEB_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "tv",
        client_name: "TVHTML5",
        client_id: "7",
        client_version: "7.20260707.07.00",
        user_agent: TV_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "tv_downgraded",
        client_name: "TVHTML5",
        client_id: "7",
        client_version: "5.20260707",
        user_agent: TV_SIMPLY_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "tv_samsung",
        client_name: "TVHTML5",
        client_id: "7",
        client_version: "7.20260707",
        user_agent: TV_SAMSUNG_UA,
        host: "www.youtube.com",
        supports_cookies: true,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: Some("Samsung"),
        device_model: Some("UKS9800"),
        os_name: Some("Tizen"),
        os_version: Some("2.4.0"),
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "tv_simply",
        client_name: "TVHTML5_SIMPLY",
        client_id: "75",
        client_version: "1.0",
        user_agent: TV_SIMPLY_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: false,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: None,
        os_version: None,
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "visionos",
        client_name: "VISIONOS",
        client_id: "101",
        client_version: "1.02",
        user_agent: VISION_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: true,
        include_context_ua: true,
        client_screen: None,
        embed_url: None,
        device_make: Some("Apple"),
        device_model: Some("RealityDevice17,1"),
        os_name: Some("visionOS"),
        os_version: Some("26.5.23O471"),
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "android",
        client_name: "ANDROID",
        client_id: "3",
        client_version: "21.26.364",
        user_agent: ANDROID_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: true,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: None,
        device_model: None,
        os_name: Some("Android"),
        os_version: Some("11"),
        android_sdk_version: Some(30),
    },
    InnerTubeClientSpec {
        key: "android_vr",
        client_name: "ANDROID_VR",
        client_id: "28",
        client_version: "1.65.10",
        user_agent: ANDROID_VR_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: true,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: Some("Oculus"),
        device_model: Some("Quest 3"),
        os_name: Some("Android"),
        os_version: Some("12L"),
        android_sdk_version: Some(32),
    },
    InnerTubeClientSpec {
        key: "ios",
        client_name: "IOS",
        client_id: "5",
        client_version: "21.26.4",
        user_agent: IOS_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: true,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: Some("Apple"),
        device_model: Some("iPhone16,2"),
        os_name: Some("iPhone"),
        os_version: Some("18.3.2.22D82"),
        android_sdk_version: None,
    },
    InnerTubeClientSpec {
        key: "ios_producer",
        client_name: "IOS_PRODUCER",
        client_id: "92",
        client_version: "0.147.945",
        user_agent: IOS_PRODUCER_UA,
        host: "www.youtube.com",
        supports_cookies: false,
        jsless: true,
        include_context_ua: false,
        client_screen: None,
        embed_url: None,
        device_make: Some("Apple"),
        device_model: Some("iPhone16,2"),
        os_name: Some("iPhone"),
        os_version: Some("18.3.2.22D82"),
        android_sdk_version: None,
    },
];

pub const DEFAULT_CLIENTS_WITH_COOKIE: &[&str] = &[
    "web_music",
    "web_safari",
    "tv_samsung",
    "android_vr",
    "visionos",
    "web_creator",
    "web",
    "mweb",
    "android",
    "ios",
];

pub const DEFAULT_CLIENTS_NO_COOKIE: &[&str] = &[
    "visionos",
    "android",
    "android_vr",
    "ios",
    "ios_producer",
    "web_safari",
    "web",
];

/// Normalizes known client aliases (`web_remix` -> `web_music`, `tv_embedded` -> `tv_simply`).
pub fn normalize_client_alias(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "web_remix" | "web-remix" | "web-music" => "web_music".to_owned(),
        "tv_embedded" | "tv-embedded" | "tv-simply" => "tv_simply".to_owned(),
        "web-safari" => "web_safari".to_owned(),
        "web-embedded" => "web_embedded".to_owned(),
        "web-creator" => "web_creator".to_owned(),
        "tv-downgraded" => "tv_downgraded".to_owned(),
        "tv-samsung" => "tv_samsung".to_owned(),
        "android-vr" => "android_vr".to_owned(),
        "ios-producer" => "ios_producer".to_owned(),
        _ => lower,
    }
}

/// Looks up a client spec by upstream key or alias (case-insensitive).
pub fn lookup_client(key: &str) -> Option<&'static InnerTubeClientSpec> {
    let norm = normalize_client_alias(key);
    INNERTUBE_CLIENTS.iter().find(|c| c.key == norm)
}

/// Resolves `--extractor-args youtube:player_client=...` specifications supporting:
/// - `default`
/// - `all`
/// - aliases (`web_remix` -> `web_music`, `tv_embedded` -> `tv_simply`)
/// - exclusions prefixed with `-` (e.g. `default,-ios` or `-ios`)
///
/// Preserves exact requested order and deduplicates cleanly.
pub fn resolve_requested_clients(
    requested: &[String],
    has_cookie: bool,
) -> Vec<&'static InnerTubeClientSpec> {
    let default_list = if has_cookie {
        DEFAULT_CLIENTS_WITH_COOKIE
    } else {
        DEFAULT_CLIENTS_NO_COOKIE
    };

    let mut excluded: HashSet<String> = HashSet::new();
    let mut positive_tokens: Vec<String> = Vec::new();

    for raw in requested {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(neg) = trimmed.strip_prefix('-') {
            let norm = normalize_client_alias(neg);
            if norm == "default" {
                for &k in default_list {
                    excluded.insert(k.to_owned());
                }
            } else if norm == "all" {
                for c in INNERTUBE_CLIENTS {
                    excluded.insert(c.key.to_owned());
                }
            } else if !norm.is_empty() {
                excluded.insert(norm);
            }
        } else {
            positive_tokens.push(normalize_client_alias(trimmed));
        }
    }

    if positive_tokens.is_empty() {
        positive_tokens.push("default".to_owned());
    }

    let mut out: Vec<&'static InnerTubeClientSpec> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();

    let mut push_key = |k: &str| {
        if excluded.contains(k) {
            return;
        }
        if let Some(spec) = lookup_client(k)
            && seen.insert(spec.key)
        {
            out.push(spec);
        }
    };

    for tok in &positive_tokens {
        match tok.as_str() {
            "default" => {
                for &k in default_list {
                    push_key(k);
                }
            }
            "all" => {
                for spec in INNERTUBE_CLIENTS {
                    push_key(spec.key);
                }
            }
            other => push_key(other),
        }
    }

    out
}

/// Computes the exact upstream YouTube multi-scheme `Authorization` header
/// (matching `yt-dlp`'s `_get_sid_authorization_header`):
/// emits space-joined `SAPISIDHASH` (from `SAPISID`, falling back to `__Secure-3PAPISID`),
/// `SAPISID1PHASH` (from `__Secure-1PAPISID`), and `SAPISID3PHASH` (from `__Secure-3PAPISID`).
pub fn compute_youtube_auth_header(
    cookie_header: &str,
    origin: &str,
    timestamp_secs: u64,
) -> Option<String> {
    let origin_clean = origin.trim_end_matches('/');
    let sapisid = extract_cookie_value(cookie_header, "SAPISID");
    let sapisid1p = extract_cookie_value(cookie_header, "__Secure-1PAPISID");
    let sapisid3p = extract_cookie_value(cookie_header, "__Secure-3PAPISID");

    let mut parts = Vec::with_capacity(3);

    if let Some(ref sid) = sapisid.or_else(|| sapisid3p.clone()) {
        let digest = sha1_hex(format!("{timestamp_secs} {sid} {origin_clean}").as_bytes());
        parts.push(format!("SAPISIDHASH {timestamp_secs}_{digest}"));
    }
    if let Some(ref sid1p) = sapisid1p {
        let digest = sha1_hex(format!("{timestamp_secs} {sid1p} {origin_clean}").as_bytes());
        parts.push(format!("SAPISID1PHASH {timestamp_secs}_{digest}"));
    }
    if let Some(ref sid3p) = sapisid3p {
        let digest = sha1_hex(format!("{timestamp_secs} {sid3p} {origin_clean}").as_bytes());
        parts.push(format!("SAPISID3PHASH {timestamp_secs}_{digest}"));
    }

    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_all_15_upstream_clients_with_expected_fields() {
        assert_eq!(INNERTUBE_CLIENTS.len(), 15);
        let expected_keys = [
            "web",
            "web_safari",
            "web_embedded",
            "web_music",
            "web_creator",
            "mweb",
            "tv",
            "tv_downgraded",
            "tv_samsung",
            "tv_simply",
            "visionos",
            "android",
            "android_vr",
            "ios",
            "ios_producer",
        ];
        for key in expected_keys {
            let spec = lookup_client(key).unwrap_or_else(|| panic!("missing client `{key}`"));
            assert_eq!(spec.key, key);
            assert!(!spec.client_id.is_empty());
            assert!(!spec.client_version.is_empty());
            assert!(!spec.user_agent.is_empty());
        }

        // Verify aliases.
        assert_eq!(lookup_client("web_remix").unwrap().key, "web_music");
        assert_eq!(lookup_client("tv_embedded").unwrap().key, "tv_simply");

        // Verify `web_embedded` dynamic context includes `clientScreen: "EMBED"` + `thirdParty.embedUrl`.
        let emb = lookup_client("web_embedded").unwrap();
        let ctx = emb.build_context("VIS_1", "zh-CN", "JP");
        assert_eq!(ctx["client"]["clientName"], "WEB_EMBEDDED_PLAYER");
        assert_eq!(ctx["client"]["clientScreen"], "EMBED");
        assert_eq!(ctx["client"]["hl"], "zh-CN");
        assert_eq!(ctx["client"]["gl"], "JP");
        assert_eq!(ctx["client"]["visitorData"], "VIS_1");
        assert_eq!(ctx["thirdParty"]["embedUrl"], "https://www.youtube.com/");

        // Verify `cookies` and `jsless` flags match upstream contract.
        assert!(lookup_client("web_music").unwrap().supports_cookies);
        assert!(lookup_client("tv_samsung").unwrap().supports_cookies);
        assert!(!lookup_client("tv_simply").unwrap().supports_cookies);
        assert!(!lookup_client("visionos").unwrap().supports_cookies);
        assert!(lookup_client("visionos").unwrap().jsless);
        assert!(lookup_client("ios_producer").unwrap().jsless);
        assert!(!lookup_client("web_safari").unwrap().jsless);
    }

    #[test]
    fn computes_multi_scheme_sapisid_authorization_header() {
        let cookie =
            "SAPISID=plain_sid; __Secure-1PAPISID=one_p_sid; __Secure-3PAPISID=three_p_sid";
        let origin = "https://music.youtube.com";
        let ts = 1_700_000_000;
        let auth = compute_youtube_auth_header(cookie, origin, ts).expect("auth header");
        let h0 = sha1_hex(b"1700000000 plain_sid https://music.youtube.com");
        let h1 = sha1_hex(b"1700000000 one_p_sid https://music.youtube.com");
        let h3 = sha1_hex(b"1700000000 three_p_sid https://music.youtube.com");
        assert_eq!(
            auth,
            format!("SAPISIDHASH {ts}_{h0} SAPISID1PHASH {ts}_{h1} SAPISID3PHASH {ts}_{h3}")
        );

        // Fallback: when only __Secure-3PAPISID is present, SAPISIDHASH uses __Secure-3PAPISID too.
        let auth_3p_only =
            compute_youtube_auth_header("__Secure-3PAPISID=three_p_sid", origin, ts).unwrap();
        assert_eq!(
            auth_3p_only,
            format!("SAPISIDHASH {ts}_{h3} SAPISID3PHASH {ts}_{h3}")
        );
    }

    #[test]
    fn resolves_requested_clients_with_default_all_aliases_and_exclusions() {
        let with_cookie: Vec<&str> = resolve_requested_clients(&[], true)
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(with_cookie, DEFAULT_CLIENTS_WITH_COOKIE);

        let no_cookie: Vec<&str> = resolve_requested_clients(&[], false)
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(no_cookie, DEFAULT_CLIENTS_NO_COOKIE);

        let excl: Vec<&str> = resolve_requested_clients(
            &["default".to_owned(), "-ios".to_owned(), "-mweb".to_owned()],
            true,
        )
        .into_iter()
        .map(|c| c.key)
        .collect();
        assert!(!excl.contains(&"ios"));
        assert!(!excl.contains(&"mweb"));
        assert_eq!(excl[0], "web_music");

        let all_minus: Vec<&str> =
            resolve_requested_clients(&["all".to_owned(), "-tv_embedded".to_owned()], false)
                .into_iter()
                .map(|c| c.key)
                .collect();
        assert_eq!(all_minus.len(), 14);
        assert!(!all_minus.contains(&"tv_simply"));

        let aliased: Vec<&str> = resolve_requested_clients(
            &[
                "web_safari".to_owned(),
                "web_remix".to_owned(),
                "web_safari".to_owned(),
            ],
            true,
        )
        .into_iter()
        .map(|c| c.key)
        .collect();
        assert_eq!(aliased, vec!["web_safari", "web_music"]);
    }
}
