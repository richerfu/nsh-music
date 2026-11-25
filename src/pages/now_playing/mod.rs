//! Full-screen / full-window immersive Now Playing overlay adapting cleanly across
//! Desktop, Pad (Landscape & Portrait), and Phone form factors.

pub mod backdrop;
pub mod lyrics_view;
pub mod panels;
pub mod retro;
pub mod stage;
pub mod wall;

use nsh_base::{SliderValue, button, slider};
use nsh_core::prelude::*;
use nsh_music_core::model::{RepeatMode, Track};

use crate::{
    state::{AppStore, NowPlayingMode, NowPlayingPanel, NowPlayingStageStyle},
    ui::{
        controls::play_pause_button,
        icons::{Icon, icon_reactive},
        motion,
        scrubber::scrubber,
        theme::{self, DeviceForm, colors, font_size, radius},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhoneSection {
    Art,
    Lyrics,
    More,
}

/// Renders the full-window / full-screen Now Playing overlay stage.
///
/// Adapts across:
/// - **Desktop**: Full-window immersive overlay with top toolbar (mode switcher,
///   stage switcher, capability-gated panel tabs, fullscreen & close) and 2-column split.
/// - **Pad**: Landscape 2-column split; Portrait stacked centerpiece + switchable panels.
/// - **Phone**: Full-screen sheet with chevron-down dismiss handle, segmented switcher
///   between Art / Lyrics / Queue & Comments, and `>= 44pt` thumb-zone transport controls.
pub fn now_playing_overlay(store: AppStore, form: DeviceForm) -> View {
    // Auto-fallback to Lyrics whenever Comments or Podcast becomes unavailable
    effect(move || {
        let cur = store.now_playing_panel.get();
        let allow_comments = store.active_can("comments");
        let allow_podcast = store
            .current_track
            .with(|t| t.as_ref().is_some_and(Track::is_episode));
        if (cur == NowPlayingPanel::Comments && !allow_comments)
            || (cur == NowPlayingPanel::Podcast && !allow_podcast)
        {
            store.set_now_playing_panel(NowPlayingPanel::Lyrics);
        }
    });

    view()
        .test_id("now-playing-overlay")
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .overflow_hidden()
        .bg(theme::np_bg)
        .text_color(theme::np_fg)
        // Layer 1: Dominant-accent GPU WGSL fog / Canvas fallback ambient backdrop
        .child(backdrop::ambient_backdrop(store))
        // Layer 2: Cover Wall atmospheric mosaic when `stage_style == Wall`
        .child(when(
            move || store.stage_style.get() == NowPlayingStageStyle::Wall,
            move || wall::cover_wall_mosaic(store, form),
        ))
        // Layer 3: Form-factor-adapted interactive stage foreground
        .child(match form {
            DeviceForm::Desktop => desktop_stage(store, form),
            DeviceForm::Pad { landscape: true } => pad_landscape_stage(store, form),
            DeviceForm::Pad { landscape: false } => pad_portrait_stage(store, form),
            DeviceForm::Phone => phone_sheet_stage(store, form),
        })
}

// -----------------------------------------------------------------------------
// Desktop Layout: Full-window toolbar + 2-column split (or fullscreen stage)
// -----------------------------------------------------------------------------

fn desktop_stage(store: AppStore, form: DeviceForm) -> View {
    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(14.0))
        .pb_6()
        .px_8()
        .gap_5()
        // Top Toolbar
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .child(mode_switcher(store, false))
                .child(stage::stage_style_switcher(store, false))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(panels::panel_tabs(store, form))
                        .child(stage_icon_button(
                            move || {
                                if store.stage_fullscreen.get() {
                                    Icon::Shrink
                                } else {
                                    Icon::Expand
                                }
                            },
                            34.0,
                            16.0,
                            move || store.t("now_playing.stage"),
                            move || store.toggle_stage_fullscreen(),
                        ))
                        .child(stage_icon_button(
                            Icon::Close,
                            34.0,
                            16.0,
                            move || store.t("now_playing.minimize"),
                            move || store.close_now_playing(),
                        )),
                ),
        )
        // Main body: fullscreen lyrics stage OR 2-column split
        .child(
            when(store.stage_fullscreen, move || {
                view()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.0))
                    .gap_4()
                    .child(panels::active_panel_content(store, form))
                    .child(
                        view()
                            .w_full()
                            .px_5()
                            .py_3()
                            .rounded(px(radius::XL))
                            .bg(theme::np_surface)
                            .border_1()
                            .border_color(theme::np_border)
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_6()
                            .child(track_meta_row(store, form, true))
                            .child(transport_controls(store, form, 44.0))
                            .child(volume_bar(store, form, 140.0)),
                    )
            })
            .otherwise(move || {
                view()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.0))
                    .flex_row()
                    .items_stretch()
                    .gap_8()
                    // Left column: centerpiece + track info + scrubber + transport + volume
                    .child(
                        view()
                            .w(relative(0.44))
                            .min_w(px(340.0))
                            .max_w(px(470.0))
                            .justify_center()
                            .items_center()
                            .gap_5()
                            .child(retro::retro_centerpiece(store, 286.0))
                            .child(
                                view()
                                    .w_full()
                                    .max_w(px(400.0))
                                    .gap_4()
                                    .child(track_meta_row(store, form, false))
                                    .child(stage_scrubber(store))
                                    .child(transport_controls(store, form, 52.0))
                                    .child(volume_bar(store, form, 240.0)),
                            ),
                    )
                    // Right column: switchable Lyrics / Queue / Comments / Podcast panel
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .p_4()
                            .rounded(px(radius::XL))
                            .bg(move || theme::np_surface().opacity(0.55))
                            .border_1()
                            .border_color(theme::np_border)
                            .child(panels::active_panel_content(store, form)),
                    )
            }),
        )
}

