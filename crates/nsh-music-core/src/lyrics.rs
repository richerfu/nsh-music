use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{LyricLine, LyricWord, Lyrics};

/// Default maximum timestamp delta (in milliseconds) when aligning translation
/// or romaji lines against primary lyric lines.
pub const DEFAULT_ALIGN_TOLERANCE_MS: i64 = 500;

/// Metadata tags parsed from an LRC header (e.g., `[ti:]`, `[ar:]`, `[offset:]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LyricMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub author: Option<String>,
    pub by: Option<String>,
    /// Global timestamp offset in milliseconds from `[offset:]`.
    /// Following the LRC specification, a positive value shifts lyrics earlier
    /// (`adjusted = (raw - offset_ms).max(0)`), while a negative value delays them.
    pub offset_ms: i64,
    pub extra: BTreeMap<String, String>,
}

/// A parsed lyric line with optional translation, romaji, and word-level timings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParsedLyricLine {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub translation: Option<String>,
    pub romaji: Option<String>,
    pub words: Vec<LyricWord>,
}

impl ParsedLyricLine {
    /// Converts this parsed line into the wire-level [`LyricLine`] model.
    pub fn to_model_line(&self) -> LyricLine {
        LyricLine {
            start: self.start_ms,
            end: self.end_ms,
            text: self.text.clone(),
            translation: self.translation.clone().unwrap_or_default(),
            words: self.words.clone(),
        }
    }

    /// Creates a [`ParsedLyricLine`] from a [`LyricLine`].
    pub fn from_model_line(line: &LyricLine) -> Self {
        Self {
            start_ms: line.start,
            end_ms: line.end,
            text: line.text.clone(),
            translation: (!line.translation.is_empty()).then(|| line.translation.clone()),
            romaji: None,
            words: line.words.clone(),
        }
    }

    /// Finds the active word index within this line at `position_ms` using binary search.
    pub fn active_word_index(&self, position_ms: i64) -> Option<usize> {
        find_active_word(&self.words, position_ms)
    }

    /// Normalized progress `0.0..=1.0` through this line at `position_ms`.
    pub fn progress(&self, position_ms: i64) -> f32 {
        compute_progress(self.start_ms, self.end_ms, position_ms)
    }
}

/// Active lyric line, optional active word, and normalized progression values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveLyric {
    pub line_index: usize,
    pub word_index: Option<usize>,
    pub line_progress: f32,
    pub word_progress: f32,
}

/// Complete parsed lyrics document supporting LRC, enhanced word-synced LRC,
/// YRC/QRC/KRC word timestamps, translation/romaji alignment, and plain-text fallback.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParsedLyrics {
    pub meta: LyricMeta,
    pub lines: Vec<ParsedLyricLine>,
    pub synced: bool,
    pub word_synced: bool,
}

impl ParsedLyrics {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Binary-searches the active line index for `position_ms`.
    /// Returns `None` for unsynced lyrics or when `position_ms` precedes the first line.
    pub fn active_line_index(&self, position_ms: i64) -> Option<usize> {
        if !self.synced || self.lines.is_empty() {
            return None;
        }
        let idx = self
            .lines
            .partition_point(|line| line.start_ms <= position_ms);
        idx.checked_sub(1)
    }

    /// Resolves active line index, optional active word index, and progress values at `position_ms`.
    pub fn active_position(&self, position_ms: i64) -> Option<ActiveLyric> {
        let line_index = self.active_line_index(position_ms)?;
        let line = &self.lines[line_index];
        let line_progress = line.progress(position_ms);
        let word_index = line.active_word_index(position_ms);
        let word_progress = word_index
            .map(|wi| word_progress(&line.words[wi], position_ms))
            .unwrap_or(line_progress);

        Some(ActiveLyric {
            line_index,
            word_index,
            line_progress,
            word_progress,
        })
    }

    /// Aligns a translation LRC string onto this document's lines by timestamp proximity.
    pub fn align_translation(&mut self, translation_lrc: &str, max_delta_ms: i64) {
        if translation_lrc.trim().is_empty() || self.lines.is_empty() {
            return;
        }
        let trans = parse_lrc(translation_lrc);
        if trans.lines.is_empty() {
            return;
        }
        if self.synced && trans.synced {
            align_secondary_lines(&mut self.lines, &trans.lines, max_delta_ms, |line, text| {
                line.translation = Some(text);
            });
        } else if trans.lines.len() == self.lines.len() {
            for (line, t) in self.lines.iter_mut().zip(trans.lines) {
                if !t.text.is_empty() {
                    line.translation = Some(t.text);
                }
            }
        }
    }

