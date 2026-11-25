//! Dynamic source-driven Browse page and clean Local Music overview page.

use std::rc::Rc;

use nsh_core::prelude::*;
use nsh_music_core::model::Track;

use crate::pages::{
    page_scroll, render_browse_sections, render_track_row, shimmer_card_grid, shimmer_track_list,
    shuffle_and_play, track_table_header,
};
use crate::state::{AppStore, Route};
use crate::ui::{
    DeviceForm, Icon, empty_state_with_actions, ghost_button, pill_button, primary_button, theme,
};
use crate::ui::theme::{font_size, radius};

pub fn browse_page(store: AppStore, form: DeviceForm, page: String, title: String) -> View {
    let active_page = page;
    let header_title_fallback = title.clone();
    let page_for_refresh = active_page.clone();
    let title_for_refresh = title;

    let pills_row = view().w_full().child(dynamic(move || {
        let entry = store.active_source_entry();
        let data = store.browse_data.get();
        let current_page = active_page.clone();

        untracked(move || {
            // Collect dynamic browse tabs from active source navigation or browse section links.
            let mut tabs_vec: Vec<(String, String)> = Vec::new();
            if let Some(src_entry) = entry {
                for nav in src_entry.info.navigation {
                    if !nav.page.is_empty() && !tabs_vec.iter().any(|(p, _)| p == &nav.page) {
                        tabs_vec.push((nav.page, nav.title));
                    }
                }
            }
            if tabs_vec.is_empty() {
                for sec in &data.sections {
                    if !sec.more_page.is_empty()
                        && !tabs_vec.iter().any(|(p, _)| p == &sec.more_page)
                    {
                        let label = if !sec.page_title.is_empty() {
                            sec.page_title.clone()
                        } else if !sec.title.is_empty() {
                            sec.title.clone()
                        } else {
                            sec.more_page.clone()
                        };
                        tabs_vec.push((sec.more_page.clone(), label));
                    }
                    for link in &sec.links {
                        if !link.page.is_empty() && !tabs_vec.iter().any(|(p, _)| p == &link.page) {
                            tabs_vec.push((link.page.clone(), link.title.clone()));
                        }
                    }
                }
            }

            if tabs_vec.is_empty() {
                return view().into_element();
            }

            let mut row = view().flex_row().items_center().gap_2().py_1();
            for (tab_page, tab_title) in tabs_vec {
                let is_active = current_page == tab_page;
                let next_page = tab_page.clone();
                let next_title = tab_title.clone();
                row = row.child(pill_button(tab_title, is_active, move || {
                    store.navigate(Route::Browse {
                        page: next_page.clone(),
                        title: next_title.clone(),
                    });
                }));
            }
            scroll().horizontal().scrollbar(false).child(row).into_element()
        })
    }));

    let content = view()
        .w_full()
        .gap_5()
        .child(
            view()
                .w_full()
                .gap_3()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            text(move || {
                                let b = store.browse_data.get();
                                if !b.title.is_empty() {
                                    SharedString::from(b.title)
                                } else if !header_title_fallback.is_empty() {
                                    SharedString::from(header_title_fallback.clone())
                                } else {
                                    store.t("nav.browse")
                                }
                            })
                            .role(AccessibilityRole::Header)
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(if form.is_phone() {
                                font_size::TITLE_LG
                            } else {
                                font_size::DISPLAY
                            }))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::fg),
                        )
                        .child(ghost_button(move || store.t("common.refresh"), move || {
                            store.refresh_browse(&page_for_refresh, &title_for_refresh);
                        })),
                )
                .child(pills_row),
        )
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let data = store.browse_data.get();

            untracked(move || {
                if loading && data.sections.is_empty() {
                    return view()
                        .w_full()
                        .gap_5()
                        .child(shimmer_card_grid(store, form, 6, false))
                        .child(shimmer_track_list(4))
                        .into_element();
                }

                if data.sections.is_empty() {
                    let actions = view()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .flex_wrap()
                        .gap_2p5()
                        .child(primary_button(
                            move || store.t("home.add_folder"),
                            move || store.pick_local_music_folders(),
                        ))
                        .child(ghost_button(
                            move || store.t("home.install_plugin"),
                            move || store.navigate(Route::Plugins),
                        ));
                    return empty_state_with_actions(
                        Icon::Compass,
                        move || store.t("browse.empty"),
                        move || store.t("home.empty_subtitle"),
                        actions,
                    )
                    .into_element();
                }

                let mut body = view().w_full().gap_5();
                if !data.subtitle.is_empty() || !data.description.is_empty() {
                    let lead = if !data.description.is_empty() {
                        data.description.clone()
                    } else {
                        data.subtitle.clone()
                    };
                    body = body.child(
                        text(lead)
                            .text_size(px(font_size::SUBTITLE))
                            .text_color(theme::muted_fg),
                    );
                }

                let first_tracks: Vec<Track> = data
                    .sections
                    .iter()
                    .find(|s| !s.tracks.is_empty())
                    .map(|s| s.tracks.clone())
                    .unwrap_or_default();
                if !first_tracks.is_empty() {
                    let play_tracks = first_tracks.clone();
                    let shuf_tracks = first_tracks;
                    body = body.child(
                        view()
                            .w_full()
                            .flex_row()
                            .items_center()
                            .flex_wrap()
                            .gap_2p5()
                            .child(primary_button(
                                move || store.t("player.play_all"),
                                move || store.play_list(play_tracks.clone(), 0),
                            ))
                            .child(ghost_button(
                                move || store.t("player.shuffle_all"),
                                move || shuffle_and_play(store, shuf_tracks.clone()),
                            )),
                    );
                }

                body = body.child(render_browse_sections(store, form, data.sections));
                body.into_element()
            })
        }));

    page_scroll(form, content)
}

