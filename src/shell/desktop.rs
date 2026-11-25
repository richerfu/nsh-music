//! Desktop application shell aligned to `egoist/mygo-music`:
//! - Hidden `44pt` title bar (`TitleBarStyle::Hidden`, `.window_drag(true)`)
//! - Full-height collapsible `232pt` / `68pt` sidebar under macOS traffic lights with:
//!   - Source switcher pill (`store.active_source_name()`)
//!   - Integrated `32pt` sidebar search box with live suggestions & recent searches popover
//!   - Capability-gated primary & library nav + active source custom `info.navigation` items
//!   - Collapsible `My Playlists` (with `+` & right-click Rename/Delete) & `Saved Playlists`
//!   - Bottom account row (`40pt`) + direct `Plugins` & `Settings` triggers
//! - `44pt` top navigation bar with sidebar toggle, Back/Forward, scroll-edge revealed route title
//!   & hairline border (`store.page_scrolled`), and theme toggle
//! - Persistent `84pt` frosted bottom player bar (`player-bar`) with quiet idle state,
//!   smoke-mode scrubber, lyrics/queue/volume/remote-devices/expand controls
//! - Global desktop keyboard shortcuts (`space`, `mod+k`, `mod+f`, `mod+[`, `mod+]`,
//!   `left`, `right`, `mod+1`, `mod+2`, `mod+3`, `escape`)

use nsh_base::{button, input};
use nsh_core::{
    desktop::{TitleBarOptions, TitleBarStyle, WindowAttributes},
    element::desktop::{current_window, title_bar},
    keymap::When,
    prelude::*,
};
use nsh_music_core::{
    model::{NavItem, Playlist, RepeatMode},
    settings::ThemeMode,
};

use crate::{
    pages,
    state::{ActiveModal, AppStore, NowPlayingPanel, Route},
    ui::{
        controls::{
            artwork_box, heart_button, icon_button_sized, play_pause_button, source_pill,
            styled_slider,
        },
        icons::{Icon, icon, icon_reactive},
        motion,
        overlays::{ContextMenuAction, confirm_destructive, styled_context_menu, styled_menu},
        scrubber::scrubber,
        theme::{self, DeviceForm, Elevated, Elevation, colors, font_size, radius},
    },
};

const TITLE_BAR_HEIGHT: f32 = 44.0;
const SIDEBAR_EXPANDED_W: f32 = 232.0;
const SIDEBAR_COLLAPSED_W: f32 = 68.0;
const PLAYER_BAR_HEIGHT: f32 = 84.0;
const SEEK_STEP_MS: u64 = 5_000;

/// Renders the full Desktop workspace shell.
pub fn desktop_shell(store: AppStore) -> View {
    configure_hidden_title_bar();
    let search_ref = element_ref();
    register_desktop_shortcuts(store, search_ref);

    // Refresh remote devices whenever the active source supports remote control.
    effect(move || {
        let _source = store.active_source_id.get();
        if store.active_can("remoteDevices") {
            untracked(|| store.refresh_remote_devices());
        }
    });

    view()
        .test_id("desktop-shell")
        .flex_1()
        .w_full()
        .h_full()
        .flex_row()
        .bg(theme::bg)
        .text_color(theme::fg)
        // Left full-height sidebar spanning from top edge to bottom edge.
        .child(desktop_sidebar(store, search_ref))
        // Right workspace column: top navigation bar, scrollable pages, and non-full-width player bar.
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .h_full()
                .child(desktop_top_bar(store))
                .child(desktop_main_viewport(store))
                .child(desktop_player_bar(store)),
        )
}

fn configure_hidden_title_bar() {
    let mut bar = TitleBarOptions::default();
    bar.style = TitleBarStyle::Hidden;
    bar.height = Some(px(TITLE_BAR_HEIGHT));
    let mut attrs = WindowAttributes::default();
    attrs.title_bar = Some(bar);
    current_window().set(attrs);
}

fn register_desktop_shortcuts(store: AppStore, search_ref: ElementRef) {
    // Space toggles play/pause when not editing text (`When::Auto`).
    on_key("space", move |_| {
        store.toggle_play();
    });

    // Cmd/Ctrl+K and Cmd/Ctrl+F navigate to Search and focus the sidebar search input.
    on_key_with(
        "mod+k",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            store.navigate(Route::Search);
            search_ref.focus();
        },
    );

    on_key_with(
        "mod+f",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            store.navigate(Route::Search);
            search_ref.focus();
        },
    );

    // History navigation: Cmd/Ctrl+[ and Cmd/Ctrl+].
    on_key_with(
        "mod+[",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            let _ = store.go_back();
        },
    );

    on_key_with(
        "mod+]",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            let _ = store.go_forward();
        },
    );

    // ArrowLeft / ArrowRight seek -/+5s when a track is loaded (and not editing text).
    on_key("left", move |_| {
        if store.current_track.with_untracked(Option::is_some) {
            let pos = store.position_ms.get_untracked();
            store.seek(pos.saturating_sub(SEEK_STEP_MS));
        }
    });

    on_key("right", move |_| {
        if store.current_track.with_untracked(Option::is_some) {
            let pos = store.position_ms.get_untracked();
            let dur = store.duration_ms.get_untracked();
            if dur > 0 {
                store.seek((pos + SEEK_STEP_MS).min(dur));
            } else {
                store.seek(pos + SEEK_STEP_MS);
            }
        }
    });

    // Cmd/Ctrl+1 / 2 / 3 switch Now Playing panel while overlay is open.
    on_key_with(
        "mod+1",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            if store.now_playing_open.get_untracked() {
                store.set_now_playing_panel(NowPlayingPanel::Lyrics);
            }
        },
    );

    on_key_with(
        "mod+2",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            if store.now_playing_open.get_untracked() {
                store.set_now_playing_panel(NowPlayingPanel::Queue);
            }
        },
    );

    on_key_with(
        "mod+3",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            if store.now_playing_open.get_untracked() && untracked(|| store.active_can("comments"))
            {
                store.set_now_playing_panel(NowPlayingPanel::Comments);
            }
        },
    );

    // Escape closes active modal first, then search suggestions, then Now Playing overlay.
    on_key_with(
        "escape",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            if !matches!(store.active_modal.get_untracked(), ActiveModal::None) {
                store.close_modal();
            } else if store.search_suggest_open.get_untracked() {
                store.search_suggest_open.set(false);
            } else if store.now_playing_open.get_untracked() {
                store.close_now_playing();
            }
        },
    );
}

