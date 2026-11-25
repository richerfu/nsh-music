use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use nsh_music_core::source::{PluginCompatLevel, SourceCompat};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, ObjectPropertyKind, PropertyKey,
    Statement,
};
use oxc::parser::Parser;
use oxc::span::SourceType;
use serde::{Deserialize, Serialize};

use crate::manifest::{ManifestError, PluginManifest};

/// High-level cross-platform compatibility classification for a music plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompatTier {
    /// Works seamlessly across Desktop, iOS, Android, and HarmonyOS (including virtualized `ffmpeg`).
    #[default]
    Full,
    /// Works on mobile with caveats (e.g. `host.webSignIn` HttpOnly cookie manual fallback).
    Partial,
    /// Uses non-ffmpeg external binaries via `host.spawn`; restricted on mobile sandboxes.
    MigrationRequired,
}

impl CompatTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
            Self::MigrationRequired => "migrationRequired",
        }
    }
}

/// One diagnostic finding produced by static plugin compatibility analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatFinding {
    pub code: String,
    pub severity: CompatTier,
    pub title: String,
    pub detail: String,
    pub migration_hint: String,
}

/// Complete static compatibility report for a plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompatReport {
    pub tier: CompatTier,
    pub uses_ffmpeg: bool,
    pub uses_ytdlp: bool,
    pub uses_web_sign_in: bool,
    pub uses_external_spawn: bool,
    pub external_commands: Vec<String>,
    pub network_hosts: Vec<String>,
    pub detected_exports: Vec<String>,
    pub findings: Vec<CompatFinding>,
    pub summary: String,
}

impl CompatReport {
    /// Converts this report into the frozen `nsh_music_core::source::SourceCompat` contract.
    pub fn to_source_compat(&self, granted: bool) -> SourceCompat {
        let level = if self.tier == CompatTier::MigrationRequired || self.uses_external_spawn {
            PluginCompatLevel::DesktopProcessLimited
        } else if self.uses_ffmpeg || self.uses_ytdlp {
            PluginCompatLevel::VirtualFfmpeg
        } else {
            PluginCompatLevel::FullNative
        };

        let mut process_commands = Vec::new();
        if self.uses_ffmpeg {
            process_commands.push("ffmpeg".to_owned());
        }
        if self.uses_ytdlp {
            process_commands.push("yt-dlp".to_owned());
        }
        for cmd in &self.external_commands {
            if !process_commands.iter().any(|c| c == cmd) {
                process_commands.push(cmd.clone());
            }
        }

        SourceCompat {
            level,
            network_hosts: self.network_hosts.clone(),
            process_commands,
            granted,
            summary: self.summary.clone(),
        }
    }
}

/// Returns `true` if `cmd` refers to an `ffmpeg` binary (any path or `.exe` suffix).
pub fn is_ffmpeg_binary_name(cmd: &str) -> bool {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    base == "ffmpeg" || base == "ffmpeg.exe"
}

/// Returns `true` if `cmd` refers to a `yt-dlp` binary (any path or `.exe` suffix).
pub fn is_ytdlp_binary_name(cmd: &str) -> bool {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    base == "yt-dlp" || base == "yt-dlp.exe"
}

fn canonical_command_name(cmd: &str) -> String {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base)
        .to_owned()
}

/// Analyzes a plugin directory (`plugin.json` + JS/TS source files) for cross-platform compatibility.
pub fn analyze_plugin_dir(plugin_dir: &Path) -> Result<CompatReport, ManifestError> {
    let manifest = PluginManifest::from_dir(plugin_dir)?;
    let entry_path = manifest.resolve_entry_path(plugin_dir)?;
    let entry_source = fs::read_to_string(&entry_path).map_err(|source| ManifestError::Io {
        path: entry_path.clone(),
        source,
    })?;

    // Collect additional local JS/TS sources so helper modules using host.spawn/webSignIn are scanned too.
    let mut combined_aux = String::new();
    collect_js_sources(plugin_dir, &entry_path, &mut combined_aux);

    let filename = entry_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("index.js");

    Ok(analyze_sources(
        &manifest,
        &entry_source,
        filename,
        &combined_aux,
    ))
}

