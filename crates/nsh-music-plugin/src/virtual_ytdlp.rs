//! In-process pure-Rust `yt-dlp` virtualization shim (`VirtualYtdlpShim` / `YtdlpVirtualShim`).
//!
//! Enables `mygo-music` plugins calling `host.spawn("yt-dlp", ...)` (including `youtube-music`)
//! to work seamlessly on iOS, Android, HarmonyOS, and desktops without an external `yt-dlp` / Python
//! installation. On Desktop when `Allow Programs` is granted and a real `yt-dlp` binary exists,
//! the real binary is preferred; otherwise (or on Mobile) `VirtualYtdlpShim` runs in-process
//! using `reqwest` (`rustls-tls`) and pure-Rust SHA-1 `SAPISIDHASH` authentication.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::host::{
    ExternalProcessHandle, HostContext, ManagedProcess, SpawnOptions, VirtualPipeHandle,
};
use crate::mobile_compat::{
    VirtualProcessShim, VirtualSpawnResult, resolve_real_binary, resolve_relative_command,
    with_default_cwd,
};
use crate::yt_ejs;

const DEFAULT_INNERTUBE_ORIGIN: &str = "https://music.youtube.com";
const WEB_YOUTUBE_ORIGIN: &str = "https://www.youtube.com";
const VISITOR_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const VISITOR_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const PLAYER_HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const PREFLIGHT_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
const DEFAULT_CASCADE_BUDGET: Duration = Duration::from_secs(40);

const WEB_REMIX_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const WEB_SAFARI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.5 Safari/605.1.15,gzip(gfe)";
const ANDROID_UA: &str = "com.google.android.youtube/21.26.364 (Linux; U; Android 11) gzip";
const ANDROID_VR_UA: &str = "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
const IOS_UA: &str =
    "com.google.ios.youtube/21.26.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)";
const VISION_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// Synthetic `--version` banner emitted by [`VirtualYtdlpShim`].
pub const VIRTUAL_YTDLP_VERSION_BANNER: &str = "2026.03.01 (nsh-virtual-ytdlp)\n";

/// Returns `true` if `cmd` targets a `yt-dlp` executable (bare name, path, or `.exe`).
pub fn is_ytdlp_command(cmd: &str) -> bool {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return false;
    }
    let file_name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("yt-dlp")
}

/// Output format requested by `yt-dlp` CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YtdlpOutputMode {
    /// `--print "%(.{url,http_headers})j"` (JSON line with `url` and `http_headers`).
    PrintUrlAndHeadersJson,
    /// `-g`, `--get-url`, `--print url`, or `--print %(url)s` (raw URL line).
    PrintUrl,
    /// `-j` or `--dump-json` (full track info JSON object).
    DumpJson,
}

/// Parsed `yt-dlp` CLI + stdin config options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtdlpParsedArgs {
    pub is_version: bool,
    pub reads_stdin_config: bool,
    pub output_mode: YtdlpOutputMode,
    pub format_expr: Option<String>,
    pub extractor_args: Vec<String>,
    pub headers: HashMap<String, String>,
    pub cookies_file: Option<String>,
    pub target: Option<String>,
}

impl Default for YtdlpParsedArgs {
    fn default() -> Self {
        Self {
            is_version: false,
            reads_stdin_config: false,
            output_mode: YtdlpOutputMode::PrintUrl,
            format_expr: None,
            extractor_args: Vec::new(),
            headers: HashMap::new(),
            cookies_file: None,
            target: None,
        }
    }
}

/// Parses `yt-dlp` CLI arguments into a structured [`YtdlpParsedArgs`].
pub fn parse_ytdlp_args(args: &[String]) -> YtdlpParsedArgs {
    let mut parsed = YtdlpParsedArgs::default();
    apply_ytdlp_tokens(&mut parsed, args, true);
    parsed
}

/// Tokenizes and merges `yt-dlp --config-locations -` stdin lines into `parsed`.
pub fn merge_ytdlp_stdin_config(parsed: &mut YtdlpParsedArgs, stdin: &str) {
    let mut tokens = Vec::new();
    for line in stdin.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        tokens.extend(tokenize_config_line(trimmed));
    }
    apply_ytdlp_tokens(parsed, &tokens, false);
}

