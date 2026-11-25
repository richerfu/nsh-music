//! Styled reusable UI controls wrapping `nsh_base` primitives with
//! `nsh-music` semantic tokens, adaptive touch targets (`>= 44pt` on
//! Pad/Phone), and reduced-motion-safe feedback.

use std::{
    f32::consts::{PI, TAU},
    rc::Rc,
};

use nsh_base::{
    ButtonElement, Input, SelectElement, SelectEntry, SelectItem, SelectValue, SliderElement,
    SliderValue, SwitchElement, button, input, select, slider, switch,
};
use nsh_core::{
    animation::{Curve, EaseMode},
    canvas::{CanvasGradient, LineCap},
    geometry::{percentage, point},
    prelude::*,
};
use nsh_music_core::source::PluginCompatLevel;

use super::{
    icons::{Icon, icon, icon_reactive},
    motion,
    theme::{self, DeviceForm, Elevated, Elevation, colors, font_size, radius},
};

/// Icon-only button with adaptive sizing (`32pt` Desktop, `>= 44pt` touch on
/// Pad/Phone), hover wash, reduced-motion press scale, and accessibility label.
pub fn icon_button(
    kind: impl IntoReactive<Icon>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    icon_button_tinted(kind, theme::fg, label, on_press)
}

/// Icon-only button with an explicit reactive [`AdaptiveColor`] tint.
pub fn icon_button_tinted(
    kind: impl IntoReactive<Icon>,
    tint: impl IntoReactive<AdaptiveColor>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let label_rx = label.into_reactive();
    let touch_target = move || {
        let form = theme::detect_device_form();
        px(form.min_touch_target())
    };
    button()
        .on_press(on_press)
        .accessibility_label(move || Some(label_rx.get()))
        .min_w(touch_target)
        .min_h(touch_target)
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.42))
        .child(icon_reactive(kind, 18.0, tint))
}

/// Icon-only button with explicit visual box size and icon glyph size.
/// Automatically expands `hit_slop` on touch devices so the tap target stays
/// `>= 44pt`.
pub fn icon_button_sized(
    kind: impl IntoReactive<Icon>,
    box_size: f32,
    glyph_size: f32,
    tint: impl IntoReactive<AdaptiveColor>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let label_rx = label.into_reactive();
    button()
        .on_press(on_press)
        .accessibility_label(move || Some(label_rx.get()))
        .hit_slop(move || {
            let min = theme::detect_device_form().min_touch_target();
            let extra = ((min - box_size) * 0.5).max(0.0);
            EdgeInsets::all(px(extra))
        })
        .size(px(box_size))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.42))
        .child(icon_reactive(kind, glyph_size, tint))
}

/// Play / Pause circular primary button with spring settle glyph transition per
/// `DESIGN.md` Motion Inventory.
pub fn play_pause_button(
    playing: impl IntoReactive<bool>,
    diameter: f32,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let playing_rx = playing.into_reactive();
    let label_rx = label.into_reactive();
    let initial = untracked(|| playing_rx.get());
    let morph = animated_value(if initial { 1.0 } else { 0.0 });
    let prev = stored(initial);

    {
        let playing_rx = playing_rx.clone();
        effect(move || {
            let now = playing_rx.get();
            if prev.replace(now) != now {
                motion::animate_spring(
                    &morph,
                    if now { 1.0 } else { 0.0 },
                    motion::spring_settle(),
                );
            }
        });
    }

    let glyph = (diameter * 0.44).clamp(14.0, 28.0);
    let hit = move || {
        let min = theme::detect_device_form().min_touch_target();
        let extra = ((min - diameter) * 0.5).max(0.0);
        EdgeInsets::all(px(extra))
    };

    button()
        .on_press(on_press)
        .accessibility_label(move || Some(label_rx.get()))
        .hit_slop(hit)
        .size(px(diameter))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme::primary)
        .elevation(Elevation::Subtle)
        .transition_all()
        .duration(move || motion::transition_ms(160.0))
        .hover(|s| s.bg(theme::primary_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .size(px(glyph))
                .items_center()
                .justify_center()
                .pointer_events(PointerEvents::None)
                .child(
                    view()
                        .absolute()
                        .inset_0()
                        .items_center()
                        .justify_center()
                        .bind_opacity(&morph.interpolate(&[0.0, 1.0], &[1.0, 0.0]))
                        .bind_scale(&morph.interpolate(&[0.0, 1.0], &[1.0, 0.72]))
                        .child(icon(Icon::Play, glyph, colors::PRIMARY_FOREGROUND)),
                )
                .child(
                    view()
                        .absolute()
                        .inset_0()
                        .items_center()
                        .justify_center()
                        .bind_opacity(&morph.interpolate(&[0.0, 1.0], &[0.0, 1.0]))
                        .bind_scale(&morph.interpolate(&[0.0, 1.0], &[0.72, 1.0]))
                        .child(icon(Icon::Pause, glyph, colors::PRIMARY_FOREGROUND)),
                ),
        )
}

