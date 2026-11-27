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
use nsh_music_core::model::{Artist, Section, Track};

use crate::state::{ActiveModal, AppStore, ArtworkLoader, Route};
use crate::ui::theme::{colors, font_size, radius};
use crate::ui::{
    ContextMenuAction, DeviceForm, Elevated, Elevation, Icon, adaptive_modal, artwork_box,
    compat_badge_text, empty_state, format_duration_ms, ghost_button, heart_button, icon,
    icon_button_sized, media_card, mini_equalizer, motion, primary_button, section_header,
    shimmer_block, skeleton_card, styled_input, styled_lazy_context_menu, styled_lazy_menu, theme,
};

/// Artwork resolution targets matching `egoist/mygo-music` sizing policy.
pub(crate) const ARTWORK_SIZE_THUMB: u32 = 120;
pub(crate) const ARTWORK_SIZE_CARD: u32 = 240;
pub(crate) const ARTWORK_SIZE_HERO: u32 = 600;

/// Varied skeleton line widths matching `mygo-music` shimmer skeletons.
const BONE_WIDTHS: [f32; 12] = [
    0.62, 0.44, 0.78, 0.53, 0.70, 0.38, 0.66, 0.49, 0.82, 0.57, 0.41, 0.73,
];

/// Persistent route-host [`Owner`] handle so page-triggered async fetches,
/// saves, and subscriptions survive ephemeral `dynamic(...)` branch updates.
#[derive(Clone, Copy)]
pub(crate) struct PagesOwner(Owner);

impl PagesOwner {
    pub(crate) fn capture() -> Self {
        use_context::<Self>().unwrap_or_else(|| Self(Owner::current()))
    }

    pub(crate) fn spawn(self, future: impl std::future::Future<Output = ()> + 'static) {
        self.0.run(|| {
            spawn(future);
        });
    }
}

/// Context for a track rendered inside an editable user playlist.
#[derive(Debug, Clone)]
pub(crate) struct PlaylistTrackContext {
    pub playlist_id: String,
    pub index: usize,
    pub total: usize,
}

/// Expands `{size}`, `{w}`, and `{h}` artwork template placeholders to `size_px`.
pub(crate) fn sized_artwork_url(raw: &str, size_px: u32) -> SharedString {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        SharedString::default()
    } else {
        ArtworkLoader::resolve_template(trimmed, size_px).into()
    }
}

/// Resolves a track's best cover artwork (`local_cover` preferred over
/// `album.image_url`) with `{size}` / `{w}` / `{h}` expanded to `size_px`.
pub(crate) fn sized_track_cover(track: &Track, size_px: u32) -> SharedString {
    if !track.local_cover.trim().is_empty() {
        sized_artwork_url(&track.local_cover, size_px)
    } else {
        sized_artwork_url(&track.album.image_url, size_px)
    }
}

/// Returns whether a local track path indicates a lossless container/codec.
pub(crate) fn is_lossless_track(track: &Track) -> bool {
    let lower = track.local_path.to_ascii_lowercase();
    lower.ends_with(".flac")
        || lower.ends_with(".wav")
        || lower.ends_with(".alac")
        || lower.ends_with(".aiff")
        || lower.ends_with(".aif")
}

/// Renders the currently active route from `store.route` adapted to `form`.
pub fn render_route(store: AppStore, form: DeviceForm) -> View {
    provide_context(PagesOwner(Owner::current()));

    view()
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .child(dynamic(move || {
            let route = store.route.get();
            untracked(move || {
                if store.page_scrolled.get_untracked() {
                    store.page_scrolled.set(false);
                }
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
                    Route::SettingsDetail(section) => {
                        settings::settings_detail_page(store, form, section)
                    }
                    Route::Plugins => settings::settings_page(store, form, true),
                };
                page_view.into_element()
            })
        }))
}

fn is_settings_modal(modal: &ActiveModal) -> bool {
    matches!(
        modal,
        ActiveModal::InstallPlugin
            | ActiveModal::ConfirmPluginInstall { .. }
            | ActiveModal::PluginConfig { .. }
            | ActiveModal::QrSignIn { .. }
            | ActiveModal::WebSignIn { .. }
    )
}

fn modal_title_for(store: AppStore, modal: &ActiveModal) -> SharedString {
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
                SharedString::from(title.clone())
            }
        }
        ActiveModal::CreatePlaylist { .. } => store.t("dialog.create_playlist.title"),
        ActiveModal::RenamePlaylist { .. } => store.t("dialog.rename_playlist.title"),
        ActiveModal::SourceSwitcher => store.t("dialog.source_switcher.title"),
    }
}

fn render_modal_body(store: AppStore, modal: ActiveModal) -> AnyElement {
    match modal {
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
            cookies_of,
            until,
            user_agent,
            allow_manual_cookie,
        } => settings::web_sign_in_modal(
            store,
            source_id,
            url,
            title,
            cookies_of,
            until,
            user_agent,
            allow_manual_cookie,
        )
        .into_element(),
        ActiveModal::CreatePlaylist { initial_track_id } => {
            create_playlist_modal(store, initial_track_id).into_element()
        }
        ActiveModal::RenamePlaylist { id, current_name } => {
            rename_playlist_modal(store, id, current_name).into_element()
        }
        ActiveModal::SourceSwitcher => source_switcher_modal(store).into_element(),
    }
}

/// Renders the active app-level modal (`store.active_modal`) inside `adaptive_modal`.
/// Mount once at the root shell (`src/shell/device.rs`).
///
/// On Desktop when the Settings child window is open, settings/plugin modals are
/// hosted inside [`render_settings_modal`] instead so they never appear twice.
pub fn render_active_modal(store: AppStore) -> View {
    let is_open = computed(move || {
        store.active_modal.with(|m| {
            if matches!(m, ActiveModal::None) {
                return false;
            }
            if theme::detect_device_form().is_desktop()
                && store.settings_window_open.get()
                && is_settings_modal(m)
            {
                return false;
            }
            true
        })
    });
    let modal_title = computed(move || {
        if !is_open.get() {
            return SharedString::default();
        }
        store.active_modal.with(|m| modal_title_for(store, m))
    });

    adaptive_modal(
        is_open,
        modal_title,
        move || store.close_modal(),
        move || {
            view().w_full().child(dynamic(move || {
                let modal = store.active_modal.get();
                let hosted_in_settings = theme::detect_device_form().is_desktop()
                    && store.settings_window_open.get()
                    && is_settings_modal(&modal);
                untracked(move || {
                    if hosted_in_settings {
                        view().into_element()
                    } else {
                        render_modal_body(store, modal)
                    }
                })
            }))
        },
    )
}