    /// Aligns a romaji / pronunciation LRC string onto this document's lines by timestamp proximity.
    pub fn align_romaji(&mut self, romaji_lrc: &str, max_delta_ms: i64) {
        if romaji_lrc.trim().is_empty() || self.lines.is_empty() {
            return;
        }
        let romaji = parse_lrc(romaji_lrc);
        if romaji.lines.is_empty() {
            return;
        }
        if self.synced && romaji.synced {
            align_secondary_lines(
                &mut self.lines,
                &romaji.lines,
                max_delta_ms,
                |line, text| {
                    line.romaji = Some(text);
                },
            );
        } else if romaji.lines.len() == self.lines.len() {
            for (line, r) in self.lines.iter_mut().zip(romaji.lines) {
                if !r.text.is_empty() {
                    line.romaji = Some(r.text);
                }
            }
        }
    }

    /// Converts back to [`Lyrics`] with populated `lines`.
    pub fn to_model_lyrics(&self) -> Lyrics {
        Lyrics {
            lrc: String::new(),
            translation: String::new(),
            lines: self
                .lines
                .iter()
                .map(ParsedLyricLine::to_model_line)
                .collect(),
        }
    }
}

/// Binary-searches the active line index directly on a slice of model [`LyricLine`]s.
/// Assumes `lines` are sorted in ascending `start` order.
pub fn find_active_line(lines: &[LyricLine], position_ms: i64) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    let idx = lines.partition_point(|line| line.start <= position_ms);
    idx.checked_sub(1)
}

/// Binary-searches the active word index directly on a slice of [`LyricWord`]s.
/// Assumes `words` are sorted in ascending `start` order.
pub fn find_active_word(words: &[LyricWord], position_ms: i64) -> Option<usize> {
    if words.is_empty() {
        return None;
    }
    let idx = words.partition_point(|word| word.start <= position_ms);
    idx.checked_sub(1)
}

/// Computes normalized progress `0.0..=1.0` for a model [`LyricLine`].
pub fn line_progress(line: &LyricLine, position_ms: i64) -> f32 {
    compute_progress(line.start, line.end, position_ms)
}

/// Computes normalized progress `0.0..=1.0` for a [`LyricWord`].
pub fn word_progress(word: &LyricWord, position_ms: i64) -> f32 {
    compute_progress(word.start, word.end, position_ms)
}

fn compute_progress(start: i64, end: i64, position_ms: i64) -> f32 {
    if position_ms <= start {
        return 0.0;
    }
    let span = end - start;
    if span <= 0 {
        return 1.0;
    }
    if position_ms >= end {
        return 1.0;
    }
    ((position_ms - start) as f32 / span as f32).clamp(0.0, 1.0)
}

/// Aligns sorted `secondary` lyric lines (e.g. translation or romaji) onto `primary` lines
/// by finding the closest timestamp within `max_delta_ms` using binary search.
pub fn align_secondary_lines(
    primary: &mut [ParsedLyricLine],
    secondary: &[ParsedLyricLine],
    max_delta_ms: i64,
    assign: impl Fn(&mut ParsedLyricLine, String),
) {
    if primary.is_empty() || secondary.is_empty() || max_delta_ms < 0 {
        return;
    }

    for line in primary.iter_mut() {
        let pos = secondary.partition_point(|s| s.start_ms < line.start_ms);
        let mut best_idx: Option<usize> = None;
        let mut best_delta = max_delta_ms + 1;

        for candidate in [pos.checked_sub(1), Some(pos)].into_iter().flatten() {
            if let Some(sec) = secondary.get(candidate) {
                if sec.text.trim().is_empty() {
                    continue;
                }
                let delta = (sec.start_ms - line.start_ms).abs();
                if delta <= max_delta_ms && delta < best_delta {
                    best_delta = delta;
                    best_idx = Some(candidate);
                }
            }
        }

        if let Some(idx) = best_idx {
            assign(line, secondary[idx].text.clone());
        }
    }
}

/// Parses a [`Lyrics`] payload from a `MusicSource`, preferring pre-parsed `lines` when present
/// and otherwise parsing `lrc`, then aligning `translation` if supplied.
pub fn parse_lyrics(raw: &Lyrics) -> ParsedLyrics {
    let mut parsed = if !raw.lines.is_empty() {
        let mut lines: Vec<ParsedLyricLine> = raw
            .lines
            .iter()
            .map(ParsedLyricLine::from_model_line)
            .collect();
        let synced = lines.iter().any(|l| l.start_ms > 0 || l.end_ms > 0);
        if synced {
            lines.sort_by_key(|l| l.start_ms);
            infer_end_times(&mut lines, &[]);
        }
        let word_synced = lines.iter().any(|l| !l.words.is_empty());
        ParsedLyrics {
            meta: LyricMeta::default(),
            lines,
            synced,
            word_synced,
        }
    } else {
        parse_lrc(&raw.lrc)
    };

    if !raw.translation.trim().is_empty() {
        let has_existing_translations = parsed.lines.iter().any(|l| l.translation.is_some());
        if !has_existing_translations {
            parsed.align_translation(&raw.translation, DEFAULT_ALIGN_TOLERANCE_MS);
        }
    }

    parsed
}