fn desktop_top_bar(store: AppStore) -> View {
    let scrolled = store.page_scrolled;
    let route_title = computed(move || resolve_route_title(store));

    view()
        .test_id("desktop-title-bar")
        .w_full()
        .h(move || {
            let top = title_bar().top.0;
            px(if top > 0.0 {
                top.max(TITLE_BAR_HEIGHT)
            } else {
                TITLE_BAR_HEIGHT
            })
        })
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .pl_3()
        .pr(move || title_bar().right + px(14.0))
        .bg(move || {
            if scrolled.get() {
                theme::glass_bar()
            } else {
                theme::bg()
            }
        })
        .border_b_1()
        .border_color(move || {
            if scrolled.get() {
                theme::border()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(160.0))
        .window_drag(true)
        // Left: sidebar toggle + Back / Forward history controls.
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .window_drag(false)
                .child(
                    icon_button_sized(
                        Icon::Sidebar,
                        30.0,
                        16.0,
                        theme::muted_fg,
                        move || {
                            if store.sidebar_collapsed.get() {
                                store.t("nav.expand_sidebar")
                            } else {
                                store.t("nav.collapse_sidebar")
                            }
                        },
                        move || store.toggle_sidebar(),
                    )
                    .test_id("desktop-sidebar-toggle"),
                )
                .child(
                    icon_button_sized(
                        Icon::ChevronLeft,
                        30.0,
                        16.0,
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
                    .test_id("desktop-back-btn"),
                )
                .child(
                    icon_button_sized(
                        Icon::ChevronRight,
                        30.0,
                        16.0,
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
                    .test_id("desktop-forward-btn"),
                ),
        )
        // Center: scroll-edge revealed route title.
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .items_center()
                .justify_center()
                .pointer_events(PointerEvents::None)
                .child(
                    text(move || route_title.get())
                        .number_of_lines(1)
                        .text_size(px(font_size::BODY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::fg)
                        .opacity(move || if scrolled.get() { 1.0_f32 } else { 0.0_f32 })
                        .transition_all()
                        .duration(move || motion::transition_ms(160.0)),
                ),
        )
        // Right: Theme cycle toggle.
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .window_drag(false)
                .child(
                    icon_button_sized(
                        Icon::Sparkles,
                        30.0,
                        16.0,
                        move || match store.theme_override.get() {
                            ThemeMode::System => theme::muted_fg(),
                            ThemeMode::Light => theme::amber(),
                            ThemeMode::Dark => theme::primary(),
                        },
                        move || store.t("settings.theme"),
                        move || {
                            let next = match store.theme_override.get_untracked() {
                                ThemeMode::System => {
                                    if untracked(dark) {
                                        ThemeMode::Light
                                    } else {
                                        ThemeMode::Dark
                                    }
                                }
                                ThemeMode::Light => ThemeMode::Dark,
                                ThemeMode::Dark => ThemeMode::System,
                            };
                            store.set_theme(next);
                        },
                    )
                    .test_id("desktop-theme-btn"),
                ),
        )
}

fn desktop_sidebar(store: AppStore, search_ref: ElementRef) -> View {
    let collapsed = store.sidebar_collapsed;

    view()
        .test_id("desktop-sidebar")
        .w(move || {
            if collapsed.get() {
                px(SIDEBAR_COLLAPSED_W)
            } else {
                px(SIDEBAR_EXPANDED_W)
            }
        })
        .flex_shrink_0()
        .h_full()
        .bg(theme::sidebar)
        .border_r_1()
        .border_color(theme::sidebar_border)
        .transition_all()
        .duration(move || motion::transition_ms(240.0))
        // Top drag clearance under macOS traffic lights (`title_bar().top.0.max(12.0)`).
        .child(
            view()
                .w_full()
                .h(move || px(title_bar().top.0.max(12.0)))
                .flex_shrink_0()
                .window_drag(true),
        )
        // Source switcher + integrated sidebar search box.
        .child(
            view()
                .w_full()
                .px_2p5()
                .pb_2()
                .gap_2()
                .flex_shrink_0()
                .child(
                    when(
                        move || !collapsed.get(),
                        move || {
                            source_pill(move || store.active_source_name(), true, move || {
                                store.open_modal(ActiveModal::SourceSwitcher);
                            })
                            .w_full()
                            .test_id("desktop-source-switcher")
                        },
                    )
                    .otherwise(move || {
                        view().w_full().items_center().child(
                            icon_button_sized(
                                Icon::Disc,
                                36.0,
                                18.0,
                                theme::primary,
                                move || store.active_source_name(),
                                move || store.open_modal(ActiveModal::SourceSwitcher),
                            )
                            .test_id("desktop-source-switcher"),
                        )
                    }),
                )
                .child(sidebar_search_area(store, search_ref, collapsed)),
        )
        // Scrollable capability-gated navigation & playlist sections.
        .child(
            scroll().flex_1().min_h(px(0.0)).child(
                view()
                    .w_full()
                    .px_2p5()
                    .pb_4()
                    .gap_0p5()
                    // Primary discovery navigation (`h(px(34.0))`).
                    .child(sidebar_nav_item(
                        store,
                        Icon::Home,
                        "nav.home",
                        Route::Home,
                        "nav-home",
                    ))
                    .child(when(
                        move || store.active_can("browse"),
                        move || {
                            sidebar_nav_item(
                                store,
                                Icon::Compass,
                                "nav.browse",
                                Route::Browse {
                                    page: String::new(),
                                    title: String::new(),
                                },
                                "nav-browse",
                            )
                        },
                    ))
                    .child(when(
                        move || store.active_can("stations") || store.active_can("radio"),
                        move || {
                            sidebar_nav_item(
                                store,
                                Icon::Radio,
                                "nav.radio",
                                Route::Radio,
                                "nav-radio",
                            )
                        },
                    ))
                    .child(when(
                        move || store.active_can("shows") || store.active_can("podcasts"),
                        move || {
                            sidebar_nav_item(
                                store,
                                Icon::Podcast,
                                "nav.podcasts",
                                Route::Podcasts,
                                "nav-podcasts",
                            )
                        },
                    ))
                    // Custom active source `info.navigation` items.
                    .child(when(
                        move || !collapsed.get(),
                        move || desktop_plugin_nav_section(store),
                    ))
                    // Library section.
                    .child(sidebar_section_label(store, "nav.library", collapsed))
                    .child(when(
                        move || {
                            store.active_can("liked")
                                || store.active_source_entry().is_none_or(|e| e.is_local)
                        },
                        move || {
                            sidebar_nav_item(
                                store,
                                Icon::Heart,
                                "nav.liked",
                                Route::Liked,
                                "nav-liked",
                            )
                        },
                    ))
                    .child(when(
                        move || store.active_can("recent"),
                        move || {
                            sidebar_nav_item(
                                store,
                                Icon::Disc,
                                "nav.recent",
                                Route::Recent,
                                "nav-recent",
                            )
                        },
                    ))
                    .child(sidebar_nav_item(
                        store,
                        Icon::Folder,
                        "nav.local",
                        Route::Local,
                        "nav-local",
                    ))
                    // Collapsible My Playlists & Saved Playlists when expanded.
                    .child(when(
                        move || !collapsed.get(),
                        move || desktop_playlists_sections(store),
                    )),
            ),
        )
        // Bottom account row + direct Plugins & Settings triggers.
        .child(desktop_sidebar_footer(store, collapsed))
}

fn sidebar_search_area(
    store: AppStore,
    search_ref: ElementRef,
    collapsed: Signal<bool>,
) -> impl IntoElement {
    when(
        move || !collapsed.get(),
        move || {
            let has_query = computed(move || store.search_query.with(|q| !q.is_empty()));
            let has_suggestions = computed(move || {
                store.search_suggestions.with(|v| !v.is_empty())
                    || store.recent_searches.with(|v| !v.is_empty())
            });

            view()
                .w_full()
                .gap_1p5()
                .child(
                    view()
                        .w_full()
                        .h(px(32.0))
                        .px_2()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .rounded(px(9.0))
                        .bg(theme::card)
                        .border_1()
                        .border_color(theme::input_border)
                        .focus_within(|s| s.border_color(theme::ring))
                        .child(
                            button()
                                .test_id("nav-search")
                                .accessibility_label(move || Some(store.t("nav.search")))
                                .on_press(move || {
                                    store.navigate(Route::Search);
                                    search_ref.focus();
                                })
                                .size(px(22.0))
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .rounded(px(radius::SM))
                                .hover(|s| s.bg(theme::hover))
                                .active(|s| s.scale(motion::press_scale))
                                .child(icon(Icon::Search, 14.0, colors::MUTED_FOREGROUND)),
                        )
                        .child(
                            input()
                                .element_ref(&search_ref)
                                .test_id("desktop-search-input")
                                .value(move || store.search_query.get())
                                .on_change(move |q: &str| {
                                    store.update_search_query(q.to_owned());
                                })
                                .on_focus(move || {
                                    if !store.search_suggestions.with_untracked(Vec::is_empty)
                                        || !store.recent_searches.with_untracked(Vec::is_empty)
                                    {
                                        store.search_suggest_open.set(true);
                                    }
                                })
                                .on_submit(move |q: &str| {
                                    store.search(q);
                                    store.navigate(Route::Search);
                                })
                                .placeholder(move || store.t("nav.search").to_string())
                                .placeholder_color(|| color(theme::muted_fg()))
                                .caret_color(|| color(theme::primary()))
                                .selection_color(|| color(theme::primary_soft()))
                                .return_key_type(ReturnKeyType::Search)
                                .flex_1()
                                .min_w(px(0.0))
                                .h_full()
                                .text_size(px(font_size::SUBTITLE))
                                .text_color(theme::fg),
                        )
                        .child(when(has_query, move || {
                            icon_button_sized(
                                Icon::Close,
                                20.0,
                                12.0,
                                theme::muted_fg,
                                move || store.t("common.close"),
                                move || store.update_search_query(String::new()),
                            )
                            .test_id("desktop-search-clear")
                        })),
                )
                .child(when(
                    move || store.search_suggest_open.get() && has_suggestions.get(),
                    move || sidebar_suggestions_popover(store),
                ))
        },
    )
    .otherwise(move || {
        sidebar_nav_item(store, Icon::Search, "nav.search", Route::Search, "nav-search")
    })
}

fn sidebar_suggestions_popover(store: AppStore) -> View {
    Elevated::elevation(
        view()
            .test_id("desktop-search-suggestions")
            .w_full()
            .p_1p5()
            .gap_1()
            .rounded(px(radius::CONTROL))
            .bg(theme::popover)
            .border_1()
            .border_color(theme::border),
        Elevation::Popover,
    )
    .child(when(
        move || store.search_suggestions.with(|v| !v.is_empty()),
        move || {
            view()
                .w_full()
                .gap_0p5()
                .child(
                    view()
                        .w_full()
                        .px_2()
                        .py_0p5()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            text(move || store.t("search.suggestions"))
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::faint_fg),
                        )
                        .child(icon_button_sized(
                            Icon::Close,
                            20.0,
                            11.0,
                            theme::muted_fg,
                            move || store.t("common.close"),
                            move || store.search_suggest_open.set(false),
                        )),
                )
                .child(each(
                    store.search_suggestions,
                    |s| s.clone(),
                    move |item, _| {
                        let term = item.get_untracked();
                        button()
                            .on_press(move || {
                                store.search(&term);
                                store.navigate(Route::Search);
                            })
                            .w_full()
                            .h(px(28.0))
                            .px_2()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .rounded(px(radius::SM))
                            .hover(|s| s.bg(theme::accent))
                            .active(|s| s.bg(theme::press))
                            .child(icon(Icon::Search, 12.0, colors::MUTED_FOREGROUND))
                            .child(
                                text(move || SharedString::from(item.get()))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::popover_fg),
                            )
                    },
                ))
        },
    ))
    .child(when(
        move || {
            store.search_suggestions.with(Vec::is_empty)
                && store.recent_searches.with(|v| !v.is_empty())
        },
        move || {
            view()
                .w_full()
                .gap_0p5()
                .child(
                    view()
                        .w_full()
                        .px_2()
                        .py_0p5()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            text(move || store.t("search.recent"))
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::faint_fg),
                        )
                        .child(
                            button()
                                .on_press(move || {
                                    store.clear_recent_searches();
                                    store.search_suggest_open.set(false);
                                })
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(radius::SM))
                                .hover(|s| s.bg(theme::hover))
                                .child(
                                    text(move || store.t("search.clear_recent"))
                                        .text_size(px(10.0))
                                        .text_color(theme::muted_fg),
                                ),
                        ),
                )
                .child(each(
                    store.recent_searches,
                    |s| s.clone(),
                    move |item, _| {
                        let term = item.get_untracked();
                        button()
                            .on_press(move || {
                                store.search(&term);
                                store.navigate(Route::Search);
                            })
                            .w_full()
                            .h(px(28.0))
                            .px_2()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .rounded(px(radius::SM))
                            .hover(|s| s.bg(theme::accent))
                            .active(|s| s.bg(theme::press))
                            .child(icon(Icon::Disc, 12.0, colors::MUTED_FOREGROUND))
                            .child(
                                text(move || SharedString::from(item.get()))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::popover_fg),
                            )
                    },
                ))
        },
    ))
}

