//! Semantic design tokens, adaptive form-factor detection, typography,
//! radii, spacing, and elevation styles for `nsh-music`.
//!
//! All raw color literals live exclusively in this module; every other UI
//! module consumes semantic [`AdaptiveColor`] tokens or helpers from here.

use nsh_base::theme as base_theme;
use nsh_core::{
    element::{AdaptiveColor, BoxShadow, Idiom, Rgba, StyleRefinement, Styled, rgb, rgba},
    geometry::px,
    prelude::*,
};

/// Adaptive form factor matching `DESIGN.md` ("Desktop · Pad · Phone").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceForm {
    /// Wide desktop workspace (`w >= 900pt`).
    Desktop,
    /// Tablet or medium window (`600..900pt` or native Pad/Mac Catalyst idiom).
    Pad { landscape: bool },
    /// Compact mobile phone layout (`w < 600pt`).
    Phone,
}

impl DeviceForm {
    pub const fn is_desktop(self) -> bool {
        matches!(self, Self::Desktop)
    }

    pub const fn is_pad(self) -> bool {
        matches!(self, Self::Pad { .. })
    }

    pub const fn is_phone(self) -> bool {
        matches!(self, Self::Phone)
    }

    pub const fn is_touch(self) -> bool {
        !self.is_desktop()
    }

    /// Minimum interactive target size in points (`>= 44pt` on Pad/Phone).
    pub const fn min_touch_target(self) -> f32 {
        match self {
            Self::Desktop => 32.0,
            Self::Pad { .. } | Self::Phone => 44.0,
        }
    }

    /// Standard track row height in points.
    pub const fn track_row_height(self) -> f32 {
        match self {
            Self::Desktop => 52.0,
            Self::Pad { .. } => 54.0,
            Self::Phone => 58.0,
        }
    }

    /// Standard page horizontal inset in points.
    pub const fn page_padding_x(self) -> f32 {
        match self {
            Self::Desktop => 28.0,
            Self::Pad { .. } => 20.0,
            Self::Phone => 16.0,
        }
    }

    /// Number of media card grid columns for the given content width.
    pub fn card_columns(self, content_width: f32) -> usize {
        match self {
            Self::Desktop => {
                let cols = ((content_width - 56.0 + 16.0) / (172.0 + 16.0)).floor() as usize;
                cols.clamp(3, 6)
            }
            Self::Pad { landscape: true } => 4,
            Self::Pad { landscape: false } => 3,
            Self::Phone => 2,
        }
    }
}

/// Reactively detects the current [`DeviceForm`] from `idiom()` and `window()`.
///
/// Calling this inside a reactive binding or `computed` subscribes to window
/// resizes automatically.
pub fn detect_device_form() -> DeviceForm {
    let win = window();
    let w = win.width.0;
    let h = win.height.0;
    let current_idiom = idiom();

    if w < 600.0 {
        return DeviceForm::Phone;
    }

    if current_idiom == Idiom::Pad {
        return DeviceForm::Pad { landscape: w >= h };
    }

    if w >= 900.0 {
        DeviceForm::Desktop
    } else {
        DeviceForm::Pad { landscape: w >= h }
    }
}

/// Formats byte counts cleanly for Cache/Storage readouts (`12.4 MB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// `#rrggbb` with opacity `alpha` in `0.0..=1.0`.
pub const fn rgb_a(hex: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    rgba((hex << 8) | a)
}

/// Pair of light & dark colors compiled into an [`AdaptiveColor`].
pub const fn adaptive(light: Rgba, dark: Rgba) -> AdaptiveColor {
    AdaptiveColor {
        light: light.to_hsla(),
        dark: dark.to_hsla(),
    }
}

/// Core semantic color tokens ("Warm Porcelain" light + "Warm Obsidian" dark,
/// with Coral Rose `#F45D6C` and Amber Gold `#F5A623` accents).
pub mod colors {
    use super::{AdaptiveColor, adaptive, rgb, rgb_a};

