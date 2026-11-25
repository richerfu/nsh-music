//! Artist detail page with circular `600px` artist hero + ambient wash,
//! capability-gated Follow toggle (`store.toggle_follow_artist`), `Songs` /
//! `Albums` switcher matching `mygo-music`, virtualized top tracks, and
//! `240px` discography albums grid (no synthetic bio fallback).

use std::rc::Rc;

use nsh_base::{tab, tabs};
use nsh_core::prelude::*;

use crate::pages::{
    ARTWORK_SIZE_CARD, PagesOwner, card_grid, detail_hero, hero_play_shuffle_buttons, page_scroll,
    shimmer_card_grid, shimmer_detail_hero, shimmer_track_list, shuffle_and_play,
    sized_artwork_url, virtual_track_list,
};
use crate::state::{AppStore, Route};
use crate::ui::theme::{font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, empty_state, ghost_button, media_card, section_header,
    theme,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtistTab {
    Songs,
    Albums,
}

pub fn artist_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let active_tab = signal(ArtistTab::Songs);
    let tasks = PagesOwner::capture();

    let body = view().w_full().child(dynamic(move || {
        let active = store.active_artist.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|a| a.artist.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form, true))
                    .child(shimmer_track_list(6))
                    .child(shimmer_card_grid(store, form, 4, false))
                    .into_element();
            };

            let artist = &page.artist;
            let track_count = page.tracks.len().to_string();
            let album_count = page.albums.len().to_string();
            let mut meta_parts = Vec::new();
            if !page.tracks.is_empty() {
                meta_parts.push(
                    store
                        .t_fmt("track.count_fmt", &[("count", &track_count)])
                        .to_string(),
                );
            }
            if !page.albums.is_empty() {
                meta_parts.push(
                    store
                        .t_fmt("library.albums_count", &[("count", &album_count)])
                        .to_string(),
                );
            }
            let meta: SharedString = meta_parts.join(" · ").into();

            let tracks_play = page.tracks.clone();
            let tracks_shuffle = page.tracks.clone();
            let following_now = artist.following;
            let artist_id_follow = artist.id.clone();
            let can_follow = store.active_can("followArtist");

            let primary_actions = hero_play_shuffle_buttons(
                store,
                form,
                page.tracks.is_empty(),
                move || {
                    if !tracks_play.is_empty() {
                        store.play_list(tracks_play.clone(), 0);
                    }
                },
                Some(Rc::new(move || {
                    shuffle_and_play(store, tracks_shuffle.clone());
                })),
            );
            let is_phone = form.is_phone();
            let min_h = px(form.min_touch_target());
            let follow_btn = can_follow.then(move || {
                ghost_button(
                    move || {
                        if following_now {
                            store.t("library.following_artist")
                        } else {
                            store.t("library.follow_artist")
                        }
                    },
                    move || {
                        store.toggle_follow_artist(&artist_id_follow, !following_now);
                    },
                )
                .min_h(min_h)
                .when(is_phone, |b| b.rounded_full())
            });

            let actions = if form.is_phone() {
                view()
                    .w_full()
                    .gap_2p5()
                    .child(primary_actions)
                    .children(follow_btn.map(|btn| {
                        view()
                            .w_full()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .child(btn)
                    }))
            } else {
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_2p5()
                    .child(primary_actions)
                    .children(follow_btn)
            };

            let hero = detail_hero(
                store,
                form,
                store.t("nav.artists"),
                artist.name.clone().into(),
                SharedString::default(),
                meta,
                SharedString::default(),
                artist.image_url.clone().into(),
                artist.id.clone().into(),
                true,
                actions,
            );

            let has_tracks = !page.tracks.is_empty();
            let has_albums = !page.albums.is_empty();
            if !has_tracks && has_albums {
                active_tab.set(ArtistTab::Albums);
            } else if has_tracks && !has_albums {
                active_tab.set(ArtistTab::Songs);
            }

            let tab_switcher = (has_tracks && has_albums).then(|| {
                tabs()
                    .flex_row()
                    .when(is_phone, |t| t.w_full())
                    .when(!is_phone, |t| t.self_start())
                    .gap_1()
                    .p_1()
                    .rounded(px(if is_phone {
                        radius::PILL
                    } else {
                        radius::CONTROL
                    }))
                    .bg(theme::muted)
                    .indicator_style(move |s| {
                        s.top_1()
                            .bottom_1()
                            .rounded(px(if is_phone { radius::PILL } else { radius::SM }))
                            .bg(theme::card_elevated)
                            .elevation(Elevation::Subtle)
                    })
                    .child(artist_tab_item(
                        store,
                        form,
                        active_tab,
                        ArtistTab::Songs,
                        "search.tab.tracks",
                    ))
                    .child(artist_tab_item(
                        store,
                        form,
                        active_tab,
                        ArtistTab::Albums,
                        "nav.albums",
                    ))
            });

            let songs_queue = Rc::new(page.tracks.clone());
            let albums_data = page.albums.clone();

            let section_bar = if is_phone {
                view().w_full().gap_2p5().children(tab_switcher).children(
                    (!(has_tracks && has_albums)).then(|| {
                        section_header(
                            move || match active_tab.get() {
                                ArtistTab::Songs => store.t("search.tab.tracks"),
                                ArtistTab::Albums => store.t("nav.albums"),
                            },
                            None,
                            None,
                            None,
                        )
                    }),
                )
            } else {
                view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap_3()
                    .child(view().flex_1().min_w(px(120.0)).child(section_header(
                        move || match active_tab.get() {
                            ArtistTab::Songs => store.t("search.tab.tracks"),
                            ArtistTab::Albums => store.t("nav.albums"),
                        },
                        None,
                        None,
                        None,
                    )))
                    .children(tab_switcher)
            };

            let section_body = view().w_full().child(dynamic(move || {
                let current_tab = active_tab.get();
                let songs_queue = Rc::clone(&songs_queue);
                let albums_data = albums_data.clone();
                untracked(move || match current_tab {
                    ArtistTab::Songs => {
                        if songs_queue.is_empty() {
                            empty_state(
                                Icon::Disc,
                                store.t("common.empty"),
                                store.t("search.tab.tracks"),
                            )
                            .into_element()
                        } else {
                            virtual_track_list(store, form, songs_queue, true, true, None, None)
                                .into_element()
                        }
                    }
                    ArtistTab::Albums => {
                        if albums_data.is_empty() {
                            empty_state(Icon::Disc, store.t("common.empty"), store.t("nav.albums"))
                                .into_element()
                        } else {
                            let cards: Vec<AnyElement> = albums_data
                                .iter()
                                .map(|alb| {
                                    let aid = alb.id.clone();
                                    let aid_play = alb.id.clone();
                                    let mut card =
                                        media_card(alb.name.clone(), alb.released.clone())
                                            .image_url(sized_artwork_url(
                                                &alb.image_url,
                                                ARTWORK_SIZE_CARD,
                                            ))
                                            .seed(alb.id.clone())
                                            .on_press(move || {
                                                store.navigate(Route::AlbumDetail {
                                                    id: aid.clone(),
                                                });
                                            })
                                            .on_play(move || {
                                                let src = store.active_source();
                                                let id = aid_play.clone();
                                                tasks.spawn(async move {
                                                    if let Ok(Ok(alb_page)) =
                                                        blocking(move || src.album(&id)).await
                                                        && !alb_page.tracks.is_empty()
                                                    {
                                                        store.play_list(alb_page.tracks, 0);
                                                    }
                                                });
                                            });
                                    if !alb.released.is_empty() {
                                        card = card.badge(alb.released.clone());
                                    }
                                    card.into_element()
                                })
                                .collect();
                            card_grid(store, form, cards).into_element()
                        }
                    }
                })
            }));

            view()
                .w_full()
                .gap_6()
                .child(hero)
                .child(
                    view()
                        .w_full()
                        .gap_3()
                        .child(section_bar)
                        .child(section_body),
                )
                .into_element()
        })
    }));

    page_scroll(form, body)
}

fn artist_tab_item(
    store: AppStore,
    form: DeviceForm,
    active_tab: Signal<ArtistTab>,
    kind: ArtistTab,
    label_key: &'static str,
) -> impl IntoElement {
    let is_sel = move || active_tab.get() == kind;
    tab()
        .selected(is_sel)
        .on_press(move |_: &()| active_tab.set(kind))
        .when(form.is_phone(), |t| t.flex_1())
        .min_h(px(if form.is_touch() { 44.0 } else { 32.0 }))
        .px_3p5()
        .py_1p5()
        .items_center()
        .justify_center()
        .child(
            text(move || store.t(label_key))
                .number_of_lines(1)
                .text_size(px(font_size::SUBTITLE))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || {
                    if is_sel() {
                        theme::fg()
                    } else {
                        theme::muted_fg()
                    }
                }),
        )
}
