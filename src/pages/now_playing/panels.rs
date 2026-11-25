//! Switchable Now Playing panels (`NowPlayingPanel::{Lyrics, Queue, Comments, Podcast}`):
//! - `Lyrics`: Delegates to [`super::lyrics_view::lyrics_view`]
//! - `Queue`: Interactive play queue with click-to-play, remove, clear, and shuffle/repeat badges
//! - `Comments`: Capability-gated (`store.active_can("comments")`) listener comments with likes & composer
//! - `Podcast`: Episode-gated show notes, metadata, parsed/synthesized seekable chapters, and show links

use chrono::{DateTime, Local};
use nsh_base::{button, input};
use nsh_core::prelude::*;
use nsh_music_core::model::{Comment, RepeatMode, Track};

use super::lyrics_view;
use crate::{
    state::{AppStore, NowPlayingPanel},
    ui::{
        icons::{Icon, icon, icon_reactive},
        motion,
        theme::{self, DeviceForm, colors, font_size, radius},
        track_row::{format_duration_ms, mini_equalizer},
    },
};

/// Resolves the effective visible panel, falling back cleanly to `Lyrics` when
/// `Comments` is unsupported by the active source or `Podcast` is selected on a non-episode track.
pub fn effective_panel(store: AppStore) -> NowPlayingPanel {
    let panel = store.now_playing_panel.get();
    let allow_comments = store.active_can("comments");
    let allow_podcast = store
        .current_track
        .with(|t| t.as_ref().is_some_and(Track::is_episode));
    match panel {
        NowPlayingPanel::Comments if !allow_comments => NowPlayingPanel::Lyrics,
        NowPlayingPanel::Podcast if !allow_podcast => NowPlayingPanel::Lyrics,
        other => other,
    }
}

/// Tab bar switching between `Lyrics`, `Queue`, and capability/episode-gated `Comments` & `Podcast`.
pub fn panel_tabs(store: AppStore, form: DeviceForm) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .rounded_full()
        .bg(theme::np_surface)
        .border_1()
        .border_color(theme::np_border)
        .child(panel_tab_button(
            store,
            NowPlayingPanel::Lyrics,
            Icon::Lyrics,
            "now_playing.panel.lyrics",
            form,
        ))
        .child(panel_tab_button(
            store,
            NowPlayingPanel::Queue,
            Icon::Queue,
            "now_playing.panel.queue",
            form,
        ))
        .child(when(
            move || store.active_can("comments"),
            move || {
                panel_tab_button(
                    store,
                    NowPlayingPanel::Comments,
                    Icon::Comment,
                    "now_playing.panel.comments",
                    form,
                )
            },
        ))
        .child(when(
            move || {
                store
                    .current_track
                    .with(|t| t.as_ref().is_some_and(Track::is_episode))
            },
            move || {
                panel_tab_button(
                    store,
                    NowPlayingPanel::Podcast,
                    Icon::Podcast,
                    "now_playing.panel.podcast",
                    form,
                )
            },
        ))
}

