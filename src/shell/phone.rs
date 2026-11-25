//! Phone application shell:
//! - Respects `safe_area()` top & bottom insets
//! - Compact top header with source selector capsule (`store.active_source_name()`) & quick search/settings buttons
//! - Capability-gated quick pills (`Radio`, `Podcasts`, `Liked`, `Recent`, custom active source `info.navigation`)
//! - Floating `58pt` mini-player dock above tabs expanding full-screen Now Playing on tap
//! - Bottom 4-tab bar (`Home`, `Browse`, `Library`, `Settings`), all touch targets `>= 44pt`
//! - `on_back_press` closing modal -> Now Playing -> drawer -> `store.go_back()`

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::NavItem;

use crate::{
    pages,
    state::{ActiveModal, AppStore, Route},
    ui::{
        controls::{
            artwork_box, icon_button, icon_button_tinted, pill_button, play_pause_button,
            source_pill,
        },
        icons::{Icon, icon_reactive},
        motion,
        theme::{self, DeviceForm, Elevated, colors, font_size, radius},
    },
};

const MINI_PLAYER_HEIGHT: f32 = 58.0;

/// Renders the compact mobile Phone shell (`w < 600pt`, including `390×844` tests).
pub fn phone_shell(store: AppStore) -> View {
    on_back_press(move || {
        if !matches!(store.active_modal.get_untracked(), ActiveModal::None) {
            store.close_modal();
            true
        } else if store.now_playing_open.get_untracked() {
            store.close_now_playing();
            true
        } else if store.mobile_drawer_open.get_untracked() {
            store.set_mobile_drawer_open(false);
            true
        } else {
            store.go_back()
        }
    });

    view()
        .test_id("shell-phone")
        .flex_1()
        .w_full()
        .h_full()
        .bg(theme::bg)
        .text_color(theme::fg)
        .child(phone_top_header(store))
        .child(phone_source_nav_strip(store))
        .child(phone_main_viewport(store))
        .child(phone_mini_player(store))
        .child(phone_bottom_tabs(store))
}

fn phone_top_header(store: AppStore) -> View {
    view()
        .test_id("phone-header")
        .w_full()
        .flex_shrink_0()
        .pt(move || px(safe_area().top.0 + 6.0))
        .pb_2()
        .px_3()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .glass_bar_surface()
        .border_b_1()
        .border_color(theme::border)
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .flex_row()
                .items_center()
                .gap_1p5()
                .child(when(
                    move || store.can_go_back(),
                    move || {
                        icon_button(
                            Icon::ChevronLeft,
                            move || store.t("common.back"),
                            move || {
                                let _ = store.go_back();
                            },
                        )
                        .test_id("phone-back-btn")
                    },
                ))
                .child(
                    source_pill(move || store.active_source_name(), true, move || {
                        store.open_modal(ActiveModal::SourceSwitcher);
                    })
                    .min_w(px(44.0))
                    .min_h(px(44.0))
                    .test_id("phone-source-switcher"),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_0p5()
                .flex_shrink_0()
                .child(
                    icon_button_tinted(
                        Icon::Search,
                        move || {
                            if store.route.get() == Route::Search {
                                theme::primary()
                            } else {
                                theme::fg()
                            }
                        },
                        move || store.t("nav.search"),
                        move || store.navigate(Route::Search),
                    )
                    .test_id("phone-search-btn"),
                )
                .child(
                    icon_button_tinted(
                        Icon::Settings,
                        move || {
                            if matches!(store.route.get(), Route::Settings | Route::Plugins) {
                                theme::primary()
                            } else {
                                theme::fg()
                            }
                        },
                        move || store.t("nav.settings"),
                        move || store.navigate(Route::Settings),
                    )
                    .test_id("phone-settings-btn"),
                ),
        )
}