    pub const BACKGROUND: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0x141416));
    pub const FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));
    pub const SIDEBAR: AdaptiveColor = adaptive(rgb(0xf4f4f6), rgb(0x1c1c1f));
    pub const SIDEBAR_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));
    pub const SIDEBAR_ACCENT: AdaptiveColor = adaptive(rgb(0xe9e9ee), rgb(0x2a2a2f));
    pub const SIDEBAR_BORDER: AdaptiveColor = adaptive(rgb(0xe5e5ea), rgb(0x26262a));

    pub const CARD: AdaptiveColor = adaptive(rgb(0xf8f8fa), rgb(0x222226));
    pub const CARD_ELEVATED: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0x28282d));
    pub const CARD_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));

    pub const POPOVER: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0x2b2b2e));
    pub const POPOVER_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));

    /// Coral Rose brand accent (light & dark tuned for crisp contrast).
    pub const PRIMARY: AdaptiveColor = adaptive(rgb(0xe53e38), rgb(0xfa586a));
    pub const PRIMARY_HOVER: AdaptiveColor = adaptive(rgb(0xcc332e), rgb(0xff7280));
    pub const PRIMARY_FOREGROUND: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0xffffff));
    pub const PRIMARY_SOFT: AdaptiveColor = adaptive(rgb_a(0xe53e38, 0.12), rgb_a(0xfa586a, 0.18));

    /// Secondary Amber Gold accent (lossless badges, vinyl highlights, warm glows).
    pub const AMBER: AdaptiveColor = adaptive(rgb(0xd98a16), rgb(0xf5a623));
    pub const AMBER_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1508), rgb(0x1d1508));
    pub const AMBER_SOFT: AdaptiveColor = adaptive(rgb_a(0xd98a16, 0.13), rgb_a(0xf5a623, 0.18));
    /// Warm champagne gold used for the Lossless badge on the dark Now Playing stage.
    pub const LOSSLESS_GOLD: AdaptiveColor = adaptive(rgb(0xf5d58a), rgb(0xf5d58a));

    pub const SECONDARY: AdaptiveColor = adaptive(rgb(0xefeff4), rgb(0x26262b));
    pub const SECONDARY_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));

    pub const MUTED: AdaptiveColor = adaptive(rgb(0xf0f0f5), rgb(0x232328));
    pub const MUTED_FOREGROUND: AdaptiveColor =
        adaptive(rgb_a(0x3c3c43, 0.62), rgb_a(0xebebf5, 0.56));
    pub const FAINT_FOREGROUND: AdaptiveColor =
        adaptive(rgb_a(0x3c3c43, 0.32), rgb_a(0xebebf5, 0.30));

    pub const ACCENT: AdaptiveColor = adaptive(rgb(0xe8e8ee), rgb(0x2c2c32));
    pub const ACCENT_FOREGROUND: AdaptiveColor = adaptive(rgb(0x1d1d1f), rgb(0xf5f5f7));

    pub const BORDER: AdaptiveColor = adaptive(rgb_a(0x000000, 0.07), rgb_a(0xffffff, 0.07));
    pub const BORDER_STRONG: AdaptiveColor = adaptive(rgb_a(0x000000, 0.14), rgb_a(0xffffff, 0.15));
    pub const INPUT: AdaptiveColor = adaptive(rgb(0xdcdce3), rgb(0x323238));
    pub const RING: AdaptiveColor = adaptive(rgb(0xe53e38), rgb(0xfa586a));

    /// Subtle interactive washes matching macOS / `mygo-music` chrome.
    pub const HOVER: AdaptiveColor = adaptive(rgb_a(0x000000, 0.04), rgb_a(0xffffff, 0.06));
    pub const PRESS: AdaptiveColor = adaptive(rgb_a(0x000000, 0.08), rgb_a(0xffffff, 0.10));
    pub const SELECTED: AdaptiveColor = adaptive(rgb_a(0xe53e38, 0.10), rgb_a(0xfa586a, 0.16));

    /// Translucent glass surfaces.
    pub const GLASS: AdaptiveColor = adaptive(rgb_a(0xffffff, 0.86), rgb_a(0x18181c, 0.84));
    pub const GLASS_BAR: AdaptiveColor = adaptive(rgb_a(0xf8f8fa, 0.90), rgb_a(0x1c1c1f, 0.88));
    pub const GLASS_RIM: AdaptiveColor = adaptive(rgb_a(0x000000, 0.07), rgb_a(0xffffff, 0.09));

    /// Semantic status tokens.
    pub const OK: AdaptiveColor = adaptive(rgb(0x16a34a), rgb(0x4ade80));
    pub const OK_SOFT: AdaptiveColor = adaptive(rgb_a(0x16a34a, 0.12), rgb_a(0x4ade80, 0.16));
    pub const WARN: AdaptiveColor = adaptive(rgb(0xd97706), rgb(0xf5a623));
    pub const WARN_SOFT: AdaptiveColor = adaptive(rgb_a(0xd97706, 0.13), rgb_a(0xf5a623, 0.16));
    pub const DANGER: AdaptiveColor = adaptive(rgb(0xdc2626), rgb(0xf87171));
    pub const DANGER_SOFT: AdaptiveColor = adaptive(rgb_a(0xdc2626, 0.12), rgb_a(0xf87171, 0.16));

    /// Now Playing stage (always deep obsidian theatrical stage).
    pub const NP_BG: AdaptiveColor = adaptive(rgb(0x0b0b0d), rgb(0x0b0b0d));
    pub const NP_FG: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0xffffff));
    pub const NP_MUTED: AdaptiveColor = adaptive(rgb_a(0xffffff, 0.66), rgb_a(0xffffff, 0.66));
    pub const NP_FAINT: AdaptiveColor = adaptive(rgb_a(0xffffff, 0.34), rgb_a(0xffffff, 0.34));
    pub const NP_SURFACE: AdaptiveColor = adaptive(rgb_a(0xffffff, 0.10), rgb_a(0xffffff, 0.10));
    pub const NP_SURFACE_HOVER: AdaptiveColor =
        adaptive(rgb_a(0xffffff, 0.17), rgb_a(0xffffff, 0.17));
    pub const NP_BORDER: AdaptiveColor = adaptive(rgb_a(0xffffff, 0.14), rgb_a(0xffffff, 0.14));

    /// Modal backdrops and media overlays.
    pub const SCRIM: AdaptiveColor = adaptive(rgb_a(0x0b0b0d, 0.48), rgb_a(0x000000, 0.64));
    pub const SCRIM_SOFT: AdaptiveColor = adaptive(rgb_a(0x0b0b0d, 0.28), rgb_a(0x000000, 0.42));
    pub const MEDIA_BADGE_BG: AdaptiveColor =
        adaptive(rgb_a(0x0b0b0d, 0.62), rgb_a(0x0b0b0d, 0.68));
    pub const MEDIA_BADGE_FG: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0xffffff));
    pub const SKELETON: AdaptiveColor = adaptive(rgb(0xe8e7e3), rgb(0x2b2b31));
    pub const SKELETON_SHEEN: AdaptiveColor =
        adaptive(rgb_a(0xffffff, 0.56), rgb_a(0xffffff, 0.09));
    pub const NP_SKELETON_BASE: AdaptiveColor =
        adaptive(rgb_a(0xffffff, 0.10), rgb_a(0xffffff, 0.10));
    pub const NP_SKELETON_SHEEN: AdaptiveColor =
        adaptive(rgb_a(0xffffff, 0.18), rgb_a(0xffffff, 0.18));
    pub const TRACK_RAIL: AdaptiveColor = adaptive(rgb_a(0x1d1d1f, 0.12), rgb_a(0xffffff, 0.14));
    pub const TRACK_BUFFERED: AdaptiveColor =
        adaptive(rgb_a(0x1d1d1f, 0.24), rgb_a(0xffffff, 0.26));
    pub const SWITCH_THUMB: AdaptiveColor = adaptive(rgb(0xffffff), rgb(0xf5f5f7));
    pub const TRANSPARENT: AdaptiveColor = AdaptiveColor::TRANSPARENT;
}

