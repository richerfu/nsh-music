//! Settings & Plugin Manager screens across Desktop, Pad, and Phone plus
//! modal views (`InstallPlugin`, `ConfirmPluginInstall`, `PluginConfig`,
//! `QrSignIn`, and `WebSignIn`) aligned 1:1 with `egoist/mygo-music`.
//!
//! - **Desktop**: child window content (`settings_window_content`) with a `44pt`
//!   hidden-titlebar drag band, `200pt` left sidebar (`settings-sidebar`) with
//!   cross-section live search (`settings-search-input`) + 6 section nav items,
//!   and right scrollable detail pane (`settings-detail-pane`) unifying Plugins
//!   directly inside Settings.
//! - **Pad**: iPadOS 2-column split view (`pad-settings-split`) with `220pt`
//!   touch-sized (`>= 44pt`) sidebar and right detail pane.
//! - **Phone**: Level-1 native grouped inset menu list (`phone-settings-list`)
//!   with `>= 56pt` rows pushing `Route::SettingsDetail(section)` rendered by
//!   `settings_detail_page` (`phone-settings-detail`), hiding desktop-only
//!   folder reveal / drag-drop chrome on mobile runtimes.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use nsh_base::{SelectItem, button, input};
use nsh_core::element::desktop::title_bar;
use nsh_core::prelude::*;
use nsh_music_core::i18n::Locale;
use nsh_music_core::local::LocalMusicSource;
use nsh_music_core::model::{PlayerStyle, Quality, SettingDef, SignInPrompt, StageStyle};
use nsh_music_core::settings::ThemeMode;
use nsh_music_core::source::PluginCompatLevel;
use nsh_music_plugin::{
    CompatTier, InstallSource, PendingPluginInstall, PluginSummary, WebSignInResult,
};
use qrcode::{Color as QrColor, QrCode};

use crate::pages::{PagesOwner, page_scroll, refresh_pill_button};
use crate::state::{
    ActiveModal, AppStore, NowPlayingMode, NowPlayingStageStyle, Route, SettingsSection,
    web_cookies,
};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, compat_badge_text, confirm_destructive, empty_state,
    empty_state_with_actions, format_bytes, ghost_button, icon, icon_button_sized, loading_spinner,
    motion, pill_button, primary_button, styled_input, styled_select, styled_slider, styled_switch,
    theme,
};

const WEB_SIGN_IN_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";
const WEB_SIGN_IN_MOBILE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Mobile/15E148 Safari/604.1";

fn looks_like_desktop_user_agent(ua: &str) -> bool {
    let has_desktop_os = ua.contains("Windows NT")
        || ua.contains("Macintosh")
        || ua.contains("X11")
        || ua.contains("CrOS")
        || ua.contains("Linux x86_64");
    let has_mobile_hint = ua.contains("Mobile") || ua.contains("Android") || ua.contains("iPhone");
    has_desktop_os && !has_mobile_hint
}

const WEB_SIGN_IN_COOKIE_SCRIPT: &str = r#"
(function() {
  function report() {
    try {
      if (window.nsh && typeof window.nsh.postMessage === 'function') {
        var payload = JSON.stringify({
          type: 'cookie',
          cookie: document.cookie || '',
          href: window.location ? window.location.href : ''
        });
        window.nsh.postMessage(payload);
      }
    } catch (_) {}
  }
  report();
  try {
    var pollId = setInterval(report, 800);
    function stopPoll() {
      try { clearInterval(pollId); } catch (_) {}
    }
    window.addEventListener('pagehide', stopPoll, { passive: true });
    window.addEventListener('beforeunload', stopPoll, { passive: true });
    document.addEventListener('DOMContentLoaded', report, { passive: true });
    window.addEventListener('load', report, { passive: true });
    window.addEventListener('hashchange', report, { passive: true });
    window.addEventListener('popstate', report, { passive: true });
    document.addEventListener('visibilitychange', report, { passive: true });
    document.addEventListener('click', function() { setTimeout(report, 350); }, { passive: true });
    document.addEventListener('submit', function() { setTimeout(report, 600); }, { passive: true });
    setTimeout(report, 400);
    setTimeout(report, 1400);
  } catch (_) {}
})();
"#;

fn section_icon(section: SettingsSection) -> Icon {
    match section {
        SettingsSection::General => Icon::Sliders,
        SettingsSection::Plugins => Icon::Plugin,
        SettingsSection::Playback => Icon::Music,
        SettingsSection::Cache => Icon::Database,
        SettingsSection::Experiments => Icon::Sparkles,
        SettingsSection::About => Icon::Info,
    }
}

fn desktop_nav_test_id(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::General => "settings-nav-general",
        SettingsSection::Plugins => "settings-nav-plugins",
        SettingsSection::Playback => "settings-nav-playback",
        SettingsSection::Cache => "settings-nav-cache",
        SettingsSection::Experiments => "settings-nav-experiments",
        SettingsSection::About => "settings-nav-about",
    }
}

fn phone_row_test_id(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::General => "phone-settings-general",
        SettingsSection::Plugins => "phone-settings-plugins",
        SettingsSection::Playback => "phone-settings-playback",
        SettingsSection::Cache => "phone-settings-cache",
        SettingsSection::Experiments => "phone-settings-experiments",
        SettingsSection::About => "phone-settings-about",
    }
}

fn is_mobile_ui(store: AppStore, form: DeviceForm) -> bool {
    form.is_phone() || store.is_mobile_runtime()
}

// -----------------------------------------------------------------------------
// Top-level entrypoints (`Route::Settings`, `Route::Plugins`, `Route::SettingsDetail`,
// and Desktop child window `settings_window_content`)
// -----------------------------------------------------------------------------

/// Renders the Settings screen adapted to `form`:
/// - **Desktop**: 2-column child-window layout (`settings_window_content`)
/// - **Pad**: iPadOS 2-column split (`pad-settings-split`)
/// - **Phone**: Level-1 native grouped inset menu list (`phone-settings-list`)
pub fn settings_page(store: AppStore, form: DeviceForm, focus_plugins: bool) -> View {
    store.refresh_cache_stats();
    if focus_plugins {
        store.settings_section.set(SettingsSection::Plugins);
        store.refresh_sources();
    }

    match form {
        DeviceForm::Desktop => settings_window_content(store),
        DeviceForm::Pad { .. } => pad_settings_split(store, form),
        DeviceForm::Phone => phone_settings_list(store, form),
    }
}

/// Renders the Level-2 Phone Settings detail page (`Route::SettingsDetail(section)`).
pub fn settings_detail_page(store: AppStore, form: DeviceForm, section: SettingsSection) -> View {
    store.settings_section.set(section);
    if section == SettingsSection::Cache {
        store.refresh_cache_stats();
    } else if section == SettingsSection::Plugins {
        store.refresh_sources();
    }

    let body = view()
        .test_id("phone-settings-detail")
        .w_full()
        .gap_5()
        .child(dynamic(move || {
            let _loc = store.locale.get();
            untracked(move || render_section_pane(store, form, section, "").into_element())
        }));

    page_scroll(form, body)
}

/// Renders the Desktop Settings child window content (`src/shell/desktop.rs`):
/// `44pt` hidden-titlebar drag band + `200pt` left sidebar (`settings-sidebar`)
/// with live cross-section search + right scrollable detail pane
/// (`settings-detail-pane`).
pub fn settings_window_content(store: AppStore) -> View {
    store.refresh_cache_stats();
    let filter_query = signal(String::new());
    let form = DeviceForm::Desktop;

    let top_drag_bar = view()
        .w_full()
        .h(move || px(title_bar().top.0.max(44.0)))
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .window_drag(true)
        .child(
            view()
                .w(px(200.0))
                .h_full()
                .flex_shrink_0()
                .bg(theme::sidebar)
                .border_r_1()
                .border_color(theme::sidebar_border),
        )
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .pl(move || px((title_bar().left.0 - 200.0).max(0.0) + 28.0))
                .pr(move || px(title_bar().right.0 + 28.0))
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .border_b_1()
                .border_color(theme::border)
                .child(
                    text(move || {
                        if filter_query.with(|q| !q.trim().is_empty()) {
                            store.t("nav.search")
                        } else {
                            store.t(store.settings_section.get().title_key())
                        }
                    })
                    .role(AccessibilityRole::Header)
                    .number_of_lines(1)
                    .text_size(px(15.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::fg),
                )
                .child(when(
                    move || {
                        filter_query.with(|q| q.trim().is_empty())
                            && store.settings_section.get() == SettingsSection::Plugins
                    },
                    move || {
                        view().window_drag(false).child(
                            button()
                                .on_press(move || store.open_modal(ActiveModal::InstallPlugin))
                                .test_id("settings-header-install-plugin")
                                .min_h(px(28.0))
                                .px_3()
                                .py_1()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .rounded(px(radius::CONTROL))
                                .bg(theme::primary)
                                .transition_all()
                                .duration(move || motion::transition_ms(140.0))
                                .hover(|s| s.bg(theme::primary_hover))
                                .active(|s| s.scale(motion::press_scale))
                                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                                .child(icon(Icon::Plus, 13.0, colors::PRIMARY_FOREGROUND))
                                .child(
                                    text(move || store.t("plugins.install"))
                                        .number_of_lines(1)
                                        .text_size(px(font_size::CAPTION))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::primary_fg),
                                ),
                        )
                    },
                )),
        );

    let mut sidebar = view()
        .test_id("settings-sidebar")
        .w(px(200.0))
        .h_full()
        .flex_shrink_0()
        .px_3()
        .pt_1p5()
        .pb_3p5()
        .gap_1()
        .bg(theme::sidebar)
        .border_r_1()
        .border_color(theme::sidebar_border)
        .child(
            view()
                .w_full()
                .pb_2p5()
                .child(settings_search_field(store, filter_query, false)),
        );

    for section in SettingsSection::ALL {
        sidebar = sidebar.child(settings_nav_item(store, section, filter_query, false));
    }

    let detail_pane = view()
        .test_id("settings-detail-pane")
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .child(
            scroll()
                .flex_1()
                .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
                .child(
                    view()
                        .w_full()
                        .max_w(px(760.0))
                        .px_7()
                        .pt_5()
                        .pb_8()
                        .gap_5()
                        .child(settings_detail_body(store, form, filter_query)),
                ),
        );

    view()
        .flex_1()
        .w_full()
        .h_full()
        .bg(theme::bg)
        .text_color(theme::fg)
        .child(top_drag_bar)
        .child(
            view()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .flex_row()
                .child(sidebar)
                .child(detail_pane),
        )
}

/// Renders the iPadOS 2-column split Settings screen (`DeviceForm::Pad`).
fn pad_settings_split(store: AppStore, form: DeviceForm) -> View {
    let filter_query = signal(String::new());

    let mut sidebar = view()
        .test_id("settings-sidebar")
        .w(move || px(220.0 + safe_area().left.0))
        .h_full()
        .flex_shrink_0()
        .pl(move || px(12.0 + safe_area().left.0))
        .pr_3()
        .pt_4()
        .pb(move || px(104.0 + safe_area().bottom.0))
        .gap_1p5()
        .bg(theme::sidebar)
        .border_r_1()
        .border_color(theme::sidebar_border)
        .child(
            text(move || store.t("settings.title"))
                .role(AccessibilityRole::Header)
                .px_2()
                .pb_2()
                .text_size(px(font_size::TITLE))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg),
        )
        .child(
            view()
                .w_full()
                .pb_2()
                .child(settings_search_field(store, filter_query, true)),
        );

    for section in SettingsSection::ALL {
        sidebar = sidebar.child(settings_nav_item(store, section, filter_query, true));
    }

    let detail_pane = view()
        .test_id("settings-detail-pane")
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .child(
            scroll()
                .flex_1()
                .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
                .keyboard_taps(KeyboardTaps::Handled)
                .child(
                    view()
                        .w_full()
                        .max_w(px(820.0))
                        .pl_6()
                        .pr(move || px(24.0 + safe_area().right.0))
                        .pt_5()
                        .pb(move || px(112.0 + safe_area().bottom.0))
                        .gap_5()
                        .child(
                            text(move || {
                                if filter_query.with(|q| !q.trim().is_empty()) {
                                    store.t("nav.search")
                                } else {
                                    store.t(store.settings_section.get().title_key())
                                }
                            })
                            .role(AccessibilityRole::Header)
                            .text_size(px(font_size::TITLE_LG))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::fg),
                        )
                        .child(settings_detail_body(store, form, filter_query)),
                ),
        );

    view()
        .test_id("pad-settings-split")
        .flex_1()
        .w_full()
        .h_full()
        .flex_row()
        .bg(theme::bg)
        .child(sidebar)
        .child(detail_pane)
}

