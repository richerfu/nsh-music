//! Settings & Plugin Manager pages (`Route::Settings` / `Route::Plugins`) plus
//! modal views (`InstallPlugin`, `ConfirmPluginInstall`, `PluginConfig`,
//! `QrSignIn`, and `WebSignIn`).

use std::path::Path;
use std::rc::Rc;

use nsh_base::{SelectItem, button};
use nsh_core::prelude::*;
use nsh_music_core::i18n::Locale;
use nsh_music_core::local::LocalMusicSource;
use nsh_music_core::model::{Quality, SettingDef, SignInPrompt};
use nsh_music_core::settings::ThemeMode;
use nsh_music_core::source::PluginCompatLevel;
use nsh_music_plugin::{CompatTier, InstallSource, PendingPluginInstall, PluginSummary};
use qrcode::{Color as QrColor, QrCode};

use crate::pages::page_scroll;
use crate::state::{ActiveModal, AppStore};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, compat_badge_text, confirm_destructive, empty_state,
    empty_state_with_actions, format_bytes, ghost_button, icon, icon_button_sized, motion,
    pill_button, primary_button, styled_input, styled_select, styled_slider, styled_switch, theme,
};

const WEB_SIGN_IN_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";

const WEB_SIGN_IN_COOKIE_SCRIPT: &str = r#"
(function() {
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
})();
"#;

/// Renders either the Settings screen (`focus_plugins == false`, `Route::Settings`)
/// or the Plugins screen (`focus_plugins == true`, `Route::Plugins`).
pub fn settings_page(store: AppStore, form: DeviceForm, focus_plugins: bool) -> View {
    let content = if focus_plugins {
        plugins_page_content(store, form)
    } else {
        settings_page_content(store, form)
    };
    page_scroll(form, content)
}

// -----------------------------------------------------------------------------
// Route::Settings (`focus_plugins == false`)
// -----------------------------------------------------------------------------

fn settings_page_content(store: AppStore, form: DeviceForm) -> View {
    let filter_query = signal(String::new());

    let header = view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_wrap()
        .gap_3()
        .child(
            text(move || store.t("settings.title"))
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
            view()
                .w(px(if form.is_phone() { 260.0 } else { 280.0 }))
                .flex_row()
                .items_center()
                .gap_1p5()
                .child(
                    styled_input(filter_query, move || {
                        store.t("settings.search_placeholder")
                    })
                    .test_id("settings-search-input")
                    .flex_1()
                    .min_w(px(0.0))
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false),
                )
                .child(when(
                    move || filter_query.with(|q| !q.is_empty()),
                    move || {
                        icon_button_sized(
                            Icon::Close,
                            32.0,
                            14.0,
                            theme::muted_fg,
                            move || store.t("common.close"),
                            move || filter_query.set(String::new()),
                        )
                    },
                )),
        );

    let sections = view().w_full().child(dynamic(move || {
        let q = filter_query.get().trim().to_ascii_lowercase();
        let _loc = store.locale.get();
        untracked(move || {
            let mut stack = view().w_full().gap_5();
            let mut visible_count = 0usize;

            if let Some(card) = build_appearance_section(store, &q) {
                stack = stack.child(card);
                visible_count += 1;
            }
            if let Some(card) = build_playback_section(store, form, &q) {
                stack = stack.child(card);
                visible_count += 1;
            }
            if let Some(card) = build_cache_section(store, form, &q) {
                stack = stack.child(card);
                visible_count += 1;
            }
            if let Some(card) = build_experiments_section(store, &q) {
                stack = stack.child(card);
                visible_count += 1;
            }

            if visible_count == 0 {
                return empty_state(
                    Icon::Search,
                    store.t("search.no_results"),
                    store.t("settings.search_placeholder"),
                )
                .into_element();
            }

            stack.into_element()
        })
    }));

    view().w_full().gap_6().child(header).child(sections)
}

fn matches_filter(query: &str, haystacks: &[&str]) -> bool {
    if query.is_empty() {
        return true;
    }
    haystacks
        .iter()
        .any(|h| h.to_ascii_lowercase().contains(query))
}