/// Play / Pause circular primary button that displays [`loading_spinner`] while
/// `buffering` is true and smoothly spring-morphs between Play and Pause otherwise.
pub fn play_pause_button_buffering(
    playing: impl IntoReactive<bool>,
    buffering: impl IntoReactive<bool>,
    diameter: f32,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let playing_rx = playing.into_reactive();
    let buffering_rx = buffering.into_reactive();
    let label_rx = label.into_reactive();
    let initial = untracked(|| playing_rx.get());
    let morph = animated_value(if initial { 1.0 } else { 0.0 });
    let prev = stored(initial);

    {
        let playing_rx = playing_rx.clone();
        effect(move || {
            let now = playing_rx.get();
            if prev.replace(now) != now {
                motion::animate_spring(
                    &morph,
                    if now { 1.0 } else { 0.0 },
                    motion::spring_settle(),
                );
            }
        });
    }

    let glyph = (diameter * 0.44).clamp(14.0, 28.0);
    let hit = move || {
        let min = theme::detect_device_form().min_touch_target();
        let extra = ((min - diameter) * 0.5).max(0.0);
        EdgeInsets::all(px(extra))
    };

    button()
        .on_press(on_press)
        .accessibility_label(move || Some(label_rx.get()))
        .hit_slop(hit)
        .size(px(diameter))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme::primary)
        .elevation(Elevation::Subtle)
        .transition_all()
        .duration(move || motion::transition_ms(160.0))
        .hover(|s| s.bg(theme::primary_hover))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            when(
                move || buffering_rx.get(),
                move || loading_spinner(glyph, colors::PRIMARY_FOREGROUND),
            )
            .otherwise(move || {
                view()
                    .size(px(glyph))
                    .items_center()
                    .justify_center()
                    .pointer_events(PointerEvents::None)
                    .child(
                        view()
                            .absolute()
                            .inset_0()
                            .items_center()
                            .justify_center()
                            .bind_opacity(&morph.interpolate(&[0.0, 1.0], &[1.0, 0.0]))
                            .bind_scale(&morph.interpolate(&[0.0, 1.0], &[1.0, 0.72]))
                            .child(icon(Icon::Play, glyph, colors::PRIMARY_FOREGROUND)),
                    )
                    .child(
                        view()
                            .absolute()
                            .inset_0()
                            .items_center()
                            .justify_center()
                            .bind_opacity(&morph.interpolate(&[0.0, 1.0], &[0.0, 1.0]))
                            .bind_scale(&morph.interpolate(&[0.0, 1.0], &[0.72, 1.0]))
                            .child(icon(Icon::Pause, glyph, colors::PRIMARY_FOREGROUND)),
                    )
            }),
        )
}