/// Renders the Level-1 Phone native grouped inset Settings menu (`DeviceForm::Phone`).
fn phone_settings_list(store: AppStore, form: DeviceForm) -> View {
    let filter_query = signal(String::new());

    let intro = view()
        .w_full()
        .gap_3()
        .child(
            text(move || store.t("settings.mobile_hint"))
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg),
        )
        .child(settings_search_field(store, filter_query, true));

    let grouped_menu = view()
        .w_full()
        .gap_4()
        .child(phone_settings_group(
            move || store.t("nav.sources"),
            vec![phone_settings_row(
                store,
                SettingsSection::Plugins,
                move || {
                    let count = store.plugins.with(Vec::len).to_string();
                    store.t_fmt("settings.plugins_count", &[("count", &count)])
                },
            )],
        ))
        .child(phone_settings_group(
            move || store.t("settings.title"),
            vec![
                phone_settings_row(store, SettingsSection::General, move || {
                    match store.theme_override.get() {
                        ThemeMode::System => store.t("settings.theme.system"),
                        ThemeMode::Light => store.t("settings.theme.light"),
                        ThemeMode::Dark => store.t("settings.theme.dark"),
                    }
                }),
                phone_settings_row(store, SettingsSection::Playback, move || {
                    match store.settings.with(|s| s.audio_quality) {
                        Quality::Standard => store.t("player.quality.standard"),
                        Quality::High => store.t("player.quality.high"),
                        Quality::Lossless => store.t("player.quality.lossless"),
                    }
                }),
                phone_settings_row(store, SettingsSection::Cache, move || {
                    SharedString::from(format_bytes(store.cache_used_bytes.get()))
                }),
                phone_settings_row(store, SettingsSection::Experiments, move || {
                    let enabled = store.settings.with(|s| {
                        usize::from(s.fog_backdrop)
                            + usize::from(s.smoke_progress)
                            + usize::from(s.grow_from_cover)
                    });
                    SharedString::from(format!("{enabled}/3"))
                }),
            ],
        ))
        .child(phone_settings_group(
            move || store.t("settings.section.about"),
            vec![phone_settings_row(
                store,
                SettingsSection::About,
                move || SharedString::from(format!("v{}", env!("CARGO_PKG_VERSION"))),
            )],
        ));

    let content = view()
        .test_id("phone-settings-list")
        .w_full()
        .gap_4()
        .child(intro)
        .child(dynamic(move || {
            let q = filter_query.get().trim().to_ascii_lowercase();
            let _loc = store.locale.get();
            if q.is_empty() {
                untracked(|| grouped_menu.clone().into_element())
            } else {
                untracked(move || render_search_results(store, form, &q).into_element())
            }
        }));

    page_scroll(form, content)
}

fn phone_section_badge_colors(section: SettingsSection) -> (AdaptiveColor, AdaptiveColor) {
    match section {
        SettingsSection::Plugins => (theme::primary_soft(), colors::PRIMARY),
        SettingsSection::General => (theme::primary_soft(), colors::PRIMARY),
        SettingsSection::Playback => (theme::amber_soft(), colors::AMBER),
        SettingsSection::Cache => (theme::ok_soft(), colors::OK),
        SettingsSection::Experiments => (theme::primary_soft(), colors::PRIMARY),
        SettingsSection::About => (theme::secondary(), colors::MUTED_FOREGROUND),
    }
}

fn phone_settings_group(header: impl IntoReactive<SharedString>, rows: Vec<View>) -> View {
    let mut card = Elevated::elevation(
        view()
            .w_full()
            .rounded(px(radius::XL))
            .bg(theme::card)
            .border_1()
            .border_color(theme::border)
            .overflow_hidden(),
        Elevation::Subtle,
    );
    for (idx, row) in rows.into_iter().enumerate() {
        if idx > 0 {
            card = card.child(
                view()
                    .w_full()
                    .pl(px(60.0))
                    .child(view().w_full().h(px(1.0)).bg(theme::border)),
            );
        }
        card = card.child(row);
    }
    view()
        .w_full()
        .gap_1p5()
        .child(
            text(header)
                .px_2()
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .letter_spacing(px(0.6))
                .text_color(theme::muted_fg),
        )
        .child(card)
}

fn phone_settings_row(
    store: AppStore,
    section: SettingsSection,
    value_label: impl Fn() -> SharedString + 'static,
) -> View {
    let glyph = section_icon(section);
    let title_key = section.title_key();
    view().w_full().child(
        button()
            .test_id(phone_row_test_id(section))
            .on_press(move || {
                store.settings_section.set(section);
                store.navigate(Route::SettingsDetail(section));
            })
            .w_full()
            .min_h(px(56.0))
            .px_4()
            .py_2p5()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .bg(colors::TRANSPARENT)
            .transition_colors()
            .duration(move || motion::transition_ms(130.0))
            .hover(|s| s.bg(theme::hover))
            .active(|s| s.bg(theme::press))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(
                view()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        view()
                            .size(px(32.0))
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.0))
                            .bg(move || phone_section_badge_colors(section).0)
                            .child(icon(glyph, 16.0, phone_section_badge_colors(section).1)),
                    )
                    .child(
                        text(move || store.t(title_key))
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::fg),
                    ),
            )
            .child(
                view()
                    .flex_shrink_0()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .child(
                        text(value_label)
                            .number_of_lines(1)
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::muted_fg),
                    )
                    .child(icon(Icon::ChevronRight, 16.0, colors::MUTED_FOREGROUND)),
            ),
    )
}

// -----------------------------------------------------------------------------
// Sidebar search field & navigation items (Desktop + Pad)
// -----------------------------------------------------------------------------

/// Single-border search input using borderless `nsh_base::input()` inside one
/// cleanly bordered container (matching `src/shell/desktop.rs` & `ui_settings.go`).
fn settings_search_field(store: AppStore, filter_query: Signal<String>, touch_sized: bool) -> View {
    let field_h = if touch_sized { 44.0 } else { 30.0 };
    let side_pad = if touch_sized { 14.0 } else { 10.0 };
    let input_ref = element_ref();

    view()
        .w_full()
        .h(px(field_h))
        .flex_row()
        .items_center()
        .rounded(px(if touch_sized { radius::PILL } else { 8.0 }))
        .bg(theme::secondary)
        .border_1()
        .border_color(move || {
            if filter_query.with(|q| !q.is_empty()) {
                theme::primary()
            } else {
                theme::border()
            }
        })
        .focus_within(|s| s.border_color(theme::ring))
        .child(
            view()
                .h_full()
                .pl(px(side_pad))
                .pr_2()
                .flex_row()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .on_press(move || input_ref.focus())
                .child(icon(Icon::Search, 14.0, colors::MUTED_FOREGROUND)),
        )
        .child(
            input()
                .test_id("settings-search-input")
                .element_ref(&input_ref)
                .value(filter_query)
                .on_change(move |v: &str| filter_query.set(v.to_owned()))
                .placeholder(move || store.t("settings.search_placeholder").to_string())
                .placeholder_color(|| color(theme::muted_fg()))
                .caret_color(|| color(theme::primary()))
                .selection_color(|| color(theme::primary_soft()))
                .auto_capitalize(AutoCapitalize::None)
                .auto_correct(false)
                .return_key_type(ReturnKeyType::Search)
                .on_submit(move |v: &str| {
                    if !v.is_empty() {
                        filter_query.set(v.to_owned());
                    }
                    dismiss_keyboard();
                })
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .bg(colors::TRANSPARENT)
                .text_size(px(if touch_sized {
                    font_size::BODY
                } else {
                    font_size::SUBTITLE
                }))
                .text_color(theme::fg),
        )
        .child(
            when(
                move || filter_query.with(|q| !q.is_empty()),
                move || {
                    view()
                        .h_full()
                        .pl_1()
                        .pr(px(if touch_sized { 8.0 } else { 6.0 }))
                        .flex_row()
                        .items_center()
                        .flex_shrink_0()
                        .child(icon_button_sized(
                            Icon::Close,
                            if touch_sized { 32.0 } else { 22.0 },
                            13.0,
                            theme::muted_fg,
                            move || store.t("common.close"),
                            move || filter_query.set(String::new()),
                        ))
                },
            )
            .otherwise(move || {
                view()
                    .h_full()
                    .w(px(side_pad))
                    .flex_shrink_0()
                    .on_press(move || input_ref.focus())
            }),
        )
}

fn settings_nav_item(
    store: AppStore,
    section: SettingsSection,
    filter_query: Signal<String>,
    touch_sized: bool,
) -> impl IntoElement {
    let is_selected = computed(move || {
        filter_query.with(|q| q.trim().is_empty()) && store.settings_section.get() == section
    });
    let glyph = section_icon(section);
    let title_key = section.title_key();

    button()
        .test_id(desktop_nav_test_id(section))
        .on_press(move || {
            filter_query.set(String::new());
            store.settings_section.set(section);
            if section == SettingsSection::Cache {
                store.refresh_cache_stats();
            } else if section == SettingsSection::Plugins {
                store.refresh_sources();
            }
        })
        .w_full()
        .min_h(px(if touch_sized { 44.0 } else { 32.0 }))
        .px_2p5()
        .py_1()
        .flex_row()
        .items_center()
        .justify_start()
        .gap_2p5()
        .rounded(px(8.0))
        .bg(move || {
            if is_selected.get() {
                theme::selected()
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(120.0))
        .hover(move |s| {
            s.bg(if is_selected.get() {
                theme::selected()
            } else {
                theme::hover()
            })
        })
        .active(|s| s.bg(theme::press))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(crate::ui::icon_reactive(glyph, 15.0, move || {
            if is_selected.get() {
                theme::primary()
            } else {
                theme::muted_fg()
            }
        }))
        .child(
            text(move || store.t(title_key))
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(if touch_sized {
                    font_size::BODY
                } else {
                    font_size::SUBTITLE
                }))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || {
                    if is_selected.get() {
                        theme::fg()
                    } else {
                        theme::muted_fg()
                    }
                }),
        )
}

fn settings_detail_body(store: AppStore, form: DeviceForm, filter_query: Signal<String>) -> View {
    view().w_full().child(dynamic(move || {
        let q = filter_query.get().trim().to_ascii_lowercase();
        let active_section = store.settings_section.get();
        let _loc = store.locale.get();
        untracked(move || {
            if q.is_empty() {
                render_section_pane(store, form, active_section, "").into_element()
            } else {
                render_search_results(store, form, &q).into_element()
            }
        })
    }))
}

fn render_search_results(store: AppStore, form: DeviceForm, query: &str) -> View {
    let mut stack = view().w_full().gap_5();
    let mut visible_count = 0usize;

    if let Some(card) = build_general_section(store, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_plugins_section_filtered(store, form, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_playback_section(store, form, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_cache_section(store, form, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_local_library_section(store, form, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_experiments_section(store, query) {
        stack = stack.child(card);
        visible_count += 1;
    }
    if let Some(card) = build_about_section(store, form, query) {
        stack = stack.child(card);
        visible_count += 1;
    }

    if visible_count == 0 {
        return empty_state(
            Icon::Search,
            store.t("search.no_results"),
            store.t("settings.search_placeholder"),
        );
    }

    stack
}

fn render_section_pane(
    store: AppStore,
    form: DeviceForm,
    section: SettingsSection,
    query: &str,
) -> View {
    match section {
        SettingsSection::General => build_general_section(store, query).unwrap_or_else(view),
        SettingsSection::Plugins => plugins_pane_content(store, form, query),
        SettingsSection::Playback => {
            build_playback_section(store, form, query).unwrap_or_else(view)
        }
        SettingsSection::Cache => view()
            .w_full()
            .gap_5()
            .children(build_cache_section(store, form, query))
            .children(build_local_library_section(store, form, query)),
        SettingsSection::Experiments => {
            build_experiments_section(store, query).unwrap_or_else(view)
        }
        SettingsSection::About => build_about_section(store, form, query).unwrap_or_else(view),
    }
}

// -----------------------------------------------------------------------------
// Individual settings sections
// -----------------------------------------------------------------------------

fn matches_filter(query: &str, haystacks: &[&str]) -> bool {
    if query.is_empty() {
        return true;
    }
    haystacks
        .iter()
        .any(|h| h.to_ascii_lowercase().contains(query))
}

fn build_general_section(store: AppStore, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.general");
    let app_sec = store.t("settings.section.appearance");
    let theme_lbl = store.t("settings.theme");
    let locale_lbl = store.t("settings.locale");
    let player_style_lbl = store.t("settings.default_player_style");
    let stage_style_lbl = store.t("settings.default_stage_style");

    let section_hit = matches_filter(
        query,
        &[
            &sec_title,
            &app_sec,
            "appearance",
            "general",
            "通用",
            "外观",
        ],
    );
    let show_theme = section_hit
        || matches_filter(
            query,
            &[
                &theme_lbl, "theme", "light", "dark", "system", "主题", "深色", "浅色",
            ],
        );
    let show_locale = section_hit
        || matches_filter(
            query,
            &[&locale_lbl, "language", "locale", "english", "中文", "语言"],
        );
    let show_player_style = section_hit
        || matches_filter(
            query,
            &[
                &player_style_lbl,
                "style",
                "cover",
                "vinyl",
                "cassette",
                "turntable",
                "黑胶",
                "磁带",
                "封面",
                "播放页样式",
            ],
        );
    let show_stage_style = section_hit
        || matches_filter(
            query,
            &[
                &stage_style_lbl,
                "stage",
                "flow",
                "steps",
                "tilt",
                "舞台",
                "流光",
                "阶梯",
                "倾斜",
            ],
        );

    if !show_theme && !show_locale && !show_player_style && !show_stage_style {
        return None;
    }

    let mut body = view().w_full().gap_4();

    if show_locale {
        body = body.child(setting_row(
            move || store.t("settings.locale"),
            None,
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(pill_button(
                    move || store.t("settings.locale.zh_cn"),
                    move || store.locale.get() == Locale::ZhCn,
                    move || store.set_locale(Locale::ZhCn),
                ))
                .child(pill_button(
                    move || store.t("settings.locale.en"),
                    move || store.locale.get() == Locale::En,
                    move || store.set_locale(Locale::En),
                )),
        ));
    }

    if show_theme {
        body = body.child(setting_row(
            move || store.t("settings.theme"),
            None,
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(pill_button(
                    move || store.t("settings.theme.system"),
                    move || store.theme_override.get() == ThemeMode::System,
                    move || store.set_theme(ThemeMode::System),
                ))
                .child(pill_button(
                    move || store.t("settings.theme.light"),
                    move || store.theme_override.get() == ThemeMode::Light,
                    move || store.set_theme(ThemeMode::Light),
                ))
                .child(pill_button(
                    move || store.t("settings.theme.dark"),
                    move || store.theme_override.get() == ThemeMode::Dark,
                    move || store.set_theme(ThemeMode::Dark),
                )),
        ));
    }

    if show_player_style {
        body = body.child(setting_row(
            move || store.t("settings.default_player_style"),
            None,
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(pill_button(
                    move || store.t("now_playing.mode.cover"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_player_style == PlayerStyle::Cover)
                    },
                    move || store.set_now_playing_mode(NowPlayingMode::Cover),
                ))
                .child(pill_button(
                    move || store.t("now_playing.mode.vinyl"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_player_style == PlayerStyle::Vinyl)
                    },
                    move || store.set_now_playing_mode(NowPlayingMode::Vinyl),
                ))
                .child(pill_button(
                    move || store.t("now_playing.mode.cassette"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_player_style == PlayerStyle::Cassette)
                    },
                    move || store.set_now_playing_mode(NowPlayingMode::Cassette),
                )),
        ));
    }

    if show_stage_style {
        body = body.child(setting_row(
            move || store.t("settings.default_stage_style"),
            None,
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(pill_button(
                    move || store.t("now_playing.stage.flow"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_stage_style == StageStyle::Flow)
                    },
                    move || store.set_stage_style(NowPlayingStageStyle::Flow),
                ))
                .child(pill_button(
                    move || store.t("now_playing.stage.steps"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_stage_style == StageStyle::Steps)
                    },
                    move || store.set_stage_style(NowPlayingStageStyle::Steps),
                ))
                .child(pill_button(
                    move || store.t("now_playing.stage.tilt"),
                    move || {
                        store
                            .settings
                            .with(|s| s.default_stage_style == StageStyle::Tilt)
                    },
                    move || store.set_stage_style(NowPlayingStageStyle::Tilt),
                )),
        ));
    }

    Some(settings_card(
        move || store.t("settings.section.general"),
        Icon::Sliders,
        body,
    ))
}

