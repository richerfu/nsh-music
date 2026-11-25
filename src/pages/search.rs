//! Search page aligned with `mygo-music`: wired to `store.update_search_query`
//! & `store.search`, live `store.search_suggestions`, real `store.recent_searches`,
//! segmented tabs (`Tracks`, `Albums`, `Artists`, `Playlists`, `Podcasts`), and
//! shimmer loading placeholders (zero hardcoded mock suggestions).

use std::rc::Rc;
use std::time::Duration;

use nsh_base::{tab, tabs};
use nsh_core::prelude::*;
use nsh_music_core::model::SearchKind;

use crate::pages::{
    card_grid, page_scroll, render_track_row, shimmer_card_grid, shimmer_track_list,
    track_table_header,
};
use crate::state::{AppStore, Route};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, empty_state, ghost_button, icon, icon_button_sized,
    media_card, pill_button, section_header, styled_input, theme,
};
use crate::ui::theme::{colors, font_size, radius};

const SEARCH_TABS: &[(SearchKind, &str)] = &[
    (SearchKind::Tracks, "search.tab.tracks"),
    (SearchKind::Albums, "search.tab.albums"),
    (SearchKind::Artists, "search.tab.artists"),
    (SearchKind::Playlists, "search.tab.playlists"),
    (SearchKind::Shows, "search.tab.shows"),
];

