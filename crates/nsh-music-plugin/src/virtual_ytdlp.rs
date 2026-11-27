//! In-process `yt-dlp` virtualization shim (`VirtualYtdlpShim` / `YtdlpVirtualShim`) powered by
//! embedded upstream `LuanRT/YouTube.js` (`youtubei.js@18.3.0`, MIT) + `yt-dlp/ejs` (`0.8.0`).
//!
//! Enables `mygo-music` plugins calling `host.spawn("yt-dlp", ...)` (including `youtube-music`)
//! to work seamlessly on iOS, Android, HarmonyOS, and desktops without an external `yt-dlp` / Python
//! installation. On Desktop when `Allow Programs` is granted and a real `yt-dlp` binary exists,
//! the real binary is preferred; otherwise (or on Mobile) `VirtualYtdlpShim` runs in-process
//! using `YouTube.js` + `yt-dlp/ejs` inside `quickjs-jit` bridged to Rust `reqwest`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host::{
    ExternalProcessHandle, HostContext, ManagedProcess, SpawnOptions, VirtualPipeHandle,
};
use crate::mobile_compat::{
    VirtualProcessShim, VirtualSpawnResult, resolve_real_binary, resolve_relative_command,
    with_default_cwd,
};
use crate::yt_ejs;
use crate::yt_format_selector::{
    CandidateFormatMeta, FormatSelector, FormatSortSpec, PoTokenContext, YoutubeExtractorArgs,
};
use crate::yt_youtubejs::{
    RESILIENT_FALLBACK_CLIENTS, YtJsClientSpec, YtJsCollectionEntry, YtJsCollectionKind,
    YtJsCollectionRequest, YtJsCollectionResponse, YtJsExtractRequest, YtJsExtractResponse,
    extract_collection_via_youtubejs, extract_via_youtubejs, lookup_ytjs_client,
    resolve_requested_ytjs_clients,
};

const DEFAULT_INNERTUBE_ORIGIN: &str = "https://music.youtube.com";
const PLAYER_HTTP_TIMEOUT: Duration = Duration::from_secs(12);
const PREFLIGHT_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
const HLS_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CASCADE_BUDGET: Duration = Duration::from_secs(24);
const CLIENT_WINNER_TTL: Duration = Duration::from_secs(15 * 60);
const CLIENT_FAILURE_TTL: Duration = Duration::from_secs(5 * 60);

/// Synthetic `--version` banner emitted by [`VirtualYtdlpShim`].
pub const VIRTUAL_YTDLP_VERSION_BANNER: &str = "2026.03.01 (nsh-youtubejs-18.3.0+ejs-0.8.0)\n";

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
    /// `-j` or `--dump-json` (full track info JSON object, NDJSON for collections).
    DumpJson,
    /// `-J` or `--dump-single-json` (single JSON object, playlist envelope for collections).
    DumpSingleJson,
}

/// Parsed `yt-dlp` CLI + stdin config options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtdlpParsedArgs {
    pub is_version: bool,
    pub reads_stdin_config: bool,
    pub output_mode: YtdlpOutputMode,
    pub print_template: Option<String>,
    pub print_templates: Vec<String>,
    pub format_expr: Option<String>,
    pub format_sort: Vec<String>,
    pub extractor_args: Vec<String>,
    pub headers: HashMap<String, String>,
    pub cookies_file: Option<String>,
    pub proxy: Option<String>,
    pub no_check_certificates: bool,
    pub socket_timeout_secs: Option<u64>,
    pub geo_bypass_country: Option<String>,
    pub flat_playlist: bool,
    pub no_playlist: bool,
    pub yes_playlist: bool,
    pub playlist_items: Option<String>,
    pub target: Option<String>,
}