fn panel_tab_button(
    store: AppStore,
    panel: NowPlayingPanel,
    glyph: Icon,
    i18n_key: &'static str,
    form: DeviceForm,
) -> impl IntoElement {
    let active = move || effective_panel(store) == panel;
    let min_h = move || {
        if form.is_touch() {
            px(44.0)
        } else {
            px(30.0)
        }
    };

    button()
        .selected(active)
        .accessibility_label(move || Some(store.t(i18n_key)))
        .on_press(move || store.set_now_playing_panel(panel))
        .min_h(min_h)
        .px(if form.is_phone() { px(10.0) } else { px(12.0) })
        .flex_row()
        .items_center()
        .gap_1p5()
        .rounded_full()
        .bg(move || {
            if active() {
                theme::np_surface_hover()
            } else {
                colors::TRANSPARENT
            }
        })
        .border_1()
        .border_color(move || {
            if active() {
                theme::np_border()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_all()
        .duration(move || motion::transition_ms(160.0))
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, 14.0, move || {
            if active() {
                theme::np_fg()
            } else {
                theme::np_muted()
            }
        }))
        .child(
            text(move || store.t(i18n_key))
                .number_of_lines(1)
                .text_size(px(font_size::CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                .text_color(move || {
                    if active() {
                        theme::np_fg()
                    } else {
                        theme::np_muted()
                    }
                }),
        )
}

/// Renders the currently selected [`NowPlayingPanel`] body (auto-falling back to `Lyrics`
/// whenever a capability-gated or episode-only tab becomes unavailable).
pub fn active_panel_content(store: AppStore, form: DeviceForm) -> View {
    let active = computed(move || effective_panel(store));

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .child(dynamic(move || {
            let panel = active.get();
            untracked(|| match panel {
                NowPlayingPanel::Lyrics => {
                    lyrics_view::lyrics_view(store, form).key("np-panel-lyrics")
                }
                NowPlayingPanel::Queue => queue_panel(store, form).key("np-panel-queue"),
                NowPlayingPanel::Comments => comments_panel(store, form).key("np-panel-comments"),
                NowPlayingPanel::Podcast => podcast_panel(store, form).key("np-panel-podcast"),
            })
        }))
}

// -----------------------------------------------------------------------------
// Queue Panel
// -----------------------------------------------------------------------------

pub fn queue_panel(store: AppStore, form: DeviceForm) -> View {
    let is_empty = computed(move || store.queue.with(Vec::is_empty));
    let touch = form.is_touch();

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .gap_3()
        // Queue header: count + shuffle/repeat indicators + Clear action
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            text(move || store.t("player.queue"))
                                .text_size(px(font_size::HEADING))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::np_fg),
                        )
                        .child(
                            view()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .bg(theme::np_surface)
                                .child(
                                    text(move || store.queue.with(|q| q.len().to_string()))
                                        .tabular_nums()
                                        .text_size(px(font_size::MICRO))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::np_muted),
                                ),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        // Shuffle indicator & toggle
                        .child(stage_icon_chip(
                            Icon::Shuffle,
                            move || store.shuffle.get(),
                            move || {
                                if store.shuffle.get() {
                                    store.t("player.shuffle_on")
                                } else {
                                    store.t("player.shuffle_off")
                                }
                            },
                            touch,
                            move || store.toggle_shuffle(),
                        ))
                        // Repeat indicator & cycle
                        .child(stage_icon_chip(
                            move || match store.repeat.get() {
                                RepeatMode::One => Icon::RepeatOne,
                                RepeatMode::Off | RepeatMode::All => Icon::Repeat,
                            },
                            move || store.repeat.get() != RepeatMode::Off,
                            move || match store.repeat.get() {
                                RepeatMode::Off => store.t("player.repeat_off"),
                                RepeatMode::All => store.t("player.repeat_all"),
                                RepeatMode::One => store.t("player.repeat_one"),
                            },
                            touch,
                            move || store.cycle_repeat(),
                        ))
                        // Clear Queue button
                        .child(
                            button()
                                .disabled(is_empty)
                                .accessibility_label(move || Some(store.t("player.queue_clear")))
                                .on_press(move || store.clear_queue())
                                .min_h(px(if touch { 44.0 } else { 30.0 }))
                                .px_3()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(theme::np_surface)
                                .border_1()
                                .border_color(theme::np_border)
                                .hover(|s| s.bg(theme::np_surface_hover))
                                .active(|s| s.scale(motion::press_scale))
                                .disabled_style(|s| s.opacity(0.38))
                                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                                .child(
                                    text(move || store.t("player.queue_clear"))
                                        .number_of_lines(1)
                                        .text_size(px(font_size::CAPTION))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::np_fg),
                                ),
                        ),
                ),
        )
        // Queue rows list
        .child(
            when(is_empty, move || {
                panel_empty_state(
                    Icon::Queue,
                    move || store.t("player.queue_empty"),
                    move || store.t("player.up_next"),
                )
            })
            .otherwise(move || {
                virtual_list(
                    move || store.queue.with(Vec::len),
                    move |at| queue_row_cell(store, at, touch),
                )
                .keys(move || {
                    store.queue.with(|q| {
                        q.iter()
                            .enumerate()
                            .map(|(i, t)| format!("{i}:{}", t.id))
                            .collect::<Vec<_>>()
                    })
                })
                .estimated_item_size(px(if touch { 62.0 } else { 56.0 }))
                .row_gap(px(4.0))
                .content_padding_bottom(px(24.0))
                .flex_1()
                .w_full()
            }),
        )
}

