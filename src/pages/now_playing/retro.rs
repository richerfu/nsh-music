//! Three selectable centerpiece artwork modes (`NowPlayingMode`):
//! - `Cover`: Crisp elevated album art with subtle audio-reactive halo
//! - `Vinyl`: Detailed `canvas`-drawn 33 RPM turntable with concentric grooves,
//!   spinning center label at smooth 60fps, specular highlight, and spring tonearm
//! - `Cassette`: Detailed `canvas`-drawn retro cassette deck with twin 60fps
//!   rotating tape reels, tape window fill reflecting playback progress, and label strip

use std::{
    f32::consts::{FRAC_PI_2, PI, TAU},
    time::Duration,
};

use nsh_core::{
    canvas::{CanvasGradient, CanvasTextAlign, CanvasTextBaseline, LineCap, LineJoin},
    geometry::{percentage, point, size},
    prelude::*,
};

use crate::{
    state::{AppStore, NowPlayingMode},
    ui::{
        icons::{Icon, icon},
        motion,
        theme::{self, Elevated, Elevation, colors, radius},
    },
};

/// 33⅓ RPM turntable speed: 200°/s -> 3.2° per 16ms frame.
const VINYL_DEG_PER_FRAME: f32 = 3.2;

/// Retro cassette reel speed: ~106°/s -> 1.70° per 16ms frame.
const CASSETTE_DEG_PER_FRAME: f32 = 1.70;

/// Renders the active centerpiece artwork presentation (`Cover`, `Vinyl`, or `Cassette`).
pub fn retro_centerpiece(store: AppStore, size_pt: f32) -> View {
    let box_w = (size_pt * 1.18).max(size_pt + 28.0);
    let box_h = size_pt + 24.0;

    view()
        .w(px(box_w))
        .h(px(box_h))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .child(dynamic(move || {
            let mode = store.now_playing_mode.get();
            untracked(|| match mode {
                NowPlayingMode::Cover => cover_mode(store, size_pt).key("np-mode-cover"),
                NowPlayingMode::Vinyl => vinyl_mode(store, size_pt).key("np-mode-vinyl"),
                NowPlayingMode::Cassette => cassette_mode(store, size_pt).key("np-mode-cassette"),
            })
        }))
}

fn current_track_seed(store: AppStore) -> String {
    store.current_track.with(|t| {
        t.as_ref()
            .map(|tr| {
                if !tr.album.id.is_empty() {
                    tr.album.id.clone()
                } else {
                    tr.id.clone()
                }
            })
            .unwrap_or_else(|| "nsh-music".to_owned())
    })
}

fn has_renderable_art(uri: &SharedString) -> bool {
    !uri.is_empty()
}

/// Smoothly springs opacity & subtle scale when switching between Cover / Vinyl / Cassette.
fn mode_entrance_animations() -> (AnimatedValue, AnimatedValue) {
    let reduced = reduce_motion();
    let opacity = animated_value(if reduced { 1.0 } else { 0.0 });
    let scale = animated_value(if reduced { 1.0 } else { 0.94 });
    if !reduced {
        motion::animate_timing(&opacity, 1.0, motion::timing_fast());
        motion::animate_spring(&scale, 1.0, motion::spring_gentle());
    }
    (opacity, scale)
}

/// Drives a local rotation angle in degrees at 60fps ONLY while playing and
/// `!reduce_motion()`, keeping paused/reduced-motion states at zero idle redraws.
fn smooth_spin_signal(store: AppStore, deg_per_frame: f32) -> Signal<f32> {
    let spin_deg = signal(0.0_f32);
    set_interval(Duration::from_millis(16), move || {
        if store.is_playing.get_untracked() && !reduce_motion() {
            let next = (spin_deg.try_get().unwrap_or(0.0) + deg_per_frame).rem_euclid(360.0);
            let _ = spin_deg.try_set(next);
        }
    });
    spin_deg
}

// -----------------------------------------------------------------------------
// 1. Cover Mode: Crisp elevated album art with subtle audio-reactive halo
// -----------------------------------------------------------------------------

