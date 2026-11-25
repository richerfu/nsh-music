//! Artist detail page with circular artist hero + ambient wash, capability-gated
//! Follow toggle (`store.toggle_follow_artist`), shimmer loading, Top Tracks
//! list, and Discography albums grid (no synthetic bio fallback).

use std::rc::Rc;

use nsh_core::prelude::*;

use crate::pages::{
    card_grid, detail_hero, page_scroll, render_track_row, shimmer_card_grid,
    shimmer_detail_hero, shimmer_track_list, shuffle_and_play, track_table_header,
};
use crate::state::{AppStore, Route};
use crate::ui::{
    DeviceForm, ghost_button, media_card, primary_button, section_header, theme,
};
use crate::ui::theme::radius;

pub fn artist_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let body = view().w_full().child(dynamic(move || {
        let active = store.active_artist.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|a| a.artist.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form))
                    .child(shimmer_track_list(5))
                    .child(shimmer_card_grid(store, form, 4, false))
                    .into_element();
            };

            let artist = &page.artist;
            let track_count = page.tracks.len().to_string();
            let album_count = page.albums.len().to_string();
            let meta: SharedString = format!(
                "{} · {}",
                store.t_fmt("track.count_fmt", &[("count", &track_count)]),
                store.t_fmt("library.albums_count", &[("count", &album_count)]),
            )
            .into();

            let tracks_play = page.tracks.clone();
            let tracks_shuffle = page.tracks.clone();
            let following_now = artist.following;
            let artist_id_follow = artist.id.clone();
            let can_follow = store.active_can("followArtist");

            let actions = view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(
                    primary_button(move || store.t("player.play_all"), move || {
                        if !tracks_play.is_empty() {
                            store.play_list(tracks_play.clone(), 0);
                        }
                    })
                    .disabled(page.tracks.is_empty()),
                )
                .child(
                    ghost_button(move || store.t("player.shuffle_all"), move || {
                        shuffle_and_play(store, tracks_shuffle.clone());
                    })
                    .disabled(page.tracks.is_empty()),
                )
                .children(can_follow.then(move || {
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
                }));

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

            let mut stack = view().w_full().gap_6().child(hero);

            if !page.tracks.is_empty() {
                let queue = Rc::new(page.tracks.clone());
                let mut rows = view().w_full().gap_1();
                if !form.is_phone() {
                    rows = rows.child(track_table_header(store, form, true));
                }
                for (idx, tr) in page.tracks.iter().enumerate() {
                    rows = rows.child(render_track_row(
                        store,
                        tr.clone(),
                        Some(idx),
                        Rc::clone(&queue),
                        idx,
                        true,
                    ));
                }
                stack = stack.child(
                    view()
                        .w_full()
                        .gap_3()
                        .child(section_header(
                            move || store.t("browse.charts"),
                            None,
                            None,
                            None,
                        ))
                        .child(
                            view()
                                .w_full()
                                .p_2()
                                .rounded(px(radius::CARD))
                                .bg(theme::card)
                                .border_1()
                                .border_color(theme::border)
                                .child(rows),
                        ),
                );
            }

            if !page.albums.is_empty() {
                let cards: Vec<AnyElement> = page
                    .albums
                    .iter()
                    .map(|alb| {
                        let aid = alb.id.clone();
                        let aid_play = alb.id.clone();
                        let mut card = media_card(alb.name.clone(), alb.released.clone())
                            .image_url(alb.image_url.clone())
                            .seed(alb.id.clone())
                            .on_press(move || {
                                store.navigate(Route::AlbumDetail { id: aid.clone() });
                            })
                            .on_play(move || {
                                let src = store.active_source();
                                let id = aid_play.clone();
                                spawn(async move {
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

                stack = stack.child(
                    view()
                        .w_full()
                        .gap_3()
                        .child(section_header(
                            move || store.t("nav.albums"),
                            None,
                            None,
                            None,
                        ))
                        .child(card_grid(store, form, cards)),
                );
            }

            stack.into_element()
        })
    }));

    page_scroll(form, body)
}