/// Reactive theme token accessors honoring `theme.json` overrides with Warm
/// Porcelain / Warm Obsidian fallbacks.
pub fn bg() -> AdaptiveColor {
    base_theme().background
}

pub fn fg() -> AdaptiveColor {
    base_theme().foreground
}

pub fn sidebar() -> AdaptiveColor {
    base_theme().sidebar
}

pub fn sidebar_fg() -> AdaptiveColor {
    base_theme().sidebar_foreground
}

pub fn sidebar_accent() -> AdaptiveColor {
    base_theme().sidebar_accent
}

pub fn sidebar_border() -> AdaptiveColor {
    base_theme().sidebar_border
}

pub fn card() -> AdaptiveColor {
    base_theme().card
}

pub fn card_elevated() -> AdaptiveColor {
    colors::CARD_ELEVATED
}

pub fn card_fg() -> AdaptiveColor {
    base_theme().card_foreground
}

pub fn popover() -> AdaptiveColor {
    base_theme().popover
}

pub fn popover_fg() -> AdaptiveColor {
    base_theme().popover_foreground
}

pub fn primary() -> AdaptiveColor {
    base_theme().primary
}

pub fn primary_hover() -> AdaptiveColor {
    colors::PRIMARY_HOVER
}

pub fn primary_fg() -> AdaptiveColor {
    base_theme().primary_foreground
}