/// Capability-gated horizontal quick-nav strip for Radio, Podcasts, and active
/// source custom `info.navigation` pages when present.
fn phone_source_nav_strip(store: AppStore) -> impl IntoElement {
    let custom_nav = computed(move || active_custom_nav(store));
    let show_strip = computed(move || {
        store.active_can("stations")
            || store.active_can("radio")
            || store.active_can("shows")
            || store.active_can("podcasts")
            || custom_nav.with(|v| !v.is_empty())
    });

    when(show_strip, move || {
        scroll()
            .horizontal()
            .scrollbar(false)
            .w_full()
            .flex_shrink_0()
            .bg(theme::bg)
            .border_b_1()
            .border_color(theme::border)
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .child(when(
                        move || store.active_can("stations") || store.active_can("radio"),
                        move || {
                            pill_button(
                                move || store.t("nav.radio"),
                                move || store.route.get() == Route::Radio,
                                move || store.navigate(Route::Radio),
                            )
                            .min_w(px(44.0))
                            .min_h(px(44.0))
                        },
                    ))
                    .child(when(
                        move || store.active_can("shows") || store.active_can("podcasts"),
                        move || {
                            pill_button(
                                move || store.t("nav.podcasts"),
                                move || store.route.get() == Route::Podcasts,
                                move || store.navigate(Route::Podcasts),
                            )
                            .min_w(px(44.0))
                            .min_h(px(44.0))
                        },
                    ))
                    .child(when(
                        move || store.active_can("liked"),
                        move || {
                            pill_button(
                                move || store.t("nav.liked"),
                                move || store.route.get() == Route::Liked,
                                move || store.navigate(Route::Liked),
                            )
                            .min_w(px(44.0))
                            .min_h(px(44.0))
                        },
                    ))
                    .child(when(
                        move || store.active_can("recent"),
                        move || {
                            pill_button(
                                move || store.t("nav.recent"),
                                move || store.route.get() == Route::Recent,
                                move || store.navigate(Route::Recent),
                            )
                            .min_w(px(44.0))
                            .min_h(px(44.0))
                        },
                    ))
                    .child(each(
                        custom_nav,
                        |item: &NavItem| format!("{}:{}", item.page, item.title),
                        move |item, _| {
                            let page = item.with_untracked(|i| i.page.clone());
                            let title = item.with_untracked(|i| i.title.clone());
                            let check_page = page.clone();
                            pill_button(
                                move || item.with(|i| SharedString::from(i.title.clone())),
                                move || {
                                    matches!(
                                        store.route.get(),
                                        Route::Browse { page: ref p, .. } if p == &check_page
                                    )
                                },
                                move || {
                                    store.navigate(Route::Browse {
                                        page: page.clone(),
                                        title: title.clone(),
                                    });
                                },
                            )
                            .min_w(px(44.0))
                            .min_h(px(44.0))
                        },
                    )),
            )
    })
}

fn phone_main_viewport(store: AppStore) -> View {
    view()
        .test_id("phone-main-viewport")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .child(pages::render_route(store, DeviceForm::Phone))
}

fn phone_mini_player(store: AppStore) -> impl IntoElement {
    let has_track = computed(move || store.current_track.with(Option::is_some));

    when(has_track, move || {
        let track_seed = computed(move || {
            store.current_track.with(|t| match t {
                Some(tr) if !tr.album.id.is_empty() => SharedString::from(tr.album.id.clone()),
                Some(tr) => SharedString::from(tr.id.clone()),
                None => SharedString::from("nsh-music-idle"),
            })
        });

        let progress_ratio = move || {
            let dur = store.duration_ms.get();
            if dur == 0 {
                0.0_f32
            } else {
                (store.position_ms.get() as f32 / dur as f32).clamp(0.0, 1.0)
            }
        };

        view()
            .w_full()
            .px_3()
            .pt_1()
            .pb_2()
            .flex_shrink_0()
            .child(
                view()
                    .test_id("phone-mini-player-dock")
                    .w_full()
                    .h(px(MINI_PLAYER_HEIGHT))
                    .rounded(px(radius::XL))
                    .glass_surface()
                    .overflow_hidden()
                    .child(
                        view()
                            .flex_1()
                            .w_full()
                            .pl_2p5()
                            .pr_1p5()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                button()
                                    .test_id("phone-mini-player")
                                    .accessibility_label(move || {
                                        Some(store.t("now_playing.expand"))
                                    })
                                    .on_press(move || store.open_now_playing())
                                    .flex_1()
                                    .min_w(px(44.0))
                                    .min_h(px(44.0))
                                    .flex_row()
                                    .items_center()
                                    .gap_2p5()
                                    .active(|s| s.opacity(0.78))
                                    .focus_visible(|s| {
                                        s.outline(px(2.0)).outline_color(theme::ring)
                                    })
                                    .child(dynamic(move || {
                                        let seed = track_seed.get();
                                        untracked(move || {
                                            artwork_box(
                                                store.current_artwork_uri,
                                                seed,
                                                40.0,
                                                radius::SM,
                                            )
                                        })
                                    }))
                                    .child(
                                        view()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .gap_0p5()
                                            .child(
                                                text(move || {
                                                    store.current_track.with(|t| match t {
                                                        Some(tr) => {
                                                            SharedString::from(tr.name.clone())
                                                        }
                                                        None => store.t("player.idle_title"),
                                                    })
                                                })
                                                .number_of_lines(1)
                                                .text_size(px(font_size::BODY))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme::fg),
                                            )
                                            .child(
                                                text(move || {
                                                    store.current_track.with(|t| match t {
                                                        Some(tr) => {
                                                            SharedString::from(tr.artist_names())
                                                        }
                                                        None => SharedString::default(),
                                                    })
                                                })
                                                .number_of_lines(1)
                                                .text_size(px(font_size::CAPTION))
                                                .text_color(theme::muted_fg),
                                            ),
                                    ),
                            )
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_0p5()
                                    .flex_shrink_0()
                                    .child(play_pause_button(
                                        store.is_playing,
                                        44.0,
                                        move || {
                                            if store.is_playing.get() {
                                                store.t("player.pause")
                                            } else {
                                                store.t("player.play")
                                            }
                                        },
                                        move || store.toggle_play(),
                                    ))
                                    .child(
                                        icon_button(
                                            Icon::Next,
                                            move || store.t("player.next"),
                                            move || store.next_track(),
                                        )
                                        .test_id("phone-next-btn"),
                                    ),
                            ),
                    )
                    .child(
                        view()
                            .w_full()
                            .h(px(2.0))
                            .bg(theme::track_rail)
                            .child(
                                view()
                                    .h_full()
                                    .w(move || relative(progress_ratio()))
                                    .bg(theme::primary),
                            ),
                    ),
            )
    })
}

