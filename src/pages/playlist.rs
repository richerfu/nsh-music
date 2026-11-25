//! Playlist detail page with artwork + ambient wash hero, Play All / Shuffle /
//! Save / Queue / Rename / Delete (with native destructive confirm) actions,
//! editable playlist Move Up / Move Down / Remove actions, and loading shimmer.

use std::rc::Rc;

use nsh_core::prelude::*;

use crate::pages::{
    PlaylistTrackContext, detail_hero, page_scroll, render_track_row_in_playlist,
    shimmer_detail_hero, shimmer_track_list, shuffle_and_play, track_table_header,
};
use crate::state::{ActiveModal, AppStore};
use crate::ui::{
    DeviceForm, Icon, confirm_destructive, empty_state, format_duration_ms, ghost_button,
    icon_button_sized, primary_button, theme,
};
use crate::ui::theme::radius;

pub fn playlist_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let edit_order = signal(false);

    let body = view().w_full().child(dynamic(move || {
        let active = store.active_playlist.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(pl) = active.filter(|p| p.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form))
                    .child(shimmer_track_list(6))
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
            let saved_now = pl.saved;
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

            let actions = view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(
                    primary_button(move || store.t("player.play_all"), move || {
                        if !tracks_for_play.is_empty() {
                            store.play_list(tracks_for_play.clone(), 0);
                        }
                    })
                    .disabled(pl.tracks.is_empty()),
                )
                .child(
                    ghost_button(move || store.t("player.shuffle_all"), move || {
                        shuffle_and_play(store, tracks_for_shuffle.clone());
                    })
                    .disabled(pl.tracks.is_empty()),
                )
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
                            store.active_playlist.set(Some(updated));
                            let src = store.active_source();
                            let pid = pl_id_save.clone();
                            spawn(async move {
                                let _ = blocking(move || src.save_playlist(&pid, next)).await;
                            });
                            store.toasts.info(if next {
                                store.t("library.unsave_playlist")
                            } else {
                                store.t("library.save_playlist")
                            });
                        },
                    )
                }))
                .child(
                    ghost_button(move || store.t("player.add_to_queue"), move || {
                        for tr in &tracks_for_queue {
                            store.enqueue(tr.clone());
                        }
                    })
                    .disabled(pl.tracks.is_empty()),
                )
                .children((is_editable && (can_move || can_remove) && !pl.tracks.is_empty()).then(
                    || {
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
                    },
                ))
                .children(can_rename.then(move || {
                    ghost_button(move || store.t("playlist.rename"), move || {
                        store.open_modal(ActiveModal::RenamePlaylist {
                            id: pl_id_rename.clone(),
                            current_name: pl_name_rename.clone(),
                        });
                    })
                }))
                .children(can_delete.then(move || {
                    ghost_button(move || store.t("playlist.delete"), move || {
                        let pid = pl_id_del.clone();
                        let title = store.t_fmt(
                            "playlist.delete_confirm_title",
                            &[("name", &pl_name_del)],
                        );
                        let desc = store.t("playlist.delete_confirm_desc");
                        let cancel = store.t("common.cancel");
                        let confirm = store.t("common.delete");
                        confirm_destructive(title, desc, cancel, confirm, move || {
                            store.delete_playlist(&pid);
                        });
                    })
                }));

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
                let total = pl.tracks.len();
                let mut rows = view().w_full().gap_1();
                if !form.is_phone() {
                    rows = rows.child(track_table_header(store, form, true));
                }
                for (idx, tr) in pl.tracks.iter().enumerate() {
                    let ctx = is_editable.then(|| PlaylistTrackContext {
                        playlist_id: pl.id.clone(),
                        index: idx,
                        total,
                    });
                    let row_el = render_track_row_in_playlist(
                        store,
                        tr.clone(),
                        Some(idx),
                        Rc::clone(&queue),
                        idx,
                        true,
                        ctx,
                    );

                    if is_editable && (can_move || can_remove) {
                        let pid_up = pl.id.clone();
                        let pid_dn = pl.id.clone();
                        let pid_rm = pl.id.clone();
                        let tid_rm = tr.id.clone();
                        rows = rows.child(
                            view()
                                .w_full()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .child(view().flex_1().min_w(px(0.0)).child(row_el))
                                .child(when(
                                    move || edit_order.get(),
                                    move || {
                                        let pid_up = pid_up.clone();
                                        let pid_dn = pid_dn.clone();
                                        let pid_rm = pid_rm.clone();
                                        let tid_rm = tid_rm.clone();
                                        view()
                                            .flex_row()
                                            .items_center()
                                            .gap_1()
                                            .flex_shrink_0()
                                            .children(can_move.then(move || {
                                                view()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap_0p5()
                                                    .child(
                                                        icon_button_sized(
                                                            Icon::ArrowUp,
                                                            32.0,
                                                            15.0,
                                                            theme::muted_fg,
                                                            move || store.t("playlist.move_up"),
                                                            move || {
                                                                if idx > 0 {
                                                                    store.move_playlist_track(
                                                                        &pid_up,
                                                                        idx,
                                                                        idx - 1,
                                                                    );
                                                                }
                                                            },
                                                        )
                                                        .disabled(idx == 0),
                                                    )
                                                    .child(
                                                        icon_button_sized(
                                                            Icon::ArrowDown,
                                                            32.0,
                                                            15.0,
                                                            theme::muted_fg,
                                                            move || store.t("playlist.move_down"),
                                                            move || {
                                                                if idx + 1 < total {
                                                                    store.move_playlist_track(
                                                                        &pid_dn,
                                                                        idx,
                                                                        idx + 1,
                                                                    );
                                                                }
                                                            },
                                                        )
                                                        .disabled(idx + 1 >= total),
                                                    )
                                            }))
                                            .children(can_remove.then(move || {
                                                icon_button_sized(
                                                    Icon::Trash,
                                                    32.0,
                                                    15.0,
                                                    theme::danger,
                                                    move || store.t("playlist.remove_track"),
                                                    move || {
                                                        store
                                                            .remove_from_playlist(&pid_rm, &tid_rm);
                                                    },
                                                )
                                            }))
                                    },
                                )),
                        );
                    } else {
                        rows = rows.child(row_el);
                    }
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
