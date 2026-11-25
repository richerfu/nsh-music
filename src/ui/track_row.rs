//! Responsive `TrackRow` displaying track index or live canvas equalizer,
//! artwork thumbnail, title/quality/source metadata, album (hidden on Phone),
//! tabular duration, animated like heart, and queue/more action.

use std::rc::Rc;

use nsh_base::button;
use nsh_core::{
    geometry::{point, size},
    prelude::*,
};
use nsh_music_core::model::Track;

use super::{
    controls::{artwork_box, heart_button, icon_button_sized},
    icons::{Icon, icon, icon_reactive},
    motion,
    theme::{self, DeviceForm, colors, font_size, radius},
};

/// Formats a millisecond duration as `m:ss` or `h:mm:ss`.
pub fn format_duration_ms(ms: i64) -> String {
    let total_secs = (ms.max(0) / 1000) as u64;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

/// Live 3-bar canvas equalizer rendered inside the index column when a track is
/// actively playing (falls back to a static indicator when `reduce_motion()`).
pub fn mini_equalizer(
    spectrum: impl IntoReactive<[f32; 16]>,
    playing: impl IntoReactive<bool>,
) -> View {
    let spec_rx = spectrum.into_reactive();
    let playing_rx = playing.into_reactive();

    view()
        .size(px(16.0))
        .pointer_events(PointerEvents::None)
        .child(
            canvas(move || {
                let tint = color(theme::primary());
                let is_playing = playing_rx.get();
                let reduced = reduce_motion();
                let levels = if is_playing && !reduced {
                    let s = spec_rx.get();
                    [
                        (s[1] * 0.8 + s[2] * 0.2).clamp(0.22, 1.0),
                        (s[4] * 0.7 + s[5] * 0.3).clamp(0.28, 1.0),
                        (s[8] * 0.7 + s[9] * 0.3).clamp(0.22, 1.0),
                    ]
                } else if is_playing {
                    [0.55, 0.90, 0.48]
                } else {
                    [0.24, 0.24, 0.24]
                };

                let mut p = Painter::new().fill_color(tint);
                for (i, level) in levels.iter().enumerate() {
                    let x = 2.0 + i as f32 * 4.5;
                    let h = (level * 12.0).max(2.5);
                    let y = 14.0 - h;
                    p = p
                        .begin_path()
                        .round_rect(
                            Bounds::new(point(px(x), px(y)), size(px(2.8), px(h))),
                            px(1.2),
                        )
                        .fill();
                }
                p
            })
            .size_full(),
        )
}

/// Responsive track list row builder.
pub struct TrackRow {
    track: Track,
    index: Option<usize>,
    active: Reactive<bool>,
    playing: Reactive<bool>,
    liked: Reactive<bool>,
    spectrum: Reactive<[f32; 16]>,
    quality_badge: Option<SharedString>,
    source_badge: Option<SharedString>,
    fallback_artist: SharedString,
    show_cover: bool,
    on_play: Option<Rc<dyn Fn()>>,
    on_toggle_like: Option<Rc<dyn Fn()>>,
    on_more: Option<Rc<dyn Fn()>>,
}

/// Convenience constructor for [`TrackRow`].
pub fn track_row(track: Track) -> TrackRow {
    TrackRow::new(track)
}

impl TrackRow {
    pub fn new(track: Track) -> Self {
        Self {
            track,
            index: None,
            active: Reactive::Static(false),
            playing: Reactive::Static(false),
            liked: Reactive::Static(false),
            spectrum: Reactive::Static([0.0; 16]),
            quality_badge: None,
            source_badge: None,
            fallback_artist: SharedString::default(),
            show_cover: true,
            on_play: None,
            on_toggle_like: None,
            on_more: None,
        }
    }

    pub fn fallback_artist(mut self, label: impl Into<SharedString>) -> Self {
        self.fallback_artist = label.into();
        self
    }

    pub fn index(mut self, idx: usize) -> Self {
        self.index = Some(idx);
        self
    }

    pub fn active(mut self, active: impl IntoReactive<bool>) -> Self {
        self.active = active.into_reactive();
        self
    }

    pub fn playing(mut self, playing: impl IntoReactive<bool>) -> Self {
        self.playing = playing.into_reactive();
        self
    }

    pub fn liked(mut self, liked: impl IntoReactive<bool>) -> Self {
        self.liked = liked.into_reactive();
        self
    }

    pub fn spectrum(mut self, spectrum: impl IntoReactive<[f32; 16]>) -> Self {
        self.spectrum = spectrum.into_reactive();
        self
    }

    pub fn quality_badge(mut self, badge: impl Into<SharedString>) -> Self {
        let b = badge.into();
        if !b.is_empty() {
            self.quality_badge = Some(b);
        }
        self
    }

    pub fn source_badge(mut self, badge: impl Into<SharedString>) -> Self {
        let b = badge.into();
        if !b.is_empty() {
            self.source_badge = Some(b);
        }
        self
    }

    pub fn show_cover(mut self, show: bool) -> Self {
        self.show_cover = show;
        self
    }

    pub fn on_play(mut self, f: impl Fn() + 'static) -> Self {
        self.on_play = Some(Rc::new(f));
        self
    }

    pub fn on_toggle_like(mut self, f: impl Fn() + 'static) -> Self {
        self.on_toggle_like = Some(Rc::new(f));
        self
    }

    pub fn on_more(mut self, f: impl Fn() + 'static) -> Self {
        self.on_more = Some(Rc::new(f));
        self
    }
}

impl IntoElement for TrackRow {
    fn into_element(self) -> AnyElement {
        let TrackRow {
            track,
            index,
            active,
            playing,
            liked,
            spectrum,
            quality_badge,
            source_badge,
            fallback_artist,
            show_cover,
            on_play,
            on_toggle_like,
            on_more,
        } = self;

        let title: SharedString = track.name.clone().into();
        let artists: SharedString = {
            let joined = track.artist_names();
            if joined.is_empty() {
                fallback_artist
            } else {
                joined.into()
            }
        };
        let album_name: SharedString = track.album.name.clone().into();
        let duration_text: SharedString = format_duration_ms(track.duration).into();
        let cover_uri: SharedString = if !track.local_cover.is_empty() {
            track.local_cover.clone().into()
        } else {
            track.album.image_url.clone().into()
        };
        let cover_seed: SharedString = if !track.album.id.is_empty() {
            track.album.id.clone().into()
        } else {
            track.id.clone().into()
        };
        let unavailable = track.unavailable;

        let active_bg = active.clone();
        let active_title = active.clone();
        let active_idx = active.clone();
        let playing_eq = playing.clone();
        let spec_eq = spectrum;

        let leading_index = index.map(|idx| {
            let idx_str: SharedString = format!("{}", idx + 1).into();
            let active_idx = active_idx.clone();
            let playing_eq = playing_eq.clone();
            let spec_eq = spec_eq.clone();
            // Hide leading track number on Phone when cover art is already shown
            // to avoid crowding 390pt rows; album lists (`!show_cover`) keep indices.
            let show_leading = move || !show_cover || !theme::detect_device_form().is_phone();
            when(show_leading, move || {
                let active_idx = active_idx.clone();
                let playing_eq = playing_eq.clone();
                let spec_eq = spec_eq.clone();
                let idx_str = idx_str.clone();
                let is_active_playing = {
                    let active_idx = active_idx.clone();
                    let playing_eq = playing_eq.clone();
                    move || active_idx.get() && playing_eq.get()
                };
                view()
                    .w(px(28.0))
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .child(
                        when(is_active_playing, move || {
                            mini_equalizer(spec_eq.clone(), playing_eq.clone())
                        })
                        .otherwise(move || {
                            let active_inner = active_idx.clone();
                            let idx_inner = idx_str.clone();
                            when(
                                move || active_inner.get(),
                                || icon(Icon::Play, 13.0, colors::PRIMARY),
                            )
                            .otherwise(move || {
                                text(idx_inner.clone())
                                    .tabular_nums()
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::muted_fg)
                            })
                        }),
                    )
            })
        });

        let cover_thumb = show_cover.then(|| {
            let form = untracked(theme::detect_device_form);
            let (size_pt, corner) = match form {
                DeviceForm::Desktop => (40.0, radius::SM),
                DeviceForm::Pad { .. } => (44.0, radius::SM),
                DeviceForm::Phone => (46.0, radius::CONTROL),
            };
            artwork_box(cover_uri, cover_seed, size_pt, corner)
        });

        let badges_row = (quality_badge.is_some() || source_badge.is_some()).then(|| {
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .children(quality_badge.map(|q| {
                    view()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(theme::amber_soft)
                        .child(
                            text(q)
                                .text_size(px(10.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::amber),
                        )
                }))
                .children(source_badge.map(|src| {
                    view()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(theme::secondary)
                        .child(
                            text(src)
                                .text_size(px(10.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::muted_fg),
                        )
                }))
        });

        let title_col = view()
            .flex_1()
            .min_w(px(0.0))
            .gap_0p5()
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .min_w(px(0.0))
                    .child(
                        text(title.clone())
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(move || {
                                if theme::detect_device_form().is_phone() {
                                    px(font_size::BODY_MOBILE)
                                } else {
                                    px(font_size::BODY)
                                }
                            })
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || {
                                if active_title.get() {
                                    theme::primary()
                                } else {
                                    theme::fg()
                                }
                            }),
                    )
                    .children(badges_row),
            )
            .child(
                text(artists)
                    .number_of_lines(1)
                    .text_size(move || {
                        if theme::detect_device_form().is_phone() {
                            px(12.5)
                        } else {
                            px(font_size::CAPTION)
                        }
                    })
                    .text_color(theme::muted_fg),
            );

        // Album column: visible on Desktop and Pad landscape, hidden on Phone & portrait Pad.
        let show_album = move || {
            matches!(
                theme::detect_device_form(),
                DeviceForm::Desktop | DeviceForm::Pad { landscape: true }
            )
        };
        let album_col = (!album_name.is_empty()).then(|| {
            when(show_album, move || {
                view().w(px(180.0)).flex_shrink_0().pr_3().child(
                    text(album_name.clone())
                        .number_of_lines(1)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg),
                )
            })
        });

        let trailing = view()
            .flex_row()
            .items_center()
            .gap(move || {
                if theme::detect_device_form().is_phone() {
                    px(2.0)
                } else {
                    px(4.0)
                }
            })
            .flex_shrink_0()
            .children(on_toggle_like.map(|like_cb| {
                let liked_label: SharedString = format!("Like {title}").into();
                if untracked(theme::detect_device_form).is_phone() {
                    // Visually compact 34pt heart with 5pt hit_slop (>= 44pt effective tap
                    // target) so Phone rows stay airy on 390pt widths.
                    let liked_icon = liked.clone();
                    let liked_tint = liked.clone();
                    let liked_sel = liked;
                    let scale = animated_value(1.0);
                    let was_liked = stored(untracked(|| liked_sel.get()));
                    {
                        let liked_eff = liked_sel.clone();
                        effect(move || {
                            let now = liked_eff.get();
                            if was_liked.replace(now) != now && now {
                                motion::pulse_pop(&scale);
                            }
                        });
                    }
                    button()
                        .selected(move || liked_sel.get())
                        .on_press(move || like_cb())
                        .accessibility_label(liked_label)
                        .hit_slop(EdgeInsets::all(px(5.0)))
                        .size(px(34.0))
                        .flex_shrink_0()
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
                                        if liked_icon.get() {
                                            Icon::HeartFilled
                                        } else {
                                            Icon::Heart
                                        }
                                    },
                                    17.0,
                                    move || {
                                        if liked_tint.get() {
                                            theme::primary()
                                        } else {
                                            theme::muted_fg()
                                        }
                                    },
                                )),
                        )
                        .into_element()
                } else {
                    heart_button(liked, liked_label, move || like_cb()).into_element()
                }
            }))
            .child(when(
                move || !theme::detect_device_form().is_phone(),
                move || {
                    text(duration_text.clone())
                        .tabular_nums()
                        .w(px(44.0))
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg)
                },
            ))
            .children(on_more.map(|more_cb| {
                let more_label: SharedString = format!("More options for {title}").into();
                icon_button_sized(
                    Icon::MoreHorizontal,
                    32.0,
                    16.0,
                    theme::muted_fg,
                    more_label,
                    move || more_cb(),
                )
            }));

        let row_min_h = move || px(theme::detect_device_form().track_row_height());
        let row_a11y: SharedString = format!("Play {title}").into();

        view()
            .w_full()
            .min_h(row_min_h)
            .px(move || {
                if theme::detect_device_form().is_phone() {
                    px(8.0)
                } else {
                    px(10.0)
                }
            })
            .py(move || {
                if theme::detect_device_form().is_phone() {
                    px(6.0)
                } else {
                    px(4.0)
                }
            })
            .flex_row()
            .items_center()
            .gap(move || {
                if theme::detect_device_form().is_phone() {
                    px(10.0)
                } else {
                    px(12.0)
                }
            })
            .rounded(px(radius::CONTROL))
            .bg(move || {
                if active_bg.get() {
                    theme::selected()
                } else {
                    colors::TRANSPARENT
                }
            })
            .when(unavailable, |v| v.opacity(0.45))
            .transition_colors()
            .duration(move || motion::transition_ms(120.0))
            .hover(|s| s.bg(theme::hover))
            .child(
                button()
                    .disabled(unavailable)
                    .accessibility_label(row_a11y)
                    .on_press(move || {
                        if let Some(cb) = &on_play {
                            cb();
                        }
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(move || px(theme::detect_device_form().min_touch_target()))
                    .flex_row()
                    .items_center()
                    .gap(move || {
                        if theme::detect_device_form().is_phone() {
                            px(10.0)
                        } else {
                            px(12.0)
                        }
                    })
                    .active(|s| s.opacity(0.75))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .children(leading_index)
                    .children(cover_thumb)
                    .child(title_col)
                    .children(album_col),
            )
            .child(trailing)
            .into_element()
    }
}