// -----------------------------------------------------------------------------
// Pad Landscape Layout: 2-column split with >= 44pt touch controls
// -----------------------------------------------------------------------------

fn pad_landscape_stage(store: AppStore, form: DeviceForm) -> View {
    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(12.0))
        .pb_safe_area_plus(px(16.0))
        .px_5()
        .gap_4()
        // Top bar
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(stage_icon_button(
                    Icon::ChevronDown,
                    44.0,
                    20.0,
                    move || store.t("now_playing.minimize"),
                    move || store.close_now_playing(),
                ))
                .child(mode_switcher(store, true))
                .child(stage::stage_style_switcher(store, true))
                .child(panels::panel_tabs(store, form)),
        )
        // 2-column landscape split
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .flex_row()
                .items_stretch()
                .gap_5()
                .child(
                    view()
                        .w(relative(0.45))
                        .min_w(px(300.0))
                        .justify_center()
                        .items_center()
                        .gap_4()
                        .child(retro::retro_centerpiece(store, 232.0))
                        .child(
                            view()
                                .w_full()
                                .max_w(px(360.0))
                                .gap_3()
                                .child(track_meta_row(store, form, false))
                                .child(stage_scrubber(store))
                                .child(transport_controls(store, form, 54.0))
                                .child(volume_bar(store, form, 200.0)),
                        ),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .p_3()
                        .rounded(px(radius::XL))
                        .bg(move || theme::np_surface().opacity(0.55))
                        .border_1()
                        .border_color(theme::np_border)
                        .child(panels::active_panel_content(store, form)),
                ),
        )
}

// -----------------------------------------------------------------------------
// Pad Portrait Layout: Stacked centerpiece + switchable panels
// -----------------------------------------------------------------------------

fn pad_portrait_stage(store: AppStore, form: DeviceForm) -> View {
    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(12.0))
        .pb_safe_area_plus(px(16.0))
        .px_5()
        .gap_4()
        // Top header bar
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(stage_icon_button(
                    Icon::ChevronDown,
                    44.0,
                    20.0,
                    move || store.t("now_playing.minimize"),
                    move || store.close_now_playing(),
                ))
                .child(mode_switcher(store, true))
                .child(stage_like_button(store, 44.0)),
        )
        // Upper centerpiece + playback deck
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_5()
                .child(retro::retro_centerpiece(store, 196.0))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_3()
                        .child(track_meta_row(store, form, false))
                        .child(stage_scrubber(store))
                        .child(transport_controls(store, form, 52.0)),
                ),
        )
        // Switchable panel & stage bar
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(panels::panel_tabs(store, form))
                .child(when(
                    move || panels::effective_panel(store) == NowPlayingPanel::Lyrics,
                    move || stage::stage_style_switcher(store, true),
                )),
        )
        // Lower switchable panel container
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .p_3()
                .rounded(px(radius::XL))
                .bg(move || theme::np_surface().opacity(0.55))
                .border_1()
                .border_color(theme::np_border)
                .child(panels::active_panel_content(store, form)),
        )
}