/// Parses primary LRC along with optional translation and romaji tracks.
pub fn parse_lyrics_with_romaji(
    lrc: &str,
    translation: Option<&str>,
    romaji: Option<&str>,
) -> ParsedLyrics {
    let mut parsed = parse_lrc(lrc);
    if let Some(trans) = translation {
        parsed.align_translation(trans, DEFAULT_ALIGN_TOLERANCE_MS);
    }
    if let Some(roma) = romaji {
        parsed.align_romaji(roma, DEFAULT_ALIGN_TOLERANCE_MS);
    }
    parsed
}

/// Parses standard LRC, multi-timestamp LRC, enhanced word-synced LRC (`<mm:ss.xx>`),
/// YRC/QRC/KRC word-timed lyrics, or falls back to plain-text unsynced lines.
pub fn parse_lrc(input: &str) -> ParsedLyrics {
    let cleaned = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut meta = LyricMeta::default();
    let mut timed_lines: Vec<ParsedLyricLine> = Vec::new();
    let mut break_timestamps: Vec<i64> = Vec::new();
    let mut plain_lines: Vec<String> = Vec::new();

    for raw_line in cleaned.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        // 1. YRC JSON credit/metadata line: {"t":0,"c":[{"tx":"作词: ..."}]}
        if line.starts_with('{')
            && line.ends_with('}')
            && let Some(json_line) = parse_yrc_json_line(line)
        {
            timed_lines.push(json_line);
            continue;
        }

        // 2. YRC / QRC / KRC millisecond-range line: [start_ms,duration_ms]...
        if let Some((start_ms, dur_ms, rest)) = parse_ms_range_prefix(line) {
            let end_ms = start_ms.saturating_add(dur_ms.max(0));
            let parsed_line = parse_word_range_body(start_ms, end_ms, rest);
            if parsed_line.text.trim().is_empty() {
                break_timestamps.push(start_ms);
            } else {
                timed_lines.push(parsed_line);
            }
            continue;
        }

        // 3. Standard / Enhanced LRC bracket tags: [mm:ss.xx] or [key:value]
        match parse_lrc_line(line, &mut meta) {
            LrcLineResult::Metadata => continue,
            LrcLineResult::Timed { lines, breaks } => {
                timed_lines.extend(lines);
                break_timestamps.extend(breaks);
            }
            LrcLineResult::Plain(text) => {
                plain_lines.push(text);
            }
        }
    }

    if timed_lines.is_empty() && break_timestamps.is_empty() {
        let lines = plain_lines
            .into_iter()
            .map(|text| ParsedLyricLine {
                start_ms: 0,
                end_ms: 0,
                text,
                translation: None,
                romaji: None,
                words: Vec::new(),
            })
            .collect();
        return ParsedLyrics {
            meta,
            lines,
            synced: false,
            word_synced: false,
        };
    }

    // Apply global [offset:] shift: positive offset moves lyrics earlier (subtracts from timestamps).
    if meta.offset_ms != 0 {
        let offset = meta.offset_ms;
        for line in &mut timed_lines {
            line.start_ms = line.start_ms.saturating_sub(offset).max(0);
            if line.end_ms > 0 {
                line.end_ms = line.end_ms.saturating_sub(offset).max(line.start_ms);
            }
            for w in &mut line.words {
                w.start = w.start.saturating_sub(offset).max(0);
                if w.end > 0 {
                    w.end = w.end.saturating_sub(offset).max(w.start);
                }
            }
        }
        for ts in &mut break_timestamps {
            *ts = ts.saturating_sub(offset).max(0);
        }
    }

    // Sort stably by timestamp and merge same-timestamp bilingual / romaji lines.
    timed_lines.sort_by_key(|l| l.start_ms);
    break_timestamps.sort_unstable();

    let mut merged: Vec<ParsedLyricLine> = Vec::with_capacity(timed_lines.len());
    for line in timed_lines {
        if let Some(prev) = merged.last_mut()
            && prev.start_ms == line.start_ms
            && prev.words.is_empty()
            && line.words.is_empty()
        {
            if prev.text == line.text {
                continue;
            }
            if prev.translation.is_none() {
                prev.translation = Some(line.text);
                continue;
            }
            if prev.romaji.is_none() {
                prev.romaji = Some(line.text);
                continue;
            }
        }
        merged.push(line);
    }

    infer_end_times(&mut merged, &break_timestamps);
    let word_synced = merged.iter().any(|l| !l.words.is_empty());

    ParsedLyrics {
        meta,
        lines: merged,
        synced: true,
        word_synced,
    }
}