pub fn primary_soft() -> AdaptiveColor {
    colors::PRIMARY_SOFT
}

pub fn amber() -> AdaptiveColor {
    colors::AMBER
}

pub fn amber_soft() -> AdaptiveColor {
    colors::AMBER_SOFT
}

/// Warm champagne gold (`#f5d58a`) for the Lossless badge on the Now Playing stage.
pub fn lossless_gold() -> AdaptiveColor {
    colors::LOSSLESS_GOLD
}

pub fn secondary() -> AdaptiveColor {
    base_theme().secondary
}

pub fn secondary_fg() -> AdaptiveColor {
    base_theme().secondary_foreground
}

pub fn muted() -> AdaptiveColor {
    base_theme().muted
}

pub fn muted_fg() -> AdaptiveColor {
    base_theme().muted_foreground
}

pub fn faint_fg() -> AdaptiveColor {
    colors::FAINT_FOREGROUND
}

pub fn accent() -> AdaptiveColor {
    base_theme().accent
}

pub fn accent_fg() -> AdaptiveColor {
    base_theme().accent_foreground
}

pub fn border() -> AdaptiveColor {
    base_theme().border
}

pub fn border_strong() -> AdaptiveColor {
    colors::BORDER_STRONG
}

pub fn input_border() -> AdaptiveColor {
    base_theme().input
}

pub fn ring() -> AdaptiveColor {
    base_theme().ring
}

pub fn hover() -> AdaptiveColor {
    colors::HOVER
}

pub fn press() -> AdaptiveColor {
    colors::PRESS
}

pub fn selected() -> AdaptiveColor {
    colors::SELECTED
}

pub fn glass() -> AdaptiveColor {
    colors::GLASS
}

pub fn glass_bar() -> AdaptiveColor {
    colors::GLASS_BAR
}

pub fn glass_rim() -> AdaptiveColor {
    colors::GLASS_RIM
}

pub fn ok() -> AdaptiveColor {
    base_theme().color("ok").unwrap_or(colors::OK)
}

pub fn ok_soft() -> AdaptiveColor {
    colors::OK_SOFT
}

pub fn warn() -> AdaptiveColor {
    base_theme().color("warn").unwrap_or(colors::WARN)
}

pub fn warn_soft() -> AdaptiveColor {
    colors::WARN_SOFT
}

pub fn danger() -> AdaptiveColor {
    base_theme().destructive
}

pub fn danger_soft() -> AdaptiveColor {
    colors::DANGER_SOFT
}

pub fn np_bg() -> AdaptiveColor {
    base_theme().color("np-bg").unwrap_or(colors::NP_BG)
}

pub fn np_fg() -> AdaptiveColor {
    base_theme().color("np-fg").unwrap_or(colors::NP_FG)
}

pub fn np_muted() -> AdaptiveColor {
    base_theme().color("np-muted").unwrap_or(colors::NP_MUTED)
}

pub fn np_faint() -> AdaptiveColor {
    colors::NP_FAINT
}

pub fn np_surface() -> AdaptiveColor {
    colors::NP_SURFACE
}

pub fn np_surface_hover() -> AdaptiveColor {
    colors::NP_SURFACE_HOVER
}

pub fn np_border() -> AdaptiveColor {
    colors::NP_BORDER
}

pub fn scrim() -> AdaptiveColor {
    colors::SCRIM
}

pub fn scrim_soft() -> AdaptiveColor {
    colors::SCRIM_SOFT
}

pub fn skeleton() -> AdaptiveColor {
    colors::SKELETON
}

pub fn track_rail() -> AdaptiveColor {
    colors::TRACK_RAIL
}

pub fn track_buffered() -> AdaptiveColor {
    colors::TRACK_BUFFERED
}

