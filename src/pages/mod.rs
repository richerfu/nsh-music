//! Adaptive pages and modal hosts for `nsh-music` across Desktop, Pad, and Phone.

pub mod album;
pub mod artist;
pub mod browse;
pub mod home;
pub mod liked;
pub mod now_playing;
pub mod playlist;
pub mod search;
pub mod settings;
pub mod shows;
pub mod stations;

use std::rc::Rc;

use nsh_base::button;
use nsh_core::prelude::*;
use nsh_music_core::model::{Section, Track};

use crate::state::{ActiveModal, AppStore, ArtworkLoader, Route};
use crate::ui::{
    ContextMenuAction, DeviceForm, Elevated, Elevation, Icon, adaptive_modal, artwork_box,
    compat_badge_text, empty_state, ghost_button, icon, icon_button_sized, media_card, motion,
    primary_button, section_header, shimmer_block, skeleton_card, styled_context_menu,
    styled_input, theme, track_row,
};
use crate::ui::theme::{colors, font_size, radius};

/// Context for a track rendered inside an editable user playlist.
#[derive(Debug, Clone)]
pub(crate) struct PlaylistTrackContext {
    pub playlist_id: String,
    pub index: usize,
    pub total: usize,
}

/// Renders the currently active route from `store.route` adapted to `form`.
pub fn render_route(store: AppStore, form: DeviceForm) -> View {
    view().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(dynamic(move || {
        let route = store.route.get();
        untracked(move || {
            store.page_scrolled.set(false);
            let page_view = match route {
                Route::Home => home::home_page(store, form),
                Route::Browse { page, title } => browse::browse_page(store, form, page, title),
                Route::Search => search::search_page(store, form),
                Route::Library => liked::library_page(store, form),
                Route::Liked => liked::liked_page(store, form),
                Route::Playlists => liked::playlists_page(store, form),
                Route::PlaylistDetail { id } => playlist::playlist_page(store, form, id),
                Route::AlbumDetail { id } => album::album_page(store, form, id),
                Route::ArtistDetail { id } => artist::artist_page(store, form, id),
                Route::Radio => stations::stations_page(store, form),
                Route::Podcasts => shows::shows_page(store, form),
                Route::ShowDetail { id } => shows::show_detail_page(store, form, id),
                Route::Recent => liked::recent_page(store, form),
                Route::Local => browse::local_library_page(store, form),
                Route::Settings => settings::settings_page(store, form, false),
                Route::Plugins => settings::settings_page(store, form, true),
            };
            page_view.into_element()
        })
    }))
}

/// Renders the active app-level modal (`store.active_modal`) inside `adaptive_modal`.
/// Mount once at the root shell (`src/shell/device.rs`).
pub fn render_active_modal(store: AppStore) -> View {
    let is_open = computed(move || store.active_modal.with(|m| !matches!(m, ActiveModal::None)));
    let modal_title = computed(move || {
        let modal = store.active_modal.get();
        match modal {
            ActiveModal::None => SharedString::default(),
            ActiveModal::InstallPlugin => store.t("dialog.install_plugin.title"),
            ActiveModal::ConfirmPluginInstall { .. } => store.t("plugins.confirm_install.title"),
            ActiveModal::PluginConfig { .. } => store.t("dialog.plugin_config.title"),
            ActiveModal::QrSignIn { .. } => store.t("dialog.qr_sign_in.title"),
            ActiveModal::WebSignIn { title, .. } => {
                if title.is_empty() {
                    store.t("dialog.web_sign_in.title")
                } else {
                    title.into()
                }
            }
            ActiveModal::CreatePlaylist { .. } => store.t("dialog.create_playlist.title"),
            ActiveModal::RenamePlaylist { .. } => store.t("dialog.rename_playlist.title"),
            ActiveModal::SourceSwitcher => store.t("dialog.source_switcher.title"),
        }
    });

    adaptive_modal(
        is_open,
        modal_title,
        move || store.close_modal(),
        move || {
            view().w_full().child(dynamic(move || {
                let modal = store.active_modal.get();
                untracked(move || match modal {
                    ActiveModal::None => view().into_element(),
                    ActiveModal::InstallPlugin => settings::install_plugin_modal(store).into_element(),
                    ActiveModal::ConfirmPluginInstall { pending } => {
                        settings::confirm_plugin_install_modal(store, pending).into_element()
                    }
                    ActiveModal::PluginConfig { plugin_id } => {
                        settings::plugin_config_modal(store, plugin_id).into_element()
                    }
                    ActiveModal::QrSignIn { source_id, prompt } => {
                        settings::qr_sign_in_modal(store, source_id, prompt).into_element()
                    }
                    ActiveModal::WebSignIn {
                        source_id,
                        url,
                        title,
                    } => settings::web_sign_in_modal(store, source_id, url, title).into_element(),
                    ActiveModal::CreatePlaylist { initial_track_id } => {
                        create_playlist_modal(store, initial_track_id).into_element()
                    }
                    ActiveModal::RenamePlaylist { id, current_name } => {
                        rename_playlist_modal(store, id, current_name).into_element()
                    }
                    ActiveModal::SourceSwitcher => source_switcher_modal(store).into_element(),
                })
            }))
        },
    )
}

