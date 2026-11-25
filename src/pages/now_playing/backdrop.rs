//! Ambient reactive backdrop for the Now Playing stage.
//!
//! Uses a real-time WGSL atmospheric fog & rising dust-speck shader ported from
//! `egoist/mygo-music` (`backdrop.go` `fogSource`) when GPU shaders are
//! available, `settings.fog_backdrop` is enabled, and `reduce_motion()` is off.
//! Falls back seamlessly to a soft audio-reactive `canvas` radial gradient wash
//! derived from the active track's dominant accent (`store.current_accent`).

use std::{f32::consts::TAU, sync::OnceLock};

use nsh_core::{
    canvas::CanvasGradient,
    geometry::{percentage, point},
    prelude::*,
    shader::Shader,
};

use crate::{
    state::AppStore,
    ui::{motion, theme},
};

/// Rising atmospheric fog and glinting dust-speck WGSL shader (1:1 port of
/// `egoist/mygo-music` `backdrop.go` `fogSource` for NSH's `fn shade(uv: vec2f) -> vec4f`
/// contract).
pub(crate) const FOG_BACKDROP_WGSL: &str = r#"
struct Uniforms {
    deep: vec4f,
    fog: vec4f,
    bass: f32,
    playing: f32,
    _pad0: f32,
    _pad1: f32,
}

