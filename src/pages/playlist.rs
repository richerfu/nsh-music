//! Playlist detail page with `600px` artwork + ambient wash hero, Play All /
//! Shuffle / Save / Queue / Rename / Delete (with native destructive confirm),
//! editable playlist Move Up / Move Down / Remove actions, virtualized track
//! list (`ListViewport::Parent`), and loading shimmer.

use std::rc::Rc;

use nsh_core::prelude::*;

use crate::pages::{
    PagesOwner, detail_hero, hero_play_shuffle_buttons, page_scroll, shimmer_detail_hero,
    shimmer_track_list, shuffle_and_play, virtual_track_list,
};
use crate::state::{ActiveModal, AppStore};
use crate::ui::{
    DeviceForm, Icon, confirm_destructive, empty_state, format_duration_ms, ghost_button,
};

pub fn playlist_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let edit_order = signal(false);
    let tasks = PagesOwner::capture();

    let body = view().w_full().child(dynamic(move || {
        let active = store.active_playlist.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(pl) = active.filter(|p| p.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form, false))
                    .child(shimmer_track_list(8))
                    .into_element();
            };

            let total_ms: i64 = pl.tracks.iter().map(|t| t.duration.max(0)).sum();
            let count = if pl.tracks.is_empty() {
                pl.track_count
            } else {
                pl.tracks.len()
            };
            let count_str = count.to_string();
            let mut meta_parts = vec![
                store
                    .t_fmt("track.count_fmt", &[("count", &count_str)])
                    .to_string(),
            ];
            if total_ms > 0 {
                meta_parts.push(format_duration_ms(total_ms));
            }
            if !pl.owner.is_empty() {
                meta_parts.push(pl.owner.clone());
            }
            let meta: SharedString = meta_parts.join(" · ").into();

            let tracks_for_play = pl.tracks.clone();
            let tracks_for_shuffle = pl.tracks.clone();
            let tracks_for_queue = pl.tracks.clone();
            let pl_id_save = pl.id.clone();
            let saved_now = pl.saved
                || store
                    .playlists
                    .with_untracked(|list| list.iter().any(|p| !p.mine && p.id == pl.id));
            let pl_for_save = pl.clone();

            let is_editable = pl.mine && !pl.fixed;
            let can_save = !pl.mine && store.active_can("savePlaylist");
            let can_rename = is_editable && store.active_can("renamePlaylist");
            let can_delete = is_editable && store.active_can("deletePlaylist");
            let can_move = is_editable && store.active_can("movePlaylistTrack");
            let can_remove = is_editable && store.active_can("removeFromPlaylist");

            let pl_id_rename = pl.id.clone();
            let pl_name_rename = pl.name.clone();
            let pl_id_del = pl.id.clone();
            let pl_name_del = pl.name.clone();

            let primary_actions = hero_play_shuffle_buttons(
                store,
                form,
                pl.tracks.is_empty(),
                move || {
                    if !tracks_for_play.is_empty() {
                        store.play_list(tracks_for_play.clone(), 0);
                    }
                },
                Some(Rc::new(move || {
                    shuffle_and_play(store, tracks_for_shuffle.clone());
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
                                store.t("library.unsave_playlist")
                            } else {
                                store.t("library.save_playlist")
                            }
                        },
                        move || {
                            let next = !saved_now;
                            let mut updated = pl_for_save.clone();
                            updated.saved = next;
                            store.active_playlist.set(Some(updated.clone()));
                            store.playlists.update(|list| {
                                list.retain(|p| p.id != updated.id);
                                if next {
                                    list.push(updated.clone());
                                }
                            });
                            let src = store.active_source();
                            let pid = pl_id_save.clone();
                            tasks.spawn(async move {
                                let res = blocking(move || src.save_playlist(&pid, next)).await;
                                if !matches!(res, Ok(Ok(()))) {
                                    store.toasts.error(store.t("common.error"));
                                }
                            });
                            store.toasts.info(if next {
                                store.t("library.unsave_playlist")
                            } else {
                                store.t("library.save_playlist")
                            });
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                }))
                .child(
                    ghost_button(
                        move || store.t("player.add_to_queue"),
                        move || {
                            for tr in &tracks_for_queue {
                                store.enqueue(tr.clone());
                            }
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                    .disabled(pl.tracks.is_empty()),
                )
                .children(
                    (is_editable && (can_move || can_remove) && !pl.tracks.is_empty()).then(|| {
                        ghost_button(
                            move || {
                                if edit_order.get() {
                                    store.t("common.done")
                                } else {
                                    store.t("common.edit")
                                }
                            },
                            move || edit_order.update(|e| *e = !*e),
                        )
                        .min_h(min_h)
                        .when(is_phone, |b| b.rounded_full())
                    }),
                )
                .children(can_rename.then(move || {
                    ghost_button(
                        move || store.t("playlist.rename"),
                        move || {
                            store.open_modal(ActiveModal::RenamePlaylist {
                                id: pl_id_rename.clone(),
                                current_name: pl_name_rename.clone(),
                            });
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                }))
                .children(can_delete.then(move || {
                    ghost_button(
                        move || store.t("playlist.delete"),
                        move || {
                            let pid = pl_id_del.clone();
                            let title = store
                                .t_fmt("playlist.delete_confirm_title", &[("name", &pl_name_del)]);
                            let desc = store.t("playlist.delete_confirm_desc");
                            let cancel = store.t("common.cancel");
                            let confirm = store.t("common.delete");
                            confirm_destructive(title, desc, cancel, confirm, move || {
                                store.delete_playlist(&pid);
                            });
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.rounded_full())
                }));

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

            let hero = detail_hero(
                store,
                form,
                store.t("nav.playlists"),
                pl.name.clone().into(),
                SharedString::default(),
                meta,
                pl.description.clone().into(),
                pl.image_url.clone().into(),
                pl.id.clone().into(),
                false,
                actions,
            );

            let track_list = if pl.tracks.is_empty() {
                empty_state(
                    Icon::Queue,
                    store.t("common.empty"),
                    store.t("playlist.add_to"),
                )
            } else {
                let queue = Rc::new(pl.tracks.clone());
                virtual_track_list(
                    store,
                    form,
                    queue,
                    true,
                    true,
                    is_editable.then(|| pl.id.clone()),
                    is_editable.then_some(edit_order),
                )
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
