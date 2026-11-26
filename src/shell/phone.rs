//! Phone application shell:
//! - Respects `safe_area()` 4-edge insets
//! - Contextual top navigation bar:
//!   - Root tabs (`Home`, `Browse`, `Library`): bold tab title + single compact source capsule + contextual Refresh (`Home`/`Browse`) + Search (zero back/duplicate settings buttons)
//!   - Root `Settings` tab: clean header title only
//!   - Level-2 pushed screens: `<` Back button + centered dynamic route title + contextual `+` Install (`Plugins`) or Refresh (`Browse`/`Radio`/`Podcasts`)
//! - Floating `64pt` artwork-tinted frosted mini-player dock expanding full-screen Now Playing on tap (respects `safe_area().bottom` when tabs are hidden)
//! - Bottom 4-tab bar (`Home`, `Browse`, `Library`, `Settings`) visible only on root tabs, switching via `store.switch_tab(...)`
//! - `on_back_press` closing modal -> Now Playing -> drawer -> `store.go_back()`

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_audio::PlaybackStatus;

use crate::{
    pages,
    state::{ActiveModal, AppStore, Route, SettingsSection},
    ui::{
        controls::{artwork_box, icon_button, icon_button_tinted, source_pill},
        icons::{Icon, icon_reactive},
        loading_spinner, motion,
        theme::{self, DeviceForm, Elevated, Elevation, colors, radius},
    },
};

const MINI_PLAYER_HEIGHT: f32 = 64.0;

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
        .child(phone_main_viewport(store))
        .child(phone_mini_player(store))
        .child(phone_bottom_tabs(store))
        .child(phone_bottom_safe_inset(store))
}

fn phone_refresh_button(store: AppStore) -> impl IntoElement {
    let loading = store.browse_loading;

    button()
        .test_id("phone-refresh-btn")
        .accessibility_label(move || Some(store.t("common.refresh")))
        .disabled(move || loading.get())
        .on_press(move || match store.route.get_untracked() {
            Route::Browse { page, title } => store.refresh_browse(&page, &title),
            _ => store.refresh_home(),
        })
        .min_w(px(44.0))
        .min_h(px(44.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.6))
        .child(
            when(
                move || loading.get(),
                move || loading_spinner(18.0, theme::muted_fg),
            )
            .otherwise(move || icon_reactive(Icon::Refresh, 18.0, theme::fg)),
        )
}

