//! Full-window Folia lyric visualizer (`NowPlayingPanel::Stage`) aligned 1:1 with
//! `egoist/mygo-music` (`internal/app/ui_stage.go` & `internal/app/ui_stage_styles.go`):
//! - Displays ONE active lyric line in `Flow` (scattered word springs + accent glow),
//!   `Steps` (staggered rows with architectural corner L-guides), or `Tilt` (editorial
//!   multi-part poster cadence with leaning italic accent phrase)
//! - Optional radial `Sparks` (`settings.stage_sparks && !reduce_motion()`) bursting
//!   off landing words
//! - Centered 3-dot breathing interlude during intros and `> 4s` gaps
//! - Centered Quiet-Part Top Comment quote dimming the stage during `>= 8s` quiet parts
//! - Footer translation & next-line preview + slim `stageBar` along the foot

use std::f32::consts::TAU;

use nsh_base::button;
use nsh_core::{LineHeight, canvas::LineCap, prelude::*};
use nsh_music_audio::PlaybackStatus;
use nsh_music_core::{
    lyrics::{ParsedLyricLine, spread_words},
    model::LyricWord,
};

use super::{
    buffering_badge, clickable_artist_links, lyrics_view, phone_scrubber_meta_row,
    phone_thumb_transport, song_more_menu, stage_like_button, stage_scrubber_bar, stage_transport,
};
use crate::{
    state::{AppStore, NowPlayingStageStyle},
    ui::{
        ContextMenuAction,
        icons::{Icon, icon, icon_reactive},
        motion, shimmer_block_on_dark, styled_menu,
        theme::{self, DeviceForm, colors, font_size},
        track_row::format_duration_ms,
    },
};

/// Selectable Folia stage visualizer styles (`Flow`, `Steps`, `Tilt`).
const FOLIA_STYLES: &[(NowPlayingStageStyle, &str)] = &[
    (NowPlayingStageStyle::Flow, "now_playing.stage.flow"),
    (NowPlayingStageStyle::Steps, "now_playing.stage.steps"),
    (NowPlayingStageStyle::Tilt, "now_playing.stage.tilt"),
];

fn normalize_stage_style(style: NowPlayingStageStyle) -> NowPlayingStageStyle {
    match style {
        NowPlayingStageStyle::Steps => NowPlayingStageStyle::Steps,
        NowPlayingStageStyle::Tilt => NowPlayingStageStyle::Tilt,
        NowPlayingStageStyle::Classic | NowPlayingStageStyle::Flow | NowPlayingStageStyle::Wall => {
            NowPlayingStageStyle::Flow
        }
    }
}

fn style_label_key(style: NowPlayingStageStyle) -> &'static str {
    match normalize_stage_style(style) {
        NowPlayingStageStyle::Steps => "now_playing.stage.steps",
        NowPlayingStageStyle::Tilt => "now_playing.stage.tilt",
        _ => "now_playing.stage.flow",
    }
}

/// Compact dropdown pill (`Flow ▾` / `Steps ▾` / `Tilt ▾`) opening `Flow`/`Steps`/`Tilt`
/// + `Sparks` menu, shown beside the top tabs whenever `NowPlayingPanel::Stage` is active.
pub fn stage_style_menu_button(store: AppStore, form: DeviceForm) -> impl IntoElement {
    let touch = form.is_touch();
    let min_h = if touch { 44.0 } else { 28.0 };

    view().child(dynamic(move || {
        let cur_style = normalize_stage_style(store.stage_style.get());
        let sparks_on = store.settings.with(|s| s.stage_sparks);
        let _loc = store.locale.get();

        let mut actions: Vec<ContextMenuAction> = FOLIA_STYLES
            .iter()
            .copied()
            .map(|(st, key)| {
                let mut act = ContextMenuAction::new(store.t(key), move || {
                    store.set_stage_style(st);
                });
                if cur_style == st {
                    act = act.icon(Icon::Check);
                }
                act
            })
            .collect();

        let mut sparks_act = ContextMenuAction::new(store.t("now_playing.sparks"), move || {
            store.set_stage_sparks(!sparks_on);
        })
        .separator_before();
        if sparks_on {
            sparks_act = sparks_act.icon(Icon::Check);
        }
        actions.push(sparks_act);

        let trigger = button()
            .test_id("stage-style-menu-btn")
            .accessibility_label(move || Some(store.t("now_playing.stage_style")))
            .min_h(px(min_h))
            .pl_3p5()
            .pr_2p5()
            .flex_row()
            .items_center()
            .gap_1()
            .rounded_full()
            .bg(colors::TRANSPARENT)
            .transition_colors()
            .duration(move || motion::transition_ms(120.0))
            .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
            .active(|s| s.scale(motion::press_scale))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(
                text(move || store.t(style_label_key(store.stage_style.get())))
                    .number_of_lines(1)
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || theme::np_fg().opacity(0.68)),
            )
            .child(icon(Icon::ChevronDown, 12.0, colors::NP_FG).opacity(0.65));

        styled_menu(trigger, actions)
    }))
}