fn create_playlist_modal(store: AppStore, initial_track_id: Option<String>) -> View {
    let name = signal(String::new());
    let submit = move || {
        let trimmed = name.with_untracked(|s| s.trim().to_owned());
        if !trimmed.is_empty() {
            dismiss_keyboard();
            store.create_playlist_and_add(&trimmed, initial_track_id.clone());
        }
    };
    let submit_enter = submit.clone();

    view()
        .w_full()
        .gap_4()
        .child(
            styled_input(name, move || store.t("dialog.create_playlist.placeholder"))
                .w_full()
                .auto_focus(true)
                .return_key_type(ReturnKeyType::Done)
                .on_submit(move |_: &str| submit_enter()),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(ghost_button(move || store.t("common.cancel"), move || {
                    store.close_modal();
                }))
                .child(
                    primary_button(move || store.t("common.create"), submit)
                        .disabled(move || name.with(|s| s.trim().is_empty())),
                ),
        )
}

fn rename_playlist_modal(store: AppStore, id: String, current_name: String) -> View {
    let name = signal(current_name);
    let submit = move || {
        let trimmed = name.with_untracked(|s| s.trim().to_owned());
        if !trimmed.is_empty() {
            dismiss_keyboard();
            store.rename_playlist(&id, &trimmed);
        }
    };
    let submit_enter = submit.clone();

    view()
        .w_full()
        .gap_4()
        .child(
            styled_input(name, move || store.t("dialog.create_playlist.placeholder"))
                .w_full()
                .auto_focus(true)
                .return_key_type(ReturnKeyType::Done)
                .on_submit(move |_: &str| submit_enter()),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(ghost_button(move || store.t("common.cancel"), move || {
                    store.close_modal();
                }))
                .child(
                    primary_button(move || store.t("common.save"), submit)
                        .disabled(move || name.with(|s| s.trim().is_empty())),
                ),
        )
}

fn source_switcher_modal(store: AppStore) -> View {
    view()
        .w_full()
        .gap_3()
        .child(each(
            store.sources,
            |entry| entry.info.id.clone(),
            move |entry_sig, _| {
                let entry = entry_sig.get_untracked();
                let source_id = entry.info.id.clone();
                let source_id_press = source_id.clone();
                let source_id_active = source_id.clone();
                let is_active =
                    computed(move || store.active_source_id.with(|id| id == &source_id_active));
                let compat_level = entry.compat.level;
                let compat_label = compat_level_label(store, compat_level);
                let display_name = store.source_display_name(&entry.info);
                let account_label: Option<SharedString> = entry
                    .account
                    .as_ref()
                    .filter(|a| !a.name.is_empty())
                    .map(|a| store.t_fmt("plugins.signed_in_as", &[("name", &a.name)]));
                let version_text: SharedString = if entry.info.version.is_empty() || entry.is_local {
                    SharedString::default()
                } else {
                    format!("v{}", entry.info.version).into()
                };

                button()
                    .on_press(move || {
                        store.select_source(&source_id_press);
                        store.close_modal();
                    })
                    .w_full()
                    .min_h(px(54.0))
                    .px_3p5()
                    .py_3()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded(px(radius::CARD))
                    .border_1()
                    .border_color(move || {
                        if is_active.get() {
                            theme::primary()
                        } else {
                            theme::border()
                        }
                    })
                    .bg(move || {
                        if is_active.get() {
                            theme::primary_soft()
                        } else {
                            theme::card()
                        }
                    })
                    .transition_colors()
                    .duration(move || motion::transition_ms(140.0))
                    .hover(|s| s.bg(theme::accent))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
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
                                    .child(
                                        text(display_name)
                                            .number_of_lines(1)
                                            .text_size(px(font_size::BODY))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::fg),
                                    )
                                    .children((!version_text.is_empty()).then(|| {
                                        text(version_text)
                                            .text_size(px(font_size::MICRO))
                                            .text_color(theme::muted_fg)
                                    })),
                            )
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .flex_wrap()
                                    .gap_2()
                                    .children((!entry.is_local).then(|| {
                                        compat_badge_text(compat_level, compat_label)
                                    }))
                                    .children(account_label.map(|acc| {
                                        text(acc)
                                            .number_of_lines(1)
                                            .text_size(px(font_size::CAPTION))
                                            .text_color(theme::muted_fg)
                                    })),
                            ),
                    )
                    .child(when(is_active, || icon(Icon::Check, 18.0, colors::PRIMARY)))
            },
        ))
        .child(
            view()
                .w_full()
                .pt_2()
                .border_t_1()
                .border_color(theme::border)
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(ghost_button(move || store.t("plugins.title"), move || {
                    store.close_modal();
                    store.navigate(Route::Plugins);
                }))
                .child(primary_button(move || store.t("plugins.install"), move || {
                    store.open_modal(ActiveModal::InstallPlugin);
                })),
        )
}

