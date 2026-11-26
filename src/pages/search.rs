//! Search page aligned 1:1 with `egoist/mygo-music` (`ui_search.go`):
//! - Crisp single-border search field (borderless `nsh_base::input()` inside a
//!   single outer focus-ring container — zero double nested borders)
//! - Compact single-row Desktop results header (`“{query}”` + `搜索 · N 首歌曲`
//!   and sleek inline input) keeping segmented tabs & track table immediately
//!   above the fold (suppressing inline suggestion pill wall on Desktop while
//!   results show; sidebar popover handles Desktop suggestions)
//! - CJK composition-safe input (`on_composition_change`), capability-gated
//!   tabs, virtualized track table (`ListViewport::Parent`), `240px` artwork,
//!   and shimmer loading placeholders.

use std::rc::Rc;
use std::time::Duration;

use nsh_base::{input, tab, tabs};
use nsh_core::prelude::*;
use nsh_music_core::model::SearchKind;

use crate::pages::{
    ARTWORK_SIZE_CARD, PagesOwner, card_grid, page_scroll, pagination_footer, shimmer_card_grid,
    shimmer_track_list, sized_artwork_url, virtual_track_list,
};
use crate::state::{AppStore, Route};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    DeviceForm, Elevated, Elevation, Icon, empty_state, ghost_button, icon, icon_button_sized,
    loading_spinner, media_card, pill_button, section_header, theme,
};

const SEARCH_TABS: &[(SearchKind, &str, &str)] = &[
    (SearchKind::Tracks, "search.tab.tracks", ""),
    (SearchKind::Artists, "search.tab.artists", "artist"),
    (SearchKind::Albums, "search.tab.albums", "album"),
    (SearchKind::Playlists, "search.tab.playlists", "playlist"),
    (SearchKind::Shows, "search.tab.shows", "show"),
];