fn build_playback_section(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.playback");
    let quality_lbl = store.t("settings.quality");
    let rg_lbl = store.t("settings.replay_gain");
    let rg_desc = store.t("settings.replay_gain.desc");
    let cf_lbl = store.t("settings.crossfade");
    let hot_lbl = store.t("settings.exp.hot_comments");
    let hot_desc = store.t("settings.exp.hot_comments.desc");
    let sparks_lbl = store.t("settings.stage_sparks");
    let sparks_desc = store.t("settings.stage_sparks_desc");

    let section_hit = matches_filter(query, &[&sec_title, "playback", "audio", "ffmpeg", "播放"]);
    let show_quality = section_hit
        || matches_filter(
            query,
            &[
                &quality_lbl,
                "quality",
                "lossless",
                "standard",
                "high",
                "音质",
                "无损",
            ],
        );
    let show_rg = section_hit
        || matches_filter(
            query,
            &[&rg_lbl, &rg_desc, "replaygain", "loudness", "响度"],
        );
    let show_cf =
        section_hit || matches_filter(query, &[&cf_lbl, "crossfade", "fade", "淡入", "淡出"]);
    let show_hot = section_hit
        || matches_filter(
            query,
            &[&hot_lbl, &hot_desc, "comments", "quiet", "热评", "间奏"],
        );
    let show_sparks = section_hit
        || matches_filter(
            query,
            &[&sparks_lbl, &sparks_desc, "sparks", "stage", "星火", "歌词"],
        );
    let show_engine =
        section_hit || matches_filter(query, &["engine", "symphonia", "ffmpeg", "decoder", "解码"]);

    if !show_quality && !show_rg && !show_cf && !show_hot && !show_sparks && !show_engine {
        return None;
    }

    let mut body = view().w_full().gap_4();

    if show_quality {
        let quality_items = vec![
            SelectItem::new("standard", store.t("player.quality.standard")),
            SelectItem::new("high", store.t("player.quality.high")),
            SelectItem::new("lossless", store.t("player.quality.lossless")),
        ];
        body = body.child(setting_row(
            move || store.t("settings.quality"),
            None,
            view()
                .w(px(if form.is_phone() { 180.0 } else { 220.0 }))
                .child(styled_select(
                    quality_items,
                    move || {
                        Some(SharedString::from(
                            store.settings.with(|s| s.audio_quality.as_str()),
                        ))
                    },
                    move || store.t("settings.quality"),
                    move || store.t("settings.quality"),
                    move |val| {
                        if let Some(v) = val {
                            store.set_quality(Quality::from_str_lossy(&v));
                        }
                    },
                )),
        ));
    }

    if show_rg {
        body = body.child(setting_row(
            move || store.t("settings.replay_gain"),
            Some(store.t("settings.replay_gain.desc")),
            styled_switch(
                move || store.settings.with(|s| s.replay_gain),
                move || {
                    if store.settings.with(|s| s.replay_gain) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |enabled| store.set_replay_gain(enabled),
            ),
        ));
    }

    if show_cf {
        body = body.child(
            view()
                .w_full()
                .gap_2()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            text(move || store.t("settings.crossfade"))
                                .text_size(px(font_size::BODY))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::fg),
                        )
                        .child(
                            text(move || {
                                let ms = store.settings.with(|s| s.crossfade_ms);
                                if ms == 0 {
                                    store.t("settings.crossfade.off")
                                } else {
                                    SharedString::from(format!("{:.1} s", ms as f32 / 1000.0))
                                }
                            })
                            .tabular_nums()
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::primary),
                        ),
                )
                .child(styled_slider(
                    move || store.settings.with(|s| s.crossfade_ms as f64),
                    0.0,
                    12000.0,
                    500.0,
                    move || store.t("settings.crossfade"),
                    move |val| store.set_crossfade_ms(val.round().max(0.0) as u32),
                )),
        );
    }

    if show_hot {
        body = body.child(setting_row(
            move || store.t("settings.exp.hot_comments"),
            Some(store.t("settings.exp.hot_comments.desc")),
            styled_switch(
                move || store.settings.with(|s| s.hot_comments),
                move || {
                    if store.settings.with(|s| s.hot_comments) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |enabled| store.set_hot_comments(enabled),
            ),
        ));
    }

    if show_sparks {
        body = body.child(setting_row(
            move || store.t("settings.stage_sparks"),
            Some(store.t("settings.stage_sparks_desc")),
            styled_switch(
                move || store.settings.with(|s| s.stage_sparks),
                move || {
                    if store.settings.with(|s| s.stage_sparks) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |enabled| store.set_stage_sparks(enabled),
            ),
        ));
    }

    if show_engine {
        let engine_summary = audio_engine_summary(store);
        if !engine_summary.is_empty() {
            body = body.child(
                view()
                    .w_full()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme::border)
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        text(move || store.t("player.local_device"))
                            .text_size(px(font_size::CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::muted_fg),
                    )
                    .child(
                        text(engine_summary)
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::muted_fg),
                    ),
            );
        }
    }

    Some(settings_card(
        move || store.t("settings.section.playback"),
        Icon::Music,
        body,
    ))
}

fn audio_engine_summary(store: AppStore) -> String {
    let caps = store.backend_caps.get_untracked();
    let mut engine_parts: Vec<String> = Vec::new();
    if !caps.active_output_backend.is_empty() {
        engine_parts.push(caps.active_output_backend);
    }
    if caps.symphonia_enabled {
        engine_parts.push("Symphonia".to_owned());
    }
    if caps.ffmpeg_enabled {
        let label = if caps.ffmpeg_version.is_empty() {
            "FFmpeg".to_owned()
        } else {
            format!("FFmpeg {}", caps.ffmpeg_version)
        };
        engine_parts.push(label);
    }
    if caps.virtual_ffmpeg_shim && store.settings.with(|s| s.mobile_virtual_compat) {
        engine_parts.push(store.t("plugins.compat_mobile_virtualized").to_string());
    }
    engine_parts.join(" · ")
}

fn build_local_library_section(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.library");
    let folders_lbl = store.t("settings.local_folders");
    let add_lbl = store.t("library.add_folder");
    let rescan_lbl = store.t("library.rescan");

    if !matches_filter(
        query,
        &[
            &sec_title,
            &folders_lbl,
            &add_lbl,
            &rescan_lbl,
            "local",
            "folder",
            "scan",
            "music",
            "本地",
            "文件夹",
            "扫描",
        ],
    ) {
        return None;
    }

    Some(local_music_folders_card(store, form))
}

fn build_cache_section(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.cache");
    let limit_lbl = store.t("settings.cache_limit");
    let clear_lbl = store.t("settings.clear_cache");

    if !matches_filter(
        query,
        &[
            &sec_title, &limit_lbl, &clear_lbl, "cache", "storage", "clear", "缓存", "清除",
        ],
    ) {
        return None;
    }

    // Values aligned cleanly to `AppStore::set_cache_max_mb`'s `128..=4096` MB range.
    let cache_items = vec![
        SelectItem::new("128", "128 MB"),
        SelectItem::new("256", "256 MB"),
        SelectItem::new("512", "512 MB"),
        SelectItem::new("1024", "1 GB"),
        SelectItem::new("2048", "2 GB"),
        SelectItem::new("4096", "4 GB"),
    ];

    let limit_row = setting_row(
        move || store.t("settings.cache_limit"),
        None,
        view()
            .w(px(if form.is_phone() { 180.0 } else { 220.0 }))
            .child(styled_select(
                cache_items,
                move || {
                    let mb = store.settings.with(|s| s.cache_max_mb);
                    let snapped = match mb {
                        0..=191 => "128",
                        192..=383 => "256",
                        384..=767 => "512",
                        768..=1535 => "1024",
                        1536..=3071 => "2048",
                        _ => "4096",
                    };
                    Some(SharedString::from(snapped))
                },
                move || store.t("settings.cache_limit"),
                move || store.t("settings.cache_limit"),
                move |val| {
                    if let Some(v) = val
                        && let Ok(mb) = v.parse::<u64>()
                    {
                        store.set_cache_max_mb(mb);
                    }
                },
            )),
    );

    let usage_row = setting_row(
        move || {
            let formatted = format_bytes(store.cache_used_bytes.get());
            store.t_fmt("settings.cache.used", &[("size", &formatted)])
        },
        None,
        ghost_button(
            move || store.t("settings.clear_cache"),
            move || {
                store.clear_audio_cache();
            },
        )
        .min_h(px(form.min_touch_target()))
        .disabled(move || store.cache_used_bytes.get() == 0),
    );

    Some(settings_card(
        move || store.t("settings.section.cache"),
        Icon::Database,
        view().w_full().gap_4().child(limit_row).child(usage_row),
    ))
}