fn infer_end_times(lines: &mut [ParsedLyricLine], break_timestamps: &[i64]) {
    let len = lines.len();
    for i in 0..len {
        let start = lines[i].start_ms;
        let next_line_start = lines.get(i + 1).map(|n| n.start_ms);
        let next_break = break_timestamps.iter().copied().find(|&b| b > start);

        let boundary = match (next_line_start, next_break) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        if lines[i].end_ms <= start {
            let word_end = lines[i].words.last().map(|w| w.end).filter(|&e| e > start);
            lines[i].end_ms = word_end
                .or(boundary)
                .unwrap_or_else(|| start.saturating_add(5_000));
        }

        let line_end = lines[i].end_ms;
        if let Some(last_word) = lines[i].words.last_mut()
            && last_word.end <= last_word.start
        {
            last_word.end = line_end.max(last_word.start);
        }
    }
}

enum LrcLineResult {
    Metadata,
    Timed {
        lines: Vec<ParsedLyricLine>,
        breaks: Vec<i64>,
    },
    Plain(String),
}

fn parse_lrc_line(line: &str, meta: &mut LyricMeta) -> LrcLineResult {
    let mut rest = line;
    let mut timestamps: Vec<i64> = Vec::new();

    while let Some(after_open) = rest.strip_prefix('[') {
        let Some(close_idx) = after_open.find(']') else {
            break;
        };
        let tag = &after_open[..close_idx];
        let next_rest = &after_open[close_idx + 1..];

        if let Some(ts) = parse_lrc_timestamp(tag) {
            timestamps.push(ts);
            rest = next_rest;
            continue;
        }

        if timestamps.is_empty()
            && let Some((k, v)) = tag.split_once(':')
        {
            let key = k.trim().to_ascii_lowercase();
            if is_metadata_key(&key) {
                apply_metadata_tag(meta, &key, v.trim());
                return LrcLineResult::Metadata;
            }
        }

        break;
    }

    if timestamps.is_empty() {
        return LrcLineResult::Plain(line.to_owned());
    }

    let body = rest.trim_matches(['\r', '\n']);
    if body.trim().is_empty() {
        return LrcLineResult::Timed {
            lines: Vec::new(),
            breaks: timestamps,
        };
    }

    // Check for enhanced word-synced LRC tags (`<mm:ss.xx>`)
    if let Some((base_words, explicit_end)) = parse_enhanced_lrc_words(timestamps[0], body) {
        let full_text: String = base_words.iter().map(|w| w.text.as_str()).collect();
        let trimmed_text = full_text.trim().to_owned();
        if trimmed_text.is_empty() {
            return LrcLineResult::Timed {
                lines: Vec::new(),
                breaks: timestamps,
            };
        }

        let base_ts = timestamps[0];
        let mut lines = Vec::with_capacity(timestamps.len());
        for ts in timestamps {
            let delta = ts - base_ts;
            let words = base_words
                .iter()
                .map(|w| LyricWord {
                    start: (w.start + delta).max(0),
                    end: if w.end > 0 { (w.end + delta).max(0) } else { 0 },
                    text: w.text.clone(),
                })
                .collect();
            let end_ms = explicit_end.map(|e| (e + delta).max(ts)).unwrap_or(0);
            lines.push(ParsedLyricLine {
                start_ms: ts,
                end_ms,
                text: trimmed_text.clone(),
                translation: None,
                romaji: None,
                words,
            });
        }
        return LrcLineResult::Timed {
            lines,
            breaks: Vec::new(),
        };
    }

    let text = body.trim().to_owned();
    let lines = timestamps
        .into_iter()
        .map(|ts| ParsedLyricLine {
            start_ms: ts,
            end_ms: 0,
            text: text.clone(),
            translation: None,
            romaji: None,
            words: Vec::new(),
        })
        .collect();

    LrcLineResult::Timed {
        lines,
        breaks: Vec::new(),
    }
}

fn is_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "ti" | "ar"
            | "al"
            | "au"
            | "by"
            | "offset"
            | "re"
            | "ve"
            | "length"
            | "la"
            | "id"
            | "sign"
            | "tool"
            | "hash"
            | "total"
    )
}

fn apply_metadata_tag(meta: &mut LyricMeta, key: &str, value: &str) {
    match key {
        "ti" => meta.title = Some(value.to_owned()),
        "ar" => meta.artist = Some(value.to_owned()),
        "al" => meta.album = Some(value.to_owned()),
        "au" => meta.author = Some(value.to_owned()),
        "by" => meta.by = Some(value.to_owned()),
        "offset" => {
            if let Ok(offset) = value.parse::<i64>() {
                meta.offset_ms = offset;
            }
        }
        _ => {
            meta.extra.insert(key.to_owned(), value.to_owned());
        }
    }
}

