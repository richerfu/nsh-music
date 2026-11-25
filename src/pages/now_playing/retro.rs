//! Three selectable centerpiece artwork presentations (`NowPlayingMode`)
//! aligned 1:1 with `egoist/mygo-music` (`ui_cover.go`, `ui_retro.go`, `retro_render.go`):
//! - `Cover`: Elevated `16pt`-rounded album art with breathing scale (`1.0` playing /
//!   `0.88` paused), subtle hover lift/3D tilt, and pointer-tracking glare
//! - `Vinyl`: Square `size × size` turntable deck with concentric grooves, spinning
//!   30fps cover label, strobe red lamp, `START / 33 / 45` indicators, and track-progress-
//!   driven tonearm sweeping smoothly from outer groove to inner label
//! - `Cassette`: Retro cassette shell with printed cover thumbnail, italic handwritten
//!   label strip, physically area-conserving wound magnetic tape spools
//!   (`sqrt(hub^2 + (full^2 - hub^2) * f)`), and twin 30fps rotating hubs
//!
//! Performance: spin timers run at 30fps ONLY while `now_playing_open && is_playing
//! && !reduce_motion()`, stopping cleanly when paused so idle CPU usage is 0%.

use std::{
    f32::consts::{FRAC_PI_2, PI, TAU},
    time::Duration,
};

use nsh_core::{
    canvas::{CanvasGradient, CanvasTextAlign, CanvasTextBaseline, LineCap, LineJoin},
    geometry::{percentage, point, size},
    prelude::*,
    time::now,
};

use crate::{
    state::{AppStore, NowPlayingMode},
    ui::{
        icons::{Icon, icon},
        motion,
        theme::{self, Elevated, Elevation, colors},
    },
};

/// 30fps frame interval matching `mygo-music` `retroFrameRate = 30`.
const RETRO_FRAME_MS: u64 = 33;

/// Turntable platter angular speed: 33⅓ RPM = 200°/s (`vinylTurn = 200`).
const VINYL_DEG_PER_SEC: f32 = 200.0;

/// Cassette reel angular speed: 150°/s (`reelTurn = 150`).
const REEL_DEG_PER_SEC: f32 = 150.0;

/// Turntable geometry constants in units of deck width (`retro_render.go:571-587`).
const DECK_CX: f32 = 0.445;
const DECK_CY: f32 = 0.50;
const DECK_PIVOT_X: f32 = 0.835;
const DECK_PIVOT_Y: f32 = 0.185;
const DECK_PLATTER: f32 = 0.415;
const DECK_RECORD: f32 = 0.385;
const DECK_LABEL: f32 = 0.36;
const DECK_REST: f32 = FRAC_PI_2 - 0.08;
const DECK_LAMP_X: f32 = 0.127;
const DECK_LAMP_Y: f32 = 0.818;
const ARM_TUBE: f32 = 0.49;
const HEADSHELL_TURN: f32 = 20.0 * PI / 180.0;
const STYLUS_AT: f32 = 0.10;

/// Cassette geometry constants in units of shell width (`retro_render.go:898-909`).
const TAPE_HEIGHT: f32 = 0.64;
const TAPE_REEL_Y: f32 = 0.315;
const TAPE_REEL_L: f32 = 0.2875;
const TAPE_REEL_R: f32 = 0.7125;
const TAPE_HUB: f32 = 0.055;
const TAPE_FULL: f32 = 0.19;
const TAPE_CORNER: f32 = 0.035;
const TAPE_LABEL_Y0: f32 = 0.045;
const TAPE_LABEL_Y1: f32 = 0.445;

