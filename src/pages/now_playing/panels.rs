//! Switchable borderless Now Playing panels (`NowPlayingPanel::{Lyrics, Stage, Queue, Wall, Comments, Podcast}`)
//! aligned 1:1 with `egoist/mygo-music` (`ui_nowplaying.go:1780-1823`, `ui_comments.go`):
//! - Borderless floating surfaces directly over the ambient fog/gradient backdrop
//! - `Queue`: `58pt` floating rows (`42pt` cover resolved at 120px, active `white 0.12`,
//!   hover `white 0.08`, zero per-row spectrum polling)
//! - `Comments`: top composer (`postComment`), 6-row `commentSkeleton` shimmer while
//!   `store.comments_loading`, borderless rows with `34pt` avatar + `ThumbUp` like toggle
//!   + `now_playing.comments.all_loaded` footer
//! - `Podcast`: episode show notes, seekable chapters, and related shows

use chrono::{DateTime, Local};
use nsh_base::{button, input};
use nsh_core::{LineHeight, prelude::*};
use nsh_music_core::model::{Comment, Track};

use super::{lyrics_view, stage, wall};
use crate::{
    state::{AppStore, ArtworkLoader, NowPlayingPanel, Route},
    ui::{
        icons::{Icon, icon, icon_reactive},
        motion, shimmer_block_on_dark,
        theme::{self, DeviceForm, colors, font_size, radius},
        track_row::format_duration_ms,
    },
};

/// Resolves the effective visible panel across all 6 [`NowPlayingPanel`] variants,
/// enforcing episode rules (`About` / `Up Next` / `Comments`) and source capability
/// gating (`active_can("comments")`).
pub fn effective_panel(store: AppStore) -> NowPlayingPanel {
    let panel = store.now_playing_panel.get();
    let allow_comments = store.active_can("comments");
    let is_episode = store
        .current_track
        .with(|t| t.as_ref().is_some_and(Track::is_episode));

    if is_episode {
        match panel {
            NowPlayingPanel::Lyrics | NowPlayingPanel::Stage => NowPlayingPanel::Podcast,
            NowPlayingPanel::Wall => NowPlayingPanel::Queue,
            NowPlayingPanel::Comments if !allow_comments => NowPlayingPanel::Queue,
            other => other,
        }
    } else {
        match panel {
            NowPlayingPanel::Podcast => NowPlayingPanel::Lyrics,
            NowPlayingPanel::Comments if !allow_comments => NowPlayingPanel::Lyrics,
            other => other,
        }
    }
}

/// Renders the currently selected right-pane [`NowPlayingPanel`] body (for split views:
/// `Lyrics`, `Queue`, `Comments`, `Podcast`, and also safely dispatches `Stage`/`Wall`).
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
                NowPlayingPanel::Stage => stage::stage_panel(store, form).key("np-panel-stage"),
                NowPlayingPanel::Queue => queue_panel(store, form).key("np-panel-queue"),
                NowPlayingPanel::Wall => wall::wall_panel(store, form).key("np-panel-wall"),
                NowPlayingPanel::Comments => comments_panel(store, form).key("np-panel-comments"),
                NowPlayingPanel::Podcast => podcast_panel(store, form).key("np-panel-podcast"),
            })
        }))
}

// -----------------------------------------------------------------------------
// Queue Panel (`ui_nowplaying.go:1780-1823`) — Borderless floating play order
// -----------------------------------------------------------------------------