fn queue_row_cell(store: AppStore, at: Position, touch: bool) -> View {
    let track_memo = computed(move || store.queue.with(|q| q.get(at.index()).cloned()));
    let is_current = computed(move || store.current_index.get() == Some(at.index()));

    let row_min_h = if touch { 58.0 } else { 52.0 };
    let cover_thumb = queue_cover_thumb(track_memo);

    view()
        .w_full()
        .min_h(px(row_min_h))
        .px_2p5()
        .py_1p5()
        .flex_row()
        .items_center()
        .gap_2p5()
        .rounded(px(radius::CONTROL))
        .bg(move || {
            if is_current.get() {
                theme::np_surface_hover()
            } else {
                colors::TRANSPARENT
            }
        })
        .border_1()
        .border_color(move || {
            if is_current.get() {
                theme::np_border()
            } else {
                colors::TRANSPARENT
            }
        })
        .hover(|s| s.bg(theme::np_surface))
        .child(
            button()
                .accessibility_label(move || {
                    track_memo.with(|t| {
                        t.as_ref()
                            .map(|tr| SharedString::from(format!("Play {}", tr.name)))
                    })
                })
                .on_press(move || store.play_from_queue(at.index()))
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(if touch { 44.0 } else { 36.0 }))
                .flex_row()
                .items_center()
                .gap_2p5()
                .active(|s| s.opacity(0.75))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                // Index or active equalizer
                .child(
                    view()
                        .w(px(24.0))
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .child(
                            when(
                                move || is_current.get() && store.is_playing.get(),
                                move || mini_equalizer(store.spectrum, store.is_playing),
                            )
                            .otherwise(move || {
                                when(is_current, || icon(Icon::Play, 13.0, colors::PRIMARY))
                                    .otherwise(move || {
                                        text(move || (at.index() + 1).to_string())
                                            .tabular_nums()
                                            .text_size(px(font_size::CAPTION))
                                            .text_color(theme::np_muted)
                                    })
                            }),
                        ),
                )
                // Cover thumbnail (reactive for virtual_list cell recycling)
                .child(cover_thumb)
                // Title & localized fallback artist
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_0p5()
                        .child(
                            text(move || {
                                track_memo.with(|t| {
                                    t.as_ref()
                                        .map(|tr| SharedString::from(tr.name.clone()))
                                        .unwrap_or_default()
                                })
                            })
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || {
                                if is_current.get() {
                                    theme::primary()
                                } else {
                                    theme::np_fg()
                                }
                            }),
                        )
                        .child(
                            text(move || {
                                let fallback_artist = store.t("track.unknown_artist");
                                track_memo.with(|t| {
                                    t.as_ref()
                                        .map(|tr| {
                                            let names = tr.artist_names();
                                            if names.is_empty() {
                                                fallback_artist.clone()
                                            } else {
                                                SharedString::from(names)
                                            }
                                        })
                                        .unwrap_or(fallback_artist)
                                })
                            })
                            .number_of_lines(1)
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::np_muted),
                        ),
                )
                // Duration
                .child(
                    text(move || {
                        track_memo.with(|t| {
                            t.as_ref()
                                .map(|tr| SharedString::from(format_duration_ms(tr.duration)))
                                .unwrap_or_default()
                        })
                    })
                    .tabular_nums()
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::np_muted),
                ),
        )
        // Remove item button (>= 44pt on Pad/Phone)
        .child(
            button()
                .accessibility_label(move || Some(store.t("common.remove")))
                .on_press(move || store.remove_from_queue(at.index()))
                .size(px(if touch { 44.0 } else { 28.0 }))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(colors::TRANSPARENT)
                .hover(|s| s.bg(theme::np_surface_hover))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(icon(Icon::Close, 14.0, colors::NP_MUTED)),
        )
}

// -----------------------------------------------------------------------------
// Comments Panel
// -----------------------------------------------------------------------------