pub(crate) fn compat_level_label(
    store: AppStore,
    level: nsh_music_core::source::PluginCompatLevel,
) -> SharedString {
    use nsh_music_core::source::PluginCompatLevel;
    match level {
        PluginCompatLevel::FullNative => store.t("compat.full_native"),
        PluginCompatLevel::VirtualFfmpeg => store.t("compat.virtual_ffmpeg"),
        PluginCompatLevel::DesktopProcessLimited => store.t("compat.desktop_limited"),
    }
}

/// Computes the reactive responsive column count using `form.card_columns(...)`.
pub(crate) fn grid_columns(store: AppStore, form: DeviceForm) -> u16 {
    let win_w = window().width.0;
    let sidebar_w = match form {
        DeviceForm::Desktop => {
            if store.sidebar_collapsed.get() {
                68.0
            } else {
                232.0
            }
        }
        DeviceForm::Pad { landscape: true } => 76.0,
        DeviceForm::Pad { landscape: false } | DeviceForm::Phone => 0.0,
    };
    let content_w = (win_w - sidebar_w).max(320.0);
    form.card_columns(content_w) as u16
}

/// Responsive `MediaCard` grid adapting cleanly across Desktop, Pad, and Phone.
pub(crate) fn card_grid(
    store: AppStore,
    form: DeviceForm,
    cards: Vec<AnyElement>,
) -> View {
    let gap_pt = if form.is_phone() { 10.0 } else { 14.0 };
    view()
        .w_full()
        .grid()
        .grid_cols(move || grid_columns(store, form))
        .gap(px(gap_pt))
        .children(cards)
}

/// Animated shimmer card grid placeholder for loading catalogs.
pub(crate) fn shimmer_card_grid(
    store: AppStore,
    form: DeviceForm,
    count: usize,
    circular: bool,
) -> View {
    let cards: Vec<AnyElement> = (0..count)
        .map(|_| skeleton_card(circular).into_element())
        .collect();
    card_grid(store, form, cards)
}

/// Animated shimmer track list placeholder for loading track tables.
pub(crate) fn shimmer_track_list(count: usize) -> View {
    let mut list = view()
        .w_full()
        .p_2()
        .gap_2()
        .rounded(px(radius::CARD))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border);
    for _ in 0..count {
        list = list.child(
            view()
                .w_full()
                .h(px(50.0))
                .px_2p5()
                .flex_row()
                .items_center()
                .gap_3()
                .child(view().size(px(38.0)).flex_shrink_0().child(shimmer_block(radius::SM)))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1p5()
                        .child(view().h(px(13.0)).w(relative(0.55)).child(shimmer_block(radius::SM)))
                        .child(view().h(px(10.0)).w(relative(0.32)).child(shimmer_block(radius::SM))),
                )
                .child(view().w(px(40.0)).h(px(11.0)).child(shimmer_block(radius::SM))),
        );
    }
    list
}

/// Animated shimmer detail hero placeholder for Playlist / Album / Artist / Show loading states.
pub(crate) fn shimmer_detail_hero(form: DeviceForm) -> View {
    let art_size = match form {
        DeviceForm::Desktop => 200.0,
        DeviceForm::Pad { .. } => 176.0,
        DeviceForm::Phone => 188.0,
    };
    let art = view().size(px(art_size)).flex_shrink_0().child(shimmer_block(radius::XL));
    let meta = view()
        .flex_1()
        .min_w(px(0.0))
        .gap_3()
        .when(form.is_phone(), |v| v.items_center())
        .child(view().w(px(88.0)).h(px(22.0)).child(shimmer_block(radius::PILL)))
        .child(view().w(relative(0.68)).h(px(28.0)).child(shimmer_block(radius::SM)))
        .child(view().w(relative(0.45)).h(px(14.0)).child(shimmer_block(radius::SM)))
        .child(
            view()
                .flex_row()
                .gap_2p5()
                .pt_2()
                .child(view().w(px(110.0)).h(px(38.0)).child(shimmer_block(radius::CONTROL)))
                .child(view().w(px(110.0)).h(px(38.0)).child(shimmer_block(radius::CONTROL))),
        );

    let body = if form.is_phone() {
        view().w_full().items_center().gap_4().child(art).child(meta)
    } else {
        view().w_full().flex_row().items_end().gap_6().child(art).child(meta)
    };

    view()
        .w_full()
        .p(px(if form.is_phone() { 18.0 } else { 24.0 }))
        .rounded(px(radius::XL))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border)
        .child(body)
}

