//! Interactive playback seek scrubber with live drag preview, buffered
//! progress indicator, hover/drag thumb expansion, multi-wisp comet smoke,
//! and tabular elapsed/remaining time labels.

use std::{rc::Rc, time::Duration};

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

const SMOKE_LIFE_SECS: f32 = 1.4;
const SMOKE_WISPS: usize = 24;
const SMOKE_PAD_X: f32 = 16.0;
const SMOKE_HEADROOM_Y: f32 = 26.0;

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
    pub fn new(
        position_ms: impl IntoReactive<u64>,
        duration_ms: impl IntoReactive<u64>,
    ) -> Self {
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
        let is_smoking = computed(move || {
            smoke_mode.get()
                && playing.get()
                && dur_for_smoke.get() > 0
                && drag_preview.get().is_none()
                && !reduce_motion()
        });

        set_interval(Duration::from_millis(33), move || {
            if is_smoking.get_untracked() {
                smoke_time.update(|t| *t = (*t + 0.033) % 1000.0);
            }
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
            .child(when(is_smoking, move || {
                let ratio_rx = progress_ratio;
                let acc_rx = acc_rx.clone();
                let spec_rx = spec_rx.clone();
                canvas(move || {
                    let (w, h) = track_size.get();
                    let ratio = ratio_rx.get();
                    let t = smoke_time.get();
                    let s = spec_rx.get();
                    let bass = ((s[0] + s[1] + s[2] + s[3]) * 0.25).clamp(0.0, 1.0);
                    let base_col = if on_dark_stage {
                        acc_rx.get().lighten(0.24)
                    } else {
                        color(theme::primary())
                    };
                    let head_x = SMOKE_PAD_X + w * ratio;
                    let cy = SMOKE_HEADROOM_Y + h * 0.5;

                    let mut painter = Painter::new();

                    // Brightening comet streak along played portion.
                    if head_x > SMOKE_PAD_X + 2.0 {
                        let streak = CanvasGradient::linear(
                            point(px(SMOKE_PAD_X), px(cy)),
                            point(px(head_x), px(cy)),
                        )
                        .add_color_stop(percentage(0.0), base_col.alpha(0.15))
                        .add_color_stop(percentage(1.0), base_col.alpha(0.75));
                        painter = painter
                            .fill_style(streak)
                            .begin_path()
                            .round_rect(
                                Bounds::new(
                                    point(px(SMOKE_PAD_X), px(cy - 2.0)),
                                    size(px(head_x - SMOKE_PAD_X), px(4.0)),
                                ),
                                px(2.0),
                            )
                            .fill();
                    }

                    // Rising smoke wisps trailing from playhead (ported from mygo-music drawSmoke).
                    let interval = SMOKE_LIFE_SECS / SMOKE_WISPS as f32;
                    let first = (t / interval).floor();
                    for j in 0..SMOKE_WISPS {
                        let k = first - j as f32;
                        let age = t - k * interval;
                        if !(0.0..=SMOKE_LIFE_SECS).contains(&age) {
                            continue;
                        }
                        if wisp_rand(k, 5.0) > 0.62 + 0.38 * bass {
                            continue;
                        }
                        let f = age / SMOKE_LIFE_SECS;
                        let wx = head_x - age * (10.0 + 20.0 * wisp_rand(k, 1.0));
                        let curl = ((age * (2.5 + 2.0 * wisp_rand(k, 3.0))
                            + wisp_rand(k, 4.0) * std::f32::consts::TAU)
                            .sin())
                            * 4.5
                            * f;
                        let wy = cy - age * (10.0 + 16.0 * wisp_rand(k, 2.0)) + curl;
                        let r = 2.5 + f * (9.0 + 7.0 * wisp_rand(k, 4.0));
                        let alpha = (0.36 * (1.0 - f).powf(1.6) * (0.7 + 0.5 * bass)).clamp(0.0, 0.55);

                        let wisp_grad = CanvasGradient::radial(
                            point(px(wx), px(wy)),
                            px(0.0),
                            point(px(wx), px(wy)),
                            px(r),
                        )
                        .add_color_stop(percentage(0.0), base_col.alpha(alpha))
                        .add_color_stop(percentage(1.0), base_col.alpha(0.0));
                        painter = painter
                            .fill_style(wisp_grad)
                            .begin_path()
                            .arc(wx, wy, r, 0.0, std::f32::consts::TAU)
                            .fill();
                    }

                    // Soft comet head glow & crisp luminous core dot.
                    let halo_r = 7.0 + 5.0 * bass;
                    let halo = CanvasGradient::radial(
                        point(px(head_x), px(cy)),
                        px(0.0),
                        point(px(head_x), px(cy)),
                        px(halo_r),
                    )
                    .add_color_stop(percentage(0.0), base_col.alpha(0.55 + 0.25 * bass))
                    .add_color_stop(percentage(1.0), base_col.alpha(0.0));
                    let core_r = 2.4 + 0.8 * bass;
                    painter
                        .fill_style(halo)
                        .begin_path()
                        .arc(head_x, cy, halo_r, 0.0, std::f32::consts::TAU)
                        .fill()
                        .fill_style(base_col.lighten(0.32).alpha(0.92))
                        .begin_path()
                        .arc(head_x, cy, core_r, 0.0, std::f32::consts::TAU)
                        .fill()
                })
                .absolute()
                .left(px(-SMOKE_PAD_X))
                .right(px(-SMOKE_PAD_X))
                .top(px(-SMOKE_HEADROOM_Y))
                .h(move || px(track_size.get().1 + SMOKE_HEADROOM_Y))
                .pointer_events(PointerEvents::None)
            }))
            .child(
                // Buffered bar drawn behind the slider indicator rail.
                view()
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(px(4.0))
                    .rounded_full()
                    .overflow_hidden()
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
                            .group_hover(|s| s.h(px(5.5)))
                    })
                    .range_style(move |s| s.rounded_full().bg(fill_color))
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
                .child(
                    view()
                        .min_w(px(36.0))
                        .items_end()
                        .child(elapsed_label),
                )
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
