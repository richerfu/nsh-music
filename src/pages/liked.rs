//! Library Hub, Liked Songs, User Playlists management, and Recently Played pages
//! aligned with `mygo-music`: zero mock strings, virtualized track lists
//! (`ListViewport::Parent`), `240px` expanded card artwork, reactive capability
//! gating, and shimmer loading skeletons.

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::Playlist;

use crate::pages::{
    ARTWORK_SIZE_CARD, PagesOwner, card_grid, page_scroll, shimmer_card_grid, shimmer_track_list,
    shuffle_and_play, sized_artwork_url, virtual_track_list,
};
use crate::state::{ActiveModal, AppStore, Route};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, empty_state, empty_state_with_actions, ghost_button,
    icon, media_card, motion, primary_button, section_header, theme,
};

/// Unified Library hub (`Route::Library`):
/// - **Phone**: native grouped inset rows (`>= 54pt`, tinted icon badge, title,
///   count, `ChevronRight`) for Liked, Playlists, Recent, Local, Saved Albums.
/// - **Desktop / Pad**: responsive quick stat tiles + Playlists + Saved Albums
///   + Liked Songs preview.
pub fn library_page(store: AppStore, form: DeviceForm) -> View {
    if form.is_phone() {
        return phone_library_page(store, form);
    }

    let tasks = PagesOwner::capture();
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
                            .image_url(sized_artwork_url(&alb.image_url, ARTWORK_SIZE_CARD))
                            .seed(alb.id.clone())
                            .on_press(move || {
                                store.navigate(Route::AlbumDetail { id: aid.clone() });
                            })
                            .on_play(move || {
                                let src = store.active_source();
                                let id = aid_play.clone();
                                tasks.spawn(async move {
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

    let is_phone = form.is_phone();
    let hero = Elevated::elevation(
        view()
            .w_full()
            .p(px(if is_phone { 16.0 } else { 24.0 }))
            .gap(px(if is_phone { 14.0 } else { 16.0 }))
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
            .gap_3p5()
            .child(
                Elevated::elevation(
                    view()
                        .size(px(if is_phone { 60.0 } else { 88.0 }))
                        .flex_shrink_0()
                        .rounded(px(radius::LG))
                        .bg(theme::primary),
                    if is_phone {
                        Elevation::Subtle
                    } else {
                        Elevation::Card
                    },
                )
                .items_center()
                .justify_center()
                .child(icon(
                    Icon::HeartFilled,
                    if is_phone { 26.0 } else { 38.0 },
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
                            .text_size(px(if is_phone {
                                font_size::TITLE
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
    .child(if is_phone {
        view()
            .w_full()
            .gap_2()
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .gap_2p5()
                    .child(
                        button()
                            .on_press(play_all)
                            .disabled(move || store.liked_tracks.with(Vec::is_empty))
                            .flex_1()
                            .h(px(44.0))
                            .px_4()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .rounded_full()
                            .bg(theme::primary)
                            .hover(|s| s.bg(theme::primary_hover))
                            .active(|s| s.scale(motion::press_scale))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .disabled_style(|s| s.opacity(0.45))
                            .child(icon(Icon::Play, 15.0, colors::PRIMARY_FOREGROUND))
                            .child(
                                text(move || store.t("player.play_all"))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::BODY))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::primary_fg),
                            ),
                    )
                    .child(
                        button()
                            .on_press(shuffle_all)
                            .disabled(move || store.liked_tracks.with(Vec::is_empty))
                            .flex_1()
                            .h(px(44.0))
                            .px_4()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .rounded_full()
                            .bg(theme::secondary)
                            .border_1()
                            .border_color(theme::border)
                            .hover(|s| s.bg(theme::accent))
                            .active(|s| s.scale(motion::press_scale))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .disabled_style(|s| s.opacity(0.45))
                            .child(icon(Icon::Shuffle, 15.0, colors::FOREGROUND))
                            .child(
                                text(move || store.t("player.shuffle_all"))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::BODY))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::fg),
                            ),
                    ),
            )
            .child(when(
                move || store.active_can("createPlaylist"),
                move || {
                    view().w_full().flex_row().justify_center().child(
                        ghost_button(
                            move || store.t("library.create_playlist"),
                            move || {
                                store.open_modal(ActiveModal::CreatePlaylist {
                                    initial_track_id: None,
                                });
                            },
                        )
                        .min_h(px(44.0))
                        .rounded_full(),
                    )
                },
            ))
    } else {
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
            ))
    });

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
    let create_btn = move || {
        primary_button(
            move || store.t("library.create_playlist"),
            move || {
                store.open_modal(ActiveModal::CreatePlaylist {
                    initial_track_id: None,
                });
            },
        )
        .min_h(px(form.min_touch_target()))
    };

    let body = if form.is_phone() {
        view()
            .w_full()
            .child(when(
                move || store.active_can("createPlaylist"),
                move || {
                    view()
                        .w_full()
                        .pb_4()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .child(create_btn().rounded_full())
                },
            ))
            .child(playlists_grid_dynamic(store, form))
    } else {
        let top_row = view()
            .w_full()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_wrap()
            .gap_3()
            .child(
                text(move || store.t("nav.playlists"))
                    .role(AccessibilityRole::Header)
                    .text_size(px(font_size::DISPLAY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::fg),
            )
            .child(when(move || store.active_can("createPlaylist"), create_btn));

        view()
            .w_full()
            .gap_5()
            .child(top_row)
            .child(playlists_grid_dynamic(store, form))
    };

    page_scroll(form, body)
}

/// Recently Played tracks page (`Route::Recent`) with virtualized track table.
pub fn recent_page(store: AppStore, form: DeviceForm) -> View {
    let top_row = if form.is_phone() {
        view()
            .w_full()
            .flex_row()
            .items_center()
            .gap_2p5()
            .child(
                button()
                    .on_press(move || {
                        let tracks = store.recent_tracks.get_untracked();
                        if !tracks.is_empty() {
                            store.play_list(tracks, 0);
                        }
                    })
                    .disabled(move || store.recent_tracks.with(Vec::is_empty))
                    .flex_1()
                    .h(px(44.0))
                    .px_4()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded_full()
                    .bg(theme::primary)
                    .hover(|s| s.bg(theme::primary_hover))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .disabled_style(|s| s.opacity(0.45))
                    .child(icon(Icon::Play, 15.0, colors::PRIMARY_FOREGROUND))
                    .child(
                        text(move || store.t("player.play_all"))
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::primary_fg),
                    ),
            )
            .child(
                button()
                    .on_press(move || {
                        let tracks = store.recent_tracks.get_untracked();
                        shuffle_and_play(store, tracks);
                    })
                    .disabled(move || store.recent_tracks.with(Vec::is_empty))
                    .flex_1()
                    .h(px(44.0))
                    .px_4()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded_full()
                    .bg(theme::secondary)
                    .border_1()
                    .border_color(theme::border)
                    .hover(|s| s.bg(theme::accent))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .disabled_style(|s| s.opacity(0.45))
                    .child(icon(Icon::Shuffle, 15.0, colors::FOREGROUND))
                    .child(
                        text(move || store.t("player.shuffle_all"))
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::fg),
                    ),
            )
    } else {
        let action_btns = view()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                primary_button(
                    move || store.t("player.play_all"),
                    move || {
                        let tracks = store.recent_tracks.get_untracked();
                        if !tracks.is_empty() {
                            store.play_list(tracks, 0);
                        }
                    },
                )
                .disabled(move || store.recent_tracks.with(Vec::is_empty)),
            )
            .child(
                ghost_button(
                    move || store.t("player.shuffle_all"),
                    move || {
                        let tracks = store.recent_tracks.get_untracked();
                        shuffle_and_play(store, tracks);
                    },
                )
                .disabled(move || store.recent_tracks.with(Vec::is_empty)),
            );

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
                    .text_size(px(font_size::DISPLAY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::fg),
            )
            .child(action_btns)
    };

    let body = view()
        .w_full()
        .gap_5()
        .child(top_row)
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let tracks = store.recent_tracks.get();
            untracked(move || {
                if loading && tracks.is_empty() {
                    return shimmer_track_list(6).into_element();
                }
                if tracks.is_empty() {
                    return empty_state(Icon::Disc, store.t("common.empty"), store.t("nav.recent"))
                        .into_element();
                }
                let queue = Rc::new(tracks);
                virtual_track_list(store, form, queue, true, true, None, None).into_element()
            })
        }));

    page_scroll(form, body)
}