/// Wraps page content in a full-bleed scrollable container with adaptive insets,
/// scroll-edge header reporting (`store.page_scrolled`), and bottom clearance
/// above desktop/floating player bars.
pub(crate) fn page_scroll(form: DeviceForm, content: View) -> View {
    let store = crate::state::use_store();
    let pad_x = form.page_padding_x();
    let pad_top = if form.is_desktop() { 20.0 } else { 14.0 };
    let pad_bottom = if form.is_desktop() { 40.0 } else { 112.0 };

    view().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(
        scroll()
            .flex_1()
            .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
            .keyboard_taps(KeyboardTaps::Handled)
            .on_scroll(move |ev: &ScrollEvent| {
                store.page_scrolled.set(ev.offset.y.0 > 24.0);
            })
            .child(
                view()
                    .w_full()
                    .max_w(px(1360.0))
                    .self_center()
                    .px(px(pad_x))
                    .pt(px(pad_top))
                    .pb(px(pad_bottom))
                    .gap_6()
                    .child(content),
            ),
    )
}

/// Shuffles `tracks` and starts playback immediately.
pub(crate) fn shuffle_and_play(store: AppStore, mut tracks: Vec<Track>) {
    if tracks.is_empty() {
        return;
    }
    if !store.shuffle.get_untracked() {
        store.toggle_shuffle();
    }
    let seed = (nsh_core::time::unix_ms() as usize) % tracks.len();
    tracks.rotate_left(seed);
    store.play_list(tracks, 0);
}