pub fn queue_panel(store: AppStore, form: DeviceForm) -> View {
    let is_empty = computed(move || store.queue.with(Vec::is_empty));
    let touch = form.is_touch();
    let is_phone = form.is_phone();

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .gap_1p5()
        // Subtle minimal header row with count & Clear action (only when non-empty)
        .child(when(
            move || !is_empty.get(),
            move || {
                view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(if is_phone { 20.0 } else { 10.0 }))
                    .child(
                        text(move || {
                            let n = store.queue.with(Vec::len);
                            format!("{} · {n}", store.t("player.up_next"))
                        })
                        .tabular_nums()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || theme::np_fg().opacity(0.52)),
                    )
                    .child(
                        button()
                            .accessibility_label(move || Some(store.t("player.queue_clear")))
                            .on_press(move || store.clear_queue())
                            .min_h(px(if touch { 44.0 } else { 28.0 }))
                            .min_w(px(if touch { 44.0 } else { 0.0 }))
                            .px(px(if touch { 0.0 } else { 12.0 }))
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(colors::TRANSPARENT)
                            .transition_colors()
                            .duration(move || motion::transition_ms(120.0))
                            .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
                            .active(|s| s.scale(motion::press_scale))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .child(
                                view()
                                    .h(px(if touch { 28.0 } else { 24.0 }))
                                    .px(px(if touch { 11.0 } else { 0.0 }))
                                    .rounded_full()
                                    .items_center()
                                    .justify_center()
                                    .bg(move || {
                                        if touch {
                                            theme::np_fg().opacity(0.09)
                                        } else {
                                            colors::TRANSPARENT
                                        }
                                    })
                                    .child(
                                        text(move || store.t("player.queue_clear"))
                                            .number_of_lines(1)
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(move || theme::np_fg().opacity(0.68)),
                                    ),
                            ),
                    )
            },
        ))
        // Queue rows list
        .child(
            when(is_empty, move || {
                panel_empty_state(move || store.t("player.queue_empty"))
            })
            .otherwise(move || {
                virtual_list(
                    move || store.queue.with(Vec::len),
                    move |at| queue_row_cell(store, at, touch, is_phone),
                )
                .keys(move || {
                    store.queue.with(|q| {
                        q.iter()
                            .enumerate()
                            .map(|(i, t)| format!("{i}:{}", t.id))
                            .collect::<Vec<_>>()
                    })
                })
                .estimated_item_size(px(if touch { 62.0 } else { 58.0 }))
                .row_gap(px(if is_phone { 4.0 } else { 2.0 }))
                .content_padding_x(px(if is_phone { 12.0 } else { 0.0 }))
                .content_padding_top(px(4.0))
                .content_padding_bottom(px(40.0))
                .scrollbar(false)
                .flex_1()
                .w_full()
            }),
        )
}