fn phone_bottom_tabs(store: AppStore) -> View {
    view()
        .test_id("phone-tab-bar")
        .w_full()
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_around()
        .pt_1p5()
        .pb(move || px(safe_area().bottom.0 + 6.0))
        .px_2()
        .glass_bar_surface()
        .border_t_1()
        .border_color(theme::border)
        .child(phone_tab_item(
            store,
            Icon::Home,
            "nav.home",
            "tab-home",
            |r| matches!(r, Route::Home),
            || Route::Home,
        ))
        .child(phone_tab_item(
            store,
            Icon::Compass,
            "nav.browse",
            "tab-browse",
            |r| {
                matches!(
                    r,
                    Route::Browse { .. } | Route::Radio | Route::Podcasts | Route::ShowDetail { .. }
                )
            },
            || Route::Browse {
                page: String::new(),
                title: String::new(),
            },
        ))
        .child(phone_tab_item(
            store,
            Icon::Library,
            "nav.library",
            "tab-library",
            |r| {
                matches!(
                    r,
                    Route::Library
                        | Route::Liked
                        | Route::Playlists
                        | Route::PlaylistDetail { .. }
                        | Route::AlbumDetail { .. }
                        | Route::ArtistDetail { .. }
                        | Route::Recent
                        | Route::Local
                )
            },
            || Route::Library,
        ))
        .child(phone_tab_item(
            store,
            Icon::Settings,
            "nav.settings",
            "tab-settings",
            |r| matches!(r, Route::Settings | Route::Plugins),
            || Route::Settings,
        ))
}

fn phone_tab_item(
    store: AppStore,
    glyph: Icon,
    label_key: &'static str,
    test_id: &'static str,
    is_active: fn(&Route) -> bool,
    make_route: fn() -> Route,
) -> impl IntoElement {
    let active = computed(move || store.route.with(is_active));

    button()
        .test_id(test_id)
        .selected(active)
        .accessibility_label(move || Some(store.t(label_key)))
        .on_press(move || store.navigate(make_route()))
        .flex_1()
        .min_w(px(44.0))
        .min_h(px(48.0))
        .py_1()
        .items_center()
        .justify_center()
        .gap_0p5()
        .rounded(px(radius::CONTROL))
        .bg(colors::TRANSPARENT)
        .active(|s| s.bg(theme::hover).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, 20.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || store.t(label_key))
                .number_of_lines(1)
                .text_size(px(font_size::MICRO))
                .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                .text_color(move || {
                    if active.get() {
                        theme::primary()
                    } else {
                        theme::muted_fg()
                    }
                }),
        )
}

fn active_custom_nav(store: AppStore) -> Vec<NavItem> {
    store
        .active_source_entry()
        .map(|e| {
            e.info
                .navigation
                .into_iter()
                .filter(|item| {
                    !item.page.is_empty()
                        && !item.title.is_empty()
                        && !matches!(
                            item.builtin.as_str(),
                            "home"
                                | "browse"
                                | "radio"
                                | "stations"
                                | "podcasts"
                                | "shows"
                                | "liked"
                                | "recent"
                                | "local"
                                | "library"
                                | "search"
                        )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}
