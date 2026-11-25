//! Artwork resolution, authenticated disk caching, offline fallback SVG
//! generation, and muted dominant accent extraction for Now Playing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use nsh_core::prelude::*;
use sha2::{Digest, Sha256};

use crate::ui::theme;

/// Resolved artwork entry ready for UI rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedArtwork {
    /// Local `file://...` URI (or pass-through URI) suitable for `image(...)`.
    pub uri: SharedString,
    /// Muted dominant accent color for the Now Playing ambient backdrop.
    pub accent: Hsla,
}

/// Resolves `{size}` artwork templates, downloads remote covers using plugin
/// `requestHeaders` into a persistent local disk cache, generates tasteful
/// fallback artwork for demo/offline tracks, and extracts a muted dominant
/// accent [`Hsla`].
#[derive(Clone)]
pub struct ArtworkLoader {
    cache_dir: PathBuf,
    client: reqwest::Client,
    mem: Arc<RwLock<HashMap<String, ResolvedArtwork>>>,
}

impl ArtworkLoader {
    /// Creates an [`ArtworkLoader`] backed by `cache_dir`.
    pub fn new(cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            cache_dir,
            client,
            mem: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates an [`ArtworkLoader`] in the platform's default cache/data path.
    pub fn default_location() -> Self {
        let base = nsh_core::element::data_dir()
            .unwrap_or_else(|| std::env::temp_dir().join("nsh-music"));
        Self::new(base.join("artwork-cache"))
    }

    /// Expands `{size}`, `{w}`, `{h}` placeholders in an artwork template URL.
    pub fn resolve_template(url: &str, size_px: u32) -> String {
        let s = size_px.to_string();
        url.replace("{size}", &s)
            .replace("{w}", &s)
            .replace("{h}", &s)
    }

    /// Returns the local cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Synchronous in-memory cache lookup.
    pub fn cached(&self, raw_url: &str, size_px: u32) -> Option<ResolvedArtwork> {
        let key = cache_key(raw_url, size_px);
        self.mem.read().ok().and_then(|m| m.get(&key).cloned())
    }

    /// Resolves artwork asynchronously (safe to run inside `tokio` / `background`).
    ///
    /// - Empty URLs produce a clean local SVG fallback cover in cache.
    /// - `file://` URLs or existing filesystem paths are reused directly.
    /// - `http://` / `https://` URLs are fetched with `headers` and cached to disk.
    pub async fn load(
        &self,
        raw_url: &str,
        size_px: u32,
        seed: &str,
        headers: &[(String, String)],
    ) -> ResolvedArtwork {
        let expanded = Self::resolve_template(raw_url.trim(), size_px);
        let cache_id = cache_key(if expanded.is_empty() { seed } else { &expanded }, size_px);

        if let Some(hit) = self.mem.read().ok().and_then(|m| m.get(&cache_id).cloned()) {
            return hit;
        }

        // Local file URI or direct path
        if let Some(local) = resolve_local_uri(&expanded) {
            let accent = extract_accent_from_path(&local, seed);
            let res = ResolvedArtwork {
                uri: path_to_file_uri(&local).into(),
                accent,
            };
            self.remember(cache_id, res.clone());
            return res;
        }

        // Remote HTTP(S) image with plugin requestHeaders
        if expanded.starts_with("http://") || expanded.starts_with("https://") {
            if let Some(existing) = self.find_cached_file(&cache_id) {
                let accent = extract_accent_from_path(&existing, &expanded);
                let res = ResolvedArtwork {
                    uri: path_to_file_uri(&existing).into(),
                    accent,
                };
                self.remember(cache_id, res.clone());
                return res;
            }

            if let Ok(resolved) = self.fetch_and_cache(&expanded, &cache_id, headers).await {
                self.remember(cache_id, resolved.clone());
                return resolved;
            }
        }

        // Fallback generated SVG artwork when remote URL is empty or fails to load
        let fallback = self.generate_fallback(&cache_id, if seed.is_empty() { &expanded } else { seed });
        self.remember(cache_id, fallback.clone());
        fallback
    }

    /// Extracts a muted dominant accent [`Hsla`] synchronously from a seed or URL.
    pub fn accent_for_seed(seed: &str) -> Hsla {
        let (start, _) = theme::fallback_cover_gradient(seed);
        mute_for_backdrop(start)
    }

    /// Generates and caches a clean SVG fallback cover for missing/offline artwork.
    pub fn generate_fallback(&self, key: &str, seed: &str) -> ResolvedArtwork {
        let (hsla_a, _) = theme::fallback_cover_gradient(seed);
        let path = self.cache_dir.join(format!("fallback-{key}.svg"));
        if !path.exists() {
            let svg = theme::fallback_cover_svg(seed);
            let _ = std::fs::write(&path, svg.as_bytes());
        }
        ResolvedArtwork {
            uri: path_to_file_uri(&path).into(),
            accent: mute_for_backdrop(hsla_a),
        }
    }

    async fn fetch_and_cache(
        &self,
        url: &str,
        cache_id: &str,
        headers: &[(String, String)],
    ) -> Result<ResolvedArtwork, String> {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err("empty artwork payload".to_owned());
        }

        let ext = guess_extension(&content_type, &bytes);
        let dest = self.cache_dir.join(format!("{cache_id}.{ext}"));
        let tmp = self.cache_dir.join(format!("{cache_id}.{ext}.part"));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &dest);
        }

        let accent = extract_accent_from_bytes(&bytes, url);
        Ok(ResolvedArtwork {
            uri: path_to_file_uri(&dest).into(),
            accent,
        })
    }

    fn find_cached_file(&self, cache_id: &str) -> Option<PathBuf> {
        for ext in ["jpg", "png", "webp", "svg", "bin"] {
            let candidate = self.cache_dir.join(format!("{cache_id}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    fn remember(&self, key: String, resolved: ResolvedArtwork) {
        if let Ok(mut guard) = self.mem.write() {
            if guard.len() > 256 {
                guard.clear();
            }
            guard.insert(key, resolved);
        }
    }
}

fn cache_key(url: &str, size_px: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.update(b":");
    hasher.update(size_px.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(24);
    for b in &digest[..12] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn resolve_local_uri(uri: &str) -> Option<PathBuf> {
    if uri.is_empty() || uri.starts_with("http://") || uri.starts_with("https://") {
        return None;
    }
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    let path = PathBuf::from(path_str);
    if path.is_file() { Some(path) } else { None }
}

fn path_to_file_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.starts_with("file://") {
        s.into_owned()
    } else if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{}", s.replace('\\', "/"))
    }
}

fn guess_extension(content_type: &str, bytes: &[u8]) -> &'static str {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("png") || bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if ct.contains("webp") || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP") {
        "webp"
    } else if ct.contains("svg") || bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "svg"
    } else if ct.contains("jpeg") || ct.contains("jpg") || bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else {
        "bin"
    }
}