fn queue_row_cell(store: AppStore, at: Position, touch: bool, is_phone: bool) -> View {
    let track_memo = computed(move || store.queue.with(|q| q.get(at.index()).cloned()));
    let is_current = computed(move || store.current_index.get() == Some(at.index()));

    let row_min_h = if touch { 60.0 } else { 58.0 };
    let cover_thumb = queue_cover_thumb(track_memo, is_phone);

    let main_btn = button()
        .accessibility_label(move || {
            track_memo.with(|t| t.as_ref().map(|tr| SharedString::from(tr.name.clone())))
        })
        .on_press(move || {
            if !is_current.get_untracked() {
                store.play_from_queue(at.index());
            }
        })
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(if touch { 56.0 } else { 48.0 }))
        .flex_row()
        .items_center()
        .gap(px(if is_phone { 12.0 } else { 14.0 }))
        .active(|s| s.opacity(0.78))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        // Cover thumbnail (`44x44` Phone, `42x42` Desktop)
        .child(cover_thumb)
        // Title (`14.5pt` Phone / `14pt` Desktop) & Artist (`12.5pt` `white 0.58`)
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
                    .text_size(px(if is_phone { 14.5 } else { 14.0 }))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || {
                        if is_current.get() && is_phone {
                            AdaptiveColor::from(theme::artwork_np_accent(
                                store.current_accent.get(),
                            ))
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
                    .text_size(px(12.5))
                    .text_color(move || theme::np_fg().opacity(0.58)),
                ),
        )
        // Duration (`12pt` tabular, `white 0.44`)
        .child(
            text(move || {
                track_memo.with(|t| {
                    t.as_ref()
                        .map(|tr| SharedString::from(format_duration_ms(tr.duration)))
                        .unwrap_or_default()
                })
            })
            .tabular_nums()
            .text_size(px(12.0))
            .text_color(move || theme::np_fg().opacity(0.44)),
        );

    let make_remove_btn = move || {
        button()
            .accessibility_label(move || Some(store.t("common.remove")))
            .on_press(move || store.remove_from_queue(at.index()))
            .size(px(if touch { 44.0 } else { 28.0 }))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(colors::TRANSPARENT)
            .hover(|s| s.bg(move || theme::np_fg().opacity(0.12)))
            .active(|s| s.scale(motion::press_scale))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(icon(Icon::Close, 13.0, colors::NP_FG).opacity(0.55))
    };

    if touch {
        // Mobile fast-path: skip desktop hover signal, mouse listeners, color transitions,
        // and reactive `when` wrapper around the always-visible touch remove button.
        return view()
            .w_full()
            .min_h(px(row_min_h))
            .px(px(if is_phone { 8.0 } else { 10.0 }))
            .flex_row()
            .items_center()
            .gap(px(if is_phone { 12.0 } else { 14.0 }))
            .rounded(px(12.0))
            .bg(move || {
                if is_current.get() {
                    theme::np_fg().opacity(if is_phone { 0.14 } else { 0.12 })
                } else {
                    colors::TRANSPARENT
                }
            })
            .child(main_btn)
            .child(make_remove_btn());
    }

    let hovered = signal(false);

    view()
        .w_full()
        .min_h(px(row_min_h))
        .px(px(10.0))
        .flex_row()
        .items_center()
        .gap(px(14.0))
        .rounded(px(12.0))
        .bg(move || {
            if is_current.get() {
                theme::np_fg().opacity(0.12)
            } else if hovered.get() {
                theme::np_fg().opacity(0.08)
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(120.0))
        .on_mouse_enter(move |_: &MouseEvent| hovered.set(true))
        .on_mouse_leave(move |_: &MouseEvent| hovered.set(false))
        .child(main_btn)
        // Hover-revealed remove button on desktop
        .child(when(hovered, make_remove_btn))
}

fn queue_cover_thumb(track_memo: Memo<Option<Track>>, is_phone: bool) -> View {
    let cover_uri = computed(move || {
        track_memo.with(|t| {
            t.as_ref()
                .map(|tr| {
                    let raw = if !tr.local_cover.is_empty() {
                        tr.local_cover.as_str()
                    } else {
                        tr.album.image_url.as_str()
                    };
                    if raw.is_empty() {
                        SharedString::default()
                    } else {
                        SharedString::from(ArtworkLoader::resolve_template(raw, 120))
                    }
                })
                .unwrap_or_default()
        })
    });
    let has_img = computed(move || cover_uri.with(|u| !u.is_empty()));
    let thumb_size = if is_phone { 44.0 } else { 42.0 };

    view()
        .size(px(thumb_size))
        .flex_shrink_0()
        .rounded(px(if is_phone { 8.0 } else { 7.0 }))
        .overflow_hidden()
        .bg(move || theme::np_fg().opacity(0.08))
        .items_center()
        .justify_center()
        .child(when(
            move || !has_img.get(),
            || icon(Icon::Disc, 16.0, colors::NP_FG).opacity(0.35),
        ))
        .child(when(has_img, move || {
            image(move || cover_uri.get())
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
        }))
}

// -----------------------------------------------------------------------------
// Comments Panel (`ui_comments.go:1263-1421`) — Borderless editorial feed
// -----------------------------------------------------------------------------

pub fn comments_panel(store: AppStore, form: DeviceForm) -> View {
    let draft = signal(String::new());
    let touch = form.is_touch();
    let is_phone = form.is_phone();
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
        .gap_2()
        // Comment composer at TOP (matching `ui_comments.go:1265-1267`, gated on `postComment`)
        .child(when(
            move || store.active_can("postComment"),
            move || {
                view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pl(px(if is_phone { 20.0 } else { 0.0 }))
                    .pr(px(if is_phone { 20.0 } else { 8.0 }))
                    .pb_1()
                    .child(
                        input()
                            .value(move || draft.get())
                            .on_change(move |txt: &str| draft.set(txt.to_owned()))
                            .on_submit(move |val: &str| {
                                if !val.trim().is_empty() {
                                    draft.set(val.to_owned());
                                }
                                submit_comment();
                            })
                            .placeholder(move || {
                                store.t("now_playing.comments.placeholder").to_string()
                            })
                            .placeholder_color(|| color(theme::np_fg()).alpha(0.42))
                            .caret_color(|| color(theme::np_fg()))
                            .selection_color(|| color(theme::primary_soft()))
                            .return_key_type(ReturnKeyType::Send)
                            .flex_1()
                            .min_w(px(0.0))
                            .h(px(if touch { 44.0 } else { 36.0 }))
                            .px(px(if is_phone { 16.0 } else { 12.0 }))
                            .rounded(px(if is_phone { 22.0 } else { 10.0 }))
                            .bg(move || theme::np_fg().opacity(0.08))
                            .border_1()
                            .border_color(move || theme::np_fg().opacity(0.12))
                            .text_size(px(13.5))
                            .text_color(theme::np_fg)
                            .focus(|s| s.border_color(move || theme::np_fg().opacity(0.32))),
                    )
                    .child(
                        button()
                            .disabled(move || !can_post.get())
                            .accessibility_label(move || Some(store.t("now_playing.comments.post")))
                            .on_press(submit_comment)
                            .min_h(px(if touch { 44.0 } else { 34.0 }))
                            .min_w(px(if touch { 44.0 } else { 0.0 }))
                            .px_4()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(move || theme::np_fg().opacity(0.16))
                            .hover(|s| s.bg(move || theme::np_fg().opacity(0.22)))
                            .active(|s| s.scale(motion::press_scale))
                            .disabled_style(|s| s.opacity(0.45))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .child(
                                text(move || store.t("now_playing.comments.post"))
                                    .number_of_lines(1)
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::np_fg),
                            ),
                    )
            },
        ))
        // Comments feed: shimmer skeleton while loading, clean empty state, or borderless list
        .child(
            when(is_empty, move || {
                when(
                    move || store.comments_loading.get(),
                    move || comment_skeleton(is_phone),
                )
                .otherwise(move || {
                    view()
                        .flex_1()
                        .w_full()
                        .px(px(if is_phone { 20.0 } else { 12.0 }))
                        .items_center()
                        .justify_center()
                        .gap_3p5()
                        .child(icon(Icon::Comment, 36.0, colors::NP_FG).opacity(0.30))
                        .child(
                            text(move || store.t("now_playing.comments.empty"))
                                .text_size(px(17.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || theme::np_fg().opacity(0.45)),
                        )
                })
            })
            .otherwise(move || {
                view().flex_1().w_full().min_h(px(0.0)).child(
                    virtual_list(
                        move || store.comments.with(Vec::len) + 1,
                        move |at| {
                            let is_footer =
                                computed(move || at.index() >= store.comments.with(Vec::len));
                            view().w_full().child(
                                when(is_footer, move || {
                                    let show_load_more = computed(move || {
                                        store.comments_more.get()
                                            || store.comments_loading_more.get()
                                    });
                                    view()
                                        .w_full()
                                        .min_h(px(56.0))
                                        .py_2()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            when(show_load_more, move || {
                                                button()
                                                    .test_id("comments-load-more-btn")
                                                    .disabled(move || {
                                                        store.comments_loading_more.get()
                                                    })
                                                    .accessibility_label(move || {
                                                        Some(if store.comments_loading_more.get() {
                                                            store.t("common.loading_more")
                                                        } else {
                                                            store.t("common.load_more")
                                                        })
                                                    })
                                                    .on_press(move || store.load_more_comments())
                                                    .min_h(px(if touch { 44.0 } else { 32.0 }))
                                                    .px_4()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .bg(move || theme::np_fg().opacity(0.14))
                                                    .border_1()
                                                    .border_color(move || {
                                                        theme::np_fg().opacity(0.14)
                                                    })
                                                    .transition_colors()
                                                    .duration(move || motion::transition_ms(120.0))
                                                    .hover(|s| {
                                                        s.bg(move || theme::np_fg().opacity(0.22))
                                                    })
                                                    .active(|s| s.scale(motion::press_scale))
                                                    .disabled_style(|s| s.opacity(0.45))
                                                    .focus_visible(|s| {
                                                        s.outline(px(2.0))
                                                            .outline_color(theme::ring)
                                                    })
                                                    .child(
                                                        text(move || {
                                                            if store.comments_loading_more.get() {
                                                                store.t("common.loading_more")
                                                            } else {
                                                                store.t("common.load_more")
                                                            }
                                                        })
                                                        .number_of_lines(1)
                                                        .text_size(px(12.5))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(theme::np_fg),
                                                    )
                                            })
                                            .otherwise(move || {
                                                text(move || {
                                                    store.t("now_playing.comments.all_loaded")
                                                })
                                                .text_size(px(12.0))
                                                .text_color(move || theme::np_fg().opacity(0.35))
                                            }),
                                        )
                                })
                                .otherwise(move || comment_row_cell(store, at, touch, is_phone)),
                            )
                        },
                    )
                    .keys(move || {
                        store.comments.with(|list| {
                            let mut keys: Vec<String> = list.iter().map(|c| c.id.clone()).collect();
                            keys.push("__comments_footer__".to_owned());
                            keys
                        })
                    })
                    .estimated_item_size(px(96.0))
                    .row_gap(px(2.0))
                    .content_padding_x(px(if is_phone { 12.0 } else { 0.0 }))
                    .content_padding_top(px(4.0))
                    .content_padding_bottom(px(48.0))
                    .keyboard_taps(KeyboardTaps::Handled)
                    .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
                    .scrollbar(false)
                    .flex_1()
                    .w_full(),
                )
            }),
        )
}