/// Compact inline Folia style pill bar (used on Phone Stage viewport for thumb ergonomics).
pub fn stage_style_switcher(store: AppStore, compact: bool) -> View {
    let touch = compact || theme::detect_device_form().is_touch();

    view()
        .flex_row()
        .items_center()
        .gap_1()
        .children(FOLIA_STYLES.iter().copied().map(move |(st, key)| {
            let active = move || normalize_stage_style(store.stage_style.get()) == st;
            button()
                .selected(active)
                .accessibility_label(move || Some(store.t(key)))
                .on_press(move || store.set_stage_style(st))
                .min_h(px(if touch { 44.0 } else { 28.0 }))
                .min_w(px(if touch { 44.0 } else { 0.0 }))
                .px(px(if touch { 2.0 } else { 0.0 }))
                .items_center()
                .justify_center()
                .bg(colors::TRANSPARENT)
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    view()
                        .h(px(if touch { 30.0 } else { 28.0 }))
                        .px_3()
                        .rounded_full()
                        .items_center()
                        .justify_center()
                        .bg(move || {
                            if active() {
                                theme::np_fg().opacity(0.16)
                            } else if touch {
                                theme::np_fg().opacity(0.06)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .border_1()
                        .border_color(move || {
                            if active() {
                                theme::np_fg().opacity(0.14)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .transition_colors()
                        .duration(move || motion::transition_ms(140.0))
                        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
                        .child(
                            text(move || store.t(key))
                                .number_of_lines(1)
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || {
                                    if active() {
                                        theme::np_fg()
                                    } else {
                                        theme::np_fg().opacity(0.62)
                                    }
                                }),
                        ),
                )
        }))
        .child({
            let sparks_on = move || store.settings.with(|s| s.stage_sparks);
            button()
                .selected(sparks_on)
                .accessibility_label(move || Some(store.t("now_playing.sparks")))
                .on_press(move || {
                    let cur = store.settings.with_untracked(|s| s.stage_sparks);
                    store.set_stage_sparks(!cur);
                })
                .min_h(px(if touch { 44.0 } else { 28.0 }))
                .min_w(px(if touch { 44.0 } else { 0.0 }))
                .px(px(if touch { 2.0 } else { 0.0 }))
                .items_center()
                .justify_center()
                .bg(colors::TRANSPARENT)
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    view()
                        .h(px(if touch { 30.0 } else { 28.0 }))
                        .px_3()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .rounded_full()
                        .bg(move || {
                            if sparks_on() {
                                theme::np_fg().opacity(0.16)
                            } else if touch {
                                theme::np_fg().opacity(0.06)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .border_1()
                        .border_color(move || {
                            if sparks_on() {
                                theme::np_fg().opacity(0.14)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .transition_colors()
                        .duration(move || motion::transition_ms(140.0))
                        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
                        .child(icon_reactive(Icon::Sparkles, 13.0, move || {
                            if sparks_on() {
                                AdaptiveColor::from(theme::artwork_np_accent(
                                    store.current_accent.get(),
                                ))
                            } else {
                                theme::np_fg().opacity(0.54)
                            }
                        }))
                        .child(
                            text(move || store.t("now_playing.sparks"))
                                .number_of_lines(1)
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || {
                                    if sparks_on() {
                                        theme::np_fg()
                                    } else {
                                        theme::np_fg().opacity(0.62)
                                    }
                                }),
                        ),
                )
        })
}

/// Deterministic pseudo-random float in `[0.0, 1.0)` matching `mygo-music` `lineRandom`.
fn line_random(seed: f64, offset: f64) -> f32 {
    let x = (seed + offset).sin() * 10_000.0;
    (x - x.floor()) as f32
}

/// Full-window Stage lyric visualizer panel + slim `stageBar` along the foot.
pub fn stage_panel(store: AppStore, form: DeviceForm) -> View {
    let is_phone = form.is_phone();
    let is_empty = computed(move || store.lyrics.with(|l| l.is_empty()));
    let active_idx = computed(move || store.active_lyric.get().map(|a| a.line_index));
    let (quiet_comment, has_quiet) = lyrics_view::quiet_comment_state(store);

    // Determine whether we are in an intro or a >4s vocal break (showing 3-dot interlude)
    let interlude_span = computed(move || -> Option<(i64, i64)> {
        let pos = store.position_ms.get() as i64;
        store.lyrics.with(|l| {
            if !l.synced || l.lines.is_empty() {
                return None;
            }
            let cur = active_idx.get();
            match cur {
                None => {
                    let first = l.lines.first()?;
                    if first.start_ms > 1_200 && pos < first.start_ms {
                        Some((0, first.start_ms))
                    } else {
                        None
                    }
                }
                Some(idx) => {
                    let line = l.lines.get(idx)?;
                    let next = l.lines.get(idx + 1)?;
                    let end = if line.end_ms > line.start_ms && line.end_ms < next.start_ms {
                        line.end_ms
                    } else {
                        return None;
                    };
                    if next.start_ms - end > 4_000 && pos > end + 1_000 && pos < next.start_ms {
                        Some((end, next.start_ms))
                    } else {
                        None
                    }
                }
            }
        })
    });

    let base_font = move || {
        let w = window().width.0;
        if is_phone {
            (w * 0.072).clamp(24.0, 34.0)
        } else {
            (w * 0.045).clamp(36.0, 64.0)
        }
    };

    view()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        // Main theatrical stage viewport
        .child(
            view()
                .relative()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .items_center()
                .justify_center()
                // Active visualizer or empty/skeleton state (dimmed under Quiet-Part Top Comment)
                .child(
                    view()
                        .relative()
                        .flex_1()
                        .w_full()
                        .min_h(px(0.0))
                        .items_center()
                        .justify_center()
                        .transition_all()
                        .duration(move || motion::transition_ms(600.0))
                        .opacity(move || if has_quiet.get() { 0.06_f32 } else { 1.0_f32 })
                        .child(
                            when(is_empty, move || {
                                when(
                                    move || store.lyrics_loading.get(),
                                    move || stage_skeleton(base_font()),
                                )
                                .otherwise(move || {
                                    text(move || store.t("now_playing.lyrics.enjoy"))
                                        .text_center()
                                        .px_6()
                                        .text_size(px(if is_phone { 20.0 } else { 28.0 }))
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        .text_color(move || theme::np_fg().opacity(0.40))
                                })
                            })
                            .otherwise(move || {
                                view()
                                    .relative()
                                    .flex_1()
                                    .w_full()
                                    .min_h(px(0.0))
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        when(
                                            move || interlude_span.with(Option::is_some),
                                            move || stage_interlude_dots(store, interlude_span),
                                        )
                                        .otherwise(
                                            move || {
                                                stage_active_line_host(store, active_idx, is_phone)
                                            },
                                        ),
                                    )
                                    // Footer along the foot of the stage: translation + next-line preview
                                    .child(stage_footer_captions(store, active_idx, is_phone))
                            }),
                        ),
                )
                // Centered Quiet-Part Top Comment quote (`ui_quiet.go:3850-3857`)
                .child(when(has_quiet, move || {
                    view()
                        .absolute()
                        .inset_0()
                        .px(px(if is_phone { 20.0 } else { 24.0 }))
                        .items_center()
                        .justify_center()
                        .pointer_events(PointerEvents::BoxNone)
                        .child(
                            view()
                                .w_full()
                                .max_w(px(if is_phone { 360.0 } else { 760.0 }))
                                .child(lyrics_view::quiet_comment_card(
                                    store,
                                    quiet_comment,
                                    if is_phone { 20.0 } else { 28.0 },
                                    form,
                                )),
                        )
                })),
        )
        // Slim `stageBar` along the foot (`ui_nowplaying.go:1511-1539`)
        .child(stage_bottom_bar(store, form))
}

fn stage_skeleton(f: f32) -> View {
    let widths = [2.2_f32, 1.3, 3.1, 1.8];
    view()
        .w_full()
        .px(px(if theme::detect_device_form().is_phone() {
            20.0
        } else {
            48.0
        }))
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_center()
        .gap(px((f * 0.35).clamp(10.0, 22.0)))
        .children(widths.into_iter().map(move |mult| {
            shimmer_block_on_dark((f * 0.22).clamp(6.0, 14.0))
                .w(px((f * mult).min(220.0)))
                .h(px((f * 0.85).clamp(24.0, 54.0)))
        }))
}

/// Three breathing dots centered on stage filling with interlude progress (`ui_stage.go:2574-2583`).
fn stage_interlude_dots(store: AppStore, span: Memo<Option<(i64, i64)>>) -> View {
    let progress_memo = computed(move || {
        let pos = store.position_ms.get() as i64;
        span.get()
            .map(|(from, to)| {
                let dur = (to - from).max(100) as f32;
                ((pos - from) as f32 / dur).clamp(0.0, 1.0)
            })
            .unwrap_or(0.5)
    });

    view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(18.0))
        .pointer_events(PointerEvents::None)
        .children((0..3_usize).map(move |i| {
            let dot_alpha = move || {
                let lit = (progress_memo.get() * 3.0 - i as f32).clamp(0.0, 1.0);
                0.20 + 0.70 * lit
            };
            view()
                .size(px(16.0))
                .rounded_full()
                .bg(move || theme::np_fg().opacity(dot_alpha()))
                .transition_all()
                .duration(move || motion::transition_ms(260.0))
        }))
}

/// Renders the ONE active lyric line dynamically keyed on `(line_index, stage_style)`
/// so each line enters with fresh spring choreography and zero full-line thrashing.
fn stage_active_line_host(
    store: AppStore,
    active_idx: Memo<Option<usize>>,
    is_phone: bool,
) -> View {
    view()
        .w_full()
        .flex_1()
        .items_center()
        .justify_center()
        .child(dynamic(move || {
            let idx = active_idx.get().unwrap_or(0);
            let style = normalize_stage_style(store.stage_style.get());
            let (line_opt, next_start) = store.lyrics.with(|l| {
                let cur = l.lines.get(idx).cloned();
                let next = l.lines.get(idx + 1).map(|n| n.start_ms);
                (cur, next)
            });

            untracked(|| {
                let Some(line) = line_opt else {
                    return view().key("stage-empty-line");
                };
                let words = spread_words(&line, next_start);
                if words.is_empty() {
                    return view().key("stage-blank-line");
                }
                let f = if is_phone {
                    (window().width.0 * 0.072).clamp(24.0, 34.0)
                } else {
                    (window().width.0 * 0.045).clamp(36.0, 64.0)
                };
                match style {
                    NowPlayingStageStyle::Steps => {
                        steps_line_view(store, &line, words, f, is_phone)
                            .key(format!("stage-steps-{idx}-{}", line.start_ms))
                    }
                    NowPlayingStageStyle::Tilt => tilt_line_view(store, &line, words, f, is_phone)
                        .key(format!("stage-tilt-{idx}-{}", line.start_ms)),
                    _ => flow_line_view(store, &line, words, f, is_phone)
                        .key(format!("stage-flow-{idx}-{}", line.start_ms)),
                }
            })
        }))
}

/// **Flow** style (`ui_stage.go:2504-2570`): scattered word springs + glowing accent halo.
fn flow_line_view(
    store: AppStore,
    line: &ParsedLyricLine,
    words: Vec<LyricWord>,
    f: f32,
    is_phone: bool,
) -> View {
    let seed = line.start_ms as f64;
    let align_slot = (line.start_ms.unsigned_abs() as usize) % 3;
    let lift_pct = match ((line.start_ms.unsigned_abs() * 2) as usize) % 3 {
        0 => -26.0_f32,
        1 => 0.0_f32,
        _ => 22.0_f32,
    };
    let reduced = reduce_motion();

    let enter_fade = animated_value(if reduced { 1.0 } else { 0.0 });
    if !reduced {
        motion::animate_timing(&enter_fade, 1.0, motion::timing_fast());
    }

    let word_count = words.len();
    let pad_x = if is_phone { 20.0 } else { 88.0 };

    view()
        .w_full()
        .max_w(px(1120.0))
        .px(px(pad_x))
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px((f * 0.22).clamp(6.0, 16.0)))
        .gap_y(px((f * 0.28).clamp(8.0, 20.0)))
        .translate_y(px(if reduced || is_phone { 0.0 } else { lift_pct }))
        .bind_opacity(&enter_fade)
        .style_with(move |s| match align_slot {
            0 if !is_phone => s.justify_start(),
            2 if !is_phone => s.justify_end(),
            _ => s.justify_center(),
        })
        .children(words.into_iter().enumerate().map(move |(i, word)| {
            let ws = seed + i as f64;
            let scatter_x = if reduced || is_phone {
                0.0
            } else {
                (line_random(ws, 1.0) - 0.5) * 26.0
            };
            let scatter_y = if reduced || is_phone {
                0.0
            } else {
                (line_random(ws, 2.0) - 0.5) * 24.0
            };
            let rot = if reduced {
                0.0
            } else {
                (line_random(ws, 3.0) - 0.5) * 9.0
            };
            let base_scale = if reduced {
                1.0
            } else {
                1.04 + line_random(ws, 4.0) * 0.14
            };
            let is_last = i + 1 == word_count;

            stage_word_node(
                store, word, f, scatter_x, scatter_y, rot, base_scale, is_last, false,
            )
        }))
}

/// **Steps** (`partitaLine`, `ui_stage_styles.go:2736-2853`): staggered alternating rows
/// with architectural corner L-guides lit in the cover's `artwork_np_accent`.
fn steps_line_view(
    store: AppStore,
    line: &ParsedLyricLine,
    words: Vec<LyricWord>,
    f: f32,
    is_phone: bool,
) -> View {
    let row_font = (f * 0.80).clamp(22.0, 50.0);
    let seed = line.start_ms as f64 / 1000.0;
    let row_count = if words.len() <= 2 {
        1
    } else if words.len() <= 5 {
        2
    } else {
        3.min(words.len())
    };
    let reduced = reduce_motion();

    // Split words evenly into staggered rows
    let mut rows: Vec<Vec<LyricWord>> = Vec::with_capacity(row_count);
    let mut start = 0;
    for r in 0..row_count {
        let remaining_words = words.len() - start;
        let remaining_rows = row_count - r;
        let take = remaining_words.div_ceil(remaining_rows).max(1);
        let end = (start + take).min(words.len());
        rows.push(words[start..end].to_vec());
        start = end;
    }

    let total_rows = rows.len();
    view()
        .w_full()
        .max_w(px(1040.0))
        .px(px(if is_phone { 20.0 } else { 64.0 }))
        .items_center()
        .justify_center()
        .gap(px(if is_phone { 12.0 } else { 20.0 }))
        .children(rows.into_iter().enumerate().map(move |(ri, row_words)| {
            let left_side = ri.is_multiple_of(2);
            let stagger = if reduced || is_phone {
                0.0
            } else {
                let mag = 22.0 + line_random(seed, (ri + 1) as f64 * 3.7) * 64.0;
                if left_side { -mag } else { mag }
            };
            let row_start_ms = row_words.first().map(|w| w.start).unwrap_or(0);
            let row_end_ms = row_words
                .last()
                .map(|w| w.end.max(w.start + 180))
                .unwrap_or(row_start_ms + 400);

            let row_phase = computed(move || -> u8 {
                let pos = store.position_ms.get() as i64;
                if pos + 80 < row_start_ms {
                    0 // waiting
                } else if pos <= row_end_ms {
                    1 // singing
                } else {
                    2 // sung
                }
            });

            let slide_x = animated_value(if reduced {
                stagger
            } else {
                stagger + if left_side { -36.0 } else { 36.0 }
            });
            let row_op = animated_value(if reduced { 1.0 } else { 0.0 });
            let triggered = stored(reduced);

            effect(move || {
                let phase = row_phase.get();
                if phase >= 1 && !triggered.get() {
                    triggered.replace(true);
                    motion::animate_spring(&slide_x, stagger, motion::spring_gentle());
                    motion::animate_timing(&row_op, 1.0, motion::timing_fast());
                }
            });

            let is_last_row = ri + 1 == total_rows;
            let row_len = row_words.len();

            view()
                .relative()
                .px_3()
                .py_1p5()
                .flex_row()
                .flex_wrap()
                .items_center()
                .justify_center()
                .gap_x(px((row_font * 0.18).clamp(4.0, 12.0)))
                .bind_translate_x(&slide_x)
                .bind_opacity(&row_op)
                // Architectural corner L-guide (`ui_stage_styles.go:2832-2845`)
                .child(
                    canvas(move || {
                        let phase = row_phase.get();
                        let accent = theme::artwork_np_accent(store.current_accent.get());
                        let fg = color(theme::np_fg());
                        let col = match phase {
                            1 => accent,
                            2 => fg.alpha(0.24),
                            _ => fg.alpha(0.10),
                        };
                        paint_corner_l_guide(left_side, col)
                    })
                    .absolute()
                    .inset_0()
                    .size_full()
                    .pointer_events(PointerEvents::None),
                )
                .children(row_words.into_iter().enumerate().map(move |(wi, w)| {
                    let is_last = is_last_row && wi + 1 == row_len;
                    stage_word_node(store, w, row_font, 0.0, 0.0, 0.0, 1.0, is_last, false)
                }))
        }))
}

fn paint_corner_l_guide(left_side: bool, col: Hsla) -> Painter {
    // Draws crisp minimal corner L-bracket framing the staggered row
    let mut p = Painter::new()
        .stroke_color(col)
        .line_width(1.2)
        .line_cap(LineCap::Round);
    if left_side {
        p = p
            .begin_path()
            .move_to(2.0, 6.0)
            .line_to(2.0, 28.0)
            .move_to(2.0, 28.0)
            .line_to(52.0, 28.0)
            .stroke();
    } else {
        p = p
            .begin_path()
            .move_to(2.0, 28.0)
            .line_to(52.0, 28.0)
            .move_to(52.0, 6.0)
            .line_to(52.0, 28.0)
            .stroke();
    }
    p
}

/// **Tilt** (`tiltLine`, `ui_stage_styles.go:2870-3050`): dramatic centered editorial
/// phrases where one phrase leans italic in the cover's `artwork_np_accent`.
fn tilt_line_view(
    store: AppStore,
    line: &ParsedLyricLine,
    words: Vec<LyricWord>,
    f: f32,
    is_phone: bool,
) -> View {
    let seed = line.start_ms as f64 / 1000.0;
    let part_count = if words.len() <= 2 {
        1
    } else if words.len() <= 5 {
        2
    } else {
        3.min(words.len())
    };
    let lean_idx = if part_count > 1 {
        ((line_random(seed, 200.0) * part_count as f32) as usize).min(part_count - 1)
    } else if line_random(seed, 100.0) < 0.35 {
        0
    } else {
        usize::MAX
    };

    let mut parts: Vec<Vec<LyricWord>> = Vec::with_capacity(part_count);
    let mut start = 0;
    for k in 0..part_count {
        let rem = words.len() - start;
        let rem_parts = part_count - k;
        let take = rem.div_ceil(rem_parts).max(1);
        let end = (start + take).min(words.len());
        parts.push(words[start..end].to_vec());
        start = end;
    }

    let poster_size = if is_phone {
        (f * 1.15).clamp(28.0, 42.0)
    } else {
        (f * 1.25).clamp(44.0, 74.0)
    };
    let total_parts = parts.len();
    let reduced = reduce_motion();

    view()
        .w_full()
        .max_w(px(1080.0))
        .px(px(if is_phone { 20.0 } else { 56.0 }))
        .items_center()
        .justify_center()
        .gap(px(if is_phone { 8.0 } else { 14.0 }))
        .children(parts.into_iter().enumerate().map(move |(pi, part_words)| {
            let is_lean = pi == lean_idx;
            let is_tail = !is_lean && pi + 1 == total_parts && part_words.len() == 1;
            let size_pt = if is_tail {
                poster_size * 1.14
            } else {
                poster_size
            };
            let part_start = part_words.first().map(|w| w.start).unwrap_or(0);

            let entered = computed(move || {
                let pos = store.position_ms.get() as i64;
                pos + 250 >= part_start
            });
            let rise = animated_value(if reduced { 0.0 } else { 20.0 });
            let op = animated_value(if reduced { 1.0 } else { 0.0 });
            let done = stored(reduced);

            effect(move || {
                if entered.get() && !done.get() {
                    done.replace(true);
                    motion::animate_spring(&rise, 0.0, motion::spring_gentle());
                    motion::animate_timing(&op, 1.0, motion::timing_base());
                }
            });

            let part_len = part_words.len();
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .justify_center()
                .gap_x(px(
                    (size_pt * if is_lean { 0.28 } else { 0.18 }).clamp(5.0, 18.0)
                ))
                .bind_translate_y(&rise)
                .bind_opacity(&op)
                .children(part_words.into_iter().enumerate().map(move |(wi, w)| {
                    let is_last = pi + 1 == total_parts && wi + 1 == part_len;
                    let bob_y = if is_lean && !reduced {
                        if wi.is_multiple_of(2) { -4.0 } else { 4.0 }
                    } else {
                        0.0
                    };
                    stage_word_node(store, w, size_pt, 0.0, bob_y, 0.0, 1.0, is_last, is_lean)
                }))
        }))
}

/// Animated individual Stage word with spring entrance, glowing accent layer,
/// and optional radial `stage_sparks` streaks when landing.
#[allow(clippy::too_many_arguments)]
fn stage_word_node(
    store: AppStore,
    word: LyricWord,
    font_pt: f32,
    scatter_x: f32,
    scatter_y: f32,
    rotate_deg: f32,
    base_scale: f32,
    is_last: bool,
    italic_accent: bool,
) -> View {
    let start_ms = word.start;
    let end_ms = word.end.max(word.start + 180);
    let label = SharedString::from(word.text);
    let glow_label = label.clone();
    let reduced = reduce_motion();

    // 0 = waiting, 1 = active/singing, 2 = sung/resting
    let phase = computed(move || -> u8 {
        let pos = store.position_ms.get() as i64;
        if pos + 80 < start_ms {
            0
        } else if pos <= end_ms {
            1
        } else {
            2
        }
    });

    let scale_anim = animated_value(if reduced {
        1.0
    } else if untracked(|| phase.get()) >= 1 {
        base_scale
    } else {
        0.72
    });
    let op_anim = animated_value(if reduced || untracked(|| phase.get()) >= 1 {
        1.0
    } else {
        0.0
    });
    let prev_phase = stored(untracked(|| phase.get()));

    effect(move || {
        let p = phase.get();
        if prev_phase.replace(p) != p {
            if reduced {
                scale_anim.set(1.0);
                op_anim.set(1.0);
            } else {
                match p {
                    0 => {
                        scale_anim.set(0.72);
                        op_anim.set(0.0);
                    }
                    1 => {
                        motion::animate_spring(
                            &scale_anim,
                            (base_scale * 1.14).min(1.26),
                            motion::spring_snappy(),
                        );
                        motion::animate_timing(&op_anim, 1.0, motion::timing_instant());
                    }
                    _ => {
                        motion::animate_spring(&scale_anim, base_scale, motion::spring_gentle());
                        motion::animate_timing(&op_anim, 0.78, motion::timing_base());
                    }
                }
            }
        }
    });

    let show_sparks = computed(move || {
        if reduced || !store.settings.with(|s| s.stage_sparks) {
            return false;
        }
        let pos = store.position_ms.get() as i64;
        let since = pos - start_ms;
        (0..600).contains(&since)
    });

    let spark_seed = start_ms as f64;

    view()
        .relative()
        .translate_x(px(scatter_x))
        .translate_y(px(scatter_y))
        .rotate(deg(rotate_deg))
        .bind_scale(&scale_anim)
        .bind_opacity(&op_anim)
        // Glowing accent bloom behind the active sung word (`ui_stage.go:2561-2565`)
        .child(when(
            move || phase.get() == 1 && !reduced,
            move || {
                text(glow_label.clone())
                    .absolute()
                    .inset_0()
                    .text_size(px(font_pt))
                    .font_weight(if italic_accent {
                        FontWeight::LIGHT
                    } else {
                        FontWeight::EXTRA_BOLD
                    })
                    .when(italic_accent, |t| t.italic())
                    .letter_spacing(px(if italic_accent { font_pt * 0.12 } else { 0.0 }))
                    .blur(px((font_pt * 0.18).clamp(6.0, 12.0)))
                    .text_color(move || {
                        AdaptiveColor::from(
                            theme::artwork_np_accent(store.current_accent.get()).alpha(0.78),
                        )
                    })
                    .pointer_events(PointerEvents::None)
            },
        ))
        // Crisp foreground word glyph
        .child(
            text(label)
                .text_size(px(font_pt))
                .font_weight(if italic_accent {
                    FontWeight::LIGHT
                } else {
                    FontWeight::EXTRA_BOLD
                })
                .when(italic_accent, |t| t.italic())
                .line_height(LineHeight::Multiple(1.2))
                .letter_spacing(px(if italic_accent { font_pt * 0.12 } else { 0.0 }))
                .text_color(move || {
                    let p = phase.get();
                    if italic_accent {
                        let acc = theme::artwork_np_accent(store.current_accent.get());
                        AdaptiveColor::from(acc.alpha(if p == 1 { 1.0 } else { 0.78 }))
                    } else {
                        match p {
                            1 => theme::np_fg(),
                            2 => theme::np_fg().opacity(0.62),
                            _ => theme::np_fg().opacity(0.42),
                        }
                    }
                }),
        )
        // Radial Stage Sparks burst when enabled (`ui_stage_styles.go:2653-2677`)
        .child(when(show_sparks, move || {
            canvas(move || {
                let pos = store.position_ms.get() as i64;
                let t = ((pos - start_ms) as f32 / 600.0).clamp(0.0, 1.0);
                let accent = theme::artwork_np_accent(store.current_accent.get());
                paint_stage_sparks(t, spark_seed, is_last, accent)
            })
            .absolute()
            .top(px(-22.0))
            .left(px(-22.0))
            .right(px(-22.0))
            .bottom(px(-22.0))
            .pointer_events(PointerEvents::None)
        }))
}

fn paint_stage_sparks(t: f32, seed: f64, is_last: bool, accent: Hsla) -> Painter {
    let fg = color(theme::np_fg());
    let out = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
    let fade = (1.0 - t) * (1.0 - t);
    let col = accent.mix(fg, 0.6 * (1.0 - t)).alpha(0.9 * fade);
    let n: usize = if is_last { 14 } else { 9 };
    let reach = if is_last { 42.0_f32 } else { 28.0_f32 };
    let cx = 44.0_f32;
    let cy = 36.0_f32;

    let mut p = Painter::new()
        .stroke_color(col)
        .line_width(0.9 + 1.5 * (1.0 - t))
        .line_cap(LineCap::Round);

    for i in 0..n {
        let jitter = line_random(seed, i as f64);
        let angle = TAU * (i as f32 + jitter * 0.7) / n as f32;
        let (dy, dx) = angle.sin_cos();
        let edge = 12.0;
        let head = edge + reach * (0.55 + 0.45 * line_random(seed, i as f64 + 50.0)) * out;
        let tail = (head - reach * 0.30 * (1.0 - t)).max(edge);
        p = p
            .begin_path()
            .move_to(cx + dx * tail, cy + dy * tail)
            .line_to(cx + dx * head, cy + dy * head)
            .stroke();
    }
    p
}

/// Footer along the foot of the stage: current translation (`18pt`) & next lyric (`14pt`).
fn stage_footer_captions(store: AppStore, active_idx: Memo<Option<usize>>, is_phone: bool) -> View {
    let trans_memo = computed(move || {
        if !store.show_translation.get() {
            return SharedString::default();
        }
        let Some(idx) = active_idx.get() else {
            return SharedString::default();
        };
        store.lyrics.with(|l| {
            l.lines
                .get(idx)
                .and_then(|line| line.translation.clone())
                .map(SharedString::from)
                .unwrap_or_default()
        })
    });

    let next_memo = computed(move || {
        let idx = active_idx.get().map(|i| i + 1).unwrap_or(0);
        store.lyrics.with(|l| {
            l.lines
                .get(idx)
                .map(|line| SharedString::from(line.text.clone()))
                .unwrap_or_default()
        })
    });

    view()
        .absolute()
        .left_0()
        .right_0()
        .bottom(px(if is_phone { 10.0 } else { 22.0 }))
        .px(px(if is_phone { 20.0 } else { 24.0 }))
        .items_center()
        .gap_2()
        .pointer_events(PointerEvents::None)
        .child(when(
            move || trans_memo.with(|s| !s.is_empty()),
            move || {
                text(move || trans_memo.get())
                    .text_center()
                    .number_of_lines(2)
                    .text_size(px(if is_phone { 14.5 } else { 18.0 }))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || theme::np_fg().opacity(0.72))
            },
        ))
        .child(when(
            move || next_memo.with(|s| !s.is_empty()),
            move || {
                text(move || next_memo.get())
                    .text_center()
                    .number_of_lines(1)
                    .text_size(px(if is_phone { 12.5 } else { 14.0 }))
                    .text_color(move || theme::np_fg().opacity(0.32))
            },
        ))
}

/// Slim `stageBar` along the foot of Stage (`ui_nowplaying.go:1511-1539`):
/// - Desktop & Pad: smoke scrubber + single row (`44pt` cover + title/artists/like/more · transport · `elapsed / duration`).
/// - Phone: stacked `20pt`-padded scrubber + `elapsed / Lossless-or-quality / remaining` + evenly distributed thumb transport
///   (track identity & quick pause live in `phone_mini_track_bar` above so nothing cramps at `390pt`).
fn stage_bottom_bar(store: AppStore, form: DeviceForm) -> View {
    let touch = form.is_touch();
    let is_phone = form.is_phone();

    if is_phone {
        return view()
            .w_full()
            .px(px(20.0))
            .pt_1()
            .gap_1p5()
            .child(
                view()
                    .w_full()
                    .gap_0p5()
                    .child(stage_scrubber_bar(store, false))
                    .child(phone_scrubber_meta_row(store)),
            )
            .child(phone_thumb_transport(store, 54.0));
    }

    let has_art = computed(move || store.current_artwork_uri.with(|u| !u.is_empty()));
    let is_desktop = form.is_desktop();

    view()
        .w_full()
        .pl(move || {
            px(if is_desktop {
                48.0 + safe_area().left.0
            } else {
                48.0
            })
        })
        .pr(move || {
            px(if is_desktop {
                48.0 + safe_area().right.0
            } else {
                48.0
            })
        })
        .pb(move || {
            px(if is_desktop {
                28.0 + safe_area().bottom.0
            } else {
                28.0
            })
        })
        .gap_2p5()
        .child(stage_scrubber_bar(store, false))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3p5()
                // Left: 44pt cover + title + clickable artists + like + `···` menu
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            view()
                                .size(px(44.0))
                                .flex_shrink_0()
                                .rounded(px(8.0))
                                .overflow_hidden()
                                .bg(theme::np_surface)
                                .items_center()
                                .justify_center()
                                .child(when(
                                    move || !has_art.get(),
                                    || icon(Icon::Disc, 18.0, colors::NP_MUTED).opacity(0.5),
                                ))
                                .child(when(has_art, move || {
                                    image(move || store.current_artwork_uri.get())
                                        .object_fit(ResizeMode::Cover)
                                        .size_full()
                                })),
                        )
                        .child(
                            view()
                                .flex_1()
                                .min_w(px(0.0))
                                .gap_0p5()
                                .child(
                                    text(move || {
                                        store.current_track.with(|t| {
                                            t.as_ref()
                                                .map(|tr| SharedString::from(tr.name.clone()))
                                                .unwrap_or_else(|| store.t("player.idle_title"))
                                        })
                                    })
                                    .number_of_lines(1)
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::np_fg),
                                )
                                .child(clickable_artist_links(store, 12.0, 0.55)),
                        )
                        .child(stage_like_button(store, 17.0, touch))
                        .child(song_more_menu(store, 17.0, touch)),
                )
                // Center: borderless white transport
                .child(stage_transport(store, form, 22.0, false))
                // Right: elapsed / duration or buffering spinner (Desktop & Pad)
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .child(
                            when(
                                move || store.playback_status.with(PlaybackStatus::is_loading),
                                move || buffering_badge(store),
                            )
                            .otherwise(move || {
                                text(move || {
                                    let pos = format_duration_ms(store.position_ms.get() as i64);
                                    let dur = format_duration_ms(store.duration_ms.get() as i64);
                                    format!("{pos} / {dur}")
                                })
                                .tabular_nums()
                                .text_size(px(font_size::MICRO))
                                .text_color(move || theme::np_fg().opacity(0.50))
                            }),
                        ),
                ),
        )
}