impl Default for YtdlpParsedArgs {
    fn default() -> Self {
        Self {
            is_version: false,
            reads_stdin_config: false,
            output_mode: YtdlpOutputMode::PrintUrl,
            print_template: None,
            print_templates: Vec::new(),
            format_expr: None,
            format_sort: Vec::new(),
            extractor_args: Vec::new(),
            headers: HashMap::new(),
            cookies_file: None,
            proxy: None,
            no_check_certificates: false,
            socket_timeout_secs: None,
            geo_bypass_country: None,
            flat_playlist: false,
            no_playlist: false,
            yes_playlist: false,
            playlist_items: None,
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

fn parse_socket_timeout_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Ok(n) = trimmed.parse::<u64>() {
        return Some(n.max(1));
    }
    if let Ok(f) = trimmed.parse::<f64>()
        && f.is_finite()
        && f > 0.0
    {
        return Some((f.ceil() as u64).max(1));
    }
    None
}

fn apply_ytdlp_tokens(parsed: &mut YtdlpParsedArgs, tokens: &[String], allow_positional: bool) {
    let mut i = 0;
    let mut end_of_options = false;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);

        if end_of_options {
            if allow_positional {
                parsed.target = Some(tok.to_owned());
            }
            i += 1;
            continue;
        }

        if tok == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

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
        if tok == "-J" || tok == "--dump-single-json" {
            parsed.output_mode = YtdlpOutputMode::DumpSingleJson;
            i += 1;
            continue;
        }
        if tok == "-O" || tok == "--print" {
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
        if tok == "-S" || tok == "--format-sort" {
            if let Some(spec) = next {
                parsed.format_sort.push(spec.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(spec) = tok.strip_prefix("--format-sort=") {
            parsed.format_sort.push(spec.to_owned());
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
        if tok == "--referer" {
            if let Some(r) = next {
                upsert_header(&mut parsed.headers, "Referer", r);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(r) = tok.strip_prefix("--referer=") {
            upsert_header(&mut parsed.headers, "Referer", r);
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
        if tok == "--proxy" {
            if let Some(p) = next {
                parsed.proxy = Some(p.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(p) = tok.strip_prefix("--proxy=") {
            parsed.proxy = Some(p.to_owned());
            i += 1;
            continue;
        }
        if tok == "--no-check-certificates" || tok == "--no-check-certificate" {
            parsed.no_check_certificates = true;
            i += 1;
            continue;
        }
        if tok == "--socket-timeout" {
            if let Some(s) = next {
                parsed.socket_timeout_secs = parse_socket_timeout_secs(s);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(s) = tok.strip_prefix("--socket-timeout=") {
            parsed.socket_timeout_secs = parse_socket_timeout_secs(s);
            i += 1;
            continue;
        }
        if tok == "--geo-bypass-country" {
            if let Some(cc) = next {
                let norm = cc.trim().to_ascii_uppercase();
                if norm.len() == 2 && norm.bytes().all(|b| b.is_ascii_uppercase()) {
                    parsed.geo_bypass_country = Some(norm);
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(cc) = tok.strip_prefix("--geo-bypass-country=") {
            let norm = cc.trim().to_ascii_uppercase();
            if norm.len() == 2 && norm.bytes().all(|b| b.is_ascii_uppercase()) {
                parsed.geo_bypass_country = Some(norm);
            }
            i += 1;
            continue;
        }
        if tok == "--flat-playlist" {
            parsed.flat_playlist = true;
            i += 1;
            continue;
        }
        if tok == "--no-playlist" {
            parsed.no_playlist = true;
            parsed.yes_playlist = false;
            i += 1;
            continue;
        }
        if tok == "--yes-playlist" {
            parsed.yes_playlist = true;
            parsed.no_playlist = false;
            i += 1;
            continue;
        }
        if tok == "-I" || tok == "--playlist-items" {
            if let Some(spec) = next {
                parsed.playlist_items = Some(spec.to_owned());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(spec) = tok.strip_prefix("--playlist-items=") {
            parsed.playlist_items = Some(spec.to_owned());
            i += 1;
            continue;
        }

        // Known 2-arg `yt-dlp` flags whose values must be consumed so `parsed.target` is never overwritten.
        if matches!(
            tok,
            "--js-runtimes"
                | "-o"
                | "--output"
                | "--cookies-from-browser"
                | "--retries"
                | "-R"
                | "--fragment-retries"
                | "--extractor-retries"
                | "--file-access-retries"
                | "--concurrent-fragments"
                | "-N"
                | "--buffer-size"
                | "--http-chunk-size"
                | "--throttled-rate"
                | "--limit-rate"
                | "-r"
                | "--sleep-interval"
                | "--max-sleep-interval"
                | "--sleep-requests"
                | "--sleep-subtitles"
                | "--max-downloads"
                | "--impersonate"
                | "--download-sections"
                | "--paths"
                | "-P"
                | "--temp-dir"
                | "--cache-dir"
                | "--ffmpeg-location"
                | "--audio-format"
                | "--audio-quality"
                | "--merge-output-format"
                | "--remux-video"
                | "--recode-video"
                | "--match-filter"
                | "--min-filesize"
                | "--max-filesize"
                | "--sub-langs"
                | "--sub-format"
                | "--source-address"
                | "--xff"
                | "--download-archive"
                | "--batch-file"
                | "-a"
                | "--playlist-start"
                | "--playlist-end"
                | "--compat-options"
                | "--color"
                | "--use-extractors"
                | "--default-search"
        ) {
            i += if next.is_some() { 2 } else { 1 };
            continue;
        }

        // 1/64th of YouTube's 11-char base64url video IDs start with `-` (e.g. `-dQw4w9WgXc`).
        // Before discarding a single-dash token as an unknown flag, accept bare 11-char IDs as target.
        if tok.starts_with('-') {
            if !tok.starts_with("--") && allow_positional && is_bare_youtube_id(tok) {
                parsed.target = Some(tok.to_owned());
            }
            i += 1;
            continue;
        }
        if allow_positional {
            parsed.target = Some(tok.to_owned());
        }
        i += 1;
    }
}

fn strip_ytdlp_print_when_prefix(raw: &str) -> &str {
    const WHEN_STAGES: &[&str] = &[
        "pre_process",
        "after_filter",
        "video",
        "before_dl",
        "post_process",
        "after_move",
        "after_video",
        "playlist",
    ];
    let trimmed = raw.trim();
    if let Some((prefix, rest)) = trimmed.split_once(':') {
        let p = prefix.trim();
        if WHEN_STAGES.iter().any(|s| p.eq_ignore_ascii_case(s)) {
            return rest.trim();
        }
    }
    trimmed
}

fn apply_print_template(parsed: &mut YtdlpParsedArgs, tpl: &str) {
    let cleaned = strip_ytdlp_print_when_prefix(tpl);
    if !cleaned.is_empty() {
        parsed.print_templates.push(cleaned.to_owned());
        parsed.print_template = Some(cleaned.to_owned());
    }
    if parsed.output_mode != YtdlpOutputMode::DumpJson
        && parsed.output_mode != YtdlpOutputMode::DumpSingleJson
    {
        if cleaned.contains("http_headers") || cleaned.ends_with("}j") || cleaned.ends_with(")j") {
            parsed.output_mode = YtdlpOutputMode::PrintUrlAndHeadersJson;
        } else if cleaned == "url" || cleaned == "%(url)s" {
            parsed.output_mode = YtdlpOutputMode::PrintUrl;
        }
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
/// `youtube-nocookie.com/embed/<id>`, `youtu.be/<id>`, `youtube.com/shorts/<id>`, `/embed/<id>`,
/// `/e/<id>`, `/live/<id>`, `/attribution_link?...u=...`, or a bare 11-char video ID (including IDs starting with `-`).
pub fn extract_youtube_video_id(input: &str) -> Option<String> {
    extract_youtube_video_id_depth(input, 0)
}

fn is_youtube_host(host: &str) -> bool {
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
}

fn extract_youtube_video_id_depth(input: &str, depth: u8) -> Option<String> {
    if depth > 2 {
        return None;
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_bare_youtube_id(trimmed) {
        return Some(trimmed.to_owned());
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if !is_youtube_host(&host) {
        return None;
    }

    if host == "youtu.be" || host.ends_with(".youtu.be") {
        let seg = parsed.path_segments()?.find(|s| !s.is_empty())?;
        return Some(seg.to_owned());
    }

    if parsed.path().eq_ignore_ascii_case("/attribution_link") {
        for (k, v) in parsed.query_pairs() {
            if k == "u" && !v.is_empty() {
                let target = if v.starts_with("http://") || v.starts_with("https://") {
                    v.into_owned()
                } else {
                    format!("https://www.youtube.com/{}", v.trim_start_matches('/'))
                };
                if let Some(id) = extract_youtube_video_id_depth(&target, depth + 1) {
                    return Some(id);
                }
            }
        }
    }

    for (k, v) in parsed.query_pairs() {
        if k == "v" && !v.is_empty() {
            return Some(v.into_owned());
        }
    }

    let segs: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.len() >= 2 && matches!(segs[0], "shorts" | "embed" | "v" | "e" | "live") {
        return Some(segs[1].to_owned());
    }
    None
}

fn is_bare_youtube_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Classified `yt-dlp` target expression (single video, `ytsearch<N>:`, or playlist URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtdlpTarget {
    Video(String),
    Search { query: String, count: usize },
    Playlist { list_id: String, is_music: bool },
}

/// Classifies a `yt-dlp` positional target into a single video, `ytsearch`, or playlist.
pub fn classify_ytdlp_target(target: &str, parsed: &YtdlpParsedArgs) -> Option<YtdlpTarget> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 1. `ytsearch:<query>`, `ytsearch<N>:<query>`, `ytsearchall:<query>`
    if let Some((prefix, query_raw)) = trimmed.split_once(':') {
        let lower = prefix.trim().to_ascii_lowercase();
        if let Some(suffix) = lower.strip_prefix("ytsearch") {
            let query = query_raw.trim();
            if query.is_empty() {
                return None;
            }
            let count = if suffix.is_empty() {
                1
            } else if suffix == "all" {
                20
            } else if let Ok(n) = suffix.parse::<usize>() {
                n.clamp(1, 50)
            } else {
                return None;
            };
            return Some(YtdlpTarget::Search {
                query: query.to_owned(),
                count,
            });
        }
    }

    // 2. YouTube / YouTube Music playlist URLs (`list=...`).
    if let Ok(url) = url::Url::parse(trimmed)
        && let Some(host) = url.host_str().map(str::to_ascii_lowercase)
        && is_youtube_host(&host)
    {
        let list_id = url
            .query_pairs()
            .find(|(k, v)| k == "list" && !v.is_empty())
            .map(|(_, v)| v.into_owned());
        let video_id = extract_youtube_video_id(trimmed);
        let is_music = host == "music.youtube.com";
        let is_playlist_path = url
            .path()
            .trim_end_matches('/')
            .eq_ignore_ascii_case("/playlist");

        if let Some(lid) = list_id {
            if is_playlist_path || (video_id.is_none() && !parsed.no_playlist) {
                return Some(YtdlpTarget::Playlist {
                    list_id: lid,
                    is_music,
                });
            }
            if parsed.yes_playlist && !parsed.no_playlist {
                return Some(YtdlpTarget::Playlist {
                    list_id: lid,
                    is_music,
                });
            }
        }
        if let Some(vid) = video_id {
            return Some(YtdlpTarget::Video(vid));
        }
        return None;
    }

    extract_youtube_video_id(trimmed).map(YtdlpTarget::Video)
}

/// Parses a `yt-dlp` `-I` / `--playlist-items` specification (1-based indices & slices like
/// `"1,3"`, `"2:5"`, `":3"`, `"2-4"`, `"-1"`) into deduplicated 0-based indices within `0..total`.
fn parse_playlist_items_indices(spec: &str, total: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let resolve_index = |raw: i64| -> Option<usize> {
        if raw > 0 {
            let idx = (raw - 1) as usize;
            (idx < total).then_some(idx)
        } else if raw < 0 {
            let from_end = (-raw) as usize;
            (from_end <= total).then(|| total - from_end)
        } else {
            None
        }
    };

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in spec.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if p.contains(':') {
            let segs: Vec<&str> = p.split(':').collect();
            let start_1 = segs
                .first()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(1);
            let end_1 = segs
                .get(1)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(total as i64);
            let step = segs
                .get(2)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(1);
            if step == 0 {
                continue;
            }
            let norm_start = if start_1 < 0 {
                (total as i64 + start_1 + 1).clamp(1, total as i64)
            } else {
                start_1.clamp(1, total as i64)
            };
            let norm_end = if end_1 < 0 {
                (total as i64 + end_1 + 1).clamp(1, total as i64)
            } else {
                end_1.clamp(1, total as i64)
            };
            let mut cur = norm_start;
            while (step > 0 && cur <= norm_end) || (step < 0 && cur >= norm_end) {
                if let Some(idx) = resolve_index(cur)
                    && seen.insert(idx)
                {
                    out.push(idx);
                }
                cur += step;
            }
        } else if let Some((a_str, b_str)) = p.split_once('-')
            && !a_str.trim().is_empty()
            && !b_str.trim().is_empty()
            && let (Ok(a), Ok(b)) = (a_str.trim().parse::<usize>(), b_str.trim().parse::<usize>())
            && a >= 1
            && b >= a
        {
            for one_based in a..=b.min(total) {
                let idx = one_based - 1;
                if seen.insert(idx) {
                    out.push(idx);
                }
            }
        } else if let Ok(n) = p.parse::<i64>()
            && let Some(idx) = resolve_index(n)
            && seen.insert(idx)
        {
            out.push(idx);
        }
    }
    out
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
    pub vcodec: String,
    pub format_id: String,
    pub bitrate: u64,
    pub abr: Option<f64>,
    pub tbr: Option<f64>,
    pub asr: Option<u64>,
    pub audio_channels: Option<u32>,
    pub filesize: Option<u64>,
    pub duration_secs: Option<u64>,
    pub protocol: String,
    pub uploader: String,
    pub channel: String,
    pub channel_id: String,
    pub is_live: bool,
    pub live_status: String,
    pub thumbnail: String,
    pub thumbnails_json: Vec<Value>,
    pub user_agent: String,
    pub is_hls: bool,
    pub formats_json: Vec<Value>,
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

#[derive(Default)]
struct ClientHealthBucket {
    last_winner: Option<(&'static str, Instant)>,
    fail_streaks: HashMap<&'static str, (u32, Instant)>,
}

#[derive(Default)]
struct ClientHealthCache {
    with_cookie: ClientHealthBucket,
    no_cookie: ClientHealthBucket,
}

impl ClientHealthCache {
    fn bucket(&self, has_cookie: bool) -> &ClientHealthBucket {
        if has_cookie {
            &self.with_cookie
        } else {
            &self.no_cookie
        }
    }

    fn bucket_mut(&mut self, has_cookie: bool) -> &mut ClientHealthBucket {
        if has_cookie {
            &mut self.with_cookie
        } else {
            &mut self.no_cookie
        }
    }

    fn fail_streak_at(&self, has_cookie: bool, client_key: &'static str, now: Instant) -> u32 {
        self.bucket(has_cookie)
            .fail_streaks
            .get(client_key)
            .filter(|(_, at)| now.saturating_duration_since(*at) < CLIENT_FAILURE_TTL)
            .map(|(count, _)| *count)
            .unwrap_or(0)
    }

    fn healthy_winner_at(&self, has_cookie: bool, now: Instant) -> Option<&'static str> {
        let (winner, at) = self.bucket(has_cookie).last_winner?;
        if now.saturating_duration_since(at) >= CLIENT_WINNER_TTL {
            return None;
        }
        if self.fail_streak_at(has_cookie, winner, now) >= 2 {
            return None;
        }
        Some(winner)
    }

    fn record_success_at(&mut self, has_cookie: bool, client_key: &'static str, now: Instant) {
        let b = self.bucket_mut(has_cookie);
        b.last_winner = Some((client_key, now));
        b.fail_streaks.remove(client_key);
    }

    fn record_failure_at(&mut self, has_cookie: bool, client_key: &'static str, now: Instant) {
        let b = self.bucket_mut(has_cookie);
        let new_count = match b.fail_streaks.get(client_key).copied() {
            Some((count, at)) if now.saturating_duration_since(at) < CLIENT_FAILURE_TTL => {
                count.saturating_add(1)
            }
            _ => 1,
        };
        b.fail_streaks.insert(client_key, (new_count, now));
        if new_count >= 2
            && b.last_winner
                .is_some_and(|(winner, _)| winner == client_key)
        {
            b.last_winner = None;
        }
    }
}

fn client_health_cache() -> &'static Mutex<ClientHealthCache> {
    static CACHE: OnceLock<Mutex<ClientHealthCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ClientHealthCache::default()))
}

fn record_client_health_success(has_cookie: bool, client_key: &'static str) {
    if let Ok(mut guard) = client_health_cache().lock() {
        guard.record_success_at(has_cookie, client_key, Instant::now());
    }
}

fn record_client_health_failure(has_cookie: bool, client_key: &'static str) {
    if let Ok(mut guard) = client_health_cache().lock() {
        guard.record_failure_at(has_cookie, client_key, Instant::now());
    }
}

#[derive(Clone)]
struct CascadeStep {
    spec: &'static YtJsClientSpec,
    loopback_origin: Option<String>,
    send_auth: bool,
    is_explicit: bool,
}

fn apply_client_health_ordering(
    chosen: Vec<(&'static YtJsClientSpec, bool)>,
    explicit_requested: bool,
    has_cookie: bool,
    cache: &ClientHealthCache,
    now: Instant,
) -> Vec<(&'static YtJsClientSpec, bool)> {
    let winner = cache.healthy_winner_at(has_cookie, now);

    if !explicit_requested {
        let mut healthy = Vec::with_capacity(chosen.len());
        let mut demoted = Vec::with_capacity(chosen.len());
        for entry in chosen {
            if cache.fail_streak_at(has_cookie, entry.0.key, now) >= 2 {
                demoted.push(entry);
            } else {
                healthy.push(entry);
            }
        }
        if let Some(winner_key) = winner
            && let Some(pos) = healthy.iter().position(|(spec, _)| spec.key == winner_key)
            && pos > 0
        {
            let promoted = healthy.remove(pos);
            healthy.insert(0, promoted);
        }
        healthy.extend(demoted);
        return healthy;
    }

    let mut explicits = Vec::new();
    let mut fallbacks = Vec::new();
    for entry in chosen {
        if entry.1 {
            explicits.push(entry);
        } else {
            fallbacks.push(entry);
        }
    }

    let mut healthy_fallbacks = Vec::with_capacity(fallbacks.len());
    let mut demoted_fallbacks = Vec::with_capacity(fallbacks.len());
    for entry in fallbacks {
        if cache.fail_streak_at(has_cookie, entry.0.key, now) >= 2 {
            demoted_fallbacks.push(entry);
        } else {
            healthy_fallbacks.push(entry);
        }
    }

    let first_explicit_failing = explicits
        .first()
        .is_some_and(|(spec, _)| cache.fail_streak_at(has_cookie, spec.key, now) >= 2);
    let winner_differs_from_explicit =
        winner.is_some_and(|w| !explicits.iter().any(|(s, _)| s.key == w));

    let mut winner_entry: Option<(&'static YtJsClientSpec, bool)> = None;
    if winner_differs_from_explicit && let Some(w) = winner {
        if let Some(pos) = healthy_fallbacks.iter().position(|(s, _)| s.key == w) {
            winner_entry = Some(healthy_fallbacks.remove(pos));
        } else if let Some(spec) = lookup_ytjs_client(w) {
            winner_entry = Some((spec, false));
        }
    }

    let mut ordered =
        Vec::with_capacity(explicits.len() + healthy_fallbacks.len() + demoted_fallbacks.len() + 1);
    if first_explicit_failing && let Some(w_entry) = winner_entry {
        ordered.push(w_entry);
        ordered.extend(explicits);
    } else {
        ordered.extend(explicits);
        if let Some(w_entry) = winner_entry {
            ordered.push(w_entry);
        }
    }
    ordered.extend(healthy_fallbacks);
    ordered.extend(demoted_fallbacks);
    ordered
}

fn build_cascade_steps(
    primary: &str,
    has_cookie: bool,
    ext_args: &YoutubeExtractorArgs,
) -> Vec<CascadeStep> {
    let loopback = is_loopback_origin(primary);
    if loopback {
        return build_cascade_steps_inner(
            primary,
            has_cookie,
            ext_args,
            false,
            None,
            Instant::now(),
        );
    }
    let now = Instant::now();
    if let Ok(cache) = client_health_cache().lock() {
        build_cascade_steps_inner(primary, has_cookie, ext_args, true, Some(&cache), now)
    } else {
        build_cascade_steps_inner(primary, has_cookie, ext_args, true, None, now)
    }
}

fn build_cascade_steps_inner(
    primary: &str,
    has_cookie: bool,
    ext_args: &YoutubeExtractorArgs,
    append_fallbacks: bool,
    health_cache: Option<&ClientHealthCache>,
    now: Instant,
) -> Vec<CascadeStep> {
    let loopback = is_loopback_origin(primary);
    let loopback_origin = loopback.then(|| primary.trim_end_matches('/').to_owned());
    let mut steps: Vec<CascadeStep> = Vec::with_capacity(14);

    if loopback && ext_args.player_client.is_empty() && !append_fallbacks {
        for key in ["visionos", "android", "android_vr", "ios"] {
            if let Some(spec) = lookup_ytjs_client(key) {
                steps.push(CascadeStep {
                    spec,
                    loopback_origin: loopback_origin.clone(),
                    send_auth: false,
                    is_explicit: false,
                });
            }
        }
        return steps;
    }

    let explicit_requested = !ext_args.player_client.is_empty();
    let mut chosen: Vec<(&'static YtJsClientSpec, bool)> =
        resolve_requested_ytjs_clients(&ext_args.player_client, has_cookie)
            .into_iter()
            .map(|spec| (spec, explicit_requested))
            .collect();

    if explicit_requested && append_fallbacks {
        for &fallback_key in RESILIENT_FALLBACK_CLIENTS {
            if let Some(spec) = lookup_ytjs_client(fallback_key)
                && !chosen.iter().any(|(c, _)| c.key == spec.key)
            {
                chosen.push((spec, false));
            }
        }
    }

    if let Some(cache) = health_cache {
        chosen = apply_client_health_ordering(chosen, explicit_requested, has_cookie, cache, now);
    }

    for (spec, is_explicit) in chosen {
        let send_auth = spec.supports_cookies && has_cookie;
        steps.push(CascadeStep {
            spec,
            loopback_origin: loopback_origin.clone(),
            send_auth,
            is_explicit,
        });
    }

    if has_cookie && !loopback {
        for anon_key in ["web_music", "tv"] {
            if let Some(spec) = lookup_ytjs_client(anon_key)
                && !steps.iter().any(|s| s.spec.key == spec.key && !s.send_auth)
            {
                steps.push(CascadeStep {
                    spec,
                    loopback_origin: None,
                    send_auth: false,
                    is_explicit: false,
                });
            }
        }
    }

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
/// stream via embedded `YouTube.js` (`youtubei.js@18.3.0`) + `yt-dlp/ejs` (`0.8.0`) with preflight.
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

/// Runs the `YouTube.js` + `yt-dlp/ejs` stream extraction cascade with preflight enabled
/// automatically for non-loopback origins.
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

/// Runs the `YouTube.js` + `yt-dlp/ejs` stream extraction cascade with explicit preflight control.
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
/// for persisting `YouTube.js` session/player caches (`<cache_dir>/youtubejs_cache/`)
/// and `yt-dlp/ejs` preprocessed player caches (`<cache_dir>/yt-ejs-player.json`).
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
    let default_ext = YoutubeExtractorArgs::default();
    resolve_innertube_stream_with_extractor_args(
        client,
        base_origin,
        video_id,
        cookie,
        format_expr,
        &default_ext,
        prefer_mobile_aac,
        verify_stream_url,
        cache_dir,
    )
    .await
}

#[derive(Default)]
struct CascadeExtraOptions<'a> {
    format_sort: Option<&'a FormatSortSpec>,
    geo_bypass_country: Option<&'a str>,
    extra_headers: &'a [(String, String)],
    socket_timeout: Option<Duration>,
    cancel_flag: Option<&'a AtomicBool>,
}

fn is_cancelled(cancel_flag: Option<&AtomicBool>) -> bool {
    cancel_flag.is_some_and(|f| f.load(Ordering::Relaxed))
}

async fn wait_for_cancel(cancel_flag: Option<&AtomicBool>) {
    let Some(flag) = cancel_flag else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_innertube_stream_with_extractor_args(
    client: &reqwest::Client,
    base_origin: &str,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    ext_args: &YoutubeExtractorArgs,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
    cache_dir: Option<&Path>,
) -> Result<ResolvedAudioStream, String> {
    let primary = base_origin.trim_end_matches('/');
    resolve_innertube_stream_inner(
        client,
        primary,
        video_id,
        cookie,
        format_expr,
        ext_args,
        prefer_mobile_aac,
        verify_stream_url,
        DEFAULT_CASCADE_BUDGET,
        cache_dir,
        CascadeExtraOptions::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn resolve_innertube_stream_inner(
    client: &reqwest::Client,
    primary_origin: &str,
    video_id: &str,
    cookie: Option<&str>,
    format_expr: Option<&str>,
    ext_args: &YoutubeExtractorArgs,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
    total_budget: Duration,
    cache_dir: Option<&Path>,
    extra: CascadeExtraOptions<'_>,
) -> Result<ResolvedAudioStream, String> {
    let primary = primary_origin.trim_end_matches('/');
    let trimmed_cookie = cookie.map(str::trim).filter(|s| !s.is_empty());
    let has_cookie = trimmed_cookie.is_some();
    let steps = build_cascade_steps(primary, has_cookie, ext_args);
    let track_health = !is_loopback_origin(primary);
    resolve_innertube_stream_with_steps(
        client,
        steps,
        video_id,
        trimmed_cookie,
        format_expr,
        ext_args,
        prefer_mobile_aac,
        verify_stream_url,
        total_budget,
        cache_dir,
        track_health,
        extra,
    )
    .await
}

fn cap_preflight_candidates(candidates: Vec<ResolvedAudioStream>) -> Vec<ResolvedAudioStream> {
    let mut out = Vec::with_capacity(3);
    let mut direct_count = 0usize;
    let mut hls_count = 0usize;
    for cand in candidates {
        if out.len() >= 3 {
            break;
        }
        if cand.is_hls {
            if hls_count < 1 {
                hls_count += 1;
                out.push(cand);
            }
        } else if direct_count < 2 {
            direct_count += 1;
            out.push(cand);
        }
    }
    out
}

fn is_client_wide_http_rejection(why: &str) -> bool {
    why.contains("HTTP 403") || why.contains("HTTP 404") || why.contains("HTTP 410")
}

fn is_transport_or_timeout_error(why: &str) -> bool {
    let lower = why.to_ascii_lowercase();
    lower.contains("timed out") || lower.contains("request error") || lower.contains("read error")
}

#[allow(clippy::too_many_arguments)]
async fn resolve_innertube_stream_with_steps(
    client: &reqwest::Client,
    steps: Vec<CascadeStep>,
    video_id: &str,
    trimmed_cookie: Option<&str>,
    format_expr: Option<&str>,
    ext_args: &YoutubeExtractorArgs,
    prefer_mobile_aac: bool,
    verify_stream_url: bool,
    total_budget: Duration,
    cache_dir: Option<&Path>,
    track_health: bool,
    extra: CascadeExtraOptions<'_>,
) -> Result<ResolvedAudioStream, String> {
    let has_cookie = trimmed_cookie.is_some();
    let selector = FormatSelector::parse(format_expr.unwrap_or(""));
    let cancel_flag = extra.cancel_flag;
    let player_cap = extra
        .socket_timeout
        .map_or(PLAYER_HTTP_TIMEOUT, |t| t.min(PLAYER_HTTP_TIMEOUT));
    let direct_preflight_cap = extra
        .socket_timeout
        .map_or(PREFLIGHT_HTTP_TIMEOUT, |t| t.min(PREFLIGHT_HTTP_TIMEOUT));
    let hls_preflight_cap = extra
        .socket_timeout
        .map_or(HLS_PREFLIGHT_TIMEOUT, |t| t.min(HLS_PREFLIGHT_TIMEOUT));

    let deadline = Instant::now() + total_budget;
    let mut detected_geo: Option<String> = ext_args
        .visitor_data
        .as_deref()
        .and_then(decode_visitor_data_geo);
    let mut tv_bundle: Option<yt_ejs::TvPlayerBundle> = None;
    let mut tv_bundle_failed = false;

    let mut last_playability_reason: Option<String> = None;
    let mut errors = Vec::new();

    for step in steps {
        if is_cancelled(cancel_flag) {
            return Err("cancelled".to_owned());
        }
        let spec = step.spec;
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

        let client_tag = if step.send_auth {
            format!("{}(auth/{})", spec.key, spec.ytjs_client)
        } else {
            format!("{}(anon/{})", spec.key, spec.ytjs_client)
        };

        let player_pot = ext_args
            .find_po_token(spec.key, PoTokenContext::Player)
            .map(str::to_owned);
        let step_extra_headers: Vec<(String, String)> = extra
            .extra_headers
            .iter()
            .filter(|(k, _)| step.send_auth || !k.eq_ignore_ascii_case("x-goog-pageid"))
            .cloned()
            .collect();
        let extract_req = YtJsExtractRequest {
            video_id: video_id.to_owned(),
            ytjs_client: spec.ytjs_client.to_owned(),
            cookie: step
                .send_auth
                .then_some(trimmed_cookie)
                .flatten()
                .map(str::to_owned),
            lang: ext_args.lang.clone(),
            location: extra.geo_bypass_country.map(str::to_owned),
            visitor_data: ext_args.visitor_data.clone(),
            po_token: player_pot,
            user_agent_override: spec.user_agent_override.map(str::to_owned),
            extra_headers: step_extra_headers,
            skip_hls: ext_args.skip_hls,
            skip_dash: ext_args.skip_dash,
            loopback_origin: step.loopback_origin.clone(),
            cache_dir: cache_dir.map(Path::to_path_buf),
            timeout: rem.min(player_cap),
        };

        let resp = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel_flag) => {
                return Err("cancelled".to_owned());
            }
            res = extract_via_youtubejs(client, extract_req) => match res {
                Ok(r) => r,
                Err(e) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} client={client_tag} YouTube.js error: {e}"
                    );
                    errors.push(format!("{client_tag}: {e}"));
                    if track_health {
                        record_client_health_failure(has_cookie, spec.key);
                    }
                    continue;
                }
            }
        };

        if is_cancelled(cancel_flag) {
            return Err("cancelled".to_owned());
        }

        if detected_geo.is_none()
            && let Some(cc) = decode_visitor_data_geo(&resp.visitor_data)
        {
            detected_geo = Some(cc);
        }
        let geo_label = detected_geo.as_deref().unwrap_or("unknown");

        if !resp.ok {
            let reason = if !resp.playability_reason.is_empty() {
                resp.playability_reason.clone()
            } else if let Some(ref e) = resp.error {
                e.clone()
            } else if !resp.playability_status.is_empty() {
                format!("video {}", resp.playability_status)
            } else {
                "extraction failed".to_owned()
            };
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} playability={}: {reason}",
                if resp.playability_status.is_empty() {
                    "ERR"
                } else {
                    &resp.playability_status
                }
            );
            if !resp.playability_status.is_empty() {
                last_playability_reason = Some(reason.clone());
            }
            errors.push(format!("{client_tag}: {reason}"));
            if track_health {
                record_client_health_failure(has_cookie, spec.key);
            }
            continue;
        }

        let (video_meta, mut all_candidates) = candidates_from_youtubejs(video_id, &resp);

        // DASH MPD `<BaseURL>` fallback when `adaptive_formats` & `formats` yield 0 audio formats.
        if all_candidates.is_empty()
            && !ext_args.skip_dash
            && let Some(ref dash_url) = resp.dash_manifest_url
            && let Some(dash_rem) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
        {
            let dash_timeout = dash_rem.min(direct_preflight_cap);
            let dash_fetch = tokio::select! {
                biased;
                _ = wait_for_cancel(cancel_flag) => return Err("cancelled".to_owned()),
                res = fetch_text_prefix(client, dash_url, &resp.user_agent, dash_timeout, 512 * 1024) => res,
            };
            if let Ok(mpd_xml) = dash_fetch {
                let dash_cands =
                    parse_dash_mpd_audio_candidates(&mpd_xml, &video_meta, &resp.user_agent);
                if !dash_cands.is_empty() {
                    all_candidates.extend(dash_cands);
                }
            }
        }

        if let Some(ref hls_url) = resp.hls_manifest_url
            && !ext_args.skip_hls
        {
            let mut expanded = Vec::new();
            if (step.loopback_origin.is_none() || verify_stream_url)
                && let Some(hls_rem) = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
            {
                let hls_timeout = hls_rem.min(direct_preflight_cap);
                let hls_res = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancel_flag) => return Err("cancelled".to_owned()),
                    res = fetch_text_prefix(client, hls_url, &resp.user_agent, hls_timeout, 256 * 1024) => res,
                };
                if let Ok(text) = hls_res {
                    expanded =
                        expand_hls_master_variants(hls_url, &text, &video_meta, &resp.user_agent);
                }
            }
            if !expanded.is_empty() {
                all_candidates.extend(expanded);
            } else {
                let needs_ejs = extract_path_n_param(hls_url).is_some()
                    || extract_query_n_param(hls_url).is_some();
                all_candidates.push(AudioCandidate {
                    stream: build_hls_stream(
                        hls_url,
                        "hls",
                        "aac",
                        "none",
                        None,
                        None,
                        &video_meta,
                        &resp.user_agent,
                    ),
                    meta: CandidateFormatMeta {
                        format_id: "hls".to_owned(),
                        ext: "m3u8".to_owned(),
                        protocol: "m3u8_native".to_owned(),
                        acodec: "aac".to_owned(),
                        vcodec: "none".to_owned(),
                        abr: None,
                        tbr: None,
                        asr: None,
                        audio_channels: None,
                        filesize: None,
                        is_drc: false,
                        is_default_audio_track: true,
                        is_original_audio_track: true,
                        is_hls: true,
                        is_muxed: false,
                    },
                    cipher_s: None,
                    cipher_sp: "signature".to_owned(),
                    deciphered_by_ytjs: !needs_ejs,
                });
            }
        }

        if all_candidates.is_empty() {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} no audio formats in streamingData"
            );
            errors.push(format!("{client_tag}: no audio formats in streamingData"));
            if track_health {
                record_client_health_failure(has_cookie, spec.key);
            }
            continue;
        }

        let metas: Vec<CandidateFormatMeta> =
            all_candidates.iter().map(|c| c.meta.clone()).collect();
        let mut selected_indices =
            selector.select_indices_with_sort(&metas, prefer_mobile_aac, extra.format_sort);
        if selected_indices.is_empty() && !step.is_explicit && !all_candidates.is_empty() {
            let relaxed = FormatSelector::parse("bestaudio/best").select_indices_with_sort(
                &metas,
                prefer_mobile_aac,
                extra.format_sort,
            );
            if !relaxed.is_empty() {
                log::info!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} format selector `{}` matched 0 of {} candidates on fallback client; relaxing to `bestaudio/best` ({} matched)",
                    format_expr.unwrap_or("bestaudio/best"),
                    all_candidates.len(),
                    relaxed.len(),
                );
                selected_indices = relaxed;
            }
        }
        if selected_indices.is_empty() {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} format selector `{}` matched 0 of {} candidates",
                format_expr.unwrap_or("bestaudio/best"),
                all_candidates.len()
            );
            errors.push(format!("{client_tag}: requested format not available"));
            if track_health {
                record_client_health_failure(has_cookie, spec.key);
            }
            continue;
        }

        let formats_summary: Vec<Value> = all_candidates
            .iter()
            .map(candidate_to_format_summary_json)
            .collect();
        let raw_selected: Vec<AudioCandidate> = selected_indices
            .into_iter()
            .map(|idx| {
                let mut c = all_candidates[idx].clone();
                c.stream.formats_json = formats_summary.clone();
                c
            })
            .collect();

        let (sig_challenges, n_challenges) = collect_unique_challenges(&raw_selected);
        let solved = if (!sig_challenges.is_empty() || !n_challenges.is_empty())
            && step.loopback_origin.is_none()
        {
            if tv_bundle.is_none() && !tv_bundle_failed {
                let fetch_rem = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default()
                    .min(Duration::from_secs(15));
                if !fetch_rem.is_zero() {
                    let fetch_res = tokio::select! {
                        biased;
                        _ = wait_for_cancel(cancel_flag) => return Err("cancelled".to_owned()),
                        res = yt_ejs::fetch_or_cached_tv_player_with_options(
                            client,
                            cache_dir,
                            fetch_rem,
                            ext_args.player_js_variant.as_deref(),
                            ext_args
                                .player_js_version
                                .as_deref()
                                .or(resp.player_id.as_deref()),
                            Some(video_id),
                        ) => res,
                    };
                    match fetch_res {
                        Ok(bundle) => tv_bundle = Some(bundle),
                        Err(e) => {
                            log::warn!(
                                target: "nsh_music_plugin",
                                "virtual yt-dlp video={video_id} yt-ejs failed to fetch player JS: {e}"
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
                let solve_fut = tokio::task::spawn_blocking(move || {
                    yt_ejs::solve_challenges(
                        &bundle_clone,
                        &sigs_clone,
                        &ns_clone,
                        cache_owned.as_deref(),
                    )
                });
                let solve_res = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancel_flag) => return Err("cancelled".to_owned()),
                    joined = solve_fut => joined
                        .map_err(|e| format!("EJS join error: {e}"))
                        .and_then(|r| r),
                };

                match solve_res {
                    Ok(res) => {
                        if res.cold
                            && let Ok(refreshed) = yt_ejs::fetch_or_cached_tv_player_with_options(
                                client,
                                cache_dir,
                                Duration::from_secs(1),
                                Some(&bundle.variant),
                                Some(&bundle.player_id),
                                Some(video_id),
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

        let gvs_pot = ext_args.find_po_token(spec.key, PoTokenContext::Gvs);
        let candidates = finalize_solved_candidates(raw_selected, solved.as_ref(), gvs_pot);
        if candidates.is_empty() {
            log::warn!(
                target: "nsh_music_plugin",
                "virtual yt-dlp video={video_id} client={client_tag} no usable candidates after challenge solving"
            );
            errors.push(format!(
                "{client_tag}: no usable candidates after challenge solving"
            ));
            if track_health {
                record_client_health_failure(has_cookie, spec.key);
            }
            continue;
        }

        let solved_n_outputs: HashSet<String> = solved
            .as_ref()
            .map(|r| r.ns.values().cloned().collect())
            .unwrap_or_default();

        let probe_candidates = cap_preflight_candidates(candidates);
        let total = probe_candidates.len();
        let mut skip_remaining_direct = false;
        let mut transport_or_timeout_failures = 0usize;

        for (idx, candidate) in probe_candidates.into_iter().enumerate() {
            if is_cancelled(cancel_flag) {
                return Err("cancelled".to_owned());
            }
            if !candidate.is_hls && skip_remaining_direct {
                continue;
            }
            let safe_host = sanitize_url_for_log(&candidate.url);
            if !verify_stream_url {
                log::info!(
                    target: "nsh_music_plugin",
                    "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} resolved format={} ext={} hls={} url={safe_host} (preflight skipped)",
                    candidate.format_id,
                    candidate.ext,
                    candidate.is_hls,
                );
                if track_health {
                    record_client_health_success(has_cookie, spec.key);
                }
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
            let cap_timeout = if candidate.is_hls {
                hls_preflight_cap
            } else {
                direct_preflight_cap
            };
            let preflight_timeout = preflight_rem.min(cap_timeout);

            let preflight_res = tokio::select! {
                biased;
                _ = wait_for_cancel(cancel_flag) => {
                    return Err("cancelled".to_owned());
                }
                res = tokio::time::timeout(
                    preflight_timeout,
                    verify_candidate_stream_url_with_ejs(
                        client,
                        &candidate,
                        preflight_timeout,
                        tv_bundle.as_ref(),
                        &solved_n_outputs,
                        cache_dir,
                    ),
                ) => res,
            };

            match preflight_res {
                Ok(Ok(())) => {
                    log::info!(
                        target: "nsh_music_plugin",
                        "virtual yt-dlp video={video_id} client={client_tag} geo={geo_label} resolved candidate={}/{total} format={} ext={} hls={} url={safe_host} (preflight ok)",
                        idx + 1,
                        candidate.format_id,
                        candidate.ext,
                        candidate.is_hls,
                    );
                    if track_health {
                        record_client_health_success(has_cookie, spec.key);
                    }
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
                    if !candidate.is_hls && is_client_wide_http_rejection(&why) {
                        skip_remaining_direct = true;
                    } else if is_transport_or_timeout_error(&why) {
                        transport_or_timeout_failures += 1;
                        if transport_or_timeout_failures >= 2 {
                            break;
                        }
                    }
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
                    transport_or_timeout_failures += 1;
                    if transport_or_timeout_failures >= 2 {
                        break;
                    }
                }
            }
        }

        if track_health {
            record_client_health_failure(has_cookie, spec.key);
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
    let deadline = Instant::now() + timeout;
    let remaining = |phase: &str| -> Result<Duration, String> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| format!("{phase} timed out"))
    };
    let send_timeout = remaining("HLS playlist request")?;
    let mut resp = tokio::time::timeout(
        send_timeout,
        client
            .get(url)
            .header("user-agent", user_agent)
            .timeout(send_timeout)
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
        let read_timeout = remaining("HLS read")?;
        match tokio::time::timeout(read_timeout, resp.chunk())
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

#[cfg(test)]
async fn verify_candidate_stream_url(
    client: &reqwest::Client,
    candidate: &ResolvedAudioStream,
    timeout: Duration,
) -> Result<(), String> {
    verify_candidate_stream_url_with_ejs(client, candidate, timeout, None, &HashSet::new(), None)
        .await
}

async fn verify_candidate_stream_url_with_ejs(
    client: &reqwest::Client,
    candidate: &ResolvedAudioStream,
    timeout: Duration,
    tv_bundle: Option<&yt_ejs::TvPlayerBundle>,
    solved_n_outputs: &HashSet<String>,
    cache_dir: Option<&Path>,
) -> Result<(), String> {
    let preflight_deadline = Instant::now() + timeout;
    let remaining = |phase: &str| -> Result<Duration, String> {
        preflight_deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| format!("{phase} timed out"))
    };

    if candidate.is_hls {
        let mut playlist_url = candidate.url.clone();
        let master_timeout = remaining("HLS playlist request")?;
        let mut text = fetch_text_prefix(
            client,
            &playlist_url,
            &candidate.user_agent,
            master_timeout,
            256 * 1024,
        )
        .await?;
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        if !trimmed.starts_with("#EXTM3U") && !trimmed.starts_with("#EXT") {
            return Err("HLS body did not start with #EXTM3U/#EXT".to_owned());
        }

        // If this is a master playlist, follow 1 hop to the selected audio-bearing variant playlist.
        if let Some(variant_url) = select_best_youtube_hls_variant(&playlist_url, &text) {
            playlist_url = variant_url;
            let variant_timeout = remaining("HLS variant request")?;
            text = fetch_text_prefix(
                client,
                &playlist_url,
                &candidate.user_agent,
                variant_timeout,
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
        // If a segment has a separate `/n/` not already solved on the variant URL, solve it first.
        if let Some(mut seg_url) = extract_first_hls_http_segment_url(&text) {
            let seg_n = extract_path_n_param(&seg_url).or_else(|| extract_query_n_param(&seg_url));
            if let Some(ref raw_seg_n) = seg_n
                && !solved_n_outputs.contains(raw_seg_n)
                && let Some(bundle) = tv_bundle
            {
                let bundle_clone = bundle.clone();
                let n_vec = vec![raw_seg_n.clone()];
                let cache_owned = cache_dir.map(Path::to_path_buf);
                if let Ok(Ok(res)) = tokio::task::spawn_blocking(move || {
                    yt_ejs::solve_challenges(&bundle_clone, &[], &n_vec, cache_owned.as_deref())
                })
                .await
                    && let Some(solved_seg_n) = res.ns.get(raw_seg_n)
                {
                    if extract_path_n_param(&seg_url).is_some() {
                        seg_url = replace_path_n_param(&seg_url, solved_seg_n);
                    } else {
                        seg_url = replace_query_param_preserve_order(&seg_url, "n", solved_seg_n);
                    }
                }
            }

            let seg_timeout = remaining("HLS segment request")?;
            let mut seg_resp = tokio::time::timeout(
                seg_timeout,
                client
                    .get(&seg_url)
                    .header("user-agent", &candidate.user_agent)
                    .timeout(seg_timeout)
                    .send(),
            )
            .await
            .map_err(|_| "HLS segment request timed out".to_owned())?
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
            loop {
                let seg_read_timeout = remaining("HLS segment read")?;
                match tokio::time::timeout(seg_read_timeout, seg_resp.chunk())
                    .await
                    .map_err(|_| "HLS segment read timed out".to_owned())?
                    .map_err(|e| format!("HLS segment read error: {e}"))?
                {
                    Some(chunk) => {
                        received += chunk.len();
                        if received >= 1 {
                            break;
                        }
                    }
                    None => break,
                }
            }
            if received == 0 {
                return Err("HLS segment empty body".to_owned());
            }
        }
        return Ok(());
    }

    let send_timeout = remaining("stream request")?;
    let mut resp = tokio::time::timeout(
        send_timeout,
        client
            .get(&candidate.url)
            .header("range", "bytes=0-63")
            .header("user-agent", &candidate.user_agent)
            .timeout(send_timeout)
            .send(),
    )
    .await
    .map_err(|_| "stream request timed out".to_owned())?
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
    loop {
        let read_timeout = remaining("stream read")?;
        match tokio::time::timeout(read_timeout, resp.chunk())
            .await
            .map_err(|_| "stream read timed out".to_owned())?
            .map_err(|e| format!("stream read error: {e}"))?
        {
            Some(chunk) => {
                received += chunk.len();
                if received >= 1 {
                    break;
                }
            }
            None => break,
        }
    }
    if received == 0 {
        return Err("empty stream body".to_owned());
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct VideoMeta {
    video_id: String,
    title: String,
    duration_secs: Option<u64>,
    uploader: String,
    channel: String,
    channel_id: String,
    is_live: bool,
    live_status: String,
    thumbnail: String,
    thumbnails_json: Vec<Value>,
}

#[derive(Debug, Clone)]
struct AudioCandidate {
    stream: ResolvedAudioStream,
    meta: CandidateFormatMeta,
    cipher_s: Option<String>,
    cipher_sp: String,
    deciphered_by_ytjs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedFormatUrl {
    base_url: String,
    s: Option<String>,
    sp: String,
}

fn parse_cipher_query(sc: &str) -> Option<ParsedFormatUrl> {
    if sc.is_empty() {
        return None;
    }
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

#[cfg(test)]
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
    parse_cipher_query(sc)
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
        if cand.deciphered_by_ytjs {
            continue;
        }
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
    gvs_po_token: Option<&str>,
) -> Vec<ResolvedAudioStream> {
    let empty_map = HashMap::new();
    let sigs = solved.map_or(&empty_map, |r| &r.sigs);
    let ns = solved.map_or(&empty_map, |r| &r.ns);

    let mut out = Vec::with_capacity(raw_candidates.len());
    for mut cand in raw_candidates {
        let maybe_url = if cand.deciphered_by_ytjs {
            Some(cand.stream.url.clone())
        } else {
            apply_solved_sig_and_n(
                &cand.stream.url,
                cand.cipher_s.as_deref(),
                &cand.cipher_sp,
                sigs,
                ns,
            )
        };
        if let Some(mut final_url) = maybe_url {
            if let Some(pot) = gvs_po_token {
                final_url = replace_query_param_preserve_order(&final_url, "pot", pot);
            }
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
            && parse_hls_attr(line, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"))
            && let Some(uri) = parse_hls_attr(line, "URI").filter(|u| !u.is_empty())
        {
            let resolved = resolve_hls_uri(master_url, &uri);
            let group_codec = parse_hls_attr(line, "GROUP-ID")
                .and_then(|g| group_codecs.get(&g))
                .map(|c| c.to_ascii_lowercase())
                .unwrap_or_default();
            let itag = extract_itag_from_url(&resolved).unwrap_or(0);
            let is_aac_lc =
                group_codec.contains("mp4a.40.2") || matches!(itag, 140 | 141 | 233 | 234);
            let is_default =
                parse_hls_attr(line, "DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES"));
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
                stream_variants.push((
                    itag_rank,
                    is_aac_lc,
                    std::cmp::Reverse(bandwidth),
                    resolved,
                ));
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

#[allow(clippy::too_many_arguments)]
fn build_hls_stream(
    url: &str,
    format_id: &str,
    acodec: &str,
    vcodec: &str,
    abr: Option<f64>,
    tbr: Option<f64>,
    meta: &VideoMeta,
    user_agent: &str,
) -> ResolvedAudioStream {
    let bitrate = abr.or(tbr).map(|k| (k * 1000.0) as u64).unwrap_or(0);
    ResolvedAudioStream {
        video_id: meta.video_id.clone(),
        title: meta.title.clone(),
        url: url.to_owned(),
        mime_type: "application/vnd.apple.mpegurl".to_owned(),
        ext: "m3u8".to_owned(),
        acodec: acodec.to_owned(),
        vcodec: vcodec.to_owned(),
        format_id: format_id.to_owned(),
        bitrate,
        abr,
        tbr,
        asr: None,
        audio_channels: Some(2),
        filesize: None,
        duration_secs: meta.duration_secs,
        protocol: "m3u8_native".to_owned(),
        uploader: meta.uploader.clone(),
        channel: meta.channel.clone(),
        channel_id: meta.channel_id.clone(),
        is_live: meta.is_live,
        live_status: meta.live_status.clone(),
        thumbnail: meta.thumbnail.clone(),
        thumbnails_json: meta.thumbnails_json.clone(),
        user_agent: user_agent.to_owned(),
        is_hls: true,
        formats_json: Vec::new(),
    }
}

fn split_hls_codecs(codecs_attr: &str) -> (String, String) {
    let mut vcodec = "none".to_owned();
    let mut acodec = "none".to_owned();
    for part in codecs_attr.split(',') {
        let c = part.trim();
        let lower = c.to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }
        if lower.starts_with("mp4a")
            || lower.starts_with("opus")
            || lower.starts_with("ac-3")
            || lower.starts_with("ec-3")
            || lower.starts_with("vorbis")
        {
            acodec = c.to_owned();
        } else {
            vcodec = c.to_owned();
        }
    }
    (acodec, vcodec)
}

/// Expands a YouTube HLS master playlist into individual `AudioCandidate` entries per variant
/// (each with real `format_id`, `protocol: "m3u8_native"`, `acodec`, `vcodec`, `abr`, `tbr`)
/// so `FormatSelector` can select variants directly (e.g. `bestaudio[protocol^=m3u8]/93/94/92/95/91`).
fn expand_hls_master_variants(
    master_url: &str,
    playlist_text: &str,
    meta: &VideoMeta,
    user_agent: &str,
) -> Vec<AudioCandidate> {
    let lines: Vec<&str> = playlist_text.lines().map(str::trim).collect();
    let mut group_codecs: HashMap<String, String> = HashMap::new();
    for line in &lines {
        if line.starts_with("#EXT-X-STREAM-INF:")
            && let Some(group) = parse_hls_attr(line, "AUDIO")
            && let Some(codecs) = parse_hls_attr(line, "CODECS")
        {
            group_codecs.insert(group, codecs);
        }
    }

    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();
    let mut audio_idx = 0usize;

    // 1. Audio-only `#EXT-X-MEDIA:TYPE=AUDIO` renditions (`vcodec = "none"`).
    for line in &lines {
        if line.starts_with("#EXT-X-MEDIA:")
            && parse_hls_attr(line, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"))
            && let Some(uri) = parse_hls_attr(line, "URI").filter(|u| !u.is_empty())
        {
            let resolved = resolve_hls_uri(master_url, &uri);
            if !seen_urls.insert(resolved.clone()) {
                continue;
            }
            let itag = extract_itag_from_url(&resolved);
            let format_id = itag.map(|n| n.to_string()).unwrap_or_else(|| {
                audio_idx += 1;
                format!("hls-audio-{audio_idx}")
            });
            let group_codec = parse_hls_attr(line, "GROUP-ID")
                .and_then(|g| group_codecs.get(&g))
                .cloned()
                .unwrap_or_default();
            let (parsed_a, _) = split_hls_codecs(&group_codec);
            let acodec = if parsed_a != "none" {
                parsed_a
            } else if matches!(itag, Some(140 | 141 | 233 | 234)) {
                "mp4a.40.2".to_owned()
            } else {
                "aac".to_owned()
            };
            let abr = match itag {
                Some(141) => Some(256.0),
                Some(140 | 234) => Some(128.0),
                Some(233 | 139) => Some(48.0),
                _ => None,
            };
            let is_default =
                parse_hls_attr(line, "DEFAULT").is_none_or(|d| d.eq_ignore_ascii_case("YES"));
            let name_lower = parse_hls_attr(line, "NAME")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_drc = name_lower.contains("drc");
            let needs_ejs = extract_path_n_param(&resolved).is_some()
                || extract_query_n_param(&resolved).is_some();
            let stream = build_hls_stream(
                &resolved, &format_id, &acodec, "none", abr, abr, meta, user_agent,
            );
            out.push(AudioCandidate {
                meta: CandidateFormatMeta {
                    format_id,
                    ext: "m3u8".to_owned(),
                    protocol: "m3u8_native".to_owned(),
                    acodec,
                    vcodec: "none".to_owned(),
                    abr,
                    tbr: abr,
                    asr: None,
                    audio_channels: Some(2),
                    filesize: None,
                    is_drc,
                    is_default_audio_track: is_default,
                    is_original_audio_track: is_default || name_lower.contains("original"),
                    is_hls: true,
                    is_muxed: false,
                },
                stream,
                cipher_s: None,
                cipher_sp: "signature".to_owned(),
                deciphered_by_ytjs: !needs_ejs,
            });
        }
    }

    // 2. `#EXT-X-STREAM-INF:` variants (muxed `91/92/93/94/95/96` or audio-only streams).
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let codecs_raw = parse_hls_attr(line, "CODECS").unwrap_or_default();
            let (mut acodec, vcodec) = split_hls_codecs(&codecs_raw);
            let bandwidth = parse_hls_attr(line, "BANDWIDTH").and_then(|b| b.parse::<u64>().ok());
            let mut j = i + 1;
            while j < lines.len() && (lines[j].is_empty() || lines[j].starts_with('#')) {
                j += 1;
            }
            if j < lines.len() {
                let resolved = resolve_hls_uri(master_url, lines[j]);
                if seen_urls.insert(resolved.clone()) {
                    let itag = extract_itag_from_url(&resolved);
                    if acodec == "none" && matches!(itag, Some(91..=96)) {
                        acodec = if matches!(itag, Some(91 | 92)) {
                            "mp4a.40.5".to_owned()
                        } else {
                            "mp4a.40.2".to_owned()
                        };
                    }
                    if acodec != "none" {
                        let format_id = itag
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| format!("hls-{}", bandwidth.unwrap_or(0)));
                        let tbr = bandwidth.map(|b| (b as f64) / 1000.0);
                        let is_muxed = vcodec != "none";
                        let abr = if !is_muxed {
                            tbr
                        } else {
                            match itag {
                                Some(93..=96) => Some(128.0),
                                Some(91 | 92) => Some(48.0),
                                _ => tbr,
                            }
                        };
                        let needs_ejs = extract_path_n_param(&resolved).is_some()
                            || extract_query_n_param(&resolved).is_some();
                        let stream = build_hls_stream(
                            &resolved, &format_id, &acodec, &vcodec, abr, tbr, meta, user_agent,
                        );
                        out.push(AudioCandidate {
                            meta: CandidateFormatMeta {
                                format_id,
                                ext: "m3u8".to_owned(),
                                protocol: "m3u8_native".to_owned(),
                                acodec,
                                vcodec,
                                abr,
                                tbr,
                                asr: None,
                                audio_channels: Some(2),
                                filesize: None,
                                is_drc: false,
                                is_default_audio_track: true,
                                is_original_audio_track: true,
                                is_hls: true,
                                is_muxed,
                            },
                            stream,
                            cipher_s: None,
                            cipher_sp: "signature".to_owned(),
                            deciphered_by_ytjs: !needs_ejs,
                        });
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    out
}

fn candidate_to_format_summary_json(c: &AudioCandidate) -> Value {
    json!({
        "format_id": c.meta.format_id,
        "ext": c.meta.ext,
        "protocol": c.meta.protocol,
        "acodec": c.meta.acodec,
        "vcodec": c.meta.vcodec,
        "abr": c.meta.abr,
        "tbr": c.meta.tbr,
        "asr": c.meta.asr,
        "audio_channels": c.meta.audio_channels,
        "filesize": c.meta.filesize,
    })
}

fn infer_direct_ext_and_codecs(mime_lower: &str, is_muxed: bool) -> (String, String, String) {
    let ext = if mime_lower.starts_with("audio/mp4") {
        "m4a"
    } else if mime_lower.starts_with("video/mp4") {
        "mp4"
    } else if mime_lower.starts_with("audio/webm") || mime_lower.starts_with("video/webm") {
        "webm"
    } else if mime_lower.starts_with("audio/mpeg") {
        "mp3"
    } else if mime_lower.starts_with("video/3gpp") {
        "3gp"
    } else {
        "audio"
    };

    let mut acodec = "none".to_owned();
    let mut vcodec = "none".to_owned();

    if let Some(pos) = mime_lower.find("codecs=") {
        let rest = &mime_lower[pos + 7..];
        let codecs_raw = if let Some(s) = rest.strip_prefix('"') {
            s.split('"').next().unwrap_or("")
        } else if let Some(s) = rest.strip_prefix('\'') {
            s.split('\'').next().unwrap_or("")
        } else {
            rest.split(';').next().unwrap_or("")
        };
        let (a, v) = split_hls_codecs(codecs_raw);
        acodec = a;
        vcodec = v;
    }

    if acodec == "none" {
        if mime_lower.contains("opus") {
            acodec = "opus".to_owned();
        } else if mime_lower.contains("mp4a.40.5") {
            acodec = "mp4a.40.5".to_owned();
        } else if mime_lower.contains("mp4a") {
            acodec = "mp4a.40.2".to_owned();
        } else if mime_lower.contains("vorbis") {
            acodec = "vorbis".to_owned();
        } else if mime_lower.starts_with("audio/") || is_muxed {
            acodec = "unknown".to_owned();
        }
    }
    if is_muxed && vcodec == "none" {
        vcodec = "unknown".to_owned();
    }

    (ext.to_owned(), acodec, vcodec)
}

fn xml_unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..semi];
            match entity {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                _ if entity.starts_with('#') => {
                    let code = if let Some(hex) = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        entity[1..].parse::<u32>().ok()
                    };
                    if let Some(ch) = code.and_then(char::from_u32) {
                        out.push(ch);
                    } else {
                        out.push('&');
                        out.push_str(entity);
                        out.push(';');
                    }
                }
                _ => {
                    out.push('&');
                    out.push_str(entity);
                    out.push(';');
                }
            }
            rest = &after[semi + 1..];
        } else {
            out.push_str(&rest[amp..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

fn extract_xml_attr(tag_header: &str, attr_name: &str) -> Option<String> {
    let mut search = tag_header;
    while let Some(pos) = find_ascii_ci(search, attr_name) {
        let before_ok = pos == 0
            || search.as_bytes()[pos - 1].is_ascii_whitespace()
            || search.as_bytes()[pos - 1] == b'<';
        let after = &search[pos + attr_name.len()..];
        if before_ok {
            let after_ws = after.trim_start();
            if let Some(after_eq) = after_ws.strip_prefix('=') {
                let val_start = after_eq.trim_start();
                if let Some(q) = val_start
                    .bytes()
                    .next()
                    .filter(|&b| b == b'"' || b == b'\'')
                {
                    let inner = &val_start[1..];
                    if let Some(end) = inner.find(q as char) {
                        return Some(xml_unescape(&inner[..end]));
                    }
                }
            }
        }
        search = after;
    }
    None
}

/// Parses a YouTube DASH MPD XML document and extracts audio `<AdaptationSet>` -> `<Representation>`
/// -> `<BaseURL>` candidates (`itag`, `mimeType`/`codecs`, `bandwidth`, `audioSamplingRate`).
fn parse_dash_mpd_audio_candidates(
    mpd_xml: &str,
    meta: &VideoMeta,
    user_agent: &str,
) -> Vec<AudioCandidate> {
    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();
    let mut rest = mpd_xml;

    while let Some(as_start) = find_ascii_ci(rest, "<AdaptationSet") {
        let after_as = &rest[as_start..];
        let Some(as_open_end) = after_as.find('>') else {
            break;
        };
        let as_header = &after_as[..as_open_end];
        let body_after_open = &after_as[as_open_end + 1..];
        let Some(as_close) = find_ascii_ci(body_after_open, "</AdaptationSet>") else {
            break;
        };
        let as_body = &body_after_open[..as_close];
        rest = &body_after_open[as_close + "</AdaptationSet>".len()..];

        let as_mime = extract_xml_attr(as_header, "mimeType").unwrap_or_default();
        let as_content_type = extract_xml_attr(as_header, "contentType").unwrap_or_default();
        let as_codecs = extract_xml_attr(as_header, "codecs").unwrap_or_default();
        let as_asr =
            extract_xml_attr(as_header, "audioSamplingRate").and_then(|s| s.parse::<u64>().ok());

        let mut rep_rest = as_body;
        while let Some(rep_start) = find_ascii_ci(rep_rest, "<Representation") {
            let after_rep = &rep_rest[rep_start..];
            let Some(rep_open_end) = after_rep.find('>') else {
                break;
            };
            let rep_header = &after_rep[..rep_open_end];
            let rep_after_open = &after_rep[rep_open_end + 1..];
            let Some(rep_close) = find_ascii_ci(rep_after_open, "</Representation>") else {
                break;
            };
            let rep_body = &rep_after_open[..rep_close];
            rep_rest = &rep_after_open[rep_close + "</Representation>".len()..];

            let mime_base = extract_xml_attr(rep_header, "mimeType")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| as_mime.clone());
            let is_audio = mime_base.to_ascii_lowercase().starts_with("audio/")
                || as_content_type.eq_ignore_ascii_case("audio");
            if !is_audio {
                continue;
            }

            let Some(bu_start) = find_ascii_ci(rep_body, "<BaseURL") else {
                continue;
            };
            let after_bu = &rep_body[bu_start..];
            let Some(bu_open_end) = after_bu.find('>') else {
                continue;
            };
            let bu_inner = &after_bu[bu_open_end + 1..];
            let Some(bu_close) = find_ascii_ci(bu_inner, "</BaseURL>") else {
                continue;
            };
            let base_url = xml_unescape(bu_inner[..bu_close].trim());
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                continue;
            }
            if !seen_urls.insert(base_url.clone()) {
                continue;
            }

            let codecs = extract_xml_attr(rep_header, "codecs")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| as_codecs.clone());
            let mime_type =
                if !codecs.is_empty() && !mime_base.to_ascii_lowercase().contains("codecs=") {
                    if mime_base.is_empty() {
                        format!("audio/mp4; codecs=\"{codecs}\"")
                    } else {
                        format!("{mime_base}; codecs=\"{codecs}\"")
                    }
                } else if mime_base.is_empty() {
                    "audio/mp4".to_owned()
                } else {
                    mime_base
                };

            let mime_lower = mime_type.to_ascii_lowercase();
            let (ext, acodec, vcodec) = infer_direct_ext_and_codecs(&mime_lower, false);
            if acodec == "none" {
                continue;
            }

            let rep_id = extract_xml_attr(rep_header, "id").filter(|s| !s.is_empty());
            let itag_str = rep_id
                .or_else(|| extract_itag_from_url(&base_url).map(|n| n.to_string()))
                .unwrap_or_else(|| "dash-audio".to_owned());
            let bandwidth = extract_xml_attr(rep_header, "bandwidth")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let kbps = (bandwidth > 0).then(|| (bandwidth as f64) / 1000.0);
            let asr = extract_xml_attr(rep_header, "audioSamplingRate")
                .and_then(|s| s.parse::<u64>().ok())
                .or(as_asr);
            let needs_ejs = extract_query_n_param(&base_url).is_some()
                || extract_path_n_param(&base_url).is_some();

            let stream = ResolvedAudioStream {
                video_id: meta.video_id.clone(),
                title: meta.title.clone(),
                url: base_url,
                mime_type,
                ext: ext.clone(),
                acodec: acodec.clone(),
                vcodec: vcodec.clone(),
                format_id: itag_str.clone(),
                bitrate: bandwidth,
                abr: kbps,
                tbr: kbps,
                asr,
                audio_channels: Some(2),
                filesize: None,
                duration_secs: meta.duration_secs,
                protocol: "https".to_owned(),
                uploader: meta.uploader.clone(),
                channel: meta.channel.clone(),
                channel_id: meta.channel_id.clone(),
                is_live: meta.is_live,
                live_status: meta.live_status.clone(),
                thumbnail: meta.thumbnail.clone(),
                thumbnails_json: meta.thumbnails_json.clone(),
                user_agent: user_agent.to_owned(),
                is_hls: false,
                formats_json: Vec::new(),
            };

            out.push(AudioCandidate {
                stream,
                meta: CandidateFormatMeta {
                    format_id: itag_str,
                    ext,
                    protocol: "https".to_owned(),
                    acodec,
                    vcodec,
                    abr: kbps,
                    tbr: kbps,
                    asr,
                    audio_channels: Some(2),
                    filesize: None,
                    is_drc: false,
                    is_default_audio_track: true,
                    is_original_audio_track: true,
                    is_hls: false,
                    is_muxed: false,
                },
                cipher_s: None,
                cipher_sp: "signature".to_owned(),
                deciphered_by_ytjs: !needs_ejs,
            });
        }
    }

    out
}

fn candidates_from_youtubejs(
    video_id: &str,
    resp: &YtJsExtractResponse,
) -> (VideoMeta, Vec<AudioCandidate>) {
    let thumbnails_json: Vec<Value> = resp
        .thumbnails
        .iter()
        .map(|t| {
            let mut obj = serde_json::Map::new();
            obj.insert("url".to_owned(), json!(t.url));
            if let Some(w) = t.width {
                obj.insert("width".to_owned(), json!(w));
            }
            if let Some(h) = t.height {
                obj.insert("height".to_owned(), json!(h));
            }
            Value::Object(obj)
        })
        .collect();
    let channel = if resp.channel.is_empty() {
        resp.uploader.clone()
    } else {
        resp.channel.clone()
    };
    let live_status = if resp.live_status.is_empty() {
        if resp.is_live {
            "is_live".to_owned()
        } else {
            "not_live".to_owned()
        }
    } else {
        resp.live_status.clone()
    };
    let video_meta = VideoMeta {
        video_id: video_id.to_owned(),
        title: resp.title.clone(),
        duration_secs: resp.duration_secs,
        uploader: resp.uploader.clone(),
        channel,
        channel_id: resp.channel_id.clone(),
        is_live: resp.is_live,
        live_status,
        thumbnail: resp.thumbnail.clone(),
        thumbnails_json,
    };

    let mut candidates = Vec::new();
    let mut seen_urls = HashSet::new();

    for fmt in &resp.formats {
        if !fmt.has_audio {
            continue;
        }
        let (base_url, cipher_s, cipher_sp, deciphered_by_ytjs) = if fmt.deciphered_by_ytjs
            && let Some(ref u) = fmt.url
            && !u.is_empty()
        {
            (u.clone(), None, "signature".to_owned(), true)
        } else if let Some(parsed) = fmt
            .signature_cipher
            .as_deref()
            .or(fmt.cipher.as_deref())
            .and_then(parse_cipher_query)
        {
            (parsed.base_url, parsed.s, parsed.sp, false)
        } else if let Some(u) = fmt
            .url
            .as_deref()
            .or(fmt.raw_url.as_deref())
            .filter(|s| !s.is_empty())
        {
            (
                u.to_owned(),
                None,
                "signature".to_owned(),
                fmt.deciphered_by_ytjs,
            )
        } else {
            continue;
        };

        let mime_lower = fmt.mime_type.to_ascii_lowercase();
        let is_muxed = fmt.is_muxed || !mime_lower.starts_with("audio/");
        let (ext, acodec, vcodec) = infer_direct_ext_and_codecs(&mime_lower, is_muxed);
        if acodec == "none" {
            continue;
        }

        if !seen_urls.insert(base_url.clone()) {
            continue;
        }

        let bitrate = fmt.average_bitrate.unwrap_or(fmt.bitrate);
        let kbps = (bitrate > 0).then(|| (bitrate as f64) / 1000.0);
        let abr = if is_muxed { None } else { kbps };
        let tbr = kbps;
        let format_id = if fmt.is_drc && fmt.itag > 0 {
            format!("{}-drc", fmt.itag)
        } else {
            fmt.itag.to_string()
        };

        let stream = ResolvedAudioStream {
            video_id: video_meta.video_id.clone(),
            title: video_meta.title.clone(),
            url: base_url,
            mime_type: fmt.mime_type.clone(),
            ext: ext.clone(),
            acodec: acodec.clone(),
            vcodec: vcodec.clone(),
            format_id: format_id.clone(),
            bitrate,
            abr,
            tbr,
            asr: fmt.audio_sample_rate,
            audio_channels: fmt.audio_channels,
            filesize: fmt.content_length,
            duration_secs: video_meta.duration_secs,
            protocol: "https".to_owned(),
            uploader: video_meta.uploader.clone(),
            channel: video_meta.channel.clone(),
            channel_id: video_meta.channel_id.clone(),
            is_live: video_meta.is_live,
            live_status: video_meta.live_status.clone(),
            thumbnail: video_meta.thumbnail.clone(),
            thumbnails_json: video_meta.thumbnails_json.clone(),
            user_agent: resp.user_agent.clone(),
            is_hls: false,
            formats_json: Vec::new(),
        };

        candidates.push(AudioCandidate {
            stream,
            meta: CandidateFormatMeta {
                format_id,
                ext,
                protocol: "https".to_owned(),
                acodec,
                vcodec,
                abr,
                tbr,
                asr: fmt.audio_sample_rate,
                audio_channels: fmt.audio_channels,
                filesize: fmt.content_length,
                is_drc: fmt.is_drc,
                is_default_audio_track: fmt.is_default_audio_track,
                is_original_audio_track: fmt.is_original_audio_track,
                is_hls: false,
                is_muxed,
            },
            cipher_s,
            cipher_sp,
            deciphered_by_ytjs,
        });
    }

    (video_meta, candidates)
}

type YtdlpExhaustedTracker = Arc<Mutex<Option<(String, Instant)>>>;

/// Pure-Rust in-process `yt-dlp` runtime engine.
pub struct VirtualYtdlpShim;

fn build_effective_http_client(
    default_client: &reqwest::Client,
    parsed: &YtdlpParsedArgs,
) -> Result<reqwest::Client, String> {
    if parsed.proxy.is_none()
        && !parsed.no_check_certificates
        && parsed.socket_timeout_secs.is_none()
    {
        return Ok(default_client.clone());
    }
    let mut builder = reqwest::Client::builder()
        .pool_idle_timeout(Some(Duration::from_secs(30)))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true);
    if let Some(secs) = parsed.socket_timeout_secs {
        builder = builder.connect_timeout(Duration::from_secs(secs.max(1)));
    }
    if parsed.no_check_certificates {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(ref proxy_raw) = parsed.proxy {
        let p = proxy_raw.trim();
        if p.is_empty() || p == "\"\"" || p == "''" {
            builder = builder.no_proxy();
        } else {
            let proxy =
                reqwest::Proxy::all(p).map_err(|e| format!("invalid --proxy `{p}`: {e}"))?;
            builder = builder.proxy(proxy);
        }
    }
    builder
        .build()
        .map_err(|e| format!("failed to build custom HTTP client: {e}"))
}

fn collect_forwardable_extra_headers(headers: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in ["X-Goog-PageId", "Accept-Language"] {
        if let Some(val) = find_header(headers, name)
            && !val.trim().is_empty()
        {
            out.push((name.to_owned(), val.trim().to_owned()));
        }
    }
    out
}

fn flat_collection_entry_json(entry: &YtJsCollectionEntry) -> Value {
    let webpage_url = if entry.url.is_empty() {
        format!("https://www.youtube.com/watch?v={}", entry.id)
    } else {
        entry.url.clone()
    };
    let thumbnails = if entry.thumbnail.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "url": entry.thumbnail })]
    };
    json!({
        "_type": "url",
        "ie_key": "Youtube",
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "id": entry.id,
        "title": entry.title,
        "track": entry.title,
        "artist": entry.uploader,
        "uploader": entry.uploader,
        "channel": entry.uploader,
        "channel_id": entry.channel_id,
        "duration": entry.duration_secs,
        "thumbnail": entry.thumbnail,
        "thumbnails": thumbnails,
        "url": webpage_url,
        "webpage_url": webpage_url,
    })
}

fn build_playlist_single_json(
    coll: &YtJsCollectionResponse,
    entries: &[YtJsCollectionEntry],
    original_url: &str,
    is_search: bool,
) -> Value {
    let entries_json: Vec<Value> = entries.iter().map(flat_collection_entry_json).collect();
    json!({
        "_type": "playlist",
        "id": coll.id,
        "title": coll.title,
        "uploader": coll.uploader,
        "webpage_url": coll.webpage_url,
        "original_url": original_url,
        "extractor": if is_search { "youtube:search" } else { "youtube:tab" },
        "extractor_key": if is_search { "YoutubeSearch" } else { "YoutubeTab" },
        "playlist_count": entries_json.len(),
        "entries": entries_json,
    })
}

fn select_collection_entries(
    entries: &[YtJsCollectionEntry],
    parsed: &YtdlpParsedArgs,
    search_cap: Option<usize>,
) -> Vec<YtJsCollectionEntry> {
    if let Some(ref spec) = parsed.playlist_items {
        let indices = parse_playlist_items_indices(spec, entries.len());
        return indices
            .into_iter()
            .filter_map(|idx| entries.get(idx).cloned())
            .collect();
    }
    let cap = search_cap.unwrap_or(entries.len());
    entries.iter().take(cap).cloned().collect()
}

fn estimate_collection_fetch_limit(parsed: &YtdlpParsedArgs, default_limit: usize) -> usize {
    let Some(ref spec) = parsed.playlist_items else {
        return default_limit.clamp(1, 200);
    };
    let mut max_pos = default_limit;
    for part in spec.split(',') {
        for tok in part.split([':', '-']) {
            if let Ok(n) = tok.trim().parse::<usize>()
                && n > max_pos
            {
                max_pos = n;
            }
        }
    }
    max_pos.clamp(1, 200)
}

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
        Self::spawn_with_origin_cache_and_tracker(
            args,
            http_client,
            base_origin,
            prefer_mobile_aac,
            cache_dir,
            None,
        )
    }

    fn spawn_with_origin_cache_and_tracker(
        args: &[String],
        http_client: reqwest::Client,
        base_origin: String,
        prefer_mobile_aac: bool,
        cache_dir: Option<PathBuf>,
        exhausted_tracker: Option<YtdlpExhaustedTracker>,
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

                let Some(target) = parsed.target.clone() else {
                    pipe_bg.push_stderr("ERROR: [youtube] You must provide at least one URL");
                    pipe_bg.finish(1);
                    return;
                };

                let Some(classified) = classify_ytdlp_target(&target, &parsed) else {
                    pipe_bg.push_stderr(&format!("ERROR: [generic] Unsupported URL: {target}"));
                    pipe_bg.finish(1);
                    return;
                };

                let effective_client = match build_effective_http_client(&http_client, &parsed) {
                    Ok(c) => c,
                    Err(e) => {
                        pipe_bg.push_stderr(&format!("ERROR: [youtube] {e}"));
                        pipe_bg.finish(1);
                        return;
                    }
                };

                let cookie = find_header(&parsed.headers, "Cookie")
                    .map(str::to_owned)
                    .or_else(|| {
                        parsed
                            .cookies_file
                            .as_deref()
                            .and_then(read_netscape_cookies_file)
                    });
                let extra_headers = collect_forwardable_extra_headers(&parsed.headers);
                let socket_timeout = parsed.socket_timeout_secs.map(Duration::from_secs);
                let format_sort = FormatSortSpec::parse(&parsed.format_sort);
                let active_sort = (!format_sort.is_empty()).then_some(&format_sort);
                let ext_args = YoutubeExtractorArgs::parse(&parsed.extractor_args);
                let verify = !is_loopback_origin(&base_origin);
                let loopback_origin =
                    is_loopback_origin(&base_origin).then(|| base_origin.trim_end_matches('/').to_owned());

                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        pipe_bg.push_stderr(&format!(
                            "ERROR: [youtube] runtime init failed: {e}"
                        ));
                        pipe_bg.finish(1);
                        return;
                    }
                };

                match classified {
                    YtdlpTarget::Video(video_id) => {
                        let resolved = rt.block_on(resolve_innertube_stream_inner(
                            &effective_client,
                            &base_origin,
                            &video_id,
                            cookie.as_deref(),
                            parsed.format_expr.as_deref(),
                            &ext_args,
                            prefer_mobile_aac,
                            verify,
                            DEFAULT_CASCADE_BUDGET,
                            cache_dir.as_deref(),
                            CascadeExtraOptions {
                                format_sort: active_sort,
                                geo_bypass_country: parsed.geo_bypass_country.as_deref(),
                                extra_headers: &extra_headers,
                                socket_timeout,
                                cancel_flag: Some(pipe_bg.cancel_flag()),
                            },
                        ));

                        if pipe_bg.is_killed() {
                            return;
                        }

                        match resolved {
                            Ok(stream) => {
                                if let Some(ref tracker) = exhausted_tracker
                                    && let Ok(mut guard) = tracker.lock()
                                {
                                    *guard = None;
                                }
                                for line in format_ytdlp_output_lines(&stream, &parsed) {
                                    pipe_bg.push_stdout_line(&line);
                                }
                                pipe_bg.finish(0);
                            }
                            Err(reason) => {
                                if let Some(ref tracker) = exhausted_tracker
                                    && let Ok(mut guard) = tracker.lock()
                                {
                                    *guard = Some((video_id.clone(), Instant::now()));
                                }
                                pipe_bg.push_stderr(&format!("ERROR: [youtube] {video_id}: {reason}"));
                                pipe_bg.finish(1);
                            }
                        }
                    }
                    YtdlpTarget::Search { query, count } => {
                        let fetch_limit = estimate_collection_fetch_limit(&parsed, count);
                        let coll_req = YtJsCollectionRequest {
                            kind: YtJsCollectionKind::Search {
                                query: query.clone(),
                                limit: fetch_limit,
                            },
                            cookie: cookie.clone(),
                            lang: ext_args.lang.clone(),
                            location: parsed.geo_bypass_country.clone(),
                            extra_headers: extra_headers.clone(),
                            loopback_origin: loopback_origin.clone(),
                            cache_dir: cache_dir.clone(),
                            timeout: socket_timeout
                                .map_or(Duration::from_secs(15), |t| t.min(Duration::from_secs(15))),
                        };
                        let coll_res = rt.block_on(async {
                            tokio::select! {
                                biased;
                                _ = wait_for_cancel(Some(pipe_bg.cancel_flag())) => Err("cancelled".to_owned()),
                                res = extract_collection_via_youtubejs(&effective_client, coll_req) => res,
                            }
                        });
                        if pipe_bg.is_killed() {
                            return;
                        }
                        let coll = match coll_res {
                            Ok(c) if c.ok => c,
                            Ok(c) => {
                                let msg = c.error.unwrap_or_else(|| "search failed".to_owned());
                                pipe_bg.push_stderr(&format!("ERROR: [youtube:search] {query}: {msg}"));
                                pipe_bg.finish(1);
                                return;
                            }
                            Err(e) => {
                                pipe_bg.push_stderr(&format!("ERROR: [youtube:search] {query}: {e}"));
                                pipe_bg.finish(1);
                                return;
                            }
                        };
                        let selected = select_collection_entries(&coll.entries, &parsed, Some(count));
                        emit_collection_output(
                            &rt,
                            &pipe_bg,
                            &effective_client,
                            &base_origin,
                            &target,
                            &parsed,
                            &ext_args,
                            cookie.as_deref(),
                            active_sort,
                            &extra_headers,
                            socket_timeout,
                            prefer_mobile_aac,
                            verify,
                            cache_dir.as_deref(),
                            &coll,
                            &selected,
                            true,
                        );
                    }
                    YtdlpTarget::Playlist { list_id, is_music } => {
                        let fetch_limit = estimate_collection_fetch_limit(&parsed, 50);
                        let coll_req = YtJsCollectionRequest {
                            kind: YtJsCollectionKind::Playlist {
                                list_id: list_id.clone(),
                                is_music,
                                limit: fetch_limit,
                            },
                            cookie: cookie.clone(),
                            lang: ext_args.lang.clone(),
                            location: parsed.geo_bypass_country.clone(),
                            extra_headers: extra_headers.clone(),
                            loopback_origin: loopback_origin.clone(),
                            cache_dir: cache_dir.clone(),
                            timeout: socket_timeout
                                .map_or(Duration::from_secs(15), |t| t.min(Duration::from_secs(15))),
                        };
                        let coll_res = rt.block_on(async {
                            tokio::select! {
                                biased;
                                _ = wait_for_cancel(Some(pipe_bg.cancel_flag())) => Err("cancelled".to_owned()),
                                res = extract_collection_via_youtubejs(&effective_client, coll_req) => res,
                            }
                        });
                        if pipe_bg.is_killed() {
                            return;
                        }
                        let coll = match coll_res {
                            Ok(c) if c.ok => c,
                            Ok(c) => {
                                let msg = c.error.unwrap_or_else(|| "playlist extraction failed".to_owned());
                                pipe_bg.push_stderr(&format!("ERROR: [youtube:tab] {list_id}: {msg}"));
                                pipe_bg.finish(1);
                                return;
                            }
                            Err(e) => {
                                pipe_bg.push_stderr(&format!("ERROR: [youtube:tab] {list_id}: {e}"));
                                pipe_bg.finish(1);
                                return;
                            }
                        };
                        let selected = select_collection_entries(&coll.entries, &parsed, None);
                        emit_collection_output(
                            &rt,
                            &pipe_bg,
                            &effective_client,
                            &base_origin,
                            &target,
                            &parsed,
                            &ext_args,
                            cookie.as_deref(),
                            active_sort,
                            &extra_headers,
                            socket_timeout,
                            prefer_mobile_aac,
                            verify,
                            cache_dir.as_deref(),
                            &coll,
                            &selected,
                            false,
                        );
                    }
                }
            });

        pipe
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_collection_output(
    rt: &tokio::runtime::Runtime,
    pipe_bg: &VirtualPipeHandle,
    http_client: &reqwest::Client,
    base_origin: &str,
    original_url: &str,
    parsed: &YtdlpParsedArgs,
    ext_args: &YoutubeExtractorArgs,
    cookie: Option<&str>,
    active_sort: Option<&FormatSortSpec>,
    extra_headers: &[(String, String)],
    socket_timeout: Option<Duration>,
    prefer_mobile_aac: bool,
    verify: bool,
    cache_dir: Option<&Path>,
    coll: &YtJsCollectionResponse,
    selected: &[YtJsCollectionEntry],
    is_search: bool,
) {
    if parsed.output_mode == YtdlpOutputMode::DumpSingleJson {
        let payload = build_playlist_single_json(coll, selected, original_url, is_search);
        pipe_bg.push_stdout_line(&payload.to_string());
        pipe_bg.finish(0);
        return;
    }

    if parsed.flat_playlist || parsed.output_mode == YtdlpOutputMode::DumpJson {
        for entry in selected {
            if pipe_bg.is_killed() {
                return;
            }
            let entry_val = flat_collection_entry_json(entry);
            if parsed.output_mode != YtdlpOutputMode::DumpJson && !parsed.print_templates.is_empty()
            {
                for tpl in &parsed.print_templates {
                    pipe_bg.push_stdout_line(&render_print_template(tpl, &entry_val));
                }
            } else {
                pipe_bg.push_stdout_line(&entry_val.to_string());
            }
        }
        pipe_bg.finish(0);
        return;
    }

    let mut resolved_any = false;
    let mut last_err = String::new();
    for entry in selected {
        if pipe_bg.is_killed() {
            return;
        }
        let resolved = rt.block_on(resolve_innertube_stream_inner(
            http_client,
            base_origin,
            &entry.id,
            cookie,
            parsed.format_expr.as_deref(),
            ext_args,
            prefer_mobile_aac,
            verify,
            DEFAULT_CASCADE_BUDGET,
            cache_dir,
            CascadeExtraOptions {
                format_sort: active_sort,
                geo_bypass_country: parsed.geo_bypass_country.as_deref(),
                extra_headers,
                socket_timeout,
                cancel_flag: Some(pipe_bg.cancel_flag()),
            },
        ));
        if pipe_bg.is_killed() {
            return;
        }
        match resolved {
            Ok(stream) => {
                resolved_any = true;
                for line in format_ytdlp_output_lines(&stream, parsed) {
                    pipe_bg.push_stdout_line(&line);
                }
            }
            Err(e) => {
                last_err = format!("{}: {e}", entry.id);
                pipe_bg.push_stderr(&format!("ERROR: [youtube] {}: {e}", entry.id));
            }
        }
    }
    if resolved_any || selected.is_empty() {
        pipe_bg.finish(0);
    } else {
        if last_err.is_empty() {
            pipe_bg.push_stderr("ERROR: [youtube] failed to resolve any playlist/search items");
        }
        pipe_bg.finish(1);
    }
}

fn build_dump_json_value(
    stream: &ResolvedAudioStream,
    http_headers: serde_json::Map<String, Value>,
    original_url: Option<&str>,
) -> Value {
    let abr_val = stream
        .abr
        .map(|v| json!(v))
        .unwrap_or_else(|| json!((stream.bitrate as f64) / 1000.0));
    let tbr_val = stream
        .tbr
        .map(|v| json!(v))
        .unwrap_or_else(|| json!((stream.bitrate as f64) / 1000.0));
    let webpage_url = format!("https://www.youtube.com/watch?v={}", stream.video_id);
    let orig_url = original_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(webpage_url.as_str());
    let channel = if stream.channel.is_empty() {
        stream.uploader.as_str()
    } else {
        stream.channel.as_str()
    };
    let artist = if stream.uploader.is_empty() {
        channel
    } else {
        stream.uploader.as_str()
    };
    let live_status = if stream.live_status.is_empty() {
        if stream.is_live {
            "is_live"
        } else {
            "not_live"
        }
    } else {
        stream.live_status.as_str()
    };
    let thumbnails: Vec<Value> = if !stream.thumbnails_json.is_empty() {
        stream.thumbnails_json.clone()
    } else if !stream.thumbnail.is_empty() {
        vec![json!({ "url": stream.thumbnail })]
    } else {
        Vec::new()
    };
    json!({
        "id": stream.video_id,
        "title": stream.title,
        "track": stream.title,
        "artist": artist,
        "channel": channel,
        "channel_id": stream.channel_id,
        "uploader": stream.uploader,
        "duration": stream.duration_secs,
        "is_live": stream.is_live,
        "live_status": live_status,
        "webpage_url": webpage_url,
        "original_url": orig_url,
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "thumbnail": stream.thumbnail,
        "thumbnails": thumbnails,
        "url": stream.url,
        "ext": stream.ext,
        "acodec": stream.acodec,
        "vcodec": stream.vcodec,
        "format_id": stream.format_id,
        "abr": abr_val,
        "tbr": tbr_val,
        "asr": stream.asr,
        "audio_channels": stream.audio_channels,
        "filesize": stream.filesize,
        "protocol": if stream.protocol.is_empty() {
            if stream.is_hls { "m3u8_native" } else { "https" }
        } else {
            stream.protocol.as_str()
        },
        "http_headers": http_headers,
        "formats": stream.formats_json,
    })
}

fn json_field_to_print_string(val: Option<&Value>) -> String {
    match val {
        None | Some(Value::Null) => "NA".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

fn render_print_template(tpl: &str, info: &Value) -> String {
    let trimmed = tpl.trim();
    // 1. Exact `%(.{f1,f2,...})j` arbitrary JSON projection.
    if let Some(inner) = trimmed
        .strip_prefix("%(.{")
        .and_then(|s| s.strip_suffix("})j"))
    {
        let mut projected = serde_json::Map::new();
        for field in inner.split(',') {
            let key = field.trim();
            if key.is_empty() {
                continue;
            }
            if let Some(v) = info.get(key) {
                projected.insert(key.to_owned(), v.clone());
            }
        }
        return Value::Object(projected).to_string();
    }

    // 2. Bare field name without `%` (e.g. `url`, `id`, `title`, `ext`, `acodec`, `format_id`, `abr`, `duration`).
    if !trimmed.contains('%') {
        if let Some(val) = info.get(trimmed) {
            return json_field_to_print_string(Some(val));
        }
        return trimmed.to_owned();
    }

    // 3. Interpolate `%(field)s`, `%(field)j`, `%(field)d`, `%(.{f1,f2})j` tokens inside arbitrary templates.
    let mut out = String::with_capacity(trimmed.len() + 32);
    let mut rest = trimmed;
    while let Some(pos) = rest.find("%(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let Some(close) = after.find(')') else {
            out.push_str(&rest[pos..]);
            rest = "";
            break;
        };
        let key_expr = &after[..close];
        let spec_char = after[close + 1..].chars().next();
        let advance = close + 1 + spec_char.map_or(0, char::len_utf8);
        if let Some(proj) = key_expr
            .strip_prefix(".{")
            .and_then(|s| s.strip_suffix('}'))
        {
            let mut projected = serde_json::Map::new();
            for field in proj.split(',') {
                let k = field.trim();
                if !k.is_empty()
                    && let Some(v) = info.get(k)
                {
                    projected.insert(k.to_owned(), v.clone());
                }
            }
            out.push_str(&Value::Object(projected).to_string());
        } else {
            let val = info.get(key_expr.trim());
            if spec_char == Some('j') {
                out.push_str(&val.cloned().unwrap_or(Value::Null).to_string());
            } else {
                out.push_str(&json_field_to_print_string(val));
            }
        }
        rest = &after[advance..];
    }
    out.push_str(rest);
    out
}

fn format_ytdlp_output_lines(
    stream: &ResolvedAudioStream,
    parsed: &YtdlpParsedArgs,
) -> Vec<String> {
    let mut http_headers = serde_json::Map::new();
    // Always advertise the winning InnerTube client's User-Agent so googlevideo.com
    // accepts the stream URL even when the plugin passed a desktop browser UA on stdin.
    let ua = if stream.user_agent.is_empty() {
        find_header(&parsed.headers, "User-Agent").unwrap_or("Mozilla/5.0")
    } else {
        &stream.user_agent
    };
    http_headers.insert("User-Agent".to_owned(), json!(ua));
    for (k, v) in &parsed.headers {
        if !k.eq_ignore_ascii_case("user-agent") {
            http_headers.insert(k.clone(), json!(v));
        }
    }

    let info = build_dump_json_value(stream, http_headers, parsed.target.as_deref());

    if matches!(
        parsed.output_mode,
        YtdlpOutputMode::DumpJson | YtdlpOutputMode::DumpSingleJson
    ) {
        return vec![info.to_string()];
    }
    if !parsed.print_templates.is_empty() {
        return parsed
            .print_templates
            .iter()
            .map(|tpl| render_print_template(tpl, &info))
            .collect();
    }
    if let Some(ref tpl) = parsed.print_template {
        return vec![render_print_template(tpl, &info)];
    }

    match parsed.output_mode {
        YtdlpOutputMode::PrintUrlAndHeadersJson => {
            vec![render_print_template("%(.{url,http_headers})j", &info)]
        }
        YtdlpOutputMode::PrintUrl => vec![stream.url.clone()],
        YtdlpOutputMode::DumpJson | YtdlpOutputMode::DumpSingleJson => vec![info.to_string()],
    }
}

#[cfg(test)]
fn format_ytdlp_output(stream: &ResolvedAudioStream, parsed: &YtdlpParsedArgs) -> String {
    format_ytdlp_output_lines(stream, parsed).join("\n")
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

        let pipe = VirtualYtdlpShim::spawn_with_origin_cache_and_tracker(
            args,
            ctx.http_client().clone(),
            DEFAULT_INNERTUBE_ORIGIN.to_owned(),
            ctx.is_mobile(),
            Some(ctx.data_dir().to_path_buf()),
            Some(ctx.ytdlp_exhausted_tracker()),
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
    use crate::yt_youtubejs::SAFARI_WEB_UA;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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
            Some("2026.03.01 (nsh-youtubejs-18.3.0+ejs-0.8.0)")
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
                            if req_lower.contains("range: bytes=0-63")
                                && req.contains("com.google.android.youtube/")
                            {
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
        assert!(
            resolved.user_agent.contains("com.google.android.youtube/"),
            "expected YouTube.js ANDROID UA, got {}",
            resolved.user_agent
        );
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
                "iOS:anon",
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
                "iOS:anon",
            ]
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn authenticated_fallback_to_anonymous_mobile_and_respects_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fallback mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let _ = listener.set_nonblocking(true);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let saw_auth_on_web_music = Arc::new(AtomicBool::new(false));
        let saw_auth_web_srv = Arc::clone(&saw_auth_on_web_music);
        let saw_mobile_auth_header = Arc::new(AtomicBool::new(false));
        let saw_mob_auth_srv = Arc::clone(&saw_mobile_auth_header);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let req_lower = req.to_ascii_lowercase();
                        let client_name = extract_json_client_name(&req);
                        let has_auth_or_cookie = req_lower.contains("\r\nauthorization:")
                            || req_lower.contains("\r\ncookie:");

                        if client_name == "WEB_REMIX" && has_auth_or_cookie {
                            saw_auth_web_srv.store(true, Ordering::SeqCst);
                        }
                        if matches!(
                            client_name.as_str(),
                            "ANDROID" | "ANDROID_VR" | "iOS" | "VISIONOS"
                        ) && has_auth_or_cookie
                        {
                            saw_mob_auth_srv.store(true, Ordering::SeqCst);
                        }

                        let body = if req.contains("SLOW_BUDGET") {
                            std::thread::sleep(Duration::from_millis(250));
                            r#"{"playabilityStatus":{"status":"UNPLAYABLE","reason":"slow"}}"#
                                .to_owned()
                        } else if req.contains("AUTH_TO_ANON_OK") && client_name == "ANDROID_VR" {
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Anon Fallback Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": "https://cdn.example.test/vr_anon140.m4a"
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        } else {
                            r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in required"}}"#
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

        // 1. Authenticated `web_music` fails with LOGIN_REQUIRED -> falls back to anonymous
        //    `android_vr` with ZERO Cookie / Authorization headers leaked.
        let ext_auth =
            YoutubeExtractorArgs::parse(&["youtube:player_client=web_music,android_vr".to_owned()]);
        let resolved = rt
            .block_on(resolve_innertube_stream_inner(
                &client,
                &origin,
                "AUTH_TO_ANON_OK",
                Some("SAPISID=dual_origin_secret"),
                None,
                &ext_auth,
                true,
                false,
                Duration::from_secs(10),
                None,
                CascadeExtraOptions::default(),
            ))
            .expect("should fall back from auth web_music to anon android_vr");
        assert_eq!(resolved.url, "https://cdn.example.test/vr_anon140.m4a");
        assert!(
            saw_auth_on_web_music.load(Ordering::SeqCst),
            "cookie-supporting YTMUSIC client should carry auth/cookie headers"
        );
        assert!(
            !saw_mobile_auth_header.load(Ordering::SeqCst),
            "native mobile/VR clients must NOT receive Cookie or Authorization headers"
        );

        // 2. Cascade enforces total time budget cleanly instead of running every slow step.
        let default_ext = YoutubeExtractorArgs::default();
        let started = Instant::now();
        let err = rt
            .block_on(resolve_innertube_stream_inner(
                &client,
                &origin,
                "SLOW_BUDGET",
                Some("SAPISID=dual_origin_secret"),
                None,
                &default_ext,
                true,
                false,
                Duration::from_millis(320),
                None,
                CascadeExtraOptions::default(),
            ))
            .expect_err("slow cascade should fail within budget");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(900),
            "cascade exceeded total budget ({elapsed:?}), err={err}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
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

        // 2. End-to-end mock server returning CN visitorData + UNPLAYABLE (with simpleText & runs
        //    subreasons) -> YouTube.js surfaces subreason and annotates `YouTube geo=CN; switch proxy/VPN region`.
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
                        let body = if req.contains("RUNSBLOCK01") {
                            json!({
                                "responseContext": { "visitorData": cn_vd_srv },
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
                            })
                            .to_string()
                        } else {
                            json!({
                                "responseContext": { "visitorData": cn_vd_srv },
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

        let runs_err = rt
            .block_on(resolve_innertube_stream_with_options(
                &client,
                &origin,
                "RUNSBLOCK01",
                None,
                None,
                true,
                false,
            ))
            .expect_err("runs subreason must be extracted");
        assert_eq!(
            runs_err,
            "Not available in your region (YouTube geo=CN; switch proxy/VPN region)"
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
        assert!(
            rewritten.contains("n=newN%2B99"),
            "n rewritten: {rewritten}"
        );
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
        let rewritten_hls =
            apply_solved_sig_and_n(&picked_audio, None, "signature", &solved_sigs, &solved_ns)
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
        assert!(
            picked_muxed.contains("/itag/93/"),
            "expected itag=93, got {picked_muxed}"
        );

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
            vcodec: "none".to_owned(),
            format_id: "hls".to_owned(),
            bitrate: 0,
            abr: None,
            tbr: None,
            asr: None,
            audio_channels: Some(2),
            filesize: None,
            duration_secs: None,
            protocol: "m3u8_native".to_owned(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            is_live: false,
            live_status: "not_live".to_owned(),
            thumbnail: String::new(),
            thumbnails_json: Vec::new(),
            user_agent: SAFARI_WEB_UA.to_owned(),
            is_hls: true,
            formats_json: Vec::new(),
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

    #[test]
    fn hls_multi_variant_expansion_and_print_templates_and_dump_json_fields() {
        let meta = VideoMeta {
            video_id: "vidHls00001".to_owned(),
            title: "Test Track".to_owned(),
            duration_secs: Some(215),
            uploader: "Test Artist".to_owned(),
            channel: "Test Artist Channel".to_owned(),
            channel_id: "UCtest123".to_owned(),
            is_live: false,
            live_status: "not_live".to_owned(),
            thumbnail: "https://i.ytimg.com/vi/vidHls00001/maxresdefault.jpg".to_owned(),
            thumbnails_json: vec![json!({
                "url": "https://i.ytimg.com/vi/vidHls00001/maxresdefault.jpg",
                "width": 1280,
                "height": 720
            })],
        };
        let master = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=86008,CODECS=\"avc1.42c00c,mp4a.40.5\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/91/n/n91/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=122443,CODECS=\"avc1.4d4015,mp4a.40.5\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/92/n/n92/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=263884,CODECS=\"avc1.4d401e,mp4a.40.2\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/93/n/n93/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=322741,CODECS=\"avc1.4d401f,mp4a.40.2\"
https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/94/n/n94/index.m3u8
";
        let expanded = expand_hls_master_variants(
            "https://manifest.googlevideo.com/master.m3u8",
            master,
            &meta,
            SAFARI_WEB_UA,
        );
        assert_eq!(expanded.len(), 4);
        let metas: Vec<CandidateFormatMeta> = expanded.iter().map(|c| c.meta.clone()).collect();
        let sel = FormatSelector::parse("bestaudio[protocol^=m3u8]/93/94/92/95/91");
        let chosen = sel.select_indices(&metas, true);
        let chosen_ids: Vec<&str> = chosen
            .iter()
            .map(|&i| expanded[i].meta.format_id.as_str())
            .collect();
        assert_eq!(chosen_ids, vec!["93", "94", "92", "91"]);

        // Test arbitrary `--print` templates, arbitrary JSON projections, and complete `-j` dump fields.
        let mut sample_stream = expanded[chosen[0]].stream.clone();
        sample_stream.asr = Some(44100);
        sample_stream.filesize = Some(4_567_890);
        sample_stream.formats_json = expanded
            .iter()
            .map(candidate_to_format_summary_json)
            .collect();

        let p_dump = parse_ytdlp_args(&["-j".to_owned(), "vidHls00001".to_owned()]);
        let dump_val: Value =
            serde_json::from_str(&format_ytdlp_output(&sample_stream, &p_dump)).unwrap();
        for req_field in [
            "id",
            "title",
            "url",
            "ext",
            "acodec",
            "vcodec",
            "format_id",
            "abr",
            "tbr",
            "asr",
            "audio_channels",
            "filesize",
            "duration",
            "protocol",
            "uploader",
            "channel_id",
            "thumbnail",
            "http_headers",
            "formats",
        ] {
            assert!(
                dump_val.get(req_field).is_some(),
                "dump-json missing `{req_field}`: {dump_val}"
            );
        }
        assert_eq!(dump_val["format_id"], "93");
        assert_eq!(dump_val["protocol"], "m3u8_native");
        assert_eq!(dump_val["formats"].as_array().unwrap().len(), 4);

        // Arbitrary JSON projection `%(.{id,format_id,ext,acodec,uploader})j`.
        let p_proj = parse_ytdlp_args(&[
            "--print".to_owned(),
            "%(.{id,format_id,ext,acodec,uploader})j".to_owned(),
            "vidHls00001".to_owned(),
        ]);
        let proj_val: Value =
            serde_json::from_str(&format_ytdlp_output(&sample_stream, &p_proj)).unwrap();
        assert_eq!(proj_val["id"], "vidHls00001");
        assert_eq!(proj_val["format_id"], "93");
        assert_eq!(proj_val["ext"], "m3u8");
        assert_eq!(proj_val["acodec"], "mp4a.40.2");
        assert_eq!(proj_val["uploader"], "Test Artist");
        assert!(proj_val.get("url").is_none());

        // Interpolated string template and bare field templates.
        let p_interp = parse_ytdlp_args(&[
            "--print".to_owned(),
            "%(id)s - %(title)s [%(format_id)s.%(ext)s]".to_owned(),
            "vidHls00001".to_owned(),
        ]);
        assert_eq!(
            format_ytdlp_output(&sample_stream, &p_interp),
            "vidHls00001 - Test Track [93.m3u8]"
        );

        let p_bare_title = parse_ytdlp_args(&[
            "--print".to_owned(),
            "title".to_owned(),
            "vidHls00001".to_owned(),
        ]);
        assert_eq!(
            format_ytdlp_output(&sample_stream, &p_bare_title),
            "Test Track"
        );
    }

    #[test]
    fn extractor_args_end_to_end_player_client_po_token_skip_hls_and_lang() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ext args mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let saw_bootstrap_logged_in = Arc::new(AtomicBool::new(false));
        let saw_boot_srv = Arc::clone(&saw_bootstrap_logged_in);
        let saw_multi_sapisid = Arc::new(AtomicBool::new(false));
        let saw_auth_srv = Arc::clone(&saw_multi_sapisid);
        let saw_player_pot_and_lang = Arc::new(AtomicBool::new(false));
        let saw_pot_srv = Arc::clone(&saw_player_pot_and_lang);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let req_lower = req.to_ascii_lowercase();
                        if req_lower.contains("x-youtube-bootstrap-logged-in: true") {
                            saw_boot_srv.store(true, Ordering::SeqCst);
                        }
                        if req_lower.contains("sapisidhash ")
                            && req_lower.contains("sapisid1phash ")
                            && req_lower.contains("sapisid3phash ")
                        {
                            saw_auth_srv.store(true, Ordering::SeqCst);
                        }
                        if req.contains("\"poToken\":\"PL_TOK_99\"")
                            && req.contains("\"hl\":\"ja\"")
                            && req.contains("\"visitorData\":\"EXPLICIT_VIS_7\"")
                            && req.contains("\"clientName\":\"WEB\"")
                        {
                            saw_pot_srv.store(true, Ordering::SeqCst);
                        }

                        let body = json!({
                            "playabilityStatus": { "status": "OK" },
                            "videoDetails": { "title": "Ext Args Track", "lengthSeconds": "150" },
                            "streamingData": {
                                "adaptiveFormats": [
                                    {
                                        "itag": 140,
                                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                        "bitrate": 128000,
                                        "url": "https://cdn.example.test/videoplayback?itag=140"
                                    }
                                ],
                                "hlsManifestUrl": "https://cdn.example.test/ignored.m3u8"
                            }
                        })
                        .to_string();
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

        let args: Vec<String> = [
            "--config-locations",
            "-",
            "--extractor-args",
            "youtube:player_client=web_safari;lang=ja;visitor_data=EXPLICIT_VIS_7;po_token=web_safari.player+PL_TOK_99,web_safari.gvs+GVS_TOK_77;skip=hls,dash",
            "--format",
            "bestaudio[protocol^=http]/bestaudio",
            "--print",
            "%(.{url,http_headers})j",
            "https://www.youtube.com/watch?v=EXTARG00001",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        let pipe = VirtualYtdlpShim::spawn_with_origin(&args, reqwest::Client::new(), origin, true);
        pipe.write_stdin(
            "--add-headers \"Cookie:SAPISID=s0; __Secure-1PAPISID=s1; __Secure-3PAPISID=s3\"\n",
        )
        .unwrap();
        pipe.close_stdin();

        let line = pipe.read_line_blocking().expect("json output");
        assert_eq!(pipe.wait(), 0);
        let parsed: Value = serde_json::from_str(&line).expect("valid JSON");
        let resolved_url = parsed["url"].as_str().expect("url string");
        assert!(
            resolved_url.contains("pot=GVS_TOK_77"),
            "GVS po_token should be appended to URL: {resolved_url}"
        );
        assert_eq!(parsed["http_headers"]["User-Agent"], SAFARI_WEB_UA);
        assert!(saw_bootstrap_logged_in.load(Ordering::SeqCst));
        assert!(saw_multi_sapisid.load(Ordering::SeqCst));
        assert!(saw_player_pot_and_lang.load(Ordering::SeqCst));

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn fallback_client_relaxes_m3u8_format_selector_to_bestaudio_and_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fallback relax mock");
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
                        let first_line = req.lines().next().unwrap_or_default();

                        if first_line.starts_with("GET /stream/direct140.m4a") {
                            let payload = vec![0x42u8; 64];
                            let header = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-63/1024\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                payload.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&payload);
                            continue;
                        }

                        let body = if req.contains(SAFARI_WEB_UA) {
                            // Explicit `web_safari` returns only direct HTTPS itag 140 (no HLS/93).
                            // Because it is explicit, it must NOT relax `bestaudio[protocol^=m3u8]/93/...`.
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Relax Track", "lengthSeconds": "180" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/direct140.m4a?src=explicit")
                                        }
                                    ]
                                }
                            })
                            .to_string()
                        } else {
                            // Fallback client (`web_music` / `YTMUSIC`) returns direct HTTPS itag 140.
                            // Because `!step.is_explicit`, the m3u8 selector relaxes to `bestaudio/best`.
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Relax Track", "lengthSeconds": "180" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/direct140.m4a?src=fallback")
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

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();
        let ext_args =
            YoutubeExtractorArgs::parse(&["youtube:player_client=web_safari".to_owned()]);
        let m3u8_selector = Some("bestaudio[protocol^=m3u8]/93/94/92/95/91");

        // 1. With fallback steps appended (`append_fallbacks = true`), `web_safari` (explicit)
        //    misses the m3u8 selector and falls through to `web_music` (fallback), which relaxes
        //    to `bestaudio/best` and succeeds with direct HTTPS itag=140.
        let steps_with_fallback =
            build_cascade_steps_inner(&origin, true, &ext_args, true, None, Instant::now());
        assert_eq!(steps_with_fallback[0].spec.key, "web_safari");
        assert!(steps_with_fallback[0].is_explicit);
        assert_eq!(steps_with_fallback[1].spec.key, "web_music");
        assert!(!steps_with_fallback[1].is_explicit);

        let ok = rt
            .block_on(resolve_innertube_stream_with_steps(
                &client,
                steps_with_fallback,
                "RELAX000001",
                Some("SAPISID=mock"),
                m3u8_selector,
                &ext_args,
                true,
                true,
                Duration::from_secs(10),
                None,
                false,
                CascadeExtraOptions::default(),
            ))
            .expect("fallback client should relax m3u8 selector to bestaudio/best");
        assert_eq!(ok.format_id, "140");
        assert!(!ok.is_hls);
        assert!(
            ok.url.contains("src=fallback"),
            "expected fallback client URL, got {}",
            ok.url
        );

        // 2. Without fallback steps (`append_fallbacks = false`), the explicit `web_safari` step
        //    MUST honor the strict m3u8 selector and fail rather than relaxing.
        let explicit_only_steps =
            build_cascade_steps_inner(&origin, true, &ext_args, false, None, Instant::now());
        assert_eq!(explicit_only_steps.len(), 1);
        let err = rt
            .block_on(resolve_innertube_stream_with_steps(
                &client,
                explicit_only_steps,
                "RELAX000001",
                Some("SAPISID=mock"),
                m3u8_selector,
                &ext_args,
                true,
                true,
                Duration::from_secs(10),
                None,
                false,
                CascadeExtraOptions::default(),
            ))
            .expect_err("explicit client must not relax strict selector");
        assert!(
            err.contains("requested format not available"),
            "unexpected error: {err}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn preflight_http_403_fast_fails_remaining_direct_candidates_on_same_client() {
        use std::sync::atomic::AtomicUsize;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 403 fast-fail mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let origin_srv = origin.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let c1_direct_403_hits = Arc::new(AtomicUsize::new(0));
        let c1_direct_srv = Arc::clone(&c1_direct_403_hits);
        let c1_hls_hits = Arc::new(AtomicUsize::new(0));
        let c1_hls_srv = Arc::clone(&c1_hls_hits);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let first_line = req.lines().next().unwrap_or_default();

                        if first_line.starts_with("GET /stream/c1_403_") {
                            c1_direct_srv.fetch_add(1, Ordering::SeqCst);
                            let body = "Forbidden";
                            let resp = format!(
                                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /stream/c1_hls.m3u8") {
                            c1_hls_srv.fetch_add(1, Ordering::SeqCst);
                            let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:5.0,\nseg0.aac\n";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /stream/c2_ok.m4a") {
                            let payload = vec![0x43u8; 64];
                            let header = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-63/1024\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                payload.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&payload);
                            continue;
                        }

                        let client_name = extract_json_client_name(&req);
                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"MOCK_VISITOR_403"}}"#.to_owned()
                        } else if req.contains("FASTFAIL_DIRECT") {
                            if client_name == "TVHTML5" {
                                json!({
                                    "playabilityStatus": { "status": "OK" },
                                    "videoDetails": { "title": "FastFail Direct", "lengthSeconds": "120" },
                                    "streamingData": {
                                        "adaptiveFormats": [
                                            { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "url": format!("{origin_srv}/stream/c1_403_140") },
                                            { "itag": 139, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 48000, "url": format!("{origin_srv}/stream/c1_403_139") },
                                            { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 160000, "url": format!("{origin_srv}/stream/c1_403_251") }
                                        ]
                                    }
                                })
                                .to_string()
                            } else {
                                json!({
                                    "playabilityStatus": { "status": "OK" },
                                    "videoDetails": { "title": "FastFail Direct", "lengthSeconds": "120" },
                                    "streamingData": {
                                        "adaptiveFormats": [
                                            { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "url": format!("{origin_srv}/stream/c2_ok.m4a") }
                                        ]
                                    }
                                })
                                .to_string()
                            }
                        } else {
                            // Direct candidates 403, plus 1 working HLS candidate on the same client.
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "FastFail To HLS", "lengthSeconds": "120" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "url": format!("{origin_srv}/stream/c1_403_140") },
                                        { "itag": 139, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 48000, "url": format!("{origin_srv}/stream/c1_403_139") }
                                    ],
                                    "hlsManifestUrl": format!("{origin_srv}/stream/c1_hls.m3u8")
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
            .unwrap();
        let client = reqwest::Client::new();
        let ext_args = YoutubeExtractorArgs::default();
        let steps = vec![
            CascadeStep {
                spec: lookup_ytjs_client("tv").unwrap(),
                loopback_origin: Some(origin.clone()),
                send_auth: false,
                is_explicit: false,
            },
            CascadeStep {
                spec: lookup_ytjs_client("android_vr").unwrap(),
                loopback_origin: Some(origin.clone()),
                send_auth: false,
                is_explicit: false,
            },
        ];

        // Case A: First direct HTTP 403 skips remaining direct candidates on `tv_samsung`
        // and immediately falls through to `android_vr`.
        let ok_a = rt
            .block_on(resolve_innertube_stream_with_steps(
                &client,
                steps.clone(),
                "FASTFAIL_DIRECT",
                None,
                None,
                &ext_args,
                true,
                true,
                Duration::from_secs(10),
                None,
                false,
                CascadeExtraOptions::default(),
            ))
            .expect("should fall back to android_vr");
        assert!(ok_a.url.contains("/stream/c2_ok.m4a"));
        assert_eq!(
            c1_direct_403_hits.load(Ordering::SeqCst),
            1,
            "HTTP 403 on first direct candidate must skip remaining direct candidates on same client"
        );

        // Case B: First direct HTTP 403 skips the 2nd direct candidate, but still tries 1 HLS candidate.
        c1_direct_403_hits.store(0, Ordering::SeqCst);
        c1_hls_hits.store(0, Ordering::SeqCst);
        let ok_b = rt
            .block_on(resolve_innertube_stream_with_steps(
                &client,
                steps,
                "FASTFAIL_HLS",
                None,
                None,
                &ext_args,
                true,
                true,
                Duration::from_secs(10),
                None,
                false,
                CascadeExtraOptions::default(),
            ))
            .expect("should still try HLS candidate after direct 403");
        assert!(ok_b.is_hls);
        assert!(ok_b.url.contains("/stream/c1_hls.m3u8"));
        assert_eq!(
            c1_direct_403_hits.load(Ordering::SeqCst),
            1,
            "2nd direct candidate must be skipped after 1st direct 403"
        );
        assert!(
            c1_hls_hits.load(Ordering::SeqCst) >= 1,
            "HLS candidate on same client must still be probed"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn preflight_caps_probes_at_three_candidates_on_ten_candidate_failing_client() {
        use std::sync::atomic::AtomicUsize;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind cap preflight mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let origin_srv = origin.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let direct_probe_hits = Arc::new(AtomicUsize::new(0));
        let direct_probe_srv = Arc::clone(&direct_probe_hits);
        let hls_probe_hits = Arc::new(AtomicUsize::new(0));
        let hls_probe_srv = Arc::clone(&hls_probe_hits);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let first_line = req.lines().next().unwrap_or_default();

                        if first_line.starts_with("GET /master.m3u8") {
                            // Master playlist exposing 3 HLS audio variants.
                            let body = format!(
                                "#EXTM3U\n\
                                #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,URI=\"{origin_srv}/cand/hls/234.m3u8\"\n\
                                #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",DEFAULT=NO,URI=\"{origin_srv}/cand/hls/233.m3u8\"\n\
                                #EXT-X-STREAM-INF:BANDWIDTH=400000,CODECS=\"avc1.42001e,mp4a.40.2\",AUDIO=\"a\"\n\
                                {origin_srv}/cand/hls/93.m3u8\n"
                            );
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /cand/direct/") {
                            direct_probe_srv.fetch_add(1, Ordering::SeqCst);
                            // Return 200 text/html (non-403, non-timeout) so ONLY the candidate cap stops probing.
                            let body = "<html>not audio</html>";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /cand/hls/") {
                            hls_probe_srv.fetch_add(1, Ordering::SeqCst);
                            let body = "<html>bad hls</html>";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            continue;
                        }
                        if first_line.starts_with("GET /stream/c2_ok.m4a") {
                            let payload = vec![0x44u8; 64];
                            let header = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-63/1024\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                payload.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&payload);
                            continue;
                        }

                        let client_name = extract_json_client_name(&req);
                        let body = if req.contains("/youtubei/v1/visitor_id") {
                            r#"{"responseContext":{"visitorData":"MOCK_VISITOR_CAP"}}"#.to_owned()
                        } else if client_name == "TVHTML5" {
                            // 8 direct formats + 3 expanded HLS variants = 11 total candidates on Client 1.
                            let mut formats = Vec::new();
                            for i in 0..8 {
                                formats.push(json!({
                                    "itag": 140 + i,
                                    "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                    "bitrate": 128000 - (i * 1000),
                                    "url": format!("{origin_srv}/cand/direct/{i}.m4a")
                                }));
                            }
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Cap Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": formats,
                                    "hlsManifestUrl": format!("{origin_srv}/master.m3u8")
                                }
                            })
                            .to_string()
                        } else {
                            json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "Cap Track", "lengthSeconds": "200" },
                                "streamingData": {
                                    "adaptiveFormats": [
                                        {
                                            "itag": 140,
                                            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                            "bitrate": 128000,
                                            "url": format!("{origin_srv}/stream/c2_ok.m4a")
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

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();
        let ext_args = YoutubeExtractorArgs::default();
        let steps = vec![
            CascadeStep {
                spec: lookup_ytjs_client("tv").unwrap(),
                loopback_origin: Some(origin.clone()),
                send_auth: false,
                is_explicit: false,
            },
            CascadeStep {
                spec: lookup_ytjs_client("android_vr").unwrap(),
                loopback_origin: Some(origin.clone()),
                send_auth: false,
                is_explicit: false,
            },
        ];

        let ok = rt
            .block_on(resolve_innertube_stream_with_steps(
                &client,
                steps,
                "CAPTEST0001",
                None,
                None,
                &ext_args,
                true,
                true,
                Duration::from_secs(10),
                None,
                false,
                CascadeExtraOptions::default(),
            ))
            .expect("should fall back to android_vr after capped probes");
        assert!(ok.url.contains("/stream/c2_ok.m4a"));

        let d_hits = direct_probe_hits.load(Ordering::SeqCst);
        let h_hits = hls_probe_hits.load(Ordering::SeqCst);
        assert_eq!(d_hits, 2, "expected at most 2 direct probes, got {d_hits}");
        assert_eq!(h_hits, 1, "expected at most 1 HLS probe, got {h_hits}");
        assert!(
            d_hits + h_hits <= 3,
            "total preflight probes per client must be <= 3, got {}",
            d_hits + h_hits
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn sticky_client_health_promotes_winner_and_demotes_failing_clients() {
        let mut cache = ClientHealthCache::default();
        let t0 = Instant::now();
        let empty_ext = YoutubeExtractorArgs::default();
        let explicit_safari =
            YoutubeExtractorArgs::parse(&["youtube:player_client=web_safari".to_owned()]);

        // 1. Baseline logged-in order starts with `web_music`, `web_safari`, `tv`, `android_vr`.
        let base_keys: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            true,
            &empty_ext,
            true,
            Some(&cache),
            t0,
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_eq!(
            &base_keys[..4],
            &["web_music", "web_safari", "tv", "android_vr"]
        );

        // 2. Recording a winner promotes it to index 0; >=2 failures demotes that client behind healthy ones.
        cache.record_success_at(true, "android_vr", t0);
        cache.record_failure_at(true, "web_safari", t0);
        cache.record_failure_at(true, "web_safari", t0);

        let reordered_keys: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            true,
            &empty_ext,
            true,
            Some(&cache),
            t0 + Duration::from_secs(1),
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_eq!(reordered_keys[0], "android_vr");
        let safari_pos = reordered_keys
            .iter()
            .position(|k| *k == "web_safari")
            .unwrap();
        let android_pos = reordered_keys.iter().position(|k| *k == "android").unwrap();
        assert!(
            safari_pos > android_pos,
            "failing web_safari should be demoted behind healthy clients: {reordered_keys:?}"
        );

        // 3. Explicit `player_client=web_safari`: because `web_safari` has fail_streak >= 2 and a
        //    different healthy winner (`android_vr`) exists, promote `android_vr` ahead of `web_safari`.
        let explicit_demoted: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            true,
            &explicit_safari,
            true,
            Some(&cache),
            t0 + Duration::from_secs(1),
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_eq!(explicit_demoted[0], "android_vr");
        assert_eq!(explicit_demoted[1], "web_safari");

        // 4. Once `web_safari` is healthy again (failure TTL elapsed after 5 min), explicit
        //    `web_safari` stays first while healthy `android_vr` (15-min winner TTL) sits second.
        let after_fail_ttl = t0 + CLIENT_FAILURE_TTL + Duration::from_secs(1);
        let explicit_recovered: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            true,
            &explicit_safari,
            true,
            Some(&cache),
            after_fail_ttl,
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_eq!(explicit_recovered[0], "web_safari");
        assert_eq!(explicit_recovered[1], "android_vr");

        // 5. Cookie-mode isolation: `has_cookie = false` bucket is unaffected by logged-in history.
        let anon_keys: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            false,
            &empty_ext,
            true,
            Some(&cache),
            t0 + Duration::from_secs(1),
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_ne!(anon_keys[0], "android_vr");

        // 6. Winner TTL expiry (15 min) restores default ordering.
        let after_winner_ttl = t0 + CLIENT_WINNER_TTL + Duration::from_secs(1);
        let expired_keys: Vec<&str> = build_cascade_steps_inner(
            DEFAULT_INNERTUBE_ORIGIN,
            true,
            &empty_ext,
            true,
            Some(&cache),
            after_winner_ttl,
        )
        .into_iter()
        .map(|s| s.spec.key)
        .collect();
        assert_eq!(expired_keys[0], "web_music");
    }

    #[test]
    fn parses_leading_dash_video_ids_end_of_options_url_forms_and_two_arg_flags() {
        // 1. Bare 11-char YouTube ID starting with `-` (`-dQw4w9WgXc`) without `--`.
        let p_dash = parse_ytdlp_args(&["-g".to_owned(), "-dQw4w9WgXc".to_owned()]);
        assert_eq!(p_dash.target.as_deref(), Some("-dQw4w9WgXc"));
        assert_eq!(
            extract_youtube_video_id("-dQw4w9WgXc").as_deref(),
            Some("-dQw4w9WgXc")
        );

        // 2. `--` end-of-options passes leading-dash and double-dash IDs cleanly as positional target.
        let p_eoo = parse_ytdlp_args(&[
            "--no-warnings".to_owned(),
            "--".to_owned(),
            "-dQw4w9WgXc".to_owned(),
        ]);
        assert_eq!(p_eoo.target.as_deref(), Some("-dQw4w9WgXc"));
        let p_eoo2 = parse_ytdlp_args(&["--".to_owned(), "--dQw4w9WgX".to_owned()]);
        assert_eq!(p_eoo2.target.as_deref(), Some("--dQw4w9WgX"));

        // 3. Complete YouTube URL forms: youtube-nocookie, /e/<id>, /attribution_link?...u=...
        assert_eq!(
            extract_youtube_video_id("https://www.youtube-nocookie.com/embed/abc123XYZ09")
                .as_deref(),
            Some("abc123XYZ09")
        );
        assert_eq!(
            extract_youtube_video_id("https://www.youtube.com/e/abc123XYZ09?rel=0").as_deref(),
            Some("abc123XYZ09")
        );
        assert_eq!(
            extract_youtube_video_id(
                "https://www.youtube.com/attribution_link?a=xyz&u=%2Fwatch%3Fv%3D-dQw4w9WgXc%26feature%3Dshare"
            )
            .as_deref(),
            Some("-dQw4w9WgXc")
        );

        // 4. Known 2-arg flags must consume their value so `parsed.target` is NEVER overwritten.
        let cli_2arg: Vec<String> = [
            "--cookies-from-browser",
            "chrome",
            "-N",
            "4",
            "--retries",
            "3",
            "--fragment-retries",
            "5",
            "--extractor-retries",
            "2",
            "--buffer-size",
            "16K",
            "--http-chunk-size",
            "10M",
            "--throttled-rate",
            "100K",
            "-r",
            "2M",
            "--sleep-interval",
            "1",
            "--max-sleep-interval",
            "5",
            "--max-downloads",
            "10",
            "--impersonate",
            "chrome",
            "--download-sections",
            "*0:00-0:30",
            "-P",
            "/tmp/out",
            "--temp-dir",
            "/tmp",
            "--cache-dir",
            "/tmp/cache",
            "--proxy",
            "http://127.0.0.1:7890",
            "--no-check-certificates",
            "--socket-timeout",
            "7",
            "--geo-bypass-country",
            "jp",
            "--flat-playlist",
            "--yes-playlist",
            "-I",
            "1,3:5",
            "https://www.youtube.com/watch?v=-dQw4w9WgXc",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let p_2arg = parse_ytdlp_args(&cli_2arg);
        assert_eq!(
            p_2arg.target.as_deref(),
            Some("https://www.youtube.com/watch?v=-dQw4w9WgXc")
        );
        assert_eq!(p_2arg.proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert!(p_2arg.no_check_certificates);
        assert_eq!(p_2arg.socket_timeout_secs, Some(7));
        assert_eq!(p_2arg.geo_bypass_country.as_deref(), Some("JP"));
        assert!(p_2arg.flat_playlist);
        assert!(p_2arg.yes_playlist);
        assert_eq!(p_2arg.playlist_items.as_deref(), Some("1,3:5"));

        // Target placed BEFORE 2-arg flags must also survive intact!
        let cli_before: Vec<String> = [
            "dQw4w9WgXcQ",
            "--cookies-from-browser",
            "firefox",
            "-N",
            "8",
            "--retries",
            "5",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let p_before = parse_ytdlp_args(&cli_before);
        assert_eq!(p_before.target.as_deref(), Some("dQw4w9WgXcQ"));

        // Verify playlist items index slicing (`1,3:5`, `-1`).
        assert_eq!(parse_playlist_items_indices("1,3:5", 6), vec![0, 2, 3, 4]);
        assert_eq!(parse_playlist_items_indices("-1", 4), vec![3]);
        assert_eq!(parse_playlist_items_indices("2-3", 4), vec![1, 2]);
    }

    #[test]
    fn multiple_print_templates_format_sort_rich_dump_single_json_and_custom_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind rich json mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let saw_geo_and_headers = Arc::new(AtomicBool::new(false));
        let saw_geo_srv = Arc::clone(&saw_geo_and_headers);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let req_lower = req.to_ascii_lowercase();
                        if req.contains("\"gl\":\"JP\"")
                            && req_lower.contains("x-goog-pageid: 112233445566")
                            && req_lower.contains("accept-language: ja-jp,ja;q=0.9")
                        {
                            saw_geo_srv.store(true, Ordering::SeqCst);
                        }
                        let body = json!({
                            "playabilityStatus": { "status": "OK" },
                            "videoDetails": {
                                "videoId": "-dQw4w9WgXc",
                                "title": "Rich Meta Track",
                                "lengthSeconds": "213",
                                "author": "Artist Official",
                                "channelId": "UCrich12345",
                                "isLive": false,
                                "thumbnail": {
                                    "thumbnails": [
                                        { "url": "https://i.ytimg.com/vi/-dQw4w9WgXc/hqdefault.jpg", "width": 480, "height": 360 },
                                        { "url": "https://i.ytimg.com/vi/-dQw4w9WgXc/maxresdefault.jpg", "width": 1280, "height": 720 }
                                    ]
                                }
                            },
                            "streamingData": {
                                "adaptiveFormats": [
                                    {
                                        "itag": 140,
                                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                        "bitrate": 128000,
                                        "audioSampleRate": "44100",
                                        "audioChannels": 2,
                                        "contentLength": "3400000",
                                        "url": "https://cdn.example.test/rich140.m4a"
                                    },
                                    {
                                        "itag": 251,
                                        "mimeType": "audio/webm; codecs=\"opus\"",
                                        "bitrate": 160000,
                                        "audioSampleRate": "48000",
                                        "audioChannels": 2,
                                        "contentLength": "4200000",
                                        "url": "https://cdn.example.test/rich251.webm"
                                    }
                                ]
                            }
                        })
                        .to_string();
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

        let client = reqwest::Client::new();

        // 1. Multiple `-O` / `--print` flags with stage prefixes + `-S acodec:opus` + bare `-dQw4w9WgXc`.
        let args_multi_o: Vec<String> = [
            "-S",
            "acodec:opus,abr",
            "-O",
            "video:%(id)s",
            "--print",
            "after_move:title",
            "-O",
            "%(format_id)s:%(ext)s",
            "-dQw4w9WgXc",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let pipe_o = VirtualYtdlpShim::spawn_with_origin(
            &args_multi_o,
            client.clone(),
            origin.clone(),
            true,
        );
        let l1 = pipe_o.read_line_blocking().expect("line 1");
        let l2 = pipe_o.read_line_blocking().expect("line 2");
        let l3 = pipe_o.read_line_blocking().expect("line 3");
        assert_eq!(pipe_o.read_line_blocking(), None);
        assert_eq!(pipe_o.wait(), 0);
        assert_eq!(l1, "-dQw4w9WgXc");
        assert_eq!(l2, "Rich Meta Track");
        assert_eq!(l3, "251:webm");

        // 2. `-J` (`--dump-single-json`) with `--geo-bypass-country JP` + `X-Goog-PageId` + `Accept-Language`.
        let args_j: Vec<String> = [
            "-J",
            "--geo-bypass-country",
            "JP",
            "--extractor-args",
            "youtube:player_client=web_music",
            "--add-header",
            "Cookie:SAPISID=brand_account_secret",
            "--add-header",
            "X-Goog-PageId:112233445566",
            "--add-header",
            "Accept-Language:ja-JP,ja;q=0.9",
            "--",
            "-dQw4w9WgXc",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let pipe_j = VirtualYtdlpShim::spawn_with_origin(&args_j, client, origin, true);
        let json_line = pipe_j.read_line_blocking().expect("single json line");
        assert_eq!(pipe_j.wait(), 0);
        assert!(
            saw_geo_and_headers.load(Ordering::SeqCst),
            "expected gl=JP, X-Goog-PageId, and Accept-Language on InnerTube request"
        );
        let val: Value = serde_json::from_str(&json_line).expect("valid -J JSON");
        for field in [
            "id",
            "title",
            "track",
            "artist",
            "channel",
            "channel_id",
            "uploader",
            "duration",
            "is_live",
            "live_status",
            "webpage_url",
            "original_url",
            "extractor",
            "extractor_key",
            "thumbnail",
            "thumbnails",
            "url",
            "ext",
            "acodec",
            "vcodec",
            "format_id",
            "abr",
            "tbr",
            "asr",
            "audio_channels",
            "filesize",
            "protocol",
            "http_headers",
            "formats",
        ] {
            assert!(val.get(field).is_some(), "-J missing `{field}`: {val}");
        }
        assert_eq!(val["id"], "-dQw4w9WgXc");
        assert_eq!(val["extractor"], "youtube");
        assert_eq!(val["extractor_key"], "Youtube");
        assert_eq!(val["live_status"], "not_live");
        assert_eq!(val["is_live"], false);
        assert_eq!(
            val["webpage_url"],
            "https://www.youtube.com/watch?v=-dQw4w9WgXc"
        );
        assert_eq!(val["thumbnails"].as_array().unwrap().len(), 2);

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn dash_mpd_baseurl_fallback_extracts_audio_when_formats_empty() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dash mock");
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
                        let (ct, body) = if first.starts_with("GET /manifest.mpd") {
                            let xml = format!(
                                r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD>
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="137" codecs="avc1.640028" bandwidth="2500000">
        <BaseURL>{origin_srv}/video.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" audioSamplingRate="44100">
      <Representation id="140" codecs="mp4a.40.2" bandwidth="129500" audioSamplingRate="44100">
        <BaseURL>{origin_srv}/dash_audio.m4a?itag=140&amp;mime=audio%2Fmp4</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#
                            );
                            ("application/dash+xml", xml)
                        } else {
                            let j = json!({
                                "playabilityStatus": { "status": "OK" },
                                "videoDetails": { "title": "DASH Fallback Track", "lengthSeconds": "190" },
                                "streamingData": {
                                    "adaptiveFormats": [],
                                    "formats": [],
                                    "dashManifestUrl": format!("{origin_srv}/manifest.mpd")
                                }
                            })
                            .to_string();
                            ("application/json", j)
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
            .unwrap();
        let client = reqwest::Client::new();
        let resolved = rt
            .block_on(resolve_innertube_stream_with_options(
                &client,
                &origin,
                "DASHONLY001",
                None,
                None,
                true,
                false,
            ))
            .expect("DASH MPD fallback should extract audio BaseURL");
        assert_eq!(resolved.format_id, "140");
        assert_eq!(resolved.ext, "m4a");
        assert_eq!(resolved.acodec, "mp4a.40.2");
        assert_eq!(resolved.asr, Some(44100));
        assert_eq!(
            resolved.url,
            format!("{origin}/dash_audio.m4a?itag=140&mime=audio%2Fmp4")
        );

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn rapid_kill_cancels_inflight_virtual_ytdlp_cascade_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        if req.contains("STALLTRACK1") {
                            // Hold socket open in small slices so test shutdown is quick too.
                            for _ in 0..50 {
                                if stop_srv.load(Ordering::Relaxed) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                        let body = json!({
                            "playabilityStatus": { "status": "OK" },
                            "videoDetails": { "title": "Fast Track", "lengthSeconds": "100" },
                            "streamingData": {
                                "adaptiveFormats": [
                                    {
                                        "itag": 140,
                                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                                        "bitrate": 128000,
                                        "url": "https://cdn.example.test/after_kill.m4a"
                                    }
                                ]
                            }
                        })
                        .to_string();
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

        let client = reqwest::Client::new();
        let started = Instant::now();
        let pipe = VirtualYtdlpShim::spawn_with_origin(
            &["-g".to_owned(), "STALLTRACK1".to_owned()],
            client.clone(),
            origin.clone(),
            true,
        );
        std::thread::sleep(Duration::from_millis(100));
        pipe.kill();
        let code = pipe.wait();
        let elapsed = started.elapsed();
        assert_eq!(code, -1);
        assert!(
            elapsed < Duration::from_secs(1),
            "pipe.kill() must abort in-flight VirtualYtdlpShim promptly, took {elapsed:?}"
        );

        // Immediately following track resolution must still work cleanly.
        let pipe2 = VirtualYtdlpShim::spawn_with_origin(
            &["-g".to_owned(), "FASTTRACK01".to_owned()],
            client,
            origin,
            true,
        );
        let url = pipe2.read_line_blocking().expect("fast track url");
        assert_eq!(pipe2.wait(), 0);
        assert_eq!(url, "https://cdn.example.test/after_kill.m4a");

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    fn youtubejs_search_and_playlist_extraction_supports_ndjson_and_dump_single_json() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind collection mock");
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let _ = listener.set_nonblocking(true);

        let srv = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let req = read_http_request(&mut stream);
                        let body = if req.contains("/youtubei/v1/search") {
                            json!({
                                "contents": {
                                    "twoColumnSearchResultsRenderer": {
                                        "primaryContents": {
                                            "sectionListRenderer": {
                                                "contents": [{
                                                    "itemSectionRenderer": {
                                                        "contents": [
                                                            {
                                                                "videoRenderer": {
                                                                    "videoId": "SRCHVID0001",
                                                                    "title": { "runs": [{ "text": "Search Song One" }] },
                                                                    "ownerText": { "runs": [{ "text": "Artist One", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCone" } } }] },
                                                                    "lengthText": { "simpleText": "3:20" },
                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/SRCHVID0001/hqdefault.jpg" }] }
                                                                }
                                                            },
                                                            {
                                                                "videoRenderer": {
                                                                    "videoId": "SRCHVID0002",
                                                                    "title": { "runs": [{ "text": "Search Song Two" }] },
                                                                    "ownerText": { "runs": [{ "text": "Artist Two", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCtwo" } } }] },
                                                                    "lengthText": { "simpleText": "4:05" },
                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/SRCHVID0002/hqdefault.jpg" }] }
                                                                }
                                                            },
                                                            {
                                                                "videoRenderer": {
                                                                    "videoId": "SRCHVID0003",
                                                                    "title": { "runs": [{ "text": "Search Song Three" }] },
                                                                    "ownerText": { "runs": [{ "text": "Artist Three", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCthr" } } }] },
                                                                    "lengthText": { "simpleText": "2:50" },
                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/SRCHVID0003/hqdefault.jpg" }] }
                                                                }
                                                            }
                                                        ]
                                                    }
                                                }]
                                            }
                                        }
                                    }
                                }
                            })
                            .to_string()
                        } else if req.contains("/youtubei/v1/browse") {
                            json!({
                                "metadata": {
                                    "playlistMetadataRenderer": {
                                        "title": "Mock Playlist"
                                    }
                                },
                                "contents": {
                                    "twoColumnBrowseResultsRenderer": {
                                        "tabs": [{
                                            "tabRenderer": {
                                                "selected": true,
                                                "content": {
                                                    "sectionListRenderer": {
                                                        "contents": [{
                                                            "itemSectionRenderer": {
                                                                "contents": [{
                                                                    "playlistVideoListRenderer": {
                                                                        "playlistId": "PLmocklist123",
                                                                        "contents": [
                                                                            {
                                                                                "playlistVideoRenderer": {
                                                                                    "videoId": "PLVID000001",
                                                                                    "title": {
                                                                                        "runs": [{ "text": "Playlist Track 1" }],
                                                                                        "accessibility": { "accessibilityData": { "label": "Playlist Track 1 by Band A" } }
                                                                                    },
                                                                                    "shortBylineText": { "runs": [{ "text": "Band A", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCbandA" } } }] },
                                                                                    "lengthSeconds": "180",
                                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/PLVID000001/hqdefault.jpg" }] }
                                                                                }
                                                                            },
                                                                            {
                                                                                "playlistVideoRenderer": {
                                                                                    "videoId": "PLVID000002",
                                                                                    "title": {
                                                                                        "runs": [{ "text": "Playlist Track 2" }],
                                                                                        "accessibility": { "accessibilityData": { "label": "Playlist Track 2 by Band B" } }
                                                                                    },
                                                                                    "shortBylineText": { "runs": [{ "text": "Band B", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCbandB" } } }] },
                                                                                    "lengthSeconds": "240",
                                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/PLVID000002/hqdefault.jpg" }] }
                                                                                }
                                                                            },
                                                                            {
                                                                                "playlistVideoRenderer": {
                                                                                    "videoId": "PLVID000003",
                                                                                    "title": {
                                                                                        "runs": [{ "text": "Playlist Track 3" }],
                                                                                        "accessibility": { "accessibilityData": { "label": "Playlist Track 3 by Band C" } }
                                                                                    },
                                                                                    "shortBylineText": { "runs": [{ "text": "Band C", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCbandC" } } }] },
                                                                                    "lengthSeconds": "210",
                                                                                    "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/PLVID000003/hqdefault.jpg" }] }
                                                                                }
                                                                            }
                                                                        ]
                                                                    }
                                                                }]
                                                            }
                                                        }]
                                                    }
                                                }
                                            }
                                        }]
                                    }
                                }
                            })
                            .to_string()
                        } else {
                            "{}".to_owned()
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

        let client = reqwest::Client::new();

        // 1. `ytsearch2:` with `--flat-playlist` emits 2 NDJSON lines.
        let p_search_flat = VirtualYtdlpShim::spawn_with_origin(
            &[
                "--flat-playlist".to_owned(),
                "ytsearch2:mock song".to_owned(),
            ],
            client.clone(),
            origin.clone(),
            true,
        );
        let s1: Value =
            serde_json::from_str(&p_search_flat.read_line_blocking().expect("search line 1"))
                .unwrap();
        let s2: Value =
            serde_json::from_str(&p_search_flat.read_line_blocking().expect("search line 2"))
                .unwrap();
        assert_eq!(p_search_flat.read_line_blocking(), None);
        assert_eq!(p_search_flat.wait(), 0);
        assert_eq!(s1["id"], "SRCHVID0001");
        assert_eq!(s1["title"], "Search Song One");
        assert_eq!(s1["duration"], 200);
        assert_eq!(s2["id"], "SRCHVID0002");

        // 2. `ytsearch3:` with `-J` and `-I 2:3` emits single playlist JSON containing items 2 & 3.
        let p_search_j = VirtualYtdlpShim::spawn_with_origin(
            &[
                "-J".to_owned(),
                "-I".to_owned(),
                "2:3".to_owned(),
                "ytsearch3:mock song".to_owned(),
            ],
            client.clone(),
            origin.clone(),
            true,
        );
        let sj: Value =
            serde_json::from_str(&p_search_j.read_line_blocking().expect("-J search line"))
                .unwrap();
        assert_eq!(p_search_j.wait(), 0);
        assert_eq!(sj["_type"], "playlist");
        assert_eq!(sj["extractor"], "youtube:search");
        let sj_entries = sj["entries"].as_array().unwrap();
        assert_eq!(sj_entries.len(), 2);
        assert_eq!(sj_entries[0]["id"], "SRCHVID0002");
        assert_eq!(sj_entries[1]["id"], "SRCHVID0003");

        // 3. `playlist?list=PLmocklist123` with `-J` emits single playlist JSON with all 3 tracks.
        let p_pl_j = VirtualYtdlpShim::spawn_with_origin(
            &[
                "-J".to_owned(),
                "https://www.youtube.com/playlist?list=PLmocklist123".to_owned(),
            ],
            client.clone(),
            origin.clone(),
            true,
        );
        let pl_val: Value =
            serde_json::from_str(&p_pl_j.read_line_blocking().expect("-J playlist line")).unwrap();
        assert_eq!(p_pl_j.wait(), 0);
        assert_eq!(pl_val["_type"], "playlist");
        assert_eq!(pl_val["id"], "PLmocklist123");
        assert_eq!(pl_val["title"], "Mock Playlist");
        assert_eq!(pl_val["entries"].as_array().unwrap().len(), 3);

        // 4. `playlist?list=PLmocklist123` with `-j -I 1,3` emits 2 NDJSON lines (items #1 & #3).
        let p_pl_nd = VirtualYtdlpShim::spawn_with_origin(
            &[
                "-j".to_owned(),
                "-I".to_owned(),
                "1,3".to_owned(),
                "https://www.youtube.com/playlist?list=PLmocklist123".to_owned(),
            ],
            client,
            origin,
            true,
        );
        let pl1: Value =
            serde_json::from_str(&p_pl_nd.read_line_blocking().expect("pl line 1")).unwrap();
        let pl3: Value =
            serde_json::from_str(&p_pl_nd.read_line_blocking().expect("pl line 2")).unwrap();
        assert_eq!(p_pl_nd.read_line_blocking(), None);
        assert_eq!(p_pl_nd.wait(), 0);
        assert_eq!(pl1["id"], "PLVID000001");
        assert_eq!(pl3["id"], "PLVID000003");

        stop.store(true, Ordering::Relaxed);
        let _ = srv.join();
    }

    #[test]
    #[ignore = "live network test verifying YouTube.js ytsearch2: extraction"]
    fn live_youtubejs_search_and_playlist_extraction() {
        let client = reqwest::Client::new();
        let pipe = VirtualYtdlpShim::spawn_with_origin(
            &[
                "-J".to_owned(),
                "ytsearch2:Never Gonna Give You Up Rick Astley".to_owned(),
            ],
            client,
            DEFAULT_INNERTUBE_ORIGIN.to_owned(),
            true,
        );
        let line = pipe
            .read_line_blocking()
            .expect("live ytsearch2 -J output line");
        let code = pipe.wait();
        let err = pipe.error_output();
        assert_eq!(code, 0, "live ytsearch2 failed: {err}");
        let val: Value = serde_json::from_str(&line).expect("valid -J JSON");
        assert_eq!(val["_type"], "playlist");
        let entries = val["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2, "expected 2 search entries: {val}");
        for entry in entries {
            assert!(
                entry["id"].as_str().is_some_and(|s| s.len() == 11),
                "expected 11-char video id: {entry}"
            );
            assert!(
                entry["title"].as_str().is_some_and(|s| !s.is_empty()),
                "expected non-empty title: {entry}"
            );
        }
        println!(
            "LIVE ytsearch2 OK: [0]={} ({:?}), [1]={} ({:?})",
            entries[0]["id"], entries[0]["title"], entries[1]["id"], entries[1]["title"]
        );
    }

    #[test]
    #[ignore = "live network test verifying YouTube.js 18.3.0 + yt-dlp/ejs 0.8.0 resolves dQw4w9WgXcQ, WMOPURvS_3s, trxuXrVJxy4 with HTTP 200/206 audio bytes"]
    fn live_youtubejs_plus_ejs_resolves_real_streams_http_206() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Some(Duration::from_secs(30)))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .tcp_nodelay(true)
            .build()
            .expect("reqwest client");
        let cache_dir =
            std::env::temp_dir().join(format!("nsh-live-youtubejs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        for video_id in ["dQw4w9WgXcQ", "WMOPURvS_3s", "trxuXrVJxy4"] {
            let t0 = Instant::now();
            let resolved = rt
                .block_on(resolve_innertube_stream_with_cache(
                    &client,
                    DEFAULT_INNERTUBE_ORIGIN,
                    video_id,
                    None,
                    None,
                    true,
                    true,
                    Some(&cache_dir),
                ))
                .unwrap_or_else(|e| panic!("YouTube.js+EJS failed for {video_id}: {e}"));
            let elapsed_ms = t0.elapsed().as_millis();

            let (status, ct, nbytes) = rt.block_on(async {
                let mut req = client
                    .get(&resolved.url)
                    .header("user-agent", &resolved.user_agent)
                    .timeout(Duration::from_secs(10));
                if !resolved.is_hls {
                    req = req.header("range", "bytes=0-1023");
                }
                let resp = req.send().await.expect("stream fetch");
                let status = resp.status();
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let bytes = resp.bytes().await.expect("read bytes");
                (status, ct, bytes.len())
            });

            println!(
                "LIVE YouTube.js+EJS video={video_id} elapsed={elapsed_ms}ms format={} ext={} hls={} HTTP={} ct={ct} bytes={nbytes} title={:?}",
                resolved.format_id,
                resolved.ext,
                resolved.is_hls,
                status.as_u16(),
                resolved.title,
            );
            assert!(
                status == reqwest::StatusCode::OK || status == reqwest::StatusCode::PARTIAL_CONTENT,
                "expected HTTP 200/206 for {video_id}, got {status}"
            );
            assert!(nbytes > 0, "expected non-empty audio bytes for {video_id}");
        }

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    fn load_local_mac_ytm_cookie() -> Option<String> {
        std::env::var("NSH_YTM_COOKIE")
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
            })
    }

    #[test]
    #[ignore = "live network test verifying exact youtube-music/index.js CLI invocations on 1_8jBVA9a3U"]
    fn live_yt_ejs_matches_youtube_music_plugin_cli_invocations() {
        let cookie = load_local_mac_ytm_cookie();
        let cache_dir =
            std::env::temp_dir().join(format!("nsh-live-yt-ejs-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        let client = reqwest::Client::new();

        // 1. Exact logged-in `youtube-music/index.js` CLI invocation on `1_8jBVA9a3U`:
        //    --config-locations - --extractor-args youtube:player_client=web_safari
        //    --format bestaudio[protocol^=m3u8]/93/94/92/95/91 --print %(.{url,http_headers})j
        if let Some(ref c) = cookie {
            let t1 = Instant::now();
            let logged_in_args: Vec<String> = [
                "--config-locations",
                "-",
                "--extractor-args",
                "youtube:player_client=web_safari",
                "--format",
                "bestaudio[protocol^=m3u8]/93/94/92/95/91",
                "--print",
                "%(.{url,http_headers})j",
                "https://www.youtube.com/watch?v=1_8jBVA9a3U",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();

            let pipe1 = VirtualYtdlpShim::spawn_with_origin_and_cache(
                &logged_in_args,
                client.clone(),
                DEFAULT_INNERTUBE_ORIGIN.to_owned(),
                true,
                Some(cache_dir.clone()),
            );
            let escaped = c.replace('\\', "\\\\").replace('"', "\\\"");
            pipe1
                .write_stdin(&format!("--add-headers \"Cookie:{escaped}\"\n"))
                .unwrap();
            pipe1.close_stdin();

            let out1 = pipe1.read_line_blocking();
            let code1 = pipe1.wait();
            let err1 = pipe1.error_output();
            println!(
                "CLI #1 (logged-in web_safari HLS): code={code1} elapsed={}ms err={:?}",
                t1.elapsed().as_millis(),
                err1.trim()
            );
            assert_eq!(code1, 0, "logged-in web_safari CLI failed: {err1}");
            let line1 = out1.expect("stdout JSON line for logged-in CLI");
            let parsed1: Value = serde_json::from_str(&line1).expect("valid JSON output");
            let url1 = parsed1["url"].as_str().expect("url string");
            println!(
                "CLI #1 resolved host={} ua={:?}",
                sanitize_url_for_log(url1),
                parsed1["http_headers"]["User-Agent"].as_str()
            );
            assert!(url1.starts_with("https://"), "expected HTTPS stream URL");
        } else {
            println!("CLI #1 skipped: no local Mac YouTube Music cookie found");
        }

        // 2. Exact logged-out `youtube-music/index.js` CLI invocation on `1_8jBVA9a3U`:
        //    --extractor-args youtube:skip=hls,dash --format bestaudio[protocol^=http]/bestaudio
        //    --print %(.{url,http_headers})j
        let t2 = Instant::now();
        let logged_out_args: Vec<String> = [
            "--extractor-args",
            "youtube:skip=hls,dash",
            "--format",
            "bestaudio[protocol^=http]/bestaudio",
            "--print",
            "%(.{url,http_headers})j",
            "https://www.youtube.com/watch?v=1_8jBVA9a3U",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        let pipe2 = VirtualYtdlpShim::spawn_with_origin_and_cache(
            &logged_out_args,
            client,
            DEFAULT_INNERTUBE_ORIGIN.to_owned(),
            true,
            Some(cache_dir.clone()),
        );
        let out2 = pipe2.read_line_blocking();
        let code2 = pipe2.wait();
        let err2 = pipe2.error_output();
        println!(
            "CLI #2 (logged-out skip=hls,dash HTTP): code={code2} elapsed={}ms err={:?}",
            t2.elapsed().as_millis(),
            err2.trim()
        );
        assert_eq!(code2, 0, "logged-out HTTP CLI failed: {err2}");
        let line2 = out2.expect("stdout JSON line for logged-out CLI");
        let parsed2: Value = serde_json::from_str(&line2).expect("valid JSON output");
        let url2 = parsed2["url"].as_str().expect("url string");
        println!(
            "CLI #2 resolved host={} ua={:?}",
            sanitize_url_for_log(url2),
            parsed2["http_headers"]["User-Agent"].as_str()
        );
        assert!(url2.starts_with("https://"), "expected HTTPS stream URL");
        assert!(
            !url2.contains(".m3u8"),
            "skip=hls,dash must not return an HLS manifest URL"
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