fn apply_ytdlp_tokens(parsed: &mut YtdlpParsedArgs, tokens: &[String], allow_positional: bool) {
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);

        if tok == "--version" || tok == "-version" {
            parsed.is_version = true;
            i += 1;
            continue;
        }
        if tok == "--config-locations" {
            if next == Some("-") {
                parsed.reads_stdin_config = true;
            }
            i += if next.is_some() { 2 } else { 1 };
            continue;
        }
        if let Some(val) = tok.strip_prefix("--config-locations=") {
            if val.trim() == "-" {
                parsed.reads_stdin_config = true;
            }
            i += 1;
            continue;
        }
        if tok == "-g" || tok == "--get-url" {
            parsed.output_mode = YtdlpOutputMode::PrintUrl;
            i += 1;
            continue;
        }
        if tok == "-j" || tok == "--dump-json" {
            parsed.output_mode = YtdlpOutputMode::DumpJson;
            i += 1;
            continue;
        }
        if tok == "--print" {
            if let Some(tpl) = next {
                apply_print_template(parsed, tpl);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(tpl) = tok.strip_prefix("--print=") {
            apply_print_template(parsed, tpl);
            i += 1;
            continue;
        }
        if tok == "-f" || tok == "--format" {
            if let Some(fmt) = next {
                parsed.format_expr = Some(fmt.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(fmt) = tok.strip_prefix("--format=") {
            parsed.format_expr = Some(fmt.to_owned());
            i += 1;
            continue;
        }
        if tok == "--extractor-args" {
            if let Some(ea) = next {
                parsed.extractor_args.push(ea.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(ea) = tok.strip_prefix("--extractor-args=") {
            parsed.extractor_args.push(ea.to_owned());
            i += 1;
            continue;
        }
        if tok == "--add-headers" || tok == "--add-header" {
            if let Some(hdr) = next {
                insert_header_line(&mut parsed.headers, hdr);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(hdr) = tok
            .strip_prefix("--add-headers=")
            .or_else(|| tok.strip_prefix("--add-header="))
        {
            insert_header_line(&mut parsed.headers, hdr);
            i += 1;
            continue;
        }
        if tok == "--user-agent" {
            if let Some(ua) = next {
                upsert_header(&mut parsed.headers, "User-Agent", ua);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(ua) = tok.strip_prefix("--user-agent=") {
            upsert_header(&mut parsed.headers, "User-Agent", ua);
            i += 1;
            continue;
        }
        if tok == "--cookies" {
            if let Some(path) = next {
                parsed.cookies_file = Some(path.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(path) = tok.strip_prefix("--cookies=") {
            parsed.cookies_file = Some(path.to_owned());
            i += 1;
            continue;
        }
        if matches!(
            tok,
            "--js-runtimes"
                | "--proxy"
                | "--referer"
                | "--socket-timeout"
                | "--retries"
                | "-o"
                | "--output"
        ) {
            if tok == "--referer"
                && let Some(r) = next
            {
                upsert_header(&mut parsed.headers, "Referer", r);
            }
            i += if next.is_some() { 2 } else { 1 };
            continue;
        }
        if tok.starts_with('-') {
            i += 1;
            continue;
        }
        if allow_positional {
            parsed.target = Some(tok.to_owned());
        }
        i += 1;
    }
}

fn apply_print_template(parsed: &mut YtdlpParsedArgs, tpl: &str) {
    let t = tpl.trim();
    if t.contains("http_headers") || t.ends_with("}j") || t.ends_with(")j") {
        parsed.output_mode = YtdlpOutputMode::PrintUrlAndHeadersJson;
    } else if t == "url" || t == "%(url)s" {
        parsed.output_mode = YtdlpOutputMode::PrintUrl;
    }
}

fn insert_header_line(headers: &mut HashMap<String, String>, raw: &str) {
    if let Some((k, v)) = raw.split_once(':') {
        let key = k.trim();
        let val = v.trim();
        if !key.is_empty() {
            upsert_header(headers, key, val);
        }
    }
}

fn upsert_header(headers: &mut HashMap<String, String>, key: &str, value: &str) {
    if let Some(existing_key) = headers
        .keys()
        .find(|k| k.eq_ignore_ascii_case(key))
        .cloned()
    {
        headers.remove(&existing_key);
    }
    headers.insert(key.to_owned(), value.to_owned());
}

fn find_header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Shell-style tokenizer for `yt-dlp` config lines (handles `"..."`, `'...'`, and `\"` / `\\` escapes).
fn tokenize_config_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    if let Some(next_c) = chars.next() {
                        cur.push(next_c);
                    }
                } else if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    in_token = true;
                } else if c == '\\' {
                    if let Some(next_c) = chars.next() {
                        cur.push(next_c);
                        in_token = true;
                    }
                } else if c.is_whitespace() {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else {
                    cur.push(c);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// Extracts a YouTube video ID from `music.youtube.com/watch?v=...`, `www.youtube.com/watch?v=...`,
/// `youtu.be/<id>`, `youtube.com/shorts/<id>`, `/embed/<id>`, or a bare 11-char video ID.
pub fn extract_youtube_video_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_bare_youtube_id(trimmed) {
        return Some(trimmed.to_owned());
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let is_yt = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be");
    if !is_yt {
        return None;
    }

    if host == "youtu.be" || host.ends_with(".youtu.be") {
        let seg = parsed.path_segments()?.find(|s| !s.is_empty())?;
        return Some(seg.to_owned());
    }

    for (k, v) in parsed.query_pairs() {
        if k == "v" && !v.is_empty() {
            return Some(v.into_owned());
        }
    }

    let segs: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.len() >= 2 && matches!(segs[0], "shorts" | "embed" | "v" | "live") {
        return Some(segs[1].to_owned());
    }
    None
}

fn is_bare_youtube_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Pure-Rust FIPS-180-4 SHA-1 lowercase hex digest (zero OpenSSL / external crate dependencies).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xefcd_ab89;
    let mut h2: u32 = 0x98ba_dcfe;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xc3d2_e1f0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc_u32),
                _ => (b ^ c ^ d, 0xca62_c1d6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    digest[0..4].copy_from_slice(&h0.to_be_bytes());
    digest[4..8].copy_from_slice(&h1.to_be_bytes());
    digest[8..12].copy_from_slice(&h2.to_be_bytes());
    digest[12..16].copy_from_slice(&h3.to_be_bytes());
    digest[16..20].copy_from_slice(&h4.to_be_bytes());
    hex::encode(digest)
}

/// Extracts a named cookie value from a `Cookie` HTTP header string.
pub fn extract_cookie_value(cookie_header: &str, name: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        if let Some((k, v)) = part.split_once('=')
            && k.trim() == name
        {
            let val = v.trim();
            if !val.is_empty() {
                return Some(val.to_owned());
            }
        }
    }
    None
}

/// Computes YouTube's `Authorization: SAPISIDHASH <ts>_<sha1>` header from a `Cookie` header
/// containing `__Secure-3PAPISID` or `SAPISID`.
pub fn compute_sapisidhash(
    cookie_header: &str,
    origin: &str,
    timestamp_secs: u64,
) -> Option<String> {
    let sapisid = extract_cookie_value(cookie_header, "__Secure-3PAPISID")
        .or_else(|| extract_cookie_value(cookie_header, "SAPISID"))?;
    let payload = format!("{timestamp_secs} {sapisid} {origin}");
    let digest = sha1_hex(payload.as_bytes());
    Some(format!("SAPISIDHASH {timestamp_secs}_{digest}"))
}

fn read_netscape_cookies_file(path: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut pairs = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("#HttpOnly_")) {
            continue;
        }
        let clean = trimmed.strip_prefix("#HttpOnly_").unwrap_or(trimmed);
        let cols: Vec<&str> = clean.split('\t').collect();
        if cols.len() >= 7 {
            let domain = cols[0].to_ascii_lowercase();
            if domain.contains("youtube.com") || domain.contains("google.com") {
                pairs.push(format!("{}={}", cols[5].trim(), cols[6].trim()));
            }
        }
    }
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// Resolved audio stream metadata returned by the InnerTube cascade.
#[derive(Debug, Clone)]
pub struct ResolvedAudioStream {
    pub video_id: String,
    pub title: String,
    pub url: String,
    pub mime_type: String,
    pub ext: String,
    pub acodec: String,
    pub format_id: String,
    pub bitrate: u64,
    pub duration_secs: Option<u64>,
    pub user_agent: String,
    pub is_hls: bool,
}

fn visitor_cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn fetch_or_cached_visitor_data(
    client: &reqwest::Client,
    base: &str,
    timeout: Duration,
) -> String {
    if let Ok(guard) = visitor_cache().lock()
        && let Some((v, at)) = guard.get(base)
        && at.elapsed() < VISITOR_CACHE_TTL
    {
        return v.clone();
    }

    let req_timeout = timeout.min(VISITOR_HTTP_TIMEOUT);
    if req_timeout.is_zero() {
        return String::new();
    }

    // 1. Primary: POST /youtubei/v1/visitor_id?prettyPrint=false
    let visitor_url = format!("{base}/youtubei/v1/visitor_id?prettyPrint=false");
    let body = json!({
        "context": {
            "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": "1.20260301.01.00",
                "hl": "en"
            }
        }
    });
    if let Ok(Ok(resp)) = tokio::time::timeout(
        req_timeout,
        client
            .post(&visitor_url)
            .header("content-type", "application/json")
            .header("user-agent", WEB_REMIX_UA)
            .header("origin", base)
            .header("x-origin", base)
            .json(&body)
            .timeout(req_timeout)
            .send(),
    )
    .await
        && resp.status().is_success()
        && let Ok(Ok(val)) = tokio::time::timeout(req_timeout, resp.json::<Value>()).await
        && let Some(vd) = val
            .pointer("/responseContext/visitorData")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    {
        cache_visitor_data(base, vd);
        return vd.to_owned();
    }

    // 2. Fallback: scrape `"VISITOR_DATA":"..."` / `"visitorData":"..."` from homepage HTML.
    if let Ok(Ok(resp)) = tokio::time::timeout(
        req_timeout,
        client
            .get(format!("{base}/"))
            .header("user-agent", WEB_REMIX_UA)
            .timeout(req_timeout)
            .send(),
    )
    .await
        && resp.status().is_success()
        && let Ok(Ok(html)) = tokio::time::timeout(req_timeout, resp.text()).await
        && let Some(vd) = scrape_visitor_data_from_html(&html)
    {
        cache_visitor_data(base, &vd);
        return vd;
    }

    String::new()
}

fn cache_visitor_data(base: &str, visitor: &str) {
    if let Ok(mut guard) = visitor_cache().lock() {
        guard.insert(base.to_owned(), (visitor.to_owned(), Instant::now()));
    }
}

fn scrape_visitor_data_from_html(html: &str) -> Option<String> {
    for needle in ["\"VISITOR_DATA\":\"", "\"visitorData\":\""] {
        if let Some(pos) = html.find(needle) {
            let rest = &html[pos + needle.len()..];
            if let Some(end) = rest.find('"') {
                let candidate = &rest[..end];
                if !candidate.is_empty() && candidate.len() < 256 {
                    return Some(candidate.to_owned());
                }
            }
        }
    }
    None
}

/// Decodes the 2-letter uppercase GeoIP country code (e.g. `"US"`, `"CN"`, `"HK"`)
/// embedded in a YouTube `visitorData` protobuf payload (field `6` -> nested field `1`).
pub fn decode_visitor_data_geo(visitor_data: &str) -> Option<String> {
    let trimmed = visitor_data.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unescaped = percent_decode_ascii(trimmed)?;
    let raw = decode_base64_flexible(&unescaped)?;
    parse_visitor_pb_geo(&raw)
}

fn percent_decode_ascii(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_nibble(bytes[i + 1])?;
            let lo = hex_nibble(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_base64_flexible(input: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            break;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        vals.push(v);
    }
    if vals.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity((vals.len() * 3) / 4);
    let mut i = 0;
    while i + 4 <= vals.len() {
        let a = vals[i];
        let b = vals[i + 1];
        let c = vals[i + 2];
        let d = vals[i + 3];
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    match vals.len() - i {
        2 => {
            let a = vals[i];
            let b = vals[i + 1];
            out.push((a << 2) | (b >> 4));
        }
        3 => {
            let a = vals[i];
            let b = vals[i + 1];
            let c = vals[i + 2];
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        }
        _ => {}
    }
    Some(out)
}

fn read_pb_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while *pos < bytes.len() && shift < 64 {
        let b = bytes[*pos];
        *pos += 1;
        value |= u64::from(b & 0x7f) << shift;
        if (b & 0x80) == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn parse_visitor_pb_geo(bytes: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos < bytes.len() {
        let tag = read_pb_varint(bytes, &mut pos)?;
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                read_pb_varint(bytes, &mut pos)?;
            }
            1 => {
                if pos.checked_add(8)? > bytes.len() {
                    return None;
                }
                pos += 8;
            }
            2 => {
                let len = usize::try_from(read_pb_varint(bytes, &mut pos)?).ok()?;
                let end = pos.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }
                let slice = &bytes[pos..end];
                pos = end;
                if field_num == 6
                    && let Some(cc) = parse_visitor_pb_nested_geo(slice)
                {
                    return Some(cc);
                }
            }
            5 => {
                if pos.checked_add(4)? > bytes.len() {
                    return None;
                }
                pos += 4;
            }
            _ => return None,
        }
    }
    None
}

fn parse_visitor_pb_nested_geo(bytes: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos < bytes.len() {
        let tag = read_pb_varint(bytes, &mut pos)?;
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                read_pb_varint(bytes, &mut pos)?;
            }
            1 => {
                if pos.checked_add(8)? > bytes.len() {
                    return None;
                }
                pos += 8;
            }
            2 => {
                let len = usize::try_from(read_pb_varint(bytes, &mut pos)?).ok()?;
                let end = pos.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }
                let slice = &bytes[pos..end];
                pos = end;
                if field_num == 1
                    && slice.len() == 2
                    && slice.iter().all(|b| b.is_ascii_uppercase())
                {
                    return String::from_utf8(slice.to_vec()).ok();
                }
            }
            5 => {
                if pos.checked_add(4)? > bytes.len() {
                    return None;
                }
                pos += 4;
            }
            _ => return None,
        }
    }
    None
}

struct InnerTubeClientSpec {
    label: &'static str,
    client_id: &'static str,
    client_version: &'static str,
    user_agent: &'static str,
    use_visitor: bool,
    is_web_client: bool,
    build_context: fn(visitor: &str) -> Value,
}

const CLIENT_WEB_REMIX: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "WEB_REMIX",
    client_id: "67",
    client_version: "1.20260707.12.00",
    user_agent: WEB_REMIX_UA,
    use_visitor: true,
    is_web_client: true,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "WEB_REMIX",
            "clientVersion": "1.20260707.12.00",
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_WEB_SAFARI: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "WEB_SAFARI",
    client_id: "1",
    client_version: "2.20260708.00.00",
    user_agent: WEB_SAFARI_UA,
    use_visitor: true,
    is_web_client: true,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "WEB",
            "clientVersion": "2.20260708.00.00",
            "userAgent": WEB_SAFARI_UA,
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_WEB_CREATOR: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "WEB_CREATOR",
    client_id: "62",
    client_version: "1.20260708.06.00",
    user_agent: WEB_REMIX_UA,
    use_visitor: true,
    is_web_client: true,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "WEB_CREATOR",
            "clientVersion": "1.20260708.06.00",
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_WEB: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "WEB",
    client_id: "1",
    client_version: "2.20260708.00.00",
    user_agent: WEB_REMIX_UA,
    use_visitor: true,
    is_web_client: true,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "WEB",
            "clientVersion": "2.20260708.00.00",
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_ANDROID: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "ANDROID",
    client_id: "3",
    client_version: "21.26.364",
    user_agent: ANDROID_UA,
    use_visitor: true,
    is_web_client: false,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "ANDROID",
            "clientVersion": "21.26.364",
            "osName": "Android",
            "osVersion": "11",
            "androidSdkVersion": 30,
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_VISIONOS: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "VISIONOS",
    client_id: "101",
    client_version: "1.02",
    user_agent: VISION_UA,
    use_visitor: true,
    is_web_client: false,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "VISIONOS",
            "clientVersion": "1.02",
            "deviceMake": "Apple",
            "deviceModel": "RealityDevice17,1",
            "userAgent": VISION_UA,
            "osName": "visionOS",
            "osVersion": "26.5.23O471"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_ANDROID_VR: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "ANDROID_VR",
    client_id: "28",
    client_version: "1.65.10",
    user_agent: ANDROID_VR_UA,
    use_visitor: true,
    is_web_client: false,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "ANDROID_VR",
            "clientVersion": "1.65.10",
            "deviceMake": "Oculus",
            "deviceModel": "Quest 3",
            "osName": "Android",
            "osVersion": "12L",
            "androidSdkVersion": 32,
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

const CLIENT_IOS: InnerTubeClientSpec = InnerTubeClientSpec {
    label: "IOS",
    client_id: "5",
    client_version: "21.26.4",
    user_agent: IOS_UA,
    use_visitor: true,
    is_web_client: false,
    build_context: |visitor| {
        let mut client = json!({
            "clientName": "IOS",
            "clientVersion": "21.26.4",
            "deviceMake": "Apple",
            "deviceModel": "iPhone16,2",
            "osName": "iPhone",
            "osVersion": "18.3.2.22D82",
            "hl": "en",
            "gl": "US"
        });
        if !visitor.is_empty()
            && let Some(m) = client.as_object_mut()
        {
            m.insert("visitorData".to_owned(), json!(visitor));
        }
        json!({ "client": client })
    },
};

struct CascadeStep<'a> {
    spec: &'static InnerTubeClientSpec,
    origin: &'a str,
    send_auth: bool,
}

fn build_cascade_steps<'a>(
    primary: &'a str,
    secondary: Option<&'a str>,
    include_auth_web: bool,
) -> Vec<CascadeStep<'a>> {
    let mut steps: Vec<CascadeStep<'a>> = Vec::with_capacity(10);
    if include_auth_web {
        let web_origin = secondary.unwrap_or(WEB_YOUTUBE_ORIGIN);
        steps.push(CascadeStep {
            spec: &CLIENT_WEB_REMIX,
            origin: primary,
            send_auth: true,
        });
        steps.push(CascadeStep {
            spec: &CLIENT_WEB_SAFARI,
            origin: web_origin,
            send_auth: true,
        });
        steps.push(CascadeStep {
            spec: &CLIENT_WEB_CREATOR,
            origin: web_origin,
            send_auth: true,
        });
        steps.push(CascadeStep {
            spec: &CLIENT_WEB,
            origin: web_origin,
            send_auth: true,
        });
    }
    steps.push(CascadeStep {
        spec: &CLIENT_VISIONOS,
        origin: primary,
        send_auth: false,
    });
    if let Some(web) = secondary {
        steps.push(CascadeStep {
            spec: &CLIENT_VISIONOS,
            origin: web,
            send_auth: false,
        });
    }
    steps.push(CascadeStep {
        spec: &CLIENT_ANDROID,
        origin: primary,
        send_auth: false,
    });
    if let Some(web) = secondary {
        steps.push(CascadeStep {
            spec: &CLIENT_ANDROID,
            origin: web,
            send_auth: false,
        });
    }
    steps.push(CascadeStep {
        spec: &CLIENT_ANDROID_VR,
        origin: primary,
        send_auth: false,
    });
    steps.push(CascadeStep {
        spec: &CLIENT_IOS,
        origin: primary,
        send_auth: false,
    });
    steps
}

fn is_loopback_origin(base_origin: &str) -> bool {
    let Ok(parsed) = url::Url::parse(base_origin) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("localhost.")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn sanitize_url_for_log(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty>".to_owned();
    }
    match url::Url::parse(trimmed) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("<no-host>");
            match u.port() {
                Some(port) => format!("{}://{host}:{port}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        }
        Err(_) => "<invalid-url>".to_owned(),
    }
}

/// Synchronous helper for `PluginSource::stream` rescue fallback, resolving a YouTube
/// stream via the official 2026 InnerTube + `yt-dlp-ejs` cascade with preflight enabled.
pub(crate) fn resolve_youtube_stream_blocking(
    http_client: reqwest::Client,
    video_id: &str,
    cookie: Option<&str>,
    prefer_mobile_aac: bool,
    cache_dir: Option<&Path>,
) -> Result<ResolvedAudioStream, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to init rescue tokio runtime: {e}"))?;
    rt.block_on(resolve_innertube_stream_with_cache(
        &http_client,
        DEFAULT_INNERTUBE_ORIGIN,
        video_id,
        cookie,
        None,
        prefer_mobile_aac,
        true,
        cache_dir,
    ))
}

/// Runs the pure-Rust InnerTube `/youtubei/v1/player` cascade with stream-URL preflight
/// enabled automatically for non-loopback origins.
pub async fn resolve_innertube_stream(
    client: &reqwest::Client,
    base_origin: &str,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    prefer_mobile_aac: bool,
) -> Result<ResolvedAudioStream, String> {
    resolve_innertube_stream_with_options(
        client,
        base_origin,
        video_id,
        cookie,
        format_expr,
        prefer_mobile_aac,
        !is_loopback_origin(base_origin),
    )
    .await
}

/// Runs the pure-Rust InnerTube `/youtubei/v1/player` cascade with explicit stream-URL
/// preflight control:
/// - When `cookie` is non-empty on a non-loopback origin, tries authenticated official 2026
///   web clients (`WEB_REMIX`, `WEB_SAFARI`, `WEB_CREATOR`, `WEB`) with `tv-player-ias.js`
///   `signatureTimestamp` and in-process `yt-dlp-ejs` 0.8.0 challenge solving (`signatureCipher`,
///   `n`, and HLS variant `/n/`), then falls back to anonymous mobile clients (`VISIONOS`,
///   `ANDROID`, `ANDROID_VR`, `IOS`) without sending browser cookies/SAPISIDHASH to mobile clients.
/// - When `cookie` is absent or `base_origin` is loopback, runs the clean anonymous mobile cascade.
pub async fn resolve_innertube_stream_with_options(
    client: &reqwest::Client,
    base_origin: &str,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
) -> Result<ResolvedAudioStream, String> {
    resolve_innertube_stream_with_cache(
        client,
        base_origin,
        video_id,
        cookie,
        format_expr,
        prefer_mobile_aac,
        verify_stream_url,
        None,
    )
    .await
}

/// Same as [`resolve_innertube_stream_with_options`] with an optional disk `cache_dir`
/// for persisting preprocessed `tv-player-ias.js` (`<cache_dir>/yt-ejs-player.json`).
#[allow(clippy::too_many_arguments)]
pub async fn resolve_innertube_stream_with_cache(
    client: &reqwest::Client,
    base_origin: &str,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
    cache_dir: Option<&Path>,
) -> Result<ResolvedAudioStream, String> {
    let primary = base_origin.trim_end_matches('/');
    let secondary = (primary == DEFAULT_INNERTUBE_ORIGIN).then_some(WEB_YOUTUBE_ORIGIN);
    resolve_innertube_stream_inner(
        client,
        primary,
        secondary,
        video_id,
        cookie,
        format_expr,
        prefer_mobile_aac,
        verify_stream_url,
        DEFAULT_CASCADE_BUDGET,
        cache_dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn resolve_innertube_stream_inner(
    client: &reqwest::Client,
    primary_origin: &str,
    secondary_origin: Option<&str>,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
    total_budget: Duration,
    cache_dir: Option<&Path>,
) -> Result<ResolvedAudioStream, String> {
    let primary = primary_origin.trim_end_matches('/');
    let secondary = secondary_origin.map(|s| s.trim_end_matches('/'));
    let trimmed_cookie = cookie.map(str::trim).filter(|s| !s.is_empty());
    let include_auth_web = trimmed_cookie.is_some() && !is_loopback_origin(primary);
    let steps = build_cascade_steps(primary, secondary, include_auth_web);

    let deadline = Instant::now() + total_budget;
    let mut visitor_by_origin: HashMap<String, String> = HashMap::new();
    let mut detected_geo: Option<String> = None;
    let mut tv_bundle: Option<yt_ejs::TvPlayerBundle> = None;
    let mut tv_bundle_failed = false;

    let mut last_playability_reason: Option<String> = None;
    let mut errors = Vec::new();

    for step in steps {
        let spec = step.spec;
        let step_origin = step.origin;
        let Some(rem) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} exceeded cascade budget ({:?})",
                total_budget
            );
            errors.push(format!("cascade budget ({:?}) exceeded", total_budget));
            break;
        };

        let origin_label = sanitize_url_for_log(step_origin);
        let client_tag = if step.send_auth {
            format!("{}(auth@{origin_label})", spec.label)
        } else {
            format!("{}(anon@{origin_label})", spec.label)
        };

        // Ensure `tv-player-ias.js` bundle (with `sts`) is available before authenticated web clients.
        if step.send_auth && tv_bundle.is_none() && !tv_bundle_failed {
            match yt_ejs::fetch_or_cached_tv_player(
                client,
                cache_dir,
                rem.min(Duration::from_secs(15)),
            )
            .await
            {
                Ok(bundle) => {
                    tv_bundle = Some(bundle);
                }
                Err(e) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} failed to fetch tv-player-ias.js: {e}"
                    );
                    tv_bundle_failed = true;
                }
            }
        }

        let Some(rem_visitor) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} exceeded cascade budget ({:?})",
                total_budget
            );
            errors.push(format!("cascade budget ({:?}) exceeded", total_budget));
            break;
        };

        let visitor = if spec.use_visitor {
            if let Some(cached) = visitor_by_origin.get(step_origin) {
                cached.clone()
            } else {
                let fetched = fetch_or_cached_visitor_data(
                    client,
                    step_origin,
                    rem_visitor.min(VISITOR_HTTP_TIMEOUT),
                )
                .await;
                let geo_opt = decode_visitor_data_geo(&fetched);
                log::info!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} origin={origin_label} visitor_len={} geo={}",
                    fetched.len(),
                    geo_opt.as_deref().unwrap_or("unknown"),
                );
                if detected_geo.is_none()
                    && let Some(cc) = geo_opt
                {
                    detected_geo = Some(cc);
                }
                visitor_by_origin.insert(step_origin.to_owned(), fetched.clone());
                fetched
            }
        } else {
            String::new()
        };
        let geo_label = detected_geo.as_deref().unwrap_or("unknown");

        let Some(rem_after_visitor) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} exceeded cascade budget ({:?})",
                total_budget
            );
            errors.push(format!("cascade budget ({:?}) exceeded", total_budget));
            break;
        };
        let req_timeout = rem_after_visitor.min(PLAYER_HTTP_TIMEOUT);

        let player_url = format!("{step_origin}/youtubei/v1/player?prettyPrint=false");
        let ctx_val = (spec.build_context)(if spec.use_visitor { &visitor } else { "" });
        let mut body = json!({
            "context": ctx_val,
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true
        });
        if step.send_auth
            && let Some(ref bundle) = tv_bundle
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert(
                "playbackContext".to_owned(),
                json!({
                    "contentPlaybackContext": {
                        "signatureTimestamp": bundle.sts
                    }
                }),
            );
        }

        let mut req = client
            .post(&player_url)
            .header("content-type", "application/json")
            .header("user-agent", spec.user_agent)
            .header("origin", step_origin)
            .header("x-origin", step_origin)
            .header("x-youtube-client-name", spec.client_id)
            .header("x-youtube-client-version", spec.client_version)
            .json(&body)
            .timeout(req_timeout);

        if spec.use_visitor && !visitor.is_empty() {
            req = req.header("x-goog-visitor-id", &visitor);
        }
        if step.send_auth
            && let Some(c) = trimmed_cookie
        {
            req = req.header("cookie", c);
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Some(auth) = compute_sapisidhash(c, step_origin, now_secs) {
                req = req.header("authorization", auth);
                req = req.header("x-goog-authuser", "0");
            }
        }

        let resp = match tokio::time::timeout(req_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} request error: {e}"
                );
                errors.push(format!("{client_tag}: {e}"));
                continue;
            }
            Err(_) => {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} request timed out after {:?}",
                    req_timeout
                );
                errors.push(format!("{client_tag}: request timed out"));
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} player HTTP {status}"
            );
            errors.push(format!("{client_tag}: HTTP {status}"));
            continue;
        }
        let json_timeout = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default()
            .min(PLAYER_HTTP_TIMEOUT);
        let payload: Value = match tokio::time::timeout(json_timeout, resp.json()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} invalid JSON: {e}"
                );
                errors.push(format!("{client_tag}: invalid JSON ({e})"));
                continue;
            }
            Err(_) => {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} body timed out"
                );
                errors.push(format!("{client_tag}: body timed out"));
                continue;
            }
        };

        let mut raw_candidates = match collect_audio_candidates(
            &payload,
            video_id,
            spec.user_agent,
            format_expr,
            prefer_mobile_aac,
        ) {
            Ok(list) => list,
            Err(SelectionError::Playability { status, reason }) => {
                let msg = if reason.is_empty() {
                    format!("video {status}")
                } else {
                    reason
                };
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} playability={status}: {msg}"
                );
                last_playability_reason = Some(msg.clone());
                errors.push(format!("{client_tag}: {msg}"));
                continue;
            }
            Err(SelectionError::NoFormat(why)) => {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} no audio candidates: {why}"
                );
                errors.push(format!("{client_tag}: {why}"));
                continue;
            }
        };

        // For HLS candidates (e.g. `WEB_SAFARI`), expand master playlist into the best audio-bearing
        // variant URL before batch EJS solving so `/n/<challenge>/` on the variant playlist is solved too.
        for cand in &mut raw_candidates {
            if cand.stream.is_hls && (spec.is_web_client || verify_stream_url) {
                let Some(hls_rem) = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                else {
                    break;
                };
                let hls_timeout = hls_rem.min(PREFLIGHT_HTTP_TIMEOUT);
                if let Ok(text) = fetch_text_prefix(
                    client,
                    &cand.stream.url,
                    &cand.stream.user_agent,
                    hls_timeout,
                    256 * 1024,
                )
                .await
                    && let Some(variant_url) =
                        select_best_youtube_hls_variant(&cand.stream.url, &text)
                {
                    cand.stream.url = variant_url;
                }
            }
        }

        // Collect all unique `s` (signatureCipher) and `n` challenges across candidates.
        let (sig_challenges, n_challenges) = collect_unique_challenges(&raw_candidates);
        let solved = if !sig_challenges.is_empty() || !n_challenges.is_empty() {
            if tv_bundle.is_none() && !tv_bundle_failed {
                let fetch_rem = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default()
                    .min(Duration::from_secs(15));
                if !fetch_rem.is_zero() {
                    match yt_ejs::fetch_or_cached_tv_player(client, cache_dir, fetch_rem).await {
                        Ok(bundle) => tv_bundle = Some(bundle),
                        Err(e) => {
                            log::warn!(
                                target: "nsh_music_plugin",
                                "virtual yt-dlp video={video_id} failed to fetch tv-player-ias.js: {e}"
                            );
                            tv_bundle_failed = true;
                        }
                    }
                }
            }

            if let Some(ref bundle) = tv_bundle {
                let bundle_clone = bundle.clone();
                let sigs_clone = sig_challenges.clone();
                let ns_clone = n_challenges.clone();
                let cache_owned = cache_dir.map(Path::to_path_buf);
                let solve_res = tokio::task::spawn_blocking(move || {
                    yt_ejs::solve_challenges(
                        &bundle_clone,
                        &sigs_clone,
                        &ns_clone,
                        cache_owned.as_deref(),
                    )
                })
                .await
                .map_err(|e| format!("EJS join error: {e}"))
                .and_then(|r| r);

                match solve_res {
                    Ok(res) => {
                        // Refresh local `tv_bundle` to pick up cached `preprocessed_player` on warm steps.
                        if res.cold
                            && let Ok(refreshed) = yt_ejs::fetch_or_cached_tv_player(
                                client,
                                cache_dir,
                                Duration::from_secs(1),
                            )
                            .await
                        {
                            tv_bundle = Some(refreshed);
                        }
                        Some(res)
                    }
                    Err(e) => {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "virtual yt-dlp video={video_id} client={client_tag} EJS solve failed: {e}"
                        );
                        errors.push(format!("{client_tag}: EJS solve failed ({e})"));
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let candidates = finalize_solved_candidates(raw_candidates, solved.as_ref());
        if candidates.is_empty() {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} no usable candidates after challenge solving"
            );
            errors.push(format!(
                "{client_tag}: no usable candidates after challenge solving"
            ));
            continue;
        }

        let total = candidates.len();
        for (idx, candidate) in candidates.into_iter().enumerate() {
            let safe_host = sanitize_url_for_log(&candidate.url);
            if !verify_stream_url {
                log::info!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} resolved format={} ext={} hls={} url={safe_host} (preflight skipped)",
                    candidate.format_id,
                    candidate.ext,
                    candidate.is_hls,
                );
                return Ok(candidate);
            }

            let Some(preflight_rem) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                log::warn!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} exceeded cascade budget ({:?}) before preflight",
                    total_budget
                );
                errors.push(format!("cascade budget ({:?}) exceeded", total_budget));
                break;
            };
            let preflight_timeout = preflight_rem.min(PREFLIGHT_HTTP_TIMEOUT);

            match tokio::time::timeout(
                preflight_timeout,
                verify_candidate_stream_url(client, &candidate, preflight_timeout),
            )
            .await
            {
                Ok(Ok(())) => {
                    log::info!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} resolved candidate={}/{total} format={} ext={} hls={} url={safe_host} (preflight ok)",
                        idx + 1,
                        candidate.format_id,
                        candidate.ext,
                        candidate.is_hls,
                    );
                    return Ok(candidate);
                }
                Ok(Err(why)) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} client={client_tag} rejected candidate={}/{total} format={} ext={} hls={} url={safe_host}: {why}",
                        idx + 1,
                        candidate.format_id,
                        candidate.ext,
                        candidate.is_hls,
                    );
                    errors.push(format!(
                        "{client_tag}[{}]: preflight failed ({why})",
                        candidate.format_id
                    ));
                }
                Err(_) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} client={client_tag} preflight timed out candidate={}/{total} format={} hls={} url={safe_host}",
                        idx + 1,
                        candidate.format_id,
                        candidate.is_hls,
                    );
                    errors.push(format!(
                        "{client_tag}[{}]: preflight timed out",
                        candidate.format_id
                    ));
                }
            }
        }
    }

    if let Some(reason) = last_playability_reason {
        if detected_geo.as_deref() == Some("CN") && !reason.contains("YouTube geo=") {
            Err(format!(
                "{reason} (YouTube geo=CN; switch proxy/VPN region)"
            ))
        } else if let Some(geo) = detected_geo
            && !reason.contains("YouTube geo=")
        {
            Err(format!("{reason} (YouTube geo={geo})"))
        } else {
            Err(reason)
        }
    } else {
        Err(errors.join("; "))
    }
}