fn cover_mode(store: AppStore, size_pt: f32) -> View {
    let halo_box = size_pt + 36.0;
    let initial_playing = untracked(|| store.is_playing.get());
    let (enter_opacity, enter_scale) = mode_entrance_animations();
    let cover_scale = animated_value(if initial_playing || reduce_motion() {
        1.0
    } else {
        0.95
    });
    let prev_playing = stored(initial_playing);

    effect(move || {
        let playing = store.is_playing.get();
        if prev_playing.replace(playing) != playing {
            let target = if playing || reduce_motion() { 1.0 } else { 0.95 };
            motion::animate_spring(&cover_scale, target, motion::spring_gentle());
        }
    });

    view()
        .size(px(halo_box))
        .items_center()
        .justify_center()
        .bind_opacity(&enter_opacity)
        .bind_scale(&enter_scale)
        // Subtle audio-reactive halo behind the album cover
        .child(
            canvas(move || {
                let accent = store.current_accent.get();
                let playing = store.is_playing.get();
                let reduced = reduce_motion();
                let spectrum = if playing && !reduced {
                    store.spectrum.get()
                } else {
                    [0.0; 16]
                };
                let energy =
                    ((spectrum[0] + spectrum[1] + spectrum[2] + spectrum[3]) * 0.25).clamp(0.0, 1.0);
                paint_cover_halo(
                    halo_box,
                    size_pt,
                    accent,
                    &spectrum,
                    energy,
                    playing && !reduced,
                )
            })
            .absolute()
            .inset_0()
            .size_full()
            .pointer_events(PointerEvents::None),
        )
        // Elevated album art frame
        .child(
            Elevated::elevation(
                view()
                    .size(px(size_pt))
                    .rounded(px(radius::XL))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme::np_border),
                Elevation::Modal,
            )
            .bind_scale(&cover_scale)
            .bg_linear_to_br()
            .gradient_from(move || {
                let (a, _) = theme::fallback_cover_gradient(&current_track_seed(store));
                AdaptiveColor::from(a)
            })
            .gradient_to(move || {
                let (_, b) = theme::fallback_cover_gradient(&current_track_seed(store));
                AdaptiveColor::from(b)
            })
            .items_center()
            .justify_center()
            .child(
                icon(Icon::Disc, (size_pt * 0.34).clamp(40.0, 92.0), colors::NP_MUTED)
                    .opacity(0.62),
            )
            .child(when(
                move || has_renderable_art(&store.current_artwork_uri.get()),
                move || {
                    image(move || store.current_artwork_uri.get())
                        .object_fit(ResizeMode::Cover)
                        .absolute()
                        .inset_0()
                        .size_full()
                },
            )),
        )
}

fn paint_cover_halo(
    box_size: f32,
    cover_size: f32,
    accent: Hsla,
    spectrum: &[f32; 16],
    energy: f32,
    animate: bool,
) -> Painter {
    let c = box_size * 0.5;
    let inner_r = cover_size * 0.36;
    let outer_r = (box_size * 0.50).min(cover_size * 0.5 + 16.0 + energy * 10.0);

    let glow_core = accent
        .saturate(0.24)
        .lighten(0.18)
        .alpha((0.28 + energy * 0.28).clamp(0.0, 0.58));
    let glow_edge = accent.alpha(0.0);

    let grad = CanvasGradient::radial(
        point(px(c), px(c)),
        px(inner_r),
        point(px(c), px(c)),
        px(outer_r),
    )
    .add_color_stop(percentage(0.0), glow_core)
    .add_color_stop(percentage(1.0), glow_edge);

    let mut p = Painter::new()
        .fill_style(grad)
        .begin_path()
        .arc(c, c, outer_r, 0.0, TAU)
        .fill();

    // Subtle 16-segment reactive rim ticks when actively playing
    if animate {
        let rim_r = cover_size * 0.5 + 7.0;
        let tick_col = accent.lighten(0.32).alpha(0.36);
        p = p
            .stroke_color(tick_col)
            .line_width(2.0)
            .line_cap(LineCap::Round);
        for (i, &band) in spectrum.iter().enumerate() {
            let amp = band.clamp(0.0, 1.0);
            if amp < 0.08 {
                continue;
            }
            let theta = (i as f32 / spectrum.len() as f32) * TAU - FRAC_PI_2;
            let (sin, cos) = theta.sin_cos();
            let r0 = rim_r;
            let r1 = rim_r + 2.0 + amp * 8.0;
            p = p
                .begin_path()
                .move_to(c + cos * r0, c + sin * r0)
                .line_to(c + cos * r1, c + sin * r1)
                .stroke();
        }
    }

    p
}

// -----------------------------------------------------------------------------
// 2. Vinyl Mode: 33 RPM turntable with grooves, 60fps spinning label & spring tonearm
// -----------------------------------------------------------------------------

