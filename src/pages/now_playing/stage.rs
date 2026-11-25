//! Folia-inspired stage style switcher (`NowPlayingStageStyle::{Classic, Flow, Steps, Tilt, Wall}`)
//! varying typography scale, horizontal cadence/offset, tilt, and focus hierarchy smoothly.

use nsh_base::button;
use nsh_core::prelude::*;

use crate::{
    state::{AppStore, NowPlayingStageStyle},
    ui::{
        motion,
        theme::{self, colors, font_size},
    },
};

/// Horizontal alignment mode for a lyric row under a stage style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAlign {
    Start,
    Center,
}

/// Computed per-line visual presentation driven by [`NowPlayingStageStyle`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageLineSpec {
    pub align: StageAlign,
    pub font_size: f32,
    pub translation_size: f32,
    pub font_weight: FontWeight,
    pub opacity: f32,
    pub scale: f32,
    pub offset_x: f32,
    pub tilt_deg: f32,
    pub show_focus_bar: bool,
}

/// Computes smooth typography scale, horizontal cadence/offset, tilt, and focus
/// hierarchy for a lyric line in the selected [`NowPlayingStageStyle`].
pub fn line_spec(
    style: NowPlayingStageStyle,
    line_index: usize,
    active_index: Option<usize>,
    synced: bool,
    compact: bool,
    reduced_motion: bool,
) -> StageLineSpec {
    if !synced {
        return StageLineSpec {
            align: if matches!(style, NowPlayingStageStyle::Classic | NowPlayingStageStyle::Wall) {
                StageAlign::Center
            } else {
                StageAlign::Start
            },
            font_size: if compact { 16.0 } else { 18.0 },
            translation_size: if compact { 12.5 } else { 13.5 },
            font_weight: FontWeight::MEDIUM,
            opacity: 0.88,
            scale: 1.0,
            offset_x: 0.0,
            tilt_deg: 0.0,
            show_focus_bar: false,
        };
    }

    let is_active = active_index == Some(line_index);
    let delta: isize = active_index
        .map(|a| line_index as isize - a as isize)
        .unwrap_or(line_index as isize + 1);
    let dist = delta.unsigned_abs();

    match style {
        NowPlayingStageStyle::Classic => {
            let opacity = if is_active {
                1.0
            } else {
                match dist {
                    1 => 0.56,
                    2 => 0.40,
                    _ => 0.28,
                }
            };
            StageLineSpec {
                align: StageAlign::Center,
                font_size: if is_active {
                    if compact { 19.0 } else { 23.0 }
                } else if compact {
                    14.5
                } else {
                    16.5
                },
                translation_size: if is_active { 14.0 } else { 12.5 },
                font_weight: if is_active {
                    FontWeight::BOLD
                } else {
                    FontWeight::MEDIUM
                },
                opacity,
                scale: if is_active && !reduced_motion { 1.03 } else { 1.0 },
                offset_x: 0.0,
                tilt_deg: 0.0,
                show_focus_bar: false,
            }
        }

        NowPlayingStageStyle::Flow => {
            let opacity = if is_active {
                1.0
            } else if delta < 0 {
                match dist {
                    1 => 0.32,
                    _ => 0.22,
                }
            } else {
                match dist {
                    1 => 0.62,
                    2 => 0.44,
                    _ => 0.30,
                }
            };
            StageLineSpec {
                align: StageAlign::Start,
                font_size: if is_active {
                    if compact { 23.0 } else { 28.0 }
                } else if compact {
                    16.0
                } else {
                    19.5
                },
                translation_size: if is_active { 15.0 } else { 13.0 },
                font_weight: if is_active {
                    FontWeight::BOLD
                } else {
                    FontWeight::SEMIBOLD
                },
                opacity,
                scale: if is_active && !reduced_motion { 1.03 } else { 0.98 },
                offset_x: if is_active && !reduced_motion { 4.0 } else { 0.0 },
                tilt_deg: 0.0,
                show_focus_bar: is_active,
            }
        }

        NowPlayingStageStyle::Steps => {
            // Architectural stepped horizontal cadence around the active spotlight axis
            let step_unit = if compact { 10.0 } else { 20.0 };
            let cadence_slot = match delta.clamp(-3, 3) {
                0 => 0.0,
                1 => 1.0,
                2 => 2.0,
                3 => 2.6,
                -1 => 0.8,
                -2 => 1.6,
                _ => 2.2,
            };
            let opacity = if is_active {
                1.0
            } else {
                match dist {
                    1 => 0.58,
                    2 => 0.40,
                    _ => 0.26,
                }
            };
            StageLineSpec {
                align: StageAlign::Start,
                font_size: if is_active {
                    if compact { 22.0 } else { 27.0 }
                } else if compact {
                    15.5
                } else {
                    18.5
                },
                translation_size: if is_active { 14.5 } else { 12.5 },
                font_weight: if is_active {
                    FontWeight::BOLD
                } else {
                    FontWeight::SEMIBOLD
                },
                opacity,
                scale: if is_active && !reduced_motion { 1.04 } else { 0.97 },
                offset_x: cadence_slot * step_unit,
                tilt_deg: 0.0,
                show_focus_bar: is_active,
            }
        }

        NowPlayingStageStyle::Tilt => {
            // Expressive Folia poster cadence with gentle alternating slant
            let sway = if compact { 6.0 } else { 14.0 };
            let offset_x = if is_active {
                0.0
            } else if line_index.is_multiple_of(2) {
                sway
            } else {
                -sway * 0.45
            };
            let tilt_deg = if reduced_motion {
                0.0
            } else if is_active {
                -1.4
            } else if delta > 0 {
                if line_index.is_multiple_of(2) { 1.8 } else { -1.6 }
            } else {
                -2.2
            };
            let opacity = if is_active {
                1.0
            } else {
                match dist {
                    1 => 0.52,
                    2 => 0.36,
                    _ => 0.24,
                }
            };
            StageLineSpec {
                align: StageAlign::Start,
                font_size: if is_active {
                    if compact { 23.0 } else { 29.0 }
                } else if compact {
                    15.5
                } else {
                    18.5
                },
                translation_size: if is_active { 14.5 } else { 12.5 },
                font_weight: if is_active {
                    FontWeight::EXTRA_BOLD
                } else {
                    FontWeight::MEDIUM
                },
                opacity,
                scale: if is_active && !reduced_motion { 1.04 } else { 0.96 },
                offset_x,
                tilt_deg,
                show_focus_bar: is_active,
            }
        }

        NowPlayingStageStyle::Wall => {
            // Luminous centered caption hierarchy designed to float over the Cover Wall mosaic
            let opacity = if is_active {
                1.0
            } else {
                match dist {
                    1 => 0.54,
                    2 => 0.34,
                    _ => 0.20,
                }
            };
            StageLineSpec {
                align: StageAlign::Center,
                font_size: if is_active {
                    if compact { 21.0 } else { 26.0 }
                } else if compact {
                    14.5
                } else {
                    17.0
                },
                translation_size: if is_active { 14.5 } else { 12.5 },
                font_weight: if is_active {
                    FontWeight::BOLD
                } else {
                    FontWeight::MEDIUM
                },
                opacity,
                scale: if is_active && !reduced_motion { 1.04 } else { 0.98 },
                offset_x: 0.0,
                tilt_deg: 0.0,
                show_focus_bar: false,
            }
        }
    }
}

