//! Synced lyrics pane & Quiet-Part Top Comments aligned 1:1 with `egoist/mygo-music`
//! (`internal/app/ui_nowplaying.go:1568-1706` & `internal/app/ui_quiet.go`):
//! - Borderless left-aligned `30pt` ExtraBold lines (`22pt` Phone), `line_height 1.25`,
//!   `-0.4` letter spacing, `17pt` SemiBold translation (`white 0.70`)
//! - Inactive opacity `0.28` (`0.55` + `white 0.06` hover), `0.94 -> 1.0` spring scale
//!   from left origin (`.origin_left()`), honoring `reduce_motion()`
//! - Smooth word-by-word opacity sweep (`0.38 + 0.62 * lit`) with ZERO underline bar
//!   and ZERO Karaoke chip clutter
//! - Quiet-Part Top Comments: borderless editorial quote (`Top Comment`, `22pt` quote,
//!   avatar/author/likes) dimming lyrics smoothly behind it (`0.06`), clicking opens Comments
//! - Click/tap any synced line to `store.seek(...)`

use nsh_base::button;
use nsh_core::{LineHeight, geometry::percentage, prelude::*};
use nsh_music_core::{
    lyrics::{ParsedLyricLine, ParsedLyrics},
    model::Comment,
};

use crate::{
    state::{AppStore, ArtworkLoader, NowPlayingPanel},
    ui::{
        icons::{Icon, icon},
        motion, shimmer_block_on_dark,
        theme::{self, DeviceForm, colors, font_size},
    },
};