fn vinyl_mode(store: AppStore, size_pt: f32) -> View {
    let w = (size_pt * 1.16).max(size_pt + 28.0);
    let h = size_pt + 16.0;
    let cx = w * 0.44;
    let cy = h * 0.52;
    let disc_r = (size_pt * 0.45).min(cx - 10.0).min(cy - 8.0);
    let label_r = disc_r * 0.36;

    let (enter_opacity, enter_scale) = mode_entrance_animations();
    let spin_deg = smooth_spin_signal(store, VINYL_DEG_PER_FRAME);

    let initial_playing = untracked(|| store.is_playing.get());
    let tonearm_engaged = animated_value(if initial_playing { 1.0 } else { 0.0 });
    let prev_playing = stored(initial_playing);

    effect(move || {
        let playing = store.is_playing.get();
        if prev_playing.replace(playing) != playing {
            motion::animate_spring(
                &tonearm_engaged,
                if playing { 1.0 } else { 0.0 },
                motion::spring_gentle(),
            );
        }
    });

    // Tonearm angle interpolated on UI thread (radians clockwise from vertical down):
    // 0.0 (parked) -> -0.14 rad (resting on cradle); 1.0 (engaged) -> 0.34 rad (stylus on grooves)
    let tonearm_angle = tonearm_engaged.interpolate(&[0.0, 1.0], &[-0.14, 0.34]);

    let spin_radians = move || {
        if reduce_motion() {
            0.0_f32
        } else {
            spin_deg.get().to_radians()
        }
    };

    view()
        .w(px(w))
        .h(px(h))
        .relative()
        .bind_opacity(&enter_opacity)
        .bind_scale(&enter_scale)
        // Platter, grooves, specular sheen, spinning center label backdrop
        .child(
            canvas(move || {
                let accent = store.current_accent.get();
                let seed = current_track_seed(store);
                let angle = spin_radians();
                let playing = store.is_playing.get();
                paint_vinyl_platter(w, h, cx, cy, disc_r, label_r, accent, &seed, angle, playing)
            })
            .absolute()
            .inset_0()
            .size_full()
            .pointer_events(PointerEvents::None),
        )
        // Spinning circular artwork label overlay (rotates smoothly at 60fps in sync with platter)
        .child(
            view()
                .absolute()
                .left(px(cx - label_r))
                .top(px(cy - label_r))
                .size(px(label_r * 2.0))
                .rounded_full()
                .overflow_hidden()
                .border_1()
                .border_color(theme::np_border)
                .rotate(move || Radians(spin_radians()))
                .pointer_events(PointerEvents::None)
                .child(when(
                    move || has_renderable_art(&store.current_artwork_uri.get()),
                    move || {
                        image(move || store.current_artwork_uri.get())
                            .object_fit(ResizeMode::Cover)
                            .size_full()
                    },
                )),
        )
        // Foreground spindle cap + spring-animated tonearm drawn ABOVE the rotating label
        .child(
            canvas(move || {
                let playing = store.is_playing.get();
                let accent = store.current_accent.get();
                paint_vinyl_spindle_and_tonearm(
                    w,
                    h,
                    cx,
                    cy,
                    disc_r,
                    accent,
                    playing,
                    &tonearm_angle,
                )
            })
            .absolute()
            .inset_0()
            .size_full()
            .pointer_events(PointerEvents::None),
        )
}