fn sidebar_section_label(
    store: AppStore,
    key: &'static str,
    collapsed: Signal<bool>,
) -> impl IntoElement {
    when(
        move || !collapsed.get(),
        move || {
            text(move || store.t(key))
                .number_of_lines(1)
                .px_2p5()
                .pt_3()
                .pb_1()
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .letter_spacing(px(0.5))
                .text_color(theme::faint_fg)
        },
    )
}

fn sidebar_nav_item(
    store: AppStore,
    glyph: Icon,
    label_key: &'static str,
    target: Route,
    test_id: &'static str,
) -> impl IntoElement {
    let collapsed = store.sidebar_collapsed;
    let target_check = target.clone();
    let active = computed(move || route_matches(&store.route.get(), &target_check));

    button()
        .test_id(test_id)
        .selected(active)
        .accessibility_label(move || Some(store.t(label_key)))
        .on_press(move || store.navigate(target.clone()))
        .w_full()
        .h(px(34.0))
        .px_2p5()
        .flex_row()
        .items_center()
        .gap_2p5()
        .rounded(px(radius::CONTROL))
        .style_when(collapsed, |s| s.justify_center().px_0())
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
        // Leading coral indicator pill.
        .child(
            view()
                .absolute()
                .left_0()
                .top(px(7.0))
                .w(px(3.0))
                .h(px(20.0))
                .rounded_full()
                .bg(theme::primary)
                .opacity(move || if active.get() { 1.0_f32 } else { 0.0_f32 }),
        )
        .child(icon_reactive(glyph, 17.0, move || {
            if active.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(when(
            move || !collapsed.get(),
            move || {
                text(move || store.t(label_key))
                    .flex_1()
                    .min_w(px(0.0))
                    .number_of_lines(1)
                    .text_size(px(font_size::BODY))
                    .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                    .text_color(move || {
                        if active.get() {
                            theme::primary()
                        } else {
                            theme::sidebar_fg()
                        }
                    })
            },
        ))
}

fn desktop_plugin_nav_section(store: AppStore) -> impl IntoElement {
    let custom_items = computed(move || active_custom_nav(store));

    when(
        move || custom_items.with(|v| !v.is_empty()),
        move || {
            view().w_full().pt_1().gap_0p5().child(each(
                custom_items,
                |item| format!("{}:{}", item.page, item.title),
                move |item, _| {
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
                        .h(px(34.0))
                        .px_2p5()
                        .flex_row()
                        .items_center()
                        .gap_2p5()
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
                        .child(icon_reactive(Icon::Compass, 16.0, move || {
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
                                .text_size(px(font_size::BODY))
                                .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
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
        },
    )
}

fn desktop_playlists_sections(store: AppStore) -> View {
    let my_playlists = computed(move || {
        store
            .playlists
            .with(|list| list.iter().filter(|p| p.mine).cloned().collect::<Vec<_>>())
    });
    let saved_playlists = computed(move || {
        store
            .playlists
            .with(|list| list.iter().filter(|p| !p.mine).cloned().collect::<Vec<_>>())
    });

    let mine_collapsed = computed(move || store.settings.with(|s| s.sidebar_mine_collapsed));
    let saved_collapsed = computed(move || store.settings.with(|s| s.sidebar_saved_collapsed));

    view()
        .w_full()
        .pt_3()
        .gap_2()
        // Collapsible "My Playlists" section.
        .child(
            view()
                .w_full()
                .gap_0p5()
                .child(
                    view()
                        .w_full()
                        .px_2()
                        .pb_0p5()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            button()
                                .on_press(move || store.toggle_sidebar_mine())
                                .flex_1()
                                .min_w(px(0.0))
                                .h(px(24.0))
                                .px_1()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .rounded(px(radius::SM))
                                .hover(|s| s.bg(theme::hover))
                                .child(icon_reactive(
                                    move || {
                                        if mine_collapsed.get() {
                                            Icon::ChevronRight
                                        } else {
                                            Icon::ChevronDown
                                        }
                                    },
                                    11.0,
                                    theme::faint_fg,
                                ))
                                .child(
                                    text(move || store.t("nav.my_playlists"))
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .number_of_lines(1)
                                        .text_size(px(font_size::MICRO))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .letter_spacing(px(0.5))
                                        .text_color(theme::faint_fg),
                                ),
                        )
                        .child(when(
                            move || store.active_can("createPlaylist"),
                            move || {
                                icon_button_sized(
                                    Icon::Plus,
                                    24.0,
                                    13.0,
                                    theme::muted_fg,
                                    move || store.t("library.create_playlist"),
                                    move || {
                                        store.open_modal(ActiveModal::CreatePlaylist {
                                            initial_track_id: None,
                                        });
                                    },
                                )
                                .test_id("desktop-new-playlist-btn")
                            },
                        )),
                )
                .child(when(
                    move || !mine_collapsed.get(),
                    move || {
                        each(
                            my_playlists,
                            |pl: &Playlist| pl.id.clone(),
                            move |pl, _| sidebar_playlist_row(store, pl, true),
                        )
                    },
                )),
        )
        // Collapsible "Saved Playlists" section (shown when non-empty).
        .child(when(
            move || saved_playlists.with(|v| !v.is_empty()),
            move || {
                view()
                    .w_full()
                    .gap_0p5()
                    .child(
                        button()
                            .on_press(move || store.toggle_sidebar_saved())
                            .w_full()
                            .h(px(24.0))
                            .px_2p5()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .rounded(px(radius::SM))
                            .hover(|s| s.bg(theme::hover))
                            .child(icon_reactive(
                                move || {
                                    if saved_collapsed.get() {
                                        Icon::ChevronRight
                                    } else {
                                        Icon::ChevronDown
                                    }
                                },
                                11.0,
                                theme::faint_fg,
                            ))
                            .child(
                                text(move || store.t("nav.saved_playlists"))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::MICRO))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .letter_spacing(px(0.5))
                                    .text_color(theme::faint_fg),
                            ),
                    )
                    .child(when(
                        move || !saved_collapsed.get(),
                        move || {
                            each(
                                saved_playlists,
                                |pl: &Playlist| pl.id.clone(),
                                move |pl, _| sidebar_playlist_row(store, pl, false),
                            )
                        },
                    ))
            },
        ))
}

fn sidebar_playlist_row(
    store: AppStore,
    pl: ReadSignal<Playlist>,
    allow_manage: bool,
) -> AnyElement {
    let snapshot = pl.get_untracked();
    let pl_id = snapshot.id.clone();
    let check_id = pl_id.clone();
    let is_fixed = snapshot.fixed;
    let active = computed(move || {
        matches!(
            store.route.get(),
            Route::PlaylistDetail { ref id } if id == &check_id
        )
    });

    let nav_id = pl_id.clone();
    let row_btn = button()
        .selected(active)
        .on_press(move || {
            store.navigate(Route::PlaylistDetail { id: nav_id.clone() });
        })
        .w_full()
        .h(px(32.0))
        .px_2p5()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded(px(radius::SM))
        .bg(move || {
            if active.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon(Icon::Queue, 14.0, colors::MUTED_FOREGROUND))
        .child(
            text(move || pl.with(|p| SharedString::from(p.name.clone())))
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(font_size::SUBTITLE))
                .text_color(move || {
                    if active.get() {
                        theme::primary()
                    } else {
                        theme::muted_fg()
                    }
                }),
        );

    if !allow_manage || is_fixed {
        return row_btn.into_element();
    }

    let mut actions = Vec::new();
    if untracked(|| store.active_can("renamePlaylist")) {
        let rename_id = pl_id.clone();
        actions.push(ContextMenuAction::new(
            untracked(|| store.t("playlist.rename")),
            move || {
                let current_name = pl.with_untracked(|p| p.name.clone());
                store.open_modal(ActiveModal::RenamePlaylist {
                    id: rename_id.clone(),
                    current_name,
                });
            },
        ));
    }
    if untracked(|| store.active_can("deletePlaylist")) {
        let delete_id = pl_id;
        actions.push(
            ContextMenuAction::new(untracked(|| store.t("playlist.delete")), move || {
                let name = pl.with_untracked(|p| p.name.clone());
                let title = store.t_fmt("playlist.delete_confirm_title", &[("name", &name)]);
                let desc = store.t("playlist.delete_confirm_desc");
                let cancel = store.t("common.cancel");
                let confirm = store.t("common.delete");
                let id_to_del = delete_id.clone();
                confirm_destructive(title, desc, cancel, confirm, move || {
                    store.delete_playlist(&id_to_del);
                });
            })
            .icon(Icon::Trash)
            .destructive(true)
            .separator_before(),
        );
    }

    if actions.is_empty() {
        row_btn.into_element()
    } else {
        styled_context_menu(row_btn, actions).into_element()
    }
}

fn desktop_sidebar_footer(store: AppStore, collapsed: Signal<bool>) -> View {
    let can_account =
        computed(move || store.active_can("signIn") || store.active_can("account"));

    view()
        .w_full()
        .px_2p5()
        .py_2()
        .gap_1()
        .flex_shrink_0()
        .border_t_1()
        .border_color(theme::sidebar_border)
        .child(when(can_account, move || {
            desktop_account_row(store, collapsed)
        }))
        .child(sidebar_nav_item(
            store,
            Icon::Plugin,
            "nav.plugins",
            Route::Plugins,
            "nav-plugins",
        ))
        .child(sidebar_nav_item(
            store,
            Icon::Settings,
            "nav.settings",
            Route::Settings,
            "nav-settings",
        ))
}

fn desktop_account_row(store: AppStore, collapsed: Signal<bool>) -> View {
    let entry_memo = computed(move || {
        store
            .active_source_entry()
            .map(|e| (e.info.id, e.account))
    });

    view().w_full().child(dynamic(move || {
        let Some((source_id, account)) = entry_memo.get() else {
            return view().into_element();
        };
        let is_collapsed = collapsed.get();
        let _locale = store.locale.get();

        untracked(move || match account {
            Some(acc) => {
                let mut actions = Vec::new();
                if !acc.page.is_empty() {
                    let page = acc.page.clone();
                    let title = store.t("account.my_page").to_string();
                    actions.push(
                        ContextMenuAction::new(store.t("account.my_page"), move || {
                            store.navigate(Route::Browse {
                                page: page.clone(),
                                title: title.clone(),
                            });
                        })
                        .icon(Icon::User),
                    );
                }
                actions.push(
                    ContextMenuAction::new(store.t("nav.plugins"), move || {
                        store.navigate(Route::Plugins);
                    })
                    .icon(Icon::Plugin),
                );
                actions.push(
                    ContextMenuAction::new(store.t("nav.settings"), move || {
                        store.navigate(Route::Settings);
                    })
                    .icon(Icon::Settings),
                );
                if store.active_can("signOut") {
                    let sid = source_id.clone();
                    actions.push(
                        ContextMenuAction::new(store.t("nav.sign_out"), move || {
                            store.sign_out_source(&sid);
                        })
                        .destructive(true)
                        .separator_before(),
                    );
                }

                let avatar_seed = if acc.id.is_empty() {
                    acc.name.clone()
                } else {
                    acc.id.clone()
                };
                let trigger = view()
                    .test_id("desktop-account-row")
                    .w_full()
                    .h(px(40.0))
                    .px_2()
                    .flex_row()
                    .items_center()
                    .gap_2p5()
                    .when(is_collapsed, |v| v.justify_center().px_0())
                    .rounded(px(radius::CONTROL))
                    .hover(|s| s.bg(theme::hover))
                    .active(|s| s.bg(theme::press).scale(motion::press_scale))
                    .child(artwork_box(
                        SharedString::from(acc.image_url),
                        SharedString::from(avatar_seed),
                        26.0,
                        radius::PILL,
                    ))
                    .children((!is_collapsed).then(|| {
                        text(SharedString::from(acc.name))
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::sidebar_fg)
                    }));

                styled_menu(trigger, actions).into_element()
            }
            None => {
                let sid = source_id;
                button()
                    .test_id("desktop-account-row")
                    .accessibility_label(move || Some(store.t("nav.sign_in")))
                    .on_press(move || store.sign_in_source(&sid))
                    .w_full()
                    .h(px(40.0))
                    .px_2p5()
                    .flex_row()
                    .items_center()
                    .gap_2p5()
                    .when(is_collapsed, |v| v.justify_center().px_0())
                    .rounded(px(radius::CONTROL))
                    .bg(theme::secondary)
                    .hover(|s| s.bg(theme::accent))
                    .active(|s| s.bg(theme::press).scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(icon(Icon::User, 16.0, colors::PRIMARY))
                    .children((!is_collapsed).then(|| {
                        text(move || store.t("nav.sign_in"))
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::fg)
                    }))
                    .into_element()
            }
        })
    }))
}

fn desktop_main_viewport(store: AppStore) -> View {
    view()
        .test_id("desktop-main-viewport")
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .bg(theme::bg)
        .child(pages::render_route(store, DeviceForm::Desktop))
}

fn desktop_player_bar(store: AppStore) -> View {
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

    let buffered_ratio = move || {
        let dur = store.duration_ms.get();
        if dur == 0 {
            0.0_f32
        } else {
            (store.buffered_ms.get() as f32 / dur as f32).clamp(0.0, 1.0)
        }
    };

    view()
        .test_id("player-bar")
        .w_full()
        .h(px(PLAYER_BAR_HEIGHT))
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .px_5()
        .glass_bar_surface()
        .border_t_1()
        .border_color(theme::border)
        // Left: artwork, title/artist (opens Now Playing), like button.
        .child(
            view()
                .flex_1()
                .min_w(px(160.0))
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    button()
                        .test_id("desktop-now-playing-trigger")
                        .accessibility_label(move || Some(store.t("now_playing.expand")))
                        .on_press(move || store.open_now_playing())
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .p_1()
                        .rounded(px(radius::CONTROL))
                        .hover(|s| s.bg(theme::hover))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(dynamic(move || {
                            let seed = track_seed.get();
                            untracked(move || {
                                artwork_box(store.current_artwork_uri, seed, 52.0, radius::SM)
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
                                            Some(tr) => SharedString::from(tr.name.clone()),
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
                                            Some(tr) => SharedString::from(tr.artist_names()),
                                            None => SharedString::default(),
                                        })
                                    })
                                    .number_of_lines(1)
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::muted_fg),
                                ),
                        ),
                )
                .child(when(
                    move || store.current_track.with(|t| t.is_some()),
                    move || {
                        heart_button(
                            track_liked,
                            move || {
                                if track_liked.get() {
                                    store.t("player.unlike")
                                } else {
                                    store.t("player.like")
                                }
                            },
                            move || store.toggle_like_current(),
                        )
                    },
                )),
        )
        // Center: transport controls + smoke-mode scrubber.
        .child(
            view()
                .w(px(420.0))
                .max_w(px(480.0))
                .min_w(px(240.0))
                .py_1p5()
                .items_center()
                .justify_center()
                .gap_1p5()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            icon_button_sized(
                                Icon::Shuffle,
                                30.0,
                                15.0,
                                move || {
                                    if store.shuffle.get() {
                                        theme::primary()
                                    } else {
                                        theme::muted_fg()
                                    }
                                },
                                move || store.t("player.shuffle"),
                                move || store.toggle_shuffle(),
                            )
                            .test_id("desktop-shuffle-btn"),
                        )
                        .child(
                            icon_button_sized(
                                Icon::Prev,
                                32.0,
                                17.0,
                                theme::fg,
                                move || store.t("player.prev"),
                                move || store.prev_track(),
                            )
                            .test_id("desktop-prev-btn"),
                        )
                        .child(play_pause_button(
                            store.is_playing,
                            34.0,
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
                            icon_button_sized(
                                Icon::Next,
                                32.0,
                                17.0,
                                theme::fg,
                                move || store.t("player.next"),
                                move || store.next_track(),
                            )
                            .test_id("desktop-next-btn"),
                        )
                        .child(
                            icon_button_sized(
                                move || {
                                    if store.repeat.get() == RepeatMode::One {
                                        Icon::RepeatOne
                                    } else {
                                        Icon::Repeat
                                    }
                                },
                                30.0,
                                15.0,
                                move || {
                                    if store.repeat.get() == RepeatMode::Off {
                                        theme::muted_fg()
                                    } else {
                                        theme::primary()
                                    }
                                },
                                move || store.t("player.repeat"),
                                move || store.cycle_repeat(),
                            )
                            .test_id("desktop-repeat-btn"),
                        ),
                )
                .child(
                    scrubber(store.position_ms, store.duration_ms)
                        .buffered_ratio(buffered_ratio)
                        .smoke_mode(move || store.settings.with(|s| s.smoke_progress))
                        .playing(move || store.is_playing.get())
                        .accent(move || store.current_accent.get())
                        .spectrum(store.spectrum)
                        .show_labels(true)
                        .inline_labels(true)
                        .on_seek(move |ms| store.seek(ms)),
                ),
        )
        // Right: lyrics toggle, queue button, remote devices menu, volume slider, expand Now Playing.
        .child(
            view()
                .flex_1()
                .min_w(px(190.0))
                .flex_row()
                .items_center()
                .justify_end()
                .gap_1p5()
                .child(
                    icon_button_sized(
                        Icon::Lyrics,
                        30.0,
                        16.0,
                        move || {
                            if store.now_playing_open.get()
                                && store.now_playing_panel.get() == NowPlayingPanel::Lyrics
                            {
                                theme::primary()
                            } else {
                                theme::muted_fg()
                            }
                        },
                        move || store.t("now_playing.panel.lyrics"),
                        move || {
                            if store.now_playing_open.get_untracked()
                                && store.now_playing_panel.get_untracked()
                                    == NowPlayingPanel::Lyrics
                            {
                                store.close_now_playing();
                            } else {
                                store.set_now_playing_panel(NowPlayingPanel::Lyrics);
                                store.open_now_playing();
                            }
                        },
                    )
                    .test_id("desktop-lyrics-btn"),
                )
                .child(
                    icon_button_sized(
                        Icon::Queue,
                        30.0,
                        16.0,
                        move || {
                            if store.now_playing_open.get()
                                && store.now_playing_panel.get() == NowPlayingPanel::Queue
                            {
                                theme::primary()
                            } else {
                                theme::muted_fg()
                            }
                        },
                        move || store.t("player.queue"),
                        move || {
                            if store.now_playing_open.get_untracked()
                                && store.now_playing_panel.get_untracked()
                                    == NowPlayingPanel::Queue
                            {
                                store.close_now_playing();
                            } else {
                                store.set_now_playing_panel(NowPlayingPanel::Queue);
                                store.open_now_playing();
                            }
                        },
                    )
                    .test_id("desktop-queue-btn"),
                )
                .child(when(
                    move || store.active_can("remoteDevices"),
                    move || remote_devices_menu(store),
                ))
                .child(
                    icon_button_sized(
                        move || {
                            if store.muted.get() || store.volume.get() <= 0.001 {
                                Icon::VolumeMute
                            } else {
                                Icon::Volume
                            }
                        },
                        30.0,
                        16.0,
                        theme::muted_fg,
                        move || {
                            if store.muted.get() {
                                store.t("player.unmute")
                            } else {
                                store.t("player.mute")
                            }
                        },
                        move || store.toggle_mute(),
                    )
                    .test_id("desktop-mute-btn"),
                )
                .child(
                    view().w(px(76.0)).child(styled_slider(
                        move || {
                            if store.muted.get() {
                                0.0
                            } else {
                                f64::from(store.volume.get())
                            }
                        },
                        0.0,
                        1.0,
                        0.02,
                        move || store.t("player.volume"),
                        move |v| {
                            if store.muted.get_untracked() && v > 0.0 {
                                store.toggle_mute();
                            }
                            store.set_volume(v as f32);
                        },
                    )),
                )
                .child(
                    icon_button_sized(
                        Icon::Expand,
                        30.0,
                        15.0,
                        theme::muted_fg,
                        move || store.t("now_playing.expand"),
                        move || store.open_now_playing(),
                    )
                    .test_id("desktop-expand-btn"),
                ),
        )
}

fn remote_devices_menu(store: AppStore) -> View {
    view().child(dynamic(move || {
        let devices = store.remote_devices.get();
        let active_id = store.active_remote_device.get();
        let _locale = store.locale.get();

        untracked(move || {
            let mut actions = Vec::with_capacity(devices.len() + 2);
            let local_act = ContextMenuAction::new(store.t("player.local_device"), move || {
                store.select_remote_device(None);
            });
            actions.push(if active_id.is_none() {
                local_act.icon(Icon::Check)
            } else {
                local_act.icon(Icon::Volume)
            });

            for dev in devices {
                let is_active = active_id.as_deref() == Some(dev.id.as_str()) || dev.active;
                let dev_id = dev.id.clone();
                let act = ContextMenuAction::new(SharedString::from(dev.name), move || {
                    store.select_remote_device(Some(dev_id.clone()));
                });
                actions.push(if is_active {
                    act.icon(Icon::Check)
                } else {
                    act.icon(Icon::Cast)
                });
            }

            actions.push(
                ContextMenuAction::new(store.t("common.refresh"), move || {
                    store.refresh_remote_devices();
                })
                .icon(Icon::Refresh)
                .separator_before(),
            );

            let has_remote = active_id.is_some();
            let trigger = view()
                .test_id("desktop-remote-devices-btn")
                .label(store.t("player.devices"))
                .size(px(30.0))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_full()
                .hover(|s| s.bg(theme::hover))
                .active(|s| s.bg(theme::press).scale(motion::press_scale))
                .child(icon(
                    Icon::Cast,
                    16.0,
                    if has_remote {
                        colors::PRIMARY
                    } else {
                        colors::MUTED_FOREGROUND
                    },
                ));

            styled_menu(trigger, actions).into_element()
        })
    }))
}

fn resolve_route_title(store: AppStore) -> SharedString {
    match store.route.get() {
        Route::Browse { ref title, .. } if !title.is_empty() => SharedString::from(title.clone()),
        Route::PlaylistDetail { .. } => store
            .active_playlist
            .with(|p| p.as_ref().map(|pl| SharedString::from(pl.name.clone())))
            .unwrap_or_else(|| store.t("nav.playlists")),
        Route::AlbumDetail { .. } => store
            .active_album
            .with(|a| a.as_ref().map(|al| SharedString::from(al.album.name.clone())))
            .unwrap_or_else(|| store.t("nav.albums")),
        Route::ArtistDetail { .. } => store
            .active_artist
            .with(|a| a.as_ref().map(|ar| SharedString::from(ar.artist.name.clone())))
            .unwrap_or_else(|| store.t("nav.artists")),
        Route::ShowDetail { .. } => store
            .active_show
            .with(|s| s.as_ref().map(|sh| SharedString::from(sh.show.name.clone())))
            .unwrap_or_else(|| store.t("nav.podcasts")),
        other => store.t(other.title_key()),
    }
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

fn route_matches(current: &Route, target: &Route) -> bool {
    match (current, target) {
        (Route::Browse { page: a, .. }, Route::Browse { page: b, .. }) => {
            b.is_empty() || a == b
        }
        _ => current == target,
    }
}