/// Clean Local Files overview page (`Route::Local`) with folder scanner actions
/// and scanned tracks list (zero demo catalog or mock text).
pub fn local_library_page(store: AppStore, form: DeviceForm) -> View {
    let header_card = view()
        .w_full()
        .p(px(if form.is_phone() { 18.0 } else { 24.0 }))
        .gap_3()
        .rounded(px(radius::XL))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border)
        .child(
            text(move || store.t("nav.local"))
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
            text(move || {
                let _ = store.home_data.get();
                let _ = store.settings.get();
                let count = store.local_source().local_track_count().to_string();
                store.t_fmt("track.count_fmt", &[("count", &count)])
            })
            .text_size(px(font_size::SUBTITLE))
            .text_color(theme::muted_fg),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(primary_button(
                    move || store.t("library.add_folder"),
                    move || store.pick_local_music_folders(),
                ))
                .child(ghost_button(
                    move || store.t("library.rescan"),
                    move || store.rescan_local_folders(),
                )),
        );

    let tracks_section = view().w_full().child(dynamic(move || {
        let loading = store.browse_loading.get();
        let _ = store.home_data.get();
        let _ = store.settings.get();

        untracked(move || {
            let tracks = store.local_source().all_tracks();
            if loading && tracks.is_empty() {
                return shimmer_track_list(6).into_element();
            }

            if tracks.is_empty() {
                let actions = view()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .flex_wrap()
                    .gap_2p5()
                    .child(primary_button(
                        move || store.t("library.add_folder"),
                        move || store.pick_local_music_folders(),
                    ))
                    .child(ghost_button(
                        move || store.t("library.rescan"),
                        move || store.rescan_local_folders(),
                    ));
                return empty_state_with_actions(
                    Icon::Folder,
                    move || store.t("library.local_empty"),
                    move || store.t("settings.local_folders.empty"),
                    actions,
                )
                .into_element();
            }

            let play_tracks = tracks.clone();
            let shuffle_tracks = tracks.clone();
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
                .gap_3()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .flex_wrap()
                        .gap_2p5()
                        .child(primary_button(
                            move || store.t("player.play_all"),
                            move || store.play_list(play_tracks.clone(), 0),
                        ))
                        .child(ghost_button(
                            move || store.t("player.shuffle_all"),
                            move || shuffle_and_play(store, shuffle_tracks.clone()),
                        )),
                )
                .child(
                    view()
                        .w_full()
                        .p_2()
                        .rounded(px(radius::CARD))
                        .bg(theme::card)
                        .border_1()
                        .border_color(theme::border)
                        .child(rows),
                )
                .into_element()
        })
    }));

    page_scroll(form, view().w_full().gap_5().child(header_card).child(tracks_section))
}