fn phone_top_header(store: AppStore) -> View {
    let is_secondary = computed(move || store.route.with(AppStore::is_phone_secondary_route));
    let is_root_settings = computed(move || store.route.get() == Route::Settings);
    let is_plugins_detail =
        computed(move || store.route.get() == Route::SettingsDetail(SettingsSection::Plugins));
    let show_root_refresh = computed(move || {
        store.route.with(|r| {
            matches!(r, Route::Home) || matches!(r, Route::Browse { page, .. } if page.is_empty())
        })
    });
    let show_l2_refresh = computed(move || {
        store.route.with(|r| {
            matches!(r, Route::Radio | Route::Podcasts)
                || matches!(r, Route::Browse { page, .. } if !page.is_empty())
        })
    });
    let route_title = computed(move || resolve_phone_route_title(store));

    view()
        .test_id("phone-header")
        .w_full()
        .flex_shrink_0()
        .pt(move || px(safe_area().top.0 + 4.0))
        .pb_1p5()
        .pl(move || px(safe_area().left.0 + if is_secondary.get() { 8.0 } else { 16.0 }))
        .pr(move || px(safe_area().right.0 + if is_secondary.get() { 8.0 } else { 16.0 }))
        .min_h(px(50.0))
        .bg(move || {
            if store.page_scrolled.get() {
                theme::card()
            } else {
                theme::bg()
            }
        })
        .border_b_1()
        .border_color(move || {
            if store.page_scrolled.get() {
                theme::border()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(160.0))
        .child(
            when(is_secondary, move || {
                // Level-2 pushed page header: `<` Back + centered dynamic route title + contextual right slot.
                view()
                    .w_full()
                    .min_h(px(44.0))
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        icon_button(
                            Icon::ChevronLeft,
                            move || store.t("common.back"),
                            move || {
                                let _ = store.go_back();
                            },
                        )
                        .min_w(px(44.0))
                        .min_h(px(44.0))
                        .test_id("phone-back-btn"),
                    )
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .items_center()
                            .justify_center()
                            .child(
                                text(move || route_title.get())
                                    .number_of_lines(1)
                                    .text_size(px(17.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::fg),
                            ),
                    )
                    .child(
                        view()
                            .w(px(44.0))
                            .h(px(44.0))
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .child(
                                when(is_plugins_detail, move || {
                                    icon_button_tinted(
                                        Icon::Plus,
                                        theme::primary,
                                        move || store.t("plugins.install"),
                                        move || store.open_modal(ActiveModal::InstallPlugin),
                                    )
                                    .min_w(px(44.0))
                                    .min_h(px(44.0))
                                    .test_id("phone-plugins-install-btn")
                                })
                                .otherwise(move || {
                                    when(show_l2_refresh, move || phone_refresh_button(store))
                                }),
                            ),
                    )
            })
            .otherwise(move || {
                // Root tab header: clean title only on Settings; bold tab title + source switcher + contextual refresh + search on Home/Browse/Library.
                when(is_root_settings, move || {
                    view()
                        .w_full()
                        .min_h(px(44.0))
                        .flex_row()
                        .items_center()
                        .pl_1()
                        .child(
                            text(move || store.t("settings.title"))
                                .number_of_lines(1)
                                .text_size(px(24.0))
                                .font_weight(FontWeight::BOLD)
                                .letter_spacing(px(-0.3))
                                .text_color(theme::fg),
                        )
                })
                .otherwise(move || {
                    view()
                        .w_full()
                        .min_h(px(44.0))
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            text(move || route_title.get())
                                .flex_shrink_0()
                                .number_of_lines(1)
                                .text_size(px(23.0))
                                .font_weight(FontWeight::BOLD)
                                .letter_spacing(px(-0.3))
                                .text_color(theme::fg),
                        )
                        .child(
                            view()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex_row()
                                .items_center()
                                .justify_end()
                                .gap_0p5()
                                .child(
                                    source_pill(
                                        move || store.active_source_name(),
                                        true,
                                        move || {
                                            store.open_modal(ActiveModal::SourceSwitcher);
                                        },
                                    )
                                    .max_w(px(140.0))
                                    .min_w(px(44.0))
                                    .min_h(px(44.0))
                                    .test_id("phone-source-switcher"),
                                )
                                .child(when(show_root_refresh, move || phone_refresh_button(store)))
                                .child(
                                    icon_button_tinted(
                                        Icon::Search,
                                        theme::fg,
                                        move || store.t("nav.search"),
                                        move || store.navigate(Route::Search),
                                    )
                                    .min_w(px(44.0))
                                    .min_h(px(44.0))
                                    .test_id("phone-search-btn"),
                                ),
                        )
                })
            }),
        )
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

        let is_loading = computed(move || store.playback_status.with(PlaybackStatus::is_loading));

        let initial_playing = untracked(|| store.is_playing.get());
        let play_morph = animated_value(if initial_playing { 1.0 } else { 0.0 });
        let prev_playing = stored(initial_playing);
        effect(move || {
            let now = store.is_playing.get();
            if prev_playing.replace(now) != now {
                motion::animate_spring(
                    &play_morph,
                    if now { 1.0 } else { 0.0 },
                    motion::spring_settle(),
                );
            }
        });

        let progress_ratio = computed(move || {
            let dur = store.duration_ms.get();
            if dur == 0 {
                0.0_f32
            } else {
                let raw = (store.position_ms.get() as f32 / dur as f32).clamp(0.0, 1.0);
                (raw * 1000.0).round() / 1000.0
            }
        });

        let buffered_ratio = computed(move || {
            let dur = store.duration_ms.get();
            if dur == 0 {
                0.0_f32
            } else {
                let raw = (store.buffered_ms.get() as f32 / dur as f32).clamp(0.0, 1.0);
                (raw * 200.0).round() / 200.0
            }
        });

        view()
            .w_full()
            .pl(move || px(safe_area().left.0 + 12.0))
            .pr(move || px(safe_area().right.0 + 12.0))
            .pt_1()
            .pb(move || {
                if store.route.with(AppStore::is_phone_secondary_route) {
                    px(safe_area().bottom.0 + 10.0)
                } else {
                    px(8.0)
                }
            })
            .flex_shrink_0()
            .child(
                Elevated::elevation(
                    view()
                        .test_id("phone-mini-player-dock")
                        .w_full()
                        .h(px(MINI_PLAYER_HEIGHT))
                        .rounded(px(radius::XL))
                        .overflow_hidden()
                        .bg(move || theme::mini_player_surface(store.current_accent.get()))
                        .style_when(
                            move || !store.now_playing_open.get(),
                            |s| s.backdrop_blur(px(20.0)),
                        )
                        .border_1()
                        .border_color(move || theme::mini_player_border(store.current_accent.get()))
                        .transition_colors()
                        .duration(move || motion::transition_ms(220.0)),
                    Elevation::Floating,
                )
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
                                .accessibility_label(move || Some(store.t("now_playing.expand")))
                                .on_press(move || store.open_now_playing())
                                .flex_1()
                                .min_w(px(44.0))
                                .min_h(px(44.0))
                                .flex_row()
                                .items_center()
                                .gap_2p5()
                                .active(|s| s.opacity(0.78))
                                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                                .child(
                                    Elevated::elevation(
                                        view()
                                            .size(px(44.0))
                                            .rounded(px(radius::CONTROL))
                                            .overflow_hidden()
                                            .flex_shrink_0(),
                                        Elevation::Subtle,
                                    )
                                    .child(dynamic(move || {
                                        let seed = track_seed.get();
                                        untracked(move || {
                                            artwork_box(
                                                store.current_artwork_uri,
                                                seed,
                                                44.0,
                                                radius::CONTROL,
                                            )
                                        })
                                    }))
                                    .child(
                                        view()
                                            .absolute()
                                            .inset_0()
                                            .rounded(px(radius::CONTROL))
                                            .border_1()
                                            .border_color(theme::glass_rim)
                                            .pointer_events(PointerEvents::None),
                                    ),
                                )
                                .child(
                                    view()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .gap_0p5()
                                        .child(
                                            text(move || {
                                                store.current_track.with(|t| match t {
                                                    Some(tr) => SharedString::from(tr.name.clone()),
                                                    None => store.t("player.idle_title"),
                                                })
                                            })
                                            .number_of_lines(1)
                                            .text_size(px(14.5))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .letter_spacing(px(-0.15))
                                            .text_color(theme::fg),
                                        )
                                        .child(
                                            view()
                                                .w_full()
                                                .min_w(px(0.0))
                                                .flex_row()
                                                .items_center()
                                                .gap_1p5()
                                                .child(when(is_loading, move || {
                                                    loading_spinner(11.0, theme::muted_fg)
                                                }))
                                                .child(
                                                    text(move || {
                                                        match store.playback_status.get() {
                                                            PlaybackStatus::Loading => {
                                                                store.t("player.buffering")
                                                            }
                                                            PlaybackStatus::Error(msg) => {
                                                                SharedString::from(msg)
                                                            }
                                                            _ => {
                                                                store.current_track.with(
                                                                    |t| match t {
                                                                        Some(tr) => {
                                                                            SharedString::from(
                                                                                tr.artist_names(),
                                                                            )
                                                                        }
                                                                        None => {
                                                                            SharedString::default()
                                                                        }
                                                                    },
                                                                )
                                                            }
                                                        }
                                                    })
                                                    .flex_1()
                                                    .min_w(px(0.0))
                                                    .number_of_lines(1)
                                                    .text_size(px(12.0))
                                                    .text_color(move || {
                                                        if matches!(
                                                            store.playback_status.get(),
                                                            PlaybackStatus::Error(_)
                                                        ) {
                                                            theme::danger()
                                                        } else {
                                                            theme::muted_fg()
                                                        }
                                                    }),
                                                ),
                                        ),
                                ),
                        )
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap_0p5()
                                .flex_shrink_0()
                                .child(
                                    button()
                                        .test_id("phone-play-pause-btn")
                                        .accessibility_label(move || {
                                            Some(if is_loading.get() {
                                                store.t("player.buffering")
                                            } else if store.is_playing.get() {
                                                store.t("player.pause")
                                            } else {
                                                store.t("player.play")
                                            })
                                        })
                                        .on_press(move || store.toggle_play())
                                        .min_w(px(44.0))
                                        .min_h(px(44.0))
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .bg(colors::TRANSPARENT)
                                        .transition_transform()
                                        .duration(move || motion::transition_ms(140.0))
                                        .active(|s| s.scale(motion::press_scale))
                                        .focus_visible(|s| {
                                            s.outline(px(2.0)).outline_color(theme::ring)
                                        })
                                        .child(
                                            view()
                                                .size(px(36.0))
                                                .rounded_full()
                                                .items_center()
                                                .justify_center()
                                                .bg(move || {
                                                    theme::mini_player_play_bg(
                                                        store.current_accent.get(),
                                                    )
                                                })
                                                .border_1()
                                                .border_color(move || {
                                                    theme::mini_player_accent(
                                                        store.current_accent.get(),
                                                    )
                                                    .opacity(0.18)
                                                })
                                                .transition_colors()
                                                .duration(move || motion::transition_ms(200.0))
                                                .pointer_events(PointerEvents::None)
                                                .child(
                                                    when(is_loading, move || {
                                                        loading_spinner(16.0, move || {
                                                            theme::mini_player_accent(
                                                                store.current_accent.get(),
                                                            )
                                                        })
                                                    })
                                                    .otherwise(move || {
                                                        view()
                                                            .size(px(19.0))
                                                            .items_center()
                                                            .justify_center()
                                                            .pointer_events(PointerEvents::None)
                                                            .child(
                                                                view()
                                                                    .absolute()
                                                                    .inset_0()
                                                                    .pl(px(1.5))
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .bind_opacity(
                                                                        &play_morph.interpolate(
                                                                            &[0.0, 1.0],
                                                                            &[1.0, 0.0],
                                                                        ),
                                                                    )
                                                                    .bind_scale(
                                                                        &play_morph.interpolate(
                                                                            &[0.0, 1.0],
                                                                            &[1.0, 0.72],
                                                                        ),
                                                                    )
                                                                    .child(icon_reactive(
                                                                        Icon::Play,
                                                                        17.0,
                                                                        move || {
                                                                            theme::mini_player_accent(
                                                                                store
                                                                                    .current_accent
                                                                                    .get(),
                                                                            )
                                                                        },
                                                                    )),
                                                            )
                                                            .child(
                                                                view()
                                                                    .absolute()
                                                                    .inset_0()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .bind_opacity(
                                                                        &play_morph.interpolate(
                                                                            &[0.0, 1.0],
                                                                            &[0.0, 1.0],
                                                                        ),
                                                                    )
                                                                    .bind_scale(
                                                                        &play_morph.interpolate(
                                                                            &[0.0, 1.0],
                                                                            &[0.72, 1.0],
                                                                        ),
                                                                    )
                                                                    .child(icon_reactive(
                                                                        Icon::Pause,
                                                                        17.0,
                                                                        move || {
                                                                            theme::mini_player_accent(
                                                                                store
                                                                                    .current_accent
                                                                                    .get(),
                                                                            )
                                                                        },
                                                                    )),
                                                            )
                                                    }),
                                                ),
                                        ),
                                )
                                .child(
                                    icon_button_tinted(
                                        Icon::Next,
                                        move || theme::fg().opacity(0.84),
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
                        .h(px(2.5))
                        .bg(move || theme::track_rail().opacity(0.65))
                        .child(
                            view()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(move || relative(buffered_ratio.get()))
                                .bg(move || theme::track_buffered().opacity(0.55)),
                        )
                        .child(
                            view()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(move || relative(progress_ratio.get()))
                                .bg(move || theme::mini_player_accent(store.current_accent.get())),
                        ),
                ),
            )
    })
}