/// Renders the active square centerpiece artwork presentation (`Cover`, `Vinyl`, or `Cassette`).
pub fn retro_centerpiece(store: AppStore, size_pt: f32) -> View {
    view()
        .size(px(size_pt))
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

fn has_renderable_art(uri: &SharedString) -> bool {
    !uri.is_empty()
}

/// Smooth entrance opacity/scale when switching between Cover / Vinyl / Cassette.
fn mode_entrance_animations() -> (AnimatedValue, AnimatedValue) {
    let reduced = reduce_motion();
    let opacity = animated_value(if reduced { 1.0 } else { 0.0 });
    let scale = animated_value(if reduced { 1.0 } else { 0.95 });
    if !reduced {
        motion::animate_timing(&opacity, 1.0, motion::timing_fast());
        motion::animate_spring(&scale, 1.0, motion::spring_gentle());
    }
    (opacity, scale)
}

/// Gated 30fps rotation signal (degrees in `[0, 360)`) that runs a `spawn`
/// timer loop ONLY while `now_playing_open && is_playing && !reduce_motion()`,
/// exiting cleanly on pause/close so idle CPU usage is 0%.
fn gated_spin_signal(store: AppStore, deg_per_sec: f32) -> Signal<f32> {
    let initial_deg = untracked(|| {
        if reduce_motion() {
            0.0_f32
        } else {
            ((store.position_ms.get() as f32 / 1000.0) * deg_per_sec).rem_euclid(360.0)
        }
    });
    let spin_deg = signal(initial_deg);
    let running = stored(false);

    effect(move || {
        let should_run = store.now_playing_open.get() && store.is_playing.get() && !reduce_motion();
        if !should_run {
            return;
        }
        let started = running.with(|r| {
            if *r {
                false
            } else {
                *r = true;
                true
            }
        });
        if started {
            spawn(async move {
                let mut last_tick = now();
                loop {
                    let active = store.now_playing_open.get_untracked()
                        && store.is_playing.get_untracked()
                        && !reduce_motion();
                    if !active {
                        running.with(|r| *r = false);
                        break;
                    }
                    let cur = now();
                    let dt = cur
                        .saturating_duration_since(last_tick)
                        .as_secs_f32()
                        .clamp(0.0, 0.1);
                    last_tick = cur;
                    let next =
                        (spin_deg.try_get().unwrap_or(0.0) + deg_per_sec * dt).rem_euclid(360.0);
                    if !spin_deg.try_set(next) {
                        running.with(|r| *r = false);
                        break;
                    }
                    timer(Duration::from_millis(RETRO_FRAME_MS)).await;
                }
            });
        }
    });

    spin_deg
}

/// Quantized track playback progress in `[0.0, 1.0]` stepped every 0.5% so
/// tonearm & tape spools stay smooth without rebuilding on every audio tick.
fn quantized_progress(store: AppStore) -> Memo<f32> {
    computed(move || {
        let dur = store.duration_ms.get();
        if dur == 0 {
            0.0_f32
        } else {
            let raw = (store.position_ms.get() as f32 / dur as f32).clamp(0.0, 1.0);
            (raw * 200.0).round() / 200.0
        }
    })
}

// -----------------------------------------------------------------------------
// 1. Cover Mode (`ui_cover.go`): Breathing scale, 16pt corners, hover tilt & glare
// -----------------------------------------------------------------------------

fn cover_mode(store: AppStore, size_pt: f32) -> View {
    let initial_playing = untracked(|| store.is_playing.get());
    let (enter_opacity, enter_scale) = mode_entrance_animations();

    let hovered = signal(false);
    let pointer = signal((size_pt * 0.5, size_pt * 0.5));

    let breathe_scale = animated_value(if initial_playing || reduce_motion() {
        1.0
    } else {
        0.88
    });
    let prev_target = stored(if initial_playing || reduce_motion() {
        1.0_f32
    } else {
        0.88_f32
    });

    effect(move || {
        let playing = store.is_playing.get();
        let over = hovered.get();
        let target = if reduce_motion() {
            1.0
        } else {
            let base = if playing { 1.0 } else { 0.88 };
            if over { base * 1.02 } else { base }
        };
        if (prev_target.get() - target).abs() > 0.001 {
            prev_target.replace(target);
            motion::animate_spring(&breathe_scale, target, motion::spring_gentle());
        }
    });

    let has_art = computed(move || has_renderable_art(&store.current_artwork_uri.get()));

    view()
        .size(px(size_pt))
        .items_center()
        .justify_center()
        .bind_opacity(&enter_opacity)
        .bind_scale(&enter_scale)
        .on_mouse_enter(move |ev: &MouseEvent| {
            hovered.set(true);
            pointer.set((ev.position.x.0, ev.position.y.0));
        })
        .on_mouse_move(move |ev: &MouseEvent| {
            if hovered.get_untracked() && !reduce_motion() {
                pointer.set((ev.position.x.0, ev.position.y.0));
            }
        })
        .on_mouse_leave(move |_: &MouseEvent| {
            hovered.set(false);
        })
        .child(
            Elevated::elevation(
                view()
                    .size(px(size_pt))
                    .rounded(px(16.0))
                    .overflow_hidden()
                    .bg(theme::np_surface)
                    .bind_scale(&breathe_scale)
                    .perspective(px(900.0))
                    .rotate_y(move || {
                        if !hovered.get() || reduce_motion() {
                            deg(0.0)
                        } else {
                            let (px_x, _) = pointer.get();
                            let tx = ((px_x / size_pt.max(1.0)) * 2.0 - 1.0).clamp(-1.0, 1.0);
                            deg(tx * 6.0)
                        }
                    })
                    .rotate_x(move || {
                        if !hovered.get() || reduce_motion() {
                            deg(0.0)
                        } else {
                            let (_, px_y) = pointer.get();
                            let ty = ((px_y / size_pt.max(1.0)) * 2.0 - 1.0).clamp(-1.0, 1.0);
                            deg(-ty * 6.0)
                        }
                    })
                    .transition_transform()
                    .duration(move || motion::transition_ms(220.0))
                    .items_center()
                    .justify_center(),
                Elevation::Modal,
            )
            // Missing cover calm placeholder (avoids flashing disc while remote cover loads)
            .child(when(
                move || !has_art.get(),
                move || {
                    view()
                        .absolute()
                        .inset_0()
                        .bg(theme::np_surface)
                        .border_1()
                        .border_color(move || theme::np_fg().opacity(0.07))
                        .items_center()
                        .justify_center()
                        .child(
                            icon(
                                Icon::Disc,
                                (size_pt * 0.30).clamp(40.0, 88.0),
                                colors::NP_FG,
                            )
                            .opacity(0.32),
                        )
                },
            ))
            // Cover image
            .child(when(has_art, move || {
                image(move || store.current_artwork_uri.get())
                    .object_fit(ResizeMode::Cover)
                    .absolute()
                    .inset_0()
                    .size_full()
            }))
            // Subtle glass hairline rim so dark covers separate cleanly from dark backdrops
            .child(
                view()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.0))
                    .border_1()
                    .border_color(move || theme::np_fg().opacity(0.10))
                    .pointer_events(PointerEvents::None),
            )
            // Soft pointer-tracking card glare (mounted ONLY while hovered & !reduce_motion())
            .child(when(
                move || hovered.get() && !reduce_motion(),
                move || {
                    canvas(move || {
                        let (gx, gy) = pointer.get();
                        paint_cover_glare(size_pt, gx, gy)
                    })
                    .absolute()
                    .inset_0()
                    .size_full()
                    .pointer_events(PointerEvents::None)
                },
            )),
        )
}

fn paint_cover_glare(size_pt: f32, gx: f32, gy: f32) -> Painter {
    let fg = color(theme::np_fg());
    let radius = (size_pt * 0.56).max(60.0);
    let grad = CanvasGradient::radial(
        point(px(gx), px(gy)),
        px(size_pt * 0.04),
        point(px(gx), px(gy)),
        px(radius),
    )
    .add_color_stop(percentage(0.0), fg.alpha(0.24))
    .add_color_stop(percentage(0.45), fg.alpha(0.08))
    .add_color_stop(percentage(1.0), fg.alpha(0.0));

    Painter::new()
        .fill_style(grad)
        .fill_rect(0.0, 0.0, size_pt, size_pt)
}

