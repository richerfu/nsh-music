//! Artwork resolution, authenticated disk caching, offline fallback SVG
//! generation, and pixel-based HSV dominant color extraction (`egoist/mygo-music`
//! `artwork.go:822-899`) for Now Playing.

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
    /// Vivid dominant cover accent color (consumed by `theme::artwork_fog_colors`
    /// and `theme::artwork_backdrop_colors` without double-muting).
    pub accent: Hsla,
}

/// Resolves `{size}` artwork templates, downloads remote covers using plugin
/// `requestHeaders` into a persistent local disk cache, generates tasteful
/// fallback artwork for offline/missing covers, and extracts a pixel-based
/// dominant cover accent [`Hsla`].
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
        let base =
            nsh_core::element::data_dir().unwrap_or_else(|| std::env::temp_dir().join("nsh-music"));
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
        let fallback =
            self.generate_fallback(&cache_id, if seed.is_empty() { &expanded } else { seed });
        self.remember(cache_id, fallback.clone());
        fallback
    }

    /// Extracts a deterministic cover accent [`Hsla`] synchronously from a seed or URL.
    pub fn accent_for_seed(seed: &str) -> Hsla {
        let (start, _) = theme::fallback_cover_gradient(seed);
        start
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
            accent: hsla_a,
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
    if ct.contains("png") || bytes.starts_with(&[137, b'P', b'N', b'G']) {
        "png"
    } else if ct.contains("webp")
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
    {
        "webp"
    } else if ct.contains("svg") || bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "svg"
    } else if ct.contains("jpeg") || ct.contains("jpg") || bytes.starts_with(&[255, 216, 255]) {
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

/// Extracts the most vivid dominant cover [`Hsla`] from image bytes using real
/// pixel decoding and HSV hue-bucket voting matching `egoist/mygo-music`
/// (`internal/app/artwork.go:822-899`).
///
/// - Raster images (JPEG / PNG / WebP): decoded via `image::load_from_memory`,
///   block-averaged to `24×24`, voted into 18 hue buckets weighted by `s*s*v`
///   (ignoring `s < 0.15 || v < 0.15`), falling back to mean gray when the best
///   bucket weight is `< 2.0`, and converted with `theme::raw_accent_from_rgb`.
/// - SVG images: reads the first `stop-color="#rrggbb"` stop into `raw_accent_from_rgb`.
/// - Undecodable inputs: falls back cleanly to `ArtworkLoader::accent_for_seed`.
pub(crate) fn extract_accent_from_bytes(bytes: &[u8], seed: &str) -> Hsla {
    if let Ok(text) = std::str::from_utf8(bytes)
        && let Some(idx) = text.find("stop-color=\"#")
    {
        let start = idx + "stop-color=\"#".len();
        if text.len() >= start + 6
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&text[start..start + 2], 16),
                u8::from_str_radix(&text[start + 2..start + 4], 16),
                u8::from_str_radix(&text[start + 4..start + 6], 16),
            )
        {
            return theme::raw_accent_from_rgb(r, g, b);
        }
    }

    if let Ok(img) = image::load_from_memory(bytes) {
        return dominant_color_from_image(&img);
    }

    ArtworkLoader::accent_for_seed(seed)
}

