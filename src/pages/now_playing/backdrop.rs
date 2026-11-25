//! Ambient reactive backdrop for the Now Playing stage (`egoist/mygo-music`
//! `internal/app/backdrop.go` 1:1 parity).
//!
//! - **Fog mode** (`settings.fog_backdrop && !reduce_motion() && shaders_available()`):
//!   Runs a 1:1 WGSL port of `mygo-music`'s `fogSource` WITHOUT reading `nsh.time`
//!   so `shader.animated()` stays `false` (never waking the 120Hz display link!).
//!   Instead, a gated ~30fps task steps `drift` (easing in on play and gliding to
//!   a stop on pause) and smoothed bass energy into UI-thread `&AnimatedValue`
//!   uniform slots, stopping completely (0% CPU/GPU) once paused and settled.
//! - **Still mode** (or `reduce_motion()` / no GPU):
//!   Renders `mygo-music`'s clean vertical HSV gradient (`artwork_backdrop_colors`)
//!   with a faint blurred cover wash and bottom contrast scrim — zero 60ms redraws.

use std::{sync::OnceLock, time::Duration};

use nsh_core::{prelude::*, shader::Shader, time::now};

use crate::{
    state::AppStore,
    ui::{
        motion,
        theme::{self, colors},
    },
};

/// 1:1 port of `egoist/mygo-music` `backdrop.go` `fogSource` for NSH's
/// `fn shade(uv: vec2f) -> vec4f` contract.
///
/// Intentionally does NOT read `nsh.time` so `Shader::animated()` is `false`:
/// `drift` and `bass` are driven at 30fps via `&AnimatedValue` uniform slots
/// only while playing or gliding to rest, eliminating 120Hz GPU re-rendering
/// and play/pause phase jumps.
pub(crate) const FOG_BACKDROP_WGSL: &str = r#"
struct Uniforms {
    deep: vec4f,
    fog: vec4f,
    drift: f32,
    bass: f32,
    _pad0: f32,
    _pad1: f32,
}

