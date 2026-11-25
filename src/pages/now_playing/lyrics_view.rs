//! Synced lyrics view using `store.lyrics` & `store.active_lyric` with:
//! - Smooth spring focus elevation & 320ms transitions honoring `reduce_motion()`
//! - Word-by-word karaoke emphasis + optional `stage_sparks` micro-glint
//! - Toggleable translation & romaji secondary lines
//! - Quiet-Part Top Comments (`mygo-music` `ui_quiet.go`) during long quiet passages
//! - 3-dot breathing musical interlude indicator during silent gaps
//! - Click/tap any synced line to `store.seek(...)`
//! - Clean plain-text and empty-lyrics fallbacks

use std::f32::consts::TAU;

use nsh_base::button;
use nsh_core::{geometry::percentage, prelude::*};
use nsh_music_core::{
    lyrics::{ParsedLyricLine, ParsedLyrics},
    model::Comment,
};

use super::stage::{self, StageAlign};
use crate::{
    state::{AppStore, NowPlayingPanel},
    ui::{
        icons::{Icon, icon},
        motion,
        theme::{self, DeviceForm, colors, font_size, radius},
    },
};

/// Minimum quiet passage duration (ms) to surface top listener comments (`mygo-music` `ui_quiet.go`).
const QUIET_LEAST_MS: i64 = 8_000;
/// Lead-in buffer (ms) after the previous vocal ends before surfacing a quiet comment.
const QUIET_LEAD_MS: i64 = 1_200;
/// Tail-out buffer (ms) before the next vocal enters so lyrics return cleanly.
const QUIET_TAIL_MS: i64 = 1_000;
/// Rotation interval (ms) between eligible top comments during long instrumental passages.
const QUIET_SLOT_MS: i64 = 10_000;
/// Maximum character count for a comment to appear in the quiet-part glass card.
const QUIET_MAX_CHARS: usize = 140;
/// Minimum musical gap (ms) to display the 3-dot breathing interlude indicator.
const INTERLUDE_MIN_MS: i64 = 3_000;
/// Estimated vocal length (ms) when line-synced LRC lacks explicit per-line end timestamps.
const FALLBACK_LINE_HOLD_MS: i64 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuietWindow {
    from_ms: i64,
    to_ms: i64,
}

/// Detects intro, interlude, outro, or instrumental quiet passages >= `min_gap_ms`.
fn detect_quiet_windows(
    lyrics: &ParsedLyrics,
    duration_ms: i64,
    min_gap_ms: i64,
) -> Vec<QuietWindow> {
    if lyrics.is_empty() {
        if duration_ms >= min_gap_ms {
            return vec![QuietWindow {
                from_ms: 0,
                to_ms: duration_ms,
            }];
        }
        return Vec::new();
    }

    if !lyrics.synced {
        return Vec::new();
    }

    let mut windows = Vec::new();
    let lines = &lyrics.lines;

    // 1. Intro silence before the first timed lyric line
    if let Some(first) = lines.first()
        && first.start_ms >= min_gap_ms
    {
        windows.push(QuietWindow {
            from_ms: 0,
            to_ms: first.start_ms,
        });
    }

    // 2. Interludes between consecutive timed lines
    for pair in lines.windows(2) {
        let cur = &pair[0];
        let next = &pair[1];
        if next.start_ms <= cur.start_ms {
            continue;
        }
        let gap_start = if cur.end_ms > cur.start_ms && cur.end_ms < next.start_ms {
            cur.end_ms
        } else if next.start_ms - cur.start_ms >= min_gap_ms + FALLBACK_LINE_HOLD_MS {
            cur.start_ms + FALLBACK_LINE_HOLD_MS
        } else {
            next.start_ms
        };
        if next.start_ms - gap_start >= min_gap_ms {
            windows.push(QuietWindow {
                from_ms: gap_start,
                to_ms: next.start_ms,
            });
        }
    }

    // 3. Outro silence after the final lyric line
    if let Some(last) = lines.last() {
        let outro_start = if last.end_ms > last.start_ms {
            last.end_ms
        } else {
            last.start_ms + FALLBACK_LINE_HOLD_MS
        };
        if duration_ms > outro_start && duration_ms - outro_start >= min_gap_ms {
            windows.push(QuietWindow {
                from_ms: outro_start,
                to_ms: duration_ms,
            });
        }
    }

    windows
}