/// Minimum quiet passage duration (ms) to surface top listener comments (`ui_quiet.go`).
pub(crate) const QUIET_LEAST_MS: i64 = 8_000;
/// Lead-in buffer (ms) after the previous vocal ends before surfacing a quiet comment.
const QUIET_LEAD_MS: i64 = 1_200;
/// Tail-out buffer (ms) before the next vocal enters so lyrics return cleanly.
const QUIET_TAIL_MS: i64 = 1_000;
/// Approximate duration (ms) each comment shows during a long quiet passage.
const QUIET_SLOT_MS: i64 = 10_000;
/// Maximum character count for a comment shown in the quiet-part quote.
const QUIET_MAX_CHARS: usize = 140;
/// Estimated vocal hold (ms) when line-synced LRC lacks explicit per-line end timestamps.
const FALLBACK_LINE_HOLD_MS: i64 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuietWindow {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// Detects intro, interlude, outro, or instrumental quiet passages `>= min_gap_ms`.
pub(crate) fn detect_quiet_windows(
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

fn select_quiet_comment_across_windows(
    windows: &[QuietWindow],
    comments: &[Comment],
    pos_ms: i64,
) -> Option<Comment> {
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
    eligible.sort_by_key(|a| std::cmp::Reverse(a.likes));
    eligible.truncate(10);

    let mut shown_slots: usize = 0;
    for w in windows {
        let from = w.from_ms + QUIET_LEAD_MS;
        let to = w.to_ms - QUIET_TAIL_MS;
        if to <= from {
            continue;
        }
        if pos_ms < from {
            return None;
        }
        let span = to - from;
        let slots = (((span + QUIET_SLOT_MS / 2) / QUIET_SLOT_MS).max(1)) as usize;
        let each = (span / slots as i64).max(1);
        if pos_ms < to {
            let slot = (((pos_ms - from) / each) as usize).min(slots - 1);
            return Some(eligible[(shown_slots + slot) % eligible.len()].clone());
        }
        shown_slots += slots;
    }
    None
}

/// Shared reactive Quiet-Part Top Comment state consumed by both `lyrics_view` and `stage`.
pub(crate) fn quiet_comment_state(store: AppStore) -> (Memo<Option<Comment>>, Memo<bool>) {
    let quiet_windows = computed(move || {
        let dur = store.duration_ms.get() as i64;
        store
            .lyrics
            .with(|l| detect_quiet_windows(l, dur, QUIET_LEAST_MS))
    });

    // Quantize position to 500ms buckets so quiet-comment checks never churn at 60ms.
    let quiet_tick = computed(move || (store.position_ms.get() / 500) as i64 * 500);

    let quiet_comment: Memo<Option<Comment>> = computed(move || {
        if !store.settings.with(|s| s.hot_comments) || !store.active_can("comments") {
            return None;
        }
        let pos = quiet_tick.get();
        quiet_windows.with(|ws| {
            if ws.is_empty() {
                return None;
            }
            store
                .comments
                .with(|list| select_quiet_comment_across_windows(ws, list, pos))
        })
    });

    let has_quiet = computed(move || quiet_comment.with(Option::is_some));
    (quiet_comment, has_quiet)
}

/// Renders the borderless left-aligned lyrics pane (`ui_nowplaying.go:1568-1706`).
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

    let (quiet_comment, has_quiet_comment) = quiet_comment_state(store);

    // Smooth auto-scroll active lyric line to the upper-third focal sweet spot (`0.30`).
    let prev_scrolled = stored(None::<usize>);
    effect(move || {
        let idx = active_line_idx.get();
        let synced = is_synced.get();
        if synced && idx != prev_scrolled.get() {
            prev_scrolled.replace(idx);
            if let Some(target) = idx {
                list_ref.scroll_to_index(
                    target as u32,
                    percentage(0.30),
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
        // Subtle utility pills ONLY when translation/romaji exist or lyrics are unsynced
        .child(when(
            move || {
                !is_empty.get() && (!is_synced.get() || has_translation.get() || has_romaji.get())
            },
            move || lyrics_subtle_toggles(store, is_synced, has_translation, has_romaji, form),
        ))
        // Main lyrics viewport (smoothly dims to 0.06 when Quiet-Part Top Comment appears)
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
                        .duration(move || motion::transition_ms(600.0))
                        .opacity(move || {
                            if has_quiet_comment.get() {
                                0.06_f32
                            } else {
                                1.0_f32
                            }
                        })
                        .child(
                            when(is_empty, move || {
                                when(
                                    move || store.lyrics_loading.get(),
                                    move || lyrics_skeleton(compact),
                                )
                                .otherwise(move || lyrics_empty_state(store, compact))
                            })
                            .otherwise(move || {
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
                                .estimated_item_size(px(if compact { 56.0 } else { 78.0 }))
                                .content_padding_top(px(if compact { 16.0 } else { 12.0 }))
                                .content_padding_bottom(px(if compact { 88.0 } else { 260.0 }))
                                .content_padding_x(px(if compact { 14.0 } else { 8.0 }))
                                .row_gap(px(if compact { 4.0 } else { 6.0 }))
                                .scrollbar(false)
                                .element_ref(&list_ref)
                                .flex_1()
                                .w_full()
                            }),
                        ),
                )
                // Borderless Quiet-Part Top Comment editorial quote (`ui_quiet.go:3844-3899`)
                .child(when(has_quiet_comment, move || {
                    view()
                        .absolute()
                        .inset_0()
                        .pt(px(if compact { 24.0 } else { 110.0 }))
                        .pl(px(if compact { 20.0 } else { 0.0 }))
                        .pr(px(if compact { 20.0 } else { 24.0 }))
                        .pointer_events(PointerEvents::BoxNone)
                        .child(quiet_comment_card(
                            store,
                            quiet_comment,
                            if compact { 18.0 } else { 22.0 },
                            form,
                        ))
                })),
        )
}

/// Borderless editorial Quiet-Part Top Comment card (`ui_quiet.go:3859-3900`).
pub(crate) fn quiet_comment_card(
    store: AppStore,
    quiet_comment: Memo<Option<Comment>>,
    font_size_pt: f32,
    form: DeviceForm,
) -> View {
    let touch = form.is_touch();
    let fade = animated_value(if reduce_motion() { 1.0 } else { 0.0 });
    let lift = animated_value(if reduce_motion() { 0.0 } else { 16.0 });
    let last_id = stored(String::new());

    effect(move || {
        let cur_id = quiet_comment.with(|c| c.as_ref().map(|x| x.id.clone()).unwrap_or_default());
        if cur_id != last_id.get() && !cur_id.is_empty() {
            last_id.replace(cur_id);
            if reduce_motion() {
                fade.set(1.0);
                lift.set(0.0);
            } else {
                fade.set(0.0);
                lift.set(16.0);
                motion::animate_timing(&fade, 1.0, motion::timing_base());
                motion::animate_spring(&lift, 0.0, motion::spring_gentle());
            }
        }
    });

    let avatar_uri = move || {
        quiet_comment.with(|c| {
            c.as_ref()
                .map(|cmt| SharedString::from(ArtworkLoader::resolve_template(&cmt.avatar_url, 64)))
                .unwrap_or_default()
        })
    };

    view()
        .w_full()
        .bind_opacity(&fade)
        .bind_translate_y(&lift)
        .child(
            button()
                .test_id("quiet-comment-card")
                .accessibility_label(move || Some(store.t("now_playing.view_comments")))
                .on_press(move || store.set_now_playing_panel(NowPlayingPanel::Comments))
                .w_full()
                .min_h(px(if touch { 64.0 } else { 48.0 }))
                .px_3()
                .py_4()
                .gap_3p5()
                .items_start()
                .rounded(px(14.0))
                .bg(colors::TRANSPARENT)
                .transition_colors()
                .duration(move || motion::transition_ms(160.0))
                .hover(|s| s.bg(move || theme::np_fg().opacity(0.06)))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                // Eyebrow: Comment icon + "Top Comment"
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(icon(Icon::Comment, 14.0, colors::NP_MUTED).opacity(0.75))
                        .child(
                            text(move || store.t("now_playing.top_comment"))
                                .text_size(px(12.5))
                                .font_weight(FontWeight::BOLD)
                                .letter_spacing(px(0.6))
                                .text_color(move || theme::np_fg().opacity(0.50)),
                        ),
                )
                // Editorial quote body
                .child(
                    text(move || {
                        quiet_comment.with(|c| {
                            c.as_ref()
                                .map(|cmt| SharedString::from(cmt.text.trim().to_owned()))
                                .unwrap_or_default()
                        })
                    })
                    .w_full()
                    .text_left()
                    .number_of_lines(6)
                    .text_size(px(font_size_pt))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(LineHeight::Multiple(1.5))
                    .text_color(move || theme::np_fg().opacity(0.92)),
                )
                // Footer: 22pt avatar + author + likes count
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            view()
                                .size(px(22.0))
                                .flex_shrink_0()
                                .rounded_full()
                                .overflow_hidden()
                                .bg(move || theme::np_fg().opacity(0.10))
                                .items_center()
                                .justify_center()
                                .child(
                                    text(move || {
                                        quiet_comment.with(|c| {
                                            c.as_ref()
                                                .and_then(|cmt| cmt.author.chars().next())
                                                .map(|ch| {
                                                    SharedString::from(
                                                        ch.to_uppercase().to_string(),
                                                    )
                                                })
                                                .unwrap_or_else(|| SharedString::from("?"))
                                        })
                                    })
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(move || theme::np_fg().opacity(0.75)),
                                )
                                .child(when(
                                    move || !avatar_uri().is_empty(),
                                    move || {
                                        image(avatar_uri)
                                            .object_fit(ResizeMode::Cover)
                                            .absolute()
                                            .inset_0()
                                            .size_full()
                                    },
                                )),
                        )
                        .child(
                            text(move || {
                                quiet_comment.with(|c| {
                                    c.as_ref()
                                        .map(|cmt| SharedString::from(cmt.author.clone()))
                                        .unwrap_or_default()
                                })
                            })
                            .flex_shrink(1.0)
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(13.0))
                            .text_color(move || theme::np_fg().opacity(0.60)),
                        )
                        .child(when(
                            move || quiet_comment.with(|c| c.as_ref().is_some_and(|x| x.likes > 0)),
                            move || {
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .ml_1p5()
                                    .child(
                                        icon(Icon::ThumbUpFilled, 13.0, colors::NP_FG)
                                            .opacity(0.45),
                                    )
                                    .child(
                                        text(move || {
                                            quiet_comment.with(|c| {
                                                c.as_ref()
                                                    .map(|x| {
                                                        SharedString::from(x.likes.to_string())
                                                    })
                                                    .unwrap_or_default()
                                            })
                                        })
                                        .tabular_nums()
                                        .text_size(px(12.0))
                                        .text_color(move || theme::np_fg().opacity(0.45)),
                                    )
                            },
                        )),
                ),
        )
}