/// Animated Like Heart toggle button (`SPRING_POP` bounce when liked).
pub fn heart_button(
    liked: impl IntoReactive<bool>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let liked_rx = liked.into_reactive();
    let label_rx = label.into_reactive();
    let scale = animated_value(1.0);
    let was_liked = stored(untracked(|| liked_rx.get()));

    {
        let liked_rx = liked_rx.clone();
        effect(move || {
            let now = liked_rx.get();
            if was_liked.replace(now) != now && now {
                motion::pulse_pop(&scale);
            }
        });
    }

    let liked_for_icon = liked_rx.clone();
    let liked_for_tint = liked_rx.clone();
    let liked_for_sel = liked_rx;

    button()
        .selected(move || liked_for_sel.get())
        .on_press(on_press)
        .accessibility_label(move || Some(label_rx.get()))
        .min_w(move || px(theme::detect_device_form().min_touch_target()))
        .min_h(move || px(theme::detect_device_form().min_touch_target()))
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .pointer_events(PointerEvents::None)
                .bind_scale(&scale)
                .child(icon_reactive(
                    move || {
                        if liked_for_icon.get() {
                            Icon::HeartFilled
                        } else {
                            Icon::Heart
                        }
                    },
                    18.0,
                    move || {
                        if liked_for_tint.get() {
                            theme::primary()
                        } else {
                            theme::muted_fg()
                        }
                    },
                )),
        )
}

/// Capsule pill filter / navigation button with active highlight state.
/// On touch devices, wraps a sleek `32pt` visible capsule inside a `>= 44pt`
/// transparent touch target.
pub fn pill_button(
    label: impl IntoReactive<SharedString>,
    active: impl IntoReactive<bool>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let active_rx = active.into_reactive();
    let active_bg = active_rx.clone();
    let active_fg = active_rx.clone();
    let active_border = active_rx.clone();

    button()
        .group()
        .selected(move || active_rx.get())
        .on_press(on_press)
        .min_h(move || {
            if theme::detect_device_form().is_touch() {
                px(44.0)
            } else {
                px(30.0)
            }
        })
        .min_w(move || {
            if theme::detect_device_form().is_touch() {
                px(44.0)
            } else {
                px(0.0)
            }
        })
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_transform()
        .duration(move || motion::transition_ms(140.0))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(
            view()
                .h(move || {
                    if theme::detect_device_form().is_touch() {
                        px(32.0)
                    } else {
                        px(30.0)
                    }
                })
                .px(move || {
                    if theme::detect_device_form().is_touch() {
                        px(13.0)
                    } else {
                        px(12.0)
                    }
                })
                .flex_row()
                .items_center()
                .justify_center()
                .gap_1p5()
                .rounded_full()
                .border_1()
                .border_color(move || {
                    if active_border.get() {
                        theme::primary().opacity(0.38)
                    } else {
                        theme::border()
                    }
                })
                .bg(move || {
                    if active_bg.get() {
                        theme::primary_soft()
                    } else {
                        theme::secondary()
                    }
                })
                .transition_colors()
                .duration(move || motion::transition_ms(150.0))
                .group_hover(|s| s.bg(theme::accent))
                .child(
                    text(label)
                        .number_of_lines(1)
                        .text_size(px(font_size::SUBTITLE))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || {
                            if active_fg.get() {
                                theme::primary()
                            } else {
                                theme::fg()
                            }
                        }),
                ),
        )
}

/// Primary Coral Rose action button.
pub fn primary_button(
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let min_h = move || match theme::detect_device_form() {
        DeviceForm::Desktop => px(34.0),
        DeviceForm::Pad { .. } | DeviceForm::Phone => px(44.0),
    };
    button()
        .on_press(on_press)
        .min_w(px(88.0))
        .min_h(min_h)
        .px_4()
        .py_2()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(theme::primary)
        .elevation(Elevation::Subtle)
        .transition_all()
        .duration(move || motion::transition_ms(160.0))
        .hover(|s| s.bg(theme::primary_hover))
        .active(|s| s.opacity(0.92).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::primary_fg),
        )
}

/// Quiet ghost button for secondary actions.
pub fn ghost_button(
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let min_h = move || match theme::detect_device_form() {
        DeviceForm::Desktop => px(34.0),
        DeviceForm::Pad { .. } | DeviceForm::Phone => px(44.0),
    };
    button()
        .on_press(on_press)
        .min_w(px(88.0))
        .min_h(min_h)
        .px_3()
        .py_2()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::fg),
        )
}

