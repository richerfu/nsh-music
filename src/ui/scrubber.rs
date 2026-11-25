//! Interactive playback seek scrubber with live drag preview, buffered
//! progress indicator, hover/drag thumb expansion, multi-wisp comet smoke,
//! and tabular elapsed/remaining time labels.

use std::{
    f32::consts::TAU,
    rc::Rc,
    time::{Duration, Instant},
};

use nsh_base::{SliderValue, slider};
use nsh_core::{
    canvas::CanvasGradient,
    geometry::{percentage, point, size},
    prelude::*,
};

use super::{
    motion,
    theme::{self, colors, font_size},
    track_row::format_duration_ms,
};

/// Exact constants matching `egoist/mygo-music` `ui_player.go:1959-1989`.
const SMOKE_LIFE_SECS: f32 = 1.4;
const SMOKE_WISPS: usize = 36;
const SMOKE_FADE_SECS: f32 = 0.60;
const SMOKE_PAD_X: f32 = 24.0;
const SMOKE_HEADROOM_Y: f32 = 34.0;

fn wisp_rand(k: f32, offset: f32) -> f32 {
    let x = (k * 127.1 + offset * 311.7).sin() * 43_758.547;
    x - x.floor()
}

/// Builder for the playback seek scrubber.
pub struct Scrubber {
    position_ms: Reactive<u64>,
    duration_ms: Reactive<u64>,
    buffered_ratio: Reactive<f32>,
    smoke_mode: Reactive<bool>,
    playing: Reactive<bool>,
    accent: Reactive<Hsla>,
    spectrum: Reactive<[f32; 16]>,
    show_labels: bool,
    inline_labels: bool,
    on_dark_stage: bool,
    disabled: Reactive<bool>,
    on_seek: Option<Rc<dyn Fn(u64)>>,
}

/// Convenience constructor for [`Scrubber`].
pub fn scrubber(
    position_ms: impl IntoReactive<u64>,
    duration_ms: impl IntoReactive<u64>,
) -> Scrubber {
    Scrubber::new(position_ms, duration_ms)
}

impl Scrubber {
    pub fn new(position_ms: impl IntoReactive<u64>, duration_ms: impl IntoReactive<u64>) -> Self {
        Self {
            position_ms: position_ms.into_reactive(),
            duration_ms: duration_ms.into_reactive(),
            buffered_ratio: Reactive::Static(0.0),
            smoke_mode: Reactive::Static(false),
            playing: Reactive::Static(false),
            accent: Reactive::Static(theme::default_artwork_accent()),
            spectrum: Reactive::Static([0.0; 16]),
            show_labels: true,
            inline_labels: false,
            on_dark_stage: false,
            disabled: Reactive::Static(false),
            on_seek: None,
        }
    }

    pub fn buffered_ratio(mut self, ratio: impl IntoReactive<f32>) -> Self {
        self.buffered_ratio = ratio.into_reactive();
        self
    }

    pub fn smoke_mode(mut self, enabled: impl IntoReactive<bool>) -> Self {
        self.smoke_mode = enabled.into_reactive();
        self
    }

    pub fn playing(mut self, playing: impl IntoReactive<bool>) -> Self {
        self.playing = playing.into_reactive();
        self
    }

    pub fn accent(mut self, accent: impl IntoReactive<Hsla>) -> Self {
        self.accent = accent.into_reactive();
        self
    }

    pub fn spectrum(mut self, spectrum: impl IntoReactive<[f32; 16]>) -> Self {
        self.spectrum = spectrum.into_reactive();
        self
    }

    pub fn show_labels(mut self, show: bool) -> Self {
        self.show_labels = show;
        self
    }

    /// Places elapsed/remaining labels on the left & right of the track row
    /// instead of stacking them underneath (ideal for compact desktop player bars).
    pub fn inline_labels(mut self, inline: bool) -> Self {
        self.inline_labels = inline;
        self
    }

    /// Styles the scrubber for the dark Now Playing stage (`np-fg` / `np-muted`).
    pub fn on_dark_stage(mut self, dark_stage: bool) -> Self {
        self.on_dark_stage = dark_stage;
        self
    }

    pub fn disabled(mut self, disabled: impl IntoReactive<bool>) -> Self {
        self.disabled = disabled.into_reactive();
        self
    }

    pub fn on_seek(mut self, f: impl Fn(u64) + 'static) -> Self {
        self.on_seek = Some(Rc::new(f));
        self
    }
}

