//! Adaptive Radio Stations page aligned with `egoist/mygo-music` (`ui_radio.go`):
//! - `<= 4` stations: pill switcher + tilted dual-cover radio hero card
//! - `> 4` stations: radio hero card + responsive `240px` station grid below
//! - Live track view inside the hero card when listening (front `600px` cover,
//!   peeking tilted next-up `240px` cover, clickable artist links, Like heart,
//!   Play/Pause with animated buffering spinner, and Next button)
//! - Animated refresh spinner and shimmer placeholders.

use nsh_core::prelude::*;
use nsh_music_audio::PlaybackStatus;
use nsh_music_core::model::{Station, Track};

use crate::pages::{
    ARTWORK_SIZE_CARD, ARTWORK_SIZE_HERO, artist_links_row, card_grid, page_scroll,
    refresh_pill_button, shimmer_card_grid, sized_artwork_url, sized_track_cover,
};
use crate::state::{AppStore, ArtworkLoader};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state, ghost_button, heart_button,
    icon, icon_button_sized, loading_spinner, media_card, pill_button, play_pause_button_buffering,
    primary_button, section_header, theme,
};

pub fn stations_page(store: AppStore, form: DeviceForm) -> View {
    let selected_idx = signal(0usize);

    let body = view()
        .w_full()
        .gap_6()
        .children((!form.is_phone()).then(|| {
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap_3()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2p5()
                        .child(icon(Icon::Radio, 22.0, colors::AMBER))
                        .child(
                            text(move || store.t("nav.stations"))
                                .role(AccessibilityRole::Header)
                                .text_size(px(font_size::DISPLAY))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::fg),
                        ),
                )
                .child(refresh_pill_button(
                    store,
                    form,
                    store.browse_loading,
                    move || store.refresh_home(),
                ))
        }))
        .child(dynamic(move || {
            let loading = store.browse_loading.get();
            let stations = store.stations.get();

            untracked(move || {
                if loading && stations.is_empty() {
                    return shimmer_card_grid(store, form, 4, false).into_element();
                }

                if stations.is_empty() {
                    return empty_state(
                        Icon::Radio,
                        store.t("common.empty"),
                        store.t("station.live_mix"),
                    )
                    .into_element();
                }

                // <= 4 stations: pill switcher + tilted dual-cover active hero card.
                if stations.len() <= 4 {
                    return compact_radio_switcher(store, form, stations, selected_idx)
                        .into_element();
                }

                // > 4 stations: hero radio card + responsive station grid underneath (`ui_radio.go:166-175`).
                let hero_section = radio_hero_host(store, form, stations.clone(), selected_idx);
                let cards: Vec<AnyElement> = stations
                    .iter()
                    .enumerate()
                    .map(|(idx, st)| {
                        let s1 = st.clone();
                        let s2 = st.clone();
                        media_card(st.name.clone(), st.description.clone())
                            .image_url(sized_artwork_url(&st.image_url, ARTWORK_SIZE_CARD))
                            .seed(st.id.clone())
                            .badge(store.t("station.live_mix"))
                            .on_press(move || {
                                selected_idx.set(idx);
                                store.play_station(s1.clone());
                            })
                            .on_play(move || {
                                selected_idx.set(idx);
                                store.play_station(s2.clone());
                            })
                            .into_element()
                    })
                    .collect();

                view()
                    .w_full()
                    .gap_6()
                    .child(hero_section)
                    .child(
                        view()
                            .w_full()
                            .gap_3()
                            .child(section_header(
                                move || store.t("nav.stations"),
                                None,
                                None,
                                None,
                            ))
                            .child(card_grid(store, form, cards)),
                    )
                    .into_element()
            })
        }));

    page_scroll(form, body)
}

fn compact_radio_switcher(
    store: AppStore,
    form: DeviceForm,
    stations: Vec<Station>,
    selected_idx: Signal<usize>,
) -> View {
    let count = stations.len();
    let is_touch = form.is_touch();
    let mut pills = view()
        .flex_row()
        .items_center()
        .gap(px(if form.is_phone() { 6.0 } else { 8.0 }))
        .py_1();
    for (idx, st) in stations.iter().enumerate() {
        let st_switch = st.clone();
        let mut pill = pill_button(
            st.name.clone(),
            move || selected_idx.get().min(count.saturating_sub(1)) == idx,
            move || {
                selected_idx.set(idx);
                if store.is_playing.get_untracked() {
                    store.play_station(st_switch.clone());
                }
            },
        );
        if is_touch {
            pill = pill.min_w(px(44.0)).min_h(px(44.0));
        }
        pills = pills.child(pill);
    }

    let hero_region = radio_hero_host(store, form, stations, selected_idx);

    view()
        .w_full()
        .gap_5()
        .child(scroll().horizontal().scrollbar(false).child(pills))
        .child(hero_region)
}