/// Controlled text input styled with semantic tokens and resolved `Hsla`
/// placeholder/caret/selection colors.
pub fn styled_input(value: Signal<String>, placeholder: impl IntoReactive<SharedString>) -> Input {
    let ph_rx = placeholder.into_reactive();
    let field_h = move || match theme::detect_device_form() {
        DeviceForm::Desktop => px(36.0),
        DeviceForm::Pad { .. } | DeviceForm::Phone => px(44.0),
    };
    input()
        .value(move || value.get())
        .on_change(move |next: &str| value.set(next.to_owned()))
        .placeholder(move || ph_rx.get().to_string())
        .placeholder_color(|| color(theme::muted_fg()))
        .caret_color(|| color(theme::primary()))
        .selection_color(|| color(theme::primary_soft()))
        .h(field_h)
        .min_h(field_h)
        .px_3()
        .text_size(px(font_size::BODY))
        .text_color(theme::fg)
        .rounded(px(radius::CONTROL))
        .bg(theme::card)
        .border_1()
        .border_color(theme::input_border)
        .focus(|s| s.border_color(theme::ring))
}

/// Controlled toggle switch wrapping `nsh_base::switch` with fully styled
/// `track_style` and `thumb_style`.
pub fn styled_switch(
    checked: impl IntoReactive<bool>,
    label: impl IntoReactive<SharedString>,
    on_change: impl Fn(bool) + 'static,
) -> SwitchElement {
    let checked_rx = checked.into_reactive();
    let label_rx = label.into_reactive();
    let label_a11y = label_rx.clone();
    switch()
        .checked(move || checked_rx.get())
        .accessibility_label(move || Some(label_a11y.get()))
        .on_change(move |next: &bool| on_change(*next))
        .hit_slop(move || {
            if theme::detect_device_form().is_touch() {
                EdgeInsets::all(px(8.0))
            } else {
                EdgeInsets::all(px(4.0))
            }
        })
        .flex_row()
        .items_center()
        .gap_3()
        .disabled_style(|s| s.opacity(0.45))
        .track_style(|s| {
            s.w(px(44.0))
                .h(px(26.0))
                .p(px(3.0))
                .rounded_full()
                .bg(theme::muted)
                .border_1()
                .border_color(theme::border)
                .transition_colors()
                .duration(move || motion::transition_ms(160.0))
                .checked(|s| s.bg(theme::primary).border_color(theme::primary))
        })
        .thumb_style(|s| {
            s.size(px(18.0))
                .rounded_full()
                .bg(colors::SWITCH_THUMB)
                .shadow_sm()
                .transition_transform()
                .duration(move || motion::transition_ms(180.0))
                .checked(|s| s.translate_x(px(18.0)))
        })
        .child(
            text(move || label_rx.get())
                .text_size(px(font_size::BODY))
                .text_color(theme::fg),
        )
}

/// Controlled horizontal slider wrapping `nsh_base::slider` with styled
/// track, indicator rail, active range fill, and thumb.
pub fn styled_slider(
    value: impl IntoReactive<f64>,
    min: f64,
    max: f64,
    step: f64,
    label: impl IntoReactive<SharedString>,
    on_change: impl Fn(f64) + 'static,
) -> SliderElement {
    let value_rx = value.into_reactive();
    let label_rx = label.into_reactive();
    slider()
        .value(move || Some(SliderValue::Single(value_rx.get())))
        .min(min)
        .max(max)
        .step(step)
        .accessibility_label(move || Some(label_rx.get()))
        .on_change(move |v: &SliderValue| on_change(v.start()))
        .track_style(|s| {
            s.h(move || {
                if theme::detect_device_form().is_touch() {
                    px(36.0)
                } else {
                    px(22.0)
                }
            })
            .w_full()
            .justify_center()
        })
        .indicator_style(|s| s.h(px(4.0)).w_full().rounded_full().bg(theme::track_rail))
        .range_style(|s| s.rounded_full().bg(theme::primary))
        .thumb_class(
            "hover:scale-115 active:scale-125 focus-visible:outline-2 focus-visible:outline-ring",
        )
        .thumb_style(|s| {
            s.size(px(14.0))
                .rounded_full()
                .bg(colors::SWITCH_THUMB)
                .border_2()
                .border_color(theme::primary)
                .shadow_sm()
                .transition_transform()
                .duration(move || motion::transition_ms(140.0))
        })
}