// -----------------------------------------------------------------------------
// 2. Vinyl Mode (`ui_retro.go` + `retro_render.go`): Square turntable deck
// -----------------------------------------------------------------------------

/// Computes exact tonearm angle (radians, 0 = +X right, PI/2 = +Y down) for
/// `engaged` (`0.0` parked -> `1.0` playing) and track `progress` (`0.0..=1.0`),
/// using the exact law-of-cosines stylus geometry from `mygo-music` `ui_retro.go:3193-3201`.
fn compute_tonearm_rad(engaged: bool, progress: f32) -> f32 {
    if !engaged {
        return DECK_REST;
    }
    let stx = ARM_TUBE - 0.005 + STYLUS_AT * HEADSHELL_TURN.cos();
    let sty = STYLUS_AT * HEADSHELL_TURN.sin();
    let reach = stx.hypot(sty);
    let offset = sty.atan2(stx);
    let dist = (DECK_CX - DECK_PIVOT_X).hypot(DECK_CY - DECK_PIVOT_Y);
    let outer = DECK_RECORD * 0.955;
    let inner = DECK_RECORD * DECK_LABEL * 1.20;
    let needle = outer + (inner - outer) * progress.clamp(0.0, 1.0);
    let cos_v =
        ((reach * reach + dist * dist - needle * needle) / (2.0 * reach * dist)).clamp(-1.0, 1.0);
    let toward = (DECK_CY - DECK_PIVOT_Y).atan2(DECK_CX - DECK_PIVOT_X);
    toward - cos_v.acos() - offset
}

fn vinyl_mode(store: AppStore, size_pt: f32) -> View {
    let s = size_pt;
    let cx = DECK_CX * s;
    let cy = DECK_CY * s;
    let label_r = DECK_RECORD * DECK_LABEL * s;

    let (enter_opacity, enter_scale) = mode_entrance_animations();
    let spin_deg = gated_spin_signal(store, VINYL_DEG_PER_SEC);
    let progress = quantized_progress(store);

    let initial_playing = untracked(|| store.is_playing.get());
    let initial_prog = untracked(|| progress.get());
    let arm_rad = animated_value(compute_tonearm_rad(initial_playing, initial_prog));
    let prev_arm_state = stored((initial_playing, initial_prog));

    effect(move || {
        let playing = store.is_playing.get();
        let prog = progress.get();
        let (was_playing, last_prog) = prev_arm_state.get();
        if playing != was_playing || (playing && (prog - last_prog).abs() >= 0.005) {
            prev_arm_state.replace((playing, prog));
            let target = compute_tonearm_rad(playing, prog);
            if playing != was_playing {
                motion::animate_spring(&arm_rad, target, motion::spring_gentle());
            } else {
                motion::animate_timing(&arm_rad, target, motion::timing_base());
            }
        }
    });

    let has_art = computed(move || has_renderable_art(&store.current_artwork_uri.get()));

    Elevated::elevation(
        view()
            .size(px(s))
            .relative()
            .rounded(px(16.0))
            .overflow_hidden()
            .bind_opacity(&enter_opacity)
            .bind_scale(&enter_scale),
        Elevation::Modal,
    )
    // Layer 1: Static brushed plinth, aluminum platter, strobe dots, vinyl grooves,
    // controls (START / 33 / 45), strobe lamp, anti-skate, cue lever, pitch fader.
    // Redraws ONLY on play/pause or accent change (ZERO 30fps canvas redraws!).
    .child(
        canvas(move || {
            let playing = store.is_playing.get();
            let accent = store.current_accent.get();
            paint_vinyl_deck(s, accent, playing)
        })
        .absolute()
        .inset_0()
        .size_full()
        .pointer_events(PointerEvents::None),
    )
    // Layer 2: Spinning circular cover label (pure GPU rotation at 30fps, zero canvas redraws!)
    .child(
        view()
            .absolute()
            .left(px(cx - label_r))
            .top(px(cy - label_r))
            .size(px(label_r * 2.0))
            .rounded_full()
            .overflow_hidden()
            .bg(move || AdaptiveColor::from(store.current_accent.get()))
            .rotate(move || deg(spin_deg.get()))
            .pointer_events(PointerEvents::None)
            .child(when(
                move || !has_art.get(),
                move || {
                    // Plain label rotation band matching `ui_retro.go:3161`
                    view()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(label_r * 0.84))
                        .h(px(label_r * 0.32))
                        .bg(move || theme::np_fg().opacity(0.22))
                },
            ))
            .child(when(has_art, move || {
                image(move || store.current_artwork_uri.get())
                    .object_fit(ResizeMode::Cover)
                    .size_full()
            })),
    )
    // Layer 3: Matte paper lighting, chrome center spindle, and UI-thread-animated tonearm
    .child(
        canvas(move || {
            let playing = store.is_playing.get();
            paint_vinyl_spindle_and_arm(s, playing, &arm_rad)
        })
        .absolute()
        .inset_0()
        .size_full()
        .pointer_events(PointerEvents::None),
    )
}