/// Converts `r, g, b` (`0.0..=1.0`) into `(hue_deg in 0..360, sat in 0..1, val in 0..1)`.
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 1e-6 {
        0.0
    } else if (max - r).abs() <= 1e-6 {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if (max - g).abs() <= 1e-6 {
        60.0 * (((b - r) / d) + 2.0)
    } else {
        60.0 * (((r - g) / d) + 4.0)
    };
    let s = if max <= 1e-6 { 0.0 } else { d / max };
    (h.rem_euclid(360.0), s.clamp(0.0, 1.0), max.clamp(0.0, 1.0))
}

/// Exact port of `egoist/mygo-music` `hsvColor(h, s, v)` (`backdrop.go:262-282`).
fn hsv_to_hsla(h_deg: f32, s: f32, v: f32) -> Hsla {
    let h = h_deg.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    let r8 = ((r + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u32;
    let g8 = ((g + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u32;
    let b8 = ((b + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u32;
    rgb((r8 << 16) | (g8 << 8) | b8).to_hsla()
}

/// Linearly blends two [`Hsla`] colors in sRGB space (`t = 0.0` returns `a`,
/// `t = 1.0` returns `b`), matching Go `ui.Color.Mix(other, t)` without hue spin.
pub fn mix_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let ca = a.to_rgb();
    let cb = b.to_rgb();
    Rgba {
        r: ca.r + (cb.r - ca.r) * t,
        g: ca.g + (cb.g - ca.g) * t,
        b: ca.b + (cb.b - ca.b) * t,
        a: ca.a + (cb.a - ca.a) * t,
    }
    .to_hsla()
}

/// Converts raw extracted cover RGB bytes directly into [`Hsla`] without
/// pre-muting so downstream HSV helpers receive the cover's true hue/saturation.
pub fn raw_accent_from_rgb(r: u8, g: u8, b: u8) -> Hsla {
    let hex24 = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    rgb(hex24).to_hsla()
}

/// Default ambient artwork accent (`Hsla` of Coral Rose muted for backdrops).
pub fn default_artwork_accent() -> Hsla {
    hsla(0.985, 0.56, 0.42, 1.0)
}

/// Derives a muted, contrast-safe [`AdaptiveColor`] surface tint from a dynamic
/// album artwork accent [`Hsla`].
pub fn artwork_tint(accent: Hsla) -> AdaptiveColor {
    let h = accent.h.rem_euclid(1.0);
    let s_light = accent.s.clamp(0.18, 0.45);
    let s_dark = accent.s.clamp(0.24, 0.55);
    AdaptiveColor::new(hsla(h, s_light, 0.94, 1.0), hsla(h, s_dark, 0.14, 1.0))
}

/// Derives a soft translucent wash from a dynamic album artwork accent [`Hsla`].
pub fn artwork_wash(accent: Hsla, alpha: f32) -> AdaptiveColor {
    let h = accent.h.rem_euclid(1.0);
    let s = accent.s.clamp(0.25, 0.65);
    let a = alpha.clamp(0.0, 1.0);
    AdaptiveColor::new(hsla(h, s, 0.50, a * 0.7), hsla(h, s, 0.46, a))
}

/// Derives the `(top, foot)` gradient shades for the Now Playing stage backdrop,
/// ported 1:1 from `egoist/mygo-music` `backdropColors` (`backdrop.go:234-240`).
/// Neutral gray covers (`s <= 0.12`) remain gray.
pub fn artwork_backdrop_colors(accent: Hsla) -> (Hsla, Hsla) {
    let c = accent.to_rgb();
    let (h, mut s, _) = rgb_to_hsv(c.r, c.g, c.b);
    if s > 0.12 {
        s = (s * 1.3).clamp(0.45, 0.66);
    }
    (
        hsv_to_hsla(h, s, 0.35),
        hsv_to_hsla(h, (s * 1.05).min(0.70), 0.24),
    )
}

/// Derives a deep atmospheric backdrop base color for the Now Playing stage.
pub fn artwork_stage_bg(accent: Hsla) -> Hsla {
    artwork_backdrop_colors(accent).1
}

/// Derives contrasting `(deep, fog)` colors for the WGSL ambient fog shader,
/// ported 1:1 from `egoist/mygo-music` `fogColors` (`backdrop.go:253-259`).
/// Neutral gray covers (`s <= 0.12`) remain gray.
pub fn artwork_fog_colors(accent: Hsla) -> (Hsla, Hsla) {
    let c = accent.to_rgb();
    let (h, mut s, _) = rgb_to_hsv(c.r, c.g, c.b);
    if s > 0.12 {
        s = (s * 1.3).clamp(0.60, 0.85);
    }
    (hsv_to_hsla(h, s, 0.30), hsv_to_hsla(h, s * 0.60, 0.72))
}

/// Derives the luminous Now Playing stage accent (`tint.Mix(white, 0.45)`)
/// used for active indicators, lyrics highlights, and scrubber smoke on the dark stage.
pub fn artwork_np_accent(accent: Hsla) -> Hsla {
    mix_color(accent, colors::NP_FG.dark, 0.45)
}

/// Curated warm/editorial 2-stop cover palettes for offline or demo artwork.
/// Kept inside `theme.rs` so no raw color literals exist elsewhere.
const FALLBACK_GRADIENTS: &[(u32, u32)] = &[
    (0xf45d6c, 0x3b1830), // Coral Rose -> Plum Obsidian
    (0xf5a623, 0x3a2010), // Amber Gold -> Espresso
    (0x4c8bf5, 0x162244), // Cobalt -> Midnight
    (0x2bb673, 0x123026), // Emerald -> Deep Pine
    (0x9b6bff, 0x261848), // Violet -> Indigo Night
    (0xe86f51, 0x3e1e18), // Terracotta -> Umber
    (0x22b8cf, 0x102d38), // Cyan -> Abyss
    (0xd65db1, 0x351732), // Orchid -> Velvet
];

/// Returns a deterministic `(start, end)` `Hsla` gradient pair for a seed string
/// (album/track/playlist id or title).
pub fn fallback_cover_gradient(seed: &str) -> (Hsla, Hsla) {
    let mut hash: u64 = 14695981039346656037;
    for b in seed.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(1099511628211);
    }
    let idx = (hash as usize) % FALLBACK_GRADIENTS.len();
    let (a, b) = FALLBACK_GRADIENTS[idx];
    (rgb(a).to_hsla(), rgb(b).to_hsla())
}

/// Returns the matching raw hex strings `(start_hex, end_hex)` for generating
/// offline fallback SVG covers in `state/artwork.rs`.
pub fn fallback_cover_hex_pair(seed: &str) -> (&'static str, &'static str) {
    const HEX_PAIRS: &[(&str, &str)] = &[
        ("#f45d6c", "#3b1830"),
        ("#f5a623", "#3a2010"),
        ("#4c8bf5", "#162244"),
        ("#2bb673", "#123026"),
        ("#9b6bff", "#261848"),
        ("#e86f51", "#3e1e18"),
        ("#22b8cf", "#102d38"),
        ("#d65db1", "#351732"),
    ];
    let mut hash: u64 = 14695981039346656037;
    for b in seed.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(1099511628211);
    }
    HEX_PAIRS[(hash as usize) % HEX_PAIRS.len()]
}

/// Builds a muted backdrop-safe [`Hsla`] accent from normalized hue, saturation,
/// and lightness values (all color construction stays in `theme.rs`).
pub fn muted_backdrop_accent(hue: f32, sat: f32, light: f32) -> Hsla {
    hsla(
        hue.rem_euclid(1.0),
        sat.clamp(0.28, 0.58),
        light.clamp(0.32, 0.48),
        1.0,
    )
}

/// Mutes a 24-bit RGB hex value into a backdrop-safe [`Hsla`] accent.
pub fn muted_backdrop_accent_from_rgb(hex24: u32) -> Hsla {
    let raw = rgb(hex24).to_hsla();
    muted_backdrop_accent(raw.h, raw.s, raw.l)
}

/// Renders a deterministic offline/demo fallback SVG cover markup string.
pub fn fallback_cover_svg(seed: &str) -> String {
    let (hex_a, hex_b) = fallback_cover_hex_pair(seed);
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 400">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="{hex_a}"/>
      <stop offset="100%" stop-color="{hex_b}"/>
    </linearGradient>
    <radialGradient id="r" cx="30%" cy="24%" r="72%">
      <stop offset="0%" stop-color="#ffffff" stop-opacity="0.28"/>
      <stop offset="100%" stop-color="#ffffff" stop-opacity="0"/>
    </radialGradient>
  </defs>
  <rect width="400" height="400" fill="url(#g)"/>
  <rect width="400" height="400" fill="url(#r)"/>
  <circle cx="200" cy="200" r="118" fill="none" stroke="#ffffff" stroke-opacity="0.16" stroke-width="2"/>
  <circle cx="200" cy="200" r="82" fill="none" stroke="#ffffff" stroke-opacity="0.12" stroke-width="1.5"/>
  <circle cx="200" cy="200" r="34" fill="#0b0b0d" fill-opacity="0.34" stroke="#ffffff" stroke-opacity="0.28" stroke-width="2"/>
  <circle cx="200" cy="200" r="10" fill="#ffffff" fill-opacity="0.72"/>
</svg>"##
    )
}

/// Typography size scale in points.
pub mod font_size {
    pub const DISPLAY: f32 = 28.0;
    pub const TITLE_LG: f32 = 22.0;
    pub const TITLE: f32 = 18.0;
    pub const HEADING: f32 = 16.0;
    pub const BODY: f32 = 14.0;
    pub const BODY_MOBILE: f32 = 15.0;
    pub const SUBTITLE: f32 = 13.0;
    pub const CAPTION: f32 = 12.0;
    pub const MICRO: f32 = 11.0;
    pub const MONO: &str = "monospace";
}

/// Corner radius scale in points.
pub mod radius {
    pub const SM: f32 = 6.0;
    pub const CONTROL: f32 = 10.0;
    pub const CARD: f32 = 12.0;
    pub const LG: f32 = 14.0;
    pub const XL: f32 = 18.0;
    pub const SHEET: f32 = 20.0;
    pub const PILL: f32 = 999.0;
}

/// Spacing scale in points.
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

/// Elevation tiers matching `DESIGN.md` & NSH Design Guides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Subtle hairline lift (buttons, chips, active tabs).
    Subtle,
    /// Media cards and artwork containers.
    Card,
    /// Floating mini-player / capsule player bar.
    Floating,
    /// Anchored popovers and menus.
    Popover,
    /// Centered dialogs and bottom sheets.
    Modal,
}

const fn drop_shadow(y: f32, blur: f32, spread: f32, col: Rgba) -> BoxShadow {
    BoxShadow {
        offset_x: px(0.0),
        offset_y: px(y),
        blur: px(blur),
        spread: px(spread),
        color: col.to_packed(),
        inset: false,
    }
}

const fn inner_rim(y: f32, col: Rgba) -> BoxShadow {
    BoxShadow {
        offset_x: px(0.0),
        offset_y: px(y),
        blur: px(0.0),
        spread: px(0.0),
        color: col.to_packed(),
        inset: true,
    }
}

const SHADOW_INK: u32 = 0x101014;
const SHADOW_BLACK: u32 = 0x000000;
const SHADOW_WHITE: u32 = 0xffffff;

const SUBTLE_LIGHT: &[BoxShadow] = &[drop_shadow(1.0, 3.0, 0.0, rgb_a(SHADOW_INK, 0.08))];
const SUBTLE_DARK: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.06)),
    drop_shadow(1.0, 4.0, 0.0, rgb_a(SHADOW_BLACK, 0.42)),
];

