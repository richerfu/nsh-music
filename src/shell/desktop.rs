//! Desktop application shell aligned 1:1 to `egoist/mygo-music`:
//! - Hidden `44pt` title bar (`TitleBarStyle::Hidden`, `.window_drag(true)`)
//! - Full-height collapsible `240pt` / `68pt` sidebar under macOS traffic lights with:
//!   - Source switcher pill (`store.active_source_name()`)
//!   - Integrated `32pt` sidebar search box with floating overlay suggestions & recent searches popover
//!   - Capability-gated primary & library nav + active source custom `info.navigation` items
//!   - Collapsible `My Playlists` (with `+` & right-click Rename/Delete) & `Saved Playlists`
//!   - Bottom account row (`40pt`) + unified `Settings` trigger opening the Settings child window
//! - Native desktop application menu & system tray (`set_application_menu`, `set_tray`)
//! - `44pt` top navigation bar with sidebar toggle, Back/Forward, scroll-edge revealed route title
//!   & hairline border (`store.page_scrolled`), and theme toggle
//! - Persistent `84pt` right-column player bar (`player-bar`) with `48×48` hover-reveal cover,
//!   clickable artist links, buffering/error status, smoke-mode scrubber (paused when Now Playing
//!   is open), adaptive lyrics/episode toggle, queue, remote devices, volume, and expand controls
//! - Global desktop keyboard shortcuts (`space`, `mod+k`, `mod+f`, `mod+,`, `mod+[`, `mod+]`,
//!   `left`, `right`, `mod+1`..`mod+5`, `escape`)

use std::f32::consts::PI;

use nsh_base::{button, input};
use nsh_core::{
    desktop::{TitleBarOptions, TitleBarStyle, WindowAttributes, WindowKind},
    element::desktop::{current_window, title_bar, window as desktop_window},
    geometry::size,
    keymap::When,
    prelude::*,
};
use nsh_music_audio::PlaybackStatus;
use nsh_music_core::{
    model::{Artist, NavItem, Playlist, RepeatMode, Track},
    settings::ThemeMode,
};

use crate::{
    pages,
    state::{ActiveModal, AppStore, NowPlayingPanel, Route, SettingsSection},
    ui::{
        controls::{
            artwork_box, heart_button, icon_button_sized, play_pause_button_buffering, source_pill,
            styled_slider,
        },
        icons::{Icon, icon, icon_reactive},
        loading_spinner, motion,
        overlays::{
            ContextMenuAction, confirm_destructive, styled_context_menu, styled_menu, toast_overlay,
        },
        scrubber::scrubber,
        theme::{self, DeviceForm, Elevated, Elevation, colors, font_size, radius},
    },
};

const TITLE_BAR_HEIGHT: f32 = 44.0;
const SIDEBAR_EXPANDED_W: f32 = 240.0;
const SIDEBAR_COLLAPSED_W: f32 = 68.0;
const PLAYER_BAR_HEIGHT: f32 = 84.0;
const SEEK_STEP_MS: u64 = 5_000;

fn sidebar_top_clearance() -> f32 {
    let tb = title_bar();
    if tb.left.0 > 0.0 || tb.top.0 > 0.0 {
        tb.top.0.max(TITLE_BAR_HEIGHT) + 4.0
    } else {
        14.0
    }
}

/// Builds hidden-titlebar attributes matching `main`'s 44pt integrated header band.
pub fn main_window_attributes() -> WindowAttributes {
    let mut bar = TitleBarOptions::default();
    bar.style = TitleBarStyle::Hidden;
    bar.height = Some(px(TITLE_BAR_HEIGHT));
    let mut attrs = WindowAttributes::default();
    attrs.title_bar = Some(bar);
    attrs
}

/// Renders the Settings child window (`id = "settings"`, `parent = "main"`).
pub fn settings_window(store: AppStore) -> AnyElement {
    let settings_attrs = main_window_attributes();

    desktop_window()
        .id("settings")
        .parent("main")
        .kind(WindowKind::Normal)
        .default_size(size(px(780.0), px(640.0)))
        .restore_bounds(true)
        .title(move || store.t("settings.title").to_string())
        .attributes(settings_attrs)
        .open(store.settings_window_open)
        .on_open_change(move |e| {
            if !e.open {
                store.close_settings_window();
            }
        })
        .content(move || {
            let win = current_window();
            effect(move || {
                let seq = store.settings_window_focus_seq.get();
                if seq > 0 {
                    win.focus();
                }
            });
            view()
                .flex_1()
                .w_full()
                .h_full()
                .bg(theme::bg)
                .text_color(theme::fg)
                .child(pages::settings::settings_window_content(store))
                .child(pages::render_settings_modal(store))
                .child(toast_overlay(store.toasts))
        })
        .into_element()
}

