//! Home / Discover page aligned with `mygo-music`: clean empty onboarding state,
//! shimmer loading skeletons, Daily Mix stacked 3-cover hover fan-out card with
//! peeking vinyl, and responsive section shelves.

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::{Section, Track};

use crate::pages::{
    page_scroll, render_browse_sections, shimmer_card_grid, shimmer_track_list, shuffle_and_play,
};
use crate::state::{ActiveModal, AppStore, ArtworkLoader, Route};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state_with_actions, ghost_button,
    icon_button_sized, motion, primary_button, source_pill, theme,
};
use crate::ui::theme::{colors, font_size, radius};

pub fn home_page(store: AppStore, form: DeviceForm) -> View {
    let body = view()
        .w_full()
        .gap_6()
        .child(top_header(store, form))
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let home = store.home_data.get();

            untracked(move || {
                if loading && home.sections.is_empty() {
                    return view()
                        .w_full()
                        .gap_6()
                        .child(shimmer_card_grid(store, form, 6, false))
                        .child(shimmer_track_list(5))
                        .into_element();
                }

                if home.sections.is_empty() {
                    let actions = view()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .flex_wrap()
                        .gap_2p5()
                        .child(primary_button(
                            move || store.t("home.add_folder"),
                            move || store.pick_local_music_folders(),
                        ))
                        .child(ghost_button(
                            move || store.t("home.install_plugin"),
                            move || store.navigate(Route::Plugins),
                        ));
                    return empty_state_with_actions(
                        Icon::Disc,
                        move || store.t("home.empty_title"),
                        move || store.t("home.empty_subtitle"),
                        actions,
                    )
                    .into_element();
                }

                let mut stack = view().w_full().gap_6();

                // Highlight a Daily Mix / Featured stacked 3-cover hover fan-out card when
                // a track-bearing featured/daily section exists.
                if let Some((spotlight_title, spotlight_tracks)) =
                    find_daily_mix_spotlight(&home.sections)
                {
                    stack = stack.child(daily_mix_spotlight_card(
                        store,
                        form,
                        spotlight_title,
                        spotlight_tracks,
                    ));
                }

                stack = stack.child(render_browse_sections(store, form, home.sections));
                stack.into_element()
            })
        }));

    page_scroll(form, body)
}

fn top_header(store: AppStore, form: DeviceForm) -> View {
    let heading = computed(move || {
        store.home_data.with(|h| {
            if !h.title.is_empty() {
                SharedString::from(h.title.clone())
            } else {
                store.t("nav.home")
            }
        })
    });

    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_wrap()
        .gap_3()
        .child(
            text(heading)
                .role(AccessibilityRole::Header)
                .number_of_lines(1)
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
                .flex_row()
                .items_center()
                .gap_2()
                .child(source_pill(
                    move || store.active_source_name(),
                    true,
                    move || store.open_modal(ActiveModal::SourceSwitcher),
                ))
                .child(icon_button_sized(
                    Icon::Refresh,
                    34.0,
                    16.0,
                    theme::muted_fg,
                    move || store.t("common.refresh"),
                    move || store.refresh_home(),
                )),
        )
}

fn find_daily_mix_spotlight(sections: &[Section]) -> Option<(String, Vec<Track>)> {
    // Prefer an explicitly daily/featured/recommended track section; fall back to the first
    // track section with at least 3 tracks so local and plugin libraries both get the card.
    if let Some(sec) = sections.iter().find(|s| {
        if s.tracks.is_empty() {
            return false;
        }
        let lower = s.title.to_ascii_lowercase();
        lower.contains("daily")
            || lower.contains("featured")
            || lower.contains("mix")
            || s.title.contains("每日")
            || s.title.contains("推荐")
    }) {
        return Some((sec.title.clone(), sec.tracks.clone()));
    }

    sections
        .iter()
        .find(|s| s.tracks.len() >= 3)
        .map(|s| (s.title.clone(), s.tracks.clone()))
}

