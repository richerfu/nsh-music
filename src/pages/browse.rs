//! Dynamic source-driven Browse page (with circular profile hero support when
//! `Browse.image_url` is provided) and virtualized Local Music overview page.

use std::rc::Rc;

use nsh_core::prelude::*;
use nsh_music_core::i18n::Locale;
use nsh_music_core::model::Track;

use crate::pages::{
    ARTWORK_SIZE_HERO, page_scroll, refresh_pill_button, render_browse_sections, shimmer_card_grid,
    shimmer_track_list, shuffle_and_play, sized_artwork_url, virtual_track_list,
};
use crate::state::{AppStore, Route};
use crate::ui::theme::{font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state_with_actions, ghost_button,
    pill_button, primary_button, theme,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowseActivePill {
    Radio,
    Podcasts,
    Page(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowsePillsSpec {
    show_radio: bool,
    show_podcasts: bool,
    tabs: Vec<(String, String)>,
    active: Option<BrowseActivePill>,
    locale: Locale,
}

pub fn browse_page(store: AppStore, form: DeviceForm, page: String, title: String) -> View {
    let active_page = page;
    let header_title_fallback = title.clone();
    let page_for_refresh = active_page.clone();
    let title_for_refresh = title;

    let pills_spec = computed(move || {
        let entry = store.active_source_entry();
        let show_radio = form.is_phone()
            && entry.as_ref().is_some_and(|e| {
                e.capabilities.contains("stations") || e.capabilities.contains("radio")
            });
        let show_podcasts = form.is_phone()
            && entry.as_ref().is_some_and(|e| {
                e.capabilities.contains("shows") || e.capabilities.contains("podcasts")
            });

        // Collect dynamic browse tabs from active source navigation or browse section links.
        let mut tabs: Vec<(String, String)> = Vec::new();
        if let Some(src_entry) = entry {
            for nav in src_entry.info.navigation {
                if !nav.page.is_empty() && !tabs.iter().any(|(p, _)| p == &nav.page) {
                    tabs.push((nav.page, nav.title));
                }
            }
        }
        if tabs.is_empty() {
            store.browse_data.with(|data| {
                for sec in &data.sections {
                    if !sec.more_page.is_empty() && !tabs.iter().any(|(p, _)| p == &sec.more_page) {
                        let label = if !sec.page_title.is_empty() {
                            sec.page_title.clone()
                        } else if !sec.title.is_empty() {
                            sec.title.clone()
                        } else {
                            sec.more_page.clone()
                        };
                        tabs.push((sec.more_page.clone(), label));
                    }
                    for link in &sec.links {
                        if !link.page.is_empty() && !tabs.iter().any(|(p, _)| p == &link.page) {
                            tabs.push((link.page.clone(), link.title.clone()));
                        }
                    }
                }
            });
        }

        let active = match store.route.get() {
            Route::Radio => Some(BrowseActivePill::Radio),
            Route::Podcasts => Some(BrowseActivePill::Podcasts),
            Route::Browse { .. } => Some(BrowseActivePill::Page(active_page.clone())),
            _ => None,
        };
        let locale = store.locale.get();

        BrowsePillsSpec {
            show_radio,
            show_podcasts,
            tabs,
            active,
            locale,
        }
    });

    let pills_row = view().w_full().child(dynamic(move || {
        let spec = pills_spec.get();

        untracked(move || {
            if !spec.show_radio && !spec.show_podcasts && spec.tabs.is_empty() {
                return view().into_element();
            }

            let is_touch = form.is_touch();
            let mut row = view()
                .flex_row()
                .items_center()
                .gap(px(if form.is_phone() { 6.0 } else { 8.0 }))
                .py_1();

            if spec.show_radio {
                let mut pill = pill_button(
                    move || store.t("nav.radio"),
                    spec.active == Some(BrowseActivePill::Radio),
                    move || store.navigate(Route::Radio),
                );
                if is_touch {
                    pill = pill.min_w(px(44.0)).min_h(px(44.0));
                }
                row = row.child(pill);
            }

            if spec.show_podcasts {
                let mut pill = pill_button(
                    move || store.t("nav.podcasts"),
                    spec.active == Some(BrowseActivePill::Podcasts),
                    move || store.navigate(Route::Podcasts),
                );
                if is_touch {
                    pill = pill.min_w(px(44.0)).min_h(px(44.0));
                }
                row = row.child(pill);
            }

            for (tab_page, tab_title) in spec.tabs {
                let is_active =
                    matches!(&spec.active, Some(BrowseActivePill::Page(p)) if p == &tab_page);
                let next_page = tab_page.clone();
                let next_title = tab_title.clone();
                let mut pill = pill_button(tab_title, is_active, move || {
                    store.navigate(Route::Browse {
                        page: next_page.clone(),
                        title: next_title.clone(),
                    });
                });
                if is_touch {
                    pill = pill.min_w(px(44.0)).min_h(px(44.0));
                }
                row = row.child(pill);
            }
            scroll()
                .horizontal()
                .scrollbar(false)
                .child(row)
                .into_element()
        })
    }));

    let show_skeleton = computed(move || {
        store.browse_loading.get() && store.browse_data.with(|d| d.sections.is_empty())
    });

    let content = view()
        .w_full()
        .gap_5()
        .child(
            view()
                .w_full()
                .gap_3()
                .children((!form.is_phone()).then(move || {
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
                            .text_size(px(font_size::DISPLAY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::fg),
                        )
                        .child(refresh_pill_button(
                            store,
                            form,
                            store.browse_loading,
                            move || {
                                store.refresh_browse(&page_for_refresh, &title_for_refresh);
                            },
                        ))
                }))
                .child(pills_row),
        )
        .child(dynamic(move || {
            let loading = show_skeleton.get();
            let data = store.browse_data.get();

            untracked(move || {
                if loading && data.sections.is_empty() {
                    return view()
                        .w_full()
                        .gap_5()
                        .child(shimmer_card_grid(store, form, 6, false))
                        .child(shimmer_track_list(5))
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

                // Creator / My Page profile hero when `Browse.image_url` is set (`ui_browse.go:108-119`).
                if !data.image_url.trim().is_empty() {
                    let avatar_size = if form.is_phone() { 84.0 } else { 112.0 };
                    let seed: SharedString = if data.title.is_empty() {
                        SharedString::from("browse-profile")
                    } else {
                        SharedString::from(data.title.clone())
                    };
                    let mut meta_col = view().flex_1().min_w(px(0.0)).gap_1();
                    if !data.subtitle.is_empty() {
                        meta_col = meta_col.child(
                            text(data.subtitle.clone())
                                .text_size(px(font_size::CAPTION))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary),
                        );
                    }
                    if !data.description.is_empty() {
                        meta_col = meta_col.child(
                            text(data.description.clone())
                                .number_of_lines(3)
                                .text_size(px(font_size::SUBTITLE))
                                .text_color(theme::muted_fg),
                        );
                    }
                    body = body.child(
                        Elevated::elevation(
                            view()
                                .w_full()
                                .p_4()
                                .flex_row()
                                .items_center()
                                .gap_5()
                                .rounded(px(radius::XL))
                                .bg(theme::card)
                                .border_1()
                                .border_color(theme::border),
                            Elevation::Subtle,
                        )
                        .child(artwork_box(
                            sized_artwork_url(&data.image_url, ARTWORK_SIZE_HERO),
                            seed,
                            avatar_size,
                            radius::PILL,
                        ))
                        .child(meta_col),
                    );
                } else if !data.subtitle.is_empty() || !data.description.is_empty() {
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
/// and virtualized scanned tracks list (zero demo catalog or mock text).
pub fn local_library_page(store: AppStore, form: DeviceForm) -> View {
    // Scope reactivity strictly to `local_folders` so unrelated settings changes
    // (theme, crossfade, experiments) never rebuild the track list.
    let folders_sig = computed(move || store.settings.with(|s| s.local_folders.clone()));

    let is_phone = form.is_phone();
    let min_h = px(form.min_touch_target());
    let header_card = view()
        .w_full()
        .p(px(if is_phone { 16.0 } else { 24.0 }))
        .gap_3()
        .rounded(px(radius::XL))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border)
        .children((!is_phone).then(|| {
            text(move || store.t("nav.local"))
                .role(AccessibilityRole::Header)
                .text_size(px(font_size::DISPLAY))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg)
        }))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_2()
                .children(is_phone.then(|| {
                    view()
                        .size(px(32.0))
                        .rounded(px(radius::CONTROL))
                        .bg(theme::primary_soft)
                        .items_center()
                        .justify_center()
                        .child(crate::ui::icon(
                            Icon::Folder,
                            16.0,
                            crate::ui::theme::colors::PRIMARY,
                        ))
                }))
                .child(
                    text(move || {
                        let _ = store.home_data.get();
                        let _ = folders_sig.get();
                        let count = store.local_source().local_track_count().to_string();
                        store.t_fmt("track.count_fmt", &[("count", &count)])
                    })
                    .text_size(px(font_size::SUBTITLE))
                    .font_weight(if is_phone {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if is_phone {
                        theme::fg()
                    } else {
                        theme::muted_fg()
                    }),
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
                    primary_button(
                        move || store.t("library.add_folder"),
                        move || store.pick_local_music_folders(),
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    ghost_button(
                        move || store.t("library.rescan"),
                        move || store.rescan_local_folders(),
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.flex_1()),
                ),
        );

    let tracks_section = view().w_full().child(dynamic(move || {
        let loading = store.browse_loading.get();
        let _ = store.home_data.get();
        let _ = folders_sig.get();

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
            let queue = Rc::new(tracks);

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
                .child(virtual_track_list(
                    store, form, queue, true, true, None, None,
                ))
                .into_element()
        })
    }));

    page_scroll(
        form,
        view()
            .w_full()
            .gap_5()
            .child(header_card)
            .child(tracks_section),
    )
}