/// Analyzes a manifest and single JS/TS source string.
pub fn analyze_source(manifest: &PluginManifest, source: &str, filename: &str) -> CompatReport {
    analyze_sources(manifest, source, filename, "")
}

fn analyze_sources(
    manifest: &PluginManifest,
    entry_source: &str,
    entry_filename: &str,
    aux_sources: &str,
) -> CompatReport {
    let mut report = CompatReport::default();
    let mut hosts: BTreeSet<String> = manifest.all_network_hosts().into_iter().collect();
    let mut external_cmds: BTreeSet<String> = BTreeSet::new();

    // 1. Inspect manifest-declared commands (recognizing `ffmpeg` and `yt-dlp` as mobile-virtualized).
    let manifest_procs = manifest.all_process_commands();
    let manifest_all_virtualized = !manifest_procs.is_empty()
        && manifest_procs
            .iter()
            .all(|c| is_ffmpeg_binary_name(c) || is_ytdlp_binary_name(c));
    for cmd in &manifest_procs {
        if is_ffmpeg_binary_name(cmd) {
            report.uses_ffmpeg = true;
        } else if is_ytdlp_binary_name(cmd) {
            report.uses_ytdlp = true;
        } else {
            report.uses_external_spawn = true;
            external_cmds.insert(canonical_command_name(cmd));
        }
    }

    // 2. Parse entry source with oxc to extract ESM exports accurately.
    report.detected_exports = extract_exports_with_oxc(entry_source, entry_filename);

    // 3. Strip JS/TS comments while preserving string literals, then scan host.* call sites.
    let mut all_code = strip_comments_preserving_strings(entry_source);
    if !aux_sources.is_empty() {
        all_code.push('\n');
        all_code.push_str(&strip_comments_preserving_strings(aux_sources));
    }

    let checks_can_spawn = all_code.contains("canSpawnExternal") || all_code.contains("isMobile");

    // Detect `host.spawn(...)` invocations.
    for arg in find_first_call_args(&all_code, "host.spawn") {
        match arg {
            CallArg::Literal(cmd) => {
                if is_ffmpeg_binary_name(&cmd) {
                    report.uses_ffmpeg = true;
                } else if is_ytdlp_binary_name(&cmd) {
                    report.uses_ytdlp = true;
                } else {
                    report.uses_external_spawn = true;
                    external_cmds.insert(canonical_command_name(&cmd));
                }
            }
            CallArg::Dynamic(expr) => {
                let lower = expr.to_ascii_lowercase();
                if lower.contains("ffmpeg") {
                    report.uses_ffmpeg = true;
                } else if lower.contains("ytdlp")
                    || lower.contains("yt_dlp")
                    || lower.contains("yt-dlp")
                {
                    report.uses_ytdlp = true;
                } else if manifest_all_virtualized {
                    // Manifest declares only mobile-virtualized binaries (`ffmpeg` / `yt-dlp`);
                    // dynamic binary probe loops resolve through the mobile virtualization layer.
                } else if !checks_can_spawn && external_cmds.is_empty() {
                    report.uses_external_spawn = true;
                    external_cmds.insert(format!("<dynamic:{}>", truncate_expr(&expr, 24)));
                }
            }
        }
    }

    // Detect `host.webSignIn(...)` invocations.
    let web_signin_calls = find_call_snippets(&all_code, "host.webSignIn");
    if !web_signin_calls.is_empty() {
        report.uses_web_sign_in = true;
    }

    // Detect literal URL hosts passed to `host.fetch(...)`.
    for arg in find_first_call_args(&all_code, "host.fetch") {
        if let CallArg::Literal(raw_url) = arg
            && let Ok(parsed) = url::Url::parse(&raw_url)
            && let Some(h) = parsed.host_str()
        {
            hosts.insert(h.to_owned());
        }
    }

    // 4. Assemble findings and compute final tier.
    if report.uses_ffmpeg {
        report.findings.push(CompatFinding {
            code: "VIRTUAL_FFMPEG_INTERCEPTED".to_owned(),
            severity: CompatTier::Full,
            title: "Embedded Virtual FFmpeg Active".to_owned(),
            detail: "Calls to `host.spawn(\"ffmpeg\", ...)` are transparently intercepted in-process by `VirtualFfmpegShim` on Desktop, iOS, Android, and HarmonyOS.".to_owned(),
            migration_hint: "No changes required; unmodified ffmpeg probe and loopback relay flows work out of the box.".to_owned(),
        });
    }

    if report.uses_ytdlp {
        report.findings.push(CompatFinding {
            code: "VIRTUAL_YTDLP_INTERCEPTED".to_owned(),
            severity: CompatTier::Full,
            title: "Embedded Virtual yt-dlp Active".to_owned(),
            detail: "Calls to `host.spawn(\"yt-dlp\", ...)` are transparently intercepted in-process by `VirtualYtdlpShim` on iOS, Android, HarmonyOS, and desktops without external `yt-dlp`.".to_owned(),
            migration_hint: "No changes required; unmodified `yt-dlp` audio resolution flows run in-process without Python or external binaries.".to_owned(),
        });
    }

    if report.uses_web_sign_in {
        let has_manual_cookie = manifest
            .settings
            .iter()
            .any(|s| s.key.eq_ignore_ascii_case("cookie"))
            || web_signin_calls
                .iter()
                .any(|s| s.contains("allowManualCookie"));
        let has_qr = report
            .detected_exports
            .iter()
            .any(|e| e == "getQrSignIn" || e == "pollQrSignIn");
        if !has_qr {
            report.tier = report.tier.max(CompatTier::Partial);
            report.findings.push(CompatFinding {
                code: "WEB_SIGN_IN_HTTPONLY".to_owned(),
                severity: CompatTier::Partial,
                title: "WebView Sign-In HttpOnly Cookie Caveat".to_owned(),
                detail: if has_manual_cookie {
                    "Uses `host.webSignIn(...)` with manual cookie fallback configured; mobile WebViews may restrict JS access to HttpOnly cookies.".to_owned()
                } else {
                    "Uses `host.webSignIn(...)`; on iOS, Android, and HarmonyOS, HttpOnly session cookies cannot always be read via `document.cookie`.".to_owned()
                },
                migration_hint: "Add `allowManualCookie: true` in `host.webSignIn(opts)` and a `cookie` setting in `plugin.json` `configSchema`, or implement `getQrSignIn` / `pollQrSignIn` for full mobile compatibility.".to_owned(),
            });
        }
    }

    if report.uses_external_spawn {
        report.tier = CompatTier::MigrationRequired;
        let cmd_list: Vec<String> = external_cmds.iter().cloned().collect();
        report.findings.push(CompatFinding {
            code: "EXTERNAL_SPAWN_MOBILE_SANDBOX".to_owned(),
            severity: CompatTier::MigrationRequired,
            title: "External CLI Binary Blocked on Mobile Sandboxes".to_owned(),
            detail: format!(
                "Invokes external binary command(s) [{}] via `host.spawn`, which is disallowed on iOS, Android, and HarmonyOS sandboxes.",
                cmd_list.join(", ")
            ),
            migration_hint: "Guard desktop-only spawns with `host.capabilities.canSpawnExternal`, replace external signing/fetching with `host.fetch` + `host.crypto`, or delegate audio relay to `host.spawn(\"ffmpeg\", ...)`.".to_owned(),
        });
    }

    report.external_commands = external_cmds.into_iter().collect();
    report.network_hosts = hosts.into_iter().collect();
    report.summary = build_summary(&report);
    report
}

