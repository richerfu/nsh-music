//! Pad / Tablet application shell:
//! - Landscape: `76pt` left icon rail + expandable drawer (`Sheet`) + `72pt` floating rounded player dock
//! - Portrait: slide-over drawer (`Sheet`) + top source bar + `72pt` floating rounded player dock
//! - Capability-gated `Radio`, `Podcasts`, `Liked`, `Recent` + active source custom `info.navigation`
//! - `Back` & `Forward` history buttons on the top bar
//! - Quiet idle player dock copy (`player.idle_title`), all touch targets `>= 44pt`

use nsh_base::{SheetEvent, button, sheet};
use nsh_core::prelude::*;
use nsh_music_audio::PlaybackStatus;
use nsh_music_core::model::{NavItem, Playlist};

use crate::{
    pages,
    state::{ActiveModal, AppStore, NowPlayingPanel, Route},
    ui::{
        controls::{
            artwork_box, heart_button, icon_button, icon_button_tinted,
            play_pause_button_buffering, source_pill,
        },
        icons::{Icon, icon, icon_reactive},
        loading_spinner, motion,
        theme::{self, DeviceForm, Elevated, Elevation, colors, font_size, radius},
    },
};

const PAD_RAIL_WIDTH: f32 = 76.0;
const PAD_DOCK_HEIGHT: f32 = 72.0;

/// Renders the Pad shell in either Landscape or Portrait orientation.
pub fn pad_shell(store: AppStore, landscape: bool) -> View {
    on_back_press(move || {
        if !matches!(store.active_modal.get_untracked(), ActiveModal::None) {
            store.close_modal();
            true
        } else if store.mobile_drawer_open.get_untracked() {
            store.set_mobile_drawer_open(false);
            true
        } else if store.now_playing_open.get_untracked() {
            store.close_now_playing();
            true
        } else {
            store.go_back()
        }
    });

    let form = DeviceForm::Pad { landscape };

    view()
        .test_id(if landscape {
            "shell-pad-landscape"
        } else {
            "shell-pad-portrait"
        })
        .flex_1()
        .w_full()
        .h_full()
        .bg(theme::bg)
        .text_color(theme::fg)
        .child(if landscape {
            pad_landscape_body(store, form)
        } else {
            pad_portrait_body(store, form)
        })
        .child(pad_drawer_sheet(store))
}

fn pad_landscape_body(store: AppStore, form: DeviceForm) -> View {
    view()
        .flex_1()
        .w_full()
        .h_full()
        .flex_row()
        .child(pad_left_rail(store))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .child(pad_top_bar(store, false))
                .child(pad_main_viewport(store, form))
                .child(pad_floating_player_dock(store, true)),
        )
}

fn pad_portrait_body(store: AppStore, form: DeviceForm) -> View {
    view()
        .flex_1()
        .w_full()
        .h_full()
        .child(pad_top_bar(store, true))
        .child(pad_main_viewport(store, form))
        .child(pad_floating_player_dock(store, false))
}