fn build_experiments_section(store: AppStore, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.experiments");
    let fog_lbl = store.t("settings.exp.fog_backdrop");
    let fog_desc = store.t("settings.exp.fog_backdrop.desc");
    let smoke_lbl = store.t("settings.exp.smoke_progress");
    let smoke_desc = store.t("settings.exp.smoke_progress.desc");
    let grow_lbl = store.t("settings.exp.grow_from_cover");
    let grow_desc = store.t("settings.exp.grow_from_cover.desc");

    let section_hit = matches_filter(query, &[&sec_title, "experiment", "实验"]);
    let show_fog = section_hit
        || matches_filter(
            query,
            &[&fog_lbl, &fog_desc, "fog", "backdrop", "shader", "雾"],
        );
    let show_smoke = section_hit
        || matches_filter(
            query,
            &[
                &smoke_lbl,
                &smoke_desc,
                "smoke",
                "progress",
                "scrubber",
                "烟雾",
                "进度",
            ],
        );
    let show_grow = section_hit
        || matches_filter(
            query,
            &[&grow_lbl, &grow_desc, "grow", "cover", "封面", "展开"],
        );

    if !show_fog && !show_smoke && !show_grow {
        return None;
    }

    let mut body = view().w_full().gap_4();

    if show_fog {
        body = body.child(setting_row(
            move || store.t("settings.exp.fog_backdrop"),
            Some(store.t("settings.exp.fog_backdrop.desc")),
            styled_switch(
                move || store.settings.with(|s| s.fog_backdrop),
                move || {
                    if store.settings.with(|s| s.fog_backdrop) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |on| store.set_fog_backdrop(on),
            ),
        ));
    }

    if show_smoke {
        body = body.child(setting_row(
            move || store.t("settings.exp.smoke_progress"),
            Some(store.t("settings.exp.smoke_progress.desc")),
            styled_switch(
                move || store.settings.with(|s| s.smoke_progress),
                move || {
                    if store.settings.with(|s| s.smoke_progress) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |on| store.set_smoke_progress(on),
            ),
        ));
    }

    if show_grow {
        body = body.child(setting_row(
            move || store.t("settings.exp.grow_from_cover"),
            Some(store.t("settings.exp.grow_from_cover.desc")),
            styled_switch(
                move || store.settings.with(|s| s.grow_from_cover),
                move || {
                    if store.settings.with(|s| s.grow_from_cover) {
                        store.t("common.enabled")
                    } else {
                        store.t("common.disabled")
                    }
                },
                move |on| store.set_grow_from_cover(on),
            ),
        ));
    }

    Some(settings_card(
        move || store.t("settings.section.experiments"),
        Icon::Sparkles,
        body,
    ))
}

fn build_about_section(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let about_title = store.t("settings.section.about");
    let app_name = store.t("app.name");
    let update_lbl = store.t("plugins.check_update");

    if !matches_filter(
        query,
        &[
            &about_title,
            &app_name,
            &update_lbl,
            "about",
            "version",
            "update",
            "nsh music",
            "关于",
            "版本",
            "更新",
        ],
    ) {
        return None;
    }

    let app_version = format!("{} v{}", store.t("app.name"), env!("CARGO_PKG_VERSION"));
    let engine_summary = audio_engine_summary(store);
    let github_plugin_ids: Vec<String> = store.plugins.with_untracked(|list| {
        list.iter()
            .filter(|p| matches!(p.source, InstallSource::GitHubRepo { .. }))
            .map(|p| p.id.clone())
            .collect()
    });
    let has_github_plugins = !github_plugin_ids.is_empty();

    let mut body = view()
        .w_full()
        .gap_4()
        .child(setting_row(
            move || store.t("app.name"),
            Some(SharedString::from(app_version)),
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .children(has_github_plugins.then(move || {
                    let ids = github_plugin_ids.clone();
                    button()
                        .on_press(move || {
                            for pid in &ids {
                                store.check_plugin_update(pid);
                            }
                        })
                        .disabled(move || store.plugin_busy.get())
                        .min_h(px(form.min_touch_target()))
                        .px_3p5()
                        .py_1p5()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .rounded(px(radius::CONTROL))
                        .bg(theme::secondary)
                        .border_1()
                        .border_color(theme::border)
                        .hover(|s| s.bg(theme::accent))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .disabled_style(|s| s.opacity(0.6))
                        .child(when(
                            move || store.plugin_busy.get(),
                            || loading_spinner(14.0, colors::PRIMARY),
                        ))
                        .child(
                            text(move || store.t("plugins.check_update"))
                                .number_of_lines(1)
                                .text_size(px(font_size::BODY))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::fg),
                        )
                })),
        ))
        .child(setting_row(
            move || store.t("nav.sources"),
            Some(store.active_source_name()),
            text(move || {
                let count = store.plugins.with(Vec::len).to_string();
                store.t_fmt("settings.plugins_count", &[("count", &count)])
            })
            .text_size(px(font_size::CAPTION))
            .text_color(theme::muted_fg),
        ));

    if !engine_summary.is_empty() {
        body = body.child(setting_row(
            move || store.t("player.local_device"),
            None,
            text(engine_summary)
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg),
        ));
    }

    Some(settings_card(
        move || store.t("settings.section.about"),
        Icon::Info,
        body,
    ))
}

// -----------------------------------------------------------------------------
// Unified Plugins section inside Settings
// -----------------------------------------------------------------------------

fn build_plugins_section_filtered(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let plugins_title = store.t("settings.section.plugins");
    let install_lbl = store.t("plugins.install");
    let compat_lbl = store.t("plugins.mobile_virtual_compat");
    let section_hit = matches_filter(
        query,
        &[
            &plugins_title,
            &install_lbl,
            &compat_lbl,
            "plugin",
            "source",
            "github",
            "zip",
            "virtual",
            "path",
            "ffmpeg",
            "yt-dlp",
            "插件",
            "音乐源",
            "虚拟化",
            "兼容",
        ],
    );
    let has_matching_plugin = store
        .plugins
        .with_untracked(|list| list.iter().any(|p| plugin_matches_query(p, query)));

    if !section_hit && !has_matching_plugin {
        return None;
    }

    let filter = if section_hit { "" } else { query };
    Some(plugins_pane_content(store, form, filter))
}

fn plugin_matches_query(plugin: &PluginSummary, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_ascii_lowercase();
    if plugin.name.to_ascii_lowercase().contains(&q)
        || plugin.id.to_ascii_lowercase().contains(&q)
        || plugin.description.to_ascii_lowercase().contains(&q)
        || plugin.author.to_ascii_lowercase().contains(&q)
    {
        return true;
    }
    plugin.settings.iter().any(|st| {
        st.label.to_ascii_lowercase().contains(&q)
            || st.help.to_ascii_lowercase().contains(&q)
            || st.key.to_ascii_lowercase().contains(&q)
    })
}

fn plugins_pane_content(store: AppStore, form: DeviceForm, query: &str) -> View {
    let tasks = PagesOwner::capture();
    let mobile_ui = is_mobile_ui(store, form);
    let query_owned = query.to_owned();

    let busy_banner = when(
        move || store.plugin_busy.get(),
        move || {
            view()
                .w_full()
                .px_3p5()
                .py_2p5()
                .rounded(px(radius::CONTROL))
                .bg(theme::primary_soft)
                .border_1()
                .border_color(theme::primary)
                .flex_row()
                .items_center()
                .gap_2()
                .child(loading_spinner(15.0, colors::PRIMARY))
                .child(
                    text(move || store.t("common.loading"))
                        .text_size(px(font_size::SUBTITLE))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::primary),
                )
        },
    );

    let error_banner = when(
        move || store.plugin_error.with(Option::is_some),
        move || {
            view()
                .w_full()
                .px_3p5()
                .py_2p5()
                .rounded(px(radius::CONTROL))
                .bg(theme::danger_soft)
                .border_1()
                .border_color(theme::danger)
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    text(move || store.plugin_error.get().unwrap_or_default())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::danger),
                )
                .child(icon_button_sized(
                    Icon::Close,
                    32.0,
                    14.0,
                    theme::danger,
                    move || store.t("common.close"),
                    move || store.plugin_error.set(None),
                ))
        },
    );

    let is_phone = form.is_phone();
    let virtual_compat_card = Elevated::elevation(
        view()
            .w_full()
            .p(px(if is_phone { 15.0 } else { 18.0 }))
            .rounded(px(radius::XL))
            .bg(theme::card)
            .border_1()
            .border_color(theme::border),
        Elevation::Subtle,
    )
    .child(
        view()
            .w_full()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_wrap()
            .gap_3()
            .child(
                view()
                    .flex_1()
                    .min_w(px(if is_phone { 180.0 } else { 220.0 }))
                    .gap_1()
                    .child(
                        text(move || store.t("plugins.mobile_virtual_compat"))
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::fg),
                    )
                    .child(
                        text(move || store.t("plugins.mobile_virtual_compat.desc"))
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::muted_fg),
                    ),
            )
            .child(
                styled_switch(
                    move || store.settings.with(|s| s.mobile_virtual_compat),
                    move || {
                        if store.settings.with(|s| s.mobile_virtual_compat) {
                            store.t("common.enabled")
                        } else {
                            store.t("common.disabled")
                        }
                    },
                    move |next| store.set_mobile_virtual_compat(next),
                )
                .test_id("plugins-mobile-virtual-compat-switch")
                .min_h(px(form.min_touch_target())),
            ),
    );

    let installed_section = settings_card(
        move || store.t("settings.section.plugins"),
        Icon::Plugin,
        view().w_full().child(dynamic(move || {
            let plugins = store.plugins.get();
            let sources = store.sources.get();
            let _loc = store.locale.get();
            let _virtual_compat = store.settings.with(|s| s.mobile_virtual_compat);
            let q = query_owned.clone();
            untracked(move || {
                let filtered: Vec<PluginSummary> = plugins
                    .into_iter()
                    .filter(|p| plugin_matches_query(p, &q))
                    .collect();

                if filtered.is_empty() {
                    let empty_sub = if mobile_ui {
                        store.t("dialog.install_plugin.desc")
                    } else {
                        store.t("plugins.empty_subtitle")
                    };
                    let actions = view()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .flex_wrap()
                        .gap_2p5()
                        .child(
                            primary_button(
                                move || store.t("plugins.from_github"),
                                move || store.open_modal(ActiveModal::InstallPlugin),
                            )
                            .min_h(px(form.min_touch_target())),
                        )
                        .child(
                            ghost_button(
                                move || store.t("plugins.from_zip"),
                                move || store.pick_and_stage_plugin_zip(),
                            )
                            .min_h(px(form.min_touch_target())),
                        );
                    return empty_state_with_actions(
                        Icon::Plugin,
                        store.t("plugins.empty_title"),
                        empty_sub,
                        actions,
                    )
                    .into_element();
                }

                let mut list = view().w_full().gap_4();
                for plugin in filtered {
                    let source_entry = sources.iter().find(|s| s.info.id == plugin.id).cloned();
                    list = list.child(plugin_card(
                        store,
                        form,
                        tasks,
                        plugin,
                        source_entry,
                        mobile_ui,
                    ));
                }
                list.into_element()
            })
        })),
    );

    view()
        .w_full()
        .gap_4()
        .child(import_toolbar(store, form, mobile_ui))
        .child(busy_banner)
        .child(error_banner)
        .child(virtual_compat_card)
        .child(installed_section)
}

fn import_toolbar(store: AppStore, form: DeviceForm, mobile_ui: bool) -> View {
    let min_h = px(form.min_touch_target());
    Elevated::elevation(
        view()
            .w_full()
            .p_3p5()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap_2p5()
            .rounded(px(radius::XL))
            .bg(theme::card)
            .border_1()
            .border_color(theme::border),
        Elevation::Subtle,
    )
    .child(
        primary_button(
            move || store.t("plugins.from_github"),
            move || store.open_modal(ActiveModal::InstallPlugin),
        )
        .min_h(min_h)
        .test_id("plugins-btn-github"),
    )
    .child(
        toolbar_action_button(
            Icon::Download,
            move || store.t("plugins.from_zip"),
            form,
            move || store.pick_and_stage_plugin_zip(),
        )
        .test_id("plugins-btn-zip"),
    )
    .child(
        toolbar_action_button(
            Icon::Folder,
            move || store.t("plugins.from_folder"),
            form,
            move || store.pick_and_stage_plugin_folder(),
        )
        .test_id("plugins-btn-folder"),
    )
    .children((!mobile_ui).then(|| {
        toolbar_action_button(
            Icon::External,
            move || store.t("plugins.open_folder"),
            form,
            move || store.open_plugins_folder(),
        )
        .test_id("plugins-btn-open-dir")
    }))
    .child(refresh_pill_button(
        store,
        form,
        store.plugin_busy,
        move || store.refresh_sources(),
    ))
}

fn toolbar_action_button(
    glyph: Icon,
    label: impl IntoReactive<SharedString>,
    form: DeviceForm,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    button()
        .on_press(on_press)
        .min_h(px(form.min_touch_target()))
        .px_3p5()
        .py_2()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::accent))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(icon(glyph, 16.0, colors::FOREGROUND))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::fg),
        )
}

fn local_music_folders_card(store: AppStore, form: DeviceForm) -> View {
    let folders_sig = computed(move || store.settings.with(|s| s.local_folders.clone()));
    let folders_list = view().w_full().child(dynamic(move || {
        let folders = folders_sig.get();
        let _loc = store.locale.get();
        untracked(move || {
            if folders.is_empty() {
                return text(move || store.t("settings.local_folders.empty"))
                    .text_size(px(font_size::SUBTITLE))
                    .text_color(theme::muted_fg)
                    .into_element();
            }

            let mut list = view().w_full().gap_2();
            for path in folders {
                let display = path.to_string_lossy().into_owned();
                let remove_path = path.clone();
                list = list.child(
                    view()
                        .w_full()
                        .min_h(px(form.min_touch_target()))
                        .px_3()
                        .py_2()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2p5()
                        .rounded(px(radius::CONTROL))
                        .bg(theme::secondary)
                        .border_1()
                        .border_color(theme::border)
                        .child(icon(Icon::Folder, 16.0, colors::MUTED_FOREGROUND))
                        .child(
                            text(display)
                                .flex_1()
                                .min_w(px(0.0))
                                .number_of_lines(1)
                                .text_size(px(font_size::SUBTITLE))
                                .text_color(theme::fg),
                        )
                        .child(icon_button_sized(
                            Icon::Close,
                            32.0,
                            15.0,
                            theme::danger,
                            move || store.t("common.remove"),
                            move || store.remove_local_folder(&remove_path),
                        )),
                );
            }
            list.into_element()
        })
    }));

    let rescan_btn = button()
        .on_press(move || store.rescan_local_folders())
        .disabled(move || store.browse_loading.get())
        .min_h(px(form.min_touch_target()))
        .min_w(px(88.0))
        .px_3p5()
        .py_1p5()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::accent))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.65))
        .child(when(
            move || store.browse_loading.get(),
            || loading_spinner(14.0, colors::PRIMARY),
        ))
        .child(
            text(move || {
                if store.browse_loading.get() {
                    store.t("common.loading")
                } else {
                    store.t("library.rescan")
                }
            })
            .number_of_lines(1)
            .text_size(px(font_size::BODY))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::fg),
        );

    let actions = view()
        .w_full()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_2p5()
        .child(
            primary_button(
                move || store.t("library.add_folder"),
                move || {
                    store.pick_local_music_folders();
                },
            )
            .min_h(px(form.min_touch_target())),
        )
        .child(rescan_btn);

    settings_card(
        move || store.t("settings.section.library"),
        Icon::Library,
        view().w_full().gap_3p5().child(folders_list).child(actions),
    )
}