fn active_window_for(
    windows: &[QuietWindow],
    pos_ms: i64,
    lead_ms: i64,
    tail_ms: i64,
) -> Option<QuietWindow> {
    windows
        .iter()
        .copied()
        .find(|w| pos_ms >= w.from_ms + lead_ms && pos_ms <= w.to_ms - tail_ms)
}

fn select_quiet_comment(comments: &[Comment], window: QuietWindow, pos_ms: i64) -> Option<Comment> {
    let mut eligible: Vec<&Comment> = comments
        .iter()
        .filter(|c| {
            let len = c.text.trim().chars().count();
            (1..=QUIET_MAX_CHARS).contains(&len)
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    // Highest likes first, preserving original order on ties
    eligible.sort_by_key(|a| std::cmp::Reverse(a.likes));
    let elapsed = (pos_ms - (window.from_ms + QUIET_LEAD_MS)).max(0);
    let slot = (elapsed / QUIET_SLOT_MS) as usize;
    Some(eligible[slot % eligible.len()].clone())
}

/// Renders the interactive lyrics panel adapted to the current [`DeviceForm`]
/// and [`crate::state::NowPlayingStageStyle`].
pub fn lyrics_view(store: AppStore, form: DeviceForm) -> View {
    let compact = form.is_phone();
    let list_ref = element_ref();

    let active_line_idx = computed(move || store.active_lyric.get().map(|a| a.line_index));
    let is_empty = computed(move || store.lyrics.with(|l| l.is_empty()));
    let is_synced = computed(move || store.lyrics.with(|l| l.synced));
    let has_translation = computed(move || {
        store
            .lyrics
            .with(|l| l.lines.iter().any(|line| line.translation.is_some()))
    });
    let has_romaji = computed(move || {
        store
            .lyrics
            .with(|l| l.lines.iter().any(|line| line.romaji.is_some()))
    });

    // Precompute quiet windows whenever lyrics or track duration change
    let quiet_windows = computed(move || {
        let dur = store.duration_ms.get() as i64;
        store
            .lyrics
            .with(|l| detect_quiet_windows(l, dur, QUIET_LEAST_MS))
    });
    let interlude_windows = computed(move || {
        let dur = store.duration_ms.get() as i64;
        store
            .lyrics
            .with(|l| detect_quiet_windows(l, dur, INTERLUDE_MIN_MS))
    });

    // Active silent interlude window (for 3-dot breathing indicator)
    let active_interlude = computed(move || {
        let pos = store.position_ms.get() as i64;
        let synced_or_inst = is_synced.get() || is_empty.get();
        if !synced_or_inst {
            return None;
        }
        interlude_windows.with(|ws| active_window_for(ws, pos, 350, 350))
    });

    // Active quiet-part top comment when settings.hot_comments & active_can("comments")
    let quiet_comment: Memo<Option<Comment>> = computed(move || {
        if !store.settings.with(|s| s.hot_comments) || !store.active_can("comments") {
            return None;
        }
        let pos = store.position_ms.get() as i64;
        let win = quiet_windows.with(|ws| active_window_for(ws, pos, QUIET_LEAD_MS, QUIET_TAIL_MS))?;
        store
            .comments
            .with(|list| select_quiet_comment(list, win, pos))
    });

    let has_quiet_comment = computed(move || quiet_comment.with(Option::is_some));

    // Smooth fade for the Quiet-Part Top Comment card
    let quiet_fade = animated_value(if untracked(|| has_quiet_comment.get()) {
        1.0
    } else {
        0.0
    });
    let prev_quiet = stored(untracked(|| has_quiet_comment.get()));
    effect(move || {
        let show = has_quiet_comment.get();
        if prev_quiet.replace(show) != show {
            let target = if show { 1.0 } else { 0.0 };
            motion::animate_timing(&quiet_fade, target, motion::timing_base());
        }
    });
    let quiet_lift = quiet_fade.interpolate(&[0.0, 1.0], &[12.0, 0.0]);

    // Auto-scroll active lyric line smoothly toward the upper-middle focal sweet spot
    let prev_scrolled = stored(None::<usize>);
    effect(move || {
        let idx = active_line_idx.get();
        let synced = is_synced.get();
        if synced && idx != prev_scrolled.get() {
            prev_scrolled.replace(idx);
            if let Some(target) = idx {
                list_ref.scroll_to_index(
                    target as u32,
                    percentage(0.35),
                    px(0.0),
                    !reduce_motion(),
                );
            }
        }
    });

    view()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .gap_2()
        // Lyrics utility header: unsynced badge + Translation / Romaji toggles
        .child(when(
            move || !is_empty.get(),
            move || lyrics_toolbar(store, is_synced, has_translation, has_romaji, form),
        ))
        // Subtle 3-dot breathing musical interlude indicator during silent gaps
        .child(when(
            move || active_interlude.with(Option::is_some),
            move || interlude_dots(store, active_interlude),
        ))
        // Body: empty state OR virtualized synced/plain lyrics list (softly dimmed during quiet comment)
        .child(
            view()
                .relative()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(
                    view()
                        .flex_1()
                        .w_full()
                        .min_h(px(0.0))
                        .transition_all()
                        .duration(move || motion::transition_ms(320.0))
                        .opacity(move || {
                            if has_quiet_comment.get() {
                                0.24_f32
                            } else {
                                1.0_f32
                            }
                        })
                        .child(
                            when(is_empty, move || lyrics_empty_state(store)).otherwise(move || {
                                virtual_list(
                                    move || store.lyrics.with(|l| l.lines.len()),
                                    move |at| {
                                        lyric_row_cell(
                                            store,
                                            at,
                                            active_line_idx,
                                            is_synced,
                                            compact,
                                        )
                                    },
                                )
                                .keys(move || {
                                    store.lyrics.with(|l| {
                                        l.lines
                                            .iter()
                                            .enumerate()
                                            .map(|(i, line)| format!("{i}:{}", line.start_ms))
                                            .collect::<Vec<_>>()
                                    })
                                })
                                .estimated_item_size(px(if compact { 64.0 } else { 76.0 }))
                                .content_padding_top(px(if compact { 28.0 } else { 64.0 }))
                                .content_padding_bottom(px(if compact { 64.0 } else { 140.0 }))
                                .content_padding_x(px(if compact { 8.0 } else { 16.0 }))
                                .row_gap(px(if compact { 10.0 } else { 14.0 }))
                                .scrollbar(false)
                                .element_ref(&list_ref)
                                .flex_1()
                                .w_full()
                            }),
                        ),
                )
                // Quiet-Part Top Comment floating glass card
                .child(when(has_quiet_comment, move || {
                    quiet_comment_overlay(store, quiet_comment, &quiet_fade, &quiet_lift, form)
                })),
        )
}

/// 3-dot breathing musical interlude indicator shown during silent passages.
fn interlude_dots(store: AppStore, active_interlude: Memo<Option<QuietWindow>>) -> View {
    view()
        .w_full()
        .h(px(22.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .pointer_events(PointerEvents::None)
        .child(
            canvas(move || {
                let reduced = reduce_motion();
                let playing = store.is_playing.get();
                let pos_ms = store.position_ms.get() as i64;
                let progress = active_interlude
                    .get()
                    .map(|w| {
                        let span = (w.to_ms - w.from_ms).max(1) as f32;
                        ((pos_ms - w.from_ms) as f32 / span).clamp(0.0, 1.0)
                    })
                    .unwrap_or(0.5);
                let accent = store.current_accent.get().lighten(0.32);
                let fg = color(theme::np_fg());
                let phase = if playing && !reduced {
                    (pos_ms as f32 / 1000.0) * 2.4
                } else {
                    0.0
                };

                let mut p = Painter::new();
                for i in 0..3_usize {
                    let cx = 14.0 + i as f32 * 16.0;
                    let cy = 11.0;
                    let wave = if playing && !reduced {
                        0.5 + 0.5 * (phase - i as f32 * 0.95).sin()
                    } else {
                        0.65
                    };
                    let lit = progress >= (i as f32 / 3.0);
                    let r = 3.0 + wave * 1.6;
                    let col = if lit {
                        accent.mix(fg, 0.35 + wave * 0.45).alpha(0.55 + wave * 0.45)
                    } else {
                        fg.alpha(0.28 + wave * 0.30)
                    };
                    p = p.fill_color(col).begin_path().arc(cx, cy, r, 0.0, TAU).fill();
                }
                p
            })
            .w(px(60.0))
            .h(px(22.0)),
        )
}

/// Clickable frosted-glass top comment card surfaced during long quiet passages.
fn quiet_comment_overlay(
    store: AppStore,
    quiet_comment: Memo<Option<Comment>>,
    fade: &AnimatedValue,
    lift: &AnimatedNode,
    form: DeviceForm,
) -> View {
    let touch = form.is_touch();

    view()
        .absolute()
        .inset_0()
        .px_4()
        .py_6()
        .items_center()
        .justify_center()
        .pointer_events(PointerEvents::BoxNone)
        .child(
            view()
                .w_full()
                .max_w(px(460.0))
                .bind_opacity(fade)
                .bind_translate_y(lift)
                .child(
                    button()
                        .test_id("quiet-comment-card")
                        .accessibility_label(move || Some(store.t("now_playing.view_comments")))
                        .on_press(move || store.set_now_playing_panel(NowPlayingPanel::Comments))
                        .w_full()
                        .min_h(px(if touch { 64.0 } else { 56.0 }))
                        .p_5()
                        .gap_3()
                        .rounded(px(radius::XL))
                        .bg(theme::np_surface_hover)
                        .border_1()
                        .border_color(theme::np_border)
                        .backdrop_blur(px(22.0))
                        .transition_all()
                        .duration(move || motion::transition_ms(220.0))
                        .hover(|s| s.border_color(theme::primary))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        // Top meta row: Comment badge + author + likes count
                        .child(
                            view()
                                .w_full()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    view()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(icon(Icon::Comment, 15.0, colors::PRIMARY))
                                        .child(
                                            text(move || {
                                                quiet_comment.with(|c| {
                                                    c.as_ref()
                                                        .map(|cmt| {
                                                            SharedString::from(cmt.author.clone())
                                                        })
                                                        .unwrap_or_default()
                                                })
                                            })
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .number_of_lines(1)
                                            .text_size(px(font_size::SUBTITLE))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::np_fg),
                                        ),
                                )
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .child(icon(Icon::HeartFilled, 13.0, colors::PRIMARY))
                                        .child(
                                            text(move || {
                                                quiet_comment.with(|c| {
                                                    c.as_ref()
                                                        .map(|cmt| {
                                                            SharedString::from(
                                                                cmt.likes.to_string(),
                                                            )
                                                        })
                                                        .unwrap_or_default()
                                                })
                                            })
                                            .tabular_nums()
                                            .text_size(px(font_size::CAPTION))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme::np_muted),
                                        ),
                                ),
                        )
                        // Comment body text
                        .child(
                            text(move || {
                                quiet_comment.with(|c| {
                                    c.as_ref()
                                        .map(|cmt| SharedString::from(cmt.text.trim().to_owned()))
                                        .unwrap_or_default()
                                })
                            })
                            .text_size(px(if form.is_phone() {
                                font_size::BODY_MOBILE
                            } else {
                                font_size::HEADING
                            }))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::np_fg),
                        )
                        // Bottom CTA: View Comments ->
                        .child(
                            view()
                                .w_full()
                                .flex_row()
                                .items_center()
                                .justify_end()
                                .gap_1()
                                .child(
                                    text(move || store.t("now_playing.view_comments"))
                                        .text_size(px(font_size::CAPTION))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::primary),
                                )
                                .child(icon(Icon::ChevronRight, 13.0, colors::PRIMARY)),
                        ),
                ),
        )
}