pub fn search_page(store: AppStore, form: DeviceForm) -> View {
    let tasks = PagesOwner::capture();

    // Keep a local draft signal + IME composition flag so CJK pinyin composition
    // is never interrupted or prematurely searched mid-edit.
    let initial = store.search_query.get_untracked();
    let draft = signal(initial.clone());
    let composing = signal(false);
    let last_synced = stored(initial);

    // User typing in draft -> live `store.update_search_query` + debounced search.
    effect(move || {
        let q = draft.get();
        if composing.get() {
            return;
        }
        if last_synced.with(|prev| prev == &q) {
            return;
        }
        last_synced.replace(q.clone());
        store.update_search_query(q.clone());

        if q.trim().is_empty() {
            store.search("");
            return;
        }

        store.search_loading.set(true);
        spawn(async move {
            timer(Duration::from_millis(260)).await;
            if !composing.get_untracked() && draft.with_untracked(|cur| cur == &q) {
                store.search(&q);
            }
        });
    });

    // External update to `store.search_query` -> mirror into draft cleanly.
    effect(move || {
        let external = store.search_query.get();
        if !composing.get_untracked() && last_synced.with(|prev| prev != &external) {
            last_synced.replace(external.clone());
            draft.set(external);
        }
    });

    let has_query = computed(move || store.search_query.with(|q| !q.trim().is_empty()));

    let build_search_field = move |adaptive_desktop: bool| {
        let is_phone = form.is_phone();
        let field_h = if form.is_touch() { 44.0 } else { 38.0 };
        let side_pad = if is_phone { 14.0 } else { 12.0 };
        let input_ref = element_ref();

        view()
            .w_full()
            .when(adaptive_desktop, |v| {
                v.max_w(move || px(if has_query.get() { 300.0 } else { 420.0 }))
            })
            .h(px(field_h))
            .flex_row()
            .items_center()
            .rounded(px(if is_phone {
                radius::PILL
            } else {
                radius::CONTROL
            }))
            .bg(if is_phone {
                theme::secondary()
            } else {
                theme::card()
            })
            .border_1()
            .border_color(if is_phone {
                theme::border()
            } else {
                theme::input_border()
            })
            .focus_within(|s| s.border_color(theme::ring))
            .child(
                view()
                    .h_full()
                    .pl(px(side_pad))
                    .pr_2()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .flex_shrink_0()
                    .on_press(move || input_ref.focus())
                    .child(
                        when(
                            move || store.search_loading.get(),
                            || loading_spinner(15.0, colors::PRIMARY),
                        )
                        .otherwise(|| icon(Icon::Search, 16.0, colors::MUTED_FOREGROUND)),
                    ),
            )
            .child(
                input()
                    .test_id("search-page-input")
                    .element_ref(&input_ref)
                    .value(move || draft.get())
                    .on_change(move |val: &str| draft.set(val.to_owned()))
                    .placeholder(move || store.t("search.placeholder").to_string())
                    .placeholder_color(|| color(theme::muted_fg()))
                    .caret_color(|| color(theme::primary()))
                    .selection_color(|| color(theme::primary_soft()))
                    .return_key_type(ReturnKeyType::Search)
                    .on_composition_change(move |c: &Composition| {
                        composing.set(c.composing);
                    })
                    .on_submit(move |val: &str| {
                        dismiss_keyboard();
                        composing.set(false);
                        let query = if !val.trim().is_empty() {
                            let owned = val.to_owned();
                            last_synced.replace(owned.clone());
                            draft.set(owned.clone());
                            owned
                        } else {
                            let cur = draft.get_untracked();
                            last_synced.replace(cur.clone());
                            cur
                        };
                        store.update_search_query(query.clone());
                        store.search(&query);
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .bg(colors::TRANSPARENT)
                    .text_size(px(if adaptive_desktop {
                        font_size::SUBTITLE
                    } else {
                        font_size::BODY
                    }))
                    .text_color(theme::fg),
            )
            .child(
                when(
                    move || draft.with(|s| !s.is_empty()),
                    move || {
                        view()
                            .h_full()
                            .pl_1()
                            .pr(px(if is_phone { 8.0 } else { 6.0 }))
                            .flex_row()
                            .items_center()
                            .flex_shrink_0()
                            .child(icon_button_sized(
                                Icon::Close,
                                if is_phone { 28.0 } else { 26.0 },
                                if is_phone { 14.0 } else { 13.0 },
                                theme::muted_fg,
                                move || store.t("common.close"),
                                move || {
                                    composing.set(false);
                                    last_synced.replace(String::new());
                                    draft.set(String::new());
                                    store.update_search_query(String::new());
                                    store.search("");
                                },
                            ))
                    },
                )
                .otherwise(move || {
                    view()
                        .h_full()
                        .w(px(side_pad))
                        .flex_shrink_0()
                        .on_press(move || input_ref.focus())
                }),
            )
    };

    // Adaptive top header matching `ui_search.go`:
    // Mount `search-page-input` ONCE outside any `dynamic(store.search_query)` so
    // typing never recreates, unmounts, or unfocuses the native text input.
    let header_block = if form.is_desktop() {
        let subtitle = move || {
            let base = store.t("nav.search");
            let loading = store.search_loading.get();
            let kind = store.search_tab.get();
            if !loading && kind == SearchKind::Tracks {
                let count = store.search_results.with(|r| r.tracks.len());
                if count > 0 {
                    let c_str = count.to_string();
                    let songs = store.t_fmt("track.count_fmt", &[("count", &c_str)]);
                    return SharedString::from(format!("{base} · {songs}"));
                }
            }
            base
        };

        view()
            .w_full()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                view()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap_0p5()
                    .child(when(
                        move || has_query.get(),
                        move || {
                            text(subtitle)
                                .text_size(px(font_size::CAPTION))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::muted_fg)
                        },
                    ))
                    .child(
                        text(move || {
                            let q = store.search_query.get();
                            let trimmed = q.trim();
                            if trimmed.is_empty() {
                                store.t("nav.search")
                            } else {
                                SharedString::from(format!("“{trimmed}”"))
                            }
                        })
                        .role(AccessibilityRole::Header)
                        .number_of_lines(1)
                        .text_size(move || {
                            px(if has_query.get() {
                                font_size::TITLE_LG
                            } else {
                                font_size::DISPLAY
                            })
                        })
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::fg),
                    ),
            )
            .child(build_search_field(true))
    } else {
        view()
            .w_full()
            .gap_3()
            .children((!form.is_phone()).then(|| {
                text(move || store.t("nav.search"))
                    .role(AccessibilityRole::Header)
                    .text_size(px(font_size::DISPLAY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::fg)
            }))
            .child(build_search_field(false))
    };

    // Live source suggestions:
    // - Suppressed on Desktop when results are showing (sidebar combobox handles Desktop suggestions).
    // - Shown cleanly on Phone/Pad only while actively editing (`search_suggest_open`).
    let live_suggestions_bar = view().w_full().child(dynamic(move || {
        let suggest_open = store.search_suggest_open.get();
        let suggestions = store.search_suggestions.get();
        let has_query = store.search_query.with(|q| !q.trim().is_empty());
        let is_desktop = form.is_desktop();

        untracked(move || {
            if suggestions.is_empty() || (is_desktop && has_query) || (!is_desktop && !suggest_open)
            {
                return view().into_element();
            }
            let mut pills = view()
                .w_full()
                .flex_row()
                .flex_wrap()
                .gap(px(if form.is_phone() { 6.0 } else { 8.0 }));
            for term in suggestions.into_iter().take(8) {
                let term_press = term.clone();
                pills = pills.child(pill_button(term, false, move || {
                    composing.set(false);
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

    // Capability-gated search kind tabs (`ui_search.go:14-24`), shown once a query is active.
    let tab_bar = view().w_full().child(dynamic(move || {
        let _source = store.active_source_id.get();
        let _sources = store.sources.get();
        let has_query = store.search_query.with(|q| !q.trim().is_empty());
        untracked(move || {
            let visible_tabs: Vec<(SearchKind, &'static str)> = SEARCH_TABS
                .iter()
                .filter(|&&(_, _, cap)| cap.is_empty() || store.active_can(cap))
                .map(|&(kind, key, _)| (kind, key))
                .collect();

            let active_kind = store.search_tab.get_untracked();
            if !visible_tabs.iter().any(|(k, _)| *k == active_kind) {
                store.set_search_tab(SearchKind::Tracks);
            }

            if visible_tabs.len() < 2 || !has_query {
                return view().into_element();
            }

            let is_phone = form.is_phone();
            scroll()
                .horizontal()
                .scrollbar(false)
                .child(
                    tabs()
                        .flex_row()
                        .self_start()
                        .gap_1()
                        .p_1()
                        .rounded(px(if is_phone {
                            radius::PILL
                        } else {
                            radius::CONTROL
                        }))
                        .bg(theme::muted)
                        .indicator_style(move |s| {
                            s.top_1()
                                .bottom_1()
                                .rounded(px(if is_phone { radius::PILL } else { radius::SM }))
                                .bg(theme::card_elevated)
                                .elevation(Elevation::Subtle)
                        })
                        .children(visible_tabs.into_iter().map(|(kind, key)| {
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
                )
                .into_element()
        })
    }));

    let results_region = view().w_full().child(dynamic(move || {
        let query = store.search_query.get();
        let tab_kind = store.search_tab.get();
        let loading = store.search_loading.get();
        let results = store.search_results.get();
        let recent = store.recent_searches.get();

        untracked(move || {
            if query.trim().is_empty() {
                return idle_search_view(store, form, draft, composing, last_synced, recent)
                    .into_element();
            }

            if loading {
                return match tab_kind {
                    SearchKind::Tracks => shimmer_track_list(6).into_element(),
                    SearchKind::Artists => shimmer_card_grid(store, form, 6, true).into_element(),
                    _ => shimmer_card_grid(store, form, 6, false).into_element(),
                };
            }

            let results_view: AnyElement = match tab_kind {
                SearchKind::Tracks => {
                    if results.tracks.is_empty() {
                        return empty_state(Icon::Search, store.t("search.no_results"), query)
                            .into_element();
                    }
                    let queue = Rc::new(results.tracks);
                    virtual_track_list(store, form, queue, true, true, None, None).into_element()
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
                                .image_url(sized_artwork_url(&alb.image_url, ARTWORK_SIZE_CARD))
                                .seed(alb.id.clone())
                                .on_press(move || {
                                    store.navigate(Route::AlbumDetail { id: aid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = aid_play.clone();
                                    tasks.spawn(async move {
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
                                .image_url(sized_artwork_url(&art.image_url, ARTWORK_SIZE_CARD))
                                .seed(art.id.clone())
                                .circular(true)
                                .on_press(move || {
                                    store.navigate(Route::ArtistDetail { id: aid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = aid_play.clone();
                                    tasks.spawn(async move {
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
                                .image_url(sized_artwork_url(&pl.image_url, ARTWORK_SIZE_CARD))
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
                                        tasks.spawn(async move {
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
                                .image_url(sized_artwork_url(&show.image_url, ARTWORK_SIZE_CARD))
                                .seed(show.id.clone())
                                .badge(store.t("nav.podcasts"))
                                .on_press(move || {
                                    store.navigate(Route::ShowDetail { id: sid.clone() });
                                })
                                .on_play(move || {
                                    let src = store.active_source();
                                    let id = sid_play.clone();
                                    tasks.spawn(async move {
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
            };

            view()
                .w_full()
                .gap_3()
                .child(results_view)
                .child(pagination_footer(
                    store,
                    form,
                    "search-load-more-btn",
                    move || store.search_results.with(|r| r.more),
                    move || store.search_loading_more.get(),
                    move || store.load_more_search(),
                ))
                .into_element()
        })
    }));

    let body = view()
        .w_full()
        .gap_4()
        .child(header_block)
        .child(live_suggestions_bar)
        .child(tab_bar)
        .child(results_region);

    page_scroll(form, body)
}

fn idle_search_view(
    store: AppStore,
    form: DeviceForm,
    draft: Signal<String>,
    composing: Signal<bool>,
    last_synced: Stored<String>,
    recent: Vec<String>,
) -> View {
    let is_phone = form.is_phone();
    let mut stack = view().w_full().gap(px(if is_phone { 16.0 } else { 20.0 }));

    if !recent.is_empty() {
        let mut pills =
            view()
                .w_full()
                .flex_row()
                .flex_wrap()
                .gap(px(if is_phone { 6.0 } else { 8.0 }));
        for term in recent {
            let term_press = term.clone();
            pills = pills.child(pill_button(term, false, move || {
                composing.set(false);
                last_synced.replace(term_press.clone());
                draft.set(term_press.clone());
                store.search(&term_press);
            }));
        }

        stack = stack.child(
            view()
                .w_full()
                .gap_2p5()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(view().flex_1().min_w(px(0.0)).child(section_header(
                            move || store.t("search.recent"),
                            None,
                            None,
                            None,
                        )))
                        .child(
                            ghost_button(
                                move || store.t("search.clear_recent"),
                                move || store.clear_recent_searches(),
                            )
                            .min_h(px(form.min_touch_target())),
                        ),
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