fn plugin_card(
    store: AppStore,
    form: DeviceForm,
    tasks: PagesOwner,
    plugin: PluginSummary,
    source_entry: Option<crate::state::SourceEntry>,
    mobile_ui: bool,
) -> View {
    let pid = plugin.id.clone();
    let pid_toggle = pid.clone();
    let pid_grant = pid.clone();
    let pid_update = pid.clone();
    let pid_reveal = pid.clone();
    let pid_reload = pid.clone();
    let pid_config = pid.clone();
    let pid_signin = pid.clone();
    let pid_signout = pid.clone();
    let pid_remove = pid.clone();
    let signing_in = signal(false);

    let (badge_level, badge_label) = compat_badge_meta(store, &plugin);

    let account = source_entry.as_ref().and_then(|e| e.account.clone());
    let signed_in_label: Option<SharedString> = account
        .as_ref()
        .filter(|a| !a.name.is_empty())
        .map(|a| store.t_fmt("plugins.signed_in_as", &[("name", &a.name)]));
    let is_signed_in = signed_in_label.is_some();

    let supports_sign_in = source_entry.as_ref().is_some_and(|e| {
        e.capabilities.contains("signIn")
            || e.capabilities.contains("qrSignIn")
            || e.capabilities.contains("account")
    });
    let supports_qr = source_entry
        .as_ref()
        .is_some_and(|e| e.capabilities.contains("qrSignIn"));

    let github_repo: Option<(String, String)> = match &plugin.source {
        InstallSource::GitHubRepo { owner, repo, .. } => Some((
            format!("{owner}/{repo}"),
            format!("https://github.com/{owner}/{repo}"),
        )),
        _ => None,
    };
    let is_github_source = github_repo.is_some();
    let local_origin_label: Option<SharedString> = match &plugin.source {
        InstallSource::LocalDir { .. } => Some(store.t("plugins.source_local")),
        InstallSource::ZipArchive { .. } => Some(store.t("plugins.source_zip")),
        InstallSource::GitHubRepo { .. } | InstallSource::Discovered => None,
    };

    let external_cmds: Vec<String> = plugin
        .report
        .external_commands
        .iter()
        .filter(|c| {
            !nsh_music_plugin::compat::is_ffmpeg_binary_name(c)
                && !nsh_music_plugin::compat::is_ytdlp_binary_name(c)
        })
        .cloned()
        .collect();
    let has_external_programs = plugin.report.uses_external_spawn || !external_cmds.is_empty();
    let external_cmds_str = external_cmds.join(", ");
    let is_granted = plugin.granted;
    let plugin_name_for_remove = plugin.name.clone();
    let pid_active = pid.clone();
    let pid_switch = pid.clone();
    let is_active_source =
        computed(move || store.active_source_id.with(|active| active == &pid_active));
    let has_plugin_settings = !plugin.settings.is_empty();

    let is_phone = form.is_phone();
    let mut card = view()
        .w_full()
        .p(px(if is_phone { 14.0 } else { 16.0 }))
        .gap(px(if is_phone { 12.0 } else { 14.0 }))
        .rounded(px(radius::CARD))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border);

    let pid_toggle_phone = pid_toggle.clone();
    // Header row: Name, version, author, origin, compat badge + Enable switch
    let mut meta_col = view()
        .flex_1()
        .min_w(px(if is_phone { 0.0 } else { 180.0 }))
        .gap_1()
        .child(if is_phone {
            view()
                .w_full()
                .gap_1p5()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            view()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex_row()
                                .items_center()
                                .flex_wrap()
                                .gap_1p5()
                                .child(
                                    text(plugin.name.clone())
                                        .number_of_lines(1)
                                        .text_size(px(font_size::HEADING))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme::fg),
                                )
                                .children((!plugin.version.is_empty()).then(|| {
                                    text(format!("v{}", plugin.version))
                                        .text_size(px(font_size::CAPTION))
                                        .text_color(theme::muted_fg)
                                })),
                        )
                        .child(styled_switch(
                            plugin.enabled,
                            if plugin.enabled {
                                store.t("common.enabled")
                            } else {
                                store.t("common.disabled")
                            },
                            move |next| store.set_plugin_enabled(&pid_toggle_phone, next),
                        )),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .flex_wrap()
                        .gap_1p5()
                        .child(compat_badge_text(badge_level, badge_label)),
                )
        } else {
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    text(plugin.name.clone())
                        .text_size(px(font_size::HEADING))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .children((!plugin.version.is_empty()).then(|| {
                    text(format!("v{}", plugin.version))
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg)
                }))
                .child(compat_badge_text(badge_level, badge_label))
        });

    if !plugin.author.is_empty() || github_repo.is_some() || local_origin_label.is_some() {
        let mut sub_row = view().flex_row().items_center().flex_wrap().gap_2();
        if !plugin.author.is_empty() {
            sub_row = sub_row.child(
                text(format!("{}: {}", store.t("plugins.author"), plugin.author))
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg),
            );
        }
        if let Some((slug, url)) = github_repo {
            sub_row = sub_row.child(
                button()
                    .on_press(move || store.open_external_url(&url))
                    .min_h(px(if form.is_touch() { 44.0 } else { 22.0 }))
                    .px_1()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .bg(colors::TRANSPARENT)
                    .hover(|s| s.opacity(0.8))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(icon(Icon::External, 12.0, colors::PRIMARY))
                    .child(
                        text(slug)
                            .text_size(px(font_size::CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary),
                    ),
            );
        } else if let Some(orig) = local_origin_label {
            sub_row = sub_row.child(
                text(orig)
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg),
            );
        }
        meta_col = meta_col.child(sub_row);
    }

    if !plugin.description.is_empty() {
        meta_col = meta_col.child(
            text(plugin.description.clone())
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg),
        );
    }

    if !plugin.compat.summary.is_empty() {
        meta_col = meta_col.child(
            text(plugin.compat.summary.clone())
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg),
        );
    }

    if let Some(acc) = signed_in_label {
        meta_col = meta_col.child(
            text(acc)
                .text_size(px(font_size::CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::ok),
        );
    }

    card = card.child(if is_phone {
        meta_col.w_full()
    } else {
        view()
            .w_full()
            .flex_row()
            .items_start()
            .justify_between()
            .flex_wrap()
            .gap_3()
            .child(meta_col)
            .child(styled_switch(
                plugin.enabled,
                if plugin.enabled {
                    store.t("common.enabled")
                } else {
                    store.t("common.disabled")
                },
                move |next| store.set_plugin_enabled(&pid_toggle, next),
            ))
    });

    // Load error banner
    if let Some(err) = &plugin.load_error {
        card = card.child(
            view()
                .w_full()
                .p_3()
                .rounded(px(radius::CONTROL))
                .bg(theme::danger_soft)
                .border_1()
                .border_color(theme::danger)
                .child(
                    text(err.clone())
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::danger),
                ),
        );
    }

    // Per-plugin runtime / PATH command error banner
    if let Some(err) = &plugin.runtime_error {
        let err_text = err.clone();
        card = card.child(
            view()
                .test_id(&format!("plugin-runtime-error-{}", plugin.id))
                .w_full()
                .p_3()
                .gap_1()
                .rounded(px(radius::CONTROL))
                .bg(theme::danger_soft)
                .border_1()
                .border_color(theme::danger)
                .child(
                    text(move || store.t("plugins.runtime_error_title"))
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::danger),
                )
                .child(
                    text(err_text)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::danger),
                ),
        );
    }

    // External programs permission warning & Allow Programs / Stop Allowing toggle
    if has_external_programs {
        let warn_msg = store.t_fmt(
            "plugins.programs_warning",
            &[("commands", &external_cmds_str)],
        );
        let toggle_label = if is_granted {
            store.t("plugins.stop_allowing")
        } else {
            store.t("plugins.allow_programs")
        };
        card = card.child(
            view()
                .w_full()
                .p_3()
                .rounded(px(radius::CONTROL))
                .bg(if is_granted {
                    theme::warn_soft()
                } else {
                    theme::danger_soft()
                })
                .border_1()
                .border_color(if is_granted {
                    theme::warn()
                } else {
                    theme::danger()
                })
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_2p5()
                .child(
                    text(warn_msg)
                        .flex_1()
                        .min_w(px(180.0))
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if is_granted {
                            theme::warn()
                        } else {
                            theme::danger()
                        }),
                )
                .child(
                    ghost_button(toggle_label, move || {
                        store.set_plugin_granted(&pid_grant, !is_granted);
                    })
                    .min_h(px(form.min_touch_target())),
                ),
        );
    }

    // Action row: Switch to Source, Sign In/Out, Settings, Check Update (GitHub),
    // Show Folder (Desktop only), Reload, Remove
    let min_h = px(form.min_touch_target());
    let mut actions = view()
        .w_full()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_2();

    if plugin.loaded {
        actions = actions.child(
            when(
                move || is_active_source.get(),
                move || {
                    view()
                        .min_h(min_h)
                        .px_3()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .rounded(px(radius::CONTROL))
                        .bg(theme::primary_soft)
                        .child(icon(Icon::Check, 14.0, colors::PRIMARY))
                        .child(
                            text(move || store.t("common.enabled"))
                                .number_of_lines(1)
                                .text_size(px(font_size::CAPTION))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary),
                        )
                },
            )
            .otherwise(move || {
                let pid_press = pid_switch.clone();
                ghost_button(
                    move || store.t("dialog.source_switcher.title"),
                    move || {
                        store.select_source(&pid_press);
                    },
                )
                .min_h(min_h)
            }),
        );
    }

    if is_signed_in {
        actions = actions.child(
            ghost_button(
                move || store.t("plugins.sign_out"),
                move || {
                    store.sign_out_source(&pid_signout);
                },
            )
            .min_h(min_h),
        );
    } else if supports_sign_in && plugin.loaded {
        let pid_check = pid.clone();
        let is_signing_in = computed(move || {
            signing_in.get()
                || store
                    .signing_in_source
                    .with(|s| s.as_deref() == Some(pid_check.as_str()))
        });
        actions = actions.child(
            button()
                .on_press(move || {
                    if supports_qr {
                        signing_in.set(true);
                        let mgr = store.plugin_manager();
                        let source_id = pid_signin.clone();
                        tasks.spawn(async move {
                            let sid_bg = source_id.clone();
                            let qr_res =
                                blocking(move || mgr.get(&sid_bg).map(|p| p.get_qr_sign_in()))
                                    .await;
                            signing_in.set(false);
                            match qr_res {
                                Ok(Some(Ok(session))) => {
                                    store.open_modal(ActiveModal::QrSignIn {
                                        source_id,
                                        prompt: SignInPrompt {
                                            qr: session.qr_url,
                                            text: session.message,
                                        },
                                    });
                                }
                                _ => {
                                    store.sign_in_source(&source_id);
                                }
                            }
                        });
                    } else {
                        store.sign_in_source(&pid_signin);
                    }
                })
                .disabled(move || is_signing_in.get())
                .min_h(min_h)
                .min_w(px(88.0))
                .px_4()
                .py_1p5()
                .flex_row()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded(px(radius::CONTROL))
                .bg(theme::primary)
                .transition_all()
                .duration(move || motion::transition_ms(140.0))
                .hover(|s| s.bg(theme::primary_hover))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .disabled_style(|s| s.opacity(0.65))
                .child(when(
                    move || is_signing_in.get(),
                    || loading_spinner(14.0, colors::PRIMARY_FOREGROUND),
                ))
                .child(
                    text(move || {
                        if is_signing_in.get() {
                            store.t("common.loading")
                        } else {
                            store.t("plugins.sign_in")
                        }
                    })
                    .number_of_lines(1)
                    .text_size(px(font_size::BODY))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::primary_fg),
                ),
        );
    }

    if has_plugin_settings {
        actions = actions.child(
            ghost_button(
                move || store.t("plugins.configure"),
                move || {
                    store.open_modal(ActiveModal::PluginConfig {
                        plugin_id: pid_config.clone(),
                    });
                },
            )
            .min_h(min_h),
        );
    }

    if is_github_source {
        actions = actions.child(
            ghost_button(
                move || store.t("plugins.check_update"),
                move || {
                    store.check_plugin_update(&pid_update);
                },
            )
            .disabled(move || store.plugin_busy.get())
            .min_h(min_h),
        );
    }

    if !mobile_ui {
        actions = actions.child(
            ghost_button(
                move || store.t("plugins.show_folder"),
                move || {
                    store.reveal_plugin_folder(&pid_reveal);
                },
            )
            .min_h(min_h),
        );
    }

    actions = actions
        .child(
            ghost_button(
                move || store.t("plugins.reload"),
                move || {
                    store.reload_plugin(&pid_reload);
                },
            )
            .disabled(move || store.plugin_busy.get())
            .min_h(min_h),
        )
        .child(
            button()
                .on_press(move || {
                    let title = store.t_fmt(
                        "plugins.confirm_remove.title",
                        &[("name", &plugin_name_for_remove)],
                    );
                    let desc = store.t("plugins.confirm_remove.desc");
                    let cancel = store.t("common.cancel");
                    let confirm = store.t("plugins.uninstall");
                    let pid_confirmed = pid_remove.clone();
                    confirm_destructive(title, desc, cancel, confirm, move || {
                        store.uninstall_plugin(&pid_confirmed);
                    });
                })
                .min_h(min_h)
                .px_3()
                .py_2()
                .flex_row()
                .items_center()
                .justify_center()
                .gap_1p5()
                .rounded(px(radius::CONTROL))
                .bg(colors::TRANSPARENT)
                .transition_all()
                .duration(move || motion::transition_ms(140.0))
                .hover(|s| s.bg(theme::danger_soft))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(icon(Icon::Trash, 15.0, colors::DANGER))
                .child(
                    text(move || store.t("plugins.uninstall"))
                        .number_of_lines(1)
                        .text_size(px(font_size::BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::danger),
                ),
        );

    card = card.child(actions);

    // Inline typed SettingDef controls directly inside the plugin card
    if !plugin.settings.is_empty() {
        card = card.child(
            view()
                .w_full()
                .pt_3()
                .border_t_1()
                .border_color(theme::border)
                .gap_3()
                .child(
                    text(move || store.t("plugins.configure"))
                        .text_size(px(font_size::SUBTITLE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::fg),
                )
                .child(render_typed_settings_form(
                    store,
                    form,
                    plugin.id.clone(),
                    plugin.settings,
                    false,
                )),
        );
    }

    card
}

fn compat_badge_meta(store: AppStore, plugin: &PluginSummary) -> (PluginCompatLevel, SharedString) {
    let virtual_compat = store.settings.with(|s| s.mobile_virtual_compat);
    if !virtual_compat && plugin.compat.level == PluginCompatLevel::VirtualFfmpeg {
        return (
            PluginCompatLevel::DesktopProcessLimited,
            store.t("compat.desktop_limited"),
        );
    }
    match plugin.report.tier {
        CompatTier::MigrationRequired => (
            PluginCompatLevel::DesktopProcessLimited,
            store.t("compat.migration_required"),
        ),
        CompatTier::Partial => {
            if plugin.compat.level == PluginCompatLevel::VirtualFfmpeg {
                (
                    PluginCompatLevel::VirtualFfmpeg,
                    store.t("plugins.compat_mobile_virtualized"),
                )
            } else {
                (PluginCompatLevel::VirtualFfmpeg, store.t("compat.partial"))
            }
        }
        CompatTier::Full => match plugin.compat.level {
            PluginCompatLevel::VirtualFfmpeg => (
                PluginCompatLevel::VirtualFfmpeg,
                store.t("plugins.compat_mobile_virtualized"),
            ),
            PluginCompatLevel::DesktopProcessLimited => (
                PluginCompatLevel::DesktopProcessLimited,
                store.t("compat.desktop_limited"),
            ),
            PluginCompatLevel::FullNative => {
                (PluginCompatLevel::FullNative, store.t("compat.full_native"))
            }
        },
    }
}

/// Renders typed `SettingDef` controls (`boolean`, `select`, `password`, text)
/// with `help` text, clickable `link` via `store.open_external_url`, and
/// dirty-tracked Save / Discard buttons.
fn render_typed_settings_form(
    store: AppStore,
    form: DeviceForm,
    plugin_id: String,
    defs: Vec<SettingDef>,
    close_modal_on_save: bool,
) -> View {
    struct TextFieldState {
        key: String,
        draft: Signal<String>,
        saved: Signal<String>,
    }

    let mut text_fields: Vec<TextFieldState> = Vec::new();
    let mut stack = view().w_full().gap_3p5();

    for def in defs {
        let initial = store.plugin_setting(&plugin_id, &def.key);
        let label = if def.label.is_empty() {
            def.key.clone()
        } else {
            def.label.clone()
        };

        let key = def.key.clone();
        let pid_field = plugin_id.clone();
        let kind_lower = def.kind.to_ascii_lowercase();

        let field_block = match kind_lower.as_str() {
            "boolean" | "bool" | "switch" => {
                let checked = signal(matches!(
                    initial.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes"
                ));
                let ctrl = styled_switch(
                    move || checked.get(),
                    move || {
                        if checked.get() {
                            store.t("common.enabled")
                        } else {
                            store.t("common.disabled")
                        }
                    },
                    move |on| {
                        checked.set(on);
                        store.set_plugin_setting(
                            &pid_field,
                            &key,
                            if on { "true" } else { "false" },
                        );
                    },
                );
                setting_row(
                    label,
                    (!def.help.is_empty()).then(|| SharedString::from(def.help.clone())),
                    ctrl,
                )
            }
            "select" if !def.options.is_empty() => {
                let current = signal(initial);
                let items: Vec<SelectItem> = def
                    .options
                    .iter()
                    .map(|o| SelectItem::new(o.value.clone(), o.label.clone()))
                    .collect();
                let ctrl = view()
                    .w(px(if form.is_phone() { 180.0 } else { 220.0 }))
                    .child(styled_select(
                        items,
                        move || {
                            let cur = current.get();
                            (!cur.is_empty()).then(|| SharedString::from(cur))
                        },
                        label.clone(),
                        label.clone(),
                        move |val| {
                            if let Some(v) = val {
                                current.set(v.to_string());
                                store.set_plugin_setting(&pid_field, &key, &v);
                            }
                        },
                    ));
                setting_row(
                    label,
                    (!def.help.is_empty()).then(|| SharedString::from(def.help.clone())),
                    ctrl,
                )
            }
            _ => {
                let is_secret = matches!(kind_lower.as_str(), "password" | "secret");
                let draft = signal(initial.clone());
                let saved = signal(initial);
                text_fields.push(TextFieldState {
                    key: def.key.clone(),
                    draft,
                    saved,
                });

                let mut input_el = styled_input(draft, label.clone())
                    .w_full()
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false);
                if is_secret {
                    input_el = input_el.masked(true);
                }

                view()
                    .w_full()
                    .gap_1p5()
                    .child(
                        text(label)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::fg),
                    )
                    .child(input_el)
                    .children((!def.help.is_empty()).then(|| {
                        text(def.help.clone())
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::muted_fg)
                    }))
            }
        };

        let mut wrapper = view().w_full().gap_1p5().child(field_block);
        if !def.link.trim().is_empty() {
            let link_url = def.link.trim().to_owned();
            let link_display = link_url.clone();
            wrapper = wrapper.child(
                button()
                    .on_press(move || store.open_external_url(&link_url))
                    .self_start()
                    .min_h(px(if form.is_touch() { 44.0 } else { 26.0 }))
                    .px_1()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .bg(colors::TRANSPARENT)
                    .hover(|s| s.opacity(0.8))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(icon(Icon::External, 13.0, colors::PRIMARY))
                    .child(
                        text(link_display)
                            .number_of_lines(1)
                            .text_size(px(font_size::CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary),
                    ),
            );
        }

        stack = stack.child(wrapper);
    }

    if !text_fields.is_empty() {
        let fields_rc = Rc::new(text_fields);
        let fields_dirty = Rc::clone(&fields_rc);
        let fields_save = Rc::clone(&fields_rc);
        let fields_discard = Rc::clone(&fields_rc);
        let pid_save = plugin_id;

        let is_dirty =
            computed(move || fields_dirty.iter().any(|f| f.draft.get() != f.saved.get()));

        let save_changes = move || {
            dismiss_keyboard();
            for f in fields_save.iter() {
                let next = f.draft.get_untracked();
                if next != f.saved.get_untracked() {
                    store.set_plugin_setting(&pid_save, &f.key, &next);
                    f.saved.set(next);
                }
            }
            store.refresh_sources();
            if close_modal_on_save {
                store.close_modal();
            }
        };

        let discard_changes = move || {
            for f in fields_discard.iter() {
                f.draft.set(f.saved.get_untracked());
            }
            if close_modal_on_save {
                store.close_modal();
            }
        };

        let min_h = px(form.min_touch_target());
        if close_modal_on_save {
            stack = stack.child(
                view()
                    .w_full()
                    .pt_2()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        ghost_button(move || store.t("common.cancel"), discard_changes)
                            .min_h(min_h),
                    )
                    .child(
                        primary_button(move || store.t("common.save_changes"), save_changes)
                            .min_h(min_h),
                    ),
            );
        } else {
            stack = stack.child(when(
                move || is_dirty.get(),
                move || {
                    view()
                        .w_full()
                        .pt_1()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            ghost_button(
                                move || store.t("common.discard_changes"),
                                discard_changes.clone(),
                            )
                            .min_h(min_h),
                        )
                        .child(
                            primary_button(
                                move || store.t("common.save_changes"),
                                save_changes.clone(),
                            )
                            .min_h(min_h),
                        )
                },
            ));
        }
    } else if close_modal_on_save {
        stack = stack.child(
            view()
                .w_full()
                .pt_2()
                .flex_row()
                .items_center()
                .justify_end()
                .child(
                    primary_button(move || store.t("common.done"), move || store.close_modal())
                        .min_h(px(form.min_touch_target())),
                ),
        );
    }

    stack
}