fn hash2(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

fn noise(p: vec2f) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (vec2f(3.0) - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2f(1.0, 0.0));
    let c = hash2(i + vec2f(0.0, 1.0));
    let d = hash2(i + vec2f(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p_in: vec2f) -> f32 {
    var p = p_in;
    var v: f32 = 0.0;
    var a: f32 = 0.5;
    for (var i: i32 = 0; i < 5; i = i + 1) {
        v = v + a * noise(p);
        p = p * 2.03 + vec2f(1.7, 9.2);
        a = a * 0.5;
    }
    return v;
}

fn shade(uv: vec2f) -> vec4f {
    let aspect = nsh.size.x / max(nsh.size.y, 1.0);
    let p = vec2f(uv.x * aspect, uv.y) * 2.4;
    let play = clamp(uniforms.playing, 0.0, 1.0);
    let bass = clamp(uniforms.bass, 0.0, 1.0);
    let t = nsh.time * mix(0.25, 1.0, play);

    // Rising domain-warped fog field (1:1 port of mygo-music backdrop.go)
    let rise = vec2f(0.0, t * 0.1);
    let q = vec2f(
        fbm(p + rise + vec2f(0.0, t * 0.04)),
        fbm(p + rise + vec2f(5.2, 1.3) - vec2f(t * 0.05, 0.0))
    );
    let f = fbm(p + rise + 0.9 * q);

    // Broad cloud banks and puffs within them
    let bank = fbm(p * 0.45 + rise * 0.6 + vec2f(3.7, 0.0));
    let cloud = smoothstep(0.3 - 0.06 * bass, 0.78, f) * (0.45 + 0.75 * smoothstep(0.3, 0.65, bank));

    // Thickest slightly above the lower controls
    let m = smoothstep(-0.1, 0.4, uv.y) * (1.0 - 0.5 * smoothstep(0.65, 1.0, uv.y));
    var c = mix(uniforms.deep.rgb, uniforms.fog.rgb, min(cloud * m * (0.85 + 0.35 * bass), 1.0));

    // Broad center glow darker toward edges
    let g = length((uv - vec2f(0.5, 0.42)) * vec2f(1.0, 1.25));
    c = c * (1.08 - 0.45 * smoothstep(0.2, 0.85, g));

    // Crisp Gaussian rising dust specks
    let cell = max(nsh.size.y / 22.0, 1.0);
    let sp = (uv * nsh.size + vec2f(0.0, t * cell * 0.9)) / cell;
    let id = floor(sp);
    let h = hash2(id + vec2f(3.1, 7.7));
    if (h > 0.93) {
        let at = vec2f(hash2(id + vec2f(1.3, 2.9)), hash2(id + vec2f(8.1, 4.3))) * 0.8 + 0.1;
        let d = length((fract(sp) - at) * cell) / max(cell * 0.05, 0.7);
        let glint = 0.5 + 0.5 * sin(t * (1.5 + 2.0 * h) + h * 40.0);
        c = c + vec3f(0.75, 0.82, 1.0) * exp(-d * d) * glint * (0.25 + 0.3 * m);
    }

    return vec4f(clamp(c, vec3f(0.0), vec3f(1.0)), 1.0);
}
"#;

fn compiled_fog_shader() -> Option<Shader> {
    static FOG_SHADER: OnceLock<Option<Shader>> = OnceLock::new();
    FOG_SHADER
        .get_or_init(|| Shader::compile(FOG_BACKDROP_WGSL).ok())
        .clone()
}

/// Renders the full-bleed ambient reactive backdrop behind Now Playing content.
pub fn ambient_backdrop(store: AppStore) -> View {
    let canvas_size = signal((1200.0_f32, 820.0_f32));

    let use_fog_shader = computed(move || {
        shaders_available()
            && store.settings.with(|s| s.fog_backdrop)
            && !reduce_motion()
            && compiled_fog_shader().is_some()
    });

    view()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .pointer_events(PointerEvents::None)
        .on_layout(move |bounds: &Bounds<Points>| {
            let w = bounds.size.width.0.max(1.0);
            let h = bounds.size.height.0.max(1.0);
            canvas_size.set((w, h));
        })
        // Deep dominant-tinted base stage surface
        .bg(move || AdaptiveColor::from(theme::artwork_stage_bg(store.current_accent.get())))
        .transition_colors()
        .duration(move || motion::transition_ms(480.0))
        // Vertical atmospheric wash from artwork accent into deep stage obsidian
        .child(
            view()
                .absolute()
                .inset_0()
                .bg_linear_to_b()
                .gradient_from(move || theme::artwork_wash(store.current_accent.get(), 0.34))
                .gradient_via(move || theme::artwork_wash(store.current_accent.get(), 0.14))
                .gradient_to(theme::np_bg)
                .pointer_events(PointerEvents::None),
        )
        // GPU WGSL fog shader when enabled & supported, otherwise Canvas radial fallback
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
                                    theme::artwork_fog_colors(store.current_accent.get()).0
                                })
                                .uniform("fog", move || {
                                    theme::artwork_fog_colors(store.current_accent.get()).1
                                })
                                .uniform("bass", move || {
                                    if store.is_playing.get() && !reduce_motion() {
                                        let s = store.spectrum.get();
                                        ((s[0] + s[1] + s[2] + s[3]) * 0.25).clamp(0.0, 1.0)
                                    } else {
                                        0.0_f32
                                    }
                                })
                                .uniform("playing", move || {
                                    if store.is_playing.get() {
                                        1.0_f32
                                    } else {
                                        0.0_f32
                                    }
                                })
                                .paused(move || !store.is_playing.get() || reduce_motion())
                                .pointer_events(PointerEvents::None),
                        )
                } else {
                    canvas_glow_layer(store, canvas_size)
                }
            })
            .otherwise(move || canvas_glow_layer(store, canvas_size)),
        )
}

fn canvas_glow_layer(store: AppStore, canvas_size: Signal<(f32, f32)>) -> View {
    view()
        .absolute()
        .inset_0()
        .size_full()
        .pointer_events(PointerEvents::None)
        .child(
            canvas(move || {
                let (w, h) = canvas_size.get();
                let accent = store.current_accent.get();
                let playing = store.is_playing.get();
                let reduced = reduce_motion();

                let (bass, mid, treble, phase) = if playing && !reduced {
                    let s = store.spectrum.get();
                    let bass = ((s[0] + s[1] + s[2] + s[3]) * 0.25).clamp(0.0, 1.0);
                    let mid = ((s[4] + s[5] + s[6] + s[7] + s[8] + s[9]) / 6.0).clamp(0.0, 1.0);
                    let treble =
                        ((s[10] + s[11] + s[12] + s[13] + s[14] + s[15]) / 6.0).clamp(0.0, 1.0);
                    let t = (store.position_ms.get() as f32) * 0.00018;
                    (bass, mid, treble, t)
                } else {
                    (0.0, 0.0, 0.0, 0.0)
                };

                paint_ambient_glow(w, h, accent, bass, mid, treble, phase, reduced)
            })
            .absolute()
            .inset_0()
            .size_full()
            .pointer_events(PointerEvents::None),
        )
}