/// Controlled dropdown / bottom-sheet select wrapping `nsh_base::select` with
/// complete trigger, popup, backdrop, and row styling.
pub fn styled_select(
    items: Vec<SelectItem>,
    value: impl IntoReactive<Option<SharedString>>,
    placeholder: impl IntoReactive<SharedString>,
    label: impl IntoReactive<SharedString>,
    on_change: impl Fn(Option<SharedString>) + 'static,
) -> SelectElement {
    let value_rx = value.into_reactive();
    let ph_rx = placeholder.into_reactive();
    let label_rx = label.into_reactive();
    let lookup_items = Rc::new(items.clone());

    let value_for_select = value_rx.clone();
    let value_for_trigger = value_rx;

    let min_h = move || match theme::detect_device_form() {
        DeviceForm::Desktop => px(36.0),
        DeviceForm::Pad { .. } | DeviceForm::Phone => px(44.0),
    };

    select()
        .items(items)
        .value(move || SelectValue::from(value_for_select.get()))
        .on_change(move |next: &Option<SharedString>| on_change(next.clone()))
        .accessibility_label(move || Some(label_rx.get()))
        .offset(px(6.0))
        .margin(px(8.0))
        .backdrop_style(|s| s.bg(theme::scrim))
        .popup_style(|s| {
            s.p_1p5()
                .rounded(px(radius::LG))
                .border_1()
                .border_color(theme::border)
                .bg(theme::popover)
                .elevation(Elevation::Popover)
        })
        .item_class("rounded-md focus:bg-accent selected:font-semibold disabled:opacity-45")
        .render_item(|item: &SelectEntry| {
            view()
                .px_3()
                .py_2()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    text(item.title.clone())
                        .number_of_lines(1)
                        .text_size(px(font_size::BODY))
                        .text_color(theme::popover_fg),
                )
        })
        .trigger(
            view()
                .min_h(min_h)
                .px_3()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .rounded(px(radius::CONTROL))
                .bg(theme::card)
                .border_1()
                .border_color(theme::input_border)
                .hover(|s| s.bg(theme::accent))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    text(move || {
                        let chosen = value_for_trigger.get();
                        chosen
                            .as_ref()
                            .and_then(|v| {
                                lookup_items
                                    .iter()
                                    .find(|it| &it.value == v)
                                    .map(|it| it.title.clone())
                            })
                            .or(chosen)
                            .unwrap_or_else(|| ph_rx.get())
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .number_of_lines(1)
                    .text_size(px(font_size::BODY))
                    .text_color(theme::fg),
                )
                .child(icon(Icon::ChevronDown, 15.0, colors::MUTED_FOREGROUND)),
        )
}