/// 6-row comment skeleton matching `mygo-music` `commentSkeleton` (`/tmp/mygo_skeleton.go:186-202`).
fn comment_skeleton(is_phone: bool) -> View {
    const AUTHOR_W: &[f32] = &[112.0, 96.0, 128.0, 104.0, 118.0, 92.0];
    const LINE2_W: &[f32] = &[0.72, 0.58, 0.0, 0.66, 0.48, 0.0];

    view()
        .flex_1()
        .w_full()
        .pt_1()
        .pl(px(if is_phone { 12.0 } else { 0.0 }))
        .pr(px(if is_phone { 12.0 } else { 8.0 }))
        .gap_0p5()
        .children((0..6_usize).map(move |i| {
            let l2 = LINE2_W[i];
            view()
                .w_full()
                .px(px(if is_phone { 8.0 } else { 12.0 }))
                .py_3()
                .flex_row()
                .items_start()
                .gap_3()
                .child(shimmer_block_on_dark(17.0).size(px(34.0)).flex_shrink_0())
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1p5()
                        .child(shimmer_block_on_dark(5.0).w(px(AUTHOR_W[i])).h(px(13.0)))
                        .child(shimmer_block_on_dark(6.0).w(relative(0.94)).h(px(15.0)))
                        .children(
                            (l2 > 0.0)
                                .then(|| shimmer_block_on_dark(6.0).w(relative(l2)).h(px(15.0))),
                        )
                        .child(shimmer_block_on_dark(4.0).w(px(96.0)).h(px(11.5))),
                )
        }))
}