/// Renders plugin/settings modals (`InstallPlugin`, `ConfirmPluginInstall`,
/// `PluginConfig`, `QrSignIn`, `WebSignIn`) inside the Desktop Settings child
/// window (`src/shell/desktop.rs`).
pub fn render_settings_modal(store: AppStore) -> View {
    let is_open = computed(move || store.active_modal.with(is_settings_modal));
    let modal_title = computed(move || {
        store.active_modal.with(|m| {
            if is_settings_modal(m) {
                modal_title_for(store, m)
            } else {
                SharedString::default()
            }
        })
    });

    adaptive_modal(
        is_open,
        modal_title,
        move || store.close_modal(),
        move || {
            view().w_full().child(dynamic(move || {
                let modal = store.active_modal.get();
                untracked(move || {
                    if is_settings_modal(&modal) {
                        render_modal_body(store, modal)
                    } else {
                        view().into_element()
                    }
                })
            }))
        },
    )
}

fn create_playlist_modal(store: AppStore, initial_track_id: Option<String>) -> View {
    let name = signal(String::new());
    let is_phone = theme::detect_device_form().is_phone();
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
                .test_id("create-playlist-input")
                .w_full()
                .auto_focus(true)
                .return_key_type(ReturnKeyType::Done)
                .on_submit(move |val: &str| {
                    if !val.trim().is_empty() {
                        name.set(val.to_owned());
                    }
                    submit_enter();
                }),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    ghost_button(
                        move || store.t("common.cancel"),
                        move || {
                            store.close_modal();
                        },
                    )
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    primary_button(move || store.t("common.create"), submit)
                        .test_id("create-playlist-submit")
                        .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                        .when(is_phone, |b| b.flex_1())
                        .disabled(move || name.with(|s| s.trim().is_empty())),
                ),
        )
}

fn rename_playlist_modal(store: AppStore, id: String, current_name: String) -> View {
    let name = signal(current_name);
    let is_phone = theme::detect_device_form().is_phone();
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
                .test_id("rename-playlist-input")
                .w_full()
                .auto_focus(true)
                .return_key_type(ReturnKeyType::Done)
                .on_submit(move |val: &str| {
                    if !val.trim().is_empty() {
                        name.set(val.to_owned());
                    }
                    submit_enter();
                }),
        )
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    ghost_button(
                        move || store.t("common.cancel"),
                        move || {
                            store.close_modal();
                        },
                    )
                    .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    primary_button(move || store.t("common.save"), submit)
                        .test_id("rename-playlist-submit")
                        .min_h(px(if is_phone { 44.0 } else { 34.0 }))
                        .when(is_phone, |b| b.flex_1())
                        .disabled(move || name.with(|s| s.trim().is_empty())),
                ),
        )
}

fn source_switcher_modal(store: AppStore) -> View {
    let form = theme::detect_device_form();
    let is_phone = form.is_phone();
    let min_h = px(form.min_touch_target());

    view()
        .w_full()
        .gap_2p5()
        .child(view().w_full().gap_2().child(each(
            store.sources,
            |entry| entry.info.id.clone(),
            move |entry_sig, _| {
                let entry = entry_sig.get_untracked();
                let source_id = entry.info.id.clone();
                let source_id_press = source_id.clone();
                let source_id_active = source_id.clone();
                let is_active =
                    computed(move || store.active_source_id.with(|id| id == &source_id_active));
                let compat_level = if entry.compat.level
                    == nsh_music_core::source::PluginCompatLevel::VirtualFfmpeg
                    && store.settings.with_untracked(|s| !s.mobile_virtual_compat)
                {
                    nsh_music_core::source::PluginCompatLevel::DesktopProcessLimited
                } else {
                    entry.compat.level
                };
                let compat_label = compat_level_label(store, compat_level);
                let display_name = store.source_display_name(&entry.info);
                let account_label: Option<SharedString> = entry
                    .account
                    .as_ref()
                    .filter(|a| !a.name.is_empty())
                    .map(|a| store.t_fmt("plugins.signed_in_as", &[("name", &a.name)]));
                let version_text: SharedString = if entry.info.version.is_empty() || entry.is_local
                {
                    SharedString::default()
                } else {
                    format!("v{}", entry.info.version).into()
                };

                button()
                    .test_id(&format!("source-switcher-item-{source_id}"))
                    .on_press(move || {
                        store.select_source(&source_id_press);
                        store.close_modal();
                    })
                    .w_full()
                    .min_h(px(56.0))
                    .px_3p5()
                    .py_2p5()
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
                                    .children(
                                        (!entry.is_local)
                                            .then(|| compat_badge_text(compat_level, compat_label)),
                                    )
                                    .children(account_label.map(|acc| {
                                        text(acc)
                                            .number_of_lines(1)
                                            .text_size(px(font_size::CAPTION))
                                            .text_color(theme::muted_fg)
                                    })),
                            ),
                    )
                    .child(when(is_active, || {
                        view()
                            .size(px(26.0))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(theme::primary)
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::Check, 14.0, colors::PRIMARY_FOREGROUND))
                    }))
            },
        )))
        .child(
            view()
                .w_full()
                .pt_3()
                .border_t_1()
                .border_color(theme::border)
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2p5()
                .child(
                    ghost_button(
                        move || store.t("plugins.title"),
                        move || {
                            store.close_modal();
                            store.navigate(Route::Plugins);
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.flex_1()),
                )
                .child(
                    primary_button(
                        move || store.t("plugins.install"),
                        move || {
                            store.open_modal(ActiveModal::InstallPlugin);
                        },
                    )
                    .min_h(min_h)
                    .when(is_phone, |b| b.flex_1()),
                ),
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
                240.0
            }
        }
        DeviceForm::Pad { landscape: true } => 76.0,
        DeviceForm::Pad { landscape: false } | DeviceForm::Phone => 0.0,
    };
    let content_w = (win_w - sidebar_w).max(320.0);
    form.card_columns(content_w) as u16
}