pub fn search_page(store: AppStore, form: DeviceForm) -> View {
    // Keep a local draft signal so CJK composition is never overwritten mid-edit,
    // while keeping two-way sync with external search inputs (e.g. desktop sidebar).
    let initial = store.search_query.get_untracked();
    let draft = signal(initial.clone());
    let last_synced = stored(initial);

    // User typing in draft -> live `store.update_search_query` + debounced search.
    effect(move || {
        let q = draft.get();
        if last_synced.with(|prev| prev == &q) {
            return;
        }
        last_synced.replace(q.clone());
        store.update_search_query(q.clone());

        if q.trim().is_empty() {
            store.search("");
            return;
        }

        spawn(async move {
            timer(Duration::from_millis(260)).await;
            if draft.with_untracked(|cur| cur == &q) {
                store.search(&q);
            }
        });
    });

    // External update to `store.search_query` -> mirror into draft cleanly.
    effect(move || {
        let external = store.search_query.get();
        if last_synced.with(|prev| prev != &external) {
            last_synced.replace(external.clone());
            draft.set(external);
        }
    });

    let search_bar = view()
        .w_full()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .rounded(px(radius::CONTROL))
                .bg(theme::card)
                .border_1()
                .border_color(theme::input_border)
                .focus_within(|s| s.border_color(theme::ring))
                .child(icon(Icon::Search, 17.0, colors::MUTED_FOREGROUND))
                .child(
                    styled_input(draft, move || store.t("search.placeholder"))
                        .flex_1()
                        .min_w(px(0.0))
                        .border_color(colors::TRANSPARENT)
                        .bg(colors::TRANSPARENT)
                        .px_0()
                        .return_key_type(ReturnKeyType::Search)
                        .on_submit(move |val: &str| {
                            dismiss_keyboard();
                            last_synced.replace(val.to_owned());
                            store.search(val);
                        }),
                )
                .child(when(
                    move || draft.with(|s| !s.is_empty()),
                    move || {
                        icon_button_sized(
                            Icon::Close,
                            28.0,
                            14.0,
                            theme::muted_fg,
                            move || store.t("common.close"),
                            move || {
                                last_synced.replace(String::new());
                                draft.set(String::new());
                                store.update_search_query(String::new());
                                store.search("");
                            },
                        )
                    },
                )),
        );

    // Live source-provided search suggestions strip (shown whenever active source returns hits)
    let live_suggestions_bar = view().w_full().child(dynamic(move || {
        let suggestions = store.search_suggestions.get();
        untracked(move || {
            if suggestions.is_empty() {
                return view().into_element();
            }
            let mut pills = view().w_full().flex_row().flex_wrap().gap_2();
            for term in suggestions {
                let term_press = term.clone();
                pills = pills.child(pill_button(term, false, move || {
                    last_synced.replace(term_press.clone());
                    draft.set(term_press.clone());
                    store.search(&term_press);
                }));
            }
            view()
                .w_full()
                .gap_2()
                .child(
                    text(move || store.t("search.suggestions"))
                        .text_size(px(font_size::CAPTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::muted_fg),
                )
                .child(pills)
                .into_element()
        })
    }));

    let tab_bar = scroll().horizontal().scrollbar(false).child(
        tabs()
            .flex_row()
            .self_start()
            .gap_1()
            .p_1()
            .rounded(px(radius::CONTROL))
            .bg(theme::muted)
            .indicator_style(|s| {
                s.top_1()
                    .bottom_1()
                    .rounded(px(radius::SM))
                    .bg(theme::card_elevated)
                    .elevation(Elevation::Subtle)
            })
            .children(SEARCH_TABS.iter().map(|&(kind, key)| {
                let is_sel = move || store.search_tab.get() == kind;
                tab()
                    .selected(is_sel)
                    .on_press(move |_: &()| store.set_search_tab(kind))
                    .min_h(px(if form.is_touch() { 44.0 } else { 32.0 }))
                    .px_3p5()
                    .py_1p5()
                    .items_center()
                    .justify_center()
                    .child(
                        text(move || store.t(key))
                            .number_of_lines(1)
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || {
                                if is_sel() {
                                    theme::fg()
                                } else {
                                    theme::muted_fg()
                                }
                            }),
                    )
            })),
    );

    let results_region = view().w_full().child(dynamic(move || {
        let query = store.search_query.get();
        let tab_kind = store.search_tab.get();
        let loading = store.search_loading.get();
        let results = store.search_results.get();
        let recent = store.recent_searches.get();

        untracked(move || {
            if query.trim().is_empty() {
                return idle_search_view(store, draft, last_synced, recent).into_element();
            }

            if loading {
                return match tab_kind {
                    SearchKind::Tracks => shimmer_track_list(6).into_element(),
                    SearchKind::Artists => shimmer_card_grid(store, form, 6, true).into_element(),
                    _ => shimmer_card_grid(store, form, 6, false).into_element(),
                };
            }

            match tab_kind {
                SearchKind::Tracks => {
                    if results.tracks.is_empty() {
                        return empty_state(Icon::Search, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let queue = Rc::new(results.tracks.clone());
                    let mut rows = view().w_full().gap_1();
                    if !form.is_phone() {
                        rows = rows.child(track_table_header(store, form, true));
                    }
                    for (idx, tr) in results.tracks.iter().enumerate() {
                        rows = rows.child(render_track_row(
                            store,
                            tr.clone(),
                            Some(idx),
                            Rc::clone(&queue),
                            idx,
                            true,
                        ));
                    }
                    view()
                        .w_full()
                        .p_2()
                        .rounded(px(radius::CARD))
                        .bg(theme::card)
                        .border_1()
                        .border_color(theme::border)
                        .child(rows)
                        .into_element()
                }
                SearchKind::Albums => {
                    if results.albums.is_empty() {
                        return empty_state(Icon::Disc, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let cards: Vec<AnyElement> = results
                        .albums
                        .iter()
                        .map(|alb| {
                            let aid = alb.id.clone();
                            let aid_play = alb.id.clone();
                            let artists: String = alb
                                .artists
                                .iter()
                                .map(|a| a.name.as_str())
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let sub = if artists.is_empty() {
                                store.t("track.unknown_artist").to_string()
                            } else {
                                artists
                            };
                            media_card(alb.name.clone(), sub)
                                .image_url(alb.image_url.clone())
                                .seed(alb.id.clone())
                                .on_press(move || {
                                    store.navigate(Route::AlbumDetail { id: aid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = aid_play.clone();
                                    spawn(async move {
                                        if let Ok(Ok(page)) = blocking(move || src.album(&id)).await
                                            && !page.tracks.is_empty()
                                        {
                                            store.play_list(page.tracks, 0);
                                        }
                                    });
                                })
                                .into_element()
                        })
                        .collect();
                    card_grid(store, form, cards).into_element()
                }
                SearchKind::Artists => {
                    if results.artists.is_empty() {
                        return empty_state(Icon::User, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let cards: Vec<AnyElement> = results
                        .artists
                        .iter()
                        .map(|art| {
                            let aid = art.id.clone();
                            let aid_play = art.id.clone();
                            media_card(art.name.clone(), store.t("nav.artists"))
                                .image_url(art.image_url.clone())
                                .seed(art.id.clone())
                                .circular(true)
                                .on_press(move || {
                                    store.navigate(Route::ArtistDetail { id: aid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = aid_play.clone();
                                    spawn(async move {
                                        if let Ok(Ok(page)) =
                                            blocking(move || src.artist(&id)).await
                                            && !page.tracks.is_empty()
                                        {
                                            store.play_list(page.tracks, 0);
                                        }
                                    });
                                })
                                .into_element()
                        })
                        .collect();
                    card_grid(store, form, cards).into_element()
                }
                SearchKind::Playlists => {
                    if results.playlists.is_empty() {
                        return empty_state(Icon::Library, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let cards: Vec<AnyElement> = results
                        .playlists
                        .iter()
                        .map(|pl| {
                            let pid = pl.id.clone();
                            let pid_play = pl.id.clone();
                            let tracks = pl.tracks.clone();
                            let count = if pl.tracks.is_empty() {
                                pl.track_count
                            } else {
                                pl.tracks.len()
                            };
                            let sub = if !pl.description.is_empty() {
                                pl.description.clone()
                            } else if count > 0 {
                                store
                                    .t_fmt("track.count_fmt", &[("count", &count.to_string())])
                                    .to_string()
                            } else {
                                pl.owner.clone()
                            };
                            media_card(pl.name.clone(), sub)
                                .image_url(pl.image_url.clone())
                                .seed(pl.id.clone())
                                .on_press(move || {
                                    store.navigate(Route::PlaylistDetail { id: pid.clone() });
                                })
                                .on_play(move || {
                                    if !tracks.is_empty() {
                                        store.play_list(tracks.clone(), 0);
                                    } else {
                                        let src = store.active_source();
                                        let id = pid_play.clone();
                                        spawn(async move {
                                            if let Ok(Ok(full)) =
                                                blocking(move || src.playlist(&id)).await
                                                && !full.tracks.is_empty()
                                            {
                                                store.play_list(full.tracks, 0);
                                            }
                                        });
                                    }
                                })
                                .into_element()
                        })
                        .collect();
                    card_grid(store, form, cards).into_element()
                }
                SearchKind::Shows => {
                    if results.shows.is_empty() {
                        return empty_state(Icon::Podcast, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let cards: Vec<AnyElement> = results
                        .shows
                        .iter()
                        .map(|show| {
                            let sid = show.id.clone();
                            let sid_play = show.id.clone();
                            media_card(show.name.clone(), show.publisher.clone())
                                .image_url(show.image_url.clone())
                                .seed(show.id.clone())
                                .badge(store.t("nav.podcasts"))
                                .on_press(move || {
                                    store.navigate(Route::ShowDetail { id: sid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = sid_play.clone();
                                    spawn(async move {
                                        if let Ok(Ok(page)) =
                                            blocking(move || src.show(&id, 0)).await
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
                }
            }
        })
    }));

    let body = view()
        .w_full()
        .gap_5()
        .child(
            text(move || store.t("nav.search"))
                .role(AccessibilityRole::Header)
                .text_size(px(if form.is_phone() {
                    font_size::TITLE_LG
                } else {
                    font_size::DISPLAY
                }))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg),
        )
        .child(search_bar)
        .child(live_suggestions_bar)
        .child(tab_bar)
        .child(results_region);

    page_scroll(form, body)
}

fn idle_search_view(
    store: AppStore,
    draft: Signal<String>,
    last_synced: Stored<String>,
    recent: Vec<String>,
) -> View {
    let mut stack = view().w_full().gap_5();

    if !recent.is_empty() {
        let mut pills = view().w_full().flex_row().flex_wrap().gap_2();
        for term in recent {
            let term_press = term.clone();
            pills = pills.child(pill_button(term, false, move || {
                last_synced.replace(term_press.clone());
                draft.set(term_press.clone());
                store.search(&term_press);
            }));
        }

        stack = stack.child(
            view()
                .w_full()
                .gap_3()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(section_header(
                            move || store.t("search.recent"),
                            None,
                            None,
                            None,
                        ))
                        .child(ghost_button(
                            move || store.t("search.clear_recent"),
                            move || store.clear_recent_searches(),
                        )),
                )
                .child(pills),
        );
    }

    stack.child(empty_state(
        Icon::Search,
        store.t("nav.search"),
        store.t("search.placeholder"),
    ))
}