// -----------------------------------------------------------------------------
// Phone Layout: Full-screen sheet with dismiss handle, segmented switcher & >=44pt thumb zone
// -----------------------------------------------------------------------------

fn phone_sheet_stage(store: AppStore, form: DeviceForm) -> View {
    let section = signal(PhoneSection::Art);

    // Clicking the Quiet-Part Top Comment card from Phone Lyrics jumps cleanly into Comments
    effect(move || {
        if store.now_playing_panel.get() == NowPlayingPanel::Comments
            && section.get_untracked() == PhoneSection::Lyrics
        {
            section.set(PhoneSection::More);
        }
    });

    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(8.0))
        .pb_safe_area_plus(px(14.0))
        .px_4()
        .gap_3()
        // Subtle top drag pill indicator
        .child(
            view()
                .w_full()
                .items_center()
                .child(
                    view()
                        .w(px(38.0))
                        .h(px(4.5))
                        .rounded_full()
                        .bg(theme::np_faint),
                ),
        )
        // Header row: chevron-down dismiss handle + Art / Lyrics / Queue & Comments switcher
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(stage_icon_button(
                    Icon::ChevronDown,
                    44.0,
                    22.0,
                    move || store.t("now_playing.minimize"),
                    move || store.close_now_playing(),
                ))
                .child(phone_section_switcher(store, section))
                .child(stage_like_button(store, 44.0)),
        )
        // Primary adaptive stage viewport
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(dynamic(move || {
                    let cur = section.get();
                    untracked(|| match cur {
                        PhoneSection::Art => view()
                            .key("np-phone-art")
                            .flex_1()
                            .w_full()
                            .items_center()
                            .justify_center()
                            .gap_4()
                            .child(retro::retro_centerpiece(store, 248.0))
                            .child(mode_switcher(store, true)),

                        PhoneSection::Lyrics => view()
                            .key("np-phone-lyrics")
                            .flex_1()
                            .w_full()
                            .min_h(px(0.0))
                            .gap_2()
                            .child(
                                scroll()
                                    .horizontal()
                                    .scrollbar(false)
                                    .child(stage::stage_style_switcher(store, true)),
                            )
                            .child(lyrics_view::lyrics_view(store, form)),

                        PhoneSection::More => view()
                            .key("np-phone-more")
                            .flex_1()
                            .w_full()
                            .min_h(px(0.0))
                            .gap_2()
                            .child(phone_more_subtabs(store))
                            .child(dynamic(move || {
                                let panel = panels::effective_panel(store);
                                untracked(|| match panel {
                                    NowPlayingPanel::Comments => {
                                        panels::comments_panel(store, form).key("np-phone-comments")
                                    }
                                    NowPlayingPanel::Podcast => {
                                        panels::podcast_panel(store, form).key("np-phone-podcast")
                                    }
                                    NowPlayingPanel::Queue | NowPlayingPanel::Lyrics => {
                                        panels::queue_panel(store, form).key("np-phone-queue")
                                    }
                                })
                            })),
                    })
                })),
        )
        // Thumb-zone controls pinned cleanly at bottom (>= 44pt targets)
        .child(
            view()
                .w_full()
                .gap_3()
                .pt_2()
                .child(track_meta_row(store, form, true))
                .child(stage_scrubber(store))
                .child(transport_controls(store, form, 58.0)),
        )
}