#[allow(clippy::too_many_arguments)]
fn paint_ambient_glow(
    w: f32,
    h: f32,
    accent: Hsla,
    bass: f32,
    mid: f32,
    treble: f32,
    phase: f32,
    reduced: bool,
) -> Painter {
    let min_dim = w.min(h).max(240.0);
    let stage_base = theme::artwork_stage_bg(accent);

    let (drift_x, drift_y) = if reduced {
        (0.0, 0.0)
    } else {
        (
            (phase * TAU).sin() * w * 0.04,
            (phase * TAU * 0.7).cos() * h * 0.035,
        )
    };

    // Primary luminous accent orb anchored behind the centerpiece artwork
    let orb1_x = w * 0.28 + drift_x;
    let orb1_y = h * 0.34 + drift_y;
    let orb1_r = min_dim * (0.56 + bass * 0.16);
    let orb1_core = accent
        .saturate(0.18)
        .lighten(0.18)
        .alpha((0.30 + bass * 0.22).clamp(0.0, 0.60));
    let orb1_mid = accent.alpha((0.12 + bass * 0.10).clamp(0.0, 0.30));
    let orb1_edge = stage_base.alpha(0.0);

    let grad1 = CanvasGradient::radial(
        point(px(orb1_x), px(orb1_y)),
        px(0.0),
        point(px(orb1_x), px(orb1_y)),
        px(orb1_r),
    )
    .add_color_stop(percentage(0.0), orb1_core)
    .add_color_stop(percentage(0.52), orb1_mid)
    .add_color_stop(percentage(1.0), orb1_edge);

    // Secondary harmonic orb behind the lyrics/panel side
    let orb2_x = w * 0.76 - drift_x * 0.85;
    let orb2_y = h * 0.66 - drift_y * 0.85;
    let orb2_r = min_dim * (0.52 + mid * 0.14);
    let harmonic = accent.spin(34.0).saturate(0.12);
    let orb2_core = harmonic
        .lighten(0.14)
        .alpha((0.22 + mid * 0.18).clamp(0.0, 0.48));
    let orb2_edge = stage_base.alpha(0.0);

    let grad2 = CanvasGradient::radial(
        point(px(orb2_x), px(orb2_y)),
        px(0.0),
        point(px(orb2_x), px(orb2_y)),
        px(orb2_r),
    )
    .add_color_stop(percentage(0.0), orb2_core)
    .add_color_stop(percentage(1.0), orb2_edge);

    // Subtle upper-center warm crest responding to treble energy
    let orb3_x = w * 0.54 + drift_y * 0.6;
    let orb3_y = h * 0.18 - drift_x * 0.4;
    let orb3_r = min_dim * (0.38 + treble * 0.12);
    let warm_tint = accent.spin(-24.0).lighten(0.22);
    let orb3_core = warm_tint.alpha((0.12 + treble * 0.14).clamp(0.0, 0.32));
    let orb3_edge = stage_base.alpha(0.0);

    let grad3 = CanvasGradient::radial(
        point(px(orb3_x), px(orb3_y)),
        px(0.0),
        point(px(orb3_x), px(orb3_y)),
        px(orb3_r),
    )
    .add_color_stop(percentage(0.0), orb3_core)
    .add_color_stop(percentage(1.0), orb3_edge);

    // Bottom contrast scrim so lyrics and thumb-zone controls stay razor sharp
    let vignette = CanvasGradient::linear(point(px(0.0), px(h * 0.55)), point(px(0.0), px(h)))
        .add_color_stop(percentage(0.0), stage_base.alpha(0.0))
        .add_color_stop(percentage(1.0), color(theme::np_bg()).alpha(0.54));

    Painter::new()
        .fill_style(grad1)
        .fill_rect(0.0, 0.0, w, h)
        .fill_style(grad2)
        .fill_rect(0.0, 0.0, w, h)
        .fill_style(grad3)
        .fill_rect(0.0, 0.0, w, h)
        .fill_style(vignette)
        .fill_rect(0.0, 0.0, w, h)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fog_backdrop_wgsl_compiles() {
        nsh_core::shader::Shader::compile(super::FOG_BACKDROP_WGSL).expect("valid WGSL");
    }
}