/// Builds the full `mygo-music`-aligned context menu for a track row.
pub(crate) fn build_track_context_menu(
    store: AppStore,
    tr: Track,
    queue_tracks: Rc<Vec<Track>>,
    queue_index: usize,
    playlist_ctx: Option<PlaylistTrackContext>,
) -> Vec<ContextMenuAction> {
    let mut actions = vec![
        ContextMenuAction::new(store.t("player.play"), move || {
            store.play_list((*queue_tracks).clone(), queue_index);
        })
        .icon(Icon::Play),
        ContextMenuAction::new(store.t("player.play_next"), {
            let t = tr.clone();
            move || store.enqueue_next(t.clone())
        })
        .icon(Icon::Queue),
        ContextMenuAction::new(store.t("player.add_to_queue"), {
            let t = tr.clone();
            move || store.enqueue(t.clone())
        })
        .icon(Icon::Plus),
    ];

    if store.active_can("like") || store.active_can("likeEpisode") {
        let liked = store.is_liked(&tr.id);
        actions.push(
            ContextMenuAction::new(
                if liked {
                    store.t("player.unlike")
                } else {
                    store.t("player.like")
                },
                {
                    let t = tr.clone();
                    move || store.toggle_like(t.clone())
                },
            )
            .icon(if liked { Icon::HeartFilled } else { Icon::Heart })
            .separator_before(),
        );
    }

    // Copy & Download actions
    actions.push(
        ContextMenuAction::new(store.t("track.copy_title"), {
            let t = tr.clone();
            move || store.copy_track_title(&t)
        })
        .separator_before(),
    );
    actions.push(ContextMenuAction::new(
        store.t("track.copy_title_artists"),
        {
            let t = tr.clone();
            move || store.copy_track_title_and_artists(&t)
        },
    ));
    if !tr.url.is_empty() {
        actions.push(
            ContextMenuAction::new(store.t("track.copy_link"), {
                let t = tr.clone();
                move || store.copy_track_link(&t)
            })
            .icon(Icon::External),
        );
    }
    if store.can_download_track(&tr) {
        actions.push(
            ContextMenuAction::new(store.t("track.download"), {
                let t = tr.clone();
                move || store.download_track(t.clone())
            })
            .icon(Icon::Download),
        );
    }

    // Add to Playlist section (all editable user playlists + New Playlist...)
    let can_add_pl = store.active_can("addToPlaylist");
    let can_create_pl = store.active_can("createPlaylist");
    if can_add_pl || can_create_pl {
        let mut first_pl_item = true;
        if can_add_pl {
            let user_playlists = store.playlists.with_untracked(|list| {
                list.iter()
                    .filter(|p| p.mine && !p.fixed)
                    .cloned()
                    .collect::<Vec<_>>()
            });
            for pl in user_playlists {
                let pid = pl.id.clone();
                let label = SharedString::from(format!(
                    "{} · {}",
                    store.t("playlist.add_to"),
                    pl.name
                ));
                let t = tr.clone();
                let mut act = ContextMenuAction::new(label, move || {
                    store.add_to_playlist(&pid, t.clone());
                })
                .icon(Icon::Library);
                if first_pl_item {
                    act = act.separator_before();
                    first_pl_item = false;
                }
                actions.push(act);
            }
        }
        if can_create_pl {
            let tid = tr.id.clone();
            let mut act = ContextMenuAction::new(store.t("playlist.new_and_add"), move || {
                store.open_modal(ActiveModal::CreatePlaylist {
                    initial_track_id: Some(tid.clone()),
                });
            })
            .icon(Icon::Plus);
            if first_pl_item {
                act = act.separator_before();
            }
            actions.push(act);
        }
    }

    // Go to Podcast / Artist / Album navigation items
    let mut first_nav = true;
    if !tr.show_id.is_empty() && store.active_can("show") {
        let show_id = tr.show_id.clone();
        let mut act = ContextMenuAction::new(store.t("track.go_to_podcast"), move || {
            store.navigate(Route::ShowDetail {
                id: show_id.clone(),
            });
        })
        .icon(Icon::Podcast);
        if first_nav {
            act = act.separator_before();
            first_nav = false;
        }
        actions.push(act);
    }

    if let Some(ar) = tr.artists.first()
        && !ar.id.is_empty()
        && store.active_can("artist")
    {
        let artist_id = ar.id.clone();
        let mut act = ContextMenuAction::new(store.t("nav.artists"), move || {
            store.navigate(Route::ArtistDetail {
                id: artist_id.clone(),
            });
        })
        .icon(Icon::User);
        if first_nav {
            act = act.separator_before();
            first_nav = false;
        }
        actions.push(act);
    }

    if !tr.album.id.is_empty() && store.active_can("album") {
        let album_id = tr.album.id.clone();
        let mut act = ContextMenuAction::new(store.t("nav.albums"), move || {
            store.navigate(Route::AlbumDetail {
                id: album_id.clone(),
            });
        })
        .icon(Icon::Disc);
        if first_nav {
            act = act.separator_before();
        }
        actions.push(act);
    }

    // Editable playlist item management (Move Up / Move Down / Remove from Playlist)
    if let Some(ctx) = playlist_ctx {
        let can_move = store.active_can("movePlaylistTrack");
        let can_remove = store.active_can("removeFromPlaylist");
        let mut first_edit = true;

        if can_move {
            let pid_up = ctx.playlist_id.clone();
            let idx = ctx.index;
            let mut up = ContextMenuAction::new(store.t("playlist.move_up"), move || {
                if idx > 0 {
                    store.move_playlist_track(&pid_up, idx, idx - 1);
                }
            })
            .icon(Icon::ArrowUp)
            .disabled(idx == 0);
            if first_edit {
                up = up.separator_before();
                first_edit = false;
            }
            actions.push(up);

            let pid_dn = ctx.playlist_id.clone();
            let total = ctx.total;
            actions.push(
                ContextMenuAction::new(store.t("playlist.move_down"), move || {
                    if idx + 1 < total {
                        store.move_playlist_track(&pid_dn, idx, idx + 1);
                    }
                })
                .icon(Icon::ArrowDown)
                .disabled(idx + 1 >= total),
            );
        }

        if can_remove {
            let pid_rm = ctx.playlist_id;
            let tid_rm = tr.id;
            let mut rm = ContextMenuAction::new(store.t("playlist.remove_track"), move || {
                store.remove_from_playlist(&pid_rm, &tid_rm);
            })
            .icon(Icon::Trash)
            .destructive(true);
            if first_edit {
                rm = rm.separator_before();
            }
            actions.push(rm);
        }
    }

    actions
}

/// Renders a `TrackRow` wired to `AppStore` playback, like heart, live spectrum,
/// localized fallback artist, and full `mygo-music` context menu.
pub(crate) fn render_track_row(
    store: AppStore,
    track: Track,
    index: Option<usize>,
    queue_tracks: Rc<Vec<Track>>,
    queue_index: usize,
    show_cover: bool,
) -> AnyElement {
    render_track_row_in_playlist(
        store,
        track,
        index,
        queue_tracks,
        queue_index,
        show_cover,
        None,
    )
}