fn paint_vinyl_deck(s: f32, accent: Hsla, playing: bool) -> Painter {
    let dark = color(theme::np_bg());
    let fg = color(theme::np_fg());
    let danger = color(theme::danger());
    let primary = color(theme::primary());
    let cx = DECK_CX * s;
    let cy = DECK_CY * s;
    let platter_r = DECK_PLATTER * s;
    let record_r = DECK_RECORD * s;
    let label_r = record_r * DECK_LABEL;

    // Brushed black anodized aluminum plinth
    let plinth_grad = CanvasGradient::linear(point(px(0.0), px(0.0)), point(px(s), px(s)))
        .add_color_stop(percentage(0.0), dark.lighten(0.14))
        .add_color_stop(percentage(0.55), dark.lighten(0.07))
        .add_color_stop(percentage(1.0), dark.lighten(0.03));

    let mut p = Painter::new()
        .fill_style(plinth_grad)
        .fill_rect(0.0, 0.0, s, s)
        .stroke_color(fg.alpha(0.12))
        .line_width(1.0)
        .begin_path()
        .round_rect(
            Bounds::new(point(px(0.5), px(0.5)), size(px(s - 1.0), px(s - 1.0))),
            px(16.0),
        )
        .stroke();

    // Soft platter drop shadow
    let shadow_grad = CanvasGradient::radial(
        point(px(cx + 0.008 * s), px(cy + 0.014 * s)),
        px(platter_r * 0.85),
        point(px(cx + 0.008 * s), px(cy + 0.014 * s)),
        px(platter_r * 1.08),
    )
    .add_color_stop(percentage(0.0), dark.alpha(0.75))
    .add_color_stop(percentage(1.0), dark.alpha(0.0));
    p = p
        .fill_style(shadow_grad)
        .begin_path()
        .arc(cx + 0.008 * s, cy + 0.014 * s, platter_r * 1.08, 0.0, TAU)
        .fill();

    // Brushed aluminum platter rim
    let rim_grad = CanvasGradient::linear(
        point(px(cx - platter_r), px(cy - platter_r)),
        point(px(cx + platter_r), px(cy + platter_r)),
    )
    .add_color_stop(percentage(0.0), fg.alpha(0.58))
    .add_color_stop(percentage(0.5), fg.alpha(0.30))
    .add_color_stop(percentage(1.0), fg.alpha(0.46));

    p = p
        .fill_style(rim_grad)
        .begin_path()
        .arc(cx, cy, platter_r, 0.0, TAU)
        .fill();

    // Strobe speed dots along platter rim
    let dot_count: usize = 60;
    p = p.fill_color(dark.alpha(0.45));
    for i in 0..dot_count {
        let theta = (i as f32 / dot_count as f32) * TAU;
        let (sin, cos) = theta.sin_cos();
        let r_mid = record_r + (platter_r - record_r) * 0.52;
        p = p
            .begin_path()
            .arc(
                cx + cos * r_mid,
                cy + sin * r_mid,
                (0.0036 * s).max(1.0),
                0.0,
                TAU,
            )
            .fill();
    }

    // Rubber mat rim & black vinyl record body
    p = p
        .fill_color(dark.lighten(0.04))
        .begin_path()
        .arc(cx, cy, record_r + 0.004 * s, 0.0, TAU)
        .fill()
        .fill_color(dark.lighten(0.015))
        .begin_path()
        .arc(cx, cy, record_r, 0.0, TAU)
        .fill();

    // Concentric vinyl grooves with subtle track separator bands
    let groove_start = label_r * 1.18;
    let groove_end = record_r * 0.97;
    let groove_count: usize = 22;
    for i in 0..groove_count {
        let t = i as f32 / (groove_count - 1).max(1) as f32;
        let r = groove_start + (groove_end - groove_start) * t;
        let is_track_gap = i == 5 || i == 11 || i == 16;
        let alpha = if is_track_gap {
            0.13
        } else if i.is_multiple_of(2) {
            0.065
        } else {
            0.03
        };
        p = p
            .stroke_color(fg.alpha(alpha))
            .line_width(if is_track_gap { 1.1 } else { 0.6 })
            .begin_path()
            .arc(cx, cy, r, 0.0, TAU)
            .stroke();
    }

    // Diagonal specular light reflection wedges across vinyl grooves (room key light)
    p = p.save().translate(cx, cy);
    for offset in [0.0_f32, PI] {
        let a0 = -1.05 + offset;
        let a1 = -0.35 + offset;
        let sheen = CanvasGradient::linear(
            point(px(a0.cos() * record_r), px(a0.sin() * record_r)),
            point(px(a1.cos() * record_r), px(a1.sin() * record_r)),
        )
        .add_color_stop(percentage(0.0), fg.alpha(0.0))
        .add_color_stop(percentage(0.5), fg.alpha(0.18))
        .add_color_stop(percentage(1.0), fg.alpha(0.0));
        p = p
            .fill_style(sheen)
            .begin_path()
            .move_to(0.0, 0.0)
            .arc(0.0, 0.0, record_r - 1.0, a0, a1)
            .close_path()
            .fill();
    }
    p = p.restore();

    // Tonearm gimbal boss plate (upper-right)
    let px_p = DECK_PIVOT_X * s;
    let py_p = DECK_PIVOT_Y * s;
    p = p
        .fill_color(dark.lighten(0.11))
        .stroke_color(fg.alpha(0.28))
        .line_width(1.2)
        .begin_path()
        .arc(px_p, py_p, 0.088 * s, 0.0, TAU)
        .fill()
        .stroke()
        .fill_color(fg.alpha(0.38))
        .begin_path()
        .arc(px_p, py_p, 0.064 * s, 0.0, TAU)
        .fill()
        .fill_color(dark.lighten(0.05))
        .begin_path()
        .arc(px_p, py_p, 0.044 * s, 0.0, TAU)
        .fill();

    // Armrest dock post, cue lever, anti-skating knob, and pitch fader
    let rest_x = (DECK_PIVOT_X + 0.42 * DECK_REST.cos()) * s;
    let rest_y = (DECK_PIVOT_Y + 0.42 * DECK_REST.sin()) * s;
    p = p
        .fill_color(fg.alpha(0.45))
        .begin_path()
        .arc(rest_x, rest_y, 0.015 * s, 0.0, TAU)
        .fill()
        .fill_color(dark.lighten(0.05))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(rest_x - 0.02 * s), px(rest_y - 0.01 * s)),
                size(px(0.04 * s), px(0.02 * s)),
            ),
            px(0.005 * s),
        )
        .fill()
        // Anti-skate knob
        .fill_color(dark.lighten(0.14))
        .stroke_color(fg.alpha(0.25))
        .line_width(1.0)
        .begin_path()
        .arc(0.94 * s, 0.30 * s, 0.025 * s, 0.0, TAU)
        .fill()
        .stroke()
        // Cue lever
        .stroke_color(fg.alpha(0.55))
        .line_width((0.01 * s).max(2.0))
        .line_cap(LineCap::Round)
        .begin_path()
        .move_to(0.915 * s, 0.50 * s)
        .line_to(0.965 * s, 0.50 * s)
        .stroke()
        // Pitch fader slot & slider cap
        .fill_color(dark)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.922 * s), px(0.80 * s)),
                size(px(0.014 * s), px(0.155 * s)),
            ),
            px(0.006 * s),
        )
        .fill()
        .fill_color(fg.alpha(0.48))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.906 * s), px(0.862 * s)),
                size(px(0.046 * s), px(0.032 * s)),
            ),
            px(0.005 * s),
        )
        .fill();

    // Strobe lamp housing & warm red/coral glow when playing (`ui_retro.go:3180-3188`)
    let lx = DECK_LAMP_X * s;
    let ly = DECK_LAMP_Y * s;
    p = p
        .fill_color(dark.lighten(0.11))
        .begin_path()
        .arc(lx, ly, 0.027 * s, 0.0, TAU)
        .fill()
        .fill_color(danger.darken(0.28))
        .begin_path()
        .arc(lx, ly, 0.015 * s, 0.0, TAU)
        .fill();
    if playing {
        let strobe_col = danger.mix(primary, 0.35);
        let glow = CanvasGradient::radial(
            point(px(lx), px(ly)),
            px(0.0),
            point(px(lx), px(ly)),
            px(0.062 * s),
        )
        .add_color_stop(percentage(0.0), strobe_col.alpha(0.78))
        .add_color_stop(percentage(0.32), strobe_col.alpha(0.26))
        .add_color_stop(percentage(1.0), strobe_col.alpha(0.0));
        p = p
            .fill_style(glow)
            .begin_path()
            .arc(lx, ly, 0.062 * s, 0.0, TAU)
            .fill()
            .fill_color(strobe_col.lighten(0.32))
            .begin_path()
            .arc(lx, ly, 0.009 * s, 0.0, TAU)
            .fill();
    }

    // Bottom-left START / 33 / 45 deck buttons + 33 RPM status LED (`ui_retro.go:3213-3231`)
    p = p
        .fill_color(fg.alpha(0.58))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.035 * s), px(0.905 * s)),
                size(px(0.125 * s), px(0.058 * s)),
            ),
            px(0.01 * s),
        )
        .fill()
        .fill_color(dark.lighten(0.12))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.178 * s), px(0.918 * s)),
                size(px(0.054 * s), px(0.038 * s)),
            ),
            px(0.008 * s),
        )
        .fill()
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.246 * s), px(0.918 * s)),
                size(px(0.054 * s), px(0.038 * s)),
            ),
            px(0.008 * s),
        )
        .fill();

    let btn_font = format!("700 {:.1}px sans-serif", (0.018 * s).max(7.0));
    p = p
        .font(&btn_font)
        .text_align(CanvasTextAlign::Center)
        .text_baseline(CanvasTextBaseline::Middle)
        .fill_color(dark.alpha(0.75))
        .fill_text("START", 0.0975 * s, 0.934 * s)
        .fill_color(fg.alpha(0.78))
        .fill_text("33", 0.205 * s, 0.937 * s)
        .fill_color(fg.alpha(0.45))
        .fill_text("45", 0.273 * s, 0.937 * s);

    // 33 RPM indicator LED above "33" button
    let led_x = 0.205 * s;
    let led_y = 0.904 * s;
    let led_col = if playing {
        danger.mix(accent, 0.15).lighten(0.12)
    } else {
        danger.darken(0.35)
    };
    if playing {
        let led_glow = CanvasGradient::radial(
            point(px(led_x), px(led_y)),
            px(0.0),
            point(px(led_x), px(led_y)),
            px(0.016 * s),
        )
        .add_color_stop(percentage(0.0), led_col.alpha(0.65))
        .add_color_stop(percentage(1.0), led_col.alpha(0.0));
        p = p
            .fill_style(led_glow)
            .begin_path()
            .arc(led_x, led_y, 0.016 * s, 0.0, TAU)
            .fill();
    }
    p.fill_color(led_col)
        .begin_path()
        .arc(led_x, led_y, (0.004 * s).max(1.5), 0.0, TAU)
        .fill()
}