fn build_appearance_section(store: AppStore, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.appearance");
    let theme_lbl = store.t("settings.theme");
    let locale_lbl = store.t("settings.locale");

    let section_hit = matches_filter(query, &[&sec_title, "appearance", "外观"]);
    let show_theme = section_hit
        || matches_filter(
            query,
            &[&theme_lbl, "theme", "light", "dark", "system", "主题", "深色", "浅色"],
        );
    let show_locale = section_hit
        || matches_filter(
            query,
            &[&locale_lbl, "language", "locale", "english", "中文", "语言"],
        );

    if !show_theme && !show_locale {
        return None;
    }

    let mut body = view().w_full().gap_4();

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

    Some(settings_card(
        move || store.t("settings.section.appearance"),
        Icon::Sparkles,
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

    let section_hit = matches_filter(query, &[&sec_title, "playback", "audio", "播放"]);
    let show_quality = section_hit
        || matches_filter(
            query,
            &[&quality_lbl, "quality", "lossless", "standard", "high", "音质", "无损"],
        );
    let show_rg = section_hit
        || matches_filter(query, &[&rg_lbl, &rg_desc, "replaygain", "loudness", "响度"]);
    let show_cf = section_hit
        || matches_filter(query, &[&cf_lbl, "crossfade", "fade", "淡入", "淡出"]);
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

    if !show_quality && !show_rg && !show_cf && !show_hot && !show_sparks {
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

    Some(settings_card(
        move || store.t("settings.section.playback"),
        Icon::Volume,
        body,
    ))
}

fn build_cache_section(store: AppStore, form: DeviceForm, query: &str) -> Option<View> {
    let sec_title = store.t("settings.section.cache");
    let limit_lbl = store.t("settings.cache_limit");
    let clear_lbl = store.t("settings.clear_cache");

    if !matches_filter(
        query,
        &[&sec_title, &limit_lbl, &clear_lbl, "cache", "storage", "clear", "缓存", "清除"],
    ) {
        return None;
    }

    let off_label = store.t("settings.crossfade.off");
    let cache_items = vec![
        SelectItem::new("0", off_label),
        SelectItem::new("1024", "1 GB"),
        SelectItem::new("2048", "2 GB"),
        SelectItem::new("5120", "5 GB"),
        SelectItem::new("10240", "10 GB"),
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
                    Some(SharedString::from(mb.to_string()))
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
        ghost_button(move || store.t("settings.clear_cache"), move || {
            store.clear_audio_cache();
        }),
    );

    Some(settings_card(
        move || store.t("settings.section.cache"),
        Icon::Disc,
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
            &[&smoke_lbl, &smoke_desc, "smoke", "progress", "scrubber", "烟雾", "进度"],
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

// -----------------------------------------------------------------------------
// Route::Plugins (`focus_plugins == true`)
// -----------------------------------------------------------------------------

fn plugins_page_content(store: AppStore, form: DeviceForm) -> View {
    let header = view()
        .w_full()
        .gap_3()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_3()
                .child(
                    text(move || store.t("plugins.title"))
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
                    ghost_button(move || store.t("common.refresh"), move || {
                        store.refresh_sources();
                    })
                    .min_h(px(form.min_touch_target())),
                ),
        )
        .child(import_toolbar(store, form));

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
                .child(icon(Icon::Refresh, 15.0, colors::PRIMARY))
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

    let installed_section = settings_card(
        move || store.t("settings.section.plugins"),
        Icon::Plugin,
        view().w_full().child(dynamic(move || {
            let plugins = store.plugins.get();
            let sources = store.sources.get();
            let _loc = store.locale.get();
            untracked(move || {
                if plugins.is_empty() {
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
                                move || store.t("plugins.from_folder"),
                                move || store.pick_and_stage_plugin_folder(),
                            )
                            .min_h(px(form.min_touch_target())),
                        );
                    return empty_state_with_actions(
                        Icon::Plugin,
                        store.t("plugins.empty_title"),
                        store.t("plugins.empty_subtitle"),
                        actions,
                    )
                    .into_element();
                }

                let mut list = view().w_full().gap_4();
                for plugin in plugins {
                    let source_entry = sources.iter().find(|s| s.info.id == plugin.id).cloned();
                    list = list.child(plugin_card(store, form, plugin, source_entry));
                }
                list.into_element()
            })
        })),
    );

    view()
        .w_full()
        .gap_5()
        .child(header)
        .child(busy_banner)
        .child(error_banner)
        .child(local_music_folders_card(store, form))
        .child(installed_section)
}

fn import_toolbar(store: AppStore, form: DeviceForm) -> View {
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
            Icon::Folder,
            move || store.t("plugins.from_folder"),
            form,
            move || store.pick_and_stage_plugin_folder(),
        )
        .test_id("plugins-btn-folder"),
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
            Icon::External,
            move || store.t("plugins.open_folder"),
            form,
            move || store.open_plugins_folder(),
        )
        .test_id("plugins-btn-open-dir"),
    )
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
    let folders_list = view().w_full().child(dynamic(move || {
        let folders = store.settings.with(|s| s.local_folders.clone());
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

    let actions = view()
        .w_full()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_2p5()
        .child(
            primary_button(move || store.t("library.add_folder"), move || {
                store.pick_local_music_folders();
            })
            .min_h(px(form.min_touch_target())),
        )
        .child(
            ghost_button(move || store.t("library.rescan"), move || {
                store.rescan_local_folders();
            })
            .min_h(px(form.min_touch_target())),
        );

    settings_card(
        move || store.t("settings.section.library"),
        Icon::Library,
        view().w_full().gap_3p5().child(folders_list).child(actions),
    )
}

fn plugin_card(
    store: AppStore,
    form: DeviceForm,
    plugin: PluginSummary,
    source_entry: Option<crate::state::SourceEntry>,
) -> View {
    let pid = plugin.id.clone();
    let pid_toggle = pid.clone();
    let pid_grant = pid.clone();
    let pid_update = pid.clone();
    let pid_reveal = pid.clone();
    let pid_reload = pid.clone();
    let pid_signin = pid.clone();
    let pid_signout = pid.clone();
    let pid_remove = pid.clone();

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

    let is_github_source = matches!(plugin.source, InstallSource::GitHubRepo { .. });
    let origin_label: Option<SharedString> = match &plugin.source {
        InstallSource::GitHubRepo { owner, repo, .. } => {
            Some(SharedString::from(format!("{owner}/{repo}")))
        }
        InstallSource::LocalDir { .. } => Some(store.t("plugins.source_local")),
        InstallSource::ZipArchive { .. } => Some(store.t("plugins.source_zip")),
        InstallSource::Discovered => None,
    };

    let external_cmds: Vec<String> = plugin
        .report
        .external_commands
        .iter()
        .filter(|c| !nsh_music_plugin::compat::is_ffmpeg_binary_name(c))
        .cloned()
        .collect();
    let has_external_programs = plugin.report.uses_external_spawn || !external_cmds.is_empty();
    let external_cmds_str = external_cmds.join(", ");
    let is_granted = plugin.granted;
    let plugin_name_for_remove = plugin.name.clone();
    let pid_switch = pid.clone();
    let is_active_source = store
        .active_source_id
        .with_untracked(|active| active == &pid_switch);

    let mut card = view()
        .w_full()
        .p_4()
        .gap_3p5()
        .rounded(px(radius::CARD))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border);

    // Header row: Name, version, author, origin, compat badge + Enable switch
    let mut meta_col = view()
        .flex_1()
        .min_w(px(180.0))
        .gap_1()
        .child(
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
                .child(compat_badge_text(badge_level, badge_label)),
        );

    let mut sub_meta_parts: Vec<String> = Vec::new();
    if !plugin.author.is_empty() {
        sub_meta_parts.push(format!("{}: {}", store.t("plugins.author"), plugin.author));
    }
    if let Some(orig) = origin_label {
        sub_meta_parts.push(orig.to_string());
    }
    if !sub_meta_parts.is_empty() {
        meta_col = meta_col.child(
            text(sub_meta_parts.join(" · "))
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg),
        );
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

    card = card.child(
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
            )),
    );

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

    // Action row: Switch to Source, Sign In/Out, Check for Updates (GitHub only), Show Folder, Reload, Remove
    let min_h = px(form.min_touch_target());
    let mut actions = view()
        .w_full()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_2();

    if plugin.loaded {
        if is_active_source {
            actions = actions.child(
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
                    ),
            );
        } else {
            actions = actions.child(
                ghost_button(move || store.t("dialog.source_switcher.title"), move || {
                    store.select_source(&pid_switch);
                })
                .min_h(min_h),
            );
        }
    }

    if is_signed_in {
        actions = actions.child(
            ghost_button(move || store.t("plugins.sign_out"), move || {
                store.sign_out_source(&pid_signout);
            })
            .min_h(min_h),
        );
    } else if supports_sign_in && plugin.loaded {
        actions = actions.child(
            primary_button(move || store.t("plugins.sign_in"), move || {
                if supports_qr {
                    let mgr = store.plugin_manager();
                    let source_id = pid_signin.clone();
                    spawn(async move {
                        let sid_bg = source_id.clone();
                        let qr_res = blocking(move || {
                            mgr.get(&sid_bg).map(|p| p.get_qr_sign_in())
                        })
                        .await;
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
            .min_h(min_h),
        );
    }

    if is_github_source {
        actions = actions.child(
            ghost_button(move || store.t("plugins.check_update"), move || {
                store.check_plugin_update(&pid_update);
            })
            .min_h(min_h),
        );
    }

    actions = actions
        .child(
            ghost_button(move || store.t("plugins.show_folder"), move || {
                store.reveal_plugin_folder(&pid_reveal);
            })
            .min_h(min_h),
        )
        .child(
            ghost_button(move || store.t("plugins.reload"), move || {
                store.reload_plugin(&pid_reload);
            })
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

fn compat_badge_meta(
    store: AppStore,
    plugin: &PluginSummary,
) -> (PluginCompatLevel, SharedString) {
    match plugin.report.tier {
        CompatTier::MigrationRequired => (
            PluginCompatLevel::DesktopProcessLimited,
            store.t("compat.migration_required"),
        ),
        CompatTier::Partial => (
            PluginCompatLevel::VirtualFfmpeg,
            store.t("compat.partial"),
        ),
        CompatTier::Full => match plugin.compat.level {
            PluginCompatLevel::VirtualFfmpeg => (
                PluginCompatLevel::VirtualFfmpeg,
                store.t("compat.virtual_ffmpeg"),
            ),
            PluginCompatLevel::DesktopProcessLimited => (
                PluginCompatLevel::DesktopProcessLimited,
                store.t("compat.desktop_limited"),
            ),
            PluginCompatLevel::FullNative => (
                PluginCompatLevel::FullNative,
                store.t("compat.full_native"),
            ),
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

        let is_dirty = computed(move || {
            fields_dirty
                .iter()
                .any(|f| f.draft.get() != f.saved.get())
        });

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
    Elevated::elevation(
        view()
            .w_full()
            .p_5()
            .gap_4()
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

// -----------------------------------------------------------------------------
// Modal contents rendered by `crate::pages::render_active_modal`
// -----------------------------------------------------------------------------

/// Install Plugin modal accepting GitHub `owner/repo`, `owner/repo@ref`, URL,
/// or local folder / `.zip` path and staging for permission confirmation.
pub fn install_plugin_modal(store: AppStore) -> View {
    let spec = signal(String::new());

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
                .on_submit(move |_: &str| submit_spec()),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    ghost_button(move || store.t("plugins.from_folder"), move || {
                        store.pick_and_stage_plugin_folder();
                    })
                    .test_id("install-plugin-pick-folder"),
                )
                .child(
                    ghost_button(move || store.t("plugins.from_zip"), move || {
                        store.pick_and_stage_plugin_zip();
                    })
                    .test_id("install-plugin-pick-zip"),
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
                    .child(icon(Icon::Refresh, 15.0, colors::PRIMARY))
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
                .child(ghost_button(move || store.t("common.cancel"), move || {
                    store.close_modal();
                }))
                .child(
                    primary_button(
                        move || {
                            if store.plugin_busy.get() {
                                store.t("common.loading")
                            } else {
                                store.t("plugins.install")
                            }
                        },
                        submit_spec,
                    )
                    .test_id("install-plugin-submit")
                    .disabled(move || {
                        store.plugin_busy.get() || spec.with(|s| s.trim().is_empty())
                    }),
                ),
        )
}

/// Confirmation modal reviewing staged plugin metadata and declared network /
/// external program permissions before activating installation.
pub fn confirm_plugin_install_modal(store: AppStore, pending: PendingPluginInstall) -> View {
    let external_cmds: Vec<String> = pending
        .process_commands
        .iter()
        .filter(|c| !nsh_music_plugin::compat::is_ffmpeg_binary_name(c))
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
                    let badge = store.t_fmt(
                        "plugins.update_available",
                        &[("version", &pending.version)],
                    );
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
                    &[store.t("compat.virtual_ffmpeg").to_string()],
                    false,
                ));
            }
        }
    }

    view()
        .w_full()
        .gap_4()
        .child(summary)
        .child(perms_box)
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(ghost_button(move || store.t("common.cancel"), move || {
                    store.cancel_plugin_install(pending_cancel.clone());
                }))
                .child(
                    primary_button(
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
                    .test_id("confirm-plugin-install-btn")
                    .disabled(move || store.plugin_busy.get()),
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
                .border_color(if warn {
                    theme::warn()
                } else {
                    theme::border()
                })
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
                    .child(primary_button(move || store.t("common.close"), move || {
                        store.close_modal();
                    })),
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
                    .child(primary_button(move || store.t("common.close"), move || {
                        store.close_modal();
                    })),
            );
        return container;
    }

    container.child(render_typed_settings_form(
        store, form, plugin_id, defs, true,
    ))
}

/// QR Sign-In modal rendering a crisp QR matrix on `canvas` via the `qrcode` crate.
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
            .child(icon(Icon::QrCode, 64.0, colors::MUTED_FOREGROUND))
    };

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
                .child(ghost_button(move || store.t("common.retry"), move || {
                    store.sign_in_source(&sid_retry);
                }))
                .child(primary_button(move || store.t("common.done"), move || {
                    store.refresh_sources();
                    store.close_modal();
                })),
        )
}

/// Embedded native `web_view()` Web Sign-In modal with `incognito(true)`,
/// custom User-Agent, JS cookie bridge, and Manual Cookie Paste fallback for
/// non-local plugins (zero placeholder URLs or fake cookie fallbacks on Local).
pub fn web_sign_in_modal(
    store: AppStore,
    source_id: String,
    url: String,
    _title: String,
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
                    .child(primary_button(move || store.t("common.close"), move || {
                        store.close_modal();
                    })),
            );
    }

    let trimmed_url = url.trim().to_owned();
    let has_valid_url =
        trimmed_url.starts_with("https://") || trimmed_url.starts_with("http://");

    let cookie_input = signal(store.plugin_setting(&source_id, "cookie"));
    let web_unavailable = signal(!has_valid_url);
    let status_note = signal(SharedString::default());
    let url_rx = signal(trimmed_url);

    let save_cookie = {
        let sid = source_id;
        move || {
            let trimmed = cookie_input.with_untracked(|s| s.trim().to_owned());
            if trimmed.is_empty() {
                return;
            }
            dismiss_keyboard();
            store.set_plugin_setting(&sid, "cookie", &trimmed);
            store.refresh_sources();
            store.close_modal();
        }
    };

    let web_box = view()
        .w_full()
        .h(px(300.0))
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
                        store.t("dialog.web_sign_in.cookie_fallback"),
                    )
                },
            )
            .otherwise(move || {
                web_view(move || url_rx.get())
                    .incognito(true)
                    .user_agent(WEB_SIGN_IN_USER_AGENT)
                    .java_script_enabled(true)
                    .injected_java_script(WEB_SIGN_IN_COOKIE_SCRIPT)
                    .on_message(move |payload: &str| {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload)
                            && let Some(cookie) = val.get("cookie").and_then(|v| v.as_str())
                            && !cookie.trim().is_empty()
                        {
                            cookie_input.set(cookie.to_owned());
                            status_note.set(store.t("common.success"));
                        }
                    })
                    .on_load(move |_: &WebNavigation| {
                        status_note.set(SharedString::default());
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

    view()
        .w_full()
        .gap_3p5()
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
        .child(
            view()
                .w_full()
                .p_3()
                .gap_2()
                .rounded(px(radius::CONTROL))
                .bg(theme::secondary)
                .border_1()
                .border_color(theme::border)
                .child(
                    text(move || store.t("dialog.web_sign_in.cookie_fallback"))
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::fg),
                )
                .child(
                    styled_input(cookie_input, move || {
                        store.t("dialog.web_sign_in.cookie_fallback")
                    })
                    .w_full()
                    .masked(true)
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false)
                    .on_submit({
                        let save = save_cookie.clone();
                        move |_: &str| save()
                    }),
                ),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(ghost_button(move || store.t("common.cancel"), move || {
                    store.close_modal();
                }))
                .child(
                    primary_button(move || store.t("common.save"), save_cookie)
                        .disabled(move || cookie_input.with(|s| s.trim().is_empty())),
                ),
        )
}