pub fn comments_panel(store: AppStore, form: DeviceForm) -> View {
    let draft = signal(String::new());
    let touch = form.is_touch();
    let is_empty = computed(move || store.comments.with(Vec::is_empty));
    let can_post = computed(move || !draft.with(|d| d.trim().is_empty()));

    let submit_comment = move || {
        let body = draft.get_untracked();
        if !body.trim().is_empty() {
            store.post_comment(&body);
            draft.set(String::new());
            dismiss_keyboard();
        }
    };

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .gap_3()
        // Header with localized comments count
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .child(
                    text(move || {
                        let count = store
                            .comments_total
                            .get()
                            .max(store.comments.with(Vec::len));
                        let count_str = count.to_string();
                        store.t_fmt("now_playing.comments.count", &[("count", &count_str)])
                    })
                    .text_size(px(font_size::HEADING))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::np_fg),
                ),
        )
        // Comments feed
        .child(
            when(is_empty, move || {
                panel_empty_state(
                    Icon::Comment,
                    move || store.t("now_playing.comments.empty"),
                    move || store.t("now_playing.comments.placeholder"),
                )
            })
            .otherwise(move || {
                virtual_list(
                    move || store.comments.with(Vec::len),
                    move |at| comment_row_cell(store, at, touch),
                )
                .keys(move || {
                    store.comments.with(|list| {
                        list.iter()
                            .map(|c| c.id.clone())
                            .collect::<Vec<_>>()
                    })
                })
                .estimated_item_size(px(92.0))
                .row_gap(px(10.0))
                .content_padding_bottom(px(12.0))
                .flex_1()
                .w_full()
            }),
        )
        // Post comment input bar
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_2()
                .pt_2()
                .border_t_1()
                .border_color(theme::np_border)
                .child(
                    input()
                        .value(move || draft.get())
                        .on_change(move |text: &str| draft.set(text.to_owned()))
                        .on_submit(move |_: &str| submit_comment())
                        .placeholder(move || store.t("now_playing.comments.placeholder").to_string())
                        .placeholder_color(|| color(theme::np_muted()))
                        .caret_color(|| color(theme::primary()))
                        .selection_color(|| color(theme::primary_soft()))
                        .return_key_type(ReturnKeyType::Send)
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(if touch { 44.0 } else { 38.0 }))
                        .px_3()
                        .rounded(px(radius::CONTROL))
                        .bg(theme::np_surface)
                        .border_1()
                        .border_color(theme::np_border)
                        .text_size(px(font_size::BODY))
                        .text_color(theme::np_fg)
                        .focus(|s| s.border_color(theme::ring)),
                )
                .child(
                    button()
                        .disabled(move || !can_post.get())
                        .accessibility_label(move || Some(store.t("now_playing.comments.post")))
                        .on_press(submit_comment)
                        .min_h(px(if touch { 44.0 } else { 38.0 }))
                        .px_4()
                        .items_center()
                        .justify_center()
                        .rounded(px(radius::CONTROL))
                        .bg(theme::primary)
                        .hover(|s| s.bg(theme::primary_hover))
                        .active(|s| s.scale(motion::press_scale))
                        .disabled_style(|s| s.opacity(0.42))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(
                            text(move || store.t("now_playing.comments.post"))
                                .number_of_lines(1)
                                .text_size(px(font_size::BODY))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary_fg),
                        ),
                ),
        )
}