fn paint_vinyl_spindle_and_arm(s: f32, playing: bool, arm_rad: &AnimatedValue) -> Painter {
    let dark = color(theme::np_bg());
    let fg = color(theme::np_fg());
    let danger = color(theme::danger());
    let cx = DECK_CX * s;
    let cy = DECK_CY * s;
    let label_r = DECK_RECORD * DECK_LABEL * s;

    // Matte paper shading & recessed ring over the spinning label
    let paper_grad = CanvasGradient::linear(
        point(px(cx - label_r), px(cy - label_r)),
        point(px(cx + label_r), px(cy + label_r)),
    )
    .add_color_stop(percentage(0.0), fg.alpha(0.11))
    .add_color_stop(percentage(1.0), dark.alpha(0.20));

    let sp = (0.013 * s).max(4.0);
    let mut p = Painter::new()
        .fill_style(paper_grad)
        .begin_path()
        .arc(cx, cy, label_r, 0.0, TAU)
        .fill()
        .stroke_color(dark.alpha(0.38))
        .line_width(1.6)
        .begin_path()
        .arc(cx, cy, label_r - 0.8, 0.0, TAU)
        .stroke()
        // Chrome spindle & shadow
        .fill_color(dark.alpha(0.55))
        .begin_path()
        .arc(cx + sp * 0.35, cy + sp * 0.5, sp * 1.4, 0.0, TAU)
        .fill()
        .fill_color(fg.alpha(0.88))
        .begin_path()
        .arc(cx, cy, sp, 0.0, TAU)
        .fill()
        .fill_color(dark.alpha(0.45))
        .begin_path()
        .arc(cx + sp * 0.2, cy + sp * 0.2, sp * 0.42, 0.0, TAU)
        .fill();

    // Tonearm assembly pivoted at (DECK_PIVOT_X, DECK_PIVOT_Y) along +X in arm frame
    let px_p = DECK_PIVOT_X * s;
    let py_p = DECK_PIVOT_Y * s;
    let tube_len = ARM_TUBE * s;
    let hs_len = (STYLUS_AT + 0.018) * s;

    p = p
        .save()
        .translate(px_p, py_p)
        .rotate(arm_rad)
        // Rear counterweight cylinder
        .fill_color(fg.alpha(0.42))
        .stroke_color(dark.alpha(0.70))
        .line_width(1.0)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(-0.155 * s), px(-0.032 * s)),
                size(px(0.088 * s), px(0.064 * s)),
            ),
            px(0.008 * s),
        )
        .fill()
        .stroke()
        // Tonearm shadow & chrome tube
        .line_cap(LineCap::Round)
        .line_join(LineJoin::Round)
        .stroke_color(dark.alpha(0.45))
        .line_width((0.022 * s).max(4.5))
        .begin_path()
        .move_to(-0.07 * s, 0.01 * s)
        .line_to(tube_len, 0.012 * s)
        .stroke()
        .stroke_color(fg.alpha(0.85))
        .line_width((0.016 * s).max(3.2))
        .begin_path()
        .move_to(-0.07 * s, 0.0)
        .line_to(tube_len, 0.0)
        .stroke()
        // Angled brushed aluminum headshell & cartridge nose
        .save()
        .translate(tube_len - 0.005 * s, 0.0)
        .rotate(HEADSHELL_TURN)
        .fill_color(fg.alpha(0.72))
        .stroke_color(dark.alpha(0.55))
        .line_width(1.0)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(-0.01 * s), px(-0.022 * s)),
                size(px(hs_len * 0.85), px(0.044 * s)),
            ),
            px(0.008 * s),
        )
        .fill()
        .stroke()
        // Finger lift
        .stroke_color(fg.alpha(0.78))
        .line_width(1.5)
        .begin_path()
        .move_to(0.055 * s, -0.02 * s)
        .line_to(0.075 * s, -0.042 * s)
        .stroke()
        // Cartridge body & red stylus tip
        .fill_color(dark.lighten(0.12))
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(hs_len * 0.72), px(-0.014 * s)),
                size(px(0.024 * s), px(0.028 * s)),
            ),
            px(0.004 * s),
        )
        .fill()
        .fill_color(if playing { danger } else { danger.darken(0.28) })
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(hs_len * 0.88), px(-0.008 * s)),
                size(px(0.009 * s), px(0.016 * s)),
            ),
            px(0.003 * s),
        )
        .fill()
        .restore()
        // Gimbal top dome cap
        .fill_color(dark.lighten(0.12))
        .begin_path()
        .arc(0.0, 0.0, 0.038 * s, 0.0, TAU)
        .fill()
        .fill_color(fg.alpha(0.84))
        .begin_path()
        .arc(0.0, 0.0, 0.026 * s, 0.0, TAU)
        .fill()
        .restore();

    p
}