fn daily_mix_spotlight_card(
    store: AppStore,
    form: DeviceForm,
    title: String,
    tracks: Vec<Track>,
) -> View {
    let cover_for = |idx: usize| -> (SharedString, SharedString) {
        if let Some(tr) = tracks.get(idx) {
            let uri: SharedString = if !tr.local_cover.is_empty() {
                tr.local_cover.clone().into()
            } else {
                tr.album.image_url.clone().into()
            };
            let seed: SharedString = if !tr.album.id.is_empty() {
                tr.album.id.clone().into()
            } else {
                tr.id.clone().into()
            };
            (uri, seed)
        } else {
            (SharedString::default(), SharedString::from(format!("mix-{idx}")))
        }
    };

    let (uri0, seed0) = cover_for(0);
    let (uri1, seed1) = cover_for(1);
    let (uri2, seed2) = cover_for(2);
    let accent = ArtworkLoader::accent_for_seed(&seed0);

    let count_str = tracks.len().to_string();
    let count_label = store.t_fmt("track.count_fmt", &[("count", &count_str)]);
    let preview_line: SharedString = tracks
        .iter()
        .take(3)
        .map(|t| t.name.as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
        .into();

    let tracks_card = tracks.clone();
    let tracks_play = tracks.clone();
    let tracks_shuffle = tracks;

    let cover_size = if form.is_phone() { 84.0 } else { 104.0 };
    let vinyl_size = cover_size * 0.92;
    let stage_w = if form.is_phone() { 156.0 } else { 186.0 };
    let stage_h = cover_size + 22.0;

    // Stacked 3-cover fan-out + peeking vinyl behind right edge.
    let cover_stack = view()
        .w(px(stage_w))
        .h(px(stage_h))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .child(
            // Peeking vinyl disc sliding out on hover
            view()
                .absolute()
                .size(px(vinyl_size))
                .rounded_full()
                .bg(colors::NP_BG)
                .border_1()
                .border_color(colors::NP_BORDER)
                .translate_x(px(if form.is_phone() { 24.0 } else { 32.0 }))
                .transition_transform()
                .duration(move || motion::transition_ms(260.0))
                .group_hover(|s| {
                    if reduce_motion() {
                        s
                    } else {
                        s.translate_x(px(46.0)).rotate(deg(22.0))
                    }
                })
                .items_center()
                .justify_center()
                .child(
                    view()
                        .size(px(vinyl_size * 0.68))
                        .rounded_full()
                        .border_1()
                        .border_color(colors::NP_BORDER)
                        .items_center()
                        .justify_center()
                        .child(
                            view()
                                .size(px(vinyl_size * 0.30))
                                .rounded_full()
                                .bg(theme::primary)
                                .items_center()
                                .justify_center()
                                .child(view().size(px(6.0)).rounded_full().bg(colors::NP_BG)),
                        ),
                ),
        )
        .child(
            // Back-left cover fanning left on hover
            view()
                .absolute()
                .translate_x(px(-14.0))
                .translate_y(px(2.0))
                .rotate(if reduce_motion() { deg(0.0) } else { deg(-8.0) })
                .transition_transform()
                .duration(move || motion::transition_ms(240.0))
                .group_hover(|s| {
                    if reduce_motion() {
                        s
                    } else {
                        s.translate_x(px(-26.0))
                            .translate_y(px(-3.0))
                            .rotate(deg(-14.0))
                    }
                })
                .child(artwork_box(uri2, seed2, cover_size * 0.88, radius::CONTROL)),
        )
        .child(
            // Back-right cover fanning right on hover
            view()
                .absolute()
                .translate_x(px(12.0))
                .translate_y(px(2.0))
                .rotate(if reduce_motion() { deg(0.0) } else { deg(7.0) })
                .transition_transform()
                .duration(move || motion::transition_ms(240.0))
                .group_hover(|s| {
                    if reduce_motion() {
                        s
                    } else {
                        s.translate_x(px(22.0))
                            .translate_y(px(-2.0))
                            .rotate(deg(12.0))
                    }
                })
                .child(artwork_box(uri1, seed1, cover_size * 0.90, radius::CONTROL)),
        )
        .child(
            // Front lead cover
            Elevated::elevation(
                artwork_box(uri0, seed0, cover_size, radius::CARD)
                    .transition_transform()
                    .duration(move || motion::transition_ms(220.0))
                    .group_hover(|s| if reduce_motion() { s } else { s.scale(1.04) }),
                Elevation::Card,
            ),
        );

    let title_text = if title.is_empty() {
        store.t("browse.featured")
    } else {
        SharedString::from(title)
    };

    let info_col = view()
        .flex_1()
        .min_w(px(0.0))
        .gap_1p5()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    view()
                        .px_2p5()
                        .py_0p5()
                        .rounded_full()
                        .bg(theme::primary_soft)
                        .child(
                            text(move || store.t("browse.featured"))
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary),
                        ),
                )
                .child(
                    text(count_label)
                        .text_size(px(font_size::CAPTION))
                        .text_color(theme::muted_fg),
                ),
        )
        .child(
            text(title_text)
                .number_of_lines(1)
                .text_size(px(if form.is_phone() {
                    font_size::TITLE
                } else {
                    font_size::TITLE_LG
                }))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg),
        )
        .children((!preview_line.is_empty()).then(|| {
            text(preview_line)
                .number_of_lines(1)
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg)
        }))
        .child(
            view()
                .pt_2()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(primary_button(
                    move || store.t("player.play_all"),
                    move || store.play_list(tracks_play.clone(), 0),
                ))
                .child(ghost_button(
                    move || store.t("player.shuffle_all"),
                    move || shuffle_and_play(store, tracks_shuffle.clone()),
                )),
        );

    let layout = if form.is_phone() {
        view()
            .w_full()
            .items_center()
            .gap_4()
            .child(cover_stack)
            .child(info_col.w_full())
    } else {
        view()
            .w_full()
            .flex_row()
            .items_center()
            .gap_6()
            .child(cover_stack)
            .child(info_col)
    };

    view().w_full().child(
        button()
            .group()
            .on_press(move || store.play_list(tracks_card.clone(), 0))
            .w_full()
            .p(px(if form.is_phone() { 16.0 } else { 22.0 }))
            .rounded(px(radius::XL))
            .bg_linear_to_br()
            .gradient_from(theme::artwork_tint(accent))
            .gradient_to(theme::card())
            .border_1()
            .border_color(theme::artwork_wash(accent, 0.25))
            .elevation(Elevation::Card)
            .transition_all()
            .duration(move || motion::transition_ms(180.0))
            .hover(|s| s.border_color(theme::primary))
            .active(|s| s.scale(motion::press_scale))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(layout),
    )
}