// -----------------------------------------------------------------------------
// Shared layout primitives
// -----------------------------------------------------------------------------

fn settings_card(title: impl IntoReactive<SharedString>, glyph: Icon, body: View) -> View {
    let is_phone = theme::detect_device_form().is_phone();
    Elevated::elevation(
        view()
            .w_full()
            .p(px(if is_phone { 15.0 } else { 20.0 }))
            .gap(px(if is_phone { 14.0 } else { 16.0 }))
            .rounded(px(radius::XL))
            .bg(theme::card)
            .border_1()
            .border_color(theme::border),
        Elevation::Subtle,
    )
    .child(
        view()
            .flex_row()
            .items_center()
            .gap_2p5()
            .child(icon(glyph, 18.0, colors::PRIMARY))
            .child(
                text(title)
                    .role(AccessibilityRole::Header)
                    .text_size(px(font_size::HEADING))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::fg),
            ),
    )
    .child(body)
}

fn setting_row(
    label: impl IntoReactive<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
) -> View {
    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_wrap()
        .gap_3()
        .child(
            view()
                .flex_1()
                .min_w(px(160.0))
                .gap_0p5()
                .child(
                    text(label)
                        .text_size(px(font_size::BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::fg),
                )
                .children(description.map(|desc| {
                    text(desc)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg)
                })),
        )
        .child(control)
}

/// Modal primary action button with guaranteed `>= 88x34pt` bounds and an
/// inline animated `loading_spinner` while `busy` is true.
fn busy_primary_button(
    test_id: &'static str,
    busy: Signal<bool>,
    disabled: impl IntoReactive<bool>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let disabled_rx = disabled.into_reactive();
    button()
        .test_id(test_id)
        .on_press(on_press)
        .disabled(move || disabled_rx.get())
        .min_w(px(88.0))
        .min_h(move || px(theme::detect_device_form().min_touch_target().max(34.0)))
        .px_4()
        .py_1p5()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(theme::primary)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::primary_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(when(
            move || busy.get(),
            || loading_spinner(14.0, colors::PRIMARY_FOREGROUND),
        ))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::primary_fg),
        )
}

// -----------------------------------------------------------------------------
// Modal contents rendered by `crate::pages::render_active_modal` / `render_settings_modal`
// -----------------------------------------------------------------------------