const CARD_LIGHT: &[BoxShadow] = &[
    drop_shadow(1.0, 3.0, 0.0, rgb_a(SHADOW_INK, 0.06)),
    drop_shadow(8.0, 22.0, -6.0, rgb_a(SHADOW_INK, 0.10)),
];
const CARD_DARK: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.05)),
    drop_shadow(10.0, 26.0, -6.0, rgb_a(SHADOW_BLACK, 0.48)),
];

const FLOATING_LIGHT: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.85)),
    drop_shadow(8.0, 28.0, -4.0, rgb_a(SHADOW_INK, 0.14)),
];
const FLOATING_DARK: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.10)),
    drop_shadow(10.0, 32.0, -4.0, rgb_a(SHADOW_BLACK, 0.58)),
];

const POPOVER_LIGHT: &[BoxShadow] = &[
    drop_shadow(2.0, 8.0, 0.0, rgb_a(SHADOW_INK, 0.06)),
    drop_shadow(14.0, 36.0, -4.0, rgb_a(SHADOW_INK, 0.15)),
];
const POPOVER_DARK: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.09)),
    drop_shadow(16.0, 40.0, -4.0, rgb_a(SHADOW_BLACK, 0.62)),
];

const MODAL_LIGHT: &[BoxShadow] = &[drop_shadow(20.0, 52.0, -6.0, rgb_a(SHADOW_INK, 0.22))];
const MODAL_DARK: &[BoxShadow] = &[
    inner_rim(1.0, rgb_a(SHADOW_WHITE, 0.10)),
    drop_shadow(24.0, 60.0, -6.0, rgb_a(SHADOW_BLACK, 0.72)),
];