fn phone_section_switcher(store: AppStore, section: Signal<PhoneSection>) -> View {
    let items: &[(PhoneSection, &str)] = &[
        (PhoneSection::Art, "now_playing.mode.cover"),
        (PhoneSection::Lyrics, "now_playing.panel.lyrics"),
        (PhoneSection::More, "now_playing.panel.queue"),
    ];

    view()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .rounded_full()
        .bg(theme::np_surface)
        .border_1()
        .border_color(theme::np_border)
        .children(items.iter().copied().map(move |(sec, key)| {
            let active = move || section.get() == sec;
            button()
                .selected(active)
                .accessibility_label(move || Some(store.t(key)))
                .on_press(move || {
                    section.set(sec);
                    match sec {
                        PhoneSection::Lyrics => store.set_now_playing_panel(NowPlayingPanel::Lyrics),
                        PhoneSection::More => {
                            if store.now_playing_panel.get_untracked() == NowPlayingPanel::Lyrics {
                                store.set_now_playing_panel(NowPlayingPanel::Queue);
                            }
                        }
                        PhoneSection::Art => {}
                    }
                })
                .min_h(px(44.0))
                .px_3()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(move || {
                    if active() {
                        theme::np_surface_hover()
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    text(move || store.t(key))
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::MEDIUM)
                        .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                        .text_color(move || {
                            if active() {
                                theme::np_fg()
                            } else {
                                theme::np_muted()
                            }
                        }),
                )
        }))
}

fn phone_more_subtabs(store: AppStore) -> View {
    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .child(phone_subtab_pill(
            store,
            NowPlayingPanel::Queue,
            "now_playing.panel.queue",
        ))
        .child(when(
            move || store.active_can("comments"),
            move || phone_subtab_pill(store, NowPlayingPanel::Comments, "now_playing.panel.comments"),
        ))
        .child(when(
            move || {
                store
                    .current_track
                    .with(|t| t.as_ref().is_some_and(Track::is_episode))
            },
            move || phone_subtab_pill(store, NowPlayingPanel::Podcast, "now_playing.panel.podcast"),
        ))
}

fn phone_subtab_pill(
    store: AppStore,
    panel: NowPlayingPanel,
    key: &'static str,
) -> impl IntoElement {
    let active = move || {
        let cur = panels::effective_panel(store);
        if panel == NowPlayingPanel::Queue {
            matches!(cur, NowPlayingPanel::Queue | NowPlayingPanel::Lyrics)
        } else {
            cur == panel
        }
    };
    button()
        .selected(active)
        .accessibility_label(move || Some(store.t(key)))
        .on_press(move || store.set_now_playing_panel(panel))
        .min_h(px(44.0))
        .px_3p5()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(move || {
            if active() {
                theme::primary()
            } else {
                theme::np_border()
            }
        })
        .bg(move || {
            if active() {
                theme::primary_soft()
            } else {
                theme::np_surface()
            }
        })
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            text(move || store.t(key))
                .number_of_lines(1)
                .text_size(px(font_size::CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || {
                    if active() {
                        theme::primary()
                    } else {
                        theme::np_fg()
                    }
                }),
        )
}

// -----------------------------------------------------------------------------
// Shared Stage Controls (Mode Switcher, Track Info, Scrubber, Transport, Volume)
// -----------------------------------------------------------------------------

fn mode_switcher(store: AppStore, compact: bool) -> View {
    let modes: &[(NowPlayingMode, Icon, &str)] = &[
        (NowPlayingMode::Cover, Icon::Disc, "now_playing.mode.cover"),
        (NowPlayingMode::Vinyl, Icon::Vinyl, "now_playing.mode.vinyl"),
        (
            NowPlayingMode::Cassette,
            Icon::Cassette,
            "now_playing.mode.cassette",
        ),
    ];

    view()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .rounded_full()
        .bg(theme::np_surface)
        .border_1()
        .border_color(theme::np_border)
        .children(modes.iter().copied().map(move |(mode, glyph, i18n_key)| {
            let active = move || store.now_playing_mode.get() == mode;
            let min_h = move || {
                if compact || theme::detect_device_form().is_touch() {
                    px(44.0)
                } else {
                    px(30.0)
                }
            };
            button()
                .selected(active)
                .accessibility_label(move || Some(store.t(i18n_key)))
                .on_press(move || store.set_now_playing_mode(mode))
                .min_h(min_h)
                .px_3()
                .flex_row()
                .items_center()
                .gap_1p5()
                .rounded_full()
                .bg(move || {
                    if active() {
                        theme::np_surface_hover()
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .border_1()
                .border_color(move || {
                    if active() {
                        theme::np_border()
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .transition_all()
                .duration(move || motion::transition_ms(140.0))
                .hover(|s| s.bg(theme::np_surface_hover))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(icon_reactive(glyph, 14.0, move || {
                    if active() {
                        theme::np_fg()
                    } else {
                        theme::np_muted()
                    }
                }))
                .child(
                    text(move || store.t(i18n_key))
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::MEDIUM)
                        .style_when(active, |s| s.font_weight(FontWeight::SEMIBOLD))
                        .text_color(move || {
                            if active() {
                                theme::np_fg()
                            } else {
                                theme::np_muted()
                            }
                        }),
                )
        }))
}

fn track_meta_row(store: AppStore, form: DeviceForm, compact: bool) -> View {
    let like_size = if form.is_touch() { 44.0 } else { 40.0 };
    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_0p5()
                .child(
                    text(move || {
                        store.current_track.with(|t| {
                            t.as_ref()
                                .map(|tr| SharedString::from(tr.name.clone()))
                                .unwrap_or_else(|| store.t("player.idle_title"))
                        })
                    })
                    .number_of_lines(1)
                    .text_size(px(if compact {
                        font_size::TITLE
                    } else {
                        font_size::TITLE_LG
                    }))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::np_fg),
                )
                .child(
                    text(move || {
                        let fallback_artist = store.t("track.unknown_artist");
                        store.current_track.with(|t| {
                            t.as_ref()
                                .map(|tr| {
                                    let raw_artist = tr.artist_names();
                                    let artist = if raw_artist.is_empty() {
                                        fallback_artist.to_string()
                                    } else {
                                        raw_artist
                                    };
                                    if !tr.album.name.is_empty() && !compact {
                                        SharedString::from(format!("{artist} · {}", tr.album.name))
                                    } else {
                                        SharedString::from(artist)
                                    }
                                })
                                .unwrap_or_default()
                        })
                    })
                    .number_of_lines(1)
                    .text_size(px(font_size::BODY))
                    .text_color(theme::np_muted),
                ),
        )
        .children((!compact).then(|| stage_like_button(store, like_size)))
}

fn stage_scrubber(store: AppStore) -> impl IntoElement {
    let buffered_ratio = move || {
        let dur = store.duration_ms.get();
        if dur == 0 {
            0.0_f32
        } else {
            (store.buffered_ms.get() as f32 / dur as f32).clamp(0.0, 1.0)
        }
    };

    scrubber(store.position_ms, store.duration_ms)
        .buffered_ratio(buffered_ratio)
        .smoke_mode(move || store.settings.with(|s| s.smoke_progress))
        .playing(move || store.is_playing.get())
        .accent(move || store.current_accent.get())
        .spectrum(store.spectrum)
        .show_labels(true)
        .on_dark_stage(true)
        .on_seek(move |ms| store.seek(ms))
}

fn transport_controls(store: AppStore, form: DeviceForm, play_diameter: f32) -> View {
    let touch = form.is_touch();
    let aux_btn = if touch { 44.0 } else { 36.0 };
    let skip_btn = if touch { 48.0 } else { 40.0 };

    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        // Shuffle
        .child(stage_toggle_icon(
            Icon::Shuffle,
            aux_btn,
            18.0,
            move || store.shuffle.get(),
            move || {
                if store.shuffle.get() {
                    store.t("player.shuffle_on")
                } else {
                    store.t("player.shuffle_off")
                }
            },
            move || store.toggle_shuffle(),
        ))
        // Previous Track
        .child(stage_icon_button(
            Icon::Prev,
            skip_btn,
            22.0,
            move || store.t("player.prev"),
            move || store.prev_track(),
        ))
        // Play / Pause Primary Morph Button
        .child(play_pause_button(
            store.is_playing,
            play_diameter,
            move || {
                if store.is_playing.get() {
                    store.t("player.pause")
                } else {
                    store.t("player.play")
                }
            },
            move || store.toggle_play(),
        ))
        // Next Track
        .child(stage_icon_button(
            Icon::Next,
            skip_btn,
            22.0,
            move || store.t("player.next"),
            move || store.next_track(),
        ))
        // Repeat Cycle
        .child(stage_toggle_icon(
            move || match store.repeat.get() {
                RepeatMode::One => Icon::RepeatOne,
                RepeatMode::Off | RepeatMode::All => Icon::Repeat,
            },
            aux_btn,
            18.0,
            move || store.repeat.get() != RepeatMode::Off,
            move || match store.repeat.get() {
                RepeatMode::Off => store.t("player.repeat_off"),
                RepeatMode::All => store.t("player.repeat_all"),
                RepeatMode::One => store.t("player.repeat_one"),
            },
            move || store.cycle_repeat(),
        ))
}

fn volume_bar(store: AppStore, form: DeviceForm, slider_max_w: f32) -> View {
    let eff_volume = move || {
        if store.muted.get() {
            0.0_f64
        } else {
            store.volume.get() as f64
        }
    };
    let mute_size = if form.is_touch() { 44.0 } else { 34.0 };

    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2p5()
        .child(stage_icon_button(
            move || {
                if store.muted.get() || store.volume.get() <= 0.001 {
                    Icon::VolumeMute
                } else {
                    Icon::Volume
                }
            },
            mute_size,
            17.0,
            move || {
                if store.muted.get() {
                    store.t("player.unmute")
                } else {
                    store.t("player.mute")
                }
            },
            move || store.toggle_mute(),
        ))
        .child(
            view().flex_1().max_w(px(slider_max_w)).child(
                slider()
                    .value(move || Some(SliderValue::Single(eff_volume())))
                    .min(0.0)
                    .max(1.0)
                    .step(0.02)
                    .accessibility_label(move || Some(store.t("player.volume")))
                    .on_change(move |v: &SliderValue| {
                        let next = v.start() as f32;
                        if store.muted.get_untracked() && next > 0.0 {
                            store.toggle_mute();
                        }
                        store.set_volume(next);
                    })
                    .track_style(|s| {
                        s.w_full()
                            .h(move || {
                                if theme::detect_device_form().is_touch() {
                                    px(44.0)
                                } else {
                                    px(22.0)
                                }
                            })
                            .justify_center()
                    })
                    .indicator_style(|s| s.w_full().h(px(4.0)).rounded_full().bg(theme::np_surface))
                    .range_style(|s| s.rounded_full().bg(theme::np_fg))
                    .thumb_class("hover:scale-115 active:scale-125 focus-visible:outline-2 focus-visible:outline-ring")
                    .thumb_style(|s| {
                        s.size(px(12.0))
                            .rounded_full()
                            .bg(colors::NP_FG)
                            .shadow_sm()
                    }),
            ),
        )
        .child(
            text(move || format!("{}%", (eff_volume() * 100.0).round() as u32))
                .tabular_nums()
                .w(px(34.0))
                .text_right()
                .text_size(px(font_size::MICRO))
                .text_color(theme::np_muted),
        )
}

fn stage_like_button(store: AppStore, box_size: f32) -> impl IntoElement {
    let is_liked = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(|tr| store.is_liked(&tr.id)))
    });
    let pop_scale = animated_value(1.0);
    let prev_liked = stored(untracked(|| is_liked.get()));

    effect(move || {
        let now = is_liked.get();
        if prev_liked.replace(now) != now && now {
            motion::pulse_pop(&pop_scale);
        }
    });

    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));

    button()
        .selected(is_liked)
        .accessibility_label(move || {
            Some(if is_liked.get() {
                store.t("player.unlike")
            } else {
                store.t("player.like")
            })
        })
        .on_press(move || store.toggle_like_current())
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.bg(theme::np_surface))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .pointer_events(PointerEvents::None)
                .bind_scale(&pop_scale)
                .child(icon_reactive(
                    move || {
                        if is_liked.get() {
                            Icon::HeartFilled
                        } else {
                            Icon::Heart
                        }
                    },
                    20.0,
                    move || {
                        if is_liked.get() {
                            theme::primary()
                        } else {
                            theme::np_muted()
                        }
                    },
                )),
        )
}

fn stage_icon_button(
    glyph: impl IntoReactive<Icon>,
    box_size: f32,
    glyph_size: f32,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let label_rx = label.into_reactive();
    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));
    button()
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.bg(theme::np_surface).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, glyph_size, theme::np_fg))
}

fn stage_toggle_icon(
    glyph: impl IntoReactive<Icon>,
    box_size: f32,
    glyph_size: f32,
    active: impl IntoReactive<bool>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active_rx = active.into_reactive();
    let label_rx = label.into_reactive();
    let active_tint = active_rx.clone();
    let active_dot = active_rx.clone();
    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));

    button()
        .selected(move || active_rx.get())
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::np_surface_hover))
        .active(|s| s.bg(theme::np_surface).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .items_center()
                .gap_0p5()
                .pointer_events(PointerEvents::None)
                .child(icon_reactive(glyph, glyph_size, move || {
                    if active_tint.get() {
                        theme::primary()
                    } else {
                        theme::np_muted()
                    }
                }))
                .child(when(move || active_dot.get(), || {
                    view().size(px(4.0)).rounded_full().bg(theme::primary)
                })),
        )
}
