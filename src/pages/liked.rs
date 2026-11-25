//! Library Hub, Liked Songs, User Playlists management, and Recently Played pages
//! aligned with `mygo-music` (zero mock strings, shimmer loading skeletons,
//! capability-gated `CreatePlaylist { initial_track_id: None }`).

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::Playlist;

use crate::pages::{
    card_grid, page_scroll, render_track_row, shimmer_card_grid, shimmer_track_list,
    shuffle_and_play, track_table_header,
};
use crate::state::{ActiveModal, AppStore, Route};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, empty_state, empty_state_with_actions, ghost_button,
    icon, media_card, motion, primary_button, section_header, theme,
};
use crate::ui::theme::{colors, font_size, radius};

/// Unified Library hub (`Route::Library`) featuring quick navigation tiles,
/// User Playlists with "+ New Playlist", Saved Albums, and Liked Songs preview.
pub fn library_page(store: AppStore, form: DeviceForm) -> View {
    let quick_cols = match form {
        DeviceForm::Desktop => 4u16,
        DeviceForm::Pad { .. } => 2u16,
        DeviceForm::Phone => 2u16,
    };

    let quick_tiles = view()
        .w_full()
        .grid()
        .grid_cols(quick_cols)
        .gap_3()
        .child(library_stat_tile(
            Icon::HeartFilled,
            colors::PRIMARY,
            move || store.t("nav.liked"),
            move || {
                let n = store.liked_tracks.with(Vec::len).to_string();
                store.t_fmt("track.count_fmt", &[("count", &n)])
            },
            move || store.navigate(Route::Liked),
        ))
        .child(library_stat_tile(
            Icon::Library,
            colors::AMBER,
            move || store.t("nav.playlists"),
            move || SharedString::from(store.playlists.with(Vec::len).to_string()),
            move || store.navigate(Route::Playlists),
        ))
        .child(library_stat_tile(
            Icon::Disc,
            colors::OK,
            move || store.t("nav.recent"),
            move || {
                let n = store.recent_tracks.with(Vec::len).to_string();
                store.t_fmt("track.count_fmt", &[("count", &n)])
            },
            move || store.navigate(Route::Recent),
        ))
        .child(library_stat_tile(
            Icon::Folder,
            colors::PRIMARY,
            move || store.t("nav.local"),
            move || {
                let _ = store.home_data.get();
                let n = store.local_source().local_track_count().to_string();
                store.t_fmt("track.count_fmt", &[("count", &n)])
            },
            move || store.navigate(Route::Local),
        ));

    let body = view()
        .w_full()
        .gap_6()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_3()
                .child(
                    text(move || store.t("nav.library"))
                        .role(AccessibilityRole::Header)
                        .text_size(px(if form.is_phone() {
                            font_size::TITLE_LG
                        } else {
                            font_size::DISPLAY
                        }))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .child(when(
                    move || store.active_can("createPlaylist"),
                    move || {
                        primary_button(
                            move || store.t("library.create_playlist"),
                            move || {
                                store.open_modal(ActiveModal::CreatePlaylist {
                                    initial_track_id: None,
                                });
                            },
                        )
                    },
                )),
        )
        .child(quick_tiles)
        .child(playlists_section(store, form))
        .child(dynamic(move || {
            let albums = store.saved_albums.get();
            untracked(move || {
                if albums.is_empty() {
                    return view().into_element();
                }
                let cards: Vec<AnyElement> = albums
                    .iter()
                    .map(|alb| {
                        let aid = alb.id.clone();
                        let aid_play = alb.id.clone();
                        let artists: String = alb
                            .artists
                            .iter()
                            .map(|a| a.name.as_str())
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let sub = if artists.is_empty() {
                            store.t("track.unknown_artist").to_string()
                        } else {
                            artists
                        };
                        media_card(alb.name.clone(), sub)
                            .image_url(alb.image_url.clone())
                            .seed(alb.id.clone())
                            .on_press(move || {
                                store.navigate(Route::AlbumDetail { id: aid.clone() });
                            })
                            .on_play(move || {
                                let src = store.active_source();
                                let id = aid_play.clone();
                                spawn(async move {
                                    if let Ok(Ok(page)) = blocking(move || src.album(&id)).await
                                        && !page.tracks.is_empty()
                                    {
                                        store.play_list(page.tracks, 0);
                                    }
                                });
                            })
                            .into_element()
                    })
                    .collect();

                view()
                    .w_full()
                    .gap_3()
                    .child(section_header(
                        move || store.t("nav.albums"),
                        None,
                        None,
                        None,
                    ))
                    .child(card_grid(store, form, cards))
                    .into_element()
            })
        }))
        .child(liked_tracks_block(store, form, true));

    page_scroll(form, body)
}