fn extract_accent_from_path(path: &Path, seed: &str) -> Hsla {
    if let Ok(bytes) = std::fs::read(path) {
        extract_accent_from_bytes(&bytes, seed)
    } else {
        ArtworkLoader::accent_for_seed(seed)
    }
}

/// Extracts a muted dominant [`Hsla`] accent from image bytes.
///
/// For SVG files it parses the first stop/fill hex color; for binary raster
/// formats it hashes sampled payload chunks into a harmonious curated accent
/// hue so Now Playing backdrops always remain warm, deep, and legible.
fn extract_accent_from_bytes(bytes: &[u8], seed: &str) -> Hsla {
    if let Ok(text) = std::str::from_utf8(bytes)
        && let Some(idx) = text.find("stop-color=\"#")
    {
        let start = idx + "stop-color=\"#".len();
        if text.len() >= start + 6
            && let Ok(val) = u32::from_str_radix(&text[start..start + 6], 16)
        {
            return theme::muted_backdrop_accent_from_rgb(val);
        }
    }

    // Sample bytes across the payload to derive a stable perceptual hue.
    let mut acc: u64 = 0xcbf29ce484222325;
    let step = (bytes.len() / 64).max(1);
    for b in bytes.iter().step_by(step).take(64) {
        acc ^= u64::from(*b);
        acc = acc.wrapping_mul(0x100000001b3);
    }
    for b in seed.as_bytes() {
        acc ^= u64::from(*b);
        acc = acc.wrapping_mul(0x100000001b3);
    }
    let hue = ((acc % 360) as f32) / 360.0;
    let sat = 0.36 + (((acc >> 12) % 24) as f32) / 100.0;
    let light = 0.36 + (((acc >> 20) % 12) as f32) / 100.0;
    theme::muted_backdrop_accent(hue, sat, light)
}

fn mute_for_backdrop(raw: Hsla) -> Hsla {
    theme::muted_backdrop_accent(raw.h, raw.s, raw.l)
}