fn build_summary(report: &CompatReport) -> String {
    match report.tier {
        CompatTier::MigrationRequired => format!(
            "Desktop only for external binary [{}]; mobile requires migration (see PLUGIN_COMPAT.md).",
            report.external_commands.join(", ")
        ),
        CompatTier::Partial => match (report.uses_ffmpeg, report.uses_ytdlp) {
            (true, true) => {
                "Full audio via Virtual FFmpeg & Virtual yt-dlp; webSignIn may require manual cookie fallback on mobile."
                    .to_owned()
            }
            (true, false) => {
                "Full audio via Virtual FFmpeg; webSignIn may require manual cookie fallback on mobile."
                    .to_owned()
            }
            (false, true) => {
                "Full audio via Virtual yt-dlp; webSignIn may require manual cookie fallback on mobile."
                    .to_owned()
            }
            (false, false) => {
                "Works on all platforms; webSignIn may require manual cookie fallback for HttpOnly cookies on mobile."
                    .to_owned()
            }
        },
        CompatTier::Full => match (report.uses_ffmpeg, report.uses_ytdlp) {
            (true, true) => {
                "Full cross-platform compatibility (ffmpeg & yt-dlp handled in-process by VirtualFfmpegShim and VirtualYtdlpShim)."
                    .to_owned()
            }
            (true, false) => {
                "Full cross-platform compatibility (ffmpeg handled in-process by VirtualFfmpegShim)."
                    .to_owned()
            }
            (false, true) => {
                "Full cross-platform compatibility (yt-dlp handled in-process by VirtualYtdlpShim)."
                    .to_owned()
            }
            (false, false) => {
                "Full native compatibility across Desktop, iOS, Android, and HarmonyOS.".to_owned()
            }
        },
    }
}