/// Parses an LRC timestamp such as `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, `mm:ss:xx`, or `hh:mm:ss.xx`.
pub fn parse_lrc_timestamp(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // Split optional fractional part after `.`
    let (main, dot_frac) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s, None),
    };

    let parts: Vec<&str> = main.split(':').collect();
    let (hours, minutes, seconds, frac_str): (i64, i64, i64, Option<&str>) = match parts.as_slice()
    {
        [mm, ss] => (0, parse_u64_digits(mm)?, parse_u64_digits(ss)?, dot_frac),
        [a, b, c] => {
            if dot_frac.is_some() {
                // hh:mm:ss.xxx
                (
                    parse_u64_digits(a)?,
                    parse_u64_digits(b)?,
                    parse_u64_digits(c)?,
                    dot_frac,
                )
            } else {
                // Distinguish mm:ss:xx (centiseconds with colon) vs hh:mm:ss:
                // In LRC, mm:ss:xx is overwhelmingly centiseconds/ms when 2-3 digits and b < 60.
                let first = parse_u64_digits(a)?;
                let second = parse_u64_digits(b)?;
                let third_str = *c;
                if third_str.len() == 2 || third_str.len() == 3 {
                    (0, first, second, Some(third_str))
                } else {
                    (first, second, parse_u64_digits(third_str)?, None)
                }
            }
        }
        _ => return None,
    };

    if seconds >= 60 && hours > 0 {
        return None;
    }

    let millis = match frac_str {
        None => 0,
        Some(f) => parse_fraction_ms(f)?,
    };

    let total_secs = hours
        .checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)?;
    total_secs.checked_mul(1000)?.checked_add(millis)
}

fn parse_u64_digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

fn parse_fraction_ms(f: &str) -> Option<i64> {
    if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match f.len() {
        1 => Some(f.parse::<i64>().ok()? * 100),
        2 => Some(f.parse::<i64>().ok()? * 10),
        3 => f.parse::<i64>().ok(),
        _ => f[..3].parse::<i64>().ok(),
    }
}

/// Parses enhanced word-synced LRC body containing `<mm:ss.xx>` markers.
fn parse_enhanced_lrc_words(
    line_start_ms: i64,
    body: &str,
) -> Option<(Vec<LyricWord>, Option<i64>)> {
    if !body.contains('<') {
        return None;
    }

    let mut words: Vec<LyricWord> = Vec::new();
    let mut found_tag = false;
    let mut explicit_end: Option<i64> = None;
    let mut cur_start = line_start_ms;
    let mut pos = 0;
    let bytes = body.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] == b'<'
            && let Some(rel_end) = body[pos + 1..].find('>')
        {
            let inner = &body[pos + 1..pos + 1 + rel_end];
            if let Some(ts) = parse_lrc_timestamp(inner) {
                found_tag = true;
                if let Some(prev) = words.last_mut()
                    && prev.end == 0
                {
                    prev.end = ts.max(prev.start);
                }
                cur_start = ts;
                pos += rel_end + 2;

                // Capture text until next valid `<ts>` tag (or end of line).
                let seg_start = pos;
                while pos < bytes.len() {
                    if bytes[pos] == b'<'
                        && let Some(next_end) = body[pos + 1..].find('>')
                        && parse_lrc_timestamp(&body[pos + 1..pos + 1 + next_end]).is_some()
                    {
                        break;
                    }
                    pos += 1;
                }

                let seg = &body[seg_start..pos];
                if seg.is_empty() {
                    explicit_end = Some(ts);
                } else {
                    words.push(LyricWord {
                        start: cur_start,
                        end: 0,
                        text: seg.to_owned(),
                    });
                }
                continue;
            }
        }

        // Leading text before the very first `<mm:ss.xx>` tag.
        let seg_start = pos;
        while pos < bytes.len() {
            if bytes[pos] == b'<'
                && let Some(next_end) = body[pos + 1..].find('>')
                && parse_lrc_timestamp(&body[pos + 1..pos + 1 + next_end]).is_some()
            {
                break;
            }
            pos += 1;
        }
        let seg = &body[seg_start..pos];
        if !seg.is_empty() {
            words.push(LyricWord {
                start: cur_start,
                end: 0,
                text: seg.to_owned(),
            });
        }
    }

    if !found_tag {
        return None;
    }

    Some((words, explicit_end))
}

/// Parses `[start_ms,duration_ms]rest` prefix used by YRC, QRC, and KRC formats.
fn parse_ms_range_prefix(line: &str) -> Option<(i64, i64, &str)> {
    let after_open = line.strip_prefix('[')?;
    let close_idx = after_open.find(']')?;
    let inner = &after_open[..close_idx];
    let (start_s, dur_s) = inner.split_once(',')?;
    let start_ms = parse_u64_digits(start_s.trim())?;
    let dur_ms = parse_u64_digits(dur_s.trim())?;
    let rest = &after_open[close_idx + 1..];
    Some((start_ms, dur_ms, rest))
}