/// Liked Songs & User Playlists page (`Route::Liked`).
pub fn liked_page(store: AppStore, form: DeviceForm) -> View {
    let play_all = move || {
        let tracks = store.liked_tracks.get_untracked();
        if !tracks.is_empty() {
            store.play_list(tracks, 0);
        }
    };
    let shuffle_all = move || {
        let tracks = store.liked_tracks.get_untracked();
        shuffle_and_play(store, tracks);
    };

    let hero = Elevated::elevation(
        view()
            .w_full()
            .p(px(if form.is_phone() { 18.0 } else { 24.0 }))
            .gap_4()
            .rounded(px(radius::XL))
            .bg_linear_to_br()
            .gradient_from(theme::primary_soft())
            .gradient_to(theme::card())
            .border_1()
            .border_color(theme::border),
        Elevation::Card,
    )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_4()
                .child(
                    Elevated::elevation(
                        view()
                            .size(px(if form.is_phone() { 64.0 } else { 88.0 }))
                            .flex_shrink_0()
                            .rounded(px(radius::LG))
                            .bg(theme::primary),
                        Elevation::Card,
                    )
                        .items_center()
                        .justify_center()
                        .child(icon(
                            Icon::HeartFilled,
                            if form.is_phone() { 28.0 } else { 38.0 },
                            colors::PRIMARY_FOREGROUND,
                        )),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1()
                        .child(
                            text(move || store.t("nav.liked"))
                                .role(AccessibilityRole::Header)
                                .number_of_lines(1)
                                .text_size(px(if form.is_phone() {
                                    font_size::TITLE_LG
                                } else {
                                    font_size::DISPLAY
                                }))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::fg),
                        )
                        .child(
                            text(move || {
                                let count = store.liked_tracks.with(Vec::len).to_string();
                                store.t_fmt("track.count_fmt", &[("count", &count)])
                            })
                            .text_size(px(font_size::SUBTITLE))
                            .text_color(theme::muted_fg),
                        ),
                ),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(
                    primary_button(move || store.t("player.play_all"), play_all)
                        .disabled(move || store.liked_tracks.with(Vec::is_empty)),
                )
                .child(
                    ghost_button(move || store.t("player.shuffle_all"), shuffle_all)
                        .disabled(move || store.liked_tracks.with(Vec::is_empty)),
                )
                .child(when(
                    move || store.active_can("createPlaylist"),
                    move || {
                        ghost_button(
                            move || store.t("library.create_playlist"),
                            move || {
                                store.open_modal(ActiveModal::CreatePlaylist {
                                    initial_track_id: None,
                                });
                            },
                        )
                    },
                )),
        );

    let body = view()
        .w_full()
        .gap_6()
        .child(hero)
        .child(liked_tracks_block(store, form, false))
        .child(playlists_section(store, form));

    page_scroll(form, body)
}

/// Playlists management directory (`Route::Playlists`).
pub fn playlists_page(store: AppStore, form: DeviceForm) -> View {
    let body = view()
        .w_full()
        .gap_5()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_3()
                .child(
                    text(move || store.t("nav.playlists"))
                        .role(AccessibilityRole::Header)
                        .text_size(px(if form.is_phone() {
                            font_size::TITLE_LG
                        } else {
                            font_size::DISPLAY
                        }))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .child(when(
                    move || store.active_can("createPlaylist"),
                    move || {
                        primary_button(
                            move || store.t("library.create_playlist"),
                            move || {
                                store.open_modal(ActiveModal::CreatePlaylist {
                                    initial_track_id: None,
                                });
                            },
                        )
                    },
                )),
        )
        .child(playlists_grid_dynamic(store, form));

    page_scroll(form, body)
}

