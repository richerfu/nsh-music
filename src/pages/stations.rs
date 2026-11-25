//! Adaptive Radio Stations page aligned with `mygo-music`:
//! - `<= 4` stations: segmented pill switcher + tilted dual-cover active radio hero card
//! - `> 4` stations: responsive `MediaCard` grid
//! - Empty / loading states with shimmer placeholders.

use nsh_core::prelude::*;
use nsh_music_core::model::Station;

use crate::pages::{card_grid, page_scroll, shimmer_card_grid};
use crate::state::{AppStore, ArtworkLoader};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state, ghost_button, icon,
    media_card, pill_button, primary_button, theme,
};
use crate::ui::theme::{colors, font_size, radius};

pub fn stations_page(store: AppStore, form: DeviceForm) -> View {
    let selected_idx = signal(0usize);

    let body = view()
        .w_full()
        .gap_6()
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
                        .flex_row()
                        .items_center()
                        .gap_2p5()
                        .child(icon(Icon::Radio, 22.0, colors::AMBER))
                        .child(
                            text(move || store.t("nav.stations"))
                                .role(AccessibilityRole::Header)
                                .text_size(px(if form.is_phone() {
                                    font_size::TITLE_LG
                                } else {
                                    font_size::DISPLAY
                                }))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::fg),
                        ),
                )
                .child(ghost_button(move || store.t("common.refresh"), move || {
                    store.refresh_home();
                })),
        )
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

                // <= 4 stations: pill/segmented switcher + tilted dual-cover active hero card.
                if stations.len() <= 4 {
                    return compact_radio_switcher(store, form, stations, selected_idx)
                        .into_element();
                }

                // > 4 stations: responsive station grid.
                let cards: Vec<AnyElement> = stations
                    .iter()
                    .map(|st| {
                        let s1 = st.clone();
                        let s2 = st.clone();
                        media_card(st.name.clone(), st.description.clone())
                            .image_url(st.image_url.clone())
                            .seed(st.id.clone())
                            .badge(store.t("station.live_mix"))
                            .on_press(move || store.play_station(s1.clone()))
                            .on_play(move || store.play_station(s2.clone()))
                            .into_element()
                    })
                    .collect();

                card_grid(store, form, cards).into_element()
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
    let mut pills = view().flex_row().items_center().gap_2().py_1();
    for (idx, st) in stations.iter().enumerate() {
        pills = pills.child(pill_button(
            st.name.clone(),
            move || selected_idx.get().min(count.saturating_sub(1)) == idx,
            move || selected_idx.set(idx),
        ));
    }

    let stations_for_hero = stations;
    let hero_region = view().w_full().child(dynamic(move || {
        let idx = selected_idx
            .get()
            .min(stations_for_hero.len().saturating_sub(1));
        let active = stations_for_hero[idx].clone();
        let companion = stations_for_hero
            .get((idx + 1) % stations_for_hero.len())
            .cloned()
            .unwrap_or_else(|| active.clone());

        untracked(move || tilted_radio_hero(store, form, active, companion).into_element())
    }));

    view()
        .w_full()
        .gap_5()
        .child(scroll().horizontal().scrollbar(false).child(pills))
        .child(hero_region)
}

fn tilted_radio_hero(
    store: AppStore,
    form: DeviceForm,
    station: Station,
    companion: Station,
) -> View {
    let accent = ArtworkLoader::accent_for_seed(&station.id);
    let art_size = if form.is_phone() { 136.0 } else { 164.0 };
    let reduced = reduce_motion();

    let dual_cover = view()
        .w(px(art_size + 44.0))
        .h(px(art_size + 28.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .child(
            // Back tilted companion cover
            Elevated::elevation(
                artwork_box(
                    companion.image_url.clone(),
                    format!("companion-{}", companion.id).into(),
                    art_size * 0.88,
                    radius::LG,
                )
                .absolute()
                .translate_x(px(-18.0))
                .translate_y(px(4.0))
                .rotate(if reduced { deg(0.0) } else { deg(-9.0) })
                .opacity(0.86),
                Elevation::Subtle,
            ),
        )
        .child(
            // Front tilted active station cover
            Elevated::elevation(
                artwork_box(
                    station.image_url.clone(),
                    station.id.clone().into(),
                    art_size,
                    radius::XL,
                )
                .translate_x(px(10.0))
                .rotate(if reduced { deg(0.0) } else { deg(3.5) }),
                Elevation::Card,
            ),
        );

    let st_tune = station.clone();
    let st_next = station.clone();

    let details = view()
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
                .text_size(px(if form.is_phone() {
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
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg)
        }))
        .child(
            view()
                .pt_2()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(primary_button(
                    move || store.t("station.tune_in"),
                    move || store.play_station(st_tune.clone()),
                ))
                .child(ghost_button(
                    move || store.t("station.refresh_mix"),
                    move || store.play_station(st_next.clone()),
                )),
        );

    let inner = if form.is_phone() {
        view()
            .w_full()
            .items_center()
            .gap_5()
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
            .p(px(if form.is_phone() { 20.0 } else { 28.0 }))
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
