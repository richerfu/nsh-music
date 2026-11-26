//! Podcast Shows directory and Podcast Show detail page aligned with
//! `egoist/mygo-music` (`ui_podcast.go`): `240px` show cards, `600px` detail
//! hero with clickable publisher link (`publisher_page`), persistent-owner
//! capability-gated Subscribe toggle, virtualized episode list (`ListViewport::Parent`)
//! with `120px` episode artwork, expandable show notes, full song `···` / context
//! menu, and shimmer loading skeletons.

use std::collections::HashSet;
use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::Track;

use crate::pages::{
    ARTWORK_SIZE_CARD, ARTWORK_SIZE_THUMB, PagesOwner, build_track_context_menu, card_grid,
    detail_hero_custom_subtitle, hero_play_shuffle_buttons, inline_nav_link, lazy_context_menu,
    lazy_more_menu_button, page_scroll, pagination_footer, refresh_pill_button, shimmer_card_grid,
    shimmer_detail_hero, shimmer_track_list, sized_artwork_url, sized_track_cover,
};
use crate::state::{AppStore, Route};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, artwork_box, empty_state, format_duration_ms,
    ghost_button, heart_button, icon, icon_button_sized, media_card, mini_equalizer, motion,
    section_header, theme,
};

/// Podcast Shows directory (`Route::Podcasts`).
pub fn shows_page(store: AppStore, form: DeviceForm) -> View {
    let tasks = PagesOwner::capture();

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
        .child(refresh_pill_button(
            store,
            form,
            store.browse_loading,
            move || store.refresh_home(),
        ));

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
                        .image_url(sized_artwork_url(&show.image_url, ARTWORK_SIZE_CARD))
                        .seed(show.id.clone())
                        .badge(badge)
                        .on_press(move || {
                            store.navigate(Route::ShowDetail { id: sid.clone() });
                        })
                        .on_play(move || {
                            let src = store.active_source();
                            let id = sid_play.clone();
                            tasks.spawn(async move {
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

    page_scroll(
        form,
        view()
            .w_full()
            .gap_6()
            .children((!form.is_phone()).then_some(header))
            .child(grid),
    )
}

/// Podcast Show detail page (`Route::ShowDetail`) showing show metadata,
/// clickable publisher link, Subscribe button, and virtualized episode cards.
pub fn show_detail_page(store: AppStore, form: DeviceForm, id: String) -> View {
    let tasks = PagesOwner::capture();
    // Per-episode expanded show-notes state stored outside recycled virtual_list cells.
    let expanded_ids = signal(HashSet::<String>::new());

    let body = view().w_full().child(dynamic(move || {
        let active = store.active_show.get();
        let target_id = id.clone();

        untracked(move || {
            let Some(page) = active.filter(|s| s.show.id == target_id) else {
                return view()
                    .w_full()
                    .gap_6()
                    .child(shimmer_detail_hero(form, false))
                    .child(shimmer_track_list(6))
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

            let subtitle_view = (!show.publisher.is_empty()).then(|| {
                if !show.publisher_page.is_empty() {
                    let pub_page = show.publisher_page.clone();
                    let pub_name = show.publisher.clone();
                    view().child(inline_nav_link(
                        pub_name.clone(),
                        font_size::BODY,
                        form,
                        move || {
                            store.navigate(Route::Browse {
                                page: pub_page.clone(),
                                title: pub_name.clone(),
                            });
                        },
                    ))
                } else {
                    view().child(
                        text(show.publisher.clone())
                            .number_of_lines(2)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::fg),
                    )
                }
            });

            let episodes_play = page.episodes.clone();
            let subscribed_now = show.subscribed;
            let show_id_sub = show.id.clone();
            let page_for_sub = page.clone();
            let can_subscribe = store.active_can("subscribeShow");

            let primary_actions = hero_play_shuffle_buttons(
                store,
                form,
                page.episodes.is_empty(),
                move || {
                    if !episodes_play.is_empty() {
                        store.play_list(episodes_play.clone(), 0);
                    }
                },
                None,
            );
            let min_h = px(form.min_touch_target());
            let sub_btn = can_subscribe.then(move || {
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
                        let updated_show = updated.show.clone();
                        store.active_show.set(Some(updated));
                        store.podcasts.update(|list| {
                            list.retain(|s| s.id != updated_show.id);
                            if next {
                                list.insert(0, updated_show.clone());
                            }
                        });
                        let src = store.active_source();
                        let sid = show_id_sub.clone();
                        tasks.spawn(async move {
                            let res = blocking(move || src.subscribe_show(&sid, next)).await;
                            if !matches!(res, Ok(Ok(()))) {
                                store.toasts.error(store.t("common.error"));
                            }
                        });
                        store.toasts.info(if next {
                            store.t("podcast.subscribed")
                        } else {
                            store.t("podcast.subscribe")
                        });
                    },
                )
                .min_h(min_h)
                .when(form.is_phone(), |b| b.rounded_full())
            });

            let actions = if form.is_phone() {
                view()
                    .w_full()
                    .gap_2p5()
                    .child(primary_actions)
                    .children(sub_btn.map(|btn| {
                        view()
                            .w_full()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .child(btn)
                    }))
            } else {
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_2p5()
                    .child(primary_actions)
                    .children(sub_btn)
            };

            let hero = detail_hero_custom_subtitle(
                store,
                form,
                store.t("nav.podcasts"),
                show.name.clone().into(),
                subtitle_view,
                meta,
                show.description.clone().into(),
                show.image_url.clone().into(),
                show.id.clone().into(),
                false,
                actions,
            );

            let fallback_show_cover = sized_artwork_url(&show.image_url, ARTWORK_SIZE_THUMB);

            let episodes_view = if page.episodes.is_empty() {
                empty_state(
                    Icon::Podcast,
                    store.t("common.empty"),
                    store.t("podcast.episodes"),
                )
            } else {
                let queue = Rc::new(page.episodes.clone());
                let total = queue.len();
                let keys: Vec<String> = queue
                    .iter()
                    .enumerate()
                    .map(|(i, ep)| format!("{i}:{}", ep.id))
                    .collect();

                let list = virtual_list(total, move |at: Position| {
                    episode_card_cell(
                        store,
                        form,
                        at,
                        Rc::clone(&queue),
                        fallback_show_cover.clone(),
                        expanded_ids,
                    )
                })
                .viewport(ListViewport::Parent)
                .keys(move || keys.clone())
                .estimated_item_size(px(148.0))
                .row_gap(px(12.0))
                .w_full();

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
                    .child(pagination_footer(
                        store,
                        form,
                        "show-load-more-btn",
                        move || {
                            store
                                .active_show
                                .with(|p| p.as_ref().is_some_and(|s| s.more))
                        },
                        move || store.show_episodes_loading_more.get(),
                        move || store.load_more_show_episodes(),
                    ))
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

fn episode_card_cell(
    store: AppStore,
    form: DeviceForm,
    at: Position,
    queue: Rc<Vec<Track>>,
    fallback_show_cover: SharedString,
    expanded_ids: Signal<HashSet<String>>,
) -> impl IntoElement {
    let q_cell = Rc::clone(&queue);
    let q_play = Rc::clone(&queue);
    let q_like = Rc::clone(&queue);
    let q_enq = Rc::clone(&queue);
    let q_menu_more = Rc::clone(&queue);
    let q_menu_ctx = Rc::clone(&queue);

    let ep_memo = computed(move || q_cell.get(at.index()).cloned());
    let is_active = computed(move || {
        let id = ep_memo.with(|e| e.as_ref().map(|t| t.id.clone()).unwrap_or_default());
        !id.is_empty()
            && store
                .current_track
                .with(|t| t.as_ref().is_some_and(|c| c.id == id))
    });
    let is_active_playing = computed(move || is_active.get() && store.is_playing.get());
    let is_liked = computed(move || {
        let id = ep_memo.with(|e| e.as_ref().map(|t| t.id.clone()).unwrap_or_default());
        !id.is_empty() && store.is_liked(&id)
    });
    let has_notes = computed(move || {
        ep_memo.with(|e| e.as_ref().is_some_and(|t| !t.description.trim().is_empty()))
    });
    let is_expanded = computed(move || {
        let id = ep_memo.with(|e| e.as_ref().map(|t| t.id.clone()).unwrap_or_default());
        !id.is_empty() && expanded_ids.with(|set| set.contains(&id))
    });

    let card = Elevated::elevation(
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
                move || {
                    let uri = ep_memo.with(|e| {
                        e.as_ref()
                            .map(|ep| sized_track_cover(ep, ARTWORK_SIZE_THUMB))
                            .unwrap_or_default()
                    });
                    if uri.is_empty() {
                        fallback_show_cover.clone()
                    } else {
                        uri
                    }
                },
                SharedString::from("podcast-ep"),
                if form.is_phone() { 54.0 } else { 60.0 },
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
                            .child(when(is_active_playing, move || {
                                mini_equalizer(store.spectrum, store.is_playing)
                            }))
                            .child(
                                text(move || {
                                    ep_memo.with(|e| {
                                        e.as_ref()
                                            .map(|ep| SharedString::from(ep.name.clone()))
                                            .unwrap_or_default()
                                    })
                                })
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
                            .child(when(
                                move || {
                                    ep_memo.with(|e| {
                                        e.as_ref().is_some_and(|ep| !ep.published.is_empty())
                                    })
                                },
                                move || {
                                    text(move || {
                                        ep_memo.with(|e| {
                                            e.as_ref()
                                                .map(|ep| SharedString::from(ep.published.clone()))
                                                .unwrap_or_default()
                                        })
                                    })
                                    .text_size(px(font_size::CAPTION))
                                    .text_color(theme::muted_fg)
                                },
                            ))
                            .child(
                                view()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(theme::secondary)
                                    .child(
                                        text(move || {
                                            ep_memo.with(|e| {
                                                e.as_ref()
                                                    .map(|ep| {
                                                        SharedString::from(format_duration_ms(
                                                            ep.duration,
                                                        ))
                                                    })
                                                    .unwrap_or_default()
                                            })
                                        })
                                        .tabular_nums()
                                        .text_size(px(font_size::MICRO))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::fg),
                                    ),
                            ),
                    ),
            ),
    )
    .child(when(has_notes, move || {
        button()
            .on_press(move || {
                let id = ep_memo
                    .with_untracked(|e| e.as_ref().map(|ep| ep.id.clone()).unwrap_or_default());
                if !id.is_empty() {
                    expanded_ids.update(|set| {
                        if !set.remove(&id) {
                            set.insert(id);
                        }
                    });
                }
            })
            .w_full()
            .min_h(px(form.min_touch_target()))
            .items_start()
            .justify_center()
            .child(
                text(move || {
                    ep_memo.with(|e| {
                        e.as_ref()
                            .map(|ep| SharedString::from(ep.description.clone()))
                            .unwrap_or_default()
                    })
                })
                .number_of_lines(move || if is_expanded.get() { 16_u32 } else { 2_u32 })
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
                    .on_press(move || {
                        let idx = at.index();
                        if idx < q_play.len() {
                            store.play_list((*q_play).clone(), idx);
                        }
                    })
                    .min_h(px(form.min_touch_target()))
                    .px_4()
                    .py_1p5()
                    .flex_row()
                    .items_center()
                    .justify_center()
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
                    .gap(px(if form.is_phone() { 6.0 } else { 4.0 }))
                    .child(heart_button(
                        is_liked,
                        move || store.t("player.like"),
                        move || {
                            if let Some(ep) = q_like.get(at.index()).cloned() {
                                store.toggle_like(ep);
                            }
                        },
                    ))
                    .child(icon_button_sized(
                        Icon::Queue,
                        34.0,
                        17.0,
                        theme::muted_fg,
                        move || store.t("player.add_to_queue"),
                        move || {
                            if let Some(ep) = q_enq.get(at.index()).cloned() {
                                store.enqueue(ep);
                            }
                        },
                    ))
                    .child(lazy_more_menu_button(
                        form,
                        move || store.t("common.more"),
                        move || {
                            let idx = at.index();
                            let Some(ep) = q_menu_more.get(idx).cloned() else {
                                return Vec::new();
                            };
                            build_track_context_menu(store, ep, Rc::clone(&q_menu_more), idx, None)
                        },
                    )),
            ),
    );

    lazy_context_menu(card, move || {
        let idx = at.index();
        let Some(ep) = q_menu_ctx.get(idx).cloned() else {
            return Vec::new();
        };
        build_track_context_menu(store, ep, Rc::clone(&q_menu_ctx), idx, None)
    })
}