fn liked_tracks_block(store: AppStore, form: DeviceForm, preview_only: bool) -> View {
    view()
        .w_full()
        .gap_3()
        .children(preview_only.then(|| {
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
                    return shimmer_track_list(6).into_element();
                }
                if tracks.is_empty() {
                    return empty_state(
                        Icon::Heart,
                        store.t("common.empty"),
                        store.t("player.like"),
                    )
                    .into_element();
                }

                let shown = if preview_only {
                    tracks.into_iter().take(8).collect()
                } else {
                    tracks
                };
                let queue = Rc::new(shown);
                virtual_track_list(store, form, queue, true, true, None, None).into_element()
            })
        }))
}

fn playlists_section(store: AppStore, form: DeviceForm) -> View {
    let header = view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(view().flex_1().min_w(px(0.0)).child(section_header(
            move || store.t("nav.playlists"),
            None,
            None,
            None,
        )))
        .child(when(
            move || store.active_can("createPlaylist") && store.playlists.with(|p| !p.is_empty()),
            move || {
                ghost_button(
                    move || store.t("library.create_playlist"),
                    move || {
                        store.open_modal(ActiveModal::CreatePlaylist {
                            initial_track_id: None,
                        });
                    },
                )
                .min_h(px(form.min_touch_target()))
            },
        ));

    view()
        .w_full()
        .gap_3()
        .child(header)
        .child(playlists_grid_dynamic(store, form))
}