fn comment_row_cell(store: AppStore, at: Position, touch: bool, is_phone: bool) -> View {
    let cmt_memo: Memo<Option<Comment>> =
        computed(move || store.comments.with(|list| list.get(at.index()).cloned()));
    let can_like = move || store.active_can("likeComment");

    let avatar_uri = move || {
        cmt_memo.with(|c| {
            c.as_ref()
                .map(|cmt| {
                    if cmt.avatar_url.is_empty() {
                        SharedString::default()
                    } else {
                        SharedString::from(ArtworkLoader::resolve_template(&cmt.avatar_url, 120))
                    }
                })
                .unwrap_or_default()
        })
    };

    let author_initial = move || {
        cmt_memo.with(|c| {
            c.as_ref()
                .and_then(|cmt| cmt.author.chars().next())
                .map(|ch| SharedString::from(ch.to_uppercase().to_string()))
                .unwrap_or_else(|| SharedString::from("?"))
        })
    };

    let is_liked = move || cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.liked));

    view()
        .w_full()
        .px(px(if is_phone { 8.0 } else { 12.0 }))
        .py_3()
        .flex_row()
        .items_start()
        .gap_3()
        .rounded(px(12.0))
        // 34pt circular avatar (`ui_comments.go:1377`)
        .child(
            view()
                .size(px(34.0))
                .flex_shrink_0()
                .rounded_full()
                .overflow_hidden()
                .bg(move || theme::np_fg().opacity(0.10))
                .items_center()
                .justify_center()
                .child(
                    text(author_initial)
                        .text_size(px(font_size::CAPTION))
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
        // Right column: Author + Like thumb, 15pt body, subtle time
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_1p5()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            text(move || {
                                cmt_memo.with(|c| {
                                    c.as_ref()
                                        .map(|cmt| SharedString::from(cmt.author.clone()))
                                        .unwrap_or_default()
                                })
                            })
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || theme::np_fg().opacity(0.60)),
                        )
                        // Comment like thumb (`Icon::ThumbUp` / `Icon::ThumbUpFilled`)
                        .child(when(
                            move || {
                                can_like()
                                    || cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.likes > 0))
                            },
                            move || {
                                button()
                                    .disabled(move || !can_like())
                                    .selected(is_liked)
                                    .accessibility_label(move || {
                                        Some(if is_liked() {
                                            store.t("player.unlike")
                                        } else {
                                            store.t("player.like")
                                        })
                                    })
                                    .on_press(move || {
                                        if let Some(cmt) = cmt_memo.get_untracked() {
                                            store.toggle_like_comment(&cmt.id);
                                        }
                                    })
                                    .min_h(px(if touch { 44.0 } else { 26.0 }))
                                    .min_w(px(if touch { 44.0 } else { 28.0 }))
                                    .px_1p5()
                                    .flex_row()
                                    .items_center()
                                    .justify_center()
                                    .gap_1()
                                    .rounded_full()
                                    .bg(colors::TRANSPARENT)
                                    .transition_colors()
                                    .duration(move || motion::transition_ms(120.0))
                                    .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
                                    .active(|s| s.scale(motion::press_scale))
                                    .focus_visible(|s| {
                                        s.outline(px(2.0)).outline_color(theme::ring)
                                    })
                                    .child(icon_reactive(
                                        move || {
                                            if is_liked() {
                                                Icon::ThumbUpFilled
                                            } else {
                                                Icon::ThumbUp
                                            }
                                        },
                                        13.0,
                                        move || {
                                            if is_liked() {
                                                theme::primary()
                                            } else {
                                                theme::np_fg().opacity(0.45)
                                            }
                                        },
                                    ))
                                    .child(when(
                                        move || {
                                            cmt_memo
                                                .with(|c| c.as_ref().is_some_and(|x| x.likes > 0))
                                        },
                                        move || {
                                            text(move || {
                                                cmt_memo.with(|c| {
                                                    c.as_ref()
                                                        .map(|x| {
                                                            SharedString::from(x.likes.to_string())
                                                        })
                                                        .unwrap_or_default()
                                                })
                                            })
                                            .tabular_nums()
                                            .text_size(px(12.0))
                                            .text_color(move || {
                                                if is_liked() {
                                                    theme::primary()
                                                } else {
                                                    theme::np_fg().opacity(0.45)
                                                }
                                            })
                                        },
                                    ))
                            },
                        )),
                )
                // Body (`15pt`, `line_height 1.55`, `white 0.92`)
                .child(
                    text(move || {
                        cmt_memo.with(|c| {
                            c.as_ref()
                                .map(|cmt| SharedString::from(cmt.text.clone()))
                                .unwrap_or_default()
                        })
                    })
                    .text_size(px(15.0))
                    .line_height(LineHeight::Multiple(1.55))
                    .text_color(move || theme::np_fg().opacity(0.92)),
                )
                // Subtle timestamp (`11.5pt`, `white 0.38`)
                .child(when(
                    move || cmt_memo.with(|c| c.as_ref().is_some_and(|x| x.time > 0)),
                    move || {
                        text(move || {
                            cmt_memo.with(|c| {
                                c.as_ref()
                                    .map(|cmt| SharedString::from(format_comment_time(cmt.time)))
                                    .unwrap_or_default()
                            })
                        })
                        .number_of_lines(1)
                        .text_size(px(11.5))
                        .text_color(move || theme::np_fg().opacity(0.38))
                    },
                )),
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
// Podcast Show Notes & Chapters Panel — Borderless editorial layout
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct PodcastChapter {
    title: String,
    start_ms: u64,
}

