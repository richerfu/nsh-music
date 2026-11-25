//! Podcast Shows directory and Podcast Show detail page with shimmer loading,
//! full episode list, publish dates, duration badges, expandable show notes,
//! and capability-gated subscribe toggle.

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::Track;

use crate::pages::{
    card_grid, detail_hero, page_scroll, shimmer_card_grid, shimmer_detail_hero,
    shimmer_track_list,
};
use crate::state::{AppStore, Route};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state, format_duration_ms,
    ghost_button, heart_button, icon, icon_button_sized, media_card, mini_equalizer, motion,
    primary_button, section_header, theme,
};
use crate::ui::theme::{colors, font_size, radius};

/// Podcast Shows directory (`Route::Podcasts`).
pub fn shows_page(store: AppStore, form: DeviceForm) -> View {
    let header = view()
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
                .child(icon(Icon::Podcast, 22.0, colors::PRIMARY))
                .child(
                    text(move || store.t("nav.podcasts"))
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
        }));

    let grid = view().w_full().child(dynamic(move || {
        let loading = store.browse_loading.get();
        let shows = store.podcasts.get();

        untracked(move || {
            if loading && shows.is_empty() {
                return shimmer_card_grid(store, form, 6, false).into_element();
            }

            if shows.is_empty() {
                return empty_state(
                    Icon::Podcast,
                    store.t("common.empty"),
                    store.t("nav.podcasts"),
                )
                .into_element();
            }

            let cards: Vec<AnyElement> = shows
                .iter()
                .map(|show| {
                    let sid = show.id.clone();
                    let sid_play = show.id.clone();
                    let count_str = show.episode_count.to_string();
                    let badge = if show.subscribed {
                        store.t("podcast.subscribed")
                    } else if show.episode_count > 0 {
                        store.t_fmt("library.episodes_count", &[("count", &count_str)])
                    } else {
                        store.t("nav.podcasts")
                    };
                    let sub = if !show.publisher.is_empty() {
                        show.publisher.clone()
                    } else {
                        show.description.clone()
                    };

                    media_card(show.name.clone(), sub)
                        .image_url(show.image_url.clone())
                        .seed(show.id.clone())
                        .badge(badge)
                        .on_press(move || {
                            store.navigate(Route::ShowDetail { id: sid.clone() });
                        })
                        .on_play(move || {
                            let src = store.active_source();
                            let id = sid_play.clone();
                            spawn(async move {
                                if let Ok(Ok(page)) = blocking(move || src.show(&id, 0)).await
                                    && !page.episodes.is_empty()
                                {
                                    store.play_list(page.episodes, 0);
                                }
                            });
                        })
                        .into_element()
                })
                .collect();

            card_grid(store, form, cards).into_element()
        })
    }));

    page_scroll(form, view().w_full().gap_6().child(header).child(grid))
}

/// Podcast Show detail page (`Route::ShowDetail`) showing show metadata,
/// Subscribe button, and rich episode cards with duration & show notes.
pub fn show_detail_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let body = view().w_full().child(dynamic(move || {
        let active = store.active_show.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|s| s.show.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form))
                    .child(shimmer_track_list(5))
                    .into_element();
            };

            let show = &page.show;
            let ep_count = if page.episodes.is_empty() {
                show.episode_count
            } else {
                page.episodes.len()
            };
            let ep_count_str = ep_count.to_string();
            let meta: SharedString =
                store.t_fmt("library.episodes_count", &[("count", &ep_count_str)]);

            let episodes_play = page.episodes.clone();
            let subscribed_now = show.subscribed;
            let show_id_sub = show.id.clone();
            let page_for_sub = page.clone();
            let can_subscribe = store.active_can("subscribeShow");

            let actions = view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2p5()
                .child(
                    primary_button(move || store.t("player.play"), move || {
                        if !episodes_play.is_empty() {
                            store.play_list(episodes_play.clone(), 0);
                        }
                    })
                    .disabled(page.episodes.is_empty()),
                )
                .children(can_subscribe.then(move || {
                    ghost_button(
                        move || {
                            if subscribed_now {
                                store.t("podcast.subscribed")
                            } else {
                                store.t("podcast.subscribe")
                            }
                        },
                        move || {
                            let next = !subscribed_now;
                            let mut updated = page_for_sub.clone();
                            updated.show.subscribed = next;
                            store.active_show.set(Some(updated));
                            let src = store.active_source();
                            let sid = show_id_sub.clone();
                            spawn(async move {
                                let _ = blocking(move || src.subscribe_show(&sid, next)).await;
                            });
                            store.toasts.info(if next {
                                store.t("podcast.subscribed")
                            } else {
                                store.t("podcast.subscribe")
                            });
                        },
                    )
                }));

            let hero = detail_hero(
                store,
                form,
                store.t("nav.podcasts"),
                show.name.clone().into(),
                show.publisher.clone().into(),
                meta,
                show.description.clone().into(),
                show.image_url.clone().into(),
                show.id.clone().into(),
                false,
                actions,
            );

            let episodes_view = if page.episodes.is_empty() {
                empty_state(
                    Icon::Podcast,
                    store.t("common.empty"),
                    store.t("podcast.episodes"),
                )
            } else {
                let queue = Rc::new(page.episodes.clone());
                let mut list = view().w_full().gap_3();
                for (idx, ep) in page.episodes.iter().enumerate() {
                    list = list.child(episode_card(store, form, ep.clone(), Rc::clone(&queue), idx));
                }
                view()
                    .w_full()
                    .gap_3()
                    .child(section_header(
                        move || store.t("podcast.episodes"),
                        None,
                        None,
                        None,
                    ))
                    .child(list)
            };

            view()
                .w_full()
                .gap_6()
                .child(hero)
                .child(episodes_view)
                .into_element()
        })
    }));

    page_scroll(form, body)
}