fn comment_row_cell(store: AppStore, at: Position, touch: bool) -> View {
    let cmt_memo: Memo<Option<Comment>> =
        computed(move || store.comments.with(|list| list.get(at.index()).cloned()));

    let author_initial = move || {
        cmt_memo.with(|c| {
            c.as_ref()
                .and_then(|cmt| cmt.author.chars().next())
                .map(|ch| SharedString::from(ch.to_uppercase().to_string()))
                .unwrap_or_else(|| SharedString::from("?"))
        })
    };

    view()
        .w_full()
        .p_3()
        .rounded(px(radius::CARD))
        .bg(theme::np_surface)
        .border_1()
        .border_color(theme::np_border)
        .gap_2()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                // Avatar + Author + Time
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .gap_2p5()
                        .child(
                            view()
                                .size(px(32.0))
                                .flex_shrink_0()
                                .rounded_full()
                                .overflow_hidden()
                                .bg_linear_to_br()
                                .gradient_from(move || {
                                    let (a, _) = cmt_memo.with(|c| {
                                        let seed = c
                                            .as_ref()
                                            .map(|cmt| cmt.author.as_str())
                                            .unwrap_or("anon");
                                        theme::fallback_cover_gradient(seed)
                                    });
                                    AdaptiveColor::from(a)
                                })
                                .gradient_to(move || {
                                    let (_, b) = cmt_memo.with(|c| {
                                        let seed = c
                                            .as_ref()
                                            .map(|cmt| cmt.author.as_str())
                                            .unwrap_or("anon");
                                        theme::fallback_cover_gradient(seed)
                                    });
                                    AdaptiveColor::from(b)
                                })
                                .items_center()
                                .justify_center()
                                .child(
                                    text(author_initial)
                                        .text_size(px(font_size::CAPTION))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme::np_fg),
                                )
                                .child(when(
                                    move || {
                                        cmt_memo.with(|c| {
                                            c.as_ref()
                                                .is_some_and(|cmt| !cmt.avatar_url.is_empty())
                                        })
                                    },
                                    move || {
                                        image(move || {
                                            cmt_memo.with(|c| {
                                                c.as_ref()
                                                    .map(|cmt| {
                                                        SharedString::from(cmt.avatar_url.clone())
                                                    })
                                                    .unwrap_or_default()
                                            })
                                        })
                                        .object_fit(ResizeMode::Cover)
                                        .absolute()
                                        .inset_0()
                                        .size_full()
                                    },
                                )),
                        )
                        .child(
                            view()
                                .flex_1()
                                .min_w(px(0.0))
                                .child(
                                    text(move || {
                                        cmt_memo.with(|c| {
                                            c.as_ref()
                                                .map(|cmt| SharedString::from(cmt.author.clone()))
                                                .unwrap_or_default()
                                        })
                                    })
                                    .number_of_lines(1)
                                    .text_size(px(font_size::SUBTITLE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::np_fg),
                                )
                                .child(
                                    text(move || {
                                        cmt_memo.with(|c| {
                                            c.as_ref()
                                                .map(|cmt| {
                                                    SharedString::from(format_comment_time(cmt.time))
                                                })
                                                .unwrap_or_default()
                                        })
                                    })
                                    .number_of_lines(1)
                                    .text_size(px(font_size::MICRO))
                                    .text_color(theme::np_muted),
                                ),
                        ),
                )
                // Like comment button with live count (>= 44pt on Pad/Phone)
                .child(
                    button()
                        .selected(move || cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.liked)))
                        .accessibility_label(move || Some(store.t("player.like")))
                        .on_press(move || {
                            if let Some(cmt) = cmt_memo.get_untracked() {
                                store.toggle_like_comment(&cmt.id);
                            }
                        })
                        .min_h(px(if touch { 44.0 } else { 30.0 }))
                        .min_w(px(if touch { 44.0 } else { 32.0 }))
                        .px_2p5()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .gap_1p5()
                        .rounded_full()
                        .bg(colors::TRANSPARENT)
                        .hover(|s| s.bg(theme::np_surface_hover))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(icon_reactive(
                            move || {
                                if cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.liked)) {
                                    Icon::HeartFilled
                                } else {
                                    Icon::Heart
                                }
                            },
                            14.0,
                            move || {
                                if cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.liked)) {
                                    theme::primary()
                                } else {
                                    theme::np_muted()
                                }
                            },
                        ))
                        .child(
                            text(move || {
                                cmt_memo.with(|c| {
                                    c.as_ref()
                                        .map(|x| SharedString::from(x.likes.to_string()))
                                        .unwrap_or_default()
                                })
                            })
                            .tabular_nums()
                            .text_size(px(font_size::CAPTION))
                            .text_color(move || {
                                if cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.liked)) {
                                    theme::primary()
                                } else {
                                    theme::np_muted()
                                }
                            }),
                        ),
                ),
        )
        // Comment text body
        .child(
            text(move || {
                cmt_memo.with(|c| {
                    c.as_ref()
                        .map(|cmt| SharedString::from(cmt.text.clone()))
                        .unwrap_or_default()
                })
            })
            .text_size(px(font_size::BODY))
            .text_color(move || theme::np_fg().opacity(0.92)),
        )
}

fn format_comment_time(timestamp_ms: i64) -> String {
    if timestamp_ms <= 0 {
        return String::new();
    }
    let normalized_ms = if timestamp_ms < 10_000_000_000 {
        timestamp_ms.saturating_mul(1000)
    } else {
        timestamp_ms
    };
    if let Some(dt) = DateTime::from_timestamp_millis(normalized_ms) {
        let local = dt.with_timezone(&Local);
        local.format("%Y-%m-%d %H:%M").to_string()
    } else {
        String::new()
    }
}

// -----------------------------------------------------------------------------
// Podcast Show Notes & Chapters Panel
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct PodcastChapter {
    title: String,
    start_ms: u64,
}