/// Recently Played tracks page (`Route::Recent`).
pub fn recent_page(store: AppStore, form: DeviceForm) -> View {
    let body = view()
        .w_full()
        .gap_5()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_3()
                .child(
                    text(move || store.t("nav.recent"))
                        .role(AccessibilityRole::Header)
                        .text_size(px(if form.is_phone() {
                            font_size::TITLE_LG
                        } else {
                            font_size::DISPLAY
                        }))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            primary_button(move || store.t("player.play_all"), move || {
                                let tracks = store.recent_tracks.get_untracked();
                                if !tracks.is_empty() {
                                    store.play_list(tracks, 0);
                                }
                            })
                            .disabled(move || store.recent_tracks.with(Vec::is_empty)),
                        )
                        .child(
                            ghost_button(move || store.t("player.shuffle_all"), move || {
                                let tracks = store.recent_tracks.get_untracked();
                                shuffle_and_play(store, tracks);
                            })
                            .disabled(move || store.recent_tracks.with(Vec::is_empty)),
                        ),
                ),
        )
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let tracks = store.recent_tracks.get();
            untracked(move || {
                if loading && tracks.is_empty() {
                    return shimmer_track_list(6).into_element();
                }
                if tracks.is_empty() {
                    return empty_state(
                        Icon::Disc,
                        store.t("common.empty"),
                        store.t("nav.recent"),
                    )
                    .into_element();
                }
                let queue = Rc::new(tracks.clone());
                let mut rows = view().w_full().gap_1();
                if !form.is_phone() {
                    rows = rows.child(track_table_header(store, form, true));
                }
                for (idx, tr) in tracks.iter().enumerate() {
                    rows = rows.child(render_track_row(
                        store,
                        tr.clone(),
                        Some(idx),
                        Rc::clone(&queue),
                        idx,
                        true,
                    ));
                }
                view()
                    .w_full()
                    .p_2()
                    .rounded(px(radius::CARD))
                    .bg(theme::card)
                    .border_1()
                    .border_color(theme::border)
                    .child(rows)
                    .into_element()
            })
        }));

    page_scroll(form, body)
}

fn liked_tracks_block(store: AppStore, form: DeviceForm, show_header: bool) -> View {
    view()
        .w_full()
        .gap_3()
        .children(show_header.then(|| {
            section_header(
                move || store.t("nav.liked"),
                None,
                Some(store.t("common.see_all")),
                Some(Rc::new(move || store.navigate(Route::Liked))),
            )
        }))
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let tracks = store.liked_tracks.get();
            untracked(move || {
                if loading && tracks.is_empty() {
                    return shimmer_track_list(5).into_element();
                }
                if tracks.is_empty() {
                    return empty_state(
                        Icon::Heart,
                        store.t("common.empty"),
                        store.t("player.like"),
                    )
                    .into_element();
                }

                let slice_len = if show_header {
                    tracks.len().min(8)
                } else {
                    tracks.len()
                };
                let queue = Rc::new(tracks.clone());
                let mut rows = view().w_full().gap_1();
                if !form.is_phone() {
                    rows = rows.child(track_table_header(store, form, true));
                }
                for (idx, tr) in tracks.iter().take(slice_len).enumerate() {
                    rows = rows.child(render_track_row(
                        store,
                        tr.clone(),
                        Some(idx),
                        Rc::clone(&queue),
                        idx,
                        true,
                    ));
                }
                view()
                    .w_full()
                    .p_2()
                    .rounded(px(radius::CARD))
                    .bg(theme::card)
                    .border_1()
                    .border_color(theme::border)
                    .child(rows)
                    .into_element()
            })
        }))
}