async fn fetch_text_prefix(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, String> {
    let mut resp = tokio::time::timeout(
        timeout,
        client
            .get(url)
            .header("user-agent", user_agent)
            .timeout(timeout)
            .send(),
    )
    .await
    .map_err(|_| "HLS playlist request timed out".to_owned())?
    .map_err(|e| format!("HLS request error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HLS HTTP {}", resp.status()));
    }
    let mut buf = Vec::with_capacity(max_bytes.min(8192));
    while buf.len() < max_bytes {
        match tokio::time::timeout(timeout, resp.chunk())
            .await
            .map_err(|_| "HLS read timed out".to_owned())?
            .map_err(|e| format!("HLS read error: {e}"))?
        {
            Some(chunk) => {
                let take = (max_bytes - buf.len()).min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn verify_candidate_stream_url(
    client: &reqwest::Client,
    candidate: &ResolvedAudioStream,
    timeout: Duration,
) -> Result<(), String> {
    if candidate.is_hls {
        let mut playlist_url = candidate.url.clone();
        let mut text =
            fetch_text_prefix(client, &playlist_url, &candidate.user_agent, timeout, 256 * 1024)
                .await?;
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        if !trimmed.starts_with("#EXTM3U") && !trimmed.starts_with("#EXT") {
            return Err("HLS body did not start with #EXTM3U/#EXT".to_owned());
        }

        // If this is a master playlist, follow 1 hop to the selected audio-bearing variant playlist.
        if let Some(variant_url) = select_best_youtube_hls_variant(&playlist_url, &text) {
            playlist_url = variant_url;
            text = fetch_text_prefix(
                client,
                &playlist_url,
                &candidate.user_agent,
                timeout,
                256 * 1024,
            )
            .await?;
            let v_trimmed = text.trim_start_matches('\u{feff}').trim_start();
            if !v_trimmed.starts_with("#EXTM3U") && !v_trimmed.starts_with("#EXT") {
                return Err("HLS variant body did not start with #EXTM3U/#EXT".to_owned());
            }
        }

        // If the media playlist contains absolute `http://`/`https://` segment URLs, verify
        // that fetching the first segment returns 2xx (rejecting 403 segments during preflight).
        if let Some(seg_url) = extract_first_hls_http_segment_url(&text) {
            let mut seg_resp = client
                .get(&seg_url)
                .header("user-agent", &candidate.user_agent)
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| format!("HLS segment request error: {e}"))?;
            if !seg_resp.status().is_success() {
                return Err(format!("HLS segment HTTP {}", seg_resp.status()));
            }
            if let Some(ct) = seg_resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
            {
                let ct_lower = ct.to_ascii_lowercase();
                if ct_lower.contains("text/html") || ct_lower.contains("application/json") {
                    return Err(format!("HLS segment unexpected Content-Type `{ct}`"));
                }
            }
            let mut received = 0usize;
            while let Some(chunk) = seg_resp
                .chunk()
                .await
                .map_err(|e| format!("HLS segment read error: {e}"))?
            {
                received += chunk.len();
                if received >= 1 {
                    break;
                }
            }
            if received == 0 {
                return Err("HLS segment empty body".to_owned());
            }
        }
        return Ok(());
    }

    let mut resp = client
        .get(&candidate.url)
        .header("range", "bytes=0-63")
        .header("user-agent", &candidate.user_agent)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("stream request error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    if let Some(ct) = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let ct_lower = ct.to_ascii_lowercase();
        if ct_lower.contains("text/html") || ct_lower.contains("application/json") {
            return Err(format!("unexpected Content-Type `{ct}`"));
        }
    }
    let mut received = 0usize;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("stream read error: {e}"))?
    {
        received += chunk.len();
        if received >= 1 {
            break;
        }
    }
    if received == 0 {
        return Err("empty stream body".to_owned());
    }
    Ok(())
}

enum SelectionError {
    Playability { status: String, reason: String },
    NoFormat(String),
}

fn extract_yt_text(val: &Value) -> Option<String> {
    if let Some(s) = val.get("simpleText").and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    if let Some(runs) = val.get("runs").and_then(|v| v.as_array()) {
        let joined: String = runs
            .iter()
            .filter_map(|r| r.get("text").and_then(|t| t.as_str()))
            .collect();
        let trimmed = joined.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

fn extract_playability_reason(payload: &Value) -> String {
    let mut reason = payload
        .pointer("/playabilityStatus/reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    if let Some(sub) = payload
        .pointer("/playabilityStatus/errorScreen/playerErrorMessageRenderer/subreason")
        .and_then(extract_yt_text)
    {
        if reason.is_empty() {
            reason = sub;
        } else if !reason
            .to_ascii_lowercase()
            .contains(&sub.to_ascii_lowercase())
        {
            reason = format!("{reason} ({sub})");
        }
    }
    reason
}

#[derive(Debug, Clone)]
struct AudioCandidate {
    stream: ResolvedAudioStream,
    cipher_s: Option<String>,
    cipher_sp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedFormatUrl {
    base_url: String,
    s: Option<String>,
    sp: String,
}

fn parse_format_url(fmt: &Value) -> Option<ParsedFormatUrl> {
    if let Some(url) = fmt
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(ParsedFormatUrl {
            base_url: url.to_owned(),
            s: None,
            sp: "signature".to_owned(),
        });
    }
    let sc = fmt
        .get("signatureCipher")
        .or_else(|| fmt.get("cipher"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let mut base_url: Option<String> = None;
    let mut s_val: Option<String> = None;
    let mut sp_val = "signature".to_owned();
    for (k, v) in url::form_urlencoded::parse(sc.as_bytes()) {
        if k == "url" && !v.is_empty() {
            base_url = Some(v.into_owned());
        } else if k == "s" && !v.is_empty() {
            s_val = Some(v.into_owned());
        } else if k == "sp" && !v.is_empty() {
            sp_val = v.into_owned();
        }
    }
    Some(ParsedFormatUrl {
        base_url: base_url?,
        s: s_val,
        sp: sp_val,
    })
}

fn percent_encode_component(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 8);
    for b in val.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn extract_query_n_param(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "n")
        .map(|(_, v)| v.into_owned())
        .filter(|s| !s.is_empty())
}

fn extract_path_n_param(raw_url: &str) -> Option<String> {
    let before_query = raw_url.split('?').next().unwrap_or(raw_url);
    let pos = before_query.find("/n/")?;
    let rest = &before_query[pos + 3..];
    let seg = rest.split('/').next().unwrap_or(rest);
    if seg.is_empty() {
        return None;
    }
    percent_decode_ascii(seg).or_else(|| Some(seg.to_owned()))
}

fn replace_query_param_preserve_order(raw_url: &str, key: &str, new_val: &str) -> String {
    let encoded = percent_encode_component(new_val);
    let Some(q_idx) = raw_url.find('?') else {
        return format!("{raw_url}?{key}={encoded}");
    };
    let prefix = &raw_url[..=q_idx];
    let query = &raw_url[q_idx + 1..];
    let mut replaced = false;
    let mut out = String::with_capacity(raw_url.len() + encoded.len());
    out.push_str(prefix);
    for (idx, part) in query.split('&').enumerate() {
        if idx > 0 {
            out.push('&');
        }
        let part_key = part.split_once('=').map_or(part, |(k, _)| k);
        if part_key == key {
            out.push_str(key);
            out.push('=');
            out.push_str(&encoded);
            replaced = true;
        } else {
            out.push_str(part);
        }
    }
    if !replaced {
        if !query.is_empty() {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(&encoded);
    }
    out
}

fn replace_path_n_param(raw_url: &str, new_n: &str) -> String {
    let (path_part, query_part) = match raw_url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (raw_url, None),
    };
    let Some(pos) = path_part.find("/n/") else {
        return raw_url.to_owned();
    };
    let after = &path_part[pos + 3..];
    let rest = match after.find('/') {
        Some(slash) => &after[slash..],
        None => "",
    };
    let encoded = percent_encode_component(new_n);
    match query_part {
        Some(q) => format!("{}/n/{encoded}{rest}?{q}", &path_part[..pos]),
        None => format!("{}/n/{encoded}{rest}", &path_part[..pos]),
    }
}

fn apply_solved_sig_and_n(
    base_url: &str,
    cipher_s: Option<&str>,
    cipher_sp: &str,
    solved_sigs: &HashMap<String, String>,
    solved_ns: &HashMap<String, String>,
) -> Option<String> {
    let mut url = base_url.to_owned();
    if let Some(orig_s) = cipher_s {
        let solved_s = solved_sigs.get(orig_s)?;
        url = replace_query_param_preserve_order(&url, cipher_sp, solved_s);
    }
    if let Some(orig_n) = extract_query_n_param(&url)
        && let Some(solved_n) = solved_ns.get(&orig_n)
    {
        url = replace_query_param_preserve_order(&url, "n", solved_n);
    }
    if let Some(orig_path_n) = extract_path_n_param(&url)
        && let Some(solved_n) = solved_ns.get(&orig_path_n)
    {
        url = replace_path_n_param(&url, solved_n);
    }
    Some(url)
}

fn collect_unique_challenges(candidates: &[AudioCandidate]) -> (Vec<String>, Vec<String>) {
    let mut sig_seen = HashSet::new();
    let mut sig_list = Vec::new();
    let mut n_seen = HashSet::new();
    let mut n_list = Vec::new();

    for cand in candidates {
        if let Some(ref s) = cand.cipher_s
            && sig_seen.insert(s.clone())
        {
            sig_list.push(s.clone());
        }
        if let Some(n) = extract_query_n_param(&cand.stream.url)
            && n_seen.insert(n.clone())
        {
            n_list.push(n);
        }
        if let Some(pn) = extract_path_n_param(&cand.stream.url)
            && n_seen.insert(pn.clone())
        {
            n_list.push(pn);
        }
    }
    (sig_list, n_list)
}

fn finalize_solved_candidates(
    raw_candidates: Vec<AudioCandidate>,
    solved: Option<&yt_ejs::EjsSolveResult>,
) -> Vec<ResolvedAudioStream> {
    let empty_map = HashMap::new();
    let sigs = solved.map_or(&empty_map, |r| &r.sigs);
    let ns = solved.map_or(&empty_map, |r| &r.ns);

    let mut out = Vec::with_capacity(raw_candidates.len());
    for mut cand in raw_candidates {
        if let Some(final_url) = apply_solved_sig_and_n(
            &cand.stream.url,
            cand.cipher_s.as_deref(),
            &cand.cipher_sp,
            sigs,
            ns,
        ) {
            cand.stream.url = final_url;
            out.push(cand.stream);
        }
    }
    out
}

fn parse_hls_attr(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let pos = line.find(&needle)?;
    let rest = &line[pos + needle.len()..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_owned())
    } else {
        let end = rest.find(',').unwrap_or(rest.len());
        Some(rest[..end].trim().to_owned())
    }
}

fn resolve_hls_uri(base_url: &str, uri: &str) -> String {
    let trimmed = uri.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_owned();
    }
    url::Url::parse(base_url)
        .and_then(|b| b.join(trimmed))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| trimmed.to_owned())
}

fn extract_itag_from_url(raw_url: &str) -> Option<u32> {
    let before_query = raw_url.split('?').next().unwrap_or(raw_url);
    if let Some(pos) = before_query.find("/itag/") {
        let rest = &before_query[pos + 6..];
        let seg = rest.split('/').next().unwrap_or(rest);
        if let Ok(n) = seg.parse::<u32>() {
            return Some(n);
        }
    }
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "itag")
        .and_then(|(_, v)| v.parse::<u32>().ok())
}

/// Picks the best audio-bearing variant URL from a YouTube HLS master playlist:
/// 1. `#EXT-X-MEDIA:TYPE=AUDIO` with `URI=` (`mp4a.40.2` > other audio).
/// 2. Muxed `#EXT-X-STREAM-INF` preferring `itag=93 -> 94 -> 92 -> 95 -> 91` / `mp4a.40.2` > lowest bandwidth.
///
/// Returns `None` if `playlist_text` is already a media playlist.
fn select_best_youtube_hls_variant(master_url: &str, playlist_text: &str) -> Option<String> {
    let lines: Vec<&str> = playlist_text.lines().map(str::trim).collect();

    // Map AUDIO group-id -> codecs from #EXT-X-STREAM-INF to identify AAC-LC audio renditions.
    let mut group_codecs: HashMap<String, String> = HashMap::new();
    for line in &lines {
        if line.starts_with("#EXT-X-STREAM-INF:")
            && let Some(group) = parse_hls_attr(line, "AUDIO")
            && let Some(codecs) = parse_hls_attr(line, "CODECS")
        {
            group_codecs.insert(group, codecs);
        }
    }

    // 1. Prefer dedicated `#EXT-X-MEDIA:TYPE=AUDIO` renditions (`mp4a.40.2` > other audio).
    let mut audio_renditions: Vec<(bool, bool, String)> = Vec::new();
    for line in &lines {
        if line.starts_with("#EXT-X-MEDIA:")
            && parse_hls_attr(line, "TYPE")
                .is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"))
            && let Some(uri) = parse_hls_attr(line, "URI").filter(|u| !u.is_empty())
        {
            let resolved = resolve_hls_uri(master_url, &uri);
            let group_codec = parse_hls_attr(line, "GROUP-ID")
                .and_then(|g| group_codecs.get(&g))
                .map(|c| c.to_ascii_lowercase())
                .unwrap_or_default();
            let itag = extract_itag_from_url(&resolved).unwrap_or(0);
            let is_aac_lc = group_codec.contains("mp4a.40.2")
                || matches!(itag, 140 | 141 | 233 | 234);
            let is_default = parse_hls_attr(line, "DEFAULT")
                .is_some_and(|d| d.eq_ignore_ascii_case("YES"));
            audio_renditions.push((is_aac_lc, is_default, resolved));
        }
    }
    if !audio_renditions.is_empty() {
        audio_renditions.sort_by_key(|a| std::cmp::Reverse((a.0, a.1)));
        return Some(audio_renditions.swap_remove(0).2);
    }

    // 2. Fallback to muxed `#EXT-X-STREAM-INF` (`itag=93 -> 94 -> 92 -> 95 -> 91` / `mp4a.40.2` > lowest bw).
    let mut stream_variants: Vec<(u8, bool, std::cmp::Reverse<u64>, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let codecs = parse_hls_attr(line, "CODECS")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_aac_lc = codecs.contains("mp4a.40.2");
            let bandwidth = parse_hls_attr(line, "BANDWIDTH")
                .and_then(|b| b.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            let mut j = i + 1;
            while j < lines.len() && (lines[j].is_empty() || lines[j].starts_with('#')) {
                j += 1;
            }
            if j < lines.len() {
                let resolved = resolve_hls_uri(master_url, lines[j]);
                let itag = extract_itag_from_url(&resolved).unwrap_or(0);
                let itag_rank = match itag {
                    93 => 5,
                    94 => 4,
                    92 => 3,
                    95 => 2,
                    91 => 1,
                    _ => 0,
                };
                stream_variants.push((itag_rank, is_aac_lc, std::cmp::Reverse(bandwidth), resolved));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    if !stream_variants.is_empty() {
        stream_variants.sort_by_key(|v| std::cmp::Reverse((v.0, v.1, v.2)));
        return Some(stream_variants.swap_remove(0).3);
    }

    None
}

fn extract_first_hls_http_segment_url(playlist_text: &str) -> Option<String> {
    for raw in playlist_text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("http://") || line.starts_with("https://") {
            return Some(line.to_owned());
        }
        // First non-comment segment line is relative (not an http/https URL).
        return None;
    }
    None
}

fn collect_audio_candidates(
    payload: &Value,
    video_id: &str,
    user_agent: &str,
    format_expr: Option<&str>,
    prefer_mobile_aac: bool,
) -> Result<Vec<AudioCandidate>, SelectionError> {
    let status = payload
        .pointer("/playabilityStatus/status")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if status != "OK" {
        let reason = extract_playability_reason(payload);
        return Err(SelectionError::Playability {
            status: status.to_owned(),
            reason,
        });
    }

    let title = payload
        .pointer("/videoDetails/title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let duration_secs = payload
        .pointer("/videoDetails/lengthSeconds")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok());

    let fmt_lower = format_expr.unwrap_or_default().to_ascii_lowercase();
    let excludes_m3u8 = fmt_lower.contains("!=m3u8") || fmt_lower.contains("!*m3u8");
    let wants_m3u8 = !excludes_m3u8 && (fmt_lower.contains("m3u8") || fmt_lower.contains("hls"));
    let wants_m4a =
        fmt_lower.contains("m4a") || fmt_lower.contains("mp4") || fmt_lower.contains("aac");
    let wants_webm = fmt_lower.contains("webm") || fmt_lower.contains("opus");
    let prefer_mp4 = wants_m4a || (prefer_mobile_aac && !wants_webm);

    let mut raw_formats: Vec<&Value> = Vec::new();
    if let Some(arr) = payload
        .pointer("/streamingData/adaptiveFormats")
        .and_then(|v| v.as_array())
    {
        raw_formats.extend(arr.iter());
    }
    if let Some(arr) = payload
        .pointer("/streamingData/formats")
        .and_then(|v| v.as_array())
    {
        raw_formats.extend(arr.iter());
    }

    let mut preferred: Vec<(u8, u64, AudioCandidate)> = Vec::new();
    let mut fallback: Vec<(u64, AudioCandidate)> = Vec::new();

    for fmt in raw_formats {
        let Some(parsed_url) = parse_format_url(fmt) else {
            continue;
        };
        let mime = fmt
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let mime_lower = mime.to_ascii_lowercase();
        if !mime_lower.starts_with("audio/") {
            continue;
        }
        if fmt.get("isDrc").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(track) = fmt.get("audioTrack")
            && track.get("audioIsDefault").and_then(|v| v.as_bool()) == Some(false)
        {
            continue;
        }

        let bitrate = fmt
            .get("bitrate")
            .and_then(|v| v.as_u64())
            .or_else(|| fmt.get("averageBitrate").and_then(|v| v.as_u64()))
            .unwrap_or(0);
        let itag = fmt.get("itag").and_then(|v| v.as_u64()).unwrap_or(0);
        let (ext, acodec) = infer_ext_and_codec(&mime_lower);

        let candidate = AudioCandidate {
            stream: ResolvedAudioStream {
                video_id: video_id.to_owned(),
                title: title.clone(),
                url: parsed_url.base_url,
                mime_type: mime.to_owned(),
                ext,
                acodec,
                format_id: itag.to_string(),
                bitrate,
                duration_secs,
                user_agent: user_agent.to_owned(),
                is_hls: false,
            },
            cipher_s: parsed_url.s,
            cipher_sp: parsed_url.sp,
        };

        let is_mp4 = mime_lower.starts_with("audio/mp4");
        let is_webm = mime_lower.starts_with("audio/webm");

        if prefer_mp4 && is_mp4 {
            let tier = if mime_lower.contains("mp4a") || matches!(itag, 139..=141) {
                match itag {
                    141 => 4,
                    140 => 3,
                    139 => 1,
                    _ => 2,
                }
            } else {
                0
            };
            preferred.push((tier, bitrate, candidate.clone()));
        } else if wants_webm && is_webm {
            preferred.push((1, bitrate, candidate.clone()));
        }

        fallback.push((bitrate, candidate));
    }

    preferred.sort_by_key(|a| std::cmp::Reverse((a.0, a.1)));
    fallback.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut ordered: Vec<AudioCandidate> = Vec::new();
    let mut seen_urls = HashSet::new();

    for (_, _, cand) in preferred {
        if seen_urls.insert(cand.stream.url.clone()) {
            ordered.push(cand);
        }
    }
    for (_, cand) in fallback {
        if seen_urls.insert(cand.stream.url.clone()) {
            ordered.push(cand);
        }
    }

    if let Some(hls) = payload
        .pointer("/streamingData/hlsManifestUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        && seen_urls.insert(hls.to_owned())
    {
        ordered.push(AudioCandidate {
            stream: ResolvedAudioStream {
                video_id: video_id.to_owned(),
                title,
                url: hls.to_owned(),
                mime_type: "application/vnd.apple.mpegurl".to_owned(),
                ext: "m3u8".to_owned(),
                acodec: if wants_m3u8 { "aac" } else { "unknown" }.to_owned(),
                format_id: "hls".to_owned(),
                bitrate: 0,
                duration_secs,
                user_agent: user_agent.to_owned(),
                is_hls: true,
            },
            cipher_s: None,
            cipher_sp: "signature".to_owned(),
        });
    }

    if ordered.is_empty() {
        return Err(SelectionError::NoFormat(
            "no direct audio stream listed in streamingData".to_owned(),
        ));
    }

    Ok(ordered)
}

fn infer_ext_and_codec(mime_lower: &str) -> (String, String) {
    let ext = if mime_lower.starts_with("audio/mp4") {
        "m4a"
    } else if mime_lower.starts_with("audio/webm") {
        "webm"
    } else if mime_lower.starts_with("audio/mpeg") {
        "mp3"
    } else {
        "audio"
    };
    let acodec = if mime_lower.contains("opus") {
        "opus"
    } else if mime_lower.contains("mp4a") {
        "mp4a.40.2"
    } else if mime_lower.contains("vorbis") {
        "vorbis"
    } else {
        "unknown"
    };
    (ext.to_owned(), acodec.to_owned())
}

/// Pure-Rust in-process `yt-dlp` runtime engine.
pub struct VirtualYtdlpShim;

impl VirtualYtdlpShim {
    /// Spawns a virtual `yt-dlp` process backed by `http_client` against `base_origin`
    /// (typically `"https://music.youtube.com"`).
    pub fn spawn_with_origin(
        args: &[String],
        http_client: reqwest::Client,
        base_origin: String,
        prefer_mobile_aac: bool,
    ) -> Arc<VirtualPipeHandle> {
        Self::spawn_with_origin_and_cache(args, http_client, base_origin, prefer_mobile_aac, None)
    }

    /// Spawns a virtual `yt-dlp` process with an optional `cache_dir` for persisting
    /// preprocessed `tv-player-ias.js` across cold starts.
    pub fn spawn_with_origin_and_cache(
        args: &[String],
        http_client: reqwest::Client,
        base_origin: String,
        prefer_mobile_aac: bool,
        cache_dir: Option<PathBuf>,
    ) -> Arc<VirtualPipeHandle> {
        let initial = parse_ytdlp_args(args);
        if initial.is_version {
            return VirtualPipeHandle::completed(
                VIRTUAL_YTDLP_VERSION_BANNER.to_owned(),
                String::new(),
                0,
            );
        }

        let pipe = VirtualPipeHandle::running();
        let pipe_bg = Arc::clone(&pipe);

        let _ = std::thread::Builder::new()
            .name("nsh-virtual-ytdlp".to_owned())
            .spawn(move || {
                let mut parsed = initial;
                if parsed.reads_stdin_config {
                    let Some(stdin_text) = pipe_bg.wait_for_stdin(Duration::from_secs(15)) else {
                        if !pipe_bg.is_killed() {
                            pipe_bg.push_stderr(
                                "ERROR: [youtube] stdin config stream was interrupted",
                            );
                            pipe_bg.finish(1);
                        }
                        return;
                    };
                    merge_ytdlp_stdin_config(&mut parsed, &stdin_text);
                }

                if pipe_bg.is_killed() {
                    return;
                }

                let Some(target) = parsed.target.as_deref() else {
                    pipe_bg.push_stderr("ERROR: [youtube] You must provide at least one URL");
                    pipe_bg.finish(1);
                    return;
                };

                let Some(video_id) = extract_youtube_video_id(target) else {
                    pipe_bg.push_stderr(&format!("ERROR: [generic] Unsupported URL: {target}"));
                    pipe_bg.finish(1);
                    return;
                };

                let cookie = find_header(&parsed.headers, "Cookie")
                    .map(str::to_owned)
                    .or_else(|| {
                        parsed
                            .cookies_file
                            .as_deref()
                            .and_then(read_netscape_cookies_file)
                    });

                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        pipe_bg.push_stderr(&format!(
                            "ERROR: [youtube] {video_id}: runtime init failed: {e}"
                        ));
                        pipe_bg.finish(1);
                        return;
                    }
                };

                let verify = !is_loopback_origin(&base_origin);
                let resolved = rt.block_on(resolve_innertube_stream_with_cache(
                    &http_client,
                    &base_origin,
                    &video_id,
                    cookie.as_deref(),
                    parsed.format_expr.as_deref(),
                    prefer_mobile_aac,
                    verify,
                    cache_dir.as_deref(),
                ));

                if pipe_bg.is_killed() {
                    return;
                }

                match resolved {
                    Ok(stream) => {
                        let line = format_ytdlp_output(&stream, &parsed);
                        pipe_bg.push_stdout_line(&line);
                        pipe_bg.finish(0);
                    }
                    Err(reason) => {
                        pipe_bg.push_stderr(&format!("ERROR: [youtube] {video_id}: {reason}"));
                        pipe_bg.finish(1);
                    }
                }
            });

        pipe
    }
}

fn format_ytdlp_output(stream: &ResolvedAudioStream, parsed: &YtdlpParsedArgs) -> String {
    let mut http_headers = serde_json::Map::new();
    // Always advertise the winning InnerTube client's User-Agent so googlevideo.com
    // accepts the stream URL even when the plugin passed a desktop browser UA on stdin.
    let ua = if stream.user_agent.is_empty() {
        find_header(&parsed.headers, "User-Agent").unwrap_or(WEB_REMIX_UA)
    } else {
        &stream.user_agent
    };
    http_headers.insert("User-Agent".to_owned(), json!(ua));
    for (k, v) in &parsed.headers {
        if !k.eq_ignore_ascii_case("user-agent") {
            http_headers.insert(k.clone(), json!(v));
        }
    }

    match parsed.output_mode {
        YtdlpOutputMode::PrintUrlAndHeadersJson => json!({
            "url": stream.url,
            "http_headers": http_headers
        })
        .to_string(),
        YtdlpOutputMode::PrintUrl => stream.url.clone(),
        YtdlpOutputMode::DumpJson => json!({
            "id": stream.video_id,
            "title": stream.title,
            "url": stream.url,
            "ext": stream.ext,
            "acodec": stream.acodec,
            "format_id": stream.format_id,
            "abr": stream.bitrate / 1000,
            "duration": stream.duration_secs,
            "protocol": if stream.is_hls { "m3u8_native" } else { "https" },
            "http_headers": http_headers
        })
        .to_string(),
    }
}

/// [`VirtualProcessShim`] intercepting `yt-dlp` invocations.
///
/// - On Desktop (`!ctx.is_mobile() && ctx.is_granted() && ctx.manifest.allows_process(...)`),
///   prefers the real system `yt-dlp` binary when installed in augmented `PATH`.
/// - When real `yt-dlp` is absent OR on Mobile (iOS / Android / HarmonyOS), runs
///   [`VirtualYtdlpShim`] in-process without spawning external Python/yt-dlp binaries.
pub struct YtdlpVirtualShim;

impl VirtualProcessShim for YtdlpVirtualShim {
    fn name(&self) -> &'static str {
        "yt-dlp"
    }

    fn matches(&self, cmd: &str) -> bool {
        is_ytdlp_command(cmd)
    }

    fn spawn(
        &self,
        cmd: &str,
        args: &[String],
        opts: &SpawnOptions,
        ctx: &HostContext,
    ) -> VirtualSpawnResult {
        if !ctx.is_mobile() && ctx.is_granted() && ctx.manifest.allows_process(&ctx.plugin_dir, cmd)
        {
            let resolved_cmd = resolve_relative_command(&ctx.plugin_dir, cmd);
            if let Some(real_ytdlp) = resolve_real_binary(&resolved_cmd, "yt-dlp") {
                let real_str = real_ytdlp.to_string_lossy().into_owned();
                let effective_opts = with_default_cwd(opts, &ctx.plugin_dir);
                match ExternalProcessHandle::spawn(&real_str, args, &effective_opts) {
                    Ok(handle) => {
                        return Ok(Some((
                            false,
                            Arc::new(ManagedProcess::from_external(handle)),
                        )));
                    }
                    Err(err) => {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "real yt-dlp spawn failed ({err}); falling back to VirtualYtdlpShim"
                        );
                    }
                }
            }
        }

        let pipe = VirtualYtdlpShim::spawn_with_origin_and_cache(
            args,
            ctx.http_client().clone(),
            DEFAULT_INNERTUBE_ORIGIN.to_owned(),
            ctx.is_mobile(),
            Some(ctx.data_dir().to_path_buf()),
        );
        Ok(Some((
            true,
            Arc::new(ManagedProcess::from_virtual_pipe(pipe)),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn matches_ytdlp_commands_and_extracts_video_ids() {
        assert!(is_ytdlp_command("yt-dlp"));
        assert!(is_ytdlp_command("YT-DLP.EXE"));
        assert!(is_ytdlp_command("/opt/homebrew/bin/yt-dlp"));
        assert!(is_ytdlp_command("/usr/local/bin/yt-dlp"));
        assert!(is_ytdlp_command(r"C:\bin\yt-dlp.exe"));
        assert!(!is_ytdlp_command("ffmpeg"));
        assert!(!is_ytdlp_command("python3"));

        assert_eq!(
            extract_youtube_video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(
            extract_youtube_video_id("https://www.youtube.com/watch?v=abc123XYZ09&list=RD1"),
            Some("abc123XYZ09".to_owned())
        );
        assert_eq!(
            extract_youtube_video_id("https://youtu.be/xyz98765432?t=12"),
            Some("xyz98765432".to_owned())
        );
        assert_eq!(
            extract_youtube_video_id("https://www.youtube.com/shorts/shortId1234"),
            Some("shortId1234".to_owned())
        );
        assert_eq!(
            extract_youtube_video_id("dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(
            extract_youtube_video_id("https://example.com/watch?v=dQw4w9WgXcQ"),
            None
        );
    }

    #[test]
    fn computes_rfc3174_sha1_and_sapisidhash() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");

        let cookie = "PREF=tz=Asia.Shanghai; SAPISID=abc/123; __Secure-3PAPISID=sec/999";
        let expected_hash = sha1_hex(b"1700000000 sec/999 https://music.youtube.com");
        assert_eq!(
            compute_sapisidhash(cookie, "https://music.youtube.com", 1_700_000_000),
            Some(format!("SAPISIDHASH 1700000000_{expected_hash}"))
        );

        let fallback_cookie = "SAPISID=only_plain";
        let expected_plain = sha1_hex(b"1700000000 only_plain https://music.youtube.com");
        assert_eq!(
            compute_sapisidhash(fallback_cookie, "https://music.youtube.com", 1_700_000_000),
            Some(format!("SAPISIDHASH 1700000000_{expected_plain}"))
        );
    }

    #[test]
    fn parses_version_probe_and_stdin_config_cookies() {
        let ver_pipe = VirtualYtdlpShim::spawn_with_origin(
            &["--version".to_owned()],
            reqwest::Client::new(),
            DEFAULT_INNERTUBE_ORIGIN.to_owned(),
            true,
        );
        assert_eq!(ver_pipe.wait(), 0);
        assert_eq!(ver_pipe.stdout(), VIRTUAL_YTDLP_VERSION_BANNER);
        assert_eq!(
            ver_pipe.read_line_blocking().as_deref(),
            Some("2026.03.01 (nsh-virtual-ytdlp)")
        );
        assert_eq!(ver_pipe.read_line_blocking(), None);

        let cli: Vec<String> = [
            "--config-locations",
            "-",
            "--no-warnings",
            "--format",
            "bestaudio[protocol^=http]/bestaudio",
            "--print",
            "%(.{url,http_headers})j",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut parsed = parse_ytdlp_args(&cli);
        assert!(parsed.reads_stdin_config);
        assert_eq!(parsed.output_mode, YtdlpOutputMode::PrintUrlAndHeadersJson);
        assert_eq!(
            parsed.format_expr.as_deref(),
            Some("bestaudio[protocol^=http]/bestaudio")
        );

        merge_ytdlp_stdin_config(
            &mut parsed,
            r#"--add-headers "Cookie:SAPISID=sap1; __Secure-3PSID=\"quoted\""
--user-agent "CustomUA/2.0"
"#,
        );
        assert_eq!(
            find_header(&parsed.headers, "Cookie"),
            Some(r#"SAPISID=sap1; __Secure-3PSID="quoted""#)
        );
        assert_eq!(
            find_header(&parsed.headers, "User-Agent"),
            Some("CustomUA/2.0")
        );
    }

    #[test]
    fn mock_innertube_cascade_emits_print_json_without_mobile_auth_and_error_stderr() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock innertube");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let saw_auth_or_cookie_on_player = Arc::new(AtomicBool::new(false));
        let saw_auth_srv = Arc::clone(&saw_auth_or_cookie_on_player);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let req_lower = req.to_ascii_lowercase();
                        if req.contains("/youtubei/v1/player")
                            && (req_lower.contains("\r\nauthorization:")
                                || req_lower.contains("\r\ncookie:"))
                        {
                            saw_auth_srv.store(true, Ordering::SeqCst);
                        }

                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"MOCK_VISITOR_42"}}"#.to_owned()
                        } else if req.contains("FAILVID0001") {
                            r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm you're not a bot"}}"#.to_owned()
                        } else {
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Mock Track", "lengthSeconds": "210" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 251,
                                            "mimeType": "audio/webm; codecs=\"opus\"",
                                            "bitrate": 160000,
                                            "url": "https://cdn.example.test/opus251.webm"
                                        },
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": "https://cdn.example.test/aac140.m4a"
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        };

                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let origin = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // 1. Mobile mode prefers Symphonia-playable `audio/mp4` AAC-LC, keeps stdin `Cookie` in
        //    emitted `http_headers`, and NEVER sends browser `Cookie` / `SAPISIDHASH` to mobile `/player`.
        let args_ok: Vec<String> = [
            "--config-locations",
            "-",
            "--print",
            "%(.{url,http_headers})j",
            "https://www.youtube.com/watch?v=OKCOOKIE001",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let pipe =
            VirtualYtdlpShim::spawn_with_origin(&args_ok, client.clone(), origin.clone(), true);
        pipe.write_stdin("--add-headers \"Cookie:SAPISID=test_sap_value\"\n")
            .unwrap();
        pipe.close_stdin();

        let line = pipe.read_line_blocking().expect("json output line");
        assert_eq!(pipe.read_line_blocking(), None);
        assert_eq!(pipe.wait(), 0);
        assert!(
            !saw_auth_or_cookie_on_player.load(Ordering::SeqCst),
            "native mobile InnerTube /player requests must NOT send Cookie or SAPISIDHASH"
        );
        let parsed: Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["url"], "https://cdn.example.test/aac140.m4a");
        assert_eq!(parsed["http_headers"]["Cookie"], "SAPISID=test_sap_value");
        assert!(parsed["http_headers"]["User-Agent"].as_str().is_some());

        // 2. `-g` emits raw URL directly.
        let args_g: Vec<String> = ["-g", "https://music.youtube.com/watch?v=OKTRACK0001"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let pipe_g =
            VirtualYtdlpShim::spawn_with_origin(&args_g, client.clone(), origin.clone(), true);
        assert_eq!(
            pipe_g.read_line_blocking().as_deref(),
            Some("https://cdn.example.test/aac140.m4a")
        );
        assert_eq!(pipe_g.wait(), 0);

        // 3. Failure emits yt-dlp-style `ERROR: [youtube] <id>: ...` on stderr and exits 1.
        let args_fail: Vec<String> = [
            "--print",
            "%(.{url,http_headers})j",
            "https://www.youtube.com/watch?v=FAILVID0001",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let pipe_fail = VirtualYtdlpShim::spawn_with_origin(&args_fail, client, origin, true);
        assert_eq!(pipe_fail.read_line_blocking(), None);
        assert_eq!(pipe_fail.wait(), 1);
        let err_out = pipe_fail.error_output();
        assert!(
            err_out.contains("ERROR: [youtube] FAILVID0001:") && err_out.contains("not a bot"),
            "unexpected stderr: {err_out}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn preflight_skips_403_stream_and_falls_back_to_working_client() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock preflight server");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let origin_srv = origin.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let saw_range_206 = Arc::new(AtomicBool::new(false));
        let saw_range_srv = Arc::clone(&saw_range_206);
        let client_order = Arc::new(Mutex::new(Vec::<String>::new()));
        let client_order_srv = Arc::clone(&client_order);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let req_lower = req.to_ascii_lowercase();
                        let first_line = req.lines().next().unwrap_or_default();

                        if first_line.starts_with("GET /stream/403") {
                            let body = "Forbidden";
                            let resp = format!(
                                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /stream/html") {
                            let body = "<html>not audio</html>";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /stream/ok206") {
                            if req_lower.contains("range: bytes=0-63") && req.contains(ANDROID_UA) {
                                saw_range_srv.store(true, Ordering::SeqCst);
                            }
                            let payload = vec![0x41u8; 64];
                            let header = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-63/1024\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                payload.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&payload);
                            continue;
                        }
                        if first_line.starts_with("GET /stream/hls.m3u8") {
                            let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:5.0,\nseg0.aac\n";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }

                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"MOCK_VISITOR_PREFLIGHT"}}"#
                                .to_owned()
                        } else if req.contains("ORDER_TRACK") {
                            let client_name = extract_json_client_name(&req);
                            let tag = if req_lower.contains("authorization: sapisidhash ") {
                                format!("{client_name}:auth")
                            } else {
                                format!("{client_name}:anon")
                            };
                            if let Ok(mut guard) = client_order_srv.lock() {
                                guard.push(tag);
                            }
                            r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"All failed"}}"#
                                .to_owned()
                        } else if req.contains("HLS_FALLBACK") {
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "HLS Track", "lengthSeconds": "120" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/403")
                                        }
                                    ],
                                    "hlsManifestUrl": format!("{origin_srv}/stream/hls.m3u8")
                                }
                            })
                            .to_string()
                        } else if req.contains("\"clientName\":\"VISIONOS\"") {
                            // Client #1 returns a 403 AAC stream and an HTML-spoofed Opus stream.
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Blocked Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/403")
                                        },
                                        {
                                            "itag": 251,
                                            "mimeType": "audio/webm; codecs=\"opus\"",
                                            "bitrate": 160000,
                                            "url": format!("{origin_srv}/stream/html")
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        } else if req.contains("\"clientName\":\"ANDROID\"") {
                            // Client #2 returns a working 206 audio stream.
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Working Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/ok206")
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        } else {
                            r#"{"playabilityStatus":{"status":"UNPLAYABLE","reason":"fallback"}}"#
                                .to_owned()
                        };

                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::new();

        // 1. Client #1 (VISIONOS) returns 403 + text/html -> preflight rejects both and resolves Client #2 (ANDROID) 206 stream.
        let resolved = rt
            .block_on(resolve_innertube_stream_with_options(
                &client,
                &origin,
                "PREFLIGHT01",
                None,
                None,
                true,
                true,
            ))
            .expect("should fall back from 403 client #1 to 206 client #2");
        assert_eq!(resolved.url, format!("{origin}/stream/ok206"));
        assert_eq!(resolved.user_agent, ANDROID_UA);
        assert!(
            saw_range_206.load(Ordering::SeqCst),
            "preflight should send Range: bytes=0-63 with client User-Agent"
        );

        // 2. Within same response, 403 direct format falls back to valid #EXTM3U hlsManifestUrl.
        let hls_resolved = rt
            .block_on(resolve_innertube_stream_with_options(
                &client,
                &origin,
                "HLS_FALLBACK",
                None,
                None,
                true,
                true,
            ))
            .expect("should fall back to valid HLS manifest in same response");
        assert_eq!(hls_resolved.url, format!("{origin}/stream/hls.m3u8"));
        assert!(hls_resolved.is_hls);

        // 3. Verify exact anonymous cascade ordering both with and without cookie.
        let _ = rt.block_on(resolve_innertube_stream_with_options(
            &client,
            &origin,
            "ORDER_TRACK",
            Some("SAPISID=test_cookie"),
            None,
            true,
            false,
        ));
        let auth_order = std::mem::take(&mut *client_order.lock().unwrap());
        assert_eq!(
            auth_order,
            vec![
                "VISIONOS:anon",
                "ANDROID:anon",
                "ANDROID_VR:anon",
                "IOS:anon",
            ]
        );

        let _ = rt.block_on(resolve_innertube_stream_with_options(
            &client,
            &origin,
            "ORDER_TRACK",
            None,
            None,
            true,
            false,
        ));
        let anon_order = std::mem::take(&mut *client_order.lock().unwrap());
        assert_eq!(
            anon_order,
            vec![
                "VISIONOS:anon",
                "ANDROID:anon",
                "ANDROID_VR:anon",
                "IOS:anon",
            ]
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn dual_origin_fallback_uses_secondary_origin_without_mobile_auth_and_respects_budget() {
        // Server 1 acts as primary (`music.youtube.com`): always rejects with UNPLAYABLE.
        let primary_listener = TcpListener::bind("127.0.0.1:0").expect("bind primary mock");
        let primary_port = primary_listener.local_addr().unwrap().port();
        let primary_origin = format!("http://127.0.0.1:{primary_port}");
        let _ = primary_listener.set_nonblocking(true);

        // Server 2 acts as secondary (`www.youtube.com`): accepts anonymous ANDROID
        // with matching Origin / X-Origin headers and NO SAPISIDHASH / Cookie headers.
        let secondary_listener = TcpListener::bind("127.0.0.1:0").expect("bind secondary mock");
        let secondary_port = secondary_listener.local_addr().unwrap().port();
        let secondary_origin = format!("http://127.0.0.1:{secondary_port}");
        let secondary_origin_srv = secondary_origin.clone();
        let _ = secondary_listener.set_nonblocking(true);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_p = Arc::clone(&stop);
        let stop_s = Arc::clone(&stop);
        let saw_secondary_origin_header = Arc::new(AtomicBool::new(false));
        let saw_sec_hdr_srv = Arc::clone(&saw_secondary_origin_header);
        let saw_mobile_auth_header = Arc::new(AtomicBool::new(false));
        let saw_sec_auth_srv = Arc::clone(&saw_mobile_auth_header);

        let primary_srv = std::thread::spawn(move || {
            while !stop_p.load(Ordering::Relaxed) {
                match primary_listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"VIS_PRIMARY"}}"#.to_owned()
                        } else if req.contains("SLOW_BUDGET") {
                            std::thread::sleep(Duration::from_millis(250));
                            r#"{"playabilityStatus":{"status":"UNPLAYABLE","reason":"slow"}}"#
                                .to_owned()
                        } else {
                            r#"{"playabilityStatus":{"status":"UNPLAYABLE","reason":"This video is not available"}}"#
                                .to_owned()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let secondary_srv = std::thread::spawn(move || {
            while !stop_s.load(Ordering::Relaxed) {
                match secondary_listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);

                        let mut origin_hdr = None;
                        let mut x_origin_hdr = None;
                        for line in req.lines() {
                            if let Some((k, v)) = line.split_once(':') {
                                if k.eq_ignore_ascii_case("origin") {
                                    origin_hdr = Some(v.trim().to_owned());
                                } else if k.eq_ignore_ascii_case("x-origin") {
                                    x_origin_hdr = Some(v.trim().to_owned());
                                } else if k.eq_ignore_ascii_case("authorization")
                                    || k.eq_ignore_ascii_case("cookie")
                                {
                                    saw_sec_auth_srv.store(true, Ordering::SeqCst);
                                }
                            }
                        }

                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"VIS_SECONDARY"}}"#.to_owned()
                        } else if req.contains("DUAL_ORIGIN_OK")
                            && req.contains("\"clientName\":\"ANDROID\"")
                        {
                            if origin_hdr.as_deref() == Some(secondary_origin_srv.as_str())
                                && x_origin_hdr.as_deref() == Some(secondary_origin_srv.as_str())
                            {
                                saw_sec_hdr_srv.store(true, Ordering::SeqCst);
                            }
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Dual Origin Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": "https://cdn.example.test/www_android140.m4a"
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        } else {
                            r#"{"playabilityStatus":{"status":"UNPLAYABLE","reason":"rejected"}}"#
                                .to_owned()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::new();

        // 1. Primary (`music.youtube.com`) fails -> falls back to secondary (`www.youtube.com`)
        //    ANDROID(anon) with matching Origin/X-Origin and NO SAPISIDHASH / Cookie headers.
        let resolved = rt
            .block_on(resolve_innertube_stream_inner(
                &client,
                &primary_origin,
                Some(&secondary_origin),
                "DUAL_ORIGIN_OK",
                Some("SAPISID=dual_origin_secret"),
                None,
                true,
                false,
                Duration::from_secs(10),
                None,
            ))
            .expect("should resolve via secondary origin ANDROID(anon)");
        assert_eq!(resolved.url, "https://cdn.example.test/www_android140.m4a");
        assert!(
            saw_secondary_origin_header.load(Ordering::SeqCst),
            "secondary origin request must carry matching Origin and X-Origin headers"
        );
        assert!(
            !saw_mobile_auth_header.load(Ordering::SeqCst),
            "native mobile InnerTube clients must NOT send SAPISIDHASH or Cookie headers"
        );

        // 2. Cascade enforces total time budget cleanly instead of running every slow step.
        let started = Instant::now();
        let err = rt
            .block_on(resolve_innertube_stream_inner(
                &client,
                &primary_origin,
                None,
                "SLOW_BUDGET",
                Some("SAPISID=dual_origin_secret"),
                None,
                true,
                false,
                Duration::from_millis(320),
                None,
            ))
            .expect_err("slow cascade should fail within budget");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(900),
            "cascade exceeded total budget ({elapsed:?}), err={err}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = primary_srv.join();
        let _ = secondary_srv.join();
    }

    fn build_test_visitor_data_for_geo(cc: &str) -> String {
        // Protobuf: field 1 (string "testVisitor") + field 5 (varint 1700000000) + field 6 { field 1: cc }
        let mut nested = Vec::new();
        nested.push(0x0a); // field 1, wire type 2
        nested.push(cc.len() as u8);
        nested.extend_from_slice(cc.as_bytes());

        let mut pb = Vec::new();
        pb.push(0x0a); // field 1, wire type 2
        pb.push(11);
        pb.extend_from_slice(b"testVisitor");
        pb.push(0x28); // field 5, wire type 0 (varint)
        pb.extend_from_slice(&[0x80, 0x90, 0xd4, 0xaa, 0x06]);
        pb.push(0x32); // field 6, wire type 2
        pb.push(nested.len() as u8);
        pb.extend_from_slice(&nested);

        crate::crypto::base64_encode(&pb)
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_owned()
    }

    #[test]
    fn decodes_visitor_data_geo_and_extracts_subreason_and_annotates_geo_cn() {
        // 1. Real YouTube visitorData -> "US".
        assert_eq!(
            decode_visitor_data_geo("CgtzN1k4cXNzSlpSZyianpvYBjIKCgJVUxIEGgAgPg%3D%3D").as_deref(),
            Some("US")
        );
        // Synthetic CN / HK (unpadded URL-safe & percent-encoded padded).
        let cn_vd = build_test_visitor_data_for_geo("CN");
        let hk_vd = build_test_visitor_data_for_geo("HK");
        assert_eq!(decode_visitor_data_geo(&cn_vd).as_deref(), Some("CN"));
        assert_eq!(
            decode_visitor_data_geo(&format!("{cn_vd}%3D%3D")).as_deref(),
            Some("CN")
        );
        assert_eq!(decode_visitor_data_geo(&hk_vd).as_deref(), Some("HK"));

        // Malformed / empty / non-country values -> None.
        assert_eq!(decode_visitor_data_geo(""), None);
        assert_eq!(decode_visitor_data_geo("%%%invalid"), None);
        assert_eq!(decode_visitor_data_geo("not!base64!"), None);
        assert_eq!(
            decode_visitor_data_geo(&build_test_visitor_data_for_geo("us")),
            None
        );
        assert_eq!(
            decode_visitor_data_geo(&build_test_visitor_data_for_geo("USA")),
            None
        );

        // 2. Subreason extraction (simpleText, runs[].text, dedup, empty reason).
        let simple_payload = json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "reason": "Video unavailable",
                "errorScreen": {
                    "playerErrorMessageRenderer": {
                        "subreason": { "simpleText": "The uploader has not made this video available in your country" }
                    }
                }
            }
        });
        match collect_audio_candidates(&simple_payload, "vid1", ANDROID_UA, None, true) {
            Err(SelectionError::Playability { status, reason }) => {
                assert_eq!(status, "UNPLAYABLE");
                assert_eq!(
                    reason,
                    "Video unavailable (The uploader has not made this video available in your country)"
                );
            }
            _ => panic!("expected Playability error"),
        }

        let runs_payload = json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "errorScreen": {
                    "playerErrorMessageRenderer": {
                        "subreason": {
                            "runs": [
                                { "text": "Not available " },
                                { "text": "in your region" }
                            ]
                        }
                    }
                }
            }
        });
        match collect_audio_candidates(&runs_payload, "vid2", ANDROID_UA, None, true) {
            Err(SelectionError::Playability { reason, .. }) => {
                assert_eq!(reason, "Not available in your region");
            }
            _ => panic!("expected Playability error"),
        }

        // 3. End-to-end mock server returning CN visitorData + UNPLAYABLE -> annotates `YouTube geo=CN; switch proxy/VPN region`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind geo mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let cn_vd_srv = cn_vd.clone();
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            json!({ "responseContext": { "visitorData": cn_vd_srv } }).to_string()
                        } else {
                            json!({
                                "playabilityStatus": {
                                    "status": "UNPLAYABLE",
                                    "reason": "This video is not available",
                                    "errorScreen": {
                                        "playerErrorMessageRenderer": {
                                            "subreason": { "simpleText": "Blocked in your region" }
                                        }
                                    }
                                }
                            })
                            .to_string()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::new();
        let err = rt
            .block_on(resolve_innertube_stream_with_options(
                &client,
                &origin,
                "CNBLOCK0001",
                None,
                None,
                true,
                false,
            ))
            .expect_err("CN geo UNPLAYABLE must error with geo annotation");
        assert_eq!(
            err,
            "This video is not available (Blocked in your region) (YouTube geo=CN; switch proxy/VPN region)"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn signature_cipher_custom_sp_and_n_rewrite_and_hls_variant_n_preflight() {
        // 1. `signatureCipher` with custom `sp=sig` and query `n` challenge rewrite.
        let fmt_custom_sp = json!({
            "itag": 140,
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "bitrate": 128000,
            "signatureCipher": "s=encSig%3D%2B1&sp=sig&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback%3Fid%3Dabc%26n%3DoldN123%26mime%3Daudio%252Fmp4"
        });
        let parsed_custom = parse_format_url(&fmt_custom_sp).expect("parse signatureCipher");
        assert_eq!(parsed_custom.s.as_deref(), Some("encSig=+1"));
        assert_eq!(parsed_custom.sp, "sig");
        assert_eq!(
            extract_query_n_param(&parsed_custom.base_url).as_deref(),
            Some("oldN123")
        );

        let mut solved_sigs = HashMap::new();
        solved_sigs.insert("encSig=+1".to_owned(), "dec/Sig=42".to_owned());
        let mut solved_ns = HashMap::new();
        solved_ns.insert("oldN123".to_owned(), "newN+99".to_owned());
        solved_ns.insert("hlsN001".to_owned(), "hlsSolved001".to_owned());

        let rewritten = apply_solved_sig_and_n(
            &parsed_custom.base_url,
            parsed_custom.s.as_deref(),
            &parsed_custom.sp,
            &solved_sigs,
            &solved_ns,
        )
        .expect("reconstruct signed URL");
        assert!(rewritten.contains("n=newN%2B99"), "n rewritten: {rewritten}");
        assert!(
            rewritten.ends_with("&sig=dec%2FSig%3D42"),
            "custom sp rewritten: {rewritten}"
        );

        // 2. Legacy `cipher` defaults `sp` to `signature`.
        let fmt_legacy = json!({
            "cipher": "s=rawS&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback%3Fitag%3D251"
        });
        let parsed_legacy = parse_format_url(&fmt_legacy).expect("parse legacy cipher");
        assert_eq!(parsed_legacy.sp, "signature");
        assert_eq!(parsed_legacy.s.as_deref(), Some("rawS"));

        // 3. HLS master variant selection: prefers `#EXT-X-MEDIA:TYPE=AUDIO` `mp4a.40.2`,
        //    and for muxed `web_safari` prefers `itag=93 -> 94 -> 92 -> 95 -> 91`.
        let audio_group_master = "\
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"he\",NAME=\"HE\",DEFAULT=NO,URI=\"https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/233/n/oldHe/index.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"lc\",NAME=\"LC\",DEFAULT=YES,URI=\"https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/140/n/hlsN001/index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"lc\"
https://manifest.googlevideo.com/video.m3u8
";
        let picked_audio = select_best_youtube_hls_variant(
            "https://manifest.googlevideo.com/master.m3u8",
            audio_group_master,
        )
        .expect("pick audio rendition");
        assert!(picked_audio.contains("/itag/140/"), "got {picked_audio}");
        let rewritten_hls = apply_solved_sig_and_n(
            &picked_audio,
            None,
            "signature",
            &solved_sigs,
            &solved_ns,
        )
        .expect("rewrite HLS /n/");
        assert!(
            rewritten_hls.contains("/n/hlsSolved001/"),
            "HLS path /n/ rewritten: {rewritten_hls}"
        );

        let muxed_safari_master = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=86008,CODECS=\"avc1.42c00c,mp4a.40.5\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/91/n/hlsN001/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=122443,CODECS=\"avc1.4d4015,mp4a.40.5\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/92/n/hlsN001/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=263884,CODECS=\"avc1.4d401e,mp4a.40.2\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/93/n/hlsN001/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=322741,CODECS=\"avc1.4d401f,mp4a.40.2\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/94/n/hlsN001/index.m3u8
";
        let picked_muxed = select_best_youtube_hls_variant(
            "https://manifest.googlevideo.com/master.m3u8",
            muxed_safari_master,
        )
        .expect("pick itag 93");
        assert!(picked_muxed.contains("/itag/93/"), "expected itag=93, got {picked_muxed}");

        // 4. HLS preflight rejects 403 media segment and accepts 200 media segment.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hls preflight mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let origin_srv = origin.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let first = req.lines().next().unwrap_or_default();
                        let resp = if first.starts_with("GET /bad.m3u8") {
                            let body = format!(
                                "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:5.0,\n{origin_srv}/seg403.ts\n"
                            );
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        } else if first.starts_with("GET /good.m3u8") {
                            let body = format!(
                                "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:5.0,\n{origin_srv}/seg200.ts\n"
                            );
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        } else if first.starts_with("GET /seg403.ts") {
                            "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\nForbidden"
                                .to_owned()
                        } else if first.starts_with("GET /seg200.ts") {
                            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 8\r\nConnection: close\r\n\r\n12345678"
                                .to_owned()
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_owned()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::new();
        let make_hls = |path: &str| ResolvedAudioStream {
            video_id: "hlsTest0001".to_owned(),
            title: "HLS".to_owned(),
            url: format!("{origin}{path}"),
            mime_type: "application/vnd.apple.mpegurl".to_owned(),
            ext: "m3u8".to_owned(),
            acodec: "aac".to_owned(),
            format_id: "hls".to_owned(),
            bitrate: 0,
            duration_secs: None,
            user_agent: WEB_SAFARI_UA.to_owned(),
            is_hls: true,
        };

        let bad_res = rt.block_on(verify_candidate_stream_url(
            &client,
            &make_hls("/bad.m3u8"),
            Duration::from_secs(5),
        ));
        assert!(
            bad_res.as_ref().is_err_and(|e| e.contains("403")),
            "HLS preflight must reject 403 segment, got: {bad_res:?}"
        );

        let good_res = rt.block_on(verify_candidate_stream_url(
            &client,
            &make_hls("/good.m3u8"),
            Duration::from_secs(5),
        ));
        assert!(
            good_res.is_ok(),
            "HLS preflight must accept 200 segment, got: {good_res:?}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    #[allow(non_snake_case)]
    #[ignore = "live network test against music.youtube.com/www.youtube.com with official yt-dlp-ejs 0.8.0"]
    fn live_yt_ejs_resolves_1_8jBVA9a3U_with_web_clients() {
        let cookie = std::env::var("NSH_YTM_COOKIE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let base = dirs::data_dir()?
                    .join("nsh-music")
                    .join("plugin-data")
                    .join("youtube-music");
                for fname in ["settings.json", "storage.json"] {
                    if let Ok(raw) = std::fs::read_to_string(base.join(fname))
                        && let Ok(val) = serde_json::from_str::<Value>(&raw)
                        && let Some(c) = val
                            .get("cookie")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                    {
                        return Some(c.to_owned());
                    }
                }
                None
            });
        println!(
            "live_yt_ejs_resolves_1_8jBVA9a3U_with_web_clients: cookie_present={}",
            cookie.is_some()
        );

        let cache_dir = std::env::temp_dir().join(format!(
            "nsh-live-yt-ejs-1_8jBVA9a3U-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&cache_dir);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::new();

        // 1. Cold solve + preflight on 1_8jBVA9a3U (with disk cache).
        let t_cold = Instant::now();
        let stream1 = rt
            .block_on(resolve_innertube_stream_with_cache(
                &client,
                DEFAULT_INNERTUBE_ORIGIN,
                "1_8jBVA9a3U",
                cookie.as_deref(),
                None,
                true,
                true,
                Some(&cache_dir),
            ))
            .expect("cold resolve_innertube_stream_with_cache on 1_8jBVA9a3U should succeed");
        let cold_ms = t_cold.elapsed().as_millis();
        println!(
            "COLD resolve: elapsed={}ms format_id={} ext={} is_hls={} mime={} host={}",
            cold_ms,
            stream1.format_id,
            stream1.ext,
            stream1.is_hls,
            stream1.mime_type,
            sanitize_url_for_log(&stream1.url),
        );

        // Verify live stream preflight directly (200/206).
        rt.block_on(verify_candidate_stream_url(
            &client,
            &stream1,
            Duration::from_secs(8),
        ))
        .expect("resolved cold stream must pass live HTTP 200/206 preflight");

        // 2. Warm solve + preflight on 1_8jBVA9a3U (reusing in-memory + disk cached preprocessed_player).
        let t_warm = Instant::now();
        let stream2 = rt
            .block_on(resolve_innertube_stream_with_cache(
                &client,
                DEFAULT_INNERTUBE_ORIGIN,
                "1_8jBVA9a3U",
                cookie.as_deref(),
                None,
                true,
                true,
                Some(&cache_dir),
            ))
            .expect("warm resolve_innertube_stream_with_cache on 1_8jBVA9a3U should succeed");
        let warm_ms = t_warm.elapsed().as_millis();
        println!(
            "WARM resolve: elapsed={}ms format_id={} ext={} is_hls={} mime={} host={}",
            warm_ms,
            stream2.format_id,
            stream2.ext,
            stream2.is_hls,
            stream2.mime_type,
            sanitize_url_for_log(&stream2.url),
        );

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut data = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        let mut header_end: Option<usize> = None;
        let mut content_length: usize = 0;

        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    if header_end.is_none()
                        && let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        let end = pos + 4;
                        header_end = Some(end);
                        let headers_str = String::from_utf8_lossy(&data[..pos]);
                        for line in headers_str.lines() {
                            if let Some((k, v)) = line.split_once(':')
                                && k.trim().eq_ignore_ascii_case("content-length")
                                && let Ok(len) = v.trim().parse::<usize>()
                            {
                                content_length = len;
                            }
                        }
                    }
                    if let Some(end) = header_end
                        && data.len() >= end + content_length
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    fn extract_json_client_name(req: &str) -> String {
        let needle = "\"clientName\":\"";
        if let Some(pos) = req.find(needle) {
            let rest = &req[pos + needle.len()..];
            if let Some(end) = rest.find('"') {
                return rest[..end].to_owned();
            }
        }
        "UNKNOWN".to_owned()
    }
}
