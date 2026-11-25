//! Album detail page with artwork + ambient wash hero, capability-gated
//! Save Album (`store.toggle_save_album`), artist jump link, shimmer loading,
//! and album track list.

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;

use crate::pages::{
    detail_hero, page_scroll, render_track_row, shimmer_detail_hero, shimmer_track_list,
    shuffle_and_play, track_table_header,
};
use crate::state::{AppStore, Route};
use crate::ui::{
    DeviceForm, Icon, empty_state, format_duration_ms, ghost_button, primary_button, theme,
};
use crate::ui::theme::{font_size, radius};

pub fn album_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let body = view().w_full().child(dynamic(move || {
        let active = store.active_album.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|a| a.album.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form))
                    .child(shimmer_track_list(6))
                    .into_element();
            };

            let album = &page.album;
            let artist_names: String = album
                .artists
                .iter()
                .map(|a| a.name.as_str())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            let subtitle: SharedString = if artist_names.is_empty() {
                store.t("track.unknown_artist")
            } else {
                artist_names.into()
            };

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
            let can_artist = store.active_can("artist");
            let primary_artist = album.artists.first().cloned();

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
                }))
                .child(
                    ghost_button(move || store.t("player.add_to_queue"), move || {
                        for tr in &tracks_queue {
                            store.enqueue(tr.clone());
                        }
                    })
                    .disabled(page.tracks.is_empty()),
                )
                .children(
                    primary_artist
                        .filter(|a| can_artist && !a.id.is_empty())
                        .map(|art| {
                            let aid = art.id.clone();
                            button()
                                .on_press(move || {
                                    store.navigate(Route::ArtistDetail { id: aid.clone() });
                                })
                                .min_h(px(form.min_touch_target()))
                                .px_3()
                                .py_1()
                                .rounded(px(radius::CONTROL))
                                .hover(|s| s.bg(theme::hover))
                                .active(|s| s.opacity(0.75))
                                .child(
                                    text(art.name)
                                        .text_size(px(font_size::BODY))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::primary),
                                )
                        }),
                );

            let hero = detail_hero(
                store,
                form,
                store.t("nav.albums"),
                album.name.clone().into(),
                subtitle,
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
                let mut rows = view().w_full().gap_1();
                if !form.is_phone() {
                    rows = rows.child(track_table_header(store, form, false));
                }
                for (idx, tr) in page.tracks.iter().enumerate() {
                    rows = rows.child(render_track_row(
                        store,
                        tr.clone(),
                        Some(idx),
                        Rc::clone(&queue),
                        idx,
                        false,
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