fn pad_left_rail(store: AppStore) -> View {
    let custom_nav = computed(move || active_custom_nav(store));

    view()
        .test_id("pad-icon-rail")
        .w(move || px(PAD_RAIL_WIDTH + safe_area().left.0))
        .flex_shrink_0()
        .h_full()
        .pt(move || px(safe_area().top.0 + 10.0))
        .pb(move || px(safe_area().bottom.0 + 12.0))
        .pl(move || px(safe_area().left.0 + 8.0))
        .pr_2()
        .items_center()
        .justify_between()
        .bg(theme::sidebar)
        .border_r_1()
        .border_color(theme::sidebar_border)
        .child(
            scroll().flex_1().min_h(px(0.0)).w_full().child(
                view()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .pb_2()
                    .child(
                        icon_button(
                            Icon::Menu,
                            move || store.t("nav.expand_sidebar"),
                            move || store.set_mobile_drawer_open(true),
                        )
                        .test_id("pad-rail-drawer-btn"),
                    )
                    .child(pad_rail_item(
                        store,
                        Icon::Home,
                        "nav.home",
                        Route::Home,
                        "pad-nav-home",
                    ))
                    .child(when(
                        move || store.active_can("browse"),
                        move || {
                            pad_rail_item(
                                store,
                                Icon::Compass,
                                "nav.browse",
                                Route::Browse {
                                    page: String::new(),
                                    title: String::new(),
                                },
                                "pad-nav-browse",
                            )
                        },
                    ))
                    .child(when(
                        move || store.active_can("stations") || store.active_can("radio"),
                        move || {
                            pad_rail_item(
                                store,
                                Icon::Radio,
                                "nav.radio",
                                Route::Radio,
                                "pad-nav-radio",
                            )
                        },
                    ))
                    .child(when(
                        move || store.active_can("shows") || store.active_can("podcasts"),
                        move || {
                            pad_rail_item(
                                store,
                                Icon::Podcast,
                                "nav.podcasts",
                                Route::Podcasts,
                                "pad-nav-podcasts",
                            )
                        },
                    ))
                    .child(each(
                        custom_nav,
                        |item: &NavItem| format!("{}:{}", item.page, item.title),
                        move |item, _| pad_rail_custom_item(store, item),
                    ))
                    .child(pad_rail_item(
                        store,
                        Icon::Library,
                        "nav.library",
                        Route::Library,
                        "pad-nav-library",
                    ))
                    .child(pad_rail_item(
                        store,
                        Icon::Search,
                        "nav.search",
                        Route::Search,
                        "pad-nav-search",
                    )),
            ),
        )
        .child(
            view()
                .w_full()
                .pt_2()
                .items_center()
                .gap_1p5()
                .flex_shrink_0()
                .border_t_1()
                .border_color(theme::sidebar_border)
                .child(pad_rail_item(
                    store,
                    Icon::Settings,
                    "nav.settings",
                    Route::Settings,
                    "pad-nav-settings",
                )),
        )
}