fn playlists_grid_dynamic(store: AppStore, form: DeviceForm) -> View {
    let tasks = PagesOwner::capture();

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
                .map(|pl| playlist_card(store, tasks, pl))
                .collect();
            card_grid(store, form, cards).into_element()
        })
    }))
}

fn playlist_card(store: AppStore, tasks: PagesOwner, pl: &Playlist) -> AnyElement {
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
        .image_url(sized_artwork_url(&pl.image_url, ARTWORK_SIZE_CARD))
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
                tasks.spawn(async move {
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

fn phone_library_page(store: AppStore, form: DeviceForm) -> View {
    let tasks = PagesOwner::capture();
    let hairline = || {
        view()
            .w_full()
            .h(px(1.0))
            .pl(px(62.0))
            .child(view().w_full().h_full().bg(theme::border))
    };

    let primary_group = Elevated::elevation(
        view()
            .w_full()
            .rounded(px(radius::XL))
            .overflow_hidden()
            .bg(theme::card)
            .border_1()
            .border_color(theme::border)
            .child(phone_library_row(
                Icon::HeartFilled,
                colors::PRIMARY,
                theme::primary_soft,
                move || store.t("nav.liked"),
                move || {
                    let n = store.liked_tracks.with(Vec::len).to_string();
                    store.t_fmt("track.count_fmt", &[("count", &n)])
                },
                move || store.navigate(Route::Liked),
            ))
            .child(hairline())
            .child(phone_library_row(
                Icon::Library,
                colors::AMBER,
                theme::amber_soft,
                move || store.t("nav.playlists"),
                move || SharedString::from(store.playlists.with(Vec::len).to_string()),
                move || store.navigate(Route::Playlists),
            ))
            .child(hairline())
            .child(phone_library_row(
                Icon::Disc,
                colors::OK,
                theme::ok_soft,
                move || store.t("nav.recent"),
                move || {
                    let n = store.recent_tracks.with(Vec::len).to_string();
                    store.t_fmt("track.count_fmt", &[("count", &n)])
                },
                move || store.navigate(Route::Recent),
            ))
            .child(hairline())
            .child(phone_library_row(
                Icon::Folder,
                colors::PRIMARY,
                theme::primary_soft,
                move || store.t("nav.local"),
                move || {
                    let _ = store.home_data.get();
                    let n = store.local_source().local_track_count().to_string();
                    store.t_fmt("track.count_fmt", &[("count", &n)])
                },
                move || store.navigate(Route::Local),
            )),
        Elevation::Subtle,
    );

    let saved_albums_block = view().w_full().child(dynamic(move || {
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
                        .image_url(sized_artwork_url(&alb.image_url, ARTWORK_SIZE_CARD))
                        .seed(alb.id.clone())
                        .on_press(move || {
                            store.navigate(Route::AlbumDetail { id: aid.clone() });
                        })
                        .on_play(move || {
                            let src = store.active_source();
                            let id = aid_play.clone();
                            tasks.spawn(async move {
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
    }));

    let body = view()
        .w_full()
        .gap_5()
        .child(primary_group)
        .child(playlists_section(store, form))
        .child(saved_albums_block);

    page_scroll(form, body)
}

fn phone_library_row(
    glyph: Icon,
    tint: AdaptiveColor,
    badge_bg: impl IntoReactive<AdaptiveColor>,
    title: impl IntoReactive<SharedString>,
    trailing_value: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    button()
        .on_press(on_press)
        .w_full()
        .min_h(px(56.0))
        .px_4()
        .py_2p5()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .bg(colors::TRANSPARENT)
        .transition_colors()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    view()
                        .size(px(34.0))
                        .flex_shrink_0()
                        .rounded(px(radius::CONTROL))
                        .bg(badge_bg)
                        .items_center()
                        .justify_center()
                        .child(icon(glyph, 17.0, tint)),
                )
                .child(
                    text(title)
                        .flex_1()
                        .min_w(px(0.0))
                        .number_of_lines(1)
                        .text_size(px(font_size::BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::fg),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1p5()
                .flex_shrink_0()
                .child(
                    text(trailing_value)
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg),
                )
                .child(icon(Icon::ChevronRight, 16.0, colors::MUTED_FOREGROUND)),
        )
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