/// Install Plugin modal accepting GitHub `owner/repo`, `owner/repo@ref`, URL,
/// or local folder / `.zip` path and staging for permission confirmation.
pub fn install_plugin_modal(store: AppStore) -> View {
    let spec = signal(String::new());
    let is_phone = theme::detect_device_form().is_phone();

    let submit_spec = move || {
        let raw = spec.with_untracked(|s| s.trim().to_owned());
        if raw.is_empty() || store.plugin_busy.get_untracked() {
            return;
        }
        dismiss_keyboard();
        let path = Path::new(&raw);
        if path.is_dir() {
            store.stage_local_plugin_dir(path.to_path_buf());
        } else if path.is_file() && raw.to_ascii_lowercase().ends_with(".zip") {
            store.stage_zip_plugin(path.to_path_buf());
        } else {
            store.stage_github_plugin(&raw);
        }
    };

    view()
        .w_full()
        .gap_4()
        .child(
            text(move || store.t("dialog.install_plugin.desc"))
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg),
        )
        .child(
            styled_input(spec, move || store.t("plugins.install_placeholder"))
                .test_id("install-plugin-input")
                .w_full()
                .auto_focus(true)
                .auto_capitalize(AutoCapitalize::None)
                .auto_correct(false)
                .return_key_type(ReturnKeyType::Go)
                .on_submit(move |val: &str| {
                    if !val.trim().is_empty() {
                        spec.set(val.to_owned());
                    }
                    submit_spec();
                }),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    ghost_button(
                        move || store.t("plugins.from_folder"),
                        move || {
                            store.pick_and_stage_plugin_folder();
                        },
                    )
                    .test_id("install-plugin-pick-folder")
                    .when(is_phone, |b| b.min_h(px(44.0))),
                )
                .child(
                    ghost_button(
                        move || store.t("plugins.from_zip"),
                        move || {
                            store.pick_and_stage_plugin_zip();
                        },
                    )
                    .test_id("install-plugin-pick-zip")
                    .when(is_phone, |b| b.min_h(px(44.0))),
                ),
        )
        .child(when(
            move || store.plugin_busy.get(),
            move || {
                view()
                    .w_full()
                    .p_3()
                    .rounded(px(radius::CONTROL))
                    .bg(theme::primary_soft)
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(loading_spinner(15.0, colors::PRIMARY))
                    .child(
                        text(move || store.t("common.loading"))
                            .text_size(px(font_size::CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary),
                    )
            },
        ))
        .child(when(
            move || store.plugin_error.with(Option::is_some),
            move || {
                view()
                    .w_full()
                    .p_3()
                    .rounded(px(radius::CONTROL))
                    .bg(theme::danger_soft)
                    .child(
                        text(move || store.plugin_error.get().unwrap_or_default())
                            .text_size(px(font_size::CAPTION))
                            .text_color(theme::danger),
                    )
            },
        ))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    ghost_button(
                        move || store.t("common.cancel"),
                        move || {
                            store.close_modal();
                        },
                    )
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    busy_primary_button(
                        "install-plugin-submit",
                        store.plugin_busy,
                        move || store.plugin_busy.get() || spec.with(|s| s.trim().is_empty()),
                        move || {
                            if store.plugin_busy.get() {
                                store.t("common.loading")
                            } else {
                                store.t("plugins.install")
                            }
                        },
                        submit_spec,
                    )
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1()),
                ),
        )
}

/// Confirmation modal reviewing staged plugin metadata and declared network /
/// external program permissions before activating installation.
///
/// The metadata & permissions body scrolls inside a bounded container so Cancel
/// and Confirm buttons remain visible even when a plugin lists many hosts.
pub fn confirm_plugin_install_modal(store: AppStore, pending: PendingPluginInstall) -> View {
    let is_phone = theme::detect_device_form().is_phone();
    let external_cmds: Vec<String> = pending
        .process_commands
        .iter()
        .filter(|c| {
            !nsh_music_plugin::compat::is_ffmpeg_binary_name(c)
                && !nsh_music_plugin::compat::is_ytdlp_binary_name(c)
        })
        .cloned()
        .collect();
    let has_network = !pending.network_hosts.is_empty();
    let has_process = !pending.process_commands.is_empty();
    let has_external_process = !external_cmds.is_empty();
    let granted = signal(!has_external_process);
    let pending_cancel = pending.clone();
    let pending_confirm = pending.clone();

    let source_text = match &pending.source {
        InstallSource::GitHubRepo {
            owner,
            repo,
            git_ref,
        } => format!("{owner}/{repo}@{git_ref}"),
        InstallSource::LocalDir { path } => path.clone(),
        InstallSource::ZipArchive { path } => path.clone(),
        InstallSource::Discovered => pending.id.clone(),
    };

    let mut summary = view()
        .w_full()
        .p_3p5()
        .gap_1p5()
        .rounded(px(radius::CARD))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border)
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    text(pending.name.clone())
                        .text_size(px(font_size::HEADING))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .children((!pending.version.is_empty()).then(|| {
                    text(format!("v{}", pending.version))
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg)
                }))
                .children(pending.is_update.then(|| {
                    let badge =
                        store.t_fmt("plugins.update_available", &[("version", &pending.version)]);
                    view()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(theme::primary_soft)
                        .child(
                            text(badge)
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary),
                        )
                })),
        );

    if !pending.author.is_empty() {
        summary = summary.child(
            text(format!("{}: {}", store.t("plugins.author"), pending.author))
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg),
        );
    }

    if !pending.description.is_empty() {
        summary = summary.child(
            text(pending.description.clone())
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::fg),
        );
    }

    summary = summary.child(
        text(source_text)
            .number_of_lines(1)
            .text_size(px(font_size::MICRO))
            .text_color(theme::muted_fg),
    );

    let mut perms_box = view()
        .w_full()
        .p_3p5()
        .gap_3()
        .rounded(px(radius::CARD))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border)
        .child(
            text(move || store.t("plugins.confirm_install.permissions"))
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::fg),
        );

    if !has_network && !has_process {
        perms_box = perms_box.child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(icon(Icon::Check, 15.0, colors::OK))
                .child(
                    text(move || store.t("plugins.confirm_install.none"))
                        .text_size(px(font_size::SUBTITLE))
                        .text_color(theme::muted_fg),
                ),
        );
    } else {
        if has_network {
            perms_box = perms_box.child(permission_chip_group(
                store.t("plugins.confirm_install.network"),
                &pending.network_hosts,
                false,
            ));
        }

        if has_process {
            if has_external_process {
                let cmds_joined = external_cmds.join(", ");
                let warning =
                    store.t_fmt("plugins.programs_warning", &[("commands", &cmds_joined)]);
                perms_box = perms_box
                    .child(permission_chip_group(
                        store.t("plugins.confirm_install.process"),
                        &external_cmds,
                        true,
                    ))
                    .child(
                        view()
                            .w_full()
                            .p_3()
                            .gap_2p5()
                            .rounded(px(radius::CONTROL))
                            .bg(theme::warn_soft)
                            .border_1()
                            .border_color(theme::warn)
                            .child(
                                text(warning)
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::warn),
                            )
                            .child(styled_switch(
                                move || granted.get(),
                                move || store.t("plugins.allow_programs"),
                                move |next| granted.set(next),
                            )),
                    );
            } else {
                perms_box = perms_box.child(permission_chip_group(
                    store.t("plugins.confirm_install.process"),
                    &[store.t("plugins.compat_mobile_virtualized").to_string()],
                    false,
                ));
            }
        }
    }

    let scrollable_details = scroll()
        .w_full()
        .max_h(px(if is_phone { 300.0 } else { 260.0 }))
        .child(view().w_full().gap_3p5().child(summary).child(perms_box));

    view().w_full().gap_4().child(scrollable_details).child(
        view()
            .w_full()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_2()
            .child(
                ghost_button(
                    move || store.t("common.cancel"),
                    move || {
                        store.cancel_plugin_install(pending_cancel.clone());
                    },
                )
                .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                .when(is_phone, |b| b.flex_1()),
            )
            .child(
                busy_primary_button(
                    "confirm-plugin-install-btn",
                    store.plugin_busy,
                    move || store.plugin_busy.get(),
                    move || {
                        if store.plugin_busy.get() {
                            store.t("common.loading")
                        } else {
                            store.t("common.confirm")
                        }
                    },
                    move || {
                        store.confirm_plugin_install(
                            pending_confirm.clone(),
                            granted.get_untracked(),
                        );
                    },
                )
                .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                .when(is_phone, |b| b.flex_1()),
            ),
    )
}

fn permission_chip_group(title: SharedString, items: &[String], warn: bool) -> View {
    let mut chips = view().w_full().flex_row().flex_wrap().gap_1p5();
    for item in items {
        chips = chips.child(
            view()
                .px_2p5()
                .py_1()
                .rounded_full()
                .bg(if warn {
                    theme::warn_soft()
                } else {
                    theme::card()
                })
                .border_1()
                .border_color(if warn { theme::warn() } else { theme::border() })
                .child(
                    text(item.clone())
                        .number_of_lines(1)
                        .text_size(px(font_size::MICRO))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if warn { theme::warn() } else { theme::fg() }),
                ),
        );
    }

    view()
        .w_full()
        .gap_1p5()
        .child(
            text(title)
                .text_size(px(font_size::CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_fg),
        )
        .child(chips)
}

/// Plugin Configuration modal rendering declared `SettingDef` controls without
/// fabricating fake settings for the built-in local music source.
pub fn plugin_config_modal(store: AppStore, plugin_id: String) -> View {
    let form = theme::detect_device_form();
    if plugin_id.is_empty() || plugin_id == LocalMusicSource::SOURCE_ID {
        return view()
            .w_full()
            .gap_4()
            .child(empty_state(
                Icon::Library,
                store.t("source.local"),
                store.t("settings.local_folders"),
            ))
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .justify_end()
                    .child(primary_button(
                        move || store.t("common.close"),
                        move || {
                            store.close_modal();
                        },
                    )),
            );
    }

    let src = store.source_by_id(&plugin_id);
    let info = src.info();
    let defs: Vec<SettingDef> = info.settings;

    let mut container = view().w_full().gap_4().child(
        text(format!("{} ({})", info.name, plugin_id))
            .text_size(px(font_size::SUBTITLE))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::muted_fg),
    );

    if defs.is_empty() {
        container = container
            .child(empty_state(
                Icon::Settings,
                store.t("common.empty"),
                store.t("plugins.confirm_install.none"),
            ))
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .justify_end()
                    .child(primary_button(
                        move || store.t("common.close"),
                        move || {
                            store.close_modal();
                        },
                    )),
            );
        return container;
    }

    container.child(render_typed_settings_form(
        store, form, plugin_id, defs, true,
    ))
}

/// QR Sign-In modal rendering a crisp QR matrix on `canvas` via the `qrcode`
/// crate, or an animated `loading_spinner` while the QR payload is loading.
pub fn qr_sign_in_modal(store: AppStore, source_id: String, prompt: SignInPrompt) -> View {
    let qr_matrix: Option<Rc<(usize, Vec<bool>)>> = if prompt.qr.trim().is_empty() {
        None
    } else {
        QrCode::new(prompt.qr.as_bytes()).ok().map(|code| {
            let w = code.width();
            let bits = code
                .to_colors()
                .into_iter()
                .map(|c| matches!(c, QrColor::Dark))
                .collect();
            Rc::new((w, bits))
        })
    };

    let hint: SharedString = if !prompt.text.is_empty() {
        prompt.text.clone().into()
    } else if qr_matrix.is_none() {
        store.t("common.loading")
    } else {
        store.t("dialog.qr_sign_in.hint")
    };

    let qr_visual: View = if let Some(matrix) = qr_matrix {
        let size_pt = 216.0_f32;
        Elevated::elevation(
            view()
                .size(px(size_pt))
                .p_2()
                .rounded(px(radius::CARD))
                .bg(colors::NP_FG)
                .border_1()
                .border_color(theme::border),
            Elevation::Card,
        )
        .child(
            canvas(move || {
                let inner = size_pt - 16.0;
                let (width, ref bits) = *matrix;
                let paper = color(colors::NP_FG);
                let ink = color(colors::NP_BG);
                let mut p = Painter::new()
                    .fill_color(paper)
                    .fill_rect(0.0, 0.0, inner, inner)
                    .fill_color(ink);
                if width > 0 {
                    let quiet = 2.0_f32;
                    let cell = inner / (width as f32 + quiet * 2.0);
                    for row in 0..width {
                        for col in 0..width {
                            if bits[row * width + col] {
                                let x = (col as f32 + quiet) * cell;
                                let y = (row as f32 + quiet) * cell;
                                p = p.fill_rect(x, y, cell + 0.25, cell + 0.25);
                            }
                        }
                    }
                }
                p
            })
            .size_full(),
        )
    } else {
        view()
            .size(px(180.0))
            .rounded(px(radius::CARD))
            .bg(theme::secondary)
            .border_1()
            .border_color(theme::border)
            .items_center()
            .justify_center()
            .gap_3()
            .child(loading_spinner(36.0, colors::PRIMARY))
    };

    let sid_done = source_id.clone();
    let sid_retry = source_id;
    view()
        .w_full()
        .items_center()
        .gap_4()
        .child(qr_visual)
        .child(
            text(hint)
                .text_size(px(font_size::BODY))
                .text_color(theme::fg),
        )
        .children((!prompt.qr.is_empty()).then(|| {
            text(prompt.qr)
                .number_of_lines(2)
                .text_size(px(font_size::MICRO))
                .text_color(theme::muted_fg)
        }))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_center()
                .gap_2()
                .child(ghost_button(
                    move || store.t("common.retry"),
                    move || {
                        store.sign_in_source(&sid_retry);
                    },
                ))
                .child(primary_button(
                    move || store.t("common.done"),
                    move || {
                        store.refresh_sources();
                        if store.active_source_id.get_untracked() == sid_done {
                            store.hydrate_active_source_async();
                        }
                        store.close_modal();
                    },
                )),
        )
}