fn parse_word_range_body(start_ms: i64, end_ms: i64, rest: &str) -> ParsedLyricLine {
    // 1. KRC relative word tags: <rel_start,dur,0>word
    if let Some(words) = parse_krc_words(start_ms, rest) {
        let text: String = words.iter().map(|w| w.text.as_str()).collect();
        return ParsedLyricLine {
            start_ms,
            end_ms,
            text,
            translation: None,
            romaji: None,
            words,
        };
    }

    // 2. YRC absolute word tags: (start,dur,0)word
    if let Some(words) = parse_yrc_words(rest) {
        let text: String = words.iter().map(|w| w.text.as_str()).collect();
        return ParsedLyricLine {
            start_ms,
            end_ms,
            text,
            translation: None,
            romaji: None,
            words,
        };
    }

    // 3. QRC absolute word tags: word(start,dur)
    if let Some(words) = parse_qrc_words(rest) {
        let text: String = words.iter().map(|w| w.text.as_str()).collect();
        return ParsedLyricLine {
            start_ms,
            end_ms,
            text,
            translation: None,
            romaji: None,
            words,
        };
    }

    ParsedLyricLine {
        start_ms,
        end_ms,
        text: rest.trim().to_owned(),
        translation: None,
        romaji: None,
        words: Vec::new(),
    }
}

/// Parses YRC tokens of the form `(start_ms,dur_ms[,flag])word`.
fn parse_yrc_words(body: &str) -> Option<Vec<LyricWord>> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with('(') {
        return None;
    }

    let mut words = Vec::new();
    let mut pos = 0;
    let bytes = trimmed.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] != b'(' {
            return None;
        }
        let close = trimmed[pos + 1..].find(')')?;
        let tuple_str = &trimmed[pos + 1..pos + 1 + close];
        let (start, dur) = parse_int_pair(tuple_str)?;
        pos += close + 2;

        // Collect word text until the next `(start,dur...)` tuple or end of string.
        let text_start = pos;
        while pos < bytes.len() {
            if bytes[pos] == b'('
                && let Some(next_close) = trimmed[pos + 1..].find(')')
                && parse_int_pair(&trimmed[pos + 1..pos + 1 + next_close]).is_some()
            {
                break;
            }
            pos += 1;
        }
        let word_text = &trimmed[text_start..pos];
        if !word_text.is_empty() {
            words.push(LyricWord {
                start,
                end: start.saturating_add(dur.max(0)),
                text: word_text.to_owned(),
            });
        }
    }

    (!words.is_empty()).then_some(words)
}

/// Parses QRC tokens of the form `word(start_ms,dur_ms[,flag])`.
fn parse_qrc_words(body: &str) -> Option<Vec<LyricWord>> {
    if !body.contains('(') || body.trim_start().starts_with('(') {
        return None;
    }

    let mut words = Vec::new();
    let mut pos = 0;
    let bytes = body.as_bytes();

    while pos < bytes.len() {
        // Scan text until a `(start,dur[,flag])` tuple.
        let text_start = pos;
        let mut found: Option<(usize, usize, i64, i64)> = None;

        while pos < bytes.len() {
            if bytes[pos] == b'('
                && let Some(rel_close) = body[pos + 1..].find(')')
                && let Some((start, dur)) = parse_int_pair(&body[pos + 1..pos + 1 + rel_close])
            {
                found = Some((pos, pos + rel_close + 2, start, dur));
                break;
            }
            pos += 1;
        }

        let Some((paren_start, after_paren, start, dur)) = found else {
            break;
        };

        let word_text = &body[text_start..paren_start];
        if !word_text.is_empty() {
            words.push(LyricWord {
                start,
                end: start.saturating_add(dur.max(0)),
                text: word_text.to_owned(),
            });
        }
        pos = after_paren;
    }

    (!words.is_empty()).then_some(words)
}

/// Parses KRC tokens of the form `<rel_start_ms,dur_ms[,flag]>word`.
fn parse_krc_words(line_start_ms: i64, body: &str) -> Option<Vec<LyricWord>> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with('<') {
        return None;
    }

    let mut words = Vec::new();
    let mut pos = 0;
    let bytes = trimmed.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] != b'<' {
            return None;
        }
        let close = trimmed[pos + 1..].find('>')?;
        let tuple_str = &trimmed[pos + 1..pos + 1 + close];
        let (rel_start, dur) = parse_int_pair(tuple_str)?;
        pos += close + 2;

        let text_start = pos;
        while pos < bytes.len() {
            if bytes[pos] == b'<'
                && let Some(next_close) = trimmed[pos + 1..].find('>')
                && parse_int_pair(&trimmed[pos + 1..pos + 1 + next_close]).is_some()
            {
                break;
            }
            pos += 1;
        }
        let word_text = &trimmed[text_start..pos];
        if !word_text.is_empty() {
            let start = line_start_ms.saturating_add(rel_start.max(0));
            words.push(LyricWord {
                start,
                end: start.saturating_add(dur.max(0)),
                text: word_text.to_owned(),
            });
        }
    }

    (!words.is_empty()).then_some(words)
}