pub fn podcast_panel(store: AppStore, form: DeviceForm) -> View {
    let touch = form.is_touch();

    let chapters_memo = computed(move || {
        let track = store.current_track.get();
        let dur_ms = store.duration_ms.get();
        extract_or_build_chapters(track.as_ref(), dur_ms)
    });

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .child(
            scroll()
                .flex_1()
                .w_full()
                .scrollbar(false)
                .child(
                    view()
                        .w_full()
                        .gap_4()
                        .px_2()
                        .pb_6()
                        // Episode / Show Notes Card
                        .child(
                            view()
                                .w_full()
                                .p_4()
                                .rounded(px(radius::LG))
                                .bg(theme::np_surface)
                                .border_1()
                                .border_color(theme::np_border)
                                .gap_2p5()
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            view()
                                                .flex_row()
                                                .items_center()
                                                .gap_2()
                                                .child(icon(Icon::Podcast, 16.0, colors::AMBER))
                                                .child(
                                                    text(move || store.t("podcast.description"))
                                                        .text_size(px(font_size::HEADING))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(theme::np_fg),
                                                ),
                                        )
                                        .child(
                                            text(move || {
                                                store.current_track.with(|t| {
                                                    t.as_ref()
                                                        .map(|tr| {
                                                            if !tr.published.is_empty() {
                                                                SharedString::from(
                                                                    tr.published.clone(),
                                                                )
                                                            } else if !tr.album.released.is_empty() {
                                                                SharedString::from(
                                                                    tr.album.released.clone(),
                                                                )
                                                            } else {
                                                                SharedString::from(
                                                                    format_duration_ms(tr.duration),
                                                                )
                                                            }
                                                        })
                                                        .unwrap_or_default()
                                                })
                                            })
                                            .tabular_nums()
                                            .text_size(px(font_size::CAPTION))
                                            .text_color(theme::np_muted),
                                        ),
                                )
                                // Show / Album & Host subtitle
                                .child(
                                    text(move || {
                                        let fallback_artist = store.t("track.unknown_artist");
                                        store.current_track.with(|t| {
                                            t.as_ref()
                                                .map(|tr| {
                                                    let show = if !tr.album.name.is_empty() {
                                                        tr.album.name.clone()
                                                    } else {
                                                        tr.name.clone()
                                                    };
                                                    let host = tr.artist_names();
                                                    if host.is_empty() {
                                                        SharedString::from(format!(
                                                            "{show} · {fallback_artist}"
                                                        ))
                                                    } else {
                                                        SharedString::from(format!(
                                                            "{show} · {host}"
                                                        ))
                                                    }
                                                })
                                                .unwrap_or_else(|| store.t("player.idle_title"))
                                        })
                                    })
                                    .text_size(px(font_size::SUBTITLE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::amber),
                                )
                                // Show notes body
                                .child(
                                    text(move || {
                                        let fallback_artist = store.t("track.unknown_artist");
                                        store.current_track.with(|t| {
                                            t.as_ref()
                                                .map(|tr| {
                                                    if !tr.description.trim().is_empty() {
                                                        SharedString::from(tr.description.clone())
                                                    } else {
                                                        let dur = format_duration_ms(tr.duration);
                                                        let artist = tr.artist_names();
                                                        let by = if artist.is_empty() {
                                                            fallback_artist.to_string()
                                                        } else {
                                                            artist
                                                        };
                                                        SharedString::from(format!(
                                                            "{} — {by} ({dur})",
                                                            tr.name
                                                        ))
                                                    }
                                                })
                                                .unwrap_or_else(|| store.t("common.empty"))
                                        })
                                    })
                                    .text_size(px(font_size::BODY))
                                    .text_color(move || theme::np_fg().opacity(0.90)),
                                ),
                        )
                        // Chapters section (click any chapter to seek)
                        .child(
                            view()
                                .w_full()
                                .gap_2()
                                .child(
                                    text("Chapters")
                                        .text_size(px(font_size::SUBTITLE))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme::np_muted),
                                )
                                .child(each(
                                    chapters_memo,
                                    |ch: &PodcastChapter| (ch.start_ms, ch.title.clone()),
                                    move |ch_sig, idx_sig| {
                                        let is_active_ch = computed(move || {
                                            let pos = store.position_ms.get();
                                            let idx = idx_sig.get();
                                            let start = ch_sig.with(|c| c.start_ms);
                                            let next_start = chapters_memo.with(|list| {
                                                list.get(idx + 1)
                                                    .map(|c| c.start_ms)
                                                    .unwrap_or(u64::MAX)
                                            });
                                            pos >= start && pos < next_start
                                        });

                                        button()
                                            .selected(is_active_ch)
                                            .accessibility_label(move || {
                                                Some(ch_sig.with(|c| {
                                                    SharedString::from(c.title.clone())
                                                }))
                                            })
                                            .on_press(move || {
                                                let start = ch_sig.with_untracked(|c| c.start_ms);
                                                store.seek(start);
                                            })
                                            .w_full()
                                            .min_h(px(if touch { 46.0 } else { 40.0 }))
                                            .px_3()
                                            .py_2()
                                            .flex_row()
                                            .items_center()
                                            .justify_between()
                                            .gap_3()
                                            .rounded(px(radius::CONTROL))
                                            .bg(move || {
                                                if is_active_ch.get() {
                                                    theme::np_surface_hover()
                                                } else {
                                                    theme::np_surface()
                                                }
                                            })
                                            .border_1()
                                            .border_color(move || {
                                                if is_active_ch.get() {
                                                    theme::primary()
                                                } else {
                                                    theme::np_border()
                                                }
                                            })
                                            .hover(|s| s.bg(theme::np_surface_hover))
                                            .active(|s| s.scale(motion::press_scale))
                                            .focus_visible(|s| {
                                                s.outline(px(2.0)).outline_color(theme::ring)
                                            })
                                            .child(
                                                view()
                                                    .flex_1()
                                                    .min_w(px(0.0))
                                                    .flex_row()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        text(move || {
                                                            format!("{:02}", idx_sig.get() + 1)
                                                        })
                                                        .tabular_nums()
                                                        .text_size(px(font_size::CAPTION))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(move || {
                                                            if is_active_ch.get() {
                                                                theme::primary()
                                                            } else {
                                                                theme::np_muted()
                                                            }
                                                        }),
                                                    )
                                                    .child(
                                                        text(move || {
                                                            ch_sig.with(|c| {
                                                                SharedString::from(c.title.clone())
                                                            })
                                                        })
                                                        .flex_1()
                                                        .min_w(px(0.0))
                                                        .number_of_lines(1)
                                                        .text_size(px(font_size::BODY))
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .text_color(theme::np_fg),
                                                    ),
                                            )
                                            .child(
                                                text(move || {
                                                    let ms = ch_sig.with(|c| c.start_ms);
                                                    SharedString::from(format_duration_ms(
                                                        ms as i64,
                                                    ))
                                                })
                                                .tabular_nums()
                                                .text_size(px(font_size::CAPTION))
                                                .text_color(theme::np_muted),
                                            )
                                    },
                                )),
                        )
                        // Related podcast shows quick-jump when available
                        .child(when(
                            move || store.podcasts.with(|p| !p.is_empty()),
                            move || {
                                view()
                                    .w_full()
                                    .gap_2()
                                    .child(
                                        text(move || store.t("nav.podcasts"))
                                            .text_size(px(font_size::SUBTITLE))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme::np_muted),
                                    )
                                    .child(each(
                                        store.podcasts,
                                        |show| show.id.clone(),
                                        move |show_sig, _| {
                                            button()
                                                .accessibility_label(move || {
                                                    Some(show_sig.with(|s| {
                                                        SharedString::from(s.name.clone())
                                                    }))
                                                })
                                                .on_press(move || {
                                                    let id =
                                                        show_sig.with_untracked(|s| s.id.clone());
                                                    store.close_now_playing();
                                                    store.navigate(
                                                        crate::state::Route::ShowDetail { id },
                                                    );
                                                })
                                                .w_full()
                                                .min_h(px(if touch { 50.0 } else { 44.0 }))
                                                .px_3()
                                                .py_2()
                                                .flex_row()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .rounded(px(radius::CONTROL))
                                                .bg(theme::np_surface)
                                                .border_1()
                                                .border_color(theme::np_border)
                                                .hover(|s| s.bg(theme::np_surface_hover))
                                                .active(|s| s.scale(motion::press_scale))
                                                .focus_visible(|s| {
                                                    s.outline(px(2.0)).outline_color(theme::ring)
                                                })
                                                .child(
                                                    view()
                                                        .flex_1()
                                                        .min_w(px(0.0))
                                                        .child(
                                                            text(move || {
                                                                show_sig.with(|s| {
                                                                    SharedString::from(
                                                                        s.name.clone(),
                                                                    )
                                                                })
                                                            })
                                                            .number_of_lines(1)
                                                            .text_size(px(font_size::BODY))
                                                            .font_weight(FontWeight::MEDIUM)
                                                            .text_color(theme::np_fg),
                                                        )
                                                        .child(
                                                            text(move || {
                                                                show_sig.with(|s| {
                                                                    SharedString::from(
                                                                        s.publisher.clone(),
                                                                    )
                                                                })
                                                            })
                                                            .number_of_lines(1)
                                                            .text_size(px(font_size::CAPTION))
                                                            .text_color(theme::np_muted),
                                                        ),
                                                )
                                                .child(icon(
                                                    Icon::ChevronRight,
                                                    15.0,
                                                    colors::NP_MUTED,
                                                ))
                                        },
                                    ))
                            },
                        )),
                ),
        )
}