/// Embedded native `web_view()` Web Sign-In modal driven by `host.webSignIn(...)`:
/// runs in an isolated `.incognito(true)` session with custom `user_agent` when
/// supplied, reads `HttpOnly` + regular cookies via
/// `crate::state::web_cookies::fetch_session_cookies` (`nsh-web-signin`),
/// monitors `until` cookie names for automatic completion via
/// `store.submit_web_sign_in_result`, and provides a manual Cookie paste
/// fallback + safe Cancel (`store.cancel_web_sign_in`).
#[allow(clippy::too_many_arguments)]
pub fn web_sign_in_modal(
    store: AppStore,
    source_id: String,
    url: String,
    _title: String,
    cookies_of: Option<String>,
    until: Vec<String>,
    user_agent: Option<String>,
    allow_manual_cookie: bool,
) -> View {
    if source_id.is_empty() || source_id == LocalMusicSource::SOURCE_ID {
        return view()
            .w_full()
            .gap_4()
            .child(empty_state(
                Icon::Globe,
                store.t("dialog.web_sign_in.title"),
                store.t("common.empty"),
            ))
            .child(
                view()
                    .w_full()
                    .flex_row()
                    .justify_end()
                    .child(primary_button(
                        move || store.t("common.close"),
                        move || {
                            store.cancel_web_sign_in();
                            store.close_modal();
                        },
                    )),
            );
    }

    let trimmed_url = url.trim().to_owned();
    let has_valid_url = trimmed_url.starts_with("https://") || trimmed_url.starts_with("http://");

    let cookie_target = cookies_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&trimmed_url)
        .to_owned();

    let is_phone = theme::detect_device_form().is_phone();
    let resolved_ua = match user_agent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ua) if !(is_phone && looks_like_desktop_user_agent(ua)) => ua.to_owned(),
        _ if is_phone => WEB_SIGN_IN_MOBILE_USER_AGENT.to_owned(),
        _ => WEB_SIGN_IN_USER_AGENT.to_owned(),
    };

    let cookie_input = signal(String::new());
    let last_auto_cookie = signal(String::new());
    let web_unavailable = signal(!has_valid_url);
    let status_note = signal(SharedString::default());
    let url_rx = signal(trimmed_url.clone());
    let latest_url = signal(trimmed_url);
    let js_cookies = signal(HashMap::<String, String>::new());
    let native_cookie_map = signal(HashMap::<String, String>::new());
    let captured_cookies = signal(HashMap::<String, String>::new());
    let submitted = signal(false);

    let alive = Rc::new(Cell::new(true));
    {
        let alive_cleanup = Rc::clone(&alive);
        cleanup(move || alive_cleanup.set(false));
    }

    let fetching_native = Rc::new(Cell::new(false));
    let cookies_of_rc = Rc::new(cookies_of.clone());
    let until_rc = Rc::new(until);
    let until_for_chips = Rc::clone(&until_rc);

    type CookieInspectFn = Rc<dyn Fn(Option<HashMap<String, String>>, Option<(String, String)>)>;
    let inspect_and_maybe_submit: CookieInspectFn = {
        let cookies_of_check = Rc::clone(&cookies_of_rc);
        let until_check = Rc::clone(&until_rc);
        Rc::new(
            move |native_update: Option<HashMap<String, String>>,
                  js_update: Option<(String, String)>| {
                if submitted.get_untracked() {
                    return;
                }

                if let Some(native_map) = native_update {
                    native_cookie_map.set(native_map);
                }

                if let Some((href, raw_js_cookie)) = js_update {
                    if !href.trim().is_empty() {
                        latest_url.set(href.clone());
                    }
                    let trimmed = raw_js_cookie.trim();
                    if !trimmed.is_empty()
                        && web_cookies::url_matches_cookies_of(&href, cookies_of_check.as_deref())
                    {
                        let parsed = web_cookies::parse_cookie_header(trimmed);
                        if !parsed.is_empty() {
                            js_cookies.update(|cur| {
                                for (k, v) in parsed {
                                    cur.insert(k, v);
                                }
                            });
                        }
                    }
                }

                // Merge JS document.cookie with authoritative session cookie snapshots
                // (native store includes HttpOnly cookies and wins on collisions).
                let mut merged = js_cookies.get_untracked();
                for (k, v) in native_cookie_map.get_untracked() {
                    merged.insert(k, v);
                }

                if !merged.is_empty() {
                    captured_cookies.set(merged.clone());
                    let cookie_header = web_cookies::format_cookie_header(&merged);
                    if !cookie_header.is_empty() {
                        let cur_input = cookie_input.get_untracked();
                        let prev_auto = last_auto_cookie.get_untracked();
                        if cur_input.trim().is_empty() || cur_input == prev_auto {
                            last_auto_cookie.set(cookie_header.clone());
                            cookie_input.set(cookie_header.clone());
                        }
                    }

                    if !submitted.get_untracked()
                        && web_cookies::satisfies_until_cookies(&merged, &until_check)
                    {
                        submitted.set(true);
                        status_note.set(store.t("common.success"));
                        store.submit_web_sign_in_result(WebSignInResult {
                            ok: true,
                            url: latest_url.get_untracked(),
                            cookies: merged,
                            cookie: cookie_header,
                            eval_result: None,
                            cancelled: false,
                            manual_fallback: false,
                        });
                    }
                }
            },
        )
    };

    let trigger_native_fetch: Rc<dyn Fn()> = {
        let alive_trigger = Rc::clone(&alive);
        let fetching_trigger = Rc::clone(&fetching_native);
        let target_trigger = cookie_target.clone();
        let inspect_trigger = Rc::clone(&inspect_and_maybe_submit);
        Rc::new(move || {
            if !has_valid_url
                || !alive_trigger.get()
                || submitted.get_untracked()
                || web_unavailable.get_untracked()
                || fetching_trigger.get()
            {
                return;
            }
            fetching_trigger.set(true);
            let alive_bg = Rc::clone(&alive_trigger);
            let fetching_bg = Rc::clone(&fetching_trigger);
            let target_bg = target_trigger.clone();
            let inspect_bg = Rc::clone(&inspect_trigger);
            spawn(async move {
                let fetched = web_cookies::fetch_session_cookies(&target_bg, true).await;
                fetching_bg.set(false);
                if alive_bg.get()
                    && !submitted.get_untracked()
                    && let Some(map) = fetched
                {
                    inspect_bg(Some(map), None);
                }
            });
        })
    };

    // Lightweight 500ms cookie poller while the incognito Web Sign-In modal is open.
    if has_valid_url {
        let alive_poll = Rc::clone(&alive);
        let fetching_poll = Rc::clone(&fetching_native);
        let target_poll = cookie_target.clone();
        let inspect_poll = Rc::clone(&inspect_and_maybe_submit);
        spawn(async move {
            while alive_poll.get() && !submitted.get_untracked() {
                timer(Duration::from_millis(500)).await;
                if !alive_poll.get() || submitted.get_untracked() {
                    break;
                }
                if web_unavailable.get_untracked() || fetching_poll.get() {
                    continue;
                }
                fetching_poll.set(true);
                let fetched = web_cookies::fetch_session_cookies(&target_poll, true).await;
                fetching_poll.set(false);
                if !alive_poll.get() || submitted.get_untracked() {
                    break;
                }
                if let Some(map) = fetched {
                    inspect_poll(Some(map), None);
                }
            }
        });
    }

    let submit_manual = move || {
        let trimmed = cookie_input.with_untracked(|s| s.trim().to_owned());
        if trimmed.is_empty() || submitted.get_untracked() {
            return;
        }
        dismiss_keyboard();
        submitted.set(true);
        let cookies = web_cookies::parse_cookie_header(&trimmed);
        store.submit_web_sign_in_result(WebSignInResult {
            ok: true,
            url: latest_url.get_untracked(),
            cookies,
            cookie: trimmed,
            eval_result: None,
            cancelled: false,
            manual_fallback: true,
        });
    };

    let cookies_of_for_hint = Rc::clone(&cookies_of_rc);
    let build_hint_bar = move || {
        let cookies_of_row = Rc::clone(&cookies_of_for_hint);
        let until_row = Rc::clone(&until_for_chips);
        let has_chips = cookies_of_row.is_some() || !until_row.is_empty();
        view()
            .w_full()
            .gap_1p5()
            .child(
                text(move || store.t("plugins.web_sign_in_hint"))
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg),
            )
            .children(has_chips.then(move || {
                let mut row = view()
                    .w_full()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_1p5();
                if let Some(host) = cookies_of_row.as_deref().filter(|s| !s.trim().is_empty()) {
                    row = row.child(
                        view()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme::secondary)
                            .border_1()
                            .border_color(theme::border)
                            .child(
                                text(host.to_owned())
                                    .number_of_lines(1)
                                    .text_size(px(font_size::MICRO))
                                    .text_color(theme::muted_fg),
                            ),
                    );
                }
                for key in until_row.iter() {
                    let key_name = key.clone();
                    let key_check = key.clone();
                    let met = computed(move || {
                        captured_cookies
                            .with(|m| m.get(&key_check).is_some_and(|v| !v.trim().is_empty()))
                    });
                    row = row.child(
                        view()
                            .px_2()
                            .py_0p5()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .rounded_full()
                            .bg(move || {
                                if met.get() {
                                    theme::ok_soft()
                                } else {
                                    theme::secondary()
                                }
                            })
                            .border_1()
                            .border_color(move || {
                                if met.get() {
                                    theme::ok()
                                } else {
                                    theme::border()
                                }
                            })
                            .child(when(met, || icon(Icon::Check, 11.0, colors::OK)))
                            .child(
                                text(key_name)
                                    .number_of_lines(1)
                                    .text_size(px(font_size::MICRO))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(move || {
                                        if met.get() {
                                            theme::ok()
                                        } else {
                                            theme::muted_fg()
                                        }
                                    }),
                            ),
                    );
                }
                row
            }))
    };

    let inspect_for_msg = Rc::clone(&inspect_and_maybe_submit);
    let trigger_for_msg = Rc::clone(&trigger_native_fetch);
    let trigger_for_load = Rc::clone(&trigger_native_fetch);
    let trigger_for_nav = Rc::clone(&trigger_native_fetch);

    let web_box = view()
        .w_full()
        .h(move || {
            if is_phone {
                px(if keyboard_height().0 > 0.0 {
                    120.0
                } else {
                    290.0
                })
            } else {
                px(420.0)
            }
        })
        .rounded(px(radius::CARD))
        .overflow_hidden()
        .border_1()
        .border_color(theme::border)
        .bg(theme::secondary)
        .child(
            when(
                move || web_unavailable.get(),
                move || {
                    empty_state(
                        Icon::Globe,
                        store.t("dialog.web_sign_in.title"),
                        store.t("plugins.web_sign_in_manual_cookie"),
                    )
                },
            )
            .otherwise(move || {
                let ua = resolved_ua.clone();
                let inspect_msg = Rc::clone(&inspect_for_msg);
                let fetch_msg = Rc::clone(&trigger_for_msg);
                let fetch_load = Rc::clone(&trigger_for_load);
                let fetch_nav = Rc::clone(&trigger_for_nav);
                web_view(move || url_rx.get())
                    .incognito(true)
                    .user_agent(ua)
                    .java_script_enabled(true)
                    .injected_java_script(WEB_SIGN_IN_COOKIE_SCRIPT)
                    .on_message(move |payload: &str| {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) {
                            let href = val
                                .get("href")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned();
                            let cookie = val
                                .get("cookie")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned();
                            inspect_msg(None, Some((href, cookie)));
                        }
                        fetch_msg();
                    })
                    .on_load(move |nav: &WebNavigation| {
                        if !nav.url.trim().is_empty() {
                            latest_url.set(nav.url.clone());
                        }
                        status_note.set(SharedString::default());
                        fetch_load();
                    })
                    .on_navigation_state_change(move |nav: &WebNavigation| {
                        if !nav.url.trim().is_empty() {
                            latest_url.set(nav.url.clone());
                        }
                        fetch_nav();
                    })
                    .on_error(move |msg: &str| {
                        status_note.set(SharedString::from(msg.to_owned()));
                    })
                    .on_unavailable(move |_: &str| {
                        web_unavailable.set(true);
                    })
                    .size_full()
            }),
        );

    let show_manual_section = move || allow_manual_cookie || web_unavailable.get();

    view()
        .w_full()
        .gap(px(if is_phone { 10.0 } else { 14.0 }))
        .child(when(
            move || !is_phone || keyboard_height().0 <= 0.0,
            build_hint_bar,
        ))
        .child(web_box)
        .child(when(
            move || status_note.with(|s| !s.is_empty()),
            move || {
                text(move || status_note.get())
                    .number_of_lines(2)
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg)
            },
        ))
        .child(when(show_manual_section, move || {
            view()
                .w_full()
                .p(px(if is_phone { 10.0 } else { 12.0 }))
                .gap(px(if is_phone { 6.0 } else { 8.0 }))
                .rounded(px(radius::CONTROL))
                .bg(theme::secondary)
                .border_1()
                .border_color(theme::border)
                .child(
                    text(move || store.t("plugins.web_sign_in_manual_cookie"))
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::fg),
                )
                .child(
                    styled_input(cookie_input, move || {
                        store.t("dialog.web_sign_in.cookie_fallback")
                    })
                    .test_id("web-sign-in-cookie-input")
                    .w_full()
                    .masked(true)
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false)
                    .on_submit(move |val: &str| {
                        if !val.trim().is_empty() {
                            cookie_input.set(val.to_owned());
                        }
                        submit_manual();
                    }),
                )
        }))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    ghost_button(
                        move || store.t("common.cancel"),
                        move || {
                            store.cancel_web_sign_in();
                            store.close_modal();
                        },
                    )
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    primary_button(
                        move || store.t("plugins.web_sign_in_submit_cookie"),
                        submit_manual,
                    )
                    .test_id("web-sign-in-submit-btn")
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1())
                    .disabled(move || cookie_input.with(|s| s.trim().is_empty())),
                ),
        )
}