impl IntoElement for Scrubber {
    fn into_element(self) -> AnyElement {
        let Scrubber {
            position_ms,
            duration_ms,
            buffered_ratio,
            smoke_mode,
            playing,
            accent,
            spectrum,
            show_labels,
            inline_labels,
            on_dark_stage,
            disabled,
            on_seek,
        } = self;

        // Holds live drag position so the elapsed time preview follows the
        // finger/cursor immediately and commits once on release.
        let drag_preview = signal(None::<u64>);
        let track_size = signal((240.0_f32, 20.0_f32));
        let smoke_time = signal(0.0_f32);
        let smoke_opacity = signal(0.0_f32);
        let smoke_running = stored(false);
        let owner = Owner::current();

        let displayed_ms = {
            let pos = position_ms.clone();
            computed(move || drag_preview.get().unwrap_or_else(|| pos.get()))
        };

        let dur_for_ratio = duration_ms.clone();
        let progress_ratio = computed(move || {
            let dur = dur_for_ratio.get();
            if dur == 0 {
                0.0_f32
            } else {
                (displayed_ms.get() as f32 / dur as f32).clamp(0.0, 1.0)
            }
        });

        let dur_for_smoke = duration_ms.clone();
        let want_smoke = computed(move || {
            smoke_mode.get()
                && playing.get()
                && dur_for_smoke.get() > 0
                && drag_preview.get().is_none()
                && !reduce_motion()
        });

        // On-demand 30fps smoke driver: spawns ONLY while smoking or fading out
        // (~600ms smooth fade-in/out matching `mygo-music`), exiting completely
        // with zero timers and zero CPU when paused, idle, dragging, or disabled.
        effect(move || {
            let active = want_smoke.get();
            if reduce_motion() {
                smoke_opacity.set(0.0);
                return;
            }
            if !active && smoke_opacity.get_untracked() <= 0.005 {
                return;
            }
            let already = smoke_running.with(|running| {
                if *running {
                    true
                } else {
                    *running = true;
                    false
                }
            });
            if already {
                return;
            }
            owner.run(move || {
                spawn(async move {
                    let mut last = Instant::now();
                    loop {
                        timer(Duration::from_millis(33)).await;
                        if reduce_motion() {
                            smoke_opacity.set(0.0);
                            smoke_running.with(|running| *running = false);
                            break;
                        }
                        let current = Instant::now();
                        let dt = current
                            .saturating_duration_since(last)
                            .as_secs_f32()
                            .clamp(0.004, 0.10);
                        last = current;

                        let target_on = want_smoke.get_untracked();
                        let prev = smoke_opacity.get_untracked();
                        let next = if target_on {
                            (prev + dt / SMOKE_FADE_SECS).min(1.0)
                        } else {
                            (prev - dt / SMOKE_FADE_SECS).max(0.0)
                        };

                        if !target_on && next <= 0.005 {
                            smoke_opacity.set(0.0);
                            smoke_running.with(|running| *running = false);
                            break;
                        }

                        if (next - prev).abs() > 1e-4 {
                            smoke_opacity.set(next);
                        }
                        smoke_time.update(|t| *t = (*t + dt) % 10_000.0);
                    }
                });
            });
        });

        // Effective visible smoke strength: zero at 0:00 (prevents any resting
        // smudge at track start) and smoothly fades in/out with `smoke_opacity`.
        let effective_smoke = computed(move || {
            let op = smoke_opacity.get();
            if op <= 0.01 {
                return 0.0_f32;
            }
            let ms = displayed_ms.get();
            if ms == 0 {
                return 0.0_f32;
            }
            (op * (ms as f32 / 350.0).clamp(0.0, 1.0)).clamp(0.0, 1.0)
        });

        let max_val = {
            let dur = duration_ms.clone();
            move || dur.get().max(1) as f64
        };

        let slider_val = move || Some(SliderValue::Single(displayed_ms.get() as f64));

        let label_color = move || {
            if on_dark_stage {
                theme::np_muted()
            } else {
                theme::muted_fg()
            }
        };
        let rail_color = move || {
            if on_dark_stage {
                theme::np_surface()
            } else {
                theme::track_rail()
            }
        };
        let buffered_color = move || {
            if on_dark_stage {
                theme::np_surface_hover()
            } else {
                theme::track_buffered()
            }
        };
        let fill_color = move || {
            if on_dark_stage {
                theme::np_fg()
            } else {
                theme::primary()
            }
        };

        let seek_cb = on_seek.clone();
        let buffered_rx = buffered_ratio;
        let acc_rx = accent;
        let spec_rx = spectrum;

        let track_stack = view()
            .group()
            .w_full()
            .justify_center()
            .on_layout(move |bounds: &Bounds<Points>| {
                track_size.set((
                    bounds.size.width.0.max(40.0),
                    bounds.size.height.0.max(20.0),
                ));
            })
            .child(when(
                move || effective_smoke.get() > 0.01,
                move || {
                    let ratio_rx = progress_ratio;
                    let acc_rx = acc_rx.clone();
                    let spec_rx = spec_rx.clone();
                    canvas(move || {
                        let shown = effective_smoke.get();
                        if shown <= 0.01 {
                            return Painter::new();
                        }
                        let (w, h) = track_size.get();
                        let ratio = ratio_rx.get();
                        let t = smoke_time.get();
                        let s = spec_rx.get();
                        // Match `mygo-music` `st.level.group(0, 6)` bass average.
                        let bass = ((s[0] + s[1] + s[2] + s[3] + s[4] + s[5]) / 6.0)
                            .clamp(0.0, 1.0);
                        let smoke_col = if on_dark_stage {
                            theme::artwork_np_accent(acc_rx.get())
                        } else {
                            color(theme::primary())
                        };
                        let fill_col = color(fill_color());
                        let head_x = SMOKE_PAD_X + w * ratio;
                        let cy = SMOKE_HEADROOM_Y + h * 0.5;
                        let thick = 4.0_f32;

                        let mut painter = Painter::new();

                        // Luminous comet streak brightening from tail (0.3) to head (`ui_player.go:1921-1922`).
                        let played_w = (head_x - SMOKE_PAD_X).max(thick);
                        let streak = CanvasGradient::linear(
                            point(px(SMOKE_PAD_X), px(cy)),
                            point(px(SMOKE_PAD_X + played_w), px(cy)),
                        )
                        .add_color_stop(
                            percentage(0.0),
                            theme::mix_color(fill_col, smoke_col, shown).alpha(0.30 * shown),
                        )
                        .add_color_stop(
                            percentage(1.0),
                            theme::mix_color(fill_col, smoke_col, 0.80 * shown).alpha(0.95 * shown),
                        );
                        painter = painter
                            .fill_style(streak)
                            .begin_path()
                            .round_rect(
                                Bounds::new(
                                    point(px(SMOKE_PAD_X), px(cy - thick * 0.5)),
                                    size(px(played_w), px(thick)),
                                ),
                                px(thick * 0.5),
                            )
                            .fill();

                        // Rising smoke wisps trailing from playhead (`ui_player.go:1966-1989`).
                        let interval = SMOKE_LIFE_SECS / SMOKE_WISPS as f32;
                        let first = (t / interval).floor();
                        for j in 0..SMOKE_WISPS {
                            let k = first - j as f32;
                            let age = t - k * interval;
                            if !(0.0..=SMOKE_LIFE_SECS).contains(&age) {
                                continue;
                            }
                            if wisp_rand(k, 5.0) > 0.60 + 0.40 * bass {
                                continue;
                            }
                            let f = age / SMOKE_LIFE_SECS;
                            let wx = head_x - age * (12.0 + 22.0 * wisp_rand(k, 1.0));
                            let curl = (age * (2.5 + 2.0 * wisp_rand(k, 3.0))
                                + wisp_rand(k, 4.0) * TAU)
                                .sin()
                                * 6.0
                                * f;
                            let wy = cy - age * (12.0 + 18.0 * wisp_rand(k, 2.0)) + curl;
                            let wisp_r = (3.0 + f * (12.0 + 10.0 * wisp_rand(k, 4.0))) * 0.65;
                            let alpha = (0.50
                                * (1.0 - f).powf(1.4)
                                * shown
                                * (0.75 + 0.50 * bass))
                                .clamp(0.0, 0.68);

                            let wisp_grad = CanvasGradient::radial(
                                point(px(wx), px(wy)),
                                px(0.0),
                                point(px(wx), px(wy)),
                                px(wisp_r),
                            )
                            .add_color_stop(percentage(0.0), smoke_col.alpha(alpha))
                            .add_color_stop(percentage(0.55), smoke_col.alpha(alpha * 0.38))
                            .add_color_stop(percentage(1.0), smoke_col.alpha(0.0));
                            painter = painter
                                .fill_style(wisp_grad)
                                .begin_path()
                                .arc(wx, wy, wisp_r, 0.0, TAU)
                                .fill();
                        }

                        // Playhead bass-reactive glow halo (`glow := 9 + 8*bass`) and crisp core (`thick*(1 + 0.25*bass)`).
                        let glow_r = 9.0 + 8.0 * bass;
                        let halo = CanvasGradient::radial(
                            point(px(head_x), px(cy)),
                            px(0.0),
                            point(px(head_x), px(cy)),
                            px(glow_r),
                        )
                        .add_color_stop(
                            percentage(0.0),
                            smoke_col.alpha((0.78 * shown).clamp(0.0, 0.95)),
                        )
                        .add_color_stop(
                            percentage(0.45),
                            smoke_col.alpha((0.32 * shown).clamp(0.0, 0.60)),
                        )
                        .add_color_stop(percentage(1.0), smoke_col.alpha(0.0));
                        let core_r = (thick * (1.0 + 0.25 * bass)) * 0.5;
                        let core_col =
                            theme::mix_color(color(colors::NP_FG), smoke_col, 0.15).alpha(shown);
                        painter
                            .fill_style(halo)
                            .begin_path()
                            .arc(head_x, cy, glow_r, 0.0, TAU)
                            .fill()
                            .fill_style(core_col)
                            .begin_path()
                            .arc(head_x, cy, core_r, 0.0, TAU)
                            .fill()
                    })
                    .absolute()
                    .left(px(-SMOKE_PAD_X))
                    .right(px(-SMOKE_PAD_X))
                    .top(px(-SMOKE_HEADROOM_Y))
                    .h(move || px(track_size.get().1 + SMOKE_HEADROOM_Y))
                    .pointer_events(PointerEvents::None)
                },
            ))
            .child(
                // Buffered bar drawn behind the slider indicator rail.
                view()
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(px(4.0))
                    .rounded_full()
                    .overflow_hidden()
                    .transition_all()
                    .duration(move || motion::transition_ms(140.0))
                    .group_hover(|s| s.h(px(6.0)))
                    .pointer_events(PointerEvents::None)
                    .child(
                        view()
                            .h_full()
                            .w(move || relative(buffered_rx.get().clamp(0.0, 1.0)))
                            .rounded_full()
                            .bg(buffered_color),
                    ),
            )
            .child(
                slider()
                    .value(slider_val)
                    .min(0.0)
                    .max(max_val)
                    .step(250.0)
                    .disabled(disabled)
                    .accessibility_label("Seek position")
                    .on_change(move |v: &SliderValue| {
                        drag_preview.set(Some(v.start().max(0.0) as u64));
                    })
                    .on_change_end(move |v: &SliderValue| {
                        let target = v.start().max(0.0) as u64;
                        drag_preview.set(None);
                        if let Some(cb) = &seek_cb {
                            cb(target);
                        }
                    })
                    .track_style(|s| {
                        s.w_full()
                            .h(move || {
                                if theme::detect_device_form().is_touch() {
                                    px(36.0)
                                } else {
                                    px(20.0)
                                }
                            })
                            .justify_center()
                    })
                    .indicator_style(move |s| {
                        s.w_full()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(rail_color)
                            .transition_all()
                            .duration(move || motion::transition_ms(140.0))
                            .group_hover(|s| s.h(px(6.0)))
                    })
                    .range_style(move |s| {
                        s.rounded_full()
                            .bg(fill_color)
                            .opacity(move || 1.0 - effective_smoke.get())
                    })
                    .thumb_class("active:opacity-100 active:scale-125 focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-ring")
                    .thumb_style(move |s| {
                        s.size(px(12.0))
                            .rounded_full()
                            .bg(if on_dark_stage {
                                colors::NP_FG
                            } else {
                                colors::PRIMARY
                            })
                            .shadow_sm()
                            .opacity(move || {
                                if theme::detect_device_form().is_touch()
                                    || drag_preview.get().is_some()
                                {
                                    1.0_f32
                                } else {
                                    0.0_f32
                                }
                            })
                            .transition_all()
                            .duration(move || motion::transition_ms(140.0))
                            .group_hover(|s| s.opacity(1.0).scale(1.15))
                    }),
            );

        if !show_labels {
            return track_stack.into_element();
        }

        let dur_for_rem = duration_ms;
        let elapsed_label = text(move || format_duration_ms(displayed_ms.get() as i64))
            .tabular_nums()
            .text_size(px(font_size::MICRO))
            .text_color(label_color);
        let remaining_label = text(move || {
            let total = dur_for_rem.get();
            let elapsed = displayed_ms.get().min(total);
            let remaining = total.saturating_sub(elapsed);
            format!("-{}", format_duration_ms(remaining as i64))
        })
        .tabular_nums()
        .text_size(px(font_size::MICRO))
        .text_color(label_color);

        if inline_labels {
            return view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_2p5()
                .child(view().min_w(px(36.0)).items_end().child(elapsed_label))
                .child(track_stack.flex_1().min_w(px(80.0)))
                .child(view().min_w(px(40.0)).child(remaining_label))
                .into_element();
        }

        view()
            .w_full()
            .gap_1()
            .child(track_stack)
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(elapsed_label)
                    .child(remaining_label),
            )
            .into_element()
    }
}