/// Computes the dominant RGB color of `img` matching `mygo-music` `dominantColor`:
/// averages down to `24×24`, tallies 18 HSV hue buckets weighted by `s * s * v`,
/// ignores desaturated/dark pixels (`s < 0.15 || v < 0.15`), and returns mean gray
/// if no vivid bucket reaches weight `2.0`.
fn dominant_color_from_image(img: &image::DynamicImage) -> Hsla {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return theme::default_artwork_accent();
    }

    const GRID: u32 = 24;
    const BUCKETS: usize = 18;

    let mut weight = [0.0_f64; BUCKETS];
    let mut sum = [[0.0_f64; 3]; BUCKETS];
    let mut gray = [0.0_f64; 3];
    let total_cells = (GRID * GRID) as f64;

    for gy in 0..GRID {
        let y0 = (gy * h) / GRID;
        let y1 = (((gy + 1) * h) / GRID).max(y0 + 1).min(h);
        let step_y = ((y1 - y0) / 4).max(1);

        for gx in 0..GRID {
            let x0 = (gx * w) / GRID;
            let x1 = (((gx + 1) * w) / GRID).max(x0 + 1).min(w);
            let step_x = ((x1 - x0) / 4).max(1);

            let mut sr: u32 = 0;
            let mut sg: u32 = 0;
            let mut sb: u32 = 0;
            let mut cnt: u32 = 0;

            let mut yy = y0;
            while yy < y1 {
                let mut xx = x0;
                while xx < x1 {
                    let px = rgb.get_pixel(xx, yy).0;
                    sr += u32::from(px[0]);
                    sg += u32::from(px[1]);
                    sb += u32::from(px[2]);
                    cnt += 1;
                    xx += step_x;
                }
                yy += step_y;
            }

            let inv = 1.0 / (f64::from(cnt.max(1)) * 255.0);
            let r = f64::from(sr) * inv;
            let g = f64::from(sg) * inv;
            let b = f64::from(sb) * inv;

            gray[0] += r;
            gray[1] += g;
            gray[2] += b;

            let (hue, sat, val) = rgb_to_hsv(r, g, b);
            if sat < 0.15 || val < 0.15 {
                continue;
            }
            let w_px = sat * sat * val;
            let k = ((hue / 360.0 * BUCKETS as f64) as usize) % BUCKETS;
            weight[k] += w_px;
            sum[k][0] += r * w_px;
            sum[k][1] += g * w_px;
            sum[k][2] += b * w_px;
        }
    }

    let mut best = 0;
    for k in 1..BUCKETS {
        if weight[k] > weight[best] {
            best = k;
        }
    }

    let (r_out, g_out, b_out) = if weight[best] < 2.0 {
        (
            ((gray[0] / total_cells) * 255.0).clamp(0.0, 255.0) as u8,
            ((gray[1] / total_cells) * 255.0).clamp(0.0, 255.0) as u8,
            ((gray[2] / total_cells) * 255.0).clamp(0.0, 255.0) as u8,
        )
    } else {
        let bw = weight[best];
        (
            ((sum[best][0] / bw) * 255.0).clamp(0.0, 255.0) as u8,
            ((sum[best][1] / bw) * 255.0).clamp(0.0, 255.0) as u8,
            ((sum[best][2] / bw) * 255.0).clamp(0.0, 255.0) as u8,
        )
    };

    theme::raw_accent_from_rgb(r_out, g_out, b_out)
}

fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let mx = r.max(g.max(b));
    let mn = r.min(g.min(b));
    let v = mx;
    if mx <= 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let d = mx - mn;
    let s = d / mx;
    let mut h = if d <= 0.0 {
        0.0
    } else if (mx - r).abs() < f64::EPSILON {
        (((g - b) / d) % 6.0) * 60.0
    } else if (mx - g).abs() < f64::EPSILON {
        ((b - r) / d + 2.0) * 60.0
    } else {
        ((r - g) / d + 4.0) * 60.0
    };
    if h < 0.0 {
        h += 360.0;
    }
    (h, s, v)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

    use super::extract_accent_from_bytes;

    fn encode_solid_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut img = RgbImage::new(32, 32);
        for px in img.pixels_mut() {
            *px = Rgb([r, g, b]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode test png");
        out.into_inner()
    }

    #[test]
    fn dominant_color_extracts_vivid_hue_and_preserves_gray() {
        let blue_png = encode_solid_png(30, 110, 235);
        let blue = extract_accent_from_bytes(&blue_png, "blue-test");
        assert!(
            blue.s > 0.5,
            "vivid blue cover must stay saturated: {blue:?}"
        );
        assert!(
            (0.55..0.68).contains(&blue.h),
            "blue cover hue expected near 0.60, got {}",
            blue.h
        );

        let gray_png = encode_solid_png(132, 132, 132);
        let gray = extract_accent_from_bytes(&gray_png, "gray-test");
        assert!(
            gray.s < 0.05,
            "neutral gray cover must stay neutral: {gray:?}"
        );
    }
}