fn lyrics_toolbar(
    store: AppStore,
    is_synced: Memo<bool>,
    has_translation: Memo<bool>,
    has_romaji: Memo<bool>,
    form: DeviceForm,
) -> View {
    let touch = form.is_touch();
    let pill_h = if touch { 44.0 } else { 28.0 };

    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        // Left: Unsynced notice or word-synced badge
        .child(
            when(
                move || !is_synced.get(),
                move || {
                    view()
                        .px_2p5()
                        .py_1()
                        .rounded_full()
                        .bg(theme::np_surface)
                        .border_1()
                        .border_color(theme::np_border)
                        .child(
                            text(move || store.t("now_playing.lyrics.unsynced"))
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::np_muted),
                        )
                },
            )
            .otherwise(move || {
                view().flex_row().items_center().gap_1p5().child(when(
                    move || store.lyrics.with(|l| l.word_synced),
                    move || {
                        view()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme::primary_soft)
                            .child(icon(Icon::Sparkles, 12.0, colors::PRIMARY))
                            .child(
                                text("Karaoke")
                                    .text_size(px(font_size::MICRO))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::primary),
                            )
                    },
                ))
            }),
        )
        // Right: Translation & Romaji toggle pills
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1p5()
                .child(when(has_translation, move || {
                    stage_toggle_pill(
                        move || store.t("now_playing.lyrics.translation"),
                        move || store.show_translation.get(),
                        pill_h,
                        move || store.toggle_translation(),
                    )
                }))
                .child(when(has_romaji, move || {
                    stage_toggle_pill(
                        move || store.t("now_playing.lyrics.romaji"),
                        move || store.show_romaji.get(),
                        pill_h,
                        move || store.toggle_romaji(),
                    )
                })),
        )
}