// -----------------------------------------------------------------------------
// 3. Cassette Mode (`ui_retro.go` + `retro_render.go`): Wound spools, cover thumb & italic label
// -----------------------------------------------------------------------------

fn cassette_mode(store: AppStore, size_pt: f32) -> View {
    let w = size_pt;
    let h = size_pt * TAPE_HEIGHT;
    let (enter_opacity, enter_scale) = mode_entrance_animations();
    let spin_deg = gated_spin_signal(store, REEL_DEG_PER_SEC);
    let progress = quantized_progress(store);
    let has_art = computed(move || has_renderable_art(&store.current_artwork_uri.get()));

    let hub_diam = TAPE_HUB * 2.0 * w;
    let left_hub_x = TAPE_REEL_L * w - hub_diam * 0.5;
    let right_hub_x = TAPE_REEL_R * w - hub_diam * 0.5;
    let hub_y = TAPE_REEL_Y * w - hub_diam * 0.5;

    let thumb_size = 0.095 * w;
    let thumb_x = 0.088 * w;
    let thumb_y = 0.262 * w;

    view()
        .size(px(size_pt))
        .items_center()
        .justify_center()
        .bind_opacity(&enter_opacity)
        .bind_scale(&enter_scale)
        .child(
            Elevated::elevation(
                view()
                    .w(px(w))
                    .h(px(h))
                    .relative()
                    .rounded(px(w * TAPE_CORNER))
                    .overflow_hidden(),
                Elevation::Modal,
            )
            // Layer 1: Cassette shell, cream ruled paper label, accent stripes,
            // tape window & physically area-conserving wound tape spools.
            // Redraws ONLY on 0.5% progress steps or accent change (ZERO 30fps redraws!).
            .child(
                canvas(move || {
                    let accent = store.current_accent.get();
                    let prog = progress.get();
                    paint_cassette_shell_and_tape(w, h, accent, prog)
                })
                .absolute()
                .inset_0()
                .size_full()
                .pointer_events(PointerEvents::None),
            )
            // Layer 2: Twin 30fps GPU-rotated 6-tooth reel hubs (zero canvas redraws!)
            .child(cassette_hub_view(left_hub_x, hub_y, hub_diam, spin_deg))
            .child(cassette_hub_view(right_hub_x, hub_y, hub_diam, spin_deg))
            // Layer 3: Cover art thumbnail printed beside the tape window (`ui_retro.go:3303-3309`)
            .child(
                view()
                    .absolute()
                    .left(px(thumb_x))
                    .top(px(thumb_y))
                    .size(px(thumb_size))
                    .overflow_hidden()
                    .bg(move || AdaptiveColor::from(store.current_accent.get()))
                    .border_1()
                    .border_color(move || theme::np_bg().opacity(0.28))
                    .pointer_events(PointerEvents::None)
                    .child(when(has_art, move || {
                        image(move || store.current_artwork_uri.get())
                            .object_fit(ResizeMode::Cover)
                            .size_full()
                    })),
            )
            // Layer 4: Italic pen-written title, artist & bold "A" side mark (`ui_retro.go:3312-3319`)
            .child(
                text(move || {
                    store.current_track.with(|t| {
                        t.as_ref()
                            .map(|tr| SharedString::from(tr.name.clone()))
                            .unwrap_or_else(|| store.t("player.idle_title"))
                    })
                })
                .absolute()
                .top(px(w * 0.094))
                .left(px(w * 0.10))
                .right(px(w * 0.10))
                .number_of_lines(1)
                .text_size(px((w * 0.044).max(11.0)))
                .font_weight(FontWeight::SEMIBOLD)
                .italic()
                .text_color(move || AdaptiveColor::from(color(theme::np_bg()).lighten(0.18)))
                .pointer_events(PointerEvents::None),
            )
            .child(
                text(move || {
                    let unknown = store.t("track.unknown_artist");
                    store.current_track.with(|t| {
                        t.as_ref()
                            .map(|tr| {
                                let names = tr.artist_names();
                                if names.is_empty() {
                                    unknown.clone()
                                } else {
                                    SharedString::from(names)
                                }
                            })
                            .unwrap_or_default()
                    })
                })
                .absolute()
                .top(px(w * 0.164))
                .left(px(w * 0.10))
                .right(px(w * 0.10))
                .number_of_lines(1)
                .text_size(px((w * 0.030).max(9.0)))
                .font_weight(FontWeight::MEDIUM)
                .italic()
                .text_color(move || AdaptiveColor::from(color(theme::np_bg()).lighten(0.28)))
                .pointer_events(PointerEvents::None),
            )
            .child(
                text("A")
                    .absolute()
                    .top(px(w * 0.258))
                    .left(px(w * 0.835))
                    .text_size(px((w * 0.075).max(16.0)))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .text_color(move || AdaptiveColor::from(color(theme::np_bg()).lighten(0.16)))
                    .pointer_events(PointerEvents::None),
            ),
        )
}