/// Renders a `TrackRow` with optional editable playlist reorder/remove context.
pub(crate) fn render_track_row_in_playlist(
    store: AppStore,
    track: Track,
    index: Option<usize>,
    queue_tracks: Rc<Vec<Track>>,
    queue_index: usize,
    show_cover: bool,
    playlist_ctx: Option<PlaylistTrackContext>,
) -> AnyElement {
    let track_id_active = track.id.clone();
    let track_id_like = track.id.clone();
    let track_for_like = track.clone();
    let track_for_more = track.clone();
    let queue_for_play = Rc::clone(&queue_tracks);

    let mut row = track_row(track.clone())
        .fallback_artist(store.t("track.unknown_artist"))
        .show_cover(show_cover)
        .active(move || {
            store
                .current_track
                .with(|t| t.as_ref().is_some_and(|cur| cur.id == track_id_active))
        })
        .playing(move || store.is_playing.get())
        .liked(move || store.is_liked(&track_id_like))
        .spectrum(store.spectrum)
        .on_play(move || {
            store.play_list((*queue_for_play).clone(), queue_index);
        })
        .on_toggle_like(move || {
            store.toggle_like(track_for_like.clone());
        })
        .on_more(move || {
            store.enqueue(track_for_more.clone());
        });

    if let Some(i) = index {
        row = row.index(i);
    }

    if track.local_path.to_ascii_lowercase().ends_with(".flac")
        || track.local_path.to_ascii_lowercase().ends_with(".wav")
    {
        row = row.quality_badge("LOSSLESS");
    }

    let actions = build_track_context_menu(store, track, queue_tracks, queue_index, playlist_ctx);
    styled_context_menu(row, actions).into_element()
}

/// Optional desktop column header bar placed above track tables.
pub(crate) fn track_table_header(store: AppStore, form: DeviceForm, show_album: bool) -> View {
    if form.is_phone() {
        return view();
    }
    view()
        .w_full()
        .px_2p5()
        .pb_2()
        .border_b_1()
        .border_color(theme::border)
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            text("#")
                .w(px(28.0))
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_fg),
        )
        .child(
            text(move || store.t("track.column.title"))
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_fg),
        )
        .children(
            (show_album
                && matches!(
                    form,
                    DeviceForm::Desktop | DeviceForm::Pad { landscape: true }
                ))
            .then(|| {
                text(move || store.t("track.column.album"))
                    .w(px(180.0))
                    .pr_3()
                    .text_size(px(font_size::MICRO))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::muted_fg)
            }),
        )
        .child(
            text(move || store.t("track.column.duration"))
                .w(px(88.0))
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_fg),
        )
}

/// Shared detail header banner with artwork, ambient wash, metadata, back navigation,
/// and action buttons used across Playlist, Album, Artist, and Podcast Show pages.
#[allow(clippy::too_many_arguments)]
pub(crate) fn detail_hero(
    store: AppStore,
    form: DeviceForm,
    badge: SharedString,
    title: SharedString,
    subtitle: SharedString,
    meta: SharedString,
    description: SharedString,
    image_url: SharedString,
    seed: SharedString,
    circular_art: bool,
    actions: View,
) -> View {
    let accent = ArtworkLoader::accent_for_seed(&seed);
    let art_size = match form {
        DeviceForm::Desktop => 200.0,
        DeviceForm::Pad { .. } => 176.0,
        DeviceForm::Phone => 188.0,
    };
    let art_radius = if circular_art {
        radius::PILL
    } else {
        radius::XL
    };
    let art_box = Elevated::elevation(
        artwork_box(image_url, seed, art_size, art_radius),
        Elevation::Card,
    );

    let info_col = view()
        .flex_1()
        .min_w(px(0.0))
        .gap_2()
        .when(form.is_phone(), |v| v.items_center())
        .children((!badge.is_empty()).then(|| {
            view()
                .px_2p5()
                .py_1()
                .self_start()
                .when(form.is_phone(), |v| v.self_center())
                .rounded_full()
                .bg(theme::primary_soft)
                .child(
                    text(badge)
                        .text_size(px(font_size::MICRO))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::primary),
                )
        }))
        .child(
            text(title)
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
        .children((!subtitle.is_empty()).then(|| {
            text(subtitle)
                .number_of_lines(2)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::fg)
        }))
        .children((!meta.is_empty()).then(|| {
            text(meta)
                .number_of_lines(2)
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg)
        }))
        .children((!description.is_empty()).then(|| {
            text(description)
                .number_of_lines(3)
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg)
        }))
        .child(view().pt_2().child(actions));

    let back_bar = when(
        move || store.can_go_back(),
        move || {
            view().w_full().flex_row().items_center().child(
                button()
                    .on_press(move || {
                        store.go_back();
                    })
                    .min_h(px(form.min_touch_target()))
                    .px_2p5()
                    .py_1()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .rounded_full()
                    .bg(theme::glass)
                    .border_1()
                    .border_color(theme::border)
                    .hover(|s| s.bg(theme::accent))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(icon(Icon::ChevronLeft, 16.0, colors::FOREGROUND))
                    .child(
                        text(move || store.t("common.back"))
                            .text_size(px(font_size::SUBTITLE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::fg),
                    ),
            )
        },
    );

    let body = if form.is_phone() {
        view()
            .w_full()
            .items_center()
            .gap_4()
            .child(art_box)
            .child(info_col)
    } else {
        view()
            .w_full()
            .flex_row()
            .items_end()
            .gap_6()
            .child(art_box)
            .child(info_col)
    };

    Elevated::elevation(
        view()
            .w_full()
            .p(px(if form.is_phone() { 18.0 } else { 24.0 }))
            .gap_4()
            .rounded(px(radius::XL))
            .overflow_hidden()
            .bg_linear_to_br()
            .gradient_from(theme::artwork_tint(accent))
            .gradient_to(theme::card())
            .border_1()
            .border_color(theme::artwork_wash(accent, 0.25)),
        Elevation::Card,
    )
        .child(back_bar)
        .child(body)
}

