//! Upstream `yt-dlp` `--extractor-args youtube:...` parser and `-f` / `--format` selector engine.

use std::collections::HashSet;

use crate::yt_clients::normalize_client_alias;

/// YouTube Proof-of-Origin (`po_token`) context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoTokenContext {
    Gvs,
    Player,
    Subs,
}

/// Parsed `po_token` entry from `--extractor-args youtube:po_token=<client>.<ctx>+<token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoTokenEntry {
    pub client: String,
    pub context: PoTokenContext,
    pub token: String,
}

/// Parsed `--extractor-args "youtube:k1=v1;k2=v2,v3"` options.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct YoutubeExtractorArgs {
    pub player_client: Vec<String>,
    pub skip_hls: bool,
    pub skip_dash: bool,
    pub player_js_variant: Option<String>,
    pub player_js_version: Option<String>,
    pub lang: Option<String>,
    pub visitor_data: Option<String>,
    pub po_tokens: Vec<PoTokenEntry>,
    pub formats: Vec<String>,
}

impl YoutubeExtractorArgs {
    /// Parses one or more raw `--extractor-args` flag values, ignoring non-`youtube:` namespaces.
    pub fn parse(raw_args: &[String]) -> Self {
        let mut out = Self::default();
        for raw in raw_args {
            let trimmed = raw.trim();
            let Some((ns, rest)) = trimmed.split_once(':') else {
                continue;
            };
            if !ns.trim().eq_ignore_ascii_case("youtube") {
                continue;
            }
            for item in rest.split(';') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                let (k_raw, v_raw) = match item.split_once('=') {
                    Some((k, v)) => (k.trim(), v.trim()),
                    None => (item, ""),
                };
                let key = k_raw.to_ascii_lowercase().replace('-', "_");
                match key.as_str() {
                    "player_client" => {
                        for tok in v_raw.split(',') {
                            let t = tok.trim();
                            if !t.is_empty() {
                                out.player_client.push(t.to_owned());
                            }
                        }
                    }
                    "skip" => {
                        for tok in v_raw.split(',') {
                            let t = tok.trim().to_ascii_lowercase();
                            if t == "hls" {
                                out.skip_hls = true;
                            } else if t == "dash" {
                                out.skip_dash = true;
                            }
                        }
                    }
                    "player_js_variant" => {
                        if !v_raw.is_empty() {
                            out.player_js_variant = Some(v_raw.to_owned());
                        }
                    }
                    "player_js_version" => {
                        if !v_raw.is_empty() {
                            out.player_js_version = Some(v_raw.to_owned());
                        }
                    }
                    "lang" => {
                        if !v_raw.is_empty() {
                            out.lang = Some(v_raw.to_owned());
                        }
                    }
                    "visitor_data" => {
                        if !v_raw.is_empty() {
                            out.visitor_data = Some(v_raw.to_owned());
                        }
                    }
                    "po_token" => {
                        for tok in v_raw.split(',') {
                            if let Some(entry) = parse_po_token_spec(tok) {
                                out.po_tokens.push(entry);
                            }
                        }
                    }
                    "formats" => {
                        for tok in v_raw.split(',') {
                            let t = tok.trim().to_ascii_lowercase();
                            if !t.is_empty() {
                                out.formats.push(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Finds the first matching `po_token` for `client_key` and `context`.
    pub fn find_po_token(&self, client_key: &str, context: PoTokenContext) -> Option<&str> {
        let norm_client = normalize_client_alias(client_key);
        self.po_tokens
            .iter()
            .find(|e| {
                e.context == context
                    && (e.client == norm_client || e.client == "all" || e.client == "*")
            })
            .map(|e| e.token.as_str())
    }
}

fn parse_po_token_spec(raw: &str) -> Option<PoTokenEntry> {
    let trimmed = raw.trim();
    let (prefix, token) = trimmed.split_once('+')?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let (client, ctx_str) = match prefix.trim().split_once('.') {
        Some((c, ctx)) => (normalize_client_alias(c), ctx.trim().to_ascii_lowercase()),
        None => ("all".to_owned(), prefix.trim().to_ascii_lowercase()),
    };
    let context = match ctx_str.as_str() {
        "gvs" => PoTokenContext::Gvs,
        "player" => PoTokenContext::Player,
        "subs" => PoTokenContext::Subs,
        _ => return None,
    };
    Some(PoTokenEntry {
        client,
        context,
        token: token.to_owned(),
    })
}

/// Format metadata inspected by [`FormatSelector`] when filtering and ranking candidates.
#[derive(Debug, Clone)]
pub struct CandidateFormatMeta {
    pub format_id: String,
    pub ext: String,
    pub protocol: String,
    pub acodec: String,
    pub vcodec: String,
    pub abr: Option<f64>,
    pub tbr: Option<f64>,
    pub asr: Option<u64>,
    pub audio_channels: Option<u32>,
    pub filesize: Option<u64>,
    pub is_drc: bool,
    pub is_default_audio_track: bool,
    pub is_original_audio_track: bool,
    pub is_hls: bool,
    pub is_muxed: bool,
}

impl CandidateFormatMeta {
    pub fn effective_bitrate_kbps(&self) -> f64 {
        self.abr.or(self.tbr).unwrap_or(0.0)
    }

    pub fn is_audio_only(&self) -> bool {
        !self.is_muxed && self.acodec != "none" && !self.acodec.is_empty()
    }

    pub fn has_audio(&self) -> bool {
        self.acodec != "none" && !self.acodec.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseTarget {
    BestAudio,
    WorstAudio,
    Best,
    ExactId(String),
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOp {
    Eq,
    Ne,
    StartsWith,
    EndsWith,
    Contains,
    NotContains,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BracketFilter {
    field: String,
    op: FilterOp,
    allow_missing: bool,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormatAlternative {
    base: BaseTarget,
    filters: Vec<BracketFilter>,
}

/// Parsed `-f` / `--format` fallback selector chain (`alt1/alt2/...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatSelector {
    alternatives: Vec<FormatAlternative>,
}

impl FormatSelector {
    /// Parses a `yt-dlp` format selector expression (e.g. `bestaudio[protocol^=m3u8]/93/94/92/95/91`).
    /// Empty expressions default to `bestaudio/best`.
    pub fn parse(expr: &str) -> Self {
        let trimmed = expr.trim();
        let effective = if trimmed.is_empty() {
            "bestaudio/best"
        } else {
            trimmed
        };
        let parts = split_outside_brackets(effective, '/');
        let mut alternatives = Vec::new();
        for part in parts {
            if let Some(alt) = parse_alternative(&part) {
                alternatives.push(alt);
            }
        }
        if alternatives.is_empty() {
            alternatives.push(FormatAlternative {
                base: BaseTarget::BestAudio,
                filters: Vec::new(),
            });
            alternatives.push(FormatAlternative {
                base: BaseTarget::Best,
                filters: Vec::new(),
            });
        }
        Self { alternatives }
    }

    /// Selects and orders candidate indices matching the `/` fallback chain in strict priority order.
    pub fn select_indices(
        &self,
        candidates: &[CandidateFormatMeta],
        prefer_mobile_aac: bool,
    ) -> Vec<usize> {
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();

        for alt in &self.alternatives {
            let mut matched: Vec<usize> = candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| matches_alternative(alt, c))
                .map(|(i, _)| i)
                .collect();

            let mandates_specific = matches!(alt.base, BaseTarget::ExactId(_))
                || alt.filters.iter().any(|f| {
                    matches!(f.field.as_str(), "ext" | "acodec" | "format_id")
                });
            let reverse_bitrate = matches!(alt.base, BaseTarget::WorstAudio);

            matched.sort_by(|&a_idx, &b_idx| {
                let a = &candidates[a_idx];
                let b = &candidates[b_idx];
                compare_candidates(a, b, prefer_mobile_aac && !mandates_specific, reverse_bitrate)
            });

            for idx in matched {
                if seen.insert(idx) {
                    ordered.push(idx);
                }
            }
        }

        ordered
    }
}

fn compare_candidates(
    a: &CandidateFormatMeta,
    b: &CandidateFormatMeta,
    prefer_mobile_aac: bool,
    worst_first: bool,
) -> std::cmp::Ordering {
    // 1. Prefer audio-only over muxed when both matched the same alternative.
    b.is_audio_only()
        .cmp(&a.is_audio_only())
        // 2. Soft-rank original & default audio track ahead of non-default dubs (never hard-drop!).
        .then_with(|| b.is_original_audio_track.cmp(&a.is_original_audio_track))
        .then_with(|| b.is_default_audio_track.cmp(&a.is_default_audio_track))
        // 3. Soft-rank non-DRC ahead of DRC (never hard-drop!).
        .then_with(|| a.is_drc.cmp(&b.is_drc))
        // 4. Prefer direct HTTP ahead of HLS within the same non-protocol-specific alternative.
        .then_with(|| a.is_hls.cmp(&b.is_hls))
        // 5. On mobile (when selector doesn't mandate codec/ext/itag), prefer Symphonia-friendly AAC-LC.
        .then_with(|| {
            if prefer_mobile_aac && !worst_first {
                mobile_aac_tier(b).cmp(&mobile_aac_tier(a))
            } else {
                std::cmp::Ordering::Equal
            }
        })
        // 6. Bitrate order (highest first for best/bestaudio, lowest first for worstaudio).
        .then_with(|| {
            let ka = (a.effective_bitrate_kbps() * 100.0) as u64;
            let kb = (b.effective_bitrate_kbps() * 100.0) as u64;
            if worst_first {
                ka.cmp(&kb)
            } else {
                kb.cmp(&ka)
            }
        })
        // 7. Prefer higher sample rate as tie-breaker.
        .then_with(|| b.asr.unwrap_or(0).cmp(&a.asr.unwrap_or(0)))
}

fn mobile_aac_tier(c: &CandidateFormatMeta) -> u8 {
    let ac = c.acodec.to_ascii_lowercase();
    let is_mp4 = c.ext.eq_ignore_ascii_case("m4a") || c.ext.eq_ignore_ascii_case("mp4");
    if is_mp4 && (ac.contains("mp4a.40.2") || matches!(c.format_id.as_str(), "139" | "140" | "141"))
    {
        match c.format_id.as_str() {
            "141" => 5,
            "140" => 4,
            "139" => 2,
            _ => 3,
        }
    } else if ac.contains("mp4a.40.2") {
        3
    } else if ac.contains("opus") {
        // Opus is built-in via `opus-pure`; keep 256k itag=774 cleanly ranked below AAC-LC on mobile.
        if c.format_id == "774" { 2 } else { 1 }
    } else {
        0
    }
}

fn matches_alternative(alt: &FormatAlternative, cand: &CandidateFormatMeta) -> bool {
    let base_ok = match &alt.base {
        BaseTarget::BestAudio | BaseTarget::WorstAudio => cand.is_audio_only(),
        BaseTarget::Best => cand.has_audio(),
        BaseTarget::ExactId(id) => cand.format_id.eq_ignore_ascii_case(id),
        BaseTarget::Any => cand.has_audio(),
    };
    if !base_ok {
        return false;
    }
    alt.filters.iter().all(|f| matches_filter(f, cand))
}

fn matches_filter(f: &BracketFilter, cand: &CandidateFormatMeta) -> bool {
    match f.field.as_str() {
        "protocol" => eval_str_filter(Some(cand.protocol.as_str()), f),
        "ext" => eval_str_filter(Some(cand.ext.as_str()), f),
        "acodec" => eval_str_filter(Some(cand.acodec.as_str()), f),
        "vcodec" => eval_str_filter(Some(cand.vcodec.as_str()), f),
        "format_id" => eval_str_filter(Some(cand.format_id.as_str()), f),
        "abr" => eval_num_filter(cand.abr, f),
        "tbr" => eval_num_filter(cand.tbr.or(cand.abr), f),
        "asr" => eval_num_filter(cand.asr.map(|v| v as f64), f),
        "audio_channels" => eval_num_filter(cand.audio_channels.map(f64::from), f),
        "filesize" => eval_num_filter(cand.filesize.map(|v| v as f64), f),
        _ => f.allow_missing,
    }
}

fn eval_str_filter(actual: Option<&str>, f: &BracketFilter) -> bool {
    let Some(val) = actual.filter(|s| !s.is_empty()) else {
        return f.allow_missing;
    };
    let a = val.to_ascii_lowercase();
    let b = f.value.to_ascii_lowercase();
    match f.op {
        FilterOp::Eq => a == b,
        FilterOp::Ne => a != b,
        FilterOp::StartsWith => a.starts_with(&b),
        FilterOp::EndsWith => a.ends_with(&b),
        FilterOp::Contains => a.contains(&b),
        FilterOp::NotContains => !a.contains(&b),
        FilterOp::Lt => a < b,
        FilterOp::Le => a <= b,
        FilterOp::Gt => a > b,
        FilterOp::Ge => a >= b,
    }
}

fn eval_num_filter(actual: Option<f64>, f: &BracketFilter) -> bool {
    let Some(a) = actual else {
        return f.allow_missing;
    };
    let Ok(b) = f.value.parse::<f64>() else {
        return false;
    };
    match f.op {
        FilterOp::Eq => (a - b).abs() < 1e-6,
        FilterOp::Ne => (a - b).abs() >= 1e-6,
        FilterOp::Lt => a < b,
        FilterOp::Le => a <= b,
        FilterOp::Gt => a > b,
        FilterOp::Ge => a >= b,
        FilterOp::StartsWith | FilterOp::EndsWith | FilterOp::Contains | FilterOp::NotContains => {
            eval_str_filter(Some(&a.to_string()), f)
        }
    }
}

fn split_outside_brackets(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for ch in input.chars() {
        match ch {
            '[' => {
                depth += 1;
                cur.push(ch);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            c if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_alternative(raw: &str) -> Option<FormatAlternative> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first_bracket = trimmed.find('[');
    let base_str = match first_bracket {
        Some(idx) => trimmed[..idx].trim(),
        None => trimmed,
    };
    let base = match base_str.to_ascii_lowercase().as_str() {
        "" => BaseTarget::Any,
        "bestaudio" | "ba" => BaseTarget::BestAudio,
        "worstaudio" | "wa" => BaseTarget::WorstAudio,
        "best" | "b" => BaseTarget::Best,
        other => BaseTarget::ExactId(other.to_owned()),
    };

    let mut filters = Vec::new();
    if let Some(mut rest) = first_bracket.map(|idx| &trimmed[idx..]) {
        while let Some(open) = rest.find('[') {
            let after_open = &rest[open + 1..];
            let Some(close) = after_open.find(']') else {
                break;
            };
            let inside = after_open[..close].trim();
            if let Some(f) = parse_bracket_filter(inside) {
                filters.push(f);
            }
            rest = &after_open[close + 1..];
        }
    }

    Some(FormatAlternative { base, filters })
}

fn parse_bracket_filter(inside: &str) -> Option<BracketFilter> {
    const OPS: &[(&str, FilterOp, bool)] = &[
        ("<=?", FilterOp::Le, true),
        (">=?", FilterOp::Ge, true),
        ("!=?", FilterOp::Ne, true),
        ("^=?", FilterOp::StartsWith, true),
        ("$=?", FilterOp::EndsWith, true),
        ("*=?", FilterOp::Contains, true),
        ("!*?", FilterOp::NotContains, true),
        ("=?", FilterOp::Eq, true),
        ("<?", FilterOp::Lt, true),
        (">?", FilterOp::Gt, true),
        ("<=", FilterOp::Le, false),
        (">=", FilterOp::Ge, false),
        ("!=", FilterOp::Ne, false),
        ("^=", FilterOp::StartsWith, false),
        ("$=", FilterOp::EndsWith, false),
        ("*=", FilterOp::Contains, false),
        ("!*", FilterOp::NotContains, false),
        ("=", FilterOp::Eq, false),
        ("<", FilterOp::Lt, false),
        (">", FilterOp::Gt, false),
    ];

    // Pick the leftmost matching operator (breaking ties by longest operator token).
    let mut best: Option<(usize, usize, FilterOp, bool)> = None;
    for &(tok, op, allow_missing) in OPS {
        if let Some(pos) = inside.find(tok) {
            let replace = match best {
                None => true,
                Some((b_pos, b_len, _, _)) => pos < b_pos || (pos == b_pos && tok.len() > b_len),
            };
            if replace {
                best = Some((pos, tok.len(), op, allow_missing));
            }
        }
    }

    let (pos, len, op, allow_missing) = best?;
    let field = inside[..pos].trim().to_ascii_lowercase();
    let val_raw = inside[pos + len..].trim();
    let value = val_raw
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| val_raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(val_raw)
        .to_owned();
    if field.is_empty() {
        return None;
    }
    Some(BracketFilter {
        field,
        op,
        allow_missing,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_fmt(
        id: &str,
        ext: &str,
        protocol: &str,
        acodec: &str,
        vcodec: &str,
        abr: Option<f64>,
    ) -> CandidateFormatMeta {
        CandidateFormatMeta {
            format_id: id.to_owned(),
            ext: ext.to_owned(),
            protocol: protocol.to_owned(),
            acodec: acodec.to_owned(),
            vcodec: vcodec.to_owned(),
            abr,
            tbr: abr,
            asr: Some(44100),
            audio_channels: Some(2),
            filesize: None,
            is_drc: false,
            is_default_audio_track: true,
            is_original_audio_track: true,
            is_hls: protocol.starts_with("m3u8"),
            is_muxed: vcodec != "none",
        }
    }

    #[test]
    fn parses_youtube_extractor_args_across_multiple_flags() {
        let raw = vec![
            "generic:impersonate=chrome".to_owned(),
            "youtube:player_client=web_safari,default,-ios;skip=hls,dash;lang=zh-CN".to_owned(),
            "youtube:po_token=web_safari.gvs+GVS_TOK_1,web_remix.player+PL_TOK_2;player_js_variant=tv_es6;player_js_version=b485e332;visitor_data=VIS_X;formats=missing_pot,duplicate".to_owned(),
        ];
        let parsed = YoutubeExtractorArgs::parse(&raw);
        assert_eq!(parsed.player_client, vec!["web_safari", "default", "-ios"]);
        assert!(parsed.skip_hls);
        assert!(parsed.skip_dash);
        assert_eq!(parsed.lang.as_deref(), Some("zh-CN"));
        assert_eq!(parsed.player_js_variant.as_deref(), Some("tv_es6"));
        assert_eq!(parsed.player_js_version.as_deref(), Some("b485e332"));
        assert_eq!(parsed.visitor_data.as_deref(), Some("VIS_X"));
        assert_eq!(parsed.formats, vec!["missing_pot", "duplicate"]);
        assert_eq!(
            parsed.find_po_token("web_safari", PoTokenContext::Gvs),
            Some("GVS_TOK_1")
        );
        assert_eq!(
            parsed.find_po_token("web_music", PoTokenContext::Player),
            Some("PL_TOK_2")
        );
        assert_eq!(parsed.find_po_token("ios", PoTokenContext::Gvs), None);
    }

    #[test]
    fn format_selector_handles_chains_predicates_and_muxed_itags() {
        let candidates = vec![
            mk_fmt("91", "m3u8", "m3u8_native", "mp4a.40.5", "avc1.42c00c", Some(86.0)),
            mk_fmt("93", "m3u8", "m3u8_native", "mp4a.40.2", "avc1.4d401e", Some(263.0)),
            mk_fmt("94", "m3u8", "m3u8_native", "mp4a.40.2", "avc1.4d401f", Some(322.0)),
            mk_fmt("251", "webm", "https", "opus", "none", Some(160.0)),
            mk_fmt("774", "webm", "https", "opus", "none", Some(256.0)),
            mk_fmt("140", "m4a", "https", "mp4a.40.2", "none", Some(128.0)),
            mk_fmt("141", "m4a", "https", "mp4a.40.2", "none", Some(256.0)),
            mk_fmt("18", "mp4", "https", "mp4a.40.2", "avc1.42001e", Some(96.0)),
        ];

        // 1. Exact `youtube-music` logged-in selector: `bestaudio[protocol^=m3u8]` matches no audio-only HLS,
        //    so it falls through cleanly to `93` -> `94` -> `91`!
        let sel_hls = FormatSelector::parse("bestaudio[protocol^=m3u8]/93/94/92/95/91");
        let idx_hls = sel_hls.select_indices(&candidates, true);
        let ids_hls: Vec<&str> = idx_hls
            .iter()
            .map(|&i| candidates[i].format_id.as_str())
            .collect();
        assert_eq!(ids_hls, vec!["93", "94", "91"]);

        // 2. Logged-out selector `bestaudio[protocol^=http]/bestaudio` on mobile prefers `141` -> `140` -> `774` -> `251`.
        let sel_http = FormatSelector::parse("bestaudio[protocol^=http]/bestaudio");
        let idx_mob = sel_http.select_indices(&candidates, true);
        let ids_mob: Vec<&str> = idx_mob
            .iter()
            .map(|&i| candidates[i].format_id.as_str())
            .collect();
        assert_eq!(ids_mob, vec!["141", "140", "774", "251"]);

        // 3. Explicit bitrate & ext filter `[ext=m4a][abr<=?160]` selects `140`, then falls back to `bestaudio`.
        let sel_cap = FormatSelector::parse("bestaudio[ext=m4a][abr<=?160]/bestaudio");
        let idx_cap = sel_cap.select_indices(&candidates, false);
        assert_eq!(candidates[idx_cap[0]].format_id, "140");

        // 4. Explicit muxed itag `18` matches `formats[]`.
        let sel_18 = FormatSelector::parse("18");
        let idx_18 = sel_18.select_indices(&candidates, true);
        assert_eq!(idx_18.len(), 1);
        assert_eq!(candidates[idx_18[0]].format_id, "18");

        // 5. Desktop (`prefer_mobile_aac = false`) ranks 256k `774`/`141` at the top.
        let sel_desk = FormatSelector::parse("bestaudio");
        let idx_desk = sel_desk.select_indices(&candidates, false);
        assert!(matches!(
            candidates[idx_desk[0]].format_id.as_str(),
            "774" | "141"
        ));
    }

    #[test]
    fn soft_ranks_drc_and_non_default_audio_tracks_without_hard_dropping() {
        let mut drc_only = mk_fmt("140-drc", "m4a", "https", "mp4a.40.2", "none", Some(128.0));
        drc_only.is_drc = true;
        drc_only.is_default_audio_track = false;
        drc_only.is_original_audio_track = false;

        // When only a DRC/non-default format exists, it MUST still be returned (never hard-dropped!).
        let sel = FormatSelector::parse("bestaudio/best");
        let only_idx = sel.select_indices(std::slice::from_ref(&drc_only), true);
        assert_eq!(only_idx, vec![0]);

        // When a non-DRC default track also exists, it ranks strictly ahead of DRC/non-default.
        let normal = mk_fmt("140", "m4a", "https", "mp4a.40.2", "none", Some(128.0));
        let both = vec![drc_only, normal];
        let ranked = sel.select_indices(&both, true);
        assert_eq!(ranked, vec![1, 0]);
    }
}