fn lyrics_subtle_toggles(
    store: AppStore,
    is_synced: Memo<bool>,
    has_translation: Memo<bool>,
    has_romaji: Memo<bool>,
    form: DeviceForm,
) -> View {
    let touch = form.is_touch();
    let is_phone = form.is_phone();

    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .px(px(if is_phone { 20.0 } else { 8.0 }))
        .child(when(
            move || !is_synced.get(),
            move || {
                text(move || store.t("now_playing.lyrics.unsynced"))
                    .text_size(px(font_size::MICRO))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || theme::np_fg().opacity(0.45))
            },
        ))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .child(when(has_translation, move || {
                    minimal_toggle_pill(
                        move || store.t("now_playing.lyrics.translation"),
                        move || store.show_translation.get(),
                        touch,
                        move || store.toggle_translation(),
                    )
                }))
                .child(when(has_romaji, move || {
                    minimal_toggle_pill(
                        move || store.t("now_playing.lyrics.romaji"),
                        move || store.show_romaji.get(),
                        touch,
                        move || store.toggle_romaji(),
                    )
                })),
        )
}

fn minimal_toggle_pill(
    label: impl IntoReactive<SharedString>,
    active: impl IntoReactive<bool>,
    touch: bool,
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
        .min_h(px(if touch { 44.0 } else { 26.0 }))
        .min_w(px(if touch { 44.0 } else { 0.0 }))
        .px(px(if touch { 2.0 } else { 0.0 }))
        .items_center()
        .justify_center()
        .bg(colors::TRANSPARENT)
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .h(px(if touch { 28.0 } else { 26.0 }))
                .px_3()
                .rounded_full()
                .items_center()
                .justify_center()
                .bg(move || {
                    if a_bg.get() {
                        theme::np_fg().opacity(0.16)
                    } else if touch {
                        theme::np_fg().opacity(0.06)
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .border_1()
                .border_color(move || {
                    if a_border.get() {
                        theme::np_fg().opacity(0.14)
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .transition_colors()
                .duration(move || motion::transition_ms(140.0))
                .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
                .child(
                    text(move || label_rx.get())
                        .number_of_lines(1)
                        .text_size(px(font_size::MICRO))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || {
                            if a_fg.get() {
                                theme::np_fg()
                            } else {
                                theme::np_fg().opacity(0.56)
                            }
                        }),
                ),
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
    let hovered = signal(false);

    let inactive_scale = if compact { 0.98_f32 } else { 0.94_f32 };
    // Spring scale from left origin (`0.94 -> 1.0` Desktop, subtle `0.98 -> 1.0` Phone where size already shifts 17->22pt)
    let initial_scale = if untracked(|| is_active.get() || !is_synced.get()) || reduce_motion() {
        1.0_f32
    } else {
        inactive_scale
    };
    let row_scale = animated_value(initial_scale);
    let prev_cell = stored((untracked(|| at.index()), untracked(|| is_active.get())));

    effect(move || {
        let idx = at.index();
        let active = is_active.get();
        let synced = is_synced.get();
        let (last_idx, last_active) = prev_cell.replace((idx, active));
        let target = if !synced || active || reduce_motion() {
            1.0
        } else {
            inactive_scale
        };
        if idx != last_idx || reduce_motion() {
            row_scale.set(target);
        } else if active != last_active {
            motion::animate_spring(&row_scale, target, motion::spring_gentle());
        }
    });

    let show_word_karaoke = computed(move || {
        is_active.get() && line_memo.with(|l| l.as_ref().is_some_and(|line| !line.words.is_empty()))
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

    let active_line_size = if compact { 22.0 } else { 30.0 };
    let trans_size = if compact { 14.0 } else { 17.0 };

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
            .on_mouse_enter(move |_: &MouseEvent| hovered.set(true))
            .on_mouse_leave(move |_: &MouseEvent| hovered.set(false))
            .w_full()
            .min_h(move || px(theme::detect_device_form().min_touch_target()))
            .px(px(if compact { 6.0 } else { 12.0 }))
            .py(px(if compact { 8.0 } else { 10.0 }))
            .items_start()
            .rounded(px(12.0))
            .bg(move || {
                if hovered.get() {
                    theme::np_fg().opacity(0.06)
                } else {
                    colors::TRANSPARENT
                }
            })
            .origin_left()
            .bind_scale(&row_scale)
            .transition_colors()
            .duration(move || motion::transition_ms(160.0))
            .active(|s| s.bg(move || theme::np_fg().opacity(0.09)))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(
                view()
                    .w_full()
                    .items_start()
                    .gap_1p5()
                    .transition_all()
                    .duration(move || motion::transition_ms(380.0))
                    .opacity(move || {
                        if !is_synced.get() {
                            0.90_f32
                        } else if is_active.get() {
                            1.0_f32
                        } else if hovered.get() {
                            if compact { 0.62_f32 } else { 0.55_f32 }
                        } else if compact {
                            0.45_f32
                        } else {
                            0.28_f32
                        }
                    })
                    // Primary lyric line: Phone active `22pt` bold vs inactive `17pt` 45% opacity;
                    // Desktop `30pt` ExtraBold with `0.28` inactive opacity.
                    .child(
                        when(show_word_karaoke, move || {
                            karaoke_word_row(store, line_memo, active_line_size)
                        })
                        .otherwise(move || {
                            text(move || {
                                line_memo.with(|l| {
                                    l.as_ref()
                                        .map(|line| SharedString::from(line.text.clone()))
                                        .unwrap_or_default()
                                })
                            })
                            .w_full()
                            .text_left()
                            .text_size(move || {
                                if !compact {
                                    px(30.0)
                                } else if !is_synced.get() || is_active.get() {
                                    px(22.0)
                                } else {
                                    px(17.0)
                                }
                            })
                            .font_weight(if compact {
                                FontWeight::BOLD
                            } else {
                                FontWeight::EXTRA_BOLD
                            })
                            .line_height(LineHeight::Multiple(if compact { 1.3 } else { 1.25 }))
                            .letter_spacing(px(if compact { -0.25 } else { -0.4 }))
                            .text_color(theme::np_fg)
                        }),
                    )
                    // Optional translation (`14pt` Phone / `17pt` Desktop SemiBold, `white 0.72`)
                    .child(when(show_trans, move || {
                        text(move || {
                            line_memo.with(|l| {
                                l.as_ref()
                                    .and_then(|line| line.translation.clone())
                                    .map(SharedString::from)
                                    .unwrap_or_default()
                            })
                        })
                        .w_full()
                        .text_left()
                        .text_size(px(trans_size))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || theme::np_fg().opacity(0.72))
                    }))
                    // Optional romaji line
                    .child(when(show_roma, move || {
                        text(move || {
                            line_memo.with(|l| {
                                l.as_ref()
                                    .and_then(|line| line.romaji.clone())
                                    .map(SharedString::from)
                                    .unwrap_or_default()
                            })
                        })
                        .w_full()
                        .text_left()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || theme::np_fg().opacity(0.52))
                    })),
            ),
    )
}

/// Clean word-by-word opacity sweep (`0.38 -> 1.0`, matching `ui_nowplaying.go:1659-1666`),
/// with zero underline bar and zero chip clutter.
fn karaoke_word_row(
    store: AppStore,
    line_memo: Memo<Option<ParsedLyricLine>>,
    line_size: f32,
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
        .justify_start()
        .child(each(
            words_keyed,
            |(idx, start, _): &(usize, i64, String)| (*idx, *start),
            move |word_sig, _| {
                let word_idx = word_sig.with_untracked(|(i, _, _)| *i);

                let word_alpha = computed(move || {
                    let active = store.active_lyric.get();
                    let lit = match active.and_then(|a| a.word_index) {
                        Some(cur) if word_idx < cur => 1.0_f32,
                        Some(cur) if word_idx == cur => active
                            .map(|a| a.word_progress.clamp(0.0, 1.0))
                            .unwrap_or(1.0),
                        _ => 0.0_f32,
                    };
                    0.38 + 0.62 * lit
                });

                text(move || word_sig.with(|(_, _, txt)| SharedString::from(txt.clone())))
                    .text_size(px(line_size))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .line_height(LineHeight::Multiple(1.25))
                    .letter_spacing(px(-0.4))
                    .text_color(move || theme::np_fg().opacity(word_alpha.get()))
            },
        ))
}

fn lyrics_skeleton(compact: bool) -> View {
    const WIDTHS: &[f32] = &[0.66, 0.48, 0.74, 0.56, 0.80, 0.44, 0.64];
    let bone_h = if compact { 20.0 } else { 30.0 };

    view()
        .flex_1()
        .w_full()
        .pt(px(if compact { 16.0 } else { 12.0 }))
        .pl(px(if compact { 20.0 } else { 0.0 }))
        .pr(px(if compact { 20.0 } else { 8.0 }))
        .gap_1p5()
        .children(WIDTHS.iter().copied().map(move |frac| {
            view()
                .w_full()
                .px(px(if compact { 0.0 } else { 12.0 }))
                .py_2()
                .child(shimmer_block_on_dark(8.0).w(relative(frac)).h(px(bone_h)))
        }))
}

fn lyrics_empty_state(store: AppStore, compact: bool) -> View {
    view()
        .flex_1()
        .w_full()
        .items_center()
        .justify_center()
        .px(px(if compact { 20.0 } else { 24.0 }))
        .child(
            text(move || store.t("now_playing.lyrics.enjoy"))
                .text_center()
                .text_size(px(if compact { 17.0 } else { 24.0 }))
                .font_weight(FontWeight::BOLD)
                .text_color(move || theme::np_fg().opacity(0.45)),
        )
}