fn extract_exports_with_oxc(source: &str, filename: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(filename).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    let mut exports = BTreeSet::new();

    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportDeclaration(decl) => match &decl.declaration {
                Declaration::FunctionDeclaration(f) => {
                    if let Some(id) = &f.id {
                        exports.insert(id.name.as_str().to_owned());
                    }
                }
                Declaration::ClassDeclaration(c) => {
                    if let Some(id) = &c.id {
                        exports.insert(id.name.as_str().to_owned());
                    }
                }
                Declaration::VariableDeclaration(v) => {
                    for declarator in &v.declarations {
                        collect_binding_names(&declarator.id, &mut exports);
                    }
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(decl) => {
                if !decl.export_kind.is_type() {
                    for spec in &decl.specifiers {
                        if !spec.export_kind.is_type() {
                            exports.insert(spec.exported.name().as_str().to_owned());
                        }
                    }
                }
            }
            Statement::ExportFromDeclaration(decl) => {
                if !decl.export_kind.is_type() {
                    for spec in &decl.specifiers {
                        if !spec.export_kind.is_type() {
                            exports.insert(spec.exported.name().as_str().to_owned());
                        }
                    }
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                exports.insert("default".to_owned());
                if let ExportDefaultDeclarationKind::ObjectExpression(obj) = &decl.declaration {
                    for prop in &obj.properties {
                        if let ObjectPropertyKind::ObjectProperty(p) = prop {
                            match &p.key {
                                PropertyKey::StaticIdentifier(id) => {
                                    exports.insert(id.name.as_str().to_owned());
                                }
                                PropertyKey::StringLiteral(s) => {
                                    exports.insert(s.value.as_str().to_owned());
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    exports.into_iter().collect()
}

fn collect_binding_names(pattern: &BindingPattern<'_>, out: &mut BTreeSet<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => {
            out.insert(id.name.as_str().to_owned());
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_names(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_binding_names(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => {
            collect_binding_names(&assign.left, out);
        }
    }
}

fn collect_js_sources(dir: &Path, skip: &Path, out: &mut String) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with('.')
                && !matches!(
                    name,
                    "node_modules" | "locales" | "tests" | "test" | "__tests__" | "dist" | "build"
                )
            {
                collect_js_sources(&path, skip, out);
            }
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "js" | "mjs" | "cjs" | "ts" | "mts" | "cts")
                && let Ok(content) = fs::read_to_string(&path)
            {
                out.push('\n');
                out.push_str(&content);
            }
        }
    }
}

enum CallArg {
    Literal(String),
    Dynamic(String),
}

fn find_first_call_args(code: &str, callee: &str) -> Vec<CallArg> {
    let mut results = Vec::new();
    let bytes = code.as_bytes();
    let mut pos = 0;

    while let Some(idx) = code[pos..].find(callee) {
        let abs = pos + idx;
        pos = abs + callee.len();

        // Ensure word boundary before callee.
        if abs > 0 {
            let prev = bytes[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' {
                continue;
            }
        }

        // Skip whitespace and optional `?.` before `(`.
        let mut cursor = pos;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }

        let ch = bytes[cursor];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            if let Some((lit, _end)) = read_string_literal(code, cursor) {
                if ch == b'`' && lit.contains("${") {
                    results.push(CallArg::Dynamic(lit));
                } else {
                    results.push(CallArg::Literal(lit));
                }
            }
        } else {
            // Read expression up to `,` or `)` at depth 0.
            let start = cursor;
            let mut depth = 0usize;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => break,
                    _ => {}
                }
                cursor += 1;
            }
            let expr = code[start..cursor].trim().to_owned();
            if !expr.is_empty() {
                results.push(CallArg::Dynamic(expr));
            }
        }
    }

    results
}

fn find_call_snippets(code: &str, callee: &str) -> Vec<String> {
    let mut results = Vec::new();
    let bytes = code.as_bytes();
    let mut pos = 0;
    while let Some(idx) = code[pos..].find(callee) {
        let abs = pos + idx;
        pos = abs + callee.len();
        let mut cursor = pos;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            continue;
        }
        let start = cursor;
        let mut depth = 0usize;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'(' => depth += 1,
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cursor += 1;
                        break;
                    }
                }
                b'"' | b'\'' | b'`' => {
                    if let Some((_, end)) = read_string_literal(code, cursor) {
                        cursor = end;
                        continue;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        results.push(code[start..cursor.min(code.len())].to_owned());
    }
    results
}

fn read_string_literal(code: &str, start: usize) -> Option<(String, usize)> {
    let bytes = code.as_bytes();
    let quote = *bytes.get(start)?;
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if b == quote {
            return Some((out, i + 1));
        }
        // Safe char slicing for UTF-8.
        let ch = code[i..].chars().next()?;
        out.push(ch);
        i += ch.len_utf8();
    }
    None
}