fn phone_bottom_tabs(store: AppStore) -> impl IntoElement {
    when(
        move || !store.route.with(AppStore::is_phone_secondary_route),
        move || {
            view()
                .test_id("phone-tab-bar")
                .w_full()
                .flex_shrink_0()
                .flex_row()
                .items_center()
                .justify_around()
                .pt_1p5()
                .pb(move || px(safe_area().bottom.0 + 4.0))
                .pl(move || px(safe_area().left.0 + 8.0))
                .pr(move || px(safe_area().right.0 + 8.0))
                .bg(theme::card)
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
                            Route::Browse { .. }
                                | Route::Radio
                                | Route::Podcasts
                                | Route::ShowDetail { .. }
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
                    |r| {
                        matches!(
                            r,
                            Route::Settings | Route::SettingsDetail(_) | Route::Plugins
                        )
                    },
                    || Route::Settings,
                ))
        },
    )
}

fn phone_bottom_safe_inset(store: AppStore) -> impl IntoElement {
    let needs_spacer = computed(move || {
        store.route.with(AppStore::is_phone_secondary_route)
            && store.current_track.with(Option::is_none)
    });

    when(needs_spacer, move || {
        view()
            .w_full()
            .h(move || safe_area().bottom)
            .flex_shrink_0()
            .bg(theme::bg)
    })
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
        .on_press(move || store.switch_tab(make_route()))
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
        .child(icon_reactive(glyph, 21.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || store.t(label_key))
                .number_of_lines(1)
                .text_size(px(10.5))
                .font_weight(FontWeight::MEDIUM)
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

fn resolve_phone_route_title(store: AppStore) -> SharedString {
    match store.route.get() {
        Route::Browse { ref title, .. } if !title.is_empty() => SharedString::from(title.clone()),
        Route::PlaylistDetail { .. } => store
            .active_playlist
            .with(|p| p.as_ref().map(|pl| SharedString::from(pl.name.clone())))
            .unwrap_or_else(|| store.t("nav.playlists")),
        Route::AlbumDetail { .. } => store
            .active_album
            .with(|a| {
                a.as_ref()
                    .map(|al| SharedString::from(al.album.name.clone()))
            })
            .unwrap_or_else(|| store.t("nav.albums")),
        Route::ArtistDetail { .. } => store
            .active_artist
            .with(|a| {
                a.as_ref()
                    .map(|ar| SharedString::from(ar.artist.name.clone()))
            })
            .unwrap_or_else(|| store.t("nav.artists")),
        Route::ShowDetail { .. } => store
            .active_show
            .with(|s| {
                s.as_ref()
                    .map(|sh| SharedString::from(sh.show.name.clone()))
            })
            .unwrap_or_else(|| store.t("nav.podcasts")),
        Route::SettingsDetail(section) => store.t(section.title_key()),
        other => store.t(other.title_key()),
    }
}