pub fn podcast_panel(store: AppStore, form: DeviceForm) -> View {
    let touch = form.is_touch();
    let is_phone = form.is_phone();

    let chapters_memo = computed(move || {
        let track = store.current_track.get();
        let dur_ms = store.duration_ms.get();
        extract_or_build_chapters(track.as_ref(), dur_ms)
    });

    view().flex_1().w_full().min_h(px(0.0)).child(
        scroll().flex_1().w_full().scrollbar(false).child(
            view()
                .w_full()
                .gap_5()
                .px(px(if is_phone { 20.0 } else { 8.0 }))
                .pt(px(if is_phone { 4.0 } else { 0.0 }))
                .pb_8()
                // Episode / Show Notes section (borderless floating typography)
                .child(
                    view()
                        .w_full()
                        .gap_2p5()
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    text(move || store.t("podcast.description"))
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::BOLD)
                                        .letter_spacing(px(0.4))
                                        .text_color(move || theme::np_fg().opacity(0.52)),
                                )
                                .child(
                                    text(move || {
                                        store.current_track.with(|t| {
                                            t.as_ref()
                                                .map(|tr| {
                                                    if !tr.published.is_empty() {
                                                        SharedString::from(tr.published.clone())
                                                    } else if !tr.album.released.is_empty() {
                                                        SharedString::from(
                                                            tr.album.released.clone(),
                                                        )
                                                    } else {
                                                        SharedString::from(format_duration_ms(
                                                            tr.duration,
                                                        ))
                                                    }
                                                })
                                                .unwrap_or_default()
                                        })
                                    })
                                    .tabular_nums()
                                    .text_size(px(12.0))
                                    .text_color(move || theme::np_fg().opacity(0.45)),
                                ),
                        )
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
                                                SharedString::from(format!("{show} · {host}"))
                                            }
                                        })
                                        .unwrap_or_else(|| store.t("player.idle_title"))
                                })
                            })
                            .text_size(px(16.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::np_fg),
                        )
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
                            .text_size(px(14.5))
                            .line_height(LineHeight::Multiple(1.6))
                            .text_color(move || theme::np_fg().opacity(0.84)),
                        ),
                )
                // Seekable chapters
                .child(view().w_full().gap_1().child(each(
                    chapters_memo,
                    |ch: &PodcastChapter| (ch.start_ms, ch.title.clone()),
                    move |ch_sig, idx_sig| {
                        let is_active_ch = computed(move || {
                            let pos = store.position_ms.get();
                            let idx = idx_sig.get();
                            let start = ch_sig.with(|c| c.start_ms);
                            let next_start = chapters_memo.with(|list| {
                                list.get(idx + 1).map(|c| c.start_ms).unwrap_or(u64::MAX)
                            });
                            pos >= start && pos < next_start
                        });

                        button()
                            .selected(is_active_ch)
                            .accessibility_label(move || {
                                Some(ch_sig.with(|c| SharedString::from(c.title.clone())))
                            })
                            .on_press(move || {
                                let start = ch_sig.with_untracked(|c| c.start_ms);
                                store.seek(start);
                            })
                            .w_full()
                            .min_h(px(if touch { 52.0 } else { 40.0 }))
                            .px_3()
                            .py_2()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .rounded(px(radius::CONTROL))
                            .bg(move || {
                                if is_active_ch.get() {
                                    theme::np_fg().opacity(0.12)
                                } else {
                                    colors::TRANSPARENT
                                }
                            })
                            .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
                            .active(|s| s.scale(motion::press_scale))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .child(
                                view()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex_row()
                                    .items_center()
                                    .gap_2p5()
                                    .child(
                                        text(move || format!("{:02}", idx_sig.get() + 1))
                                            .tabular_nums()
                                            .text_size(px(font_size::CAPTION))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(move || {
                                                if is_active_ch.get() {
                                                    AdaptiveColor::from(theme::artwork_np_accent(
                                                        store.current_accent.get(),
                                                    ))
                                                } else {
                                                    theme::np_fg().opacity(0.45)
                                                }
                                            }),
                                    )
                                    .child(
                                        text(move || {
                                            ch_sig.with(|c| SharedString::from(c.title.clone()))
                                        })
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .number_of_lines(1)
                                        .text_size(px(14.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::np_fg),
                                    ),
                            )
                            .child(
                                text(move || {
                                    let ms = ch_sig.with(|c| c.start_ms);
                                    SharedString::from(format_duration_ms(ms as i64))
                                })
                                .tabular_nums()
                                .text_size(px(12.0))
                                .text_color(move || theme::np_fg().opacity(0.42)),
                            )
                    },
                )))
                // Related shows quick-jump
                .child(when(
                    move || store.podcasts.with(|p| !p.is_empty()),
                    move || {
                        view()
                            .w_full()
                            .gap_1p5()
                            .child(
                                text(move || store.t("nav.podcasts"))
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(move || theme::np_fg().opacity(0.48)),
                            )
                            .child(each(
                                store.podcasts,
                                |show| show.id.clone(),
                                move |show_sig, _| {
                                    button()
                                        .accessibility_label(move || {
                                            Some(
                                                show_sig
                                                    .with(|s| SharedString::from(s.name.clone())),
                                            )
                                        })
                                        .on_press(move || {
                                            let id = show_sig.with_untracked(|s| s.id.clone());
                                            store.close_now_playing();
                                            store.navigate(Route::ShowDetail { id });
                                        })
                                        .w_full()
                                        .min_h(px(if touch { 48.0 } else { 42.0 }))
                                        .px_3()
                                        .py_2()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .rounded(px(radius::CONTROL))
                                        .bg(colors::TRANSPARENT)
                                        .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
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
                                                            SharedString::from(s.name.clone())
                                                        })
                                                    })
                                                    .number_of_lines(1)
                                                    .text_size(px(14.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(theme::np_fg),
                                                )
                                                .child(
                                                    text(move || {
                                                        show_sig.with(|s| {
                                                            SharedString::from(s.publisher.clone())
                                                        })
                                                    })
                                                    .number_of_lines(1)
                                                    .text_size(px(12.0))
                                                    .text_color(move || {
                                                        theme::np_fg().opacity(0.50)
                                                    }),
                                                ),
                                        )
                                        .child(
                                            icon(Icon::ChevronRight, 14.0, colors::NP_FG)
                                                .opacity(0.45),
                                        )
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
        return Vec::new();
    }

    vec![
        PodcastChapter {
            title: "00:00".to_owned(),
            start_ms: 0,
        },
        PodcastChapter {
            title: format_duration_ms((total / 4) as i64),
            start_ms: total / 4,
        },
        PodcastChapter {
            title: format_duration_ms((total / 2) as i64),
            start_ms: total / 2,
        },
        PodcastChapter {
            title: format_duration_ms(((total * 3) / 4) as i64),
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
            hh.parse::<u64>().ok()? * 3600
                + mm.parse::<u64>().ok()? * 60
                + ss.parse::<u64>().ok()?
        }
        _ => return None,
    };
    let rest = line[first_tok.len()..]
        .trim()
        .trim_start_matches(['-', '·', ':'])
        .trim();
    let title = if rest.is_empty() {
        clean.to_owned()
    } else {
        rest.to_owned()
    };
    Some((secs * 1000, title))
}

fn panel_empty_state(title: impl IntoReactive<SharedString>) -> View {
    view()
        .flex_1()
        .w_full()
        .py_10()
        .px_6()
        .items_center()
        .justify_center()
        .child(
            text(title)
                .text_center()
                .text_size(px(18.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || theme::np_fg().opacity(0.42)),
        )
}