/// Responsive `MediaCard` grid adapting cleanly across Desktop, Pad, and Phone.
pub(crate) fn card_grid(store: AppStore, form: DeviceForm, cards: Vec<AnyElement>) -> View {
    let gap_pt = if form.is_phone() { 12.0 } else { 14.0 };
    view()
        .w_full()
        .grid()
        .grid_cols(move || grid_columns(store, form))
        .gap(px(gap_pt))
        .children(cards)
}

/// Horizontal peeking carousel (`148pt` card width, ~2.3 cards visible) for multi-item
/// shelves on Phone, falling back cleanly to [`card_grid`] on Desktop/Pad or small shelves.
fn phone_carousel_or_grid(store: AppStore, form: DeviceForm, cards: Vec<AnyElement>) -> View {
    if form.is_phone() && cards.len() >= 3 {
        let mut row = view().flex_row().items_start().gap_3().py_0p5();
        for card in cards {
            row = row.child(view().w(px(148.0)).flex_shrink_0().child(card));
        }
        return view()
            .w_full()
            .child(scroll().horizontal().scrollbar(false).child(row));
    }
    card_grid(store, form, cards)
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

/// Animated shimmer track list placeholder with varied bone widths matching `mygo-music`.
pub(crate) fn shimmer_track_list(count: usize) -> View {
    let mut list = view()
        .w_full()
        .p_2()
        .gap_2()
        .rounded(px(radius::CARD))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border);
    for i in 0..count {
        let title_w = (BONE_WIDTHS[i % BONE_WIDTHS.len()] * 0.82).clamp(0.28, 0.78);
        let artist_w = (BONE_WIDTHS[(i + 5) % BONE_WIDTHS.len()] * 0.56).clamp(0.18, 0.48);
        list = list.child(
            view()
                .w_full()
                .h(px(50.0))
                .px_2p5()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    view()
                        .size(px(38.0))
                        .flex_shrink_0()
                        .child(shimmer_block(radius::SM)),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1p5()
                        .child(
                            view()
                                .h(px(13.0))
                                .w(relative(title_w))
                                .child(shimmer_block(radius::SM)),
                        )
                        .child(
                            view()
                                .h(px(10.0))
                                .w(relative(artist_w))
                                .child(shimmer_block(radius::SM)),
                        ),
                )
                .child(
                    view()
                        .w(px(40.0))
                        .h(px(11.0))
                        .child(shimmer_block(radius::SM)),
                ),
        );
    }
    list
}