impl Elevation {
    pub fn shadows(self) -> (&'static [BoxShadow], &'static [BoxShadow]) {
        match self {
            Self::Subtle => (SUBTLE_LIGHT, SUBTLE_DARK),
            Self::Card => (CARD_LIGHT, CARD_DARK),
            Self::Floating => (FLOATING_LIGHT, FLOATING_DARK),
            Self::Popover => (POPOVER_LIGHT, POPOVER_DARK),
            Self::Modal => (MODAL_LIGHT, MODAL_DARK),
        }
    }
}

/// Extension trait adding elevation and glass surface helpers to every [`Styled`] element.
pub trait Elevated: Styled + Sized {
    /// Applies adaptive light/dark box shadows for the requested [`Elevation`] tier.
    fn elevation(self, level: Elevation) -> Self {
        let (light, dark) = level.shadows();
        let dark_vec: Vec<BoxShadow> = dark.to_vec();
        self.shadow(light.iter().copied())
            .dark(move |s: StyleRefinement| s.shadow(dark_vec))
    }

    /// Translucent blurred surface for floating player capsules and drawers.
    fn glass_surface(self) -> Self {
        self.bg(colors::GLASS)
            .border_1()
            .border_color(colors::GLASS_RIM)
            .backdrop_blur(px(16.0))
            .elevation(Elevation::Floating)
    }