fn extract_or_build_chapters(track: Option<&Track>, duration_ms: u64) -> Vec<PodcastChapter> {
    if let Some(tr) = track {
        let mut parsed = Vec::new();
        for raw_line in tr.description.lines() {
            let line = raw_line.trim();
            if let Some((ms, title)) = parse_chapter_line(line) {
                parsed.push(PodcastChapter {
                    title,
                    start_ms: ms,
                });
            }
        }
        if !parsed.is_empty() {
            return parsed;
        }
    }

    let total = track
        .map(|t| t.duration.max(0) as u64)
        .unwrap_or(duration_ms)
        .max(duration_ms);
    if total < 30_000 {
        return vec![PodcastChapter {
            title: track
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "Full Playback".to_owned()),
            start_ms: 0,
        }];
    }

    vec![
        PodcastChapter {
            title: "Opening & Introduction".to_owned(),
            start_ms: 0,
        },
        PodcastChapter {
            title: "Main Theme & Discussion".to_owned(),
            start_ms: total / 4,
        },
        PodcastChapter {
            title: "Deep Dive & Highlights".to_owned(),
            start_ms: total / 2,
        },
        PodcastChapter {
            title: "Closing Notes".to_owned(),
            start_ms: (total * 3) / 4,
        },
    ]
}

