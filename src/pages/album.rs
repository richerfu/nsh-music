//! Album detail page with `600px` artwork + ambient wash hero, clickable artist
//! credits navigating to `Route::ArtistDetail`, capability-gated Save Album
//! (`store.toggle_save_album`), shimmer loading, and virtualized album tracks.

use std::rc::Rc;

use nsh_core::prelude::*;

use crate::pages::{
    artist_links_row, detail_hero_custom_subtitle, hero_play_shuffle_buttons, page_scroll,
    shimmer_detail_hero, shimmer_track_list, shuffle_and_play, virtual_track_list,
};
use crate::state::AppStore;
use crate::ui::theme::font_size;
use crate::ui::{DeviceForm, Icon, empty_state, format_duration_ms, ghost_button};

pub fn album_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let body = view().w_full().child(dynamic(move || {
        let active = store.active_album.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|a| a.album.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form, false))
                    .child(shimmer_track_list(8))
                    .into_element();
            };

            let album = &page.album;
            let subtitle_view = Some(artist_links_row(
                store,
                &album.artists,
                "",
                store.t("track.unknown_artist"),
                font_size::BODY,
                form,
            ));

            let total_ms: i64 = page.tracks.iter().map(|t| t.duration.max(0)).sum();
            let count_str = page.tracks.len().to_string();
            let mut meta_parts = Vec::new();
            if !album.released.is_empty() {
                meta_parts.push(album.released.clone());
            }
            meta_parts.push(
                store
                    .t_fmt("track.count_fmt", &[("count", &count_str)])
                    .to_string(),
            );
            if total_ms > 0 {
                meta_parts.push(format_duration_ms(total_ms));
            }
            let meta: SharedString = meta_parts.join(" · ").into();

            let tracks_play = page.tracks.clone();
            let tracks_shuffle = page.tracks.clone();
            let tracks_queue = page.tracks.clone();
            let saved_now = album.saved;
            let album_id_save = album.id.clone();
            let can_save = store.active_can("saveAlbum");

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
            let secondary = view()
                .flex_row()
                .items_center()
                .when(is_phone, |v| v.w_full().justify_center())
                .flex_wrap()
                .gap_2()
                .children(can_save.then(move || {
                    ghost_button(
                        move || {
                            if saved_now {
                                store.t("library.unsave_album")
                            } else {
                                store.t("library.save_album")
                            }
                        },
                        move || {
                            store.toggle_save_album(&album_id_save, !saved_now);
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                }))
                .child(
                    ghost_button(
                        move || store.t("player.add_to_queue"),
                        move || {
                            for tr in &tracks_queue {
                                store.enqueue(tr.clone());
                            }
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                    .disabled(page.tracks.is_empty()),
                );

            let actions = if form.is_phone() {
                view()
                    .w_full()
                    .gap_2p5()
                    .child(primary_actions)
                    .child(secondary)
            } else {
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_2p5()
                    .child(primary_actions)
                    .child(secondary)
            };

            let hero = detail_hero_custom_subtitle(
                store,
                form,
                store.t("nav.albums"),
                album.name.clone().into(),
                subtitle_view,
                meta,
                SharedString::default(),
                album.image_url.clone().into(),
                album.id.clone().into(),
                false,
                actions,
            );

            let track_list = if page.tracks.is_empty() {
                empty_state(Icon::Disc, store.t("common.empty"), store.t("nav.albums"))
            } else {
                let queue = Rc::new(page.tracks.clone());
                virtual_track_list(store, form, queue, false, false, None, None)
            };

            view()
                .w_full()
                .gap_6()
                .child(hero)
                .child(track_list)
                .into_element()
        })
    }));

    page_scroll(form, body)
}