fn cassette_hub_view(x: f32, y: f32, diam: f32, spin_deg: Signal<f32>) -> View {
    let r = diam * 0.5;
    view()
        .absolute()
        .left(px(x))
        .top(px(y))
        .size(px(diam))
        .rotate(move || deg(spin_deg.get()))
        .pointer_events(PointerEvents::None)
        .child(
            canvas(move || paint_tape_hub(diam, r))
                .size_full()
                .pointer_events(PointerEvents::None),
        )
}

fn paint_tape_hub(diam: f32, r: f32) -> Painter {
    let fg = color(theme::np_fg());
    let dark = color(theme::np_bg());
    let c = diam * 0.5;

    let mut p = Painter::new()
        .fill_color(fg.alpha(0.90))
        .begin_path()
        .arc(c, c, r, 0.0, TAU)
        .fill()
        .stroke_color(dark.alpha(0.22))
        .line_width(1.0)
        .begin_path()
        .arc(c, c, r * 0.85, 0.0, TAU)
        .stroke()
        .fill_color(dark.lighten(0.04))
        .begin_path()
        .arc(c, c, r * 0.58, 0.0, TAU)
        .fill()
        .stroke_color(fg.alpha(0.92))
        .line_width((r * 0.14).max(1.8))
        .line_cap(LineCap::Round);

    for i in 0..6_usize {
        let theta = (i as f32 / 6.0) * TAU;
        let (sin, cos) = theta.sin_cos();
        p = p
            .begin_path()
            .move_to(c + cos * (r * 0.36), c + sin * (r * 0.36))
            .line_to(c + cos * (r * 0.62), c + sin * (r * 0.62))
            .stroke();
    }
    p
}