fn parse_chapter_line(line: &str) -> Option<(u64, String)> {
    let first_tok = line.split_whitespace().next()?;
    let clean = first_tok.trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')');
    let parts: Vec<&str> = clean.split(':').collect();
    let secs: u64 = match parts.as_slice() {
        [mm, ss] => mm.parse::<u64>().ok()? * 60 + ss.parse::<u64>().ok()?,
        [hh, mm, ss] => {
            hh.parse::<u64>().ok()? * 3600 + mm.parse::<u64>().ok()? * 60 + ss.parse::<u64>().ok()?
        }
        _ => return None,
    };
    let rest = line[first_tok.len()..]
        .trim()
        .trim_start_matches(['-', '·', ':'])
        .trim();
    let title = if rest.is_empty() {
        format!("Chapter {clean}")
    } else {
        rest.to_owned()
    };
    Some((secs * 1000, title))
}

fn queue_cover_thumb(track_memo: Memo<Option<Track>>) -> View {
    let cover_uri = move || {
        track_memo.with(|t| {
            t.as_ref()
                .map(|tr| {
                    if !tr.local_cover.is_empty() {
                        SharedString::from(tr.local_cover.clone())
                    } else {
                        SharedString::from(tr.album.image_url.clone())
                    }
                })
                .unwrap_or_default()
        })
    };
    let has_img = move || !cover_uri().is_empty();

    view()
        .size(px(38.0))
        .flex_shrink_0()
        .rounded(px(radius::SM))
        .overflow_hidden()
        .border_1()
        .border_color(theme::np_border)
        .bg_linear_to_br()
        .gradient_from(move || {
            let (a, _) = track_memo.with(|t| {
                let seed = t
                    .as_ref()
                    .map(|tr| {
                        if !tr.album.id.is_empty() {
                            tr.album.id.as_str()
                        } else {
                            tr.id.as_str()
                        }
                    })
                    .unwrap_or("queue-item");
                theme::fallback_cover_gradient(seed)
            });
            AdaptiveColor::from(a)
        })
        .gradient_to(move || {
            let (_, b) = track_memo.with(|t| {
                let seed = t
                    .as_ref()
                    .map(|tr| {
                        if !tr.album.id.is_empty() {
                            tr.album.id.as_str()
                        } else {
                            tr.id.as_str()
                        }
                    })
                    .unwrap_or("queue-item");
                theme::fallback_cover_gradient(seed)
            });
            AdaptiveColor::from(b)
        })
        .items_center()
        .justify_center()
        .child(icon(Icon::Disc, 16.0, colors::NP_MUTED).opacity(0.65))
        .child(when(has_img, move || {
            image(cover_uri)
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
        }))
}

// -----------------------------------------------------------------------------
// Shared Dark-Stage Helpers
// -----------------------------------------------------------------------------

fn stage_icon_chip(
    glyph: impl IntoReactive<Icon>,
    active: impl IntoReactive<bool>,
    label: impl IntoReactive<SharedString>,
    touch: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active_rx = active.into_reactive();
    let label_rx = label.into_reactive();
    let a_bg = active_rx.clone();
    let a_border = active_rx.clone();
    let a_tint = active_rx.clone();

    button()
        .selected(move || active_rx.get())
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .size(px(if touch { 44.0 } else { 32.0 }))
        .flex_shrink_0()
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
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, 16.0, move || {
            if a_tint.get() {
                theme::primary()
            } else {
                theme::np_muted()
            }
        }))
}

fn panel_empty_state(
    glyph: Icon,
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
) -> View {
    view()
        .flex_1()
        .w_full()
        .py_10()
        .px_6()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            view()
                .size(px(54.0))
                .rounded_full()
                .bg(theme::np_surface)
                .border_1()
                .border_color(theme::np_border)
                .items_center()
                .justify_center()
                .child(icon(glyph, 22.0, colors::NP_MUTED)),
        )
        .child(
            text(title)
                .text_size(px(font_size::HEADING))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::np_fg),
        )
        .child(
            text(subtitle)
                .text_center()
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::np_muted),
        )
}