fn hash(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

fn noise(p: vec2f) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (vec2f(3.0) - 2.0 * f);
    let a = hash(i);
    let b = hash(i + vec2f(1.0, 0.0));
    let c = hash(i + vec2f(0.0, 1.0));
    let d = hash(i + vec2f(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p_in: vec2f) -> f32 {
    var p = p_in;
    var v: f32 = 0.0;
    var amp: f32 = 0.5;
    for (var i: i32 = 0; i < 5; i = i + 1) {
        v = v + amp * noise(p);
        p = p * 2.03 + vec2f(1.7, 9.2);
        amp = amp * 0.5;
    }
    return v;
}

fn shade(uv: vec2f) -> vec4f {
    let size = max(nsh.size, vec2f(1.0));
    let pos = uv * size;
    let p = pos / size.y * 2.4;
    let t = uniforms.drift;
    let bass = clamp(uniforms.bass, 0.0, 1.0);
    let rise = vec2f(0.0, t * 0.1);
    let q = vec2f(
        fbm(p + rise + vec2f(0.0, t * 0.04)),
        fbm(p + rise + vec2f(5.2, 1.3) - vec2f(t * 0.05, 0.0))
    );
    let f = fbm(p + rise + 0.9 * q);

    // Broad banks of fog, and the puffs within them.
    let bank = fbm(p * 0.45 + rise * 0.6 + vec2f(3.7, 0.0));
    let cloud = smoothstep(0.3 - 0.06 * bass, 0.78, f)
        * (0.45 + 0.75 * smoothstep(0.3, 0.65, bank));

    // Thickest a little above the middle, thinning toward top & foot controls.
    let m = smoothstep(-0.1, 0.4, uv.y) * (1.0 - 0.5 * smoothstep(0.65, 1.0, uv.y));
    var c = mix(uniforms.deep.rgb, uniforms.fog.rgb, min(cloud * m * (0.85 + 0.35 * bass), 1.0));

    // A broad glow behind the stage, darker at the corners.
    let g = length((uv - vec2f(0.5, 0.42)) * vec2f(1.0, 1.25));
    c = c * (1.08 - 0.45 * smoothstep(0.2, 0.85, g));

    // Specks of dust, rising with the fog, each glinting in its own time.
    let cell = max(size.y / 22.0, 1.0);
    let sp = (pos + vec2f(0.0, t * cell * 0.9)) / cell;
    let id = floor(sp);
    let h = hash(id + vec2f(3.1, 7.7));
    if (h > 0.93) {
        let at = vec2f(
            hash(id + vec2f(1.3, 2.9)),
            hash(id + vec2f(8.1, 4.3))
        ) * 0.8 + vec2f(0.1);
        let d = length((fract(sp) - at) * cell) / max(cell * 0.05, 0.7);
        let glint = 0.5 + 0.5 * sin(t * (1.5 + 2.0 * h) + h * 40.0);
        c = c + vec3f(0.75, 0.82, 1.0) * exp(-d * d) * glint * (0.25 + 0.3 * m);
    }

    return vec4f(clamp(c, vec3f(0.0), vec3f(1.0)), 1.0);
}
"#;

/// Target frame cadence for the moving fog shader (~30fps matching `backdropFrame`).
const FOG_FRAME_MS: u64 = 33;
/// Duration of smooth crossfade between tracks' backdrop hues (`backdropFade = 700ms`).
const BACKDROP_FADE_SECS: f32 = 0.70;

fn compiled_fog_shader() -> Option<Shader> {
    static FOG_SHADER: OnceLock<Option<Shader>> = OnceLock::new();
    FOG_SHADER
        .get_or_init(|| Shader::compile(FOG_BACKDROP_WGSL).ok())
        .clone()
}

struct FogDriver {
    drift: f32,
    speed: f32,
    bass: f32,
    running: bool,
    from_accent: Hsla,
    to_accent: Hsla,
    fade_elapsed: f32,
}

/// Renders the full-bleed ambient backdrop behind Now Playing content.
pub fn ambient_backdrop(store: AppStore) -> View {
    let initial_accent = untracked(|| store.current_accent.get());
    let shown_accent = signal(initial_accent);
    let drift_anim = animated_value(0.0_f32);
    let bass_anim = animated_value(0.0_f32);

    let driver = stored(FogDriver {
        drift: 0.0,
        speed: 0.0,
        bass: 0.0,
        running: false,
        from_accent: initial_accent,
        to_accent: initial_accent,
        fade_elapsed: BACKDROP_FADE_SECS,
    });

    let use_fog_shader = computed(move || {
        shaders_available()
            && store.settings.with(|s| s.fog_backdrop)
            && !reduce_motion()
            && compiled_fog_shader().is_some()
    });

    // Gated 30fps loop: starts ONLY while playing, gliding to a stop, or fading
    // between track accents, and exits completely (0% CPU/GPU) once settled.
    effect(move || {
        let open = store.now_playing_open.get();
        let fog_on = use_fog_shader.get();
        let playing = store.is_playing.get();
        let target_accent = store.current_accent.get();

        let should_start = driver.with(|d| {
            if d.to_accent != target_accent {
                if !fog_on || reduce_motion() {
                    d.from_accent = target_accent;
                    d.to_accent = target_accent;
                    d.fade_elapsed = BACKDROP_FADE_SECS;
                    shown_accent.set(target_accent);
                } else {
                    d.from_accent = shown_accent.get_untracked();
                    d.to_accent = target_accent;
                    d.fade_elapsed = 0.0;
                }
            }

            if !open || !fog_on {
                return false;
            }
            let needs_tick =
                playing || d.speed > 0.0 || d.bass > 0.004 || d.fade_elapsed < BACKDROP_FADE_SECS;
            if needs_tick && !d.running {
                d.running = true;
                true
            } else {
                false
            }
        });

        if should_start {
            spawn(async move {
                let mut last_tick = now();
                loop {
                    let cur_time = now();
                    let dt = cur_time
                        .saturating_duration_since(last_tick)
                        .as_secs_f32()
                        .clamp(1.0 / 60.0, 0.12);
                    last_tick = cur_time;

                    let still_active = store.now_playing_open.get_untracked()
                        && store.settings.with_untracked(|s| s.fog_backdrop)
                        && !reduce_motion()
                        && shaders_available();
                    if !still_active {
                        driver.with(|d| d.running = false);
                        break;
                    }

                    let is_playing = store.is_playing.get_untracked();
                    let target_bass = if is_playing {
                        let s = store.spectrum.get_untracked();
                        ((s[0] + s[1] + s[2] + s[3] + s[4] + s[5]) / 6.0).clamp(0.0, 1.0)
                    } else {
                        0.0_f32
                    };

                    let (next_drift, next_bass, next_accent, keep_going) = driver.with(|d| {
                        let target_speed = if is_playing { 1.0_f32 } else { 0.0_f32 };
                        d.speed += (target_speed - d.speed) * (2.5 * dt).min(1.0);
                        if (target_speed - d.speed).abs() < 0.005 {
                            d.speed = target_speed;
                        }
                        d.drift += d.speed * dt;

                        let rate = if target_bass > d.bass { 18.0 } else { 6.0 };
                        d.bass += (target_bass - d.bass) * (rate * dt).min(1.0);
                        if target_bass == 0.0 && d.bass < 0.004 {
                            d.bass = 0.0;
                        }

                        let faded = if d.fade_elapsed < BACKDROP_FADE_SECS {
                            d.fade_elapsed = (d.fade_elapsed + dt).min(BACKDROP_FADE_SECS);
                            let t = (d.fade_elapsed / BACKDROP_FADE_SECS).clamp(0.0, 1.0);
                            let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                            Some(d.from_accent.mix(d.to_accent, eased))
                        } else {
                            None
                        };

                        let moving = is_playing
                            || d.speed > 0.0
                            || d.bass > 0.004
                            || d.fade_elapsed < BACKDROP_FADE_SECS;
                        if !moving {
                            d.running = false;
                        }
                        (d.drift, d.bass, faded, moving)
                    });

                    drift_anim.set(next_drift);
                    bass_anim.set(next_bass);
                    if let Some(accent) = next_accent {
                        let _ = shown_accent.try_set(accent);
                    }

                    if !keep_going {
                        break;
                    }
                    timer(Duration::from_millis(FOG_FRAME_MS)).await;
                }
            });
        }
    });

    view()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .pointer_events(PointerEvents::None)
        .bg(theme::np_bg)
        .child(
            when(use_fog_shader, move || {
                if let Some(sh) = compiled_fog_shader() {
                    view()
                        .absolute()
                        .inset_0()
                        .size_full()
                        .pointer_events(PointerEvents::None)
                        .child(
                            shader()
                                .source(sh)
                                .w_full()
                                .h_full()
                                .uniform("deep", move || {
                                    theme::artwork_fog_colors(shown_accent.get()).0
                                })
                                .uniform("fog", move || {
                                    theme::artwork_fog_colors(shown_accent.get()).1
                                })
                                .uniform("drift", &drift_anim)
                                .uniform("bass", &bass_anim)
                                .pointer_events(PointerEvents::None),
                        )
                } else {
                    still_backdrop_layer(store)
                }
            })
            .otherwise(move || still_backdrop_layer(store)),
        )
}

/// Clean still backdrop matching `mygo-music` `drawBackdrop` (`backdrop.go:209-222`):
/// vertical HSV gradient (`artwork_backdrop_colors`), faint blurred cover wash,
/// and subtle bottom scrim — zero per-frame timers or spectrum subscriptions.
fn still_backdrop_layer(store: AppStore) -> View {
    let has_cover = computed(move || store.current_artwork_uri.with(|u| !u.is_empty()));

    view()
        .absolute()
        .inset_0()
        .size_full()
        .overflow_hidden()
        .pointer_events(PointerEvents::None)
        // Base HSV vertical gradient (top lighter -> foot deeper, legible under white text)
        .bg_linear_to_b()
        .gradient_from(move || {
            AdaptiveColor::from(theme::artwork_backdrop_colors(store.current_accent.get()).0)
        })
        .gradient_to(move || {
            AdaptiveColor::from(theme::artwork_backdrop_colors(store.current_accent.get()).1)
        })
        .transition_colors()
        .duration(move || motion::transition_ms(700.0))
        // Faint blurred album cover wash of its own colors
        .child(when(has_cover, move || {
            image(move || store.current_artwork_uri.get())
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
                .scale(1.16)
                .blur(px(28.0))
                .opacity(0.11)
                .pointer_events(PointerEvents::None)
        }))
        // Subtle foot scrim so bottom controls remain razor sharp
        .child(
            view()
                .absolute()
                .inset_0()
                .bg_linear_to_b()
                .gradient_from(colors::TRANSPARENT)
                .gradient_via(colors::TRANSPARENT)
                .gradient_to(move || theme::np_bg().opacity(0.24))
                .pointer_events(PointerEvents::None),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn fog_backdrop_wgsl_compiles() {
        let shader =
            nsh_core::shader::Shader::compile(super::FOG_BACKDROP_WGSL).expect("valid WGSL");
        assert!(
            !shader.animated(),
            "fog shader must not read nsh.time so macOS display link stays idle when paused"
        );
    }
}