/// Source switcher capsule pill used in headers, sidebars, and search bars.
/// On touch devices, preserves a `>= 44pt` transparent outer hit-box wrapping a
/// sleek `32pt` visible capsule with status dot, label, and chevron.
pub fn source_pill(
    name: impl IntoReactive<SharedString>,
    active: impl IntoReactive<bool>,
    on_press: impl Fn() + 'static,
) -> ButtonElement {
    let active_rx = active.into_reactive();
    let active_dot = active_rx.clone();
    let active_bg = active_rx.clone();
    let active_border = active_rx.clone();
    let active_fg = active_rx.clone();
    let active_chev = active_rx.clone();

    button()
        .group()
        .selected(move || active_rx.get())
        .on_press(on_press)
        .min_h(move || {
            if theme::detect_device_form().is_touch() {
                px(44.0)
            } else {
                px(30.0)
            }
        })
        .min_w(move || {
            if theme::detect_device_form().is_touch() {
                px(44.0)
            } else {
                px(0.0)
            }
        })
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_transform()
        .duration(move || motion::transition_ms(140.0))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .h(move || {
                    if theme::detect_device_form().is_touch() {
                        px(32.0)
                    } else {
                        px(30.0)
                    }
                })
                .px_2p5()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_1p5()
                .rounded_full()
                .border_1()
                .border_color(move || {
                    if active_border.get() {
                        theme::primary().opacity(0.32)
                    } else {
                        theme::border()
                    }
                })
                .bg(move || {
                    if active_bg.get() {
                        theme::primary_soft()
                    } else {
                        theme::card()
                    }
                })
                .transition_colors()
                .duration(move || motion::transition_ms(140.0))
                .group_hover(|s| s.bg(theme::accent))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .min_w(px(0.0))
                        .flex_shrink(1.0)
                        .child(
                            view()
                                .size(px(6.0))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(move || {
                                    if active_dot.get() {
                                        theme::primary()
                                    } else {
                                        theme::ok()
                                    }
                                }),
                        )
                        .child(
                            text(name)
                                .number_of_lines(1)
                                .text_size(move || {
                                    if theme::detect_device_form().is_touch() {
                                        px(font_size::CAPTION)
                                    } else {
                                        px(font_size::SUBTITLE)
                                    }
                                })
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || {
                                    if active_fg.get() {
                                        theme::primary()
                                    } else {
                                        theme::fg()
                                    }
                                }),
                        ),
                )
                .child(icon_reactive(Icon::ChevronDown, 12.0, move || {
                    if active_chev.get() {
                        theme::primary().opacity(0.8)
                    } else {
                        theme::muted_fg()
                    }
                })),
        )
}

/// Semantic compatibility badge showing how a plugin executes across
/// desktop, iOS, Android, and HarmonyOS.
pub fn compat_badge(level: PluginCompatLevel) -> View {
    let (label, fg_col, bg_col) = match level {
        PluginCompatLevel::FullNative => ("All Platforms", theme::ok(), theme::ok_soft()),
        PluginCompatLevel::VirtualFfmpeg => ("Embedded FFmpeg", theme::warn(), theme::warn_soft()),
        PluginCompatLevel::DesktopProcessLimited => {
            ("Desktop Binary", theme::danger(), theme::danger_soft())
        }
    };
    compat_badge_labeled(level, label, fg_col, bg_col)
}

/// Compatibility badge with custom localized text.
pub fn compat_badge_text(level: PluginCompatLevel, label: impl IntoReactive<SharedString>) -> View {
    let (fg_col, bg_col) = match level {
        PluginCompatLevel::FullNative => (theme::ok(), theme::ok_soft()),
        PluginCompatLevel::VirtualFfmpeg => (theme::warn(), theme::warn_soft()),
        PluginCompatLevel::DesktopProcessLimited => (theme::danger(), theme::danger_soft()),
    };
    compat_badge_labeled(level, label, fg_col, bg_col)
}

fn compat_badge_labeled(
    _level: PluginCompatLevel,
    label: impl IntoReactive<SharedString>,
    fg_col: AdaptiveColor,
    bg_col: AdaptiveColor,
) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_1p5()
        .px_2p5()
        .py_1()
        .rounded_full()
        .bg(bg_col)
        .child(view().size(px(6.0)).rounded_full().bg(fg_col))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(fg_col),
        )
}

/// Centered empty-state panel with icon badge, heading, supporting copy, and optional CTA buttons.
pub fn empty_state(
    kind: Icon,
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
) -> View {
    empty_state_inner(kind, title, subtitle, None)
}

/// Centered empty-state panel with trailing action row (primary/secondary buttons).
pub fn empty_state_with_actions(
    kind: Icon,
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
    actions: View,
) -> View {
    empty_state_inner(kind, title, subtitle, Some(actions))
}