fn episode_card(
    store: AppStore,
    form: DeviceForm,
    ep: Track,
    queue: Rc<Vec<Track>>,
    idx: usize,
) -> View {
    let ep_id_active = ep.id.clone();
    let ep_id_like = ep.id.clone();
    let ep_for_like = ep.clone();
    let ep_for_queue = ep.clone();
    let queue_play = Rc::clone(&queue);
    let expanded = signal(false);

    let cover_uri: SharedString = if !ep.local_cover.is_empty() {
        ep.local_cover.clone().into()
    } else {
        ep.album.image_url.clone().into()
    };
    let seed: SharedString = if !ep.show_id.is_empty() {
        ep.show_id.clone().into()
    } else {
        ep.id.clone().into()
    };
    let duration = format_duration_ms(ep.duration);
    let has_notes = !ep.description.trim().is_empty();
    let notes = ep.description.clone();
    let title = ep.name.clone();
    let published = ep.published.clone();

    let is_active = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(|c| c.id == ep_id_active))
    });

    Elevated::elevation(
        view()
            .w_full()
            .p(px(if form.is_phone() { 14.0 } else { 16.0 }))
            .gap_3()
            .rounded(px(radius::CARD))
            .bg(move || {
                if is_active.get() {
                    theme::selected()
                } else {
                    theme::card()
                }
            })
            .border_1()
            .border_color(move || {
                if is_active.get() {
                    theme::primary()
                } else {
                    theme::border()
                }
            }),
        Elevation::Subtle,
    )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_start()
                .gap_3()
                .child(artwork_box(
                    cover_uri,
                    seed,
                    if form.is_phone() { 52.0 } else { 60.0 },
                    radius::CONTROL,
                ))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1()
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(when(
                                    move || is_active.get() && store.is_playing.get(),
                                    move || mini_equalizer(store.spectrum, store.is_playing),
                                ))
                                .child(
                                    text(title)
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .number_of_lines(2)
                                        .text_size(px(font_size::BODY))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(move || {
                                            if is_active.get() {
                                                theme::primary()
                                            } else {
                                                theme::fg()
                                            }
                                        }),
                                ),
                        )
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .flex_wrap()
                                .gap_2()
                                .children((!published.is_empty()).then(|| {
                                    text(published)
                                        .text_size(px(font_size::CAPTION))
                                        .text_color(theme::muted_fg)
                                }))
                                .child(
                                    view()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(theme::secondary)
                                        .child(
                                            text(duration)
                                                .tabular_nums()
                                                .text_size(px(font_size::MICRO))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme::fg),
                                        ),
                                ),
                        ),
                ),
        )
        .children(has_notes.then(|| {
            let notes_text = notes.clone();
            button()
                .on_press(move || expanded.update(|e| *e = !*e))
                .w_full()
                .min_h(px(form.min_touch_target()))
                .items_start()
                .justify_center()
                .child(
                    text(notes_text)
                        .number_of_lines(move || if expanded.get() { 16_u32 } else { 2_u32 })
                        .text_size(px(font_size::SUBTITLE))
                        .text_color(theme::muted_fg),
                )
        }))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    button()
                        .on_press(move || store.play_list((*queue_play).clone(), idx))
                        .min_h(px(form.min_touch_target()))
                        .px_3p5()
                        .py_1p5()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .rounded_full()
                        .bg(theme::primary)
                        .hover(|s| s.bg(theme::primary_hover))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(icon(Icon::Play, 14.0, colors::PRIMARY_FOREGROUND))
                        .child(
                            text(move || store.t("player.play"))
                                .text_size(px(font_size::SUBTITLE))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::primary_fg),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(heart_button(
                            move || store.is_liked(&ep_id_like),
                            move || store.t("player.like"),
                            move || store.toggle_like(ep_for_like.clone()),
                        ))
                        .child(icon_button_sized(
                            Icon::Queue,
                            34.0,
                            17.0,
                            theme::muted_fg,
                            move || store.t("player.add_to_queue"),
                            move || store.enqueue(ep_for_queue.clone()),
                        )),
                ),
        )
}