/// Renders a list of `Browse` sections (tracks, playlists, albums, artists,
/// stations, podcasts, browse links) cleanly across any music source.
pub(crate) fn render_browse_sections(
    store: AppStore,
    form: DeviceForm,
    sections: Vec<Section>,
) -> View {
    let mut container = view().w_full().gap_6();

    for sec in sections {
        let has_any = !sec.playlists.is_empty()
            || !sec.tracks.is_empty()
            || !sec.albums.is_empty()
            || !sec.artists.is_empty()
            || !sec.stations.is_empty()
            || !sec.shows.is_empty()
            || !sec.links.is_empty();

        let action_cb: Option<Rc<dyn Fn()>> = if !sec.more_page.is_empty() {
            let page = sec.more_page.clone();
            let title = if sec.page_title.is_empty() {
                sec.title.clone()
            } else {
                sec.page_title.clone()
            };
            Some(Rc::new(move || match page.as_str() {
                "radio" => store.navigate(Route::Radio),
                "podcasts" => store.navigate(Route::Podcasts),
                "local" => store.navigate(Route::Local),
                _ => store.navigate(Route::Browse {
                    page: page.clone(),
                    title: title.clone(),
                }),
            }))
        } else {
            None
        };
        let action_label = action_cb.as_ref().map(|_| store.t("common.see_all"));
        let subtitle =
            (!sec.subtitle.is_empty()).then(|| SharedString::from(sec.subtitle.clone()));

        let mut sec_view = view().w_full().gap_3();
        if !sec.title.is_empty() {
            sec_view = sec_view.child(section_header(
                sec.title.clone(),
                subtitle,
                action_label,
                action_cb,
            ));
        }

        if !has_any {
            if !sec.empty.is_empty() {
                sec_view = sec_view.child(empty_state(
                    Icon::Disc,
                    store.t("common.empty"),
                    sec.empty.clone(),
                ));
            }
            container = container.child(sec_view);
            continue;
        }

        if !sec.playlists.is_empty() {
            let cards: Vec<AnyElement> = sec
                .playlists
                .iter()
                .map(|pl| {
                    let pid = pl.id.clone();
                    let pid_play = pl.id.clone();
                    let pl_tracks = pl.tracks.clone();
                    let count = if pl.tracks.is_empty() {
                        pl.track_count
                    } else {
                        pl.tracks.len()
                    };
                    let count_str = count.to_string();
                    let sub = if !pl.description.is_empty() {
                        pl.description.clone()
                    } else if count > 0 {
                        store
                            .t_fmt("track.count_fmt", &[("count", &count_str)])
                            .to_string()
                    } else {
                        pl.owner.clone()
                    };
                    let mut card = media_card(pl.name.clone(), sub)
                        .image_url(pl.image_url.clone())
                        .seed(pl.id.clone())
                        .on_press(move || {
                            store.navigate(Route::PlaylistDetail { id: pid.clone() });
                        })
                        .on_play(move || {
                            if !pl_tracks.is_empty() {
                                store.play_list(pl_tracks.clone(), 0);
                            } else {
                                let src = store.active_source();
                                let id = pid_play.clone();
                                spawn(async move {
                                    if let Ok(Ok(full)) = blocking(move || src.playlist(&id)).await
                                        && !full.tracks.is_empty()
                                    {
                                        store.play_list(full.tracks, 0);
                                    }
                                });
                            }
                        });
                    if count > 0 {
                        card = card.badge(store.t_fmt("track.count_fmt", &[("count", &count_str)]));
                    }
                    card.into_element()
                })
                .collect();
            sec_view = sec_view.child(card_grid(store, form, cards));
        }

        if !sec.tracks.is_empty() {
            let queue = Rc::new(sec.tracks.clone());
            let mut list = view().w_full().gap_1();
            if !form.is_phone() {
                list = list.child(track_table_header(store, form, true));
            }
            for (idx, tr) in sec.tracks.iter().enumerate() {
                list = list.child(render_track_row(
                    store,
                    tr.clone(),
                    Some(idx),
                    Rc::clone(&queue),
                    idx,
                    true,
                ));
            }
            sec_view = sec_view.child(
                view()
                    .w_full()
                    .p_2()
                    .rounded(px(radius::CARD))
                    .bg(theme::card)
                    .border_1()
                    .border_color(theme::border)
                    .child(list),
            );
        }

        if !sec.albums.is_empty() {
            let cards: Vec<AnyElement> = sec
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
                    let mut card = media_card(alb.name.clone(), sub)
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
                        });
                    if !alb.released.is_empty() {
                        card = card.badge(alb.released.clone());
                    }
                    card.into_element()
                })
                .collect();
            sec_view = sec_view.child(card_grid(store, form, cards));
        }

        if !sec.stations.is_empty() {
            let cards: Vec<AnyElement> = sec
                .stations
                .iter()
                .map(|st| {
                    let st_press = st.clone();
                    let st_play = st.clone();
                    media_card(st.name.clone(), st.description.clone())
                        .image_url(st.image_url.clone())
                        .seed(st.id.clone())
                        .badge(store.t("station.live_mix"))
                        .on_press(move || store.play_station(st_press.clone()))
                        .on_play(move || store.play_station(st_play.clone()))
                        .into_element()
                })
                .collect();
            sec_view = sec_view.child(card_grid(store, form, cards));
        }

        if !sec.shows.is_empty() {
            let cards: Vec<AnyElement> = sec
                .shows
                .iter()
                .map(|show| {
                    let sid = show.id.clone();
                    let sid_play = show.id.clone();
                    let badge = if show.episode_count > 0 {
                        store.t_fmt(
                            "library.episodes_count",
                            &[("count", &show.episode_count.to_string())],
                        )
                    } else {
                        store.t("nav.podcasts")
                    };
                    media_card(show.name.clone(), show.publisher.clone())
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
            sec_view = sec_view.child(card_grid(store, form, cards));
        }

        if !sec.artists.is_empty() {
            let cards: Vec<AnyElement> = sec
                .artists
                .iter()
                .map(|art| {
                    let aid = art.id.clone();
                    let aid_play = art.id.clone();
                    let sub = if art.following {
                        store.t("library.following_artist")
                    } else {
                        store.t("nav.artists")
                    };
                    media_card(art.name.clone(), sub)
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
                                if let Ok(Ok(page)) = blocking(move || src.artist(&id)).await
                                    && !page.tracks.is_empty()
                                {
                                    store.play_list(page.tracks, 0);
                                }
                            });
                        })
                        .into_element()
                })
                .collect();
            sec_view = sec_view.child(card_grid(store, form, cards));
        }

        if !sec.links.is_empty() {
            let link_cols = match form {
                DeviceForm::Desktop => 3u16,
                DeviceForm::Pad { .. } => 2u16,
                DeviceForm::Phone => 1u16,
            };
            let mut grid = view().w_full().grid().grid_cols(link_cols).gap_3();
            for link in &sec.links {
                let target_page = link.page.clone();
                let target_title = link.title.clone();
                let seed = format!("link-{}", link.page);
                let accent = ArtworkLoader::accent_for_seed(&seed);
                grid = grid.child(
                    button()
                        .on_press(move || match target_page.as_str() {
                            "radio" => store.navigate(Route::Radio),
                            "podcasts" => store.navigate(Route::Podcasts),
                            "local" => store.navigate(Route::Local),
                            _ => store.navigate(Route::Browse {
                                page: target_page.clone(),
                                title: target_title.clone(),
                            }),
                        })
                        .w_full()
                        .min_h(px(76.0))
                        .p_3p5()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .rounded(px(radius::CARD))
                        .bg_linear_to_br()
                        .gradient_from(theme::artwork_tint(accent))
                        .gradient_to(theme::card())
                        .border_1()
                        .border_color(theme::border)
                        .elevation(Elevation::Subtle)
                        .transition_all()
                        .duration(move || motion::transition_ms(160.0))
                        .hover(|s| s.border_color(theme::primary))
                        .active(|s| s.scale(motion::press_scale))
                        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                        .child(artwork_box(
                            link.image_url.clone(),
                            seed.into(),
                            48.0,
                            radius::SM,
                        ))
                        .child(
                            view()
                                .flex_1()
                                .min_w(px(0.0))
                                .gap_0p5()
                                .child(
                                    text(link.title.clone())
                                        .number_of_lines(1)
                                        .text_size(px(font_size::BODY))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::fg),
                                )
                                .children((!link.subtitle.is_empty()).then(|| {
                                    text(link.subtitle.clone())
                                        .number_of_lines(1)
                                        .text_size(px(font_size::CAPTION))
                                        .text_color(theme::muted_fg)
                                })),
                        )
                        .child(icon(Icon::ChevronRight, 16.0, colors::MUTED_FOREGROUND)),
                );
            }
            sec_view = sec_view.child(grid);
        }

        container = container.child(sec_view);
    }

    container
}

/// Helper for a compact action icon button with touch-safe hit target.
#[allow(dead_code)]
pub(crate) fn action_icon(
    kind: Icon,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    icon_button_sized(kind, 36.0, 18.0, theme::fg, label, on_press)
}