#[allow(clippy::too_many_arguments)]
fn paint_vinyl_platter(
    w: f32,
    h: f32,
    cx: f32,
    cy: f32,
    disc_r: f32,
    label_r: f32,
    accent: Hsla,
    seed: &str,
    angle: f32,
    playing: bool,
) -> Painter {
    let stage_dark = color(theme::np_bg());
    let fg = color(theme::np_fg());
    let amber = color(theme::amber());
    let (grad_a, grad_b) = theme::fallback_cover_gradient(seed);

    let plinth_pad = 4.0;
    let plinth_bounds = Bounds::new(
        point(px(plinth_pad), px(plinth_pad)),
        size(px(w - plinth_pad * 2.0), px(h - plinth_pad * 2.0)),
    );

    // Subtle dark brushed turntable plinth deck
    let plinth_fill = CanvasGradient::linear(point(px(0.0), px(0.0)), point(px(w), px(h)))
        .add_color_stop(percentage(0.0), stage_dark.lighten(0.14).alpha(0.72))
        .add_color_stop(percentage(1.0), stage_dark.alpha(0.86));

    let mut p = Painter::new()
        .fill_style(plinth_fill)
        .begin_path()
        .round_rect(plinth_bounds, px(radius::XL))
        .fill()
        .stroke_color(fg.alpha(0.12))
        .line_width(1.0)
        .begin_path()
        .round_rect(plinth_bounds, px(radius::XL))
        .stroke();

    // Aluminum platter base ring + strobe speed dots
    let platter_r = disc_r + 6.0;
    p = p
        .fill_color(stage_dark.lighten(0.22).alpha(0.92))
        .begin_path()
        .arc(cx, cy, platter_r, 0.0, TAU)
        .fill()
        .stroke_color(fg.alpha(0.22))
        .line_width(1.5)
        .begin_path()
        .arc(cx, cy, platter_r, 0.0, TAU)
        .stroke();

    let dot_count: usize = 36;
    p = p.fill_color(fg.alpha(0.32));
    for i in 0..dot_count {
        let theta = (i as f32 / dot_count as f32) * TAU + angle;
        let (sin, cos) = theta.sin_cos();
        let rx = cx + cos * (platter_r - 2.6);
        let ry = cy + sin * (platter_r - 2.6);
        p = p.begin_path().arc(rx, ry, 1.0, 0.0, TAU).fill();
    }

    // Main vinyl record body with subtle radial sheen
    let vinyl_grad = CanvasGradient::radial(
        point(px(cx - disc_r * 0.22), px(cy - disc_r * 0.22)),
        px(label_r * 0.6),
        point(px(cx), px(cy)),
        px(disc_r),
    )
    .add_color_stop(percentage(0.0), stage_dark.lighten(0.18))
    .add_color_stop(percentage(0.58), stage_dark.lighten(0.06))
    .add_color_stop(percentage(1.0), stage_dark);

    p = p
        .fill_style(vinyl_grad)
        .begin_path()
        .arc(cx, cy, disc_r, 0.0, TAU)
        .fill()
        .stroke_color(fg.alpha(0.16))
        .line_width(1.2)
        .begin_path()
        .arc(cx, cy, disc_r - 0.8, 0.0, TAU)
        .stroke();

    // Concentric micro-grooves across the playable band
    let groove_start = label_r + 8.0;
    let groove_end = disc_r - 5.0;
    let groove_count: usize = 18;
    for i in 0..groove_count {
        let t = i as f32 / (groove_count - 1).max(1) as f32;
        let r = groove_start + (groove_end - groove_start) * t;
        // Subtle wider track separator bands at 32% and 68%
        let is_break = i == 5 || i == 12;
        let alpha = if is_break {
            0.14
        } else if i.is_multiple_of(2) {
            0.075
        } else {
            0.035
        };
        let width = if is_break { 1.2 } else { 0.65 };
        p = p
            .stroke_color(fg.alpha(alpha))
            .line_width(width)
            .begin_path()
            .arc(cx, cy, r, 0.0, TAU)
            .stroke();
    }

    // Specular diagonal light reflection across the vinyl grooves (fixed to studio lamp)
    p = p.save().translate(cx, cy);
    for offset in [0.0_f32, PI] {
        let a0 = -0.82 + offset;
        let a1 = -0.28 + offset;
        let sheen = CanvasGradient::linear(
            point(px(a0.cos() * disc_r), px(a0.sin() * disc_r)),
            point(px(a1.cos() * disc_r), px(a1.sin() * disc_r)),
        )
        .add_color_stop(percentage(0.0), fg.alpha(0.0))
        .add_color_stop(percentage(0.5), fg.alpha(0.14))
        .add_color_stop(percentage(1.0), fg.alpha(0.0));

        p = p
            .fill_style(sheen)
            .begin_path()
            .move_to(0.0, 0.0)
            .arc(0.0, 0.0, disc_r - 1.5, a0, a1)
            .close_path()
            .fill();
    }
    p = p.restore();

    // Spinning center label (visible natively AND beneath circular artwork image)
    p = p.save().translate(cx, cy).rotate(angle);
    let label_grad = CanvasGradient::linear(
        point(px(-label_r), px(-label_r)),
        point(px(label_r), px(label_r)),
    )
    .add_color_stop(percentage(0.0), grad_a.mix(accent, 0.45))
    .add_color_stop(percentage(1.0), grad_b);

    p = p
        .fill_style(label_grad)
        .begin_path()
        .arc(0.0, 0.0, label_r, 0.0, TAU)
        .fill()
        .stroke_color(fg.alpha(0.32))
        .line_width(1.2)
        .begin_path()
        .arc(0.0, 0.0, label_r - 4.5, 0.0, TAU)
        .stroke()
        // Subtle rotation cue notch on the label ring so 33 RPM motion is unmistakable
        .stroke_color(fg.alpha(0.75))
        .line_width(2.0)
        .line_cap(LineCap::Round)
        .begin_path()
        .arc(0.0, 0.0, label_r - 8.5, -0.45, 0.45)
        .stroke()
        .fill_color(fg.alpha(0.85))
        .font("700 8px sans-serif")
        .text_align(CanvasTextAlign::Center)
        .text_baseline(CanvasTextBaseline::Middle)
        .fill_text("33⅓ RPM", 0.0, -label_r * 0.42)
        .fill_text("SIDE A", 0.0, label_r * 0.46)
        .restore();

    // Power / speed indicator LED at bottom-left corner of plinth
    let led_x = 18.0;
    let led_y = h - 18.0;
    let led_col = if playing { amber } else { fg.alpha(0.24) };
    p = p
        .fill_color(led_col.alpha(if playing { 0.28 } else { 0.0 }))
        .begin_path()
        .arc(led_x, led_y, 7.0, 0.0, TAU)
        .fill()
        .fill_color(led_col)
        .begin_path()
        .arc(led_x, led_y, 2.8, 0.0, TAU)
        .fill();

    p
}