fn paint_cassette_shell_and_tape(w: f32, h: f32, accent: Hsla, progress: f32) -> Painter {
    let dark = color(theme::np_bg());
    let fg = color(theme::np_fg());
    let amber = color(theme::amber());

    // Black textured plastic shell
    let shell_grad = CanvasGradient::linear(point(px(0.0), px(0.0)), point(px(w), px(h)))
        .add_color_stop(percentage(0.0), dark.lighten(0.14))
        .add_color_stop(percentage(1.0), dark.lighten(0.05));

    let mut p = Painter::new()
        .fill_style(shell_grad)
        .fill_rect(0.0, 0.0, w, h)
        .stroke_color(fg.alpha(0.14))
        .line_width(1.2)
        .begin_path()
        .round_rect(
            Bounds::new(point(px(0.6), px(0.6)), size(px(w - 1.2), px(h - 1.2))),
            px(w * TAPE_CORNER),
        )
        .stroke();

    // Cream paper label with ruled writing lines & accent footer stripes
    let lx = 0.06 * w;
    let ly = TAPE_LABEL_Y0 * w;
    let lw = 0.88 * w;
    let lh = (TAPE_LABEL_Y1 - TAPE_LABEL_Y0) * w;
    let label_rect = Bounds::new(point(px(lx), px(ly)), size(px(lw), px(lh)));
    let paper = fg.mix(amber, 0.09);

    p = p
        .fill_color(paper)
        .begin_path()
        .round_rect(label_rect, px(0.018 * w))
        .fill()
        // Inner ruled border
        .stroke_color(dark.alpha(0.24))
        .line_width(0.8)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.075 * w), px((TAPE_LABEL_Y0 + 0.015) * w)),
                size(px(0.85 * w), px((TAPE_LABEL_Y1 - TAPE_LABEL_Y0 - 0.03) * w)),
            ),
            px(0.01 * w),
        )
        .stroke()
        // Ruled writing lines
        .stroke_color(dark.alpha(0.20))
        .line_width(0.8)
        .begin_path()
        .move_to(0.09 * w, 0.155 * w)
        .line_to(0.91 * w, 0.155 * w)
        .move_to(0.09 * w, 0.212 * w)
        .line_to(0.91 * w, 0.212 * w)
        .stroke()
        // Twin cover-accent stripes along label foot (`ui_retro.go:3298-3301`)
        .save()
        .begin_path()
        .round_rect(label_rect, px(0.018 * w))
        .clip()
        .fill_color(accent.alpha(0.88))
        .fill_rect(lx, 0.398 * w, lw, 0.020 * w)
        .fill_color(accent.mix(fg, 0.40).alpha(0.88))
        .fill_rect(lx, 0.418 * w, lw, 0.014 * w)
        .restore();

    // Central tape window (`0.20..0.80`, `0.24..0.39`, radius `0.075 * w`)
    let wx = 0.20 * w;
    let wy = 0.24 * w;
    let ww = 0.60 * w;
    let wh = 0.15 * w;
    let win_r = 0.075 * w;
    let win_rect = Bounds::new(point(px(wx), px(wy)), size(px(ww), px(wh)));

    p = p
        .fill_color(dark.lighten(0.03))
        .begin_path()
        .round_rect(win_rect, px(win_r))
        .fill()
        .save()
        .begin_path()
        .round_rect(win_rect, px(win_r))
        .clip();

    // Physically area-conserving magnetic tape spools (`sqrt(hub^2 + (full^2 - hub^2) * f)`)
    let hub = TAPE_HUB * w;
    let full = TAPE_FULL * w;
    let done = progress.clamp(0.0, 1.0);
    let wound = |f: f32| -> f32 { (hub * hub + (full * full - hub * hub) * f).sqrt() };
    let tape_brown = dark.lighten(0.11).mix(amber.darken(0.32), 0.38);

    for (rx_u, f) in [(TAPE_REEL_L, 1.0 - done), (TAPE_REEL_R, done)] {
        let rx = rx_u * w;
        let ry = TAPE_REEL_Y * w;
        let t = wound(f);
        p = p
            .fill_color(dark.alpha(0.55))
            .begin_path()
            .arc(rx + 2.0, ry + 3.0, t + 4.0, 0.0, TAU)
            .fill()
            .fill_color(tape_brown)
            .begin_path()
            .arc(rx, ry, t, 0.0, TAU)
            .fill()
            .stroke_color(dark.alpha(0.50))
            .line_width(1.0)
            .begin_path()
            .arc(rx, ry, t, 0.0, TAU)
            .stroke();
        for k in 1..6_usize {
            let g = hub + (t - hub) * (k as f32 / 6.0);
            let a = if k.is_multiple_of(2) { 0.12 } else { 0.18 };
            p = p
                .stroke_color(dark.alpha(a))
                .line_width(0.6)
                .begin_path()
                .arc(rx, ry, g, 0.0, TAU)
                .stroke();
        }
    }

    // Diagonal glass reflection streak across window
    p = p
        .stroke_color(fg.alpha(0.10))
        .line_width(0.03 * w)
        .begin_path()
        .move_to(wx + ww * 0.18, wy - 4.0)
        .line_to(wx + ww * 0.36, wy + wh + 4.0)
        .stroke()
        .restore()
        .stroke_color(fg.alpha(0.16))
        .line_width(1.0)
        .begin_path()
        .round_rect(win_rect, px(win_r))
        .stroke();

    // Raised bottom trapezoidal head bay (`y = 0.52..0.64`)
    let bay_y = 0.52 * w;
    p = p
        .fill_color(dark.lighten(0.12))
        .stroke_color(fg.alpha(0.14))
        .line_width(1.0)
        .begin_path()
        .move_to(0.235 * w, bay_y)
        .line_to(0.765 * w, bay_y)
        .line_to(0.830 * w, h)
        .line_to(0.170 * w, h)
        .close_path()
        .fill()
        .stroke();

    // Capstan & guide openings + felt pad
    for &(hx, hy, r) in &[
        (0.29 * w, 0.585 * w, 0.020 * w),
        (0.71 * w, 0.585 * w, 0.020 * w),
        (0.42 * w, 0.580 * w, 0.014 * w),
        (0.58 * w, 0.580 * w, 0.014 * w),
    ] {
        p = p
            .fill_color(dark)
            .stroke_color(fg.alpha(0.18))
            .line_width(0.9)
            .begin_path()
            .arc(hx, hy, r, 0.0, TAU)
            .fill()
            .stroke();
    }
    p = p
        .fill_color(dark)
        .begin_path()
        .round_rect(
            Bounds::new(
                point(px(0.45 * w), px(0.60 * w)),
                size(px(0.10 * w), px(0.04 * w)),
            ),
            px(0.004 * w),
        )
        .fill()
        .fill_color(fg.mix(amber, 0.15).alpha(0.65))
        .fill_rect(0.48 * w, 0.602 * w, 0.04 * w, 0.011 * w)
        .fill_color(tape_brown)
        .fill_rect(0.455 * w, 0.614 * w, 0.09 * w, 0.012 * w);

    // 5 cassette screws
    let sr = (0.0125 * w).max(2.6);
    for &(sx, sy) in &[
        (0.04 * w, 0.04 * w),
        (0.96 * w, 0.04 * w),
        (0.04 * w, 0.60 * w),
        (0.96 * w, 0.60 * w),
        (0.50 * w, 0.535 * w),
    ] {
        p = p
            .fill_color(fg.alpha(0.36))
            .stroke_color(dark.alpha(0.65))
            .line_width(0.9)
            .begin_path()
            .arc(sx, sy, sr, 0.0, TAU)
            .fill()
            .stroke()
            .stroke_color(dark.alpha(0.70))
            .line_width(1.0)
            .begin_path()
            .move_to(sx - sr * 0.55, sy)
            .line_to(sx + sr * 0.55, sy)
            .move_to(sx, sy - sr * 0.55)
            .line_to(sx, sy + sr * 0.55)
            .stroke();
    }

    p
}