fn empty_state_inner(
    kind: Icon,
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
    actions: Option<View>,
) -> View {
    view()
        .w_full()
        .py(move || {
            if theme::detect_device_form().is_phone() {
                px(40.0)
            } else {
                px(48.0)
            }
        })
        .px(move || {
            if theme::detect_device_form().is_phone() {
                px(20.0)
            } else {
                px(24.0)
            }
        })
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            view()
                .size(px(60.0))
                .rounded_full()
                .bg(theme::muted)
                .border_1()
                .border_color(move || theme::border().opacity(0.75))
                .items_center()
                .justify_center()
                .child(icon(kind, 26.0, colors::MUTED_FOREGROUND)),
        )
        .child(
            text(title)
                .text_center()
                .text_size(px(font_size::HEADING))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::fg),
        )
        .child(
            text(subtitle)
                .max_w(px(360.0))
                .text_center()
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg),
        )
        .children(actions.map(|a| a.pt_1()))
}

/// Section header with accessible `Header` role, optional subtitle, and
/// optional trailing action button.
pub fn section_header(
    title: impl IntoReactive<SharedString>,
    subtitle: Option<SharedString>,
    action_label: Option<SharedString>,
    on_action: Option<Rc<dyn Fn()>>,
) -> View {
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
                    text(title)
                        .role(AccessibilityRole::Header)
                        .number_of_lines(1)
                        .text_size(move || {
                            if theme::detect_device_form().is_phone() {
                                px(font_size::HEADING)
                            } else {
                                px(font_size::TITLE)
                            }
                        })
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .children(subtitle.map(|sub| {
                    text(sub)
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg)
                })),
        )
        .children(match (action_label, on_action) {
            (Some(label), Some(cb)) => Some(
                button()
                    .on_press(move || cb())
                    .min_h(move || {
                        if theme::detect_device_form().is_touch() {
                            px(44.0)
                        } else {
                            px(28.0)
                        }
                    })
                    .px(move || {
                        if theme::detect_device_form().is_phone() {
                            px(8.0)
                        } else {
                            px(10.0)
                        }
                    })
                    .py_1()
                    .flex_shrink_0()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .rounded(px(radius::SM))
                    .hover(|s| s.bg(theme::hover))
                    .active(|s| s.opacity(0.7))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(
                        text(label)
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary),
                    )
                    .child(icon(Icon::ChevronRight, 14.0, colors::PRIMARY)),
            ),
            _ => None,
        })
}

/// Crisp 270° rounded-cap loading spinner rotated continuously on the native
/// UI animation thread (`900ms` linear loop; static under `reduce_motion()`,
/// automatically stopped on unmount with zero Rust polling timers).
pub fn loading_spinner(size_pt: f32, tint: impl IntoReactive<AdaptiveColor>) -> View {
    let tint_rx = tint.into_reactive();
    let angle = animated_value(0.0);
    let spin = Animation::timing(&angle, 360.0, Timing::new(900.0, Easing::Linear)).looped(None);
    let running = spin.clone();

    effect(move || {
        if reduce_motion() {
            running.stop();
            angle.set(0.0);
        } else {
            angle.set(0.0);
            let _ = running.start();
        }
    });
    cleanup(move || spin.stop());

    let stroke = (size_pt * 0.12).clamp(1.6, 3.2);
    let radius = ((size_pt - stroke) * 0.5).max(2.0);
    let center = size_pt * 0.5;

    view()
        .size(px(size_pt))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .pointer_events(PointerEvents::None)
        .bind_rotate(&angle)
        .child(
            canvas(move || {
                let resolved = color(tint_rx.get());
                Painter::new()
                    .line_width(stroke)
                    .line_cap(LineCap::Round)
                    .stroke_color(resolved.opacity(0.18))
                    .begin_path()
                    .arc(center, center, radius, 0.0, TAU)
                    .stroke()
                    .stroke_color(resolved)
                    .begin_path()
                    .arc(center, center, radius, -PI * 0.5, PI)
                    .stroke()
            })
            .size_full(),
        )
}