fn radio_hero_host(
    store: AppStore,
    form: DeviceForm,
    stations: Vec<Station>,
    selected_idx: Signal<usize>,
) -> View {
    view().w_full().child(dynamic(move || {
        let idx = selected_idx.get().min(stations.len().saturating_sub(1));
        let active_station = stations[idx].clone();
        let companion_station = stations
            .get((idx + 1) % stations.len())
            .cloned()
            .unwrap_or_else(|| active_station.clone());

        let active_src = store.active_source_id.get();
        let live_track = store
            .current_track
            .get()
            .filter(|tr| tr.source.is_empty() || tr.source == active_src);
        let next_track = live_track.as_ref().and_then(|_| {
            let cur_idx = store.current_index.get()?;
            store.queue.with(|q| q.get(cur_idx + 1).cloned())
        });

        untracked(move || {
            tilted_radio_hero(
                store,
                form,
                active_station,
                companion_station,
                live_track,
                next_track,
            )
            .into_element()
        })
    }))
}

fn tilted_radio_hero(
    store: AppStore,
    form: DeviceForm,
    station: Station,
    companion: Station,
    live_track: Option<Track>,
    next_track: Option<Track>,
) -> View {
    let front_seed: SharedString = match &live_track {
        Some(tr) if !tr.album.id.is_empty() => tr.album.id.clone().into(),
        Some(tr) => tr.id.clone().into(),
        None => station.id.clone().into(),
    };
    let front_uri: SharedString = match &live_track {
        Some(tr) => sized_track_cover(tr, ARTWORK_SIZE_HERO),
        None => sized_artwork_url(&station.image_url, ARTWORK_SIZE_HERO),
    };
    let back_uri: SharedString = match &next_track {
        Some(tr) => sized_track_cover(tr, ARTWORK_SIZE_CARD),
        None => sized_artwork_url(&companion.image_url, ARTWORK_SIZE_CARD),
    };
    let back_seed: SharedString = match &next_track {
        Some(tr) => format!("next-{}", tr.id).into(),
        None => format!("companion-{}", companion.id).into(),
    };

    let accent = ArtworkLoader::accent_for_seed(&front_seed);
    let is_phone = form.is_phone();
    let art_size = if is_phone { 132.0 } else { 172.0 };
    let reduced = reduce_motion();

    let dual_cover = view()
        .w(px(art_size + 44.0))
        .h(px(art_size + 28.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .child(
            // Back tilted companion / next-up cover
            Elevated::elevation(
                artwork_box(back_uri, back_seed, art_size * 0.88, radius::LG)
                    .absolute()
                    .translate_x(px(-18.0))
                    .translate_y(px(4.0))
                    .rotate(if reduced { deg(0.0) } else { deg(-9.0) })
                    .opacity(0.84),
                Elevation::Subtle,
            ),
        )
        .child(
            // Front tilted active cover (`600px`)
            Elevated::elevation(
                artwork_box(front_uri, front_seed, art_size, radius::XL)
                    .translate_x(px(10.0))
                    .rotate(if reduced { deg(0.0) } else { deg(3.5) }),
                Elevation::Card,
            ),
        );

    let is_buffering = computed(move || store.playback_status.get() == PlaybackStatus::Loading);

    let details = match live_track {
        Some(tr) => {
            let tr_id = tr.id.clone();
            let tr_for_like = tr.clone();
            let st_next = station.clone();
            let liked = computed(move || store.is_liked(&tr_id));

            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_2p5()
                .when(form.is_phone(), |v| v.items_center())
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .px_2p5()
                        .py_1()
                        .self_start()
                        .when(form.is_phone(), |v| v.self_center())
                        .rounded_full()
                        .bg(theme::amber_soft)
                        .child(icon(Icon::Radio, 14.0, colors::AMBER))
                        .child(
                            text(station.name)
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::amber),
                        ),
                )
                .child(
                    text(tr.name)
                        .role(AccessibilityRole::Header)
                        .number_of_lines(2)
                        .when(is_phone, |t| t.text_center())
                        .text_size(px(if is_phone {
                            font_size::TITLE_LG
                        } else {
                            font_size::DISPLAY
                        }))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .child(
                    when(is_buffering, move || {
                        view()
                            .flex_row()
                            .items_center()
                            .when(is_phone, |v| v.justify_center())
                            .gap_1p5()
                            .child(loading_spinner(13.0, colors::PRIMARY))
                            .child(
                                text(move || store.t("player.buffering"))
                                    .text_size(px(font_size::SUBTITLE))
                                    .text_color(theme::muted_fg),
                            )
                    })
                    .otherwise(move || {
                        view()
                            .when(is_phone, |v| v.w_full().items_center().justify_center())
                            .child(artist_links_row(
                                store,
                                &tr.artists,
                                &tr.show_id,
                                store.t("track.unknown_artist"),
                                font_size::SUBTITLE,
                                form,
                            ))
                    }),
                )
                .child(
                    view()
                        .pt_2()
                        .flex_row()
                        .items_center()
                        .when(is_phone, |v| v.w_full().justify_center())
                        .flex_wrap()
                        .gap_3()
                        .child(heart_button(
                            liked,
                            move || store.t("player.like"),
                            move || store.toggle_like(tr_for_like.clone()),
                        ))
                        .child(play_pause_button_buffering(
                            store.is_playing,
                            is_buffering,
                            48.0,
                            move || {
                                if store.is_playing.get() {
                                    store.t("player.pause")
                                } else {
                                    store.t("player.play")
                                }
                            },
                            move || store.toggle_play(),
                        ))
                        .child(icon_button_sized(
                            Icon::Next,
                            40.0,
                            20.0,
                            theme::fg,
                            move || store.t("player.next"),
                            move || store.next_track(),
                        ))
                        .child(
                            ghost_button(
                                move || store.t("station.refresh_mix"),
                                move || store.play_station(st_next.clone()),
                            )
                            .min_h(px(form.min_touch_target()))
                            .when(is_phone, |b| b.rounded_full()),
                        ),
                )
        }
        None => {
            let st_tune = station.clone();
            let st_next = station.clone();
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_2p5()
                .when(is_phone, |v| v.items_center())
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .px_2p5()
                        .py_1()
                        .self_start()
                        .when(is_phone, |v| v.self_center())
                        .rounded_full()
                        .bg(theme::amber_soft)
                        .child(icon(Icon::Radio, 14.0, colors::AMBER))
                        .child(
                            text(move || store.t("station.live_mix"))
                                .text_size(px(font_size::MICRO))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::amber),
                        ),
                )
                .child(
                    text(station.name)
                        .role(AccessibilityRole::Header)
                        .number_of_lines(2)
                        .when(is_phone, |t| t.text_center())
                        .text_size(px(if is_phone {
                            font_size::TITLE_LG
                        } else {
                            font_size::DISPLAY
                        }))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                )
                .children((!station.description.is_empty()).then(|| {
                    text(station.description)
                        .number_of_lines(3)
                        .when(is_phone, |t| t.text_center())
                        .text_size(px(font_size::SUBTITLE))
                        .text_color(theme::muted_fg)
                }))
                .child(
                    view()
                        .pt_2()
                        .flex_row()
                        .items_center()
                        .when(is_phone, |v| v.w_full().justify_center())
                        .flex_wrap()
                        .gap_2p5()
                        .child(
                            primary_button(
                                move || {
                                    if is_buffering.get() {
                                        store.t("player.buffering")
                                    } else {
                                        store.t("station.tune_in")
                                    }
                                },
                                move || store.play_station(st_tune.clone()),
                            )
                            .min_h(px(form.min_touch_target()))
                            .when(is_phone, |b| b.flex_1().rounded_full()),
                        )
                        .child(
                            ghost_button(
                                move || store.t("station.refresh_mix"),
                                move || store.play_station(st_next.clone()),
                            )
                            .min_h(px(form.min_touch_target()))
                            .when(is_phone, |b| b.flex_1().rounded_full()),
                        ),
                )
        }
    };

    let inner = if is_phone {
        view()
            .w_full()
            .items_center()
            .gap_4()
            .child(dual_cover)
            .child(details.w_full())
    } else {
        view()
            .w_full()
            .flex_row()
            .items_center()
            .gap_7()
            .child(dual_cover)
            .child(details)
    };

    Elevated::elevation(
        view()
            .w_full()
            .p(px(if is_phone { 16.0 } else { 28.0 }))
            .rounded(px(radius::XL))
            .overflow_hidden()
            .bg_linear_to_br()
            .gradient_from(theme::artwork_tint(accent))
            .gradient_to(theme::card())
            .border_1()
            .border_color(theme::artwork_wash(accent, 0.25)),
        Elevation::Card,
    )
    .child(inner)
}