/// All selectable Folia stage styles with their i18n keys.
pub const STAGE_STYLES: &[(NowPlayingStageStyle, &str)] = &[
    (NowPlayingStageStyle::Classic, "now_playing.stage.classic"),
    (NowPlayingStageStyle::Flow, "now_playing.stage.flow"),
    (NowPlayingStageStyle::Steps, "now_playing.stage.steps"),
    (NowPlayingStageStyle::Tilt, "now_playing.stage.tilt"),
    (NowPlayingStageStyle::Wall, "now_playing.stage.wall"),
];

/// Segmented capsule switcher for `NowPlayingStageStyle::{Classic, Flow, Steps, Tilt, Wall}`.
pub fn stage_style_switcher(store: AppStore, compact: bool) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .rounded_full()
        .bg(theme::np_surface)
        .border_1()
        .border_color(theme::np_border)
        .children(STAGE_STYLES.iter().copied().map(move |(style, i18n_key)| {
            let is_active = move || store.stage_style.get() == style;
            let min_h = move || {
                if compact || theme::detect_device_form().is_touch() {
                    px(44.0)
                } else {
                    px(28.0)
                }
            };

            button()
                .selected(is_active)
                .accessibility_label(move || Some(store.t(i18n_key)))
                .on_press(move || store.set_stage_style(style))
                .min_h(min_h)
                .px(if compact { px(10.0) } else { px(12.0) })
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(move || {
                    if is_active() {
                        theme::np_surface_hover()
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .border_1()
                .border_color(move || {
                    if is_active() {
                        theme::np_border()
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .transition_all()
                .duration(move || motion::transition_ms(320.0))
                .hover(|s| s.bg(theme::np_surface_hover))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    text(move || store.t(i18n_key))
                        .number_of_lines(1)
                        .text_size(px(if compact {
                            font_size::CAPTION
                        } else {
                            font_size::MICRO
                        }))
                        .font_weight(FontWeight::MEDIUM)
                        .style_when(is_active, |s| s.font_weight(FontWeight::SEMIBOLD))
                        .text_color(move || {
                            if is_active() {
                                theme::np_fg()
                            } else {
                                theme::np_muted()
                            }
                        }),
                )
        }))
}