fn pad_rail_item(
    store: AppStore,
    glyph: Icon,
    label_key: &'static str,
    target: Route,
    test_id: &'static str,
) -> impl IntoElement {
    let target_check = target.clone();
    let active = computed(move || {
        let current = store.route.get();
        let custom = active_custom_nav(store);
        pad_route_matches(&current, &target_check, &custom)
    });

    button()
        .test_id(test_id)
        .selected(active)
        .accessibility_label(move || Some(store.t(label_key)))
        .on_press(move || store.navigate(target.clone()))
        .w_full()
        .min_w(px(56.0))
        .min_h(px(52.0))
        .py_1p5()
        .px_1()
        .items_center()
        .justify_center()
        .gap_1()
        .rounded(px(radius::CONTROL))
        .bg(move || {
            if active.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
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
                .text_size(px(10.0))
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

fn pad_rail_custom_item(store: AppStore, item: ReadSignal<NavItem>) -> impl IntoElement {
    let page = item.with_untracked(|i| i.page.clone());
    let title = item.with_untracked(|i| i.title.clone());
    let check_page = page.clone();
    let active = computed(move || {
        matches!(
            store.route.get(),
            Route::Browse { page: ref p, .. } if p == &check_page
        )
    });

    button()
        .selected(active)
        .accessibility_label(move || Some(item.with(|i| SharedString::from(i.title.clone()))))
        .on_press(move || {
            store.navigate(Route::Browse {
                page: page.clone(),
                title: title.clone(),
            });
        })
        .w_full()
        .min_w(px(56.0))
        .min_h(px(52.0))
        .py_1p5()
        .px_1()
        .items_center()
        .justify_center()
        .gap_1()
        .rounded(px(radius::CONTROL))
        .bg(move || {
            if active.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(Icon::Compass, 20.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || item.with(|i| SharedString::from(i.title.clone())))
                .number_of_lines(1)
                .text_size(px(10.0))
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

fn pad_top_bar(store: AppStore, show_drawer_btn: bool) -> View {
    view()
        .test_id("pad-top-bar")
        .w_full()
        .flex_shrink_0()
        .pt(move || px(safe_area().top.0 + 8.0))
        .pb_2()
        .pl(move || {
            if show_drawer_btn {
                px(safe_area().left.0 + 16.0)
            } else {
                px(16.0)
            }
        })
        .pr(move || px(safe_area().right.0 + 16.0))
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .bg(move || {
            if store.page_scrolled.get() {
                theme::card()
            } else {
                theme::bg()
            }
        })
        .border_b_1()
        .border_color(theme::border)
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1p5()
                .flex_1()
                .min_w(px(0.0))
                .children(show_drawer_btn.then(|| {
                    icon_button(
                        Icon::Menu,
                        move || store.t("nav.expand_sidebar"),
                        move || store.set_mobile_drawer_open(true),
                    )
                    .test_id("pad-portrait-drawer-btn")
                }))
                .child(when(
                    move || store.can_go_back() || store.can_go_forward(),
                    move || {
                        view()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                icon_button_tinted(
                                    Icon::ChevronLeft,
                                    move || {
                                        if store.can_go_back() {
                                            theme::fg()
                                        } else {
                                            theme::faint_fg()
                                        }
                                    },
                                    move || store.t("common.back"),
                                    move || {
                                        let _ = store.go_back();
                                    },
                                )
                                .disabled(move || !store.can_go_back())
                                .test_id("pad-back-btn"),
                            )
                            .child(
                                icon_button_tinted(
                                    Icon::ChevronRight,
                                    move || {
                                        if store.can_go_forward() {
                                            theme::fg()
                                        } else {
                                            theme::faint_fg()
                                        }
                                    },
                                    move || store.t("common.forward"),
                                    move || {
                                        let _ = store.go_forward();
                                    },
                                )
                                .disabled(move || !store.can_go_forward())
                                .test_id("pad-forward-btn"),
                            )
                    },
                ))
                .child(
                    source_pill(
                        move || store.active_source_name(),
                        true,
                        move || {
                            store.open_modal(ActiveModal::SourceSwitcher);
                        },
                    )
                    .min_w(px(44.0))
                    .min_h(px(44.0))
                    .test_id("pad-source-switcher"),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1()
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
                    .test_id("pad-search-btn"),
                )
                .child(
                    icon_button_tinted(
                        Icon::Settings,
                        move || {
                            if matches!(
                                store.route.get(),
                                Route::Settings | Route::SettingsDetail(_)
                            ) {
                                theme::primary()
                            } else {
                                theme::fg()
                            }
                        },
                        move || store.t("nav.settings"),
                        move || store.navigate(Route::Settings),
                    )
                    .test_id("pad-settings-btn"),
                ),
        )
}

fn pad_main_viewport(store: AppStore, form: DeviceForm) -> View {
    view()
        .test_id("pad-main-viewport")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .child(pages::render_route(store, form))
}

fn pad_floating_player_dock(store: AppStore, has_left_rail: bool) -> View {
    let track_seed = computed(move || {
        store.current_track.with(|t| match t {
            Some(tr) if !tr.album.id.is_empty() => SharedString::from(tr.album.id.clone()),
            Some(tr) => SharedString::from(tr.id.clone()),
            None => SharedString::from("nsh-music-idle"),
        })
    });

    let track_liked = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(|tr| store.is_liked(&tr.id)))
    });

    let is_loading = computed(move || store.playback_status.with(PlaybackStatus::is_loading));

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
        .pl(move || {
            if has_left_rail {
                px(16.0)
            } else {
                px(safe_area().left.0 + 16.0)
            }
        })
        .pr(move || px(safe_area().right.0 + 16.0))
        .pt_1()
        .pb(move || px(safe_area().bottom.0 + 12.0))
        .flex_shrink_0()
        .child(
            Elevated::elevation(
                view()
                    .test_id("pad-player-dock")
                    .w_full()
                    .h(px(PAD_DOCK_HEIGHT))
                    .rounded(px(radius::XL))
                    .overflow_hidden()
                    .bg(move || theme::mini_player_surface(store.current_accent.get()))
                    .backdrop_blur(px(20.0))
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
                    .px_3p5()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    // Tappable artwork + metadata.
                    .child(
                        button()
                            .test_id("pad-now-playing-trigger")
                            .accessibility_label(move || Some(store.t("now_playing.expand")))
                            .on_press(move || store.open_now_playing())
                            .flex_1()
                            .min_w(px(44.0))
                            .min_h(px(48.0))
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .rounded(px(radius::CONTROL))
                            .active(|s| s.opacity(0.8))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .child(
                                Elevated::elevation(
                                    view()
                                        .size(px(48.0))
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
                                            48.0,
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
                                        .text_size(px(font_size::BODY_MOBILE))
                                        .font_weight(FontWeight::SEMIBOLD)
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
                                                text(move || match store.playback_status.get() {
                                                    PlaybackStatus::Loading => {
                                                        store.t("player.buffering")
                                                    }
                                                    PlaybackStatus::Error(msg) => {
                                                        SharedString::from(msg)
                                                    }
                                                    _ => store.current_track.with(|t| match t {
                                                        Some(tr) => {
                                                            SharedString::from(tr.artist_names())
                                                        }
                                                        None => SharedString::default(),
                                                    }),
                                                })
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .number_of_lines(1)
                                                .text_size(px(font_size::CAPTION))
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
                    // Touch-friendly (>= 44pt) transport controls.
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .child(heart_button(
                                track_liked,
                                move || {
                                    if track_liked.get() {
                                        store.t("player.unlike")
                                    } else {
                                        store.t("player.like")
                                    }
                                },
                                move || store.toggle_like_current(),
                            ))
                            .child(
                                icon_button(
                                    Icon::Prev,
                                    move || store.t("player.prev"),
                                    move || store.prev_track(),
                                )
                                .test_id("pad-prev-btn"),
                            )
                            .child(play_pause_button_buffering(
                                store.is_playing,
                                is_loading,
                                44.0,
                                move || {
                                    if is_loading.get() {
                                        store.t("player.buffering")
                                    } else if store.is_playing.get() {
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
                                .test_id("pad-next-btn"),
                            )
                            .child(
                                icon_button_tinted(
                                    Icon::Queue,
                                    theme::muted_fg,
                                    move || store.t("player.queue"),
                                    move || {
                                        store.set_now_playing_panel(NowPlayingPanel::Queue);
                                        store.open_now_playing();
                                    },
                                )
                                .test_id("pad-queue-btn"),
                            ),
                    ),
            )
            // Full-bleed bottom progress hairline with buffered + artwork-accent fills.
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
}

fn pad_drawer_sheet(store: AppStore) -> impl IntoElement {
    let custom_nav = computed(move || active_custom_nav(store));

    sheet()
        .open(move || Some(store.mobile_drawer_open.get()))
        .side(Placement::Left)
        .close_on_escape(true)
        .close_on_backdrop_press(true)
        .accessibility_label(move || Some(store.t("app.name")))
        .on_open_change(move |e: &SheetEvent| {
            if let SheetEvent::OpenChange(open, _) = e {
                store.set_mobile_drawer_open(*open);
            }
        })
        .backdrop_style(|s| s.bg(theme::scrim))
        .popup_style(|s| {
            s.w(move || px(284.0 + safe_area().left.0))
                .h_full()
                .bg(theme::sidebar)
                .border_r_1()
                .border_color(theme::sidebar_border)
                .elevation(Elevation::Modal)
        })
        .child(
            view()
                .flex_1()
                .w_full()
                .pt(move || px(safe_area().top.0 + 16.0))
                .pb(move || px(safe_area().bottom.0 + 16.0))
                .pl(move || px(safe_area().left.0 + 12.0))
                .pr_3()
                .gap_3()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_1()
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap_2p5()
                                .child(
                                    view()
                                        .size(px(30.0))
                                        .rounded(px(radius::SM))
                                        .bg(theme::primary)
                                        .items_center()
                                        .justify_center()
                                        .child(icon(Icon::Disc, 17.0, colors::PRIMARY_FOREGROUND)),
                                )
                                .child(
                                    text(move || store.t("app.name"))
                                        .text_size(px(font_size::TITLE))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme::sidebar_fg),
                                ),
                        )
                        .child(icon_button(
                            Icon::Close,
                            move || store.t("common.close"),
                            move || store.set_mobile_drawer_open(false),
                        )),
                )
                .child(
                    source_pill(
                        move || store.active_source_name(),
                        true,
                        move || {
                            store.set_mobile_drawer_open(false);
                            store.open_modal(ActiveModal::SourceSwitcher);
                        },
                    )
                    .w_full()
                    .min_h(px(44.0)),
                )
                .child(
                    scroll().flex_1().min_h(px(0.0)).child(
                        view()
                            .w_full()
                            .gap_1()
                            .child(drawer_row(store, Icon::Home, "nav.home", Route::Home))
                            .child(when(
                                move || store.active_can("browse"),
                                move || {
                                    drawer_row(
                                        store,
                                        Icon::Compass,
                                        "nav.browse",
                                        Route::Browse {
                                            page: String::new(),
                                            title: String::new(),
                                        },
                                    )
                                },
                            ))
                            .child(when(
                                move || store.active_can("stations") || store.active_can("radio"),
                                move || drawer_row(store, Icon::Radio, "nav.radio", Route::Radio),
                            ))
                            .child(when(
                                move || store.active_can("shows") || store.active_can("podcasts"),
                                move || {
                                    drawer_row(
                                        store,
                                        Icon::Podcast,
                                        "nav.podcasts",
                                        Route::Podcasts,
                                    )
                                },
                            ))
                            .child(each(
                                custom_nav,
                                |item: &NavItem| format!("{}:{}", item.page, item.title),
                                move |item, _| drawer_custom_row(store, item),
                            ))
                            .child(drawer_row(store, Icon::Search, "nav.search", Route::Search))
                            .child(drawer_row(
                                store,
                                Icon::Library,
                                "nav.library",
                                Route::Library,
                            ))
                            .child(when(
                                move || {
                                    store.active_can("liked")
                                        || store.active_source_entry().is_none_or(|e| e.is_local)
                                },
                                move || drawer_row(store, Icon::Heart, "nav.liked", Route::Liked),
                            ))
                            .child(when(
                                move || store.active_can("recent"),
                                move || drawer_row(store, Icon::Disc, "nav.recent", Route::Recent),
                            ))
                            .child(drawer_row(store, Icon::Folder, "nav.local", Route::Local))
                            // Playlists header + items inside drawer.
                            .child(
                                view()
                                    .w_full()
                                    .pt_3()
                                    .px_2()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        text(move || store.t("nav.playlists"))
                                            .text_size(px(font_size::MICRO))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .letter_spacing(px(0.5))
                                            .text_color(theme::faint_fg),
                                    )
                                    .child(when(
                                        move || store.active_can("createPlaylist"),
                                        move || {
                                            icon_button(
                                                Icon::Plus,
                                                move || store.t("library.create_playlist"),
                                                move || {
                                                    store.set_mobile_drawer_open(false);
                                                    store.open_modal(ActiveModal::CreatePlaylist {
                                                        initial_track_id: None,
                                                    });
                                                },
                                            )
                                        },
                                    )),
                            )
                            .child(each(
                                store.playlists,
                                |pl: &Playlist| pl.id.clone(),
                                move |pl, _| {
                                    let pid = pl.with_untracked(|p| p.id.clone());
                                    let check_id = pid.clone();
                                    let active = computed(move || {
                                        matches!(
                                            store.route.get(),
                                            Route::PlaylistDetail { ref id } if id == &check_id
                                        )
                                    });
                                    button()
                                        .selected(active)
                                        .on_press(move || {
                                            store.navigate(Route::PlaylistDetail {
                                                id: pid.clone(),
                                            });
                                        })
                                        .w_full()
                                        .min_h(px(46.0))
                                        .px_3()
                                        .flex_row()
                                        .items_center()
                                        .gap_3()
                                        .rounded(px(radius::CONTROL))
                                        .bg(move || {
                                            if active.get() {
                                                theme::selected()
                                            } else {
                                                colors::TRANSPARENT
                                            }
                                        })
                                        .hover(|s| s.bg(theme::hover))
                                        .active(|s| s.bg(theme::press).scale(motion::press_scale))
                                        .focus_visible(|s| {
                                            s.outline(px(2.0)).outline_color(theme::ring)
                                        })
                                        .child(icon(Icon::Queue, 17.0, colors::MUTED_FOREGROUND))
                                        .child(
                                            text(move || {
                                                pl.with(|p| SharedString::from(p.name.clone()))
                                            })
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .number_of_lines(1)
                                            .text_size(px(font_size::BODY_MOBILE))
                                            .text_color(move || {
                                                if active.get() {
                                                    theme::primary()
                                                } else {
                                                    theme::sidebar_fg()
                                                }
                                            }),
                                        )
                                },
                            ))
                            .child(drawer_row(
                                store,
                                Icon::Settings,
                                "nav.settings",
                                Route::Settings,
                            )),
                    ),
                ),
        )
}

fn drawer_row(
    store: AppStore,
    glyph: Icon,
    label_key: &'static str,
    target: Route,
) -> impl IntoElement {
    let target_check = target.clone();
    let active = computed(move || {
        let current = store.route.get();
        let custom = active_custom_nav(store);
        pad_route_matches(&current, &target_check, &custom)
    });

    button()
        .selected(active)
        .on_press(move || store.navigate(target.clone()))
        .w_full()
        .min_w(px(44.0))
        .min_h(px(46.0))
        .px_3()
        .flex_row()
        .items_center()
        .gap_3()
        .rounded(px(radius::CONTROL))
        .bg(move || {
            if active.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, 19.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || store.t(label_key))
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(font_size::BODY_MOBILE))
                .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                .text_color(move || {
                    if active.get() {
                        theme::primary()
                    } else {
                        theme::sidebar_fg()
                    }
                }),
        )
}

fn drawer_custom_row(store: AppStore, item: ReadSignal<NavItem>) -> impl IntoElement {
    let page = item.with_untracked(|i| i.page.clone());
    let title = item.with_untracked(|i| i.title.clone());
    let check_page = page.clone();
    let active = computed(move || {
        matches!(
            store.route.get(),
            Route::Browse { page: ref p, .. } if p == &check_page
        )
    });

    button()
        .selected(active)
        .on_press(move || {
            store.navigate(Route::Browse {
                page: page.clone(),
                title: title.clone(),
            });
        })
        .w_full()
        .min_w(px(44.0))
        .min_h(px(46.0))
        .px_3()
        .flex_row()
        .items_center()
        .gap_3()
        .rounded(px(radius::CONTROL))
        .bg(move || {
            if active.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(Icon::Compass, 19.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || item.with(|i| SharedString::from(i.title.clone())))
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(font_size::BODY_MOBILE))
                .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                .text_color(move || {
                    if active.get() {
                        theme::primary()
                    } else {
                        theme::sidebar_fg()
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

fn pad_route_matches(current: &Route, target: &Route, custom_nav: &[NavItem]) -> bool {
    match (current, target) {
        (Route::Browse { page: a, .. }, Route::Browse { page: b, .. }) => {
            if b.is_empty() {
                a.is_empty() || !custom_nav.iter().any(|item| item.page == *a)
            } else {
                a == b
            }
        }
        (Route::Settings | Route::SettingsDetail(_), Route::Settings) => true,
        _ => current == target,
    }
}