fn stage_toggle_pill(
    label: impl IntoReactive<SharedString>,
    active: impl IntoReactive<bool>,
    height_pt: f32,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active_rx = active.into_reactive();
    let label_rx = label.into_reactive();
    let label_a11y = label_rx.clone();
    let a_bg = active_rx.clone();
    let a_border = active_rx.clone();
    let a_fg = active_rx.clone();

    button()
        .selected(move || active_rx.get())
        .accessibility_label(move || Some(label_a11y.get()))
        .on_press(on_press)
        .min_h(px(height_pt))
        .min_w(move || px(theme::detect_device_form().min_touch_target()))
        .px_3()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(move || {
            if a_border.get() {
                theme::primary()
            } else {
                theme::np_border()
            }
        })
        .bg(move || {
            if a_bg.get() {
                theme::primary_soft()
            } else {
                theme::np_surface()
            }
        })
        .transition_all()
        .duration(move || motion::transition_ms(160.0))
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            text(move || label_rx.get())
                .number_of_lines(1)
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || {
                    if a_fg.get() {
                        theme::primary()
                    } else {
                        theme::np_muted()
                    }
                }),
        )
}

fn lyric_row_cell(
    store: AppStore,
    at: Position,
    active_line_idx: Memo<Option<usize>>,
    is_synced: Memo<bool>,
    compact: bool,
) -> View {
    let line_memo: Memo<Option<ParsedLyricLine>> =
        computed(move || store.lyrics.with(|l| l.lines.get(at.index()).cloned()));
    let is_active = computed(move || is_synced.get() && active_line_idx.get() == Some(at.index()));
    let spec = computed(move || {
        stage::line_spec(
            store.stage_style.get(),
            at.index(),
            active_line_idx.get(),
            is_synced.get(),
            compact,
            reduce_motion(),
        )
    });

    // Spring-driven focus emphasis (snaps on cell recycle or reduce_motion())
    let initial_active = untracked(|| is_active.get());
    let focus_anim = animated_value(if initial_active && !reduce_motion() {
        1.0
    } else {
        0.0
    });
    let prev_cell = stored((untracked(|| at.index()), initial_active));
    effect(move || {
        let idx = at.index();
        let active = is_active.get();
        let (last_idx, last_active) = prev_cell.replace((idx, active));
        let target = if active && !reduce_motion() { 1.0 } else { 0.0 };
        if idx != last_idx || reduce_motion() {
            focus_anim.set(target);
        } else if active != last_active {
            motion::animate_spring(&focus_anim, target, motion::spring_gentle());
        }
    });
    let focus_scale = focus_anim.interpolate(&[0.0, 1.0], &[1.0, 1.025]);

    let show_word_karaoke = computed(move || {
        is_active.get()
            && line_memo.with(|l| l.as_ref().is_some_and(|line| !line.words.is_empty()))
    });

    let show_trans = computed(move || {
        store.show_translation.get()
            && line_memo.with(|l| {
                l.as_ref()
                    .and_then(|line| line.translation.as_deref())
                    .is_some_and(|t| !t.trim().is_empty())
            })
    });

    let show_roma = computed(move || {
        store.show_romaji.get()
            && line_memo.with(|l| {
                l.as_ref()
                    .and_then(|line| line.romaji.as_deref())
                    .is_some_and(|r| !r.trim().is_empty())
            })
    });

    let row_a11y = move || {
        line_memo
            .with(|l| l.as_ref().map(|line| SharedString::from(line.text.clone())))
            .or_else(|| Some(SharedString::from("Lyric line")))
    };

    view().w_full().child(
        button()
            .accessibility_label(row_a11y)
            .on_press(move || {
                if is_synced.get_untracked()
                    && let Some(line) = line_memo.get_untracked()
                {
                    store.seek(line.start_ms.max(0) as u64);
                }
            })
            .w_full()
            .min_h(move || px(theme::detect_device_form().min_touch_target()))
            .px_3()
            .py_2()
            .rounded(px(radius::CARD))
            .bg(colors::TRANSPARENT)
            .transition_all()
            .duration(move || motion::transition_ms(320.0))
            .hover(|s| s.bg(theme::np_surface))
            .active(|s| s.bg(theme::np_surface_hover))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .opacity(move || spec.get().opacity)
            .translate_x(move || px(spec.get().offset_x))
            .scale(move || spec.get().scale)
            .rotate(move || deg(spec.get().tilt_deg))
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .items_stretch()
                    .gap_2p5()
                    .bind_scale(&focus_scale)
                    .style_with(move |s| match spec.get().align {
                        StageAlign::Center => s.justify_center(),
                        StageAlign::Start => s.justify_start(),
                    })
                    // Subtle vertical spotlight bar for active Flow / Steps / Tilt lines
                    .child(when(
                        move || spec.get().show_focus_bar,
                        move || {
                            view()
                                .w(px(3.5))
                                .rounded_full()
                                .bg(move || {
                                    AdaptiveColor::from(
                                        store.current_accent.get().lighten(0.24).saturate(0.22),
                                    )
                                })
                                .flex_shrink_0()
                        },
                    ))
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap_1()
                            .style_with(move |s| match spec.get().align {
                                StageAlign::Center => s.items_center(),
                                StageAlign::Start => s.items_start(),
                            })
                            // Primary lyric line (karaoke word-by-word when active & word-synced)
                            .child(
                                when(show_word_karaoke, move || {
                                    karaoke_word_row(store, line_memo, spec)
                                })
                                .otherwise(move || {
                                    text(move || {
                                        line_memo.with(|l| {
                                            l.as_ref()
                                                .map(|line| SharedString::from(line.text.clone()))
                                                .unwrap_or_default()
                                        })
                                    })
                                    .text_size(move || px(spec.get().font_size))
                                    .text_color(move || {
                                        if is_active.get() {
                                            theme::np_fg()
                                        } else {
                                            theme::np_fg().opacity(0.86)
                                        }
                                    })
                                    .style_with(move |s| {
                                        let sp = spec.get();
                                        let s = s.font_weight(sp.font_weight);
                                        match sp.align {
                                            StageAlign::Center => s.text_center(),
                                            StageAlign::Start => s.text_left(),
                                        }
                                    })
                                }),
                            )
                            // Optional translation line
                            .child(when(show_trans, move || {
                                text(move || {
                                    line_memo.with(|l| {
                                        l.as_ref()
                                            .and_then(|line| line.translation.clone())
                                            .map(SharedString::from)
                                            .unwrap_or_default()
                                    })
                                })
                                .text_size(move || px(spec.get().translation_size))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(theme::np_muted)
                                .style_with(move |s| match spec.get().align {
                                    StageAlign::Center => s.text_center(),
                                    StageAlign::Start => s.text_left(),
                                })
                            }))
                            // Optional romaji pronunciation line
                            .child(when(show_roma, move || {
                                text(move || {
                                    line_memo.with(|l| {
                                        l.as_ref()
                                            .and_then(|line| line.romaji.clone())
                                            .map(SharedString::from)
                                            .unwrap_or_default()
                                    })
                                })
                                .text_size(px(font_size::CAPTION))
                                .font_weight(FontWeight::LIGHT)
                                .text_color(move || theme::np_muted().opacity(0.78))
                                .style_with(move |s| match spec.get().align {
                                    StageAlign::Center => s.text_center(),
                                    StageAlign::Start => s.text_left(),
                                })
                            })),
                    ),
            ),
    )
}