fn strip_comments_preserving_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            out.push(quote as char);
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' && i + 1 < bytes.len() {
                    out.push('\\');
                    let next_ch = src[i + 1..].chars().next().unwrap_or(' ');
                    out.push(next_ch);
                    i += 1 + next_ch.len_utf8();
                    continue;
                }
                let ch = src[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
                if c == quote {
                    break;
                }
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn truncate_expr(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_virtual_ffmpeg_as_full() {
        let manifest = PluginManifest::parse_json(
            r#"{"id":"ytm","name":"YouTube Music","permissions":{"process":["ffmpeg"]}}"#,
        )
        .unwrap();
        let src = r#"
            export async function getStreamUrl(id: string) {
                const p = host.spawn("ffmpeg", ["-version"]);
                return { url: "http://127.0.0.1:9000/" };
            }
        "#;
        let report = analyze_source(&manifest, src, "index.ts");
        assert_eq!(report.tier, CompatTier::Full);
        assert!(report.uses_ffmpeg);
        assert!(!report.uses_external_spawn);
        assert!(report.detected_exports.contains(&"getStreamUrl".to_owned()));

        let sc = report.to_source_compat(true);
        assert_eq!(sc.level, PluginCompatLevel::VirtualFfmpeg);
        assert!(sc.process_commands.contains(&"ffmpeg".to_owned()));
    }

    #[test]
    fn classifies_external_cli_as_migration_required() {
        let manifest = PluginManifest::parse_json(r#"{"id":"cli-src"}"#).unwrap();
        let src = r#"
            // host.spawn("ignored-in-comment")
            export default {
                async getStreamUrl(id) {
                    return host.spawn("python3", ["fetch.py", id]);
                }
            };
        "#;
        let report = analyze_source(&manifest, src, "index.js");
        assert_eq!(report.tier, CompatTier::MigrationRequired);
        assert!(report.uses_external_spawn);
        assert_eq!(report.external_commands, vec!["python3"]);
        assert!(report.detected_exports.contains(&"getStreamUrl".to_owned()));

        let sc = report.to_source_compat(true);
        assert_eq!(sc.level, PluginCompatLevel::DesktopProcessLimited);
    }

    #[test]
    fn classifies_virtual_ytdlp_as_full() {
        let manifest = PluginManifest::parse_json(
            r#"{"id":"yt-only","name":"YT Only","permissions":{"process":["yt-dlp"]}}"#,
        )
        .unwrap();
        let src = r#"
            export async function getStreamUrl(id) {
                return host.spawn("yt-dlp", ["-g", id]);
            }
        "#;
        let report = analyze_source(&manifest, src, "index.js");
        assert_eq!(report.tier, CompatTier::Full);
        assert!(report.uses_ytdlp);
        assert!(!report.uses_external_spawn);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "VIRTUAL_YTDLP_INTERCEPTED")
        );
        let sc = report.to_source_compat(true);
        assert_eq!(sc.level, PluginCompatLevel::VirtualFfmpeg);
        assert_eq!(sc.process_commands, vec!["yt-dlp"]);
    }

    #[test]
    fn classifies_web_sign_in_httponly_as_partial() {
        let manifest = PluginManifest::parse_json(r#"{"id":"web-auth"}"#).unwrap();
        let src = r#"
            export async function signIn() {
                return await host.webSignIn({ url: "https://music.example.com/login" });
            }
        "#;
        let report = analyze_source(&manifest, src, "index.ts");
        assert_eq!(report.tier, CompatTier::Partial);
        assert!(report.uses_web_sign_in);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "WEB_SIGN_IN_HTTPONLY" && !f.migration_hint.is_empty())
        );
    }

    #[test]
    fn classifies_dynamic_ffmpeg_path_variables_as_virtual_ffmpeg() {
        let manifest = PluginManifest::parse_json(
            r#"{
                "id": "bilibili",
                "name": "Bilibili",
                "minAppVersion": "0.1.21",
                "permissions": {
                    "network": ["api.bilibili.com"],
                    "process": ["ffmpeg", "/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.min_host_version, "0.1.21");
        let src = r#"
            for (const path of ["ffmpeg", "/opt/homebrew/bin/ffmpeg"]) {
                const proc = host.spawn(path, ["-version"]);
            }
            export { getAlbum as album, getArtist as artist };
            function getAlbum() {}
            function getArtist() {}
        "#;
        let report = analyze_source(&manifest, src, "index.js");
        assert_eq!(report.tier, CompatTier::Full);
        assert!(report.uses_ffmpeg);
        assert!(!report.uses_external_spawn);
        assert!(report.external_commands.is_empty());
        assert!(report.detected_exports.contains(&"album".to_owned()));
        assert!(report.detected_exports.contains(&"artist".to_owned()));

        let sc = report.to_source_compat(true);
        assert_eq!(sc.level, PluginCompatLevel::VirtualFfmpeg);
        assert_eq!(sc.process_commands, vec!["ffmpeg"]);
    }

    #[test]
    fn classifies_youtube_music_manifest_without_dynamic_placeholders() {
        let manifest = PluginManifest::parse_json(
            r#"{
                "id": "youtube-music",
                "name": "YouTube Music",
                "version": "0.1.1",
                "minAppVersion": "0.1.8",
                "main": "index.js",
                "permissions": {
                    "network": ["music.youtube.com", "s.youtube.com", "api.betterlyrics.org"],
                    "process": [
                        "ffmpeg",
                        "yt-dlp",
                        "/opt/homebrew/bin/ffmpeg",
                        "/opt/homebrew/bin/yt-dlp",
                        "/usr/local/bin/ffmpeg",
                        "/usr/local/bin/yt-dlp"
                    ]
                }
            }"#,
        )
        .unwrap();
        let src = r#"
            async function program(name, flag) {
                for (const path of [name, `/opt/homebrew/bin/${name}`]) {
                    const proc = host.spawn(path, [flag]);
                }
            }
            export async function signIn() {
                return await host.webSignIn({
                    url: "https://accounts.google.com/ServiceLogin",
                    cookiesOf: "https://music.youtube.com",
                    until: ["SAPISID", "__Secure-3PSID"],
                });
            }
            export async function stream() {
                host.spawn(ffmpeg, ["-i", url, "-c:a", "libmp3lame", "-f", "mp3", "-listen", "1", out]);
                host.spawn(ytdlp, ["--print", "url"]);
            }
        "#;
        let report = analyze_source(&manifest, src, "index.js");
        assert_eq!(report.tier, CompatTier::Partial);
        assert!(report.uses_ffmpeg);
        assert!(report.uses_ytdlp);
        assert!(report.uses_web_sign_in);
        assert!(!report.uses_external_spawn);
        assert!(report.external_commands.is_empty());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "VIRTUAL_FFMPEG_INTERCEPTED")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "VIRTUAL_YTDLP_INTERCEPTED")
        );
        let sc = report.to_source_compat(true);
        assert_eq!(sc.level, PluginCompatLevel::VirtualFfmpeg);
        assert_eq!(sc.process_commands, vec!["ffmpeg", "yt-dlp"]);
        assert!(!sc.process_commands.iter().any(|c| c.contains("dynamic")));
    }
}