/// Renders the full Desktop workspace shell.
pub fn desktop_shell(store: AppStore) -> View {
    configure_hidden_title_bar();
    let search_ref = element_ref();
    let sidebar_header_h = signal(84.0_f32);
    register_desktop_shortcuts(store, search_ref);

    // Refresh remote devices whenever the active source supports remote control.
    effect(move || {
        let _source = store.active_source_id.get();
        if store.active_can("remoteDevices") {
            untracked(|| store.refresh_remote_devices());
        }
    });

    let show_suggestions = computed(move || {
        !store.sidebar_collapsed.get()
            && store.search_suggest_open.get()
            && (store.search_suggestions.with(|v| !v.is_empty())
                || store.recent_searches.with(|v| !v.is_empty()))
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
        .child(desktop_sidebar(store, search_ref, sidebar_header_h))
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
        // Floating sidebar search suggestions popover mounted above navigation without shifting layout.
        .child(when(show_suggestions, move || {
            let popover_top = move || px(sidebar_top_clearance() + sidebar_header_h.get() - 2.0);
            view()
                .absolute()
                .inset_0()
                // Click-outside dismiss scrim covering navigation below the search box.
                .child(
                    view()
                        .test_id("desktop-search-suggestions-scrim")
                        .absolute()
                        .left_0()
                        .w(px(SIDEBAR_EXPANDED_W))
                        .top(popover_top)
                        .bottom_0()
                        .on_press_down(move |_: &PressEvent| {
                            store.search_suggest_open.set(false);
                        }),
                )
                // Click-outside dismiss scrim covering the right workspace column.
                .child(
                    view()
                        .absolute()
                        .left(px(SIDEBAR_EXPANDED_W))
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .on_press_down(move |_: &PressEvent| {
                            store.search_suggest_open.set(false);
                        }),
                )
                // Floating suggestions popover anchored cleanly beneath `desktop-search-input`.
                .child(
                    view()
                        .absolute()
                        .left(px(10.0))
                        .w(px(SIDEBAR_EXPANDED_W - 20.0))
                        .top(popover_top)
                        .child(sidebar_suggestions_popover(store)),
                )
        }))
}

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos"))
))]
fn resolve_tray_icon_src() -> nsh_core::element::ImageSrc {
    use std::path::{Path, PathBuf};

    use nsh_core::element::ImageSrc;

    let rel = Path::new("assets").join("tray-icon.svg");
    if rel.is_file() {
        return ImageSrc::from(rel.to_string_lossy().into_owned());
    }

    let manifest_icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/tray-icon.svg");
    if manifest_icon.is_file() {
        return ImageSrc::from(manifest_icon.to_string_lossy().into_owned());
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("assets/tray-icon.svg"),
            dir.join("../assets/tray-icon.svg"),
            dir.join("../Resources/tray-icon.svg"),
        ] {
            if candidate.is_file() {
                return ImageSrc::from(candidate.to_string_lossy().into_owned());
            }
        }
    }

    let bytes = include_bytes!("../../assets/tray-icon.svg");
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nsh-music");
    let cached = cache_dir.join("tray-icon.svg");
    let needs_write = std::fs::metadata(&cached)
        .map(|m| m.len() != bytes.len() as u64)
        .unwrap_or(true);
    if !needs_write
        || (std::fs::create_dir_all(&cache_dir).is_ok() && std::fs::write(&cached, bytes).is_ok())
    {
        return ImageSrc::from(cached.to_string_lossy().into_owned());
    }

    ImageSrc::svg_markup(include_str!("../../assets/tray-icon.svg"))
}

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos"))
))]
pub fn install_desktop_menu_and_tray(store: AppStore, show_main: std::rc::Rc<dyn Fn()>) {
    use nsh_core::{
        desktop::MenuRole,
        element::desktop::{MenuItem, quit, set_application_menu, set_tray, tray},
    };
    use nsh_music_core::i18n::Locale;

    // Native application menu reactive to locale and playback toggle state.
    effect(move || {
        let locale = store.locale.get();
        let playing = store.is_playing.get();
        let shuffle_on = store.shuffle.get();
        let repeat_mode = store.repeat.get();

        let play_label = if playing {
            store.t("player.pause").to_string()
        } else {
            store.t("player.play").to_string()
        };
        let repeat_label = match repeat_mode {
            RepeatMode::Off => store.t("player.repeat_off").to_string(),
            RepeatMode::All => store.t("player.repeat_all").to_string(),
            RepeatMode::One => store.t("player.repeat_one").to_string(),
        };
        let edit_menu_title = match locale {
            Locale::ZhCn => "编辑",
            Locale::En => "Edit",
        };

        let items = vec![
            MenuItem::submenu(
                store.t("menu.app").to_string(),
                [
                    MenuItem::new(store.t("menu.about").to_string()).on_select(move || {
                        store.open_settings_window(SettingsSection::About);
                    }),
                    MenuItem::separator(),
                    MenuItem::new(store.t("menu.settings").to_string())
                        .shortcut("mod+,")
                        .on_select(move || {
                            store.open_settings_window(SettingsSection::General);
                        }),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Hide),
                    MenuItem::role(MenuRole::HideOthers),
                    MenuItem::role(MenuRole::ShowAll),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Quit).label(store.t("menu.quit").to_string()),
                ],
            ),
            MenuItem::submenu(
                store.t("menu.file").to_string(),
                [
                    MenuItem::new(store.t("menu.import_folder").to_string()).on_select(move || {
                        store.open_settings_window(SettingsSection::Cache);
                    }),
                    MenuItem::new(store.t("menu.install_plugin").to_string()).on_select(
                        move || {
                            store.open_settings_window(SettingsSection::Plugins);
                            store.open_modal(ActiveModal::InstallPlugin);
                        },
                    ),
                ],
            ),
            MenuItem::submenu(
                edit_menu_title,
                [
                    MenuItem::role(MenuRole::Undo),
                    MenuItem::role(MenuRole::Redo),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Cut),
                    MenuItem::role(MenuRole::Copy),
                    MenuItem::role(MenuRole::Paste),
                    MenuItem::role(MenuRole::SelectAll),
                ],
            ),
            MenuItem::submenu(
                store.t("menu.playback").to_string(),
                [
                    MenuItem::new(play_label).on_select(move || store.toggle_play()),
                    MenuItem::new(store.t("player.prev").to_string())
                        .on_select(move || store.prev_track()),
                    MenuItem::new(store.t("player.next").to_string())
                        .on_select(move || store.next_track()),
                    MenuItem::separator(),
                    MenuItem::new(store.t("player.shuffle").to_string())
                        .checked(shuffle_on)
                        .on_select(move || store.toggle_shuffle()),
                    MenuItem::new(repeat_label)
                        .checked(repeat_mode != RepeatMode::Off)
                        .on_select(move || store.cycle_repeat()),
                ],
            ),
            MenuItem::submenu(
                store.t("menu.window").to_string(),
                [
                    MenuItem::role(MenuRole::Minimize),
                    MenuItem::role(MenuRole::Zoom),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Close),
                ],
            ),
        ];

        set_application_menu(items);
    });

    // Native system tray icon & menu reactive to current track, play state, and locale.
    let tray_icon_src = resolve_tray_icon_src();
    let tray_track = computed(move || {
        store.current_track.with(|t| {
            t.as_ref().map(|tr| {
                let artists = tr.artist_names();
                if artists.is_empty() {
                    tr.name.clone()
                } else {
                    format!("{} — {}", tr.name, artists)
                }
            })
        })
    });

    effect(move || {
        let _locale = store.locale.get();
        let playing = store.is_playing.get();
        let tooltip = tray_track
            .get()
            .unwrap_or_else(|| store.t("app.name").to_string());
        let play_label = if playing {
            store.t("player.pause").to_string()
        } else {
            store.t("player.play").to_string()
        };
        let show_main_click = show_main.clone();
        let show_main_menu = show_main.clone();

        set_tray(
            tray()
                .icon(tray_icon_src.clone())
                .tooltip(tooltip)
                .on_click(move |_| {
                    show_main_click();
                })
                .menu([
                    MenuItem::new(play_label).on_select(move || store.toggle_play()),
                    MenuItem::new(store.t("player.prev").to_string())
                        .on_select(move || store.prev_track()),
                    MenuItem::new(store.t("player.next").to_string())
                        .on_select(move || store.next_track()),
                    MenuItem::separator(),
                    MenuItem::new(store.t("tray.show_window").to_string()).on_select(move || {
                        show_main_menu();
                    }),
                    MenuItem::new(store.t("menu.settings").to_string()).on_select(move || {
                        store.open_settings_window(SettingsSection::General);
                    }),
                    MenuItem::separator(),
                    MenuItem::new(store.t("menu.quit").to_string()).on_select(quit),
                ]),
        );
    });
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos"))
)))]
pub fn install_desktop_menu_and_tray(_store: AppStore, _show_main: std::rc::Rc<dyn Fn()>) {}

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

    // Cmd/Ctrl+, opens the Desktop Settings child window.
    on_key_with(
        "mod+,",
        KeyOptions {
            when: When::Always,
            ..Default::default()
        },
        move |_| {
            store.open_settings_window(SettingsSection::General);
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

    // Cmd/Ctrl+1..5 switch Now Playing panels in upstream `npTabs` order while open:
    // Songs: Lyrics, Stage, Queue, Wall, Comments (capability-gated).
    // Episodes: Podcast (About), Queue, Comments (capability-gated).
    for (idx, shortcut) in [
        (0, "mod+1"),
        (1, "mod+2"),
        (2, "mod+3"),
        (3, "mod+4"),
        (4, "mod+5"),
    ] {
        on_key_with(
            shortcut,
            KeyOptions {
                when: When::Always,
                ..Default::default()
            },
            move |_| {
                switch_now_playing_tab(store, idx);
            },
        );
    }

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

fn switch_now_playing_tab(store: AppStore, index: usize) {
    if !store.now_playing_open.get_untracked() {
        return;
    }
    let Some(track) = store.current_track.get_untracked() else {
        return;
    };
    let can_comments = untracked(|| store.active_can("comments"));
    let mut panels = Vec::with_capacity(5);
    if track.is_episode() {
        panels.push(NowPlayingPanel::Podcast);
        panels.push(NowPlayingPanel::Queue);
    } else {
        panels.push(NowPlayingPanel::Lyrics);
        panels.push(NowPlayingPanel::Stage);
        panels.push(NowPlayingPanel::Queue);
        panels.push(NowPlayingPanel::Wall);
    }
    if can_comments {
        panels.push(NowPlayingPanel::Comments);
    }
    if let Some(&panel) = panels.get(index) {
        store.set_now_playing_panel(panel);
    }
}

fn desktop_top_bar(store: AppStore) -> View {
    let scrolled = store.page_scrolled;
    let route_title = computed(move || resolve_route_title(store));

    view()
        .test_id("desktop-title-bar")
        .w_full()
        .h(move || px(title_bar().top.0.max(TITLE_BAR_HEIGHT)))
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .pl(move || {
            let sidebar_w = if store.sidebar_collapsed.get() {
                SIDEBAR_COLLAPSED_W
            } else {
                SIDEBAR_EXPANDED_W
            };
            let extra_left = (title_bar().left.0 - sidebar_w).max(0.0);
            px(12.0 + extra_left)
        })
        .pr(move || px(title_bar().right.0 + 14.0))
        .bg(move || {
            if scrolled.get() {
                theme::card()
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
                        .transition_opacity()
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

fn desktop_sidebar(store: AppStore, search_ref: ElementRef, sidebar_header_h: Signal<f32>) -> View {
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
        // Generous top drag clearance below macOS traffic lights (upstream `topBar(c) + 4`).
        .child(
            view()
                .w_full()
                .h(move || px(sidebar_top_clearance()))
                .flex_shrink_0()
                .window_drag(true),
        )
        // Source switcher + integrated sidebar search box (measured for floating suggestions anchor).
        .child(
            view()
                .w_full()
                .px_2p5()
                .pb_2()
                .gap_2()
                .flex_shrink_0()
                .on_layout(move |bounds: &Bounds<Points>| {
                    let h = bounds.size.height.0;
                    if h > 0.0 && (sidebar_header_h.get_untracked() - h).abs() > 0.5 {
                        sidebar_header_h.set(h);
                    }
                })
                .child(
                    when(
                        move || !collapsed.get(),
                        move || {
                            source_pill(
                                move || store.active_source_name(),
                                true,
                                move || {
                                    store.open_modal(ActiveModal::SourceSwitcher);
                                },
                            )
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
                }))
        },
    )
    .otherwise(move || {
        sidebar_nav_item(
            store,
            Icon::Search,
            "nav.search",
            Route::Search,
            "nav-search",
        )
    })
}

fn sidebar_suggestions_popover(store: AppStore) -> View {
    Elevated::elevation(
        view()
            .test_id("desktop-search-suggestions")
            .w_full()
            .max_h(px(340.0))
            .rounded(px(radius::CONTROL))
            .bg(theme::popover)
            .border_1()
            .border_color(theme::border)
            .overflow_hidden(),
        Elevation::Popover,
    )
    .child(
        scroll().w_full().max_h(px(338.0)).child(
            view()
                .w_full()
                .p_1p5()
                .gap_1()
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
                )),
        ),
    )
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
    let active = computed(move || {
        let current = store.route.get();
        let custom = active_custom_nav(store);
        route_matches(&current, &target_check, &custom)
    });

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
    let can_account = computed(move || store.active_can("signIn") || store.active_can("account"));
    let settings_active = computed(move || {
        store.settings_window_open.get()
            || matches!(
                store.route.get(),
                Route::Settings | Route::SettingsDetail(_)
            )
    });

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
        .child(
            button()
                .test_id("nav-settings")
                .selected(settings_active)
                .accessibility_label(move || Some(store.t("nav.settings")))
                .on_press(move || store.open_settings_window(SettingsSection::General))
                .w_full()
                .h(px(34.0))
                .px_2p5()
                .flex_row()
                .items_center()
                .gap_2p5()
                .rounded(px(radius::CONTROL))
                .style_when(collapsed, |s| s.justify_center().px_0())
                .bg(move || {
                    if settings_active.get() {
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
                .child(
                    view()
                        .absolute()
                        .left_0()
                        .top(px(7.0))
                        .w(px(3.0))
                        .h(px(20.0))
                        .rounded_full()
                        .bg(theme::primary)
                        .opacity(move || {
                            if settings_active.get() {
                                1.0_f32
                            } else {
                                0.0_f32
                            }
                        }),
                )
                .child(icon_reactive(Icon::Settings, 17.0, move || {
                    if settings_active.get() {
                        theme::primary()
                    } else {
                        theme::muted_fg()
                    }
                }))
                .child(when(
                    move || !collapsed.get(),
                    move || {
                        text(move || store.t("nav.settings"))
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .style_when(settings_active, |s| s.font_weight(FontWeight::SEMIBOLD))
                            .text_color(move || {
                                if settings_active.get() {
                                    theme::primary()
                                } else {
                                    theme::sidebar_fg()
                                }
                            })
                    },
                )),
        )
}

fn desktop_account_row(store: AppStore, collapsed: Signal<bool>) -> View {
    let entry_memo = computed(move || store.active_source_entry().map(|e| (e.info.id, e.account)));

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
                    ContextMenuAction::new(store.t("nav.settings"), move || {
                        store.open_settings_window(SettingsSection::General);
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

#[derive(Clone, PartialEq, Eq)]
enum PlayerSubtitle {
    Idle,
    Buffering,
    Error(String),
    Artists(Vec<Artist>),
}

fn desktop_player_bar(store: AppStore) -> View {
    let has_track = computed(move || store.current_track.with(Option::is_some));

    let is_episode = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(Track::is_episode))
    });

    let is_loading = computed(move || store.playback_status.with(PlaybackStatus::is_loading));

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

    // Narrow memo so Play/Pause toggles and 60ms `position_ms` ticks never rebuild artist links.
    let subtitle_state = computed(move || {
        match store.playback_status.get() {
            PlaybackStatus::Loading => return PlayerSubtitle::Buffering,
            PlaybackStatus::Error(msg) => return PlayerSubtitle::Error(msg),
            PlaybackStatus::Stopped | PlaybackStatus::Playing | PlaybackStatus::Paused => {}
        }
        store.current_track.with(|t| match t {
            Some(tr) => PlayerSubtitle::Artists(tr.artists.clone()),
            None => PlayerSubtitle::Idle,
        })
    });

    // Quantized buffered ratio memo to prevent high-frequency sub-pixel layout churn.
    let buffered_ratio = computed(move || {
        let dur = store.duration_ms.get();
        if dur == 0 {
            0.0_f32
        } else {
            let raw = (store.buffered_ms.get() as f32 / dur as f32).clamp(0.0, 1.0);
            (raw * 250.0).round() / 250.0
        }
    });

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
        .bg(theme::card)
        .border_t_1()
        .border_color(theme::border)
        // Left: 48x48 cover (hover reveals scrim + upward chevron), title, clickable artist links
        // (or buffering/error status), and Like heart button.
        .child(
            view()
                .flex_1()
                .min_w(px(176.0))
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    button()
                        .test_id("desktop-now-playing-trigger")
                        .group()
                        .accessibility_label(move || Some(store.t("now_playing.expand")))
                        .on_press(move || {
                            if store.current_track.with_untracked(Option::is_some) {
                                store.open_now_playing();
                            }
                        })
                        .size(px(48.0))
                        .flex_shrink_0()
                        .rounded(px(radius::SM))
                        .overflow_hidden()
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(
                            when(has_track, move || {
                                view()
                                    .size_full()
                                    .child(dynamic(move || {
                                        let seed = track_seed.get();
                                        untracked(move || {
                                            artwork_box(
                                                store.current_artwork_uri,
                                                seed,
                                                48.0,
                                                radius::SM,
                                            )
                                        })
                                    }))
                                    .child(
                                        view()
                                            .absolute()
                                            .inset_0()
                                            .items_center()
                                            .justify_center()
                                            .bg(colors::MEDIA_BADGE_BG)
                                            .opacity(0.0)
                                            .transition_opacity()
                                            .duration(move || motion::transition_ms(140.0))
                                            .group_hover(|s| s.opacity(1.0))
                                            .pointer_events(PointerEvents::None)
                                            .child(
                                                icon(
                                                    Icon::ChevronDown,
                                                    18.0,
                                                    colors::MEDIA_BADGE_FG,
                                                )
                                                .rotate(Radians(PI)),
                                            ),
                                    )
                            })
                            .otherwise(move || {
                                view()
                                    .size_full()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme::muted)
                                    .child(icon(Icon::Disc, 20.0, colors::FAINT_FOREGROUND))
                            }),
                        ),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_0p5()
                        .child(
                            when(has_track, move || {
                                button()
                                    .accessibility_label(move || {
                                        Some(store.t("now_playing.expand"))
                                    })
                                    .on_press(move || store.open_now_playing())
                                    .w_full()
                                    .items_start()
                                    .min_w(px(0.0))
                                    .rounded(px(radius::SM))
                                    .hover(|s| s.opacity(0.78))
                                    .focus_visible(|s| {
                                        s.outline(px(2.0)).outline_color(theme::ring)
                                    })
                                    .child(
                                        text(move || {
                                            store.current_track.with(|t| match t {
                                                Some(tr) => SharedString::from(tr.name.clone()),
                                                None => SharedString::default(),
                                            })
                                        })
                                        .number_of_lines(1)
                                        .text_size(px(font_size::BODY))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::fg),
                                    )
                            })
                            .otherwise(move || {
                                text(move || store.t("player.idle_title"))
                                    .number_of_lines(1)
                                    .text_size(px(font_size::BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::muted_fg)
                            }),
                        )
                        .child(dynamic(move || match subtitle_state.get() {
                            PlayerSubtitle::Idle => view().into_element(),
                            PlayerSubtitle::Buffering => view()
                                .w_full()
                                .min_w(px(0.0))
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .child(loading_spinner(11.0, theme::muted_fg))
                                .child(
                                    text(move || store.t("player.buffering"))
                                        .number_of_lines(1)
                                        .text_size(px(font_size::CAPTION))
                                        .text_color(theme::muted_fg),
                                )
                                .into_element(),
                            PlayerSubtitle::Error(msg) => text(SharedString::from(msg))
                                .number_of_lines(1)
                                .text_size(px(font_size::CAPTION))
                                .text_color(theme::danger)
                                .into_element(),
                            PlayerSubtitle::Artists(artists) => {
                                untracked(move || player_artist_links(store, artists))
                                    .into_element()
                            }
                        })),
                )
                .child(when(has_track, move || {
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
                })),
        )
        // Center: transport controls + smoke-mode scrubber (smoke paused while Now Playing covers bar).
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
                        .child(play_pause_button_buffering(
                            store.is_playing,
                            is_loading,
                            34.0,
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
                        .smoke_mode(move || {
                            !store.now_playing_open.get()
                                && store.settings.with(|s| s.smoke_progress)
                        })
                        .playing(move || store.is_playing.get())
                        .accent(move || store.current_accent.get())
                        .spectrum(store.spectrum)
                        .disabled(move || !has_track.get())
                        .show_labels(true)
                        .inline_labels(true)
                        .on_seek(move |ms| store.seek(ms)),
                ),
        )
        // Right: adaptive lyrics/episode toggle, queue button, remote devices menu, volume slider,
        // expand Now Playing.
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
                        move || {
                            if is_episode.get() {
                                Icon::Podcast
                            } else {
                                Icon::Lyrics
                            }
                        },
                        30.0,
                        16.0,
                        move || {
                            let open = store.now_playing_open.get();
                            let panel = store.now_playing_panel.get();
                            let active = open
                                && if is_episode.get() {
                                    panel == NowPlayingPanel::Podcast
                                } else {
                                    matches!(
                                        panel,
                                        NowPlayingPanel::Lyrics | NowPlayingPanel::Stage
                                    )
                                };
                            if active {
                                theme::primary()
                            } else {
                                theme::muted_fg()
                            }
                        },
                        move || {
                            if is_episode.get() {
                                store.t("now_playing.panel.podcast")
                            } else {
                                store.t("now_playing.panel.lyrics")
                            }
                        },
                        move || {
                            let ep = is_episode.get_untracked();
                            let target = if ep {
                                NowPlayingPanel::Podcast
                            } else {
                                store.preferred_lyrics_panel()
                            };
                            let open = store.now_playing_open.get_untracked();
                            let current = store.now_playing_panel.get_untracked();
                            let already_active = open
                                && if ep {
                                    current == NowPlayingPanel::Podcast
                                } else {
                                    matches!(
                                        current,
                                        NowPlayingPanel::Lyrics | NowPlayingPanel::Stage
                                    )
                                };
                            if already_active {
                                store.close_now_playing();
                            } else {
                                store.open_now_playing_panel(target);
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
                                && store.now_playing_panel.get_untracked() == NowPlayingPanel::Queue
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
                .child(view().w(px(84.0)).child(styled_slider(
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
                )))
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

fn player_artist_links(store: AppStore, artists: Vec<Artist>) -> View {
    let mut row = view()
        .w_full()
        .min_w(px(0.0))
        .flex_row()
        .items_center()
        .overflow_hidden();

    let mut rendered = 0usize;
    for artist in artists {
        if artist.name.is_empty() {
            continue;
        }
        if rendered > 0 {
            row = row.child(
                text(", ")
                    .flex_shrink_0()
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg),
            );
        }
        let name = SharedString::from(artist.name);
        if artist.id.is_empty() {
            row = row.child(
                text(name)
                    .min_w(px(0.0))
                    .number_of_lines(1)
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg),
            );
        } else {
            let artist_id = artist.id;
            let label = name.clone();
            row = row.child(
                button()
                    .group()
                    .accessibility_label(Some(label))
                    .on_press(move || {
                        store.go_to_artist(&artist_id);
                    })
                    .min_w(px(0.0))
                    .rounded(px(radius::SM))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(
                        text(name)
                            .number_of_lines(1)
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::muted_fg)
                            .group_hover(|s| s.text_color(theme::fg)),
                    ),
            );
        }
        rendered += 1;
    }

    row
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

fn route_matches(current: &Route, target: &Route, custom_nav: &[NavItem]) -> bool {
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