/// Word-by-word karaoke emphasis row for the currently active word-synced line,
/// with optional `stage_sparks` micro-glint burst when enabled.
fn karaoke_word_row(
    store: AppStore,
    line_memo: Memo<Option<ParsedLyricLine>>,
    spec: Memo<stage::StageLineSpec>,
) -> View {
    let words_keyed = computed(move || {
        line_memo.with(|l| {
            l.as_ref()
                .map(|line| {
                    line.words
                        .iter()
                        .enumerate()
                        .map(|(i, w)| (i, w.start, w.text.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });

    view()
        .w_full()
        .flex_row()
        .flex_wrap()
        .items_baseline()
        .gap_y_1()
        .style_with(move |s| match spec.get().align {
            StageAlign::Center => s.justify_center(),
            StageAlign::Start => s.justify_start(),
        })
        .child(each(
            words_keyed,
            |(idx, start, _): &(usize, i64, String)| (*idx, *start),
            move |word_sig, _| {
                let word_idx = word_sig.with_untracked(|(i, _, _)| *i);

                let word_state = computed(move || {
                    let active = store.active_lyric.get();
                    match active.and_then(|a| a.word_index) {
                        Some(cur) if word_idx < cur => (2_u8, 1.0_f32), // Sung
                        Some(cur) if word_idx == cur => {
                            let prog = active.map(|a| a.word_progress).unwrap_or(0.5);
                            (1_u8, prog) // Singing now
                        }
                        _ => (0_u8, 0.0_f32), // Upcoming
                    }
                });

                let show_sparks = computed(move || {
                    word_state.get().0 == 1
                        && store.settings.with(|s| s.stage_sparks)
                        && !reduce_motion()
                });

                view()
                    .relative()
                    .pb_0p5()
                    .transition_transform()
                    .duration(move || motion::transition_ms(140.0))
                    .translate_y(move || {
                        let (st, _) = word_state.get();
                        if st == 1 && !reduce_motion() {
                            px(-2.0)
                        } else {
                            px(0.0)
                        }
                    })
                    // Subtle stage_sparks rising micro-glints off the active sung word
                    .child(when(show_sparks, move || {
                        canvas(move || {
                            let (_, prog) = word_state.get();
                            let accent = store.current_accent.get().lighten(0.36);
                            let fg = color(theme::np_fg());
                            let spark_col = accent.mix(fg, 0.55);
                            let mut p = Painter::new();
                            for s_idx in 0..4_usize {
                                let seed = (word_idx * 7 + s_idx * 13) as f32;
                                let local_t = (prog + s_idx as f32 * 0.18).clamp(0.0, 1.0);
                                let alpha = (1.0 - local_t) * 0.85;
                                if alpha <= 0.04 {
                                    continue;
                                }
                                let sx = 6.0 + ((seed * 17.3).sin() * 0.5 + 0.5) * 28.0;
                                let sy = 14.0 - local_t * 13.0;
                                let sr = (1.8 - local_t * 0.9).max(0.7);
                                p = p
                                    .fill_color(spark_col.alpha(alpha))
                                    .begin_path()
                                    .arc(sx, sy, sr, 0.0, TAU)
                                    .fill();
                            }
                            p
                        })
                        .absolute()
                        .top(px(-10.0))
                        .left_0()
                        .w(px(40.0))
                        .h(px(16.0))
                        .pointer_events(PointerEvents::None)
                    }))
                    .child(
                        text(move || {
                            word_sig.with(|(_, _, txt)| SharedString::from(txt.clone()))
                        })
                        .text_size(move || px(spec.get().font_size))
                        .style_with(move |s| s.font_weight(spec.get().font_weight))
                        .text_color(move || {
                            let (st, prog) = word_state.get();
                            match st {
                                2 => theme::np_fg(),
                                1 => {
                                    let accent = store.current_accent.get().lighten(0.32);
                                    let bright = color(theme::np_fg());
                                    AdaptiveColor::from(accent.mix(bright, 0.45 + prog * 0.45))
                                }
                                _ => theme::np_muted(),
                            }
                        }),
                    )
                    // Subtle glowing karaoke underline sweeping across the singing word
                    .child(when(
                        move || word_state.get().0 == 1 && !reduce_motion(),
                        move || {
                            view()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(2.5))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme::np_surface)
                                .child(
                                    view()
                                        .h_full()
                                        .w(move || relative(word_state.get().1.clamp(0.12, 1.0)))
                                        .rounded_full()
                                        .bg(move || {
                                            AdaptiveColor::from(
                                                store.current_accent.get().lighten(0.28),
                                            )
                                        }),
                                )
                        },
                    ))
            },
        ))
}

fn lyrics_empty_state(store: AppStore) -> View {
    view()
        .flex_1()
        .w_full()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .child(
            view()
                .size(px(58.0))
                .rounded_full()
                .bg(theme::np_surface)
                .border_1()
                .border_color(theme::np_border)
                .items_center()
                .justify_center()
                .child(icon(Icon::Lyrics, 24.0, colors::NP_MUTED)),
        )
        .child(
            text(move || store.t("now_playing.lyrics.empty"))
                .text_size(px(font_size::HEADING))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::np_fg),
        )
        .child(
            text(move || {
                store.current_track.with(|t| {
                    t.as_ref()
                        .map(|tr| {
                            let artists = tr.artist_names();
                            if artists.is_empty() {
                                SharedString::from(format!(
                                    "{} · {}",
                                    tr.name,
                                    store.t("track.unknown_artist")
                                ))
                            } else {
                                SharedString::from(format!("{} · {artists}", tr.name))
                            }
                        })
                        .unwrap_or_else(|| store.t("player.idle_title"))
                })
            })
            .text_center()
            .text_size(px(font_size::SUBTITLE))
            .text_color(theme::np_muted),
        )
}