#[allow(clippy::too_many_arguments)]
fn paint_vinyl_spindle_and_tonearm(
    w: f32,
    h: f32,
    cx: f32,
    cy: f32,
    disc_r: f32,
    accent: Hsla,
    playing: bool,
    tonearm_angle: &AnimatedNode,
) -> Painter {
    let fg = color(theme::np_fg());
    let dark = color(theme::np_bg());
    let primary = color(theme::primary());
    let muted = color(theme::np_muted());

    // Center brushed metal spindle pin drawn over the artwork label
    let mut p = Painter::new()
        .fill_color(dark.alpha(0.85))
        .begin_path()
        .arc(cx, cy, 7.0, 0.0, TAU)
        .fill()
        .fill_color(fg.alpha(0.88))
        .begin_path()
        .arc(cx, cy, 3.6, 0.0, TAU)
        .fill()
        .fill_color(dark.alpha(0.55))
        .begin_path()
        .arc(cx - 0.8, cy - 0.8, 1.2, 0.0, TAU)
        .fill();

    // Tonearm gimbal pivot in upper-right corner of the plinth
    let pivot_x = (w - 28.0).max(cx + disc_r * 0.72);
    let pivot_y = (cy - disc_r * 0.68).max(28.0);
    let arm_len = (h * 0.56).min(disc_r * 1.18);

    // Armrest dock marker beneath the parked arm position
    let rest_x = pivot_x - 3.0;
    let rest_y = pivot_y + arm_len * 0.68;
    p = p
        .fill_color(dark.lighten(0.18).alpha(0.9))
        .stroke_color(fg.alpha(0.24))
        .line_width(1.2)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(rest_x - 5.0), px(rest_y - 3.0)),
                size(px(10.0), px(6.0)),
            ),
            px(2.5),
        )
        .fill()
        .stroke();

    // Static gimbal base plate
    p = p
        .fill_color(dark.lighten(0.16))
        .stroke_color(fg.alpha(0.32))
        .line_width(1.6)
        .begin_path()
        .arc(pivot_x, pivot_y, 17.0, 0.0, TAU)
        .fill()
        .stroke()
        .stroke_color(accent.lighten(0.24).alpha(0.45))
        .line_width(1.0)
        .begin_path()
        .arc(pivot_x, pivot_y, 12.0, 0.0, TAU)
        .stroke();

    // Animated rotating tonearm assembly (runs smoothly on UI thread via AnimatedNode!)
    p = p
        .save()
        .translate(pivot_x, pivot_y)
        .rotate(tonearm_angle)
        // Rear counterweight cylinder
        .fill_color(fg.alpha(0.55))
        .stroke_color(dark.alpha(0.75))
        .line_width(1.0)
        .begin_path()
        .round_rect(
            Bounds::new(point(px(-5.5), px(-22.0)), size(px(11.0), px(14.0))),
            px(3.0),
        )
        .fill()
        .stroke()
        // Main S-shaped / angled brushed aluminum tonearm shaft (dark drop edge + metallic highlight)
        .line_cap(LineCap::Round)
        .line_join(LineJoin::Round)
        .stroke_color(dark.alpha(0.60))
        .line_width(6.2)
        .begin_path()
        .move_to(0.0, -8.0)
        .line_to(0.0, arm_len * 0.68)
        .line_to(-14.0, arm_len * 0.92)
        .stroke()
        .stroke_color(fg.alpha(0.84))
        .line_width(3.6)
        .begin_path()
        .move_to(0.0, -8.0)
        .line_to(0.0, arm_len * 0.68)
        .line_to(-14.0, arm_len * 0.92)
        .stroke()
        // Headshell cartridge housing
        .save()
        .translate(-14.0, arm_len * 0.92)
        .rotate(0.42)
        .fill_color(dark.lighten(0.22))
        .stroke_color(fg.alpha(0.55))
        .line_width(1.2)
        .begin_path()
        .round_rect(
            Bounds::new(point(px(-6.0), px(-3.0)), size(px(12.0), px(22.0))),
            px(3.0),
        )
        .fill()
        .stroke()
        // Finger lift wire
        .stroke_color(fg.alpha(0.72))
        .line_width(1.4)
        .begin_path()
        .move_to(6.0, 5.0)
        .line_to(12.0, 8.0)
        .stroke()
        // Stylus tip indicator dot
        .fill_color(if playing { primary } else { muted })
        .begin_path()
        .arc(0.0, 16.0, 2.2, 0.0, TAU)
        .fill()
        .restore()
        // Pivot top cap jewel
        .fill_color(fg.alpha(0.9))
        .begin_path()
        .arc(0.0, 0.0, 5.0, 0.0, TAU)
        .fill()
        .fill_color(dark.lighten(0.18))
        .begin_path()
        .arc(0.0, 0.0, 2.2, 0.0, TAU)
        .fill()
        .restore();

    p
}

// -----------------------------------------------------------------------------
// 3. Cassette Mode: Retro cassette deck with 60fps twin reels, tape window & label strip
// -----------------------------------------------------------------------------

