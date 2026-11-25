//! Responsive `MediaCard` for albums, playlists, artists, podcasts, and radio
//! stations across Desktop, Pad, and Phone form factors.

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;

use super::{
    icons::{Icon, icon},
    motion,
    theme::{self, Elevated, Elevation, colors, font_size, radius},
};

/// Builder for a responsive square or circular media card.
pub struct MediaCard {
    title: SharedString,
    subtitle: SharedString,
    image_url: SharedString,
    seed: SharedString,
    badge: Option<SharedString>,
    circular: bool,
    on_press: Option<Rc<dyn Fn()>>,
    on_play: Option<Rc<dyn Fn()>>,
}

/// Convenience constructor for [`MediaCard`].
pub fn media_card(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
) -> MediaCard {
    MediaCard::new(title, subtitle)
}

impl MediaCard {
    pub fn new(title: impl Into<SharedString>, subtitle: impl Into<SharedString>) -> Self {
        let title = title.into();
        let seed = title.clone();
        Self {
            title,
            subtitle: subtitle.into(),
            image_url: SharedString::default(),
            seed,
            badge: None,
            circular: false,
            on_press: None,
            on_play: None,
        }
    }

    pub fn image_url(mut self, url: impl Into<SharedString>) -> Self {
        self.image_url = url.into();
        self
    }

    pub fn seed(mut self, seed: impl Into<SharedString>) -> Self {
        self.seed = seed.into();
        self
    }

    pub fn badge(mut self, badge: impl Into<SharedString>) -> Self {
        let b = badge.into();
        if !b.is_empty() {
            self.badge = Some(b);
        }
        self
    }

    pub fn circular(mut self, circular: bool) -> Self {
        self.circular = circular;
        self
    }

    pub fn on_press(mut self, f: impl Fn() + 'static) -> Self {
        self.on_press = Some(Rc::new(f));
        self
    }

    pub fn on_play(mut self, f: impl Fn() + 'static) -> Self {
        self.on_play = Some(Rc::new(f));
        self
    }
}

impl IntoElement for MediaCard {
    fn into_element(self) -> AnyElement {
        let MediaCard {
            title,
            subtitle,
            image_url,
            seed,
            badge,
            circular,
            on_press,
            on_play,
        } = self;

        let (grad_a, grad_b) = theme::fallback_cover_gradient(&seed);
        let cover_radius = if circular {
            px(radius::PILL)
        } else {
            px(radius::CARD)
        };
        let has_img = !image_url.is_empty();
        let a11y_label: SharedString = if subtitle.is_empty() {
            title.clone()
        } else {
            format!("{title} · {subtitle}").into()
        };

        let cover = Elevated::elevation(
            view()
                .w_full()
                .aspect_square()
                .rounded(cover_radius)
                .overflow_hidden()
                .bg_linear_to_br()
                .gradient_from(AdaptiveColor::new(grad_a, grad_a))
                .gradient_to(AdaptiveColor::new(grad_b, grad_b))
                .border_1()
                .border_color(theme::border),
            Elevation::Card,
        )
            .items_center()
            .justify_center()
            .child(icon(Icon::Disc, 36.0, colors::NP_MUTED).opacity(0.55))
            .children(has_img.then(|| {
                view()
                    .absolute()
                    .inset_0()
                    .size_full()
                    .transition_transform()
                    .duration(move || motion::transition_ms(220.0))
                    .group_hover(|s| {
                        if reduce_motion() {
                            s
                        } else {
                            s.scale(1.04)
                        }
                    })
                    .child(
                        image(image_url)
                            .object_fit(ResizeMode::Cover)
                            .size_full()
                            .transition_opacity()
                            .duration(move || motion::transition_ms(200.0)),
                    )
            }))
            .children(badge.map(|label| {
                view()
                    .absolute()
                    .top_2()
                    .left_2()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(colors::MEDIA_BADGE_BG)
                    .pointer_events(PointerEvents::None)
                    .child(
                        text(label)
                            .number_of_lines(1)
                            .text_size(px(font_size::MICRO))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors::MEDIA_BADGE_FG),
                    )
            }))
            .children(on_play.map(|play_cb| {
                // Desktop reveals play FAB on hover (and keeps keyboard focusability);
                // Pad/Phone renders a compact always-reachable corner play button.
                let title_play: SharedString = format!("Play {title}").into();
                view()
                    .absolute()
                    .right_2()
                    .bottom_2()
                    .opacity(move || {
                        if theme::detect_device_form().is_desktop() {
                            0.0_f32
                        } else {
                            0.96_f32
                        }
                    })
                    .translate_y(move || {
                        if theme::detect_device_form().is_desktop() && !reduce_motion() {
                            px(6.0)
                        } else {
                            px(0.0)
                        }
                    })
                    .transition_all()
                    .duration(move || motion::transition_ms(180.0))
                    .group_hover(|s| s.opacity(1.0).translate_y(px(0.0)))
                    .child(
                        button()
                            .on_press(move || play_cb())
                            .accessibility_label(title_play)
                            .hit_slop(move || {
                                if theme::detect_device_form().is_touch() {
                                    EdgeInsets::all(px(5.0))
                                } else {
                                    EdgeInsets::default()
                                }
                            })
                            .size(px(36.0))
                            .rounded_full()
                            .bg(theme::primary)
                            .elevation(Elevation::Floating)
                            .items_center()
                            .justify_center()
                            .hover(|s| s.bg(theme::primary_hover).scale(1.06))
                            .active(|s| s.scale(motion::press_scale))
                            .focus_visible(|s| {
                                s.outline(px(2.0)).outline_color(theme::ring)
                            })
                            .child(icon(Icon::Play, 15.0, colors::PRIMARY_FOREGROUND)),
                    )
            }));

        let caption = view()
            .w_full()
            .px_0p5()
            .gap_0p5()
            .when(circular, |v| v.items_center())
            .child(
                text(title)
                    .number_of_lines(1)
                    .text_size(move || {
                        if theme::detect_device_form().is_phone() {
                            px(font_size::BODY_MOBILE)
                        } else {
                            px(font_size::BODY)
                        }
                    })
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::fg),
            )
            .children((!subtitle.is_empty()).then(|| {
                text(subtitle)
                    .number_of_lines(1)
                    .text_size(px(font_size::CAPTION))
                    .text_color(theme::muted_fg)
            }));

        let card_pad = move || {
            if theme::detect_device_form().is_desktop() {
                px(8.0)
            } else {
                px(4.0)
            }
        };

        button()
            .group()
            .accessibility_label(a11y_label)
            .on_press(move || {
                if let Some(cb) = &on_press {
                    cb();
                }
            })
            .w_full()
            .p(card_pad)
            .gap_2()
            .rounded(px(radius::LG))
            .bg(colors::TRANSPARENT)
            .transition_all()
            .duration(move || motion::transition_ms(160.0))
            .hover(|s| s.bg(theme::hover))
            .active(|s| {
                s.bg(theme::press)
                    .scale(motion::press_scale)
            })
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(cover)
            .child(caption)
            .into_element()
    }
}