fn parse_int_pair(tuple_str: &str) -> Option<(i64, i64)> {
    let mut parts = tuple_str.split(',');
    let a = parse_u64_digits(parts.next()?.trim())?;
    let b = parse_u64_digits(parts.next()?.trim())?;
    // Allow optional 3rd integer flag (e.g. `,0`), reject extraneous non-digit parts.
    if let Some(third) = parts.next() {
        parse_u64_digits(third.trim())?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((a, b))
}

fn parse_yrc_json_line(line: &str) -> Option<ParsedLyricLine> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;
    let start_ms = val.get("t")?.as_i64()?;
    let parts = val.get("c")?.as_array()?;
    let mut text = String::new();
    for part in parts {
        if let Some(tx) = part.get("tx").and_then(|v| v.as_str()) {
            text.push_str(tx);
        }
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    Some(ParsedLyricLine {
        start_ms,
        end_ms: 0,
        text,
        translation: None,
        romaji: None,
        words: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_lrc_multi_timestamp_metadata_and_offset() {
        let input = "\
[ti:Midnight Glass]
[ar:Luna Echo]
[al:Neon Harbor]
[offset:500]
[00:10.00]First line
[00:15.50][00:30.25]Chorus line
[00:20.00]
[00:35.00]Final line
";
        let parsed = parse_lrc(input);
        assert!(parsed.synced);
        assert!(!parsed.word_synced);
        assert_eq!(parsed.meta.title.as_deref(), Some("Midnight Glass"));
        assert_eq!(parsed.meta.artist.as_deref(), Some("Luna Echo"));
        assert_eq!(parsed.meta.album.as_deref(), Some("Neon Harbor"));
        assert_eq!(parsed.meta.offset_ms, 500);
        assert_eq!(parsed.lines.len(), 4);

        // Positive offset +500 shifts timestamps 500ms earlier
        assert_eq!(parsed.lines[0].start_ms, 9_500);
        assert_eq!(parsed.lines[0].end_ms, 15_000);
        assert_eq!(parsed.lines[0].text, "First line");

        // Multi-timestamp chorus expanded and sorted; break marker caps end_ms at 19_500
        assert_eq!(parsed.lines[1].start_ms, 15_000);
        assert_eq!(parsed.lines[1].end_ms, 19_500);
        assert_eq!(parsed.lines[1].text, "Chorus line");

        assert_eq!(parsed.lines[2].start_ms, 29_750);
        assert_eq!(parsed.lines[2].end_ms, 34_500);
        assert_eq!(parsed.lines[2].text, "Chorus line");

        assert_eq!(parsed.lines[3].start_ms, 34_500);
        assert_eq!(parsed.lines[3].text, "Final line");

        // Binary-search active line lookup
        assert_eq!(parsed.active_line_index(9_499), None);
        assert_eq!(parsed.active_line_index(9_500), Some(0));
        assert_eq!(parsed.active_line_index(15_000), Some(1));
        assert_eq!(parsed.active_line_index(30_000), Some(2));
        assert_eq!(parsed.active_line_index(40_000), Some(3));
    }

    #[test]
    fn parses_negative_offset_and_colon_centiseconds() {
        let input = "\
[offset:-250]
[01:02:50]Colon format
[00:05.125]Millisecond format
";
        let parsed = parse_lrc(input);
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].start_ms, 5_375);
        assert_eq!(parsed.lines[1].start_ms, 62_750);
    }

    #[test]
    fn parses_word_synced_enhanced_lrc() {
        let input = "[00:12.00]<00:12.00>Night <00:12.50>falls <00:13.20>softly<00:14.00>\n";
        let parsed = parse_lrc(input);
        assert!(parsed.synced);
        assert!(parsed.word_synced);
        assert_eq!(parsed.lines.len(), 1);

        let line = &parsed.lines[0];
        assert_eq!(line.text, "Night falls softly");
        assert_eq!(line.start_ms, 12_000);
        assert_eq!(line.end_ms, 14_000);
        assert_eq!(line.words.len(), 3);
        assert_eq!(
            line.words[0],
            LyricWord {
                start: 12_000,
                end: 12_500,
                text: "Night ".into()
            }
        );
        assert_eq!(
            line.words[1],
            LyricWord {
                start: 12_500,
                end: 13_200,
                text: "falls ".into()
            }
        );
        assert_eq!(
            line.words[2],
            LyricWord {
                start: 13_200,
                end: 14_000,
                text: "softly".into()
            }
        );

        let pos = parsed.active_position(12_850).expect("active position");
        assert_eq!(pos.line_index, 0);
        assert_eq!(pos.word_index, Some(1));
        assert!((pos.word_progress - 0.5).abs() < 0.01);
    }

    #[test]
    fn parses_yrc_qrc_and_krc_word_timestamps() {
        let yrc = "\
{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"南橙\"}]}
[12000,2400](12000,600,0)星(12600,600,0)光(13200,1200,0)落
";
        let parsed_yrc = parse_lrc(yrc);
        assert!(parsed_yrc.synced);
        assert!(parsed_yrc.word_synced);
        assert_eq!(parsed_yrc.lines.len(), 2);
        assert_eq!(parsed_yrc.lines[0].text, "作词: 南橙");
        assert_eq!(parsed_yrc.lines[1].text, "星光落");
        assert_eq!(parsed_yrc.lines[1].words.len(), 3);
        assert_eq!(parsed_yrc.lines[1].words[1].start, 12_600);
        assert_eq!(parsed_yrc.lines[1].words[1].end, 13_200);

        let qrc = "[15000,2000]夜(15000,500)色(15500,700)温(16200,800)柔(17000,0)\n";
        let parsed_qrc = parse_lrc(qrc);
        assert!(parsed_qrc.word_synced);
        assert_eq!(parsed_qrc.lines[0].text, "夜色温柔");
        assert_eq!(parsed_qrc.lines[0].words.len(), 4);
        assert_eq!(parsed_qrc.lines[0].words[2].text, "温");
        assert_eq!(parsed_qrc.lines[0].words[2].start, 16_200);

        let krc = "[20000,1500]<0,500,0>风<500,500,0>起<1000,500,0>时\n";
        let parsed_krc = parse_lrc(krc);
        assert!(parsed_krc.word_synced);
        assert_eq!(parsed_krc.lines[0].text, "风起时");
        assert_eq!(parsed_krc.lines[0].words[1].start, 20_500);
        assert_eq!(parsed_krc.lines[0].words[1].end, 21_000);
    }

    #[test]
    fn aligns_translation_and_romaji_by_timestamp_proximity() {
        let primary = "\
[00:10.00]夜空に光る星
[00:18.00]君の声が響く
";
        let translation = "\
[00:10.18]Stars shining in the night sky
[00:17.85]Your voice echoes through the night
";
        let romaji = "\
[00:10.05]Yozora ni hikaru hoshi
[00:18.10]Kimi no koe ga hibiku
";

        let parsed = parse_lyrics_with_romaji(primary, Some(translation), Some(romaji));
        assert_eq!(
            parsed.lines[0].translation.as_deref(),
            Some("Stars shining in the night sky")
        );
        assert_eq!(
            parsed.lines[0].romaji.as_deref(),
            Some("Yozora ni hikaru hoshi")
        );
        assert_eq!(
            parsed.lines[1].translation.as_deref(),
            Some("Your voice echoes through the night")
        );
        assert_eq!(
            parsed.lines[1].romaji.as_deref(),
            Some("Kimi no koe ga hibiku")
        );
    }

    #[test]
    fn merges_inline_bilingual_same_timestamp_lines() {
        let lrc = "\
[00:08.00]City lights blur in the rain
[00:08.00]城市灯火在雨中模糊
";
        let parsed = parse_lrc(lrc);
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].text, "City lights blur in the rain");
        assert_eq!(
            parsed.lines[0].translation.as_deref(),
            Some("城市灯火在雨中模糊")
        );
    }

    #[test]
    fn plain_text_fallback_marks_unsynced() {
        let plain = "Line one without timestamps\nLine two without timestamps\n";
        let parsed = parse_lrc(plain);
        assert!(!parsed.synced);
        assert!(!parsed.word_synced);
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].text, "Line one without timestamps");
        assert_eq!(parsed.active_line_index(10_000), None);
    }

    #[test]
    fn model_lyrics_helpers_work_directly() {
        let raw = Lyrics {
            lrc: "[00:05.00]Hello\n[00:10.00]World\n".into(),
            translation: "[00:05.10]你好\n[00:10.00]世界\n".into(),
            lines: Vec::new(),
        };
        let parsed = parse_lyrics(&raw);
        let model = parsed.to_model_lyrics();
        assert_eq!(model.lines.len(), 2);
        assert_eq!(model.lines[0].translation, "你好");
        assert_eq!(find_active_line(&model.lines, 7_500), Some(0));
        assert!((line_progress(&model.lines[0], 7_500) - 0.5).abs() < 0.01);
    }
}