    /// Translucent bar surface for sticky desktop player bar and top chrome.
    fn glass_bar_surface(self) -> Self {
        self.bg(colors::GLASS_BAR).backdrop_blur(px(16.0))
    }
}

impl<T: Styled + Sized> Elevated for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_cover_stays_neutral_in_backdrop_and_fog() {
        let gray = raw_accent_from_rgb(128, 128, 128);
        let (deep, fog) = artwork_fog_colors(gray);
        assert!(
            deep.s < 0.02,
            "expected neutral gray deep, got s={}",
            deep.s
        );
        assert!(fog.s < 0.02, "expected neutral gray fog, got s={}", fog.s);

        let (top, foot) = artwork_backdrop_colors(gray);
        assert!(top.s < 0.02, "expected neutral gray top, got s={}", top.s);
        assert!(
            foot.s < 0.02,
            "expected neutral gray foot, got s={}",
            foot.s
        );
    }

    #[test]
    fn saturated_cover_matches_mygo_hsv_targets() {
        let coral = raw_accent_from_rgb(240, 80, 96);
        let (deep, fog) = artwork_fog_colors(coral);
        let deep_rgb = deep.to_rgb();
        let fog_rgb = fog.to_rgb();
        let (_, deep_s, deep_v) = rgb_to_hsv(deep_rgb.r, deep_rgb.g, deep_rgb.b);
        let (_, fog_s, fog_v) = rgb_to_hsv(fog_rgb.r, fog_rgb.g, fog_rgb.b);
        assert!((0.58..=0.88).contains(&deep_s), "deep_s={deep_s}");
        assert!((0.27..=0.33).contains(&deep_v), "deep_v={deep_v}");
        assert!((fog_s - deep_s * 0.60).abs() < 0.04, "fog_s={fog_s}");
        assert!((0.69..=0.75).contains(&fog_v), "fog_v={fog_v}");

        let (top, foot) = artwork_backdrop_colors(coral);
        let top_rgb = top.to_rgb();
        let foot_rgb = foot.to_rgb();
        let (_, top_s, top_v) = rgb_to_hsv(top_rgb.r, top_rgb.g, top_rgb.b);
        let (_, _, foot_v) = rgb_to_hsv(foot_rgb.r, foot_rgb.g, foot_rgb.b);
        assert!((0.43..=0.68).contains(&top_s), "top_s={top_s}");
        assert!((0.32..=0.38).contains(&top_v), "top_v={top_v}");
        assert!((0.21..=0.27).contains(&foot_v), "foot_v={foot_v}");
    }

    #[test]
    fn np_accent_mixes_toward_white_in_srgb() {
        let blue = raw_accent_from_rgb(0, 100, 200);
        let mixed = artwork_np_accent(blue).to_rgb();
        assert!((mixed.r - 0.45).abs() < 0.02, "r={}", mixed.r);
        assert!(mixed.b > 0.85, "b={}", mixed.b);
    }
}