fn cassette_mode(store: AppStore, size_pt: f32) -> View {
    let w = (size_pt * 1.18).max(size_pt + 28.0);
    let h = (w * 0.66).min(size_pt + 12.0);
    let (enter_opacity, enter_scale) = mode_entrance_animations();
    let spin_deg = smooth_spin_signal(store, CASSETTE_DEG_PER_FRAME);

    view()
        .w(px(w))
        .h(px(h))
        .items_center()
        .justify_center()
        .bind_opacity(&enter_opacity)
        .bind_scale(&enter_scale)
        .child(
            canvas(move || {
                let accent = store.current_accent.get();
                let playing = store.is_playing.get();
                let pos_ms = store.position_ms.get();
                let dur_ms = store.duration_ms.get().max(1);
                let progress = (pos_ms as f32 / dur_ms as f32).clamp(0.0, 1.0);
                let reduced = reduce_motion();

                let unknown_artist = store.t("track.unknown_artist").to_string();
                let (title, artist) = store.current_track.with(|t| {
                    t.as_ref()
                        .map(|tr| {
                            let names = tr.artist_names();
                            let artist_str = if names.is_empty() {
                                unknown_artist.clone()
                            } else {
                                names
                            };
                            (tr.name.clone(), artist_str)
                        })
                        .unwrap_or_else(|| ("NSH Music".to_owned(), "Side A".to_owned()))
                });

                let vu_level = if playing && !reduced {
                    let s = store.spectrum.get();
                    ((s[1] + s[3] + s[5] + s[7]) * 0.25).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                let reel_angle = if reduced {
                    0.0
                } else {
                    spin_deg.get().to_radians()
                };

                paint_cassette_deck(
                    w,
                    h,
                    accent,
                    &title,
                    &artist,
                    progress,
                    reel_angle,
                    vu_level,
                    playing,
                )
            })
            .size_full()
            .pointer_events(PointerEvents::None),
        )
}

#[allow(clippy::too_many_arguments)]
fn paint_cassette_deck(
    w: f32,
    h: f32,
    accent: Hsla,
    title: &str,
    artist: &str,
    progress: f32,
    reel_angle: f32,
    vu_level: f32,
    playing: bool,
) -> Painter {
    let dark = color(theme::np_bg());
    let fg = color(theme::np_fg());
    let primary = color(theme::primary());
    let amber = color(theme::amber());

    let pad = 6.0;
    let cw = w - pad * 2.0;
    let ch = h - pad * 2.0;
    let shell_rect = Bounds::new(point(px(pad), px(pad)), size(px(cw), px(ch)));

    // Translucent smoky cassette shell gradient
    let shell_grad = CanvasGradient::linear(point(px(pad), px(pad)), point(px(pad), px(pad + ch)))
        .add_color_stop(percentage(0.0), dark.lighten(0.22).alpha(0.95))
        .add_color_stop(percentage(0.55), dark.lighten(0.12).alpha(0.96))
        .add_color_stop(percentage(1.0), dark.lighten(0.06).alpha(0.98));

    let mut p = Painter::new()
        .fill_style(shell_grad)
        .begin_path()
        .round_rect(shell_rect, px(14.0))
        .fill()
        .stroke_color(fg.alpha(0.22))
        .line_width(1.5)
        .begin_path()
        .round_rect(shell_rect, px(14.0))
        .stroke();

    // Inner recessed bevel frame
    let inner_rect = Bounds::new(
        point(px(pad + 6.0), px(pad + 6.0)),
        size(px(cw - 12.0), px(ch - 12.0)),
    );
    p = p
        .stroke_color(fg.alpha(0.08))
        .line_width(1.0)
        .begin_path()
        .round_rect(inner_rect, px(10.0))
        .stroke();

    // 4 Corner cassette screws
    let screw_inset = 13.0;
    for (sx, sy) in [
        (pad + screw_inset, pad + screw_inset),
        (pad + cw - screw_inset, pad + screw_inset),
        (pad + screw_inset, pad + ch - screw_inset),
        (pad + cw - screw_inset, pad + ch - screw_inset),
    ] {
        p = p
            .fill_color(dark.lighten(0.28))
            .stroke_color(fg.alpha(0.32))
            .line_width(0.9)
            .begin_path()
            .arc(sx, sy, 4.0, 0.0, TAU)
            .fill()
            .stroke()
            .stroke_color(dark.alpha(0.7))
            .line_width(1.0)
            .begin_path()
            .move_to(sx - 2.0, sy)
            .line_to(sx + 2.0, sy)
            .move_to(sx, sy - 2.0)
            .line_to(sx, sy + 2.0)
            .stroke();
    }

    // Track Label Strip (upper 66% body with warm accent band + ruled paper top)
    let label_x = pad + 18.0;
    let label_y = pad + 16.0;
    let label_w = cw - 36.0;
    let label_h = ch * 0.62;
    let label_rect = Bounds::new(
        point(px(label_x), px(label_y)),
        size(px(label_w), px(label_h)),
    );

    let strip_grad = CanvasGradient::linear(
        point(px(label_x), px(label_y)),
        point(px(label_x + label_w), px(label_y + label_h)),
    )
    .add_color_stop(percentage(0.0), accent.lighten(0.18).saturate(0.12))
    .add_color_stop(percentage(1.0), accent.darken(0.22));

    p = p
        .fill_style(strip_grad)
        .begin_path()
        .round_rect(label_rect, px(8.0))
        .fill()
        .stroke_color(fg.alpha(0.22))
        .line_width(1.0)
        .begin_path()
        .round_rect(label_rect, px(8.0))
        .stroke();

    // Cream/porcelain upper title writing strip inside label
    let header_h = (label_h * 0.34).max(30.0);
    let header_rect = Bounds::new(
        point(px(label_x + 4.0), px(label_y + 4.0)),
        size(px(label_w - 8.0), px(header_h)),
    );
    p = p
        .fill_color(fg.alpha(0.90))
        .begin_path()
        .round_rect(header_rect, px(5.0))
        .fill()
        // Subtle ruled writing line
        .stroke_color(dark.alpha(0.16))
        .line_width(0.8)
        .begin_path()
        .move_to(label_x + 12.0, label_y + 4.0 + header_h * 0.56)
        .line_to(label_x + label_w - 12.0, label_y + 4.0 + header_h * 0.56)
        .stroke();

    // Side A badge + track title & artist printed on the label strip
    let badge_rect = Bounds::new(
        point(px(label_x + 10.0), px(label_y + 9.0)),
        size(px(22.0), px(18.0)),
    );
    p = p
        .fill_color(dark.lighten(0.10))
        .begin_path()
        .round_rect(badge_rect, px(3.5))
        .fill()
        .fill_color(fg)
        .font("700 10px sans-serif")
        .text_align(CanvasTextAlign::Center)
        .text_baseline(CanvasTextBaseline::Middle)
        .fill_text("A", label_x + 21.0, label_y + 18.0)
        // Track title
        .fill_color(dark.lighten(0.08))
        .font("700 11px sans-serif")
        .text_align(CanvasTextAlign::Left)
        .text_baseline(CanvasTextBaseline::Middle)
        .fill_text_max_width(
            title,
            label_x + 38.0,
            label_y + 4.0 + header_h * 0.32,
            px((label_w - 54.0).max(40.0)),
        )
        // Artist subtitle
        .fill_color(dark.lighten(0.28))
        .font("500 9px sans-serif")
        .fill_text_max_width(
            artist,
            label_x + 38.0,
            label_y + 4.0 + header_h * 0.76,
            px((label_w - 54.0).max(40.0)),
        );

    // Central transparent tape window cut-out
    let win_w = label_w * 0.76;
    let win_h = (label_h - header_h - 16.0).max(38.0);
    let win_x = label_x + (label_w - win_w) * 0.5;
    let win_y = label_y + header_h + 10.0;
    let win_rect = Bounds::new(point(px(win_x), px(win_y)), size(px(win_w), px(win_h)));

    p = p
        .fill_color(dark.alpha(0.92))
        .stroke_color(fg.alpha(0.24))
        .line_width(1.2)
        .begin_path()
        .round_rect(win_rect, px(win_h * 0.48))
        .fill()
        .stroke();

    // Clip to tape window so tape packs stay cleanly inside window aperture
    p = p
        .save()
        .begin_path()
        .round_rect(win_rect, px(win_h * 0.48))
        .clip();

    let reel_cy = win_y + win_h * 0.5;
    let left_rx = win_x + win_w * 0.24;
    let right_rx = win_x + win_w * 0.76;
    let hub_r = (win_h * 0.30).clamp(10.0, 18.0);
    let max_tape_r = (win_h * 0.62).max(hub_r + 14.0);

    // Tape spool fill reflects playback progress:
    // left supply reel unwinds (1 - progress), right take-up reel fills (progress)
    let left_tape_r = hub_r + 3.0 + (max_tape_r - hub_r - 3.0) * (1.0 - progress * 0.84);
    let right_tape_r = hub_r + 3.0 + (max_tape_r - hub_r - 3.0) * (0.16 + progress * 0.84);

    let tape_col = dark.lighten(0.18).mix(amber, 0.14);
    let tape_edge = fg.alpha(0.14);

    // Left & right magnetic tape rolls
    p = p
        .fill_color(tape_col)
        .stroke_color(tape_edge)
        .line_width(1.0)
        .begin_path()
        .arc(left_rx, reel_cy, left_tape_r, 0.0, TAU)
        .fill()
        .stroke()
        .begin_path()
        .arc(right_rx, reel_cy, right_tape_r, 0.0, TAU)
        .fill()
        .stroke()
        // Subtle tape layer rings on each spool
        .stroke_color(fg.alpha(0.06))
        .line_width(0.8)
        .begin_path()
        .arc(left_rx, reel_cy, (hub_r + left_tape_r) * 0.5, 0.0, TAU)
        .stroke()
        .begin_path()
        .arc(right_rx, reel_cy, (hub_r + right_tape_r) * 0.5, 0.0, TAU)
        .stroke();

    // Central tape bridge gauge window with progress bar
    let gauge_w = (right_rx - left_rx - hub_r * 2.2).max(24.0);
    let gauge_x = (left_rx + right_rx) * 0.5 - gauge_w * 0.5;
    let gauge_y = reel_cy - 6.0;
    let gauge_rect = Bounds::new(point(px(gauge_x), px(gauge_y)), size(px(gauge_w), px(12.0)));
    p = p
        .fill_color(dark.alpha(0.75))
        .stroke_color(fg.alpha(0.16))
        .line_width(0.8)
        .begin_path()
        .round_rect(gauge_rect, px(3.0))
        .fill()
        .stroke()
        // Progress fill inside tape window
        .fill_color(primary.alpha(0.82))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(gauge_x + 2.0), px(gauge_y + 3.0)),
                size(px(((gauge_w - 4.0) * progress).max(2.0)), px(6.0)),
            ),
            px(2.0),
        )
        .fill();

    // Scale tick marks across central window
    p = p.stroke_color(fg.alpha(0.28)).line_width(0.8);
    for tick in 0..=4 {
        let tx = gauge_x + (gauge_w * tick as f32 / 4.0);
        p = p
            .begin_path()
            .move_to(tx, gauge_y - 4.0)
            .line_to(tx, gauge_y - 1.0)
            .stroke();
    }

    // Twin rotating 6-tooth reel hubs (right reel turns slightly different speed proportionally)
    p = draw_tape_hub(p, left_rx, reel_cy, hub_r, reel_angle, fg, dark);
    p = draw_tape_hub(p, right_rx, reel_cy, hub_r, reel_angle * 1.12, fg, dark);

    p = p.restore(); // end tape window clip

    // Bottom magnetic head trapezoid bay
    let bay_w = cw * 0.58;
    let bay_h = ch * 0.21;
    let bay_x = pad + (cw - bay_w) * 0.5;
    let bay_y = pad + ch - bay_h;
    p = p
        .fill_color(dark.lighten(0.16))
        .stroke_color(fg.alpha(0.20))
        .line_width(1.2)
        .begin_path()
        .move_to(bay_x + 12.0, bay_y)
        .line_to(bay_x + bay_w - 12.0, bay_y)
        .line_to(bay_x + bay_w, bay_y + bay_h)
        .line_to(bay_x, bay_y + bay_h)
        .close_path()
        .fill()
        .stroke();

    // Capstan & guide holes inside bottom trapezoid bay
    let hole_y = bay_y + bay_h * 0.55;
    for &(hx, r) in &[
        (bay_x + bay_w * 0.22, 4.2),
        (bay_x + bay_w * 0.36, 2.8),
        (bay_x + bay_w * 0.64, 2.8),
        (bay_x + bay_w * 0.78, 4.2),
    ] {
        p = p
            .fill_color(dark)
            .stroke_color(fg.alpha(0.24))
            .line_width(1.0)
            .begin_path()
            .arc(hx, hole_y, r, 0.0, TAU)
            .fill()
            .stroke();
    }

    // Subtle analog VU meter LEDs flanking the bottom bay
    let led_count: usize = 4;
    for i in 0..led_count {
        let threshold = (i as f32 + 0.5) / led_count as f32;
        let lit = playing && vu_level >= threshold * 0.72;
        let col = if lit {
            if i == led_count - 1 { primary } else { amber }
        } else {
            fg.alpha(0.14)
        };
        let lx = pad + 24.0 + i as f32 * 8.0;
        let rx = pad + cw - 24.0 - i as f32 * 8.0;
        let ly = bay_y + bay_h * 0.55;
        p = p
            .fill_color(col)
            .begin_path()
            .arc(lx, ly, 2.2, 0.0, TAU)
            .fill()
            .begin_path()
            .arc(rx, ly, 2.2, 0.0, TAU)
            .fill();
    }

    p
}

fn draw_tape_hub(
    p: Painter,
    cx: f32,
    cy: f32,
    r: f32,
    angle: f32,
    fg: Hsla,
    dark: Hsla,
) -> Painter {
    let mut p = p
        .save()
        .translate(cx, cy)
        .rotate(angle)
        // Outer white/silver hub collar
        .fill_color(fg.alpha(0.88))
        .begin_path()
        .arc(0.0, 0.0, r, 0.0, TAU)
        .fill()
        // Inner dark spindle hole
        .fill_color(dark)
        .begin_path()
        .arc(0.0, 0.0, r * 0.58, 0.0, TAU)
        .fill()
        // 6 Hub sprocket teeth
        .stroke_color(fg.alpha(0.92))
        .line_width(2.2)
        .line_cap(LineCap::Round);

    for i in 0..6 {
        let theta = (i as f32 / 6.0) * TAU;
        let (sin, cos) = theta.sin_cos();
        p = p
            .begin_path()
            .move_to(cos * (r * 0.34), sin * (r * 0.34))
            .line_to(cos * (r * 0.64), sin * (r * 0.64))
            .stroke();
    }

    p.restore()
}