fn shimmer_block_styled(corner_radius: f32, base: AdaptiveColor, sheen: AdaptiveColor) -> View {
    const BAND_W: f32 = 220.0;
    let sweep = animated_value(0.0);
    // Match `mygo-music` skeleton cadence: 1100ms sweep + 500ms rest = 1600ms period.
    let anim = Animation::sequence([
        Animation::timing(
            &sweep,
            1.0,
            Timing::new(1100.0, Easing::Curve(Curve::Sin, EaseMode::InOut)),
        ),
        Animation::delay(500.0),
    ])
    .looped(None);
    let running = anim.clone();

    effect(move || {
        if reduce_motion() {
            running.stop();
            sweep.set(0.0);
        } else {
            sweep.set(0.0);
            let _ = running.start();
        }
    });
    cleanup(move || anim.stop());

    let translate_x = sweep.interpolate(&[0.0, 1.0], &[-BAND_W, 560.0]);

    view()
        .size_full()
        .rounded(px(corner_radius))
        .overflow_hidden()
        .bg(base)
        .child(when(
            move || !reduce_motion(),
            move || {
                view()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(BAND_W))
                    .pointer_events(PointerEvents::None)
                    .bind_translate_x(&translate_x)
                    .child(
                        canvas(move || {
                            let tint = color(sheen);
                            let grad = CanvasGradient::linear(
                                point(px(0.0), px(0.0)),
                                point(px(BAND_W), px(18.0)),
                            )
                            .add_color_stop(percentage(0.0), tint.alpha(0.0))
                            .add_color_stop(percentage(0.5), tint)
                            .add_color_stop(percentage(1.0), tint.alpha(0.0));
                            Painter::new()
                                .fill_style(grad)
                                .fill_rect(0.0, 0.0, BAND_W, 1024.0)
                        })
                        .size_full(),
                    )
            },
        ))
}

/// Loading placeholder block with smooth UI-thread animated sheen sweep
/// (static under `reduce_motion()`, zero Rust interval timers).
pub fn shimmer_block(corner_radius: f32) -> View {
    shimmer_block_styled(corner_radius, colors::SKELETON, colors::SKELETON_SHEEN)
}

/// Translucent white loading placeholder block for dark Now Playing stage
/// skeletons (lyrics, stage words, comments), matching `mygo-music` `npShimmer`.
pub fn shimmer_block_on_dark(corner_radius: f32) -> View {
    shimmer_block_styled(
        corner_radius,
        colors::NP_SKELETON_BASE,
        colors::NP_SKELETON_SHEEN,
    )
}

/// Loading placeholder skeleton card matching `MediaCard` proportions with shimmer sweep.
pub fn skeleton_card(circular: bool) -> View {
    let cover_radius = if circular { radius::PILL } else { radius::CARD };
    view()
        .w_full()
        .gap_2()
        .child(
            view()
                .w_full()
                .aspect_square()
                .child(shimmer_block(cover_radius)),
        )
        .child(
            view()
                .h(px(14.0))
                .w(relative(0.78))
                .child(shimmer_block(radius::SM)),
        )
        .child(
            view()
                .h(px(11.0))
                .w(relative(0.52))
                .child(shimmer_block(radius::SM)),
        )
}

/// Square (or circular) artwork container showing a remote/file image when
/// present with soft fade-in, or a warm deterministic gradient fallback with disc watermark.
pub fn artwork_box(
    uri: impl IntoReactive<SharedString>,
    seed: SharedString,
    size_pt: f32,
    corner_radius: f32,
) -> View {
    let uri_rx = uri.into_reactive();
    let (grad_a, grad_b) = theme::fallback_cover_gradient(&seed);
    let glyph = (size_pt * 0.42).clamp(16.0, 56.0);
    let has_img = {
        let uri_rx = uri_rx.clone();
        move || !uri_rx.get().is_empty()
    };

    view()
        .size(px(size_pt))
        .flex_shrink_0()
        .rounded(px(corner_radius))
        .overflow_hidden()
        .bg_linear_to_br()
        .gradient_from(AdaptiveColor::new(grad_a, grad_a))
        .gradient_to(AdaptiveColor::new(grad_b, grad_b))
        .border_1()
        .border_color(theme::border)
        .items_center()
        .justify_center()
        .child(icon(Icon::Disc, glyph, colors::NP_MUTED).opacity(0.65))
        .child(when(has_img, move || {
            let uri_rx = uri_rx.clone();
            image(move || uri_rx.get())
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
                .transition_opacity()
                .duration(move || motion::transition_ms(200.0))
        }))
}