/// Animated shimmer detail hero placeholder for Playlist / Album / Artist / Show loading states.
pub(crate) fn shimmer_detail_hero(form: DeviceForm, circular: bool) -> View {
    let art_size = match form {
        DeviceForm::Desktop => 200.0,
        DeviceForm::Pad { .. } => 176.0,
        DeviceForm::Phone => 184.0,
    };
    let art_radius = if circular { radius::PILL } else { radius::XL };
    let art = view()
        .size(px(art_size))
        .flex_shrink_0()
        .child(shimmer_block(art_radius));
    let meta = view()
        .flex_1()
        .min_w(px(0.0))
        .gap_3()
        .when(form.is_phone(), |v| v.items_center())
        .child(
            view()
                .w(px(88.0))
                .h(px(22.0))
                .child(shimmer_block(radius::PILL)),
        )
        .child(
            view()
                .w(relative(0.68))
                .h(px(28.0))
                .child(shimmer_block(radius::SM)),
        )
        .child(
            view()
                .w(relative(0.45))
                .h(px(14.0))
                .child(shimmer_block(radius::SM)),
        )
        .child(
            view()
                .flex_row()
                .gap_2p5()
                .pt_2()
                .child(
                    view()
                        .w(px(110.0))
                        .h(px(38.0))
                        .child(shimmer_block(radius::CONTROL)),
                )
                .child(
                    view()
                        .w(px(110.0))
                        .h(px(38.0))
                        .child(shimmer_block(radius::CONTROL)),
                ),
        );

    let body = if form.is_phone() {
        view()
            .w_full()
            .items_center()
            .gap_4()
            .child(art)
            .child(meta)
    } else {
        view()
            .w_full()
            .flex_row()
            .items_end()
            .gap_6()
            .child(art)
            .child(meta)
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
/// deduplicated scroll-edge header reporting (`store.page_scrolled`), and bottom
/// clearance so desktop/floating player bars never occlude the last row or card.
pub(crate) fn page_scroll(form: DeviceForm, content: View) -> View {
    let store = crate::state::use_store();
    let pad_x = if form.is_phone() {
        16.0
    } else {
        form.page_padding_x()
    };
    let pad_top = match form {
        DeviceForm::Desktop => 24.0,
        DeviceForm::Pad { .. } => 18.0,
        DeviceForm::Phone => 12.0,
    };
    let pad_bottom = match form {
        DeviceForm::Desktop => 56.0,
        DeviceForm::Pad { .. } => 104.0,
        DeviceForm::Phone => 112.0,
    };

    view().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(
        scroll()
            .flex_1()
            .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
            .keyboard_taps(KeyboardTaps::Handled)
            .on_scroll(move |ev: &ScrollEvent| {
                let scrolled = ev.offset.y.0 > 24.0;
                if store.page_scrolled.get_untracked() != scrolled {
                    store.page_scrolled.set(scrolled);
                }
            })
            .child(
                view()
                    .w_full()
                    .max_w(px(1360.0))
                    .self_center()
                    .pl(move || {
                        px(pad_x
                            + if form.is_desktop() {
                                0.0
                            } else {
                                safe_area().left.0
                            })
                    })
                    .pr(move || {
                        px(pad_x
                            + if form.is_desktop() {
                                0.0
                            } else {
                                safe_area().right.0
                            })
                    })
                    .pt(px(pad_top))
                    .pb(move || {
                        let kb = keyboard_height().0;
                        let kb_open = kb > safe_area().bottom.0 + 24.0;
                        match form {
                            DeviceForm::Desktop => px(pad_bottom),
                            DeviceForm::Phone => {
                                if kb_open {
                                    px(24.0)
                                } else {
                                    px(pad_bottom + safe_area().bottom.0)
                                }
                            }
                            DeviceForm::Pad { .. } => {
                                if kb_open {
                                    px(28.0 + kb)
                                } else {
                                    px(pad_bottom + safe_area().bottom.0)
                                }
                            }
                        }
                    })
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

/// Clickable comma-separated artist links row (or show link for podcast episodes).
/// Never nested inside another `button()`, so artist taps navigate cleanly.
pub(crate) fn artist_links_row(
    store: AppStore,
    artists: &[Artist],
    show_id: &str,
    fallback: SharedString,
    text_size_pt: f32,
    form: DeviceForm,
) -> View {
    let can_show = !show_id.is_empty() && store.active_can("show");
    let can_artist = store.active_can("artist");
    let named: Vec<&Artist> = artists
        .iter()
        .filter(|a| !a.name.trim().is_empty())
        .collect();

    if named.is_empty() {
        return view().min_w(px(0.0)).child(
            text(fallback)
                .number_of_lines(1)
                .text_size(px(text_size_pt))
                .text_color(theme::muted_fg),
        );
    }

    let mut row = view()
        .flex_row()
        .items_center()
        .min_w(px(0.0))
        .overflow_hidden();

    for (i, ar) in named.into_iter().enumerate() {
        if i > 0 {
            row = row.child(
                text(", ")
                    .flex_shrink_0()
                    .text_size(px(text_size_pt))
                    .text_color(theme::muted_fg),
            );
        }

        if can_show {
            let sid = show_id.to_owned();
            let label = ar.name.clone();
            row = row.child(inline_nav_link(label, text_size_pt, form, move || {
                store.navigate(Route::ShowDetail { id: sid.clone() });
            }));
        } else if !ar.id.is_empty() && can_artist {
            let aid = ar.id.clone();
            let label = ar.name.clone();
            row = row.child(inline_nav_link(label, text_size_pt, form, move || {
                store.navigate(Route::ArtistDetail { id: aid.clone() });
            }));
        } else {
            row = row.child(
                text(ar.name.clone())
                    .number_of_lines(1)
                    .text_size(px(text_size_pt))
                    .text_color(theme::muted_fg),
            );
        }
    }

    row
}

/// Compact inline text link button used for artist/album/publisher navigation.
pub(crate) fn inline_nav_link(
    label: impl Into<SharedString>,
    text_size_pt: f32,
    form: DeviceForm,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let a11y = label.clone();
    button()
        .on_press(on_press)
        .accessibility_label(a11y)
        .hit_slop(if form.is_touch() {
            EdgeInsets {
                top: px(12.0),
                right: px(4.0),
                bottom: px(12.0),
                left: px(4.0),
            }
        } else {
            EdgeInsets {
                top: px(2.0),
                right: px(1.0),
                bottom: px(2.0),
                left: px(1.0),
            }
        })
        .rounded(px(4.0))
        .bg(colors::TRANSPARENT)
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.opacity(0.7))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            text(label)
                .number_of_lines(1)
                .text_size(px(text_size_pt))
                .text_color(theme::muted_fg)
                .hover(|s| s.text_color(theme::fg)),
        )
}

/// Builds the full `mygo-music`-aligned action list for a track (`songItems`).
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
            .icon(if liked {
                Icon::HeartFilled
            } else {
                Icon::Heart
            })
            .separator_before(),
        );
    }

    // Copy & Download actions (`ui_songmenu.go:29-46`)
    actions.push(
        ContextMenuAction::new(store.t("track.copy_title"), {
            let t = tr.clone();
            move || store.copy_track_title(&t)
        })
        .separator_before(),
    );
    if !tr.artist_names().is_empty() {
        actions.push(ContextMenuAction::new(
            store.t("track.copy_title_artists"),
            {
                let t = tr.clone();
                move || store.copy_track_title_and_artists(&t)
            },
        ));
    }
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

    // Add to Playlist section (editable user playlists excluding current playlist + New Playlist...)
    let current_pid = playlist_ctx
        .as_ref()
        .map(|ctx| ctx.playlist_id.as_str())
        .unwrap_or("");
    let can_add_pl = store.active_can("addToPlaylist");
    let can_create_pl = store.active_can("createPlaylist");
    if can_add_pl || can_create_pl {
        let mut first_pl_item = true;
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
                first_pl_item = false;
            }
            actions.push(act);
        }
        if can_add_pl {
            let user_playlists = store.playlists.with_untracked(|list| {
                list.iter()
                    .filter(|p| p.mine && !p.fixed && p.id != current_pid)
                    .cloned()
                    .collect::<Vec<_>>()
            });
            for pl in user_playlists {
                let pid = pl.id.clone();
                let label =
                    SharedString::from(format!("{} · {}", store.t("playlist.add_to"), pl.name));
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
    }

    // Go to Podcast / Artist (every credited artist!) / Album (`ui_songmenu.go:47-65`)
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

    if store.active_can("artist") {
        let go_artist = store.t("now_playing.go_to_artist");
        for ar in &tr.artists {
            if ar.id.is_empty() {
                continue;
            }
            let artist_id = ar.id.clone();
            let label: SharedString = if ar.name.is_empty() {
                go_artist.clone()
            } else {
                format!("{go_artist}: {}", ar.name).into()
            };
            let mut act = ContextMenuAction::new(label, move || {
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
    }

    if !tr.album.id.is_empty() && store.active_can("album") {
        let album_id = tr.album.id.clone();
        let go_album = store.t("now_playing.go_to_album");
        let label: SharedString = if tr.album.name.is_empty() {
            go_album
        } else {
            format!("{go_album}: {}", tr.album.name).into()
        };
        let mut act = ContextMenuAction::new(label, move || {
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

/// Wraps `trigger` in a right-click / long-press context menu whose items are
/// evaluated lazily only when the menu actually opens.
pub(crate) fn lazy_context_menu(
    trigger: impl IntoElement,
    build_actions: impl Fn() -> Vec<ContextMenuAction> + 'static,
) -> impl IntoElement {
    styled_lazy_context_menu(trigger, build_actions)
}

/// Renders a touch-safe `···` trigger button that opens a lazy dropdown / sheet
/// song menu on press (no nested `on_press`; `nsh_base::menu()` drives open state).
pub(crate) fn lazy_more_menu_button(
    form: DeviceForm,
    a11y_label: impl IntoReactive<SharedString>,
    build_actions: impl Fn() -> Vec<ContextMenuAction> + 'static,
) -> impl IntoElement {
    let a11y_rx = a11y_label.into_reactive();
    let trigger_size = if form.is_touch() { 44.0 } else { 32.0 };
    let trigger = view()
        .role(AccessibilityRole::Button)
        .label(move || a11y_rx.get())
        .size(px(trigger_size))
        .rounded_full()
        .items_center()
        .justify_center()
        .bg(colors::TRANSPARENT)
        .transition_colors()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(theme::hover))
        .active(|s| s.bg(theme::press).scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon(Icon::MoreHorizontal, 16.0, colors::MUTED_FOREGROUND));

    styled_lazy_menu(trigger, build_actions)
}

/// Virtualized, cell-recycled track list rendered inside `page_scroll` via
/// [`ListViewport::Parent`].
///
/// Key properties:
/// - Only mounts rows near the viewport (smooth 120fps scrolling even for 1,000+ tracks).
/// - Zero 60ms `position_ms` or `spectrum` subscriptions on inactive rows
///   (`mini_equalizer` is mounted exclusively when `is_active && is_playing`).
/// - Clickable artist & album links directly on each row (never nested in play button).
/// - `···` button and right-click open the full lazy `mygo-music` song menu.
/// - Artwork thumbnails expand `{size}`/`{w}`/`{h}` to `120px` (`ARTWORK_SIZE_THUMB`).
pub(crate) fn virtual_track_list(
    store: AppStore,
    form: DeviceForm,
    tracks: Rc<Vec<Track>>,
    show_cover: bool,
    show_album: bool,
    editable_playlist_id: Option<String>,
    edit_order: Option<Signal<bool>>,
) -> View {
    let total = tracks.len();
    let row_h = form.track_row_height();
    let can_move = editable_playlist_id.is_some() && store.active_can("movePlaylistTrack");
    let can_remove = editable_playlist_id.is_some() && store.active_can("removeFromPlaylist");
    let has_inline_edit = editable_playlist_id.is_some() && (can_move || can_remove);

    let keys: Vec<String> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{i}:{}", t.id))
        .collect();
    let tracks_for_cell = Rc::clone(&tracks);

    let list = virtual_list(total, move |at: Position| {
        let tracks_cell = Rc::clone(&tracks_for_cell);
        let tracks_play = Rc::clone(&tracks_for_cell);
        let tracks_like = Rc::clone(&tracks_for_cell);
        let tracks_menu_ctx = Rc::clone(&tracks_for_cell);
        let tracks_menu_more = Rc::clone(&tracks_for_cell);
        let tracks_rm = Rc::clone(&tracks_for_cell);
        let pid_ctx = editable_playlist_id.clone();
        let pid_more = editable_playlist_id.clone();
        let pid_up = editable_playlist_id.clone();
        let pid_dn = editable_playlist_id.clone();
        let pid_rm = editable_playlist_id.clone();

        let tr_memo = computed(move || tracks_cell.get(at.index()).cloned());
        let is_active = computed(move || {
            let tid = tr_memo.with(|t| t.as_ref().map(|tr| tr.id.clone()).unwrap_or_default());
            !tid.is_empty()
                && store
                    .current_track
                    .with(|cur| cur.as_ref().is_some_and(|c| c.id == tid))
        });
        let is_active_playing = computed(move || is_active.get() && store.is_playing.get());
        let is_liked = computed(move || {
            let tid = tr_memo.with(|t| t.as_ref().map(|tr| tr.id.clone()).unwrap_or_default());
            !tid.is_empty() && store.is_liked(&tid)
        });
        let is_unavailable =
            computed(move || tr_memo.with(|t| t.as_ref().is_some_and(|tr| tr.unavailable)));
        let is_lossless =
            computed(move || tr_memo.with(|t| t.as_ref().is_some_and(is_lossless_track)));

        let play_action: Rc<dyn Fn()> = Rc::new(move || {
            let idx = at.index();
            if let Some(tr) = tracks_play.get(idx)
                && !tr.unavailable
            {
                store.play_list((*tracks_play).clone(), idx);
            }
        });
        let play_leading = Rc::clone(&play_action);
        let play_title = Rc::clone(&play_action);

        let leading_index = view()
            .w(px(28.0))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .child(
                when(is_active_playing, move || {
                    mini_equalizer(store.spectrum, store.is_playing)
                })
                .otherwise(move || {
                    when(is_active, || icon(Icon::Play, 13.0, colors::PRIMARY)).otherwise(
                        move || {
                            text(move || format!("{}", at.index() + 1))
                                .tabular_nums()
                                .text_size(px(font_size::CAPTION))
                                .text_color(theme::muted_fg)
                        },
                    )
                }),
            );

        let cover_thumb = show_cover.then(|| {
            let size_pt = if form.is_desktop() { 40.0 } else { 44.0 };
            artwork_box(
                move || {
                    tr_memo.with(|t| {
                        t.as_ref()
                            .map(|tr| sized_track_cover(tr, ARTWORK_SIZE_THUMB))
                            .unwrap_or_default()
                    })
                },
                SharedString::from("track-row-cover"),
                size_pt,
                radius::SM,
            )
        });

        let leading_btn = button()
            .disabled(is_unavailable)
            .accessibility_label(move || {
                let name =
                    tr_memo.with(|t| t.as_ref().map(|tr| tr.name.clone()).unwrap_or_default());
                Some(SharedString::from(format!("Play {name}")))
            })
            .on_press(move || play_leading())
            .min_h(px(form.min_touch_target()))
            .flex_row()
            .items_center()
            .gap_3()
            .flex_shrink_0()
            .active(|s| s.opacity(0.75))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(leading_index)
            .children(cover_thumb);

        let title_btn = button()
            .disabled(is_unavailable)
            .on_press(move || play_title())
            .w_full()
            .flex_row()
            .items_center()
            .justify_start()
            .gap_1p5()
            .min_w(px(0.0))
            .active(|s| s.opacity(0.75))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(
                text(move || {
                    tr_memo.with(|t| {
                        t.as_ref()
                            .map(|tr| SharedString::from(tr.name.clone()))
                            .unwrap_or_default()
                    })
                })
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(if form.is_phone() {
                    font_size::BODY_MOBILE
                } else {
                    font_size::BODY
                }))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || {
                    if is_active.get() {
                        theme::primary()
                    } else {
                        theme::fg()
                    }
                }),
            )
            .child(when(is_lossless, move || {
                view()
                    .px_1p5()
                    .py_0p5()
                    .flex_shrink_0()
                    .rounded(px(4.0))
                    .bg(theme::amber_soft)
                    .child(
                        text(move || store.t("player.lossless"))
                            .text_size(px(10.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::amber),
                    )
            }));

        let artists_row = view().w_full().min_w(px(0.0)).child(dynamic(move || {
            let maybe = tr_memo.get();
            let fallback = store.t("track.unknown_artist");
            untracked(move || match maybe {
                Some(tr) => artist_links_row(
                    store,
                    &tr.artists,
                    &tr.show_id,
                    fallback,
                    font_size::CAPTION,
                    form,
                )
                .into_element(),
                None => view().into_element(),
            })
        }));

        let title_col = view()
            .flex_1()
            .min_w(px(0.0))
            .gap_0p5()
            .child(title_btn)
            .child(artists_row);

        let show_album_col = show_album
            && matches!(
                form,
                DeviceForm::Desktop | DeviceForm::Pad { landscape: true }
            );
        let album_col = show_album_col.then(|| {
            view()
                .w(px(180.0))
                .flex_shrink_0()
                .pr_3()
                .min_w(px(0.0))
                .child(dynamic(move || {
                    let maybe = tr_memo.get();
                    untracked(move || {
                        let Some(tr) = maybe else {
                            return view().into_element();
                        };
                        if tr.album.name.is_empty() {
                            return view().into_element();
                        }
                        if !tr.show_id.is_empty() && store.active_can("show") {
                            let sid = tr.show_id.clone();
                            inline_nav_link(tr.album.name, font_size::CAPTION, form, move || {
                                store.navigate(Route::ShowDetail { id: sid.clone() });
                            })
                            .into_element()
                        } else if !tr.album.id.is_empty() && store.active_can("album") {
                            let aid = tr.album.id.clone();
                            inline_nav_link(tr.album.name, font_size::CAPTION, form, move || {
                                store.navigate(Route::AlbumDetail { id: aid.clone() });
                            })
                            .into_element()
                        } else {
                            text(tr.album.name)
                                .number_of_lines(1)
                                .text_size(px(font_size::CAPTION))
                                .text_color(theme::muted_fg)
                                .into_element()
                        }
                    })
                }))
        });

        let more_menu = lazy_more_menu_button(
            form,
            move || store.t("common.more"),
            move || {
                let idx = at.index();
                let Some(tr) = tracks_menu_more.get(idx).cloned() else {
                    return Vec::new();
                };
                let ctx = pid_more.as_ref().map(|pid| PlaylistTrackContext {
                    playlist_id: pid.clone(),
                    index: idx,
                    total: tracks_menu_more.len(),
                });
                build_track_context_menu(store, tr, Rc::clone(&tracks_menu_more), idx, ctx)
            },
        );

        let inline_edit_controls = (has_inline_edit && edit_order.is_some()).then(|| {
            let edit_sig = edit_order.expect("checked is_some");
            when(
                move || edit_sig.get(),
                move || {
                    let pid_up = pid_up.clone();
                    let pid_dn = pid_dn.clone();
                    let pid_rm = pid_rm.clone();
                    let tracks_rm = Rc::clone(&tracks_rm);
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_shrink_0()
                        .children(can_move.then(move || {
                            let pid_up = pid_up.clone();
                            let pid_dn = pid_dn.clone();
                            view()
                                .flex_row()
                                .items_center()
                                .gap_0p5()
                                .child(
                                    icon_button_sized(
                                        Icon::ArrowUp,
                                        32.0,
                                        15.0,
                                        theme::muted_fg,
                                        move || store.t("playlist.move_up"),
                                        move || {
                                            let idx = at.index();
                                            if idx > 0
                                                && let Some(pid) = &pid_up
                                            {
                                                store.move_playlist_track(pid, idx, idx - 1);
                                            }
                                        },
                                    )
                                    .disabled(move || at.index() == 0),
                                )
                                .child(
                                    icon_button_sized(
                                        Icon::ArrowDown,
                                        32.0,
                                        15.0,
                                        theme::muted_fg,
                                        move || store.t("playlist.move_down"),
                                        move || {
                                            let idx = at.index();
                                            if idx + 1 < total
                                                && let Some(pid) = &pid_dn
                                            {
                                                store.move_playlist_track(pid, idx, idx + 1);
                                            }
                                        },
                                    )
                                    .disabled(move || at.index() + 1 >= total),
                                )
                        }))
                        .children(can_remove.then(move || {
                            let pid_rm = pid_rm.clone();
                            let tracks_rm = Rc::clone(&tracks_rm);
                            icon_button_sized(
                                Icon::Trash,
                                32.0,
                                15.0,
                                theme::danger,
                                move || store.t("playlist.remove_track"),
                                move || {
                                    let idx = at.index();
                                    if let (Some(pid), Some(tr)) = (&pid_rm, tracks_rm.get(idx)) {
                                        store.remove_from_playlist(pid, &tr.id);
                                    }
                                },
                            )
                        }))
                },
            )
        });

        let trailing = view()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .child(heart_button(
                is_liked,
                move || store.t("player.like"),
                move || {
                    if let Some(tr) = tracks_like.get(at.index()).cloned() {
                        store.toggle_like(tr);
                    }
                },
            ))
            .children((!form.is_phone()).then(|| {
                text(move || {
                    tr_memo.with(|t| {
                        t.as_ref()
                            .map(|tr| SharedString::from(format_duration_ms(tr.duration)))
                            .unwrap_or_default()
                    })
                })
                .tabular_nums()
                .w(px(44.0))
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg)
            }))
            .child(more_menu)
            .children(inline_edit_controls);

        let row_view = view()
            .w_full()
            .min_h(px(row_h))
            .px_2p5()
            .py_1()
            .flex_row()
            .items_center()
            .gap_3()
            .rounded(px(radius::CONTROL))
            .bg(move || {
                if is_active.get() {
                    theme::selected()
                } else {
                    colors::TRANSPARENT
                }
            })
            .opacity(move || {
                if is_unavailable.get() {
                    0.45_f32
                } else {
                    1.0_f32
                }
            })
            .transition_colors()
            .duration(move || motion::transition_ms(120.0))
            .hover(|s| s.bg(theme::hover))
            .child(leading_btn)
            .child(title_col)
            .children(album_col)
            .child(trailing);

        lazy_context_menu(row_view, move || {
            let idx = at.index();
            let Some(tr) = tracks_menu_ctx.get(idx).cloned() else {
                return Vec::new();
            };
            let ctx = pid_ctx.as_ref().map(|pid| PlaylistTrackContext {
                playlist_id: pid.clone(),
                index: idx,
                total: tracks_menu_ctx.len(),
            });
            build_track_context_menu(store, tr, Rc::clone(&tracks_menu_ctx), idx, ctx)
        })
    })
    .viewport(ListViewport::Parent)
    .keys(move || keys.clone())
    .estimated_item_size(px(row_h))
    .row_gap(px(4.0))
    .w_full();

    view()
        .w_full()
        .p_2()
        .gap_1()
        .rounded(px(radius::CARD))
        .bg(theme::card)
        .border_1()
        .border_color(theme::border)
        .children((!form.is_phone()).then(|| track_table_header(store, form, show_album)))
        .child(list)
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
                .w(px(116.0))
                .text_size(px(font_size::MICRO))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_fg),
        )
}

/// Shared detail header banner with artwork (`600px` expanded), ambient wash,
/// metadata, back navigation, and action buttons used across Playlist, Album,
/// Artist, and Podcast Show pages.
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
    let subtitle_view = (!subtitle.is_empty()).then(|| {
        view().child(
            text(subtitle)
                .number_of_lines(2)
                .text_size(px(font_size::BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::fg),
        )
    });
    detail_hero_custom_subtitle(
        store,
        form,
        badge,
        title,
        subtitle_view,
        meta,
        description,
        image_url,
        seed,
        circular_art,
        actions,
    )
}

/// Renders the primary Play / Shuffle action row on detail heroes:
/// - **Phone**: balanced side-by-side equal-width rounded-pill buttons (`44pt` tall)
///   with leading Play / Shuffle glyphs.
/// - **Desktop / Pad**: crisp inline primary + ghost buttons.
pub(crate) fn hero_play_shuffle_buttons(
    store: AppStore,
    form: DeviceForm,
    disabled: bool,
    on_play: impl Fn() + 'static,
    on_shuffle: Option<Rc<dyn Fn()>>,
) -> View {
    if form.is_phone() {
        let play_pill = button()
            .on_press(on_play)
            .disabled(disabled)
            .flex_1()
            .h(px(44.0))
            .px_4()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .rounded_full()
            .bg(theme::primary)
            .transition_all()
            .duration(move || motion::transition_ms(140.0))
            .hover(|s| s.bg(theme::primary_hover))
            .active(|s| s.scale(motion::press_scale))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .disabled_style(|s| s.opacity(0.45))
            .child(icon(Icon::Play, 15.0, colors::PRIMARY_FOREGROUND))
            .child(
                text(move || store.t("player.play_all"))
                    .number_of_lines(1)
                    .text_size(px(font_size::BODY))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::primary_fg),
            );

        return view()
            .w_full()
            .flex_row()
            .items_center()
            .gap_2p5()
            .child(play_pill)
            .children(on_shuffle.map(|shuf| {
                button()
                    .on_press(move || shuf())
                    .disabled(disabled)
                    .flex_1()
                    .h(px(44.0))
                    .px_4()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded_full()
                    .bg(theme::secondary)
                    .border_1()
                    .border_color(theme::border)
                    .transition_all()
                    .duration(move || motion::transition_ms(140.0))
                    .hover(|s| s.bg(theme::accent))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .disabled_style(|s| s.opacity(0.45))
                    .child(icon(Icon::Shuffle, 15.0, colors::FOREGROUND))
                    .child(
                        text(move || store.t("player.shuffle_all"))
                            .number_of_lines(1)
                            .text_size(px(font_size::BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::fg),
                    )
            }));
    }

    view()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_2p5()
        .child(primary_button(move || store.t("player.play_all"), on_play).disabled(disabled))
        .children(on_shuffle.map(|shuf| {
            ghost_button(move || store.t("player.shuffle_all"), move || shuf()).disabled(disabled)
        }))
}

/// Detail header banner supporting a custom subtitle view (e.g. clickable
/// multi-artist links on Album pages or publisher links on Podcast pages).
#[allow(clippy::too_many_arguments)]
pub(crate) fn detail_hero_custom_subtitle(
    store: AppStore,
    form: DeviceForm,
    badge: SharedString,
    title: SharedString,
    subtitle_view: Option<View>,
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
        DeviceForm::Phone => 184.0,
    };
    let art_radius = if circular_art {
        radius::PILL
    } else {
        radius::XL
    };
    let hero_uri = sized_artwork_url(&image_url, ARTWORK_SIZE_HERO);
    let is_phone = form.is_phone();
    let art_box = Elevated::elevation(
        artwork_box(hero_uri, seed, art_size, art_radius),
        if is_phone {
            Elevation::Subtle
        } else {
            Elevation::Card
        },
    );

    let info_col = view()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(if is_phone { 6.0 } else { 8.0 }))
        .when(is_phone, |v| v.w_full().items_center())
        .children((!badge.is_empty()).then(|| {
            view()
                .px_2p5()
                .py_1()
                .self_start()
                .when(is_phone, |v| v.self_center())
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
                .when(is_phone, |t| t.text_center())
                .text_size(px(if is_phone {
                    font_size::TITLE_LG
                } else {
                    font_size::DISPLAY
                }))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg),
        )
        .children(subtitle_view.map(|sub| {
            view()
                .when(is_phone, |v| v.w_full().items_center().justify_center())
                .child(sub)
        }))
        .children((!meta.is_empty()).then(|| {
            text(meta)
                .number_of_lines(2)
                .when(is_phone, |t| t.text_center())
                .text_size(px(font_size::CAPTION))
                .text_color(theme::muted_fg)
        }))
        .children((!description.is_empty()).then(|| {
            text(description)
                .number_of_lines(3)
                .when(is_phone, |t| t.text_center())
                .text_size(px(font_size::SUBTITLE))
                .text_color(theme::muted_fg)
        }))
        .child(view().w_full().pt_2().child(actions));

    // Phone Level-2 pages already provide native `<` back chrome in `phone_top_header`.
    let back_bar = (!is_phone).then(|| {
        when(
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
        )
    });

    let body = if is_phone {
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
            .p(px(if is_phone { 16.0 } else { 24.0 }))
            .gap(px(if is_phone { 14.0 } else { 16.0 }))
            .rounded(px(radius::XL))
            .overflow_hidden()
            .bg_linear_to_br()
            .gradient_from(theme::artwork_tint(accent))
            .gradient_to(theme::card())
            .border_1()
            .border_color(theme::artwork_wash(accent, 0.25)),
        Elevation::Card,
    )
    .children(back_bar)
    .child(body)
}

/// Renders a list of `Browse` sections (tracks, playlists, albums, artists,
/// stations, podcasts, browse links) cleanly across any music source with
/// `{size}`-expanded artwork and virtualized track lists.
pub(crate) fn render_browse_sections(
    store: AppStore,
    form: DeviceForm,
    sections: Vec<Section>,
) -> View {
    let tasks = PagesOwner::capture();
    let is_phone = form.is_phone();
    let mut container = view().w_full().gap(px(if is_phone { 20.0 } else { 24.0 }));

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
        let subtitle = (!sec.subtitle.is_empty()).then(|| SharedString::from(sec.subtitle.clone()));

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
                        .image_url(sized_artwork_url(&pl.image_url, ARTWORK_SIZE_CARD))
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
                                tasks.spawn(async move {
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
            sec_view = sec_view.child(phone_carousel_or_grid(store, form, cards));
        }

        if !sec.tracks.is_empty() {
            let queue = Rc::new(sec.tracks.clone());
            sec_view = sec_view.child(virtual_track_list(
                store, form, queue, true, true, None, None,
            ));
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
                        });
                    if !alb.released.is_empty() {
                        card = card.badge(alb.released.clone());
                    }
                    card.into_element()
                })
                .collect();
            sec_view = sec_view.child(phone_carousel_or_grid(store, form, cards));
        }

        if !sec.stations.is_empty() {
            let cards: Vec<AnyElement> = sec
                .stations
                .iter()
                .map(|st| {
                    let st_press = st.clone();
                    let st_play = st.clone();
                    media_card(st.name.clone(), st.description.clone())
                        .image_url(sized_artwork_url(&st.image_url, ARTWORK_SIZE_CARD))
                        .seed(st.id.clone())
                        .badge(store.t("station.live_mix"))
                        .on_press(move || store.play_station(st_press.clone()))
                        .on_play(move || store.play_station(st_play.clone()))
                        .into_element()
                })
                .collect();
            sec_view = sec_view.child(phone_carousel_or_grid(store, form, cards));
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
            sec_view = sec_view.child(phone_carousel_or_grid(store, form, cards));
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
            sec_view = sec_view.child(phone_carousel_or_grid(store, form, cards));
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
                            sized_artwork_url(&link.image_url, ARTWORK_SIZE_THUMB),
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

/// Refresh action button that swaps its static icon for an animated
/// `loading_spinner` while `loading` is active.
pub(crate) fn refresh_pill_button(
    store: AppStore,
    form: DeviceForm,
    loading: Signal<bool>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    button()
        .on_press(on_press)
        .disabled(move || loading.get())
        .min_h(px(form.min_touch_target()))
        .px_3p5()
        .py_1p5()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(radius::CONTROL))
        .bg(theme::secondary)
        .border_1()
        .border_color(theme::border)
        .transition_all()
        .duration(move || motion::transition_ms(140.0))
        .hover(|s| s.bg(theme::accent))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .disabled_style(|s| s.opacity(0.65))
        .child(
            when(
                move || loading.get(),
                || crate::ui::loading_spinner(14.0, colors::PRIMARY),
            )
            .otherwise(|| icon(Icon::Refresh, 14.0, colors::MUTED_FOREGROUND)),
        )
        .child(
            text(move || {
                if loading.get() {
                    store.t("common.loading")
                } else {
                    store.t("common.refresh")
                }
            })
            .number_of_lines(1)
            .text_size(px(font_size::BODY))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::fg),
        )
}

/// Compact, centered pagination footer rendering a touch-sized `ghost_button`
/// (`Load More` / `Loading more...`) whenever additional pages exist or are
/// currently loading, collapsing to an empty container once all items are loaded.
pub(crate) fn pagination_footer(
    store: AppStore,
    form: DeviceForm,
    test_id: &'static str,
    has_more: impl Fn() -> bool + Copy + 'static,
    loading_more: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> View {
    let on_press = Rc::new(on_press);
    let min_h = px(form.min_touch_target());
    let is_phone = form.is_phone();

    view().w_full().child(when(
        move || has_more() || loading_more(),
        move || {
            let press = Rc::clone(&on_press);
            view()
                .w_full()
                .pt_3()
                .pb_2()
                .flex_row()
                .items_center()
                .justify_center()
                .child(
                    ghost_button(
                        move || {
                            if loading_more() {
                                store.t("common.loading_more")
                            } else {
                                store.t("common.load_more")
                            }
                        },
                        move || press(),
                    )
                    .test_id(test_id)
                    .min_w(min_h)
                    .min_h(min_h)
                    .disabled(loading_more)
                    .when(is_phone, |b| b.rounded_full()),
                )
        },
    ))
}