fn playlists_section(store: AppStore, form: DeviceForm) -> View {
    let can_create = store.active_can("createPlaylist");
    let action_label = can_create.then(|| store.t("library.create_playlist"));
    let action_cb: Option<Rc<dyn Fn()>> = can_create.then(|| {
        Rc::new(move || {
            store.open_modal(ActiveModal::CreatePlaylist {
                initial_track_id: None,
            });
        }) as Rc<dyn Fn()>
    });

    view()
        .w_full()
        .gap_3()
        .child(section_header(
            move || store.t("nav.playlists"),
            None,
            action_label,
            action_cb,
        ))
        .child(playlists_grid_dynamic(store, form))
}

fn playlists_grid_dynamic(store: AppStore, form: DeviceForm) -> View {
    view().w_full().child(dynamic(move || {
        let loading = store.browse_loading.get();
        let playlists = store.playlists.get();
        let can_create = store.active_can("createPlaylist");

        untracked(move || {
            if loading && playlists.is_empty() {
                return shimmer_card_grid(store, form, 4, false).into_element();
            }

            if playlists.is_empty() {
                if can_create {
                    return empty_state_with_actions(
                        Icon::Library,
                        move || store.t("common.empty"),
                        move || store.t("library.create_playlist"),
                        view().child(primary_button(
                            move || store.t("library.create_playlist"),
                            move || {
                                store.open_modal(ActiveModal::CreatePlaylist {
                                    initial_track_id: None,
                                });
                            },
                        )),
                    )
                    .into_element();
                }
                return empty_state(
                    Icon::Library,
                    store.t("common.empty"),
                    store.t("nav.playlists"),
                )
                .into_element();
            }

            let cards: Vec<AnyElement> = playlists
                .iter()
                .map(|pl| playlist_card(store, pl))
                .collect();
            card_grid(store, form, cards).into_element()
        })
    }))
}

fn playlist_card(store: AppStore, pl: &Playlist) -> AnyElement {
    let pid = pl.id.clone();
    let pid_play = pl.id.clone();
    let tracks = pl.tracks.clone();
    let count = if pl.tracks.is_empty() {
        pl.track_count
    } else {
        pl.tracks.len()
    };
    let count_str = count.to_string();
    let sub = if !pl.description.is_empty() {
        pl.description.clone()
    } else {
        store
            .t_fmt("track.count_fmt", &[("count", &count_str)])
            .to_string()
    };

    media_card(pl.name.clone(), sub)
        .image_url(pl.image_url.clone())
        .seed(pl.id.clone())
        .badge(store.t_fmt("track.count_fmt", &[("count", &count_str)]))
        .on_press(move || {
            store.navigate(Route::PlaylistDetail { id: pid.clone() });
        })
        .on_play(move || {
            if !tracks.is_empty() {
                store.play_list(tracks.clone(), 0);
            } else {
                let src = store.active_source();
                let id = pid_play.clone();
                spawn(async move {
                    if let Ok(Ok(full)) = blocking(move || src.playlist(&id)).await
                        && !full.tracks.is_empty()
                    {
                        store.play_list(full.tracks, 0);
                    }
                });
            }
        })
        .into_element()
}

fn library_stat_tile(
    glyph: Icon,
    tint: AdaptiveColor,
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    button()
        .on_press(on_press)
        .w_full()
        .min_h(px(74.0))
        .p_3p5()
        .flex_row()
        .items_center()
        .gap_3()
        .rounded(px(radius::CARD))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border)
        .elevation(Elevation::Subtle)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::accent))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .size(px(40.0))
                .flex_shrink_0()
                .rounded(px(radius::CONTROL))
                .bg(theme::secondary)
                .items_center()
                .justify_center()
                .child(icon(glyph, 20.0, tint)),
        )
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_0p5()
                .child(
                    text(title)
                        .number_of_lines(1)
                        .text_size(px(font_size::BODY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::fg),
                )
                .child(
                    text(subtitle)
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg),
                ),
        )
}
