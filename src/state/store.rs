//! Reactive `AppStore` (`#[derive(Clone, Copy)]`) holding fine-grained NSH
//! `Signal`s and coordinating `LocalMusicSource`, `PluginManager`,
//! `AudioEngine`, and `ArtworkLoader`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsh_core::file_dialog::{FileFilter, OpenDialog, SaveDialog};
use nsh_core::prelude::*;
use nsh_music_audio::{
    AudioBackendCapabilities, AudioEngine, AudioEngineConfig, PlaybackSnapshot, PlaybackStatus,
    SPECTRUM_BANDS, audio_backend_capabilities,
};
use nsh_music_core::{
    i18n::{I18n, Locale},
    local::LocalMusicSource,
    lyrics::{ActiveLyric, ParsedLyrics, parse_lyrics},
    model::{
        Account, Album, AlbumPage, ArtistPage, Browse, Comment, PlayerStyle, Playlist, Quality,
        RemoteDevice, RepeatMode, SearchKind, SearchResult, Show, ShowPage, SignInPrompt,
        SourceInfo, StageStyle, Station, Track,
    },
    session::PlaybackSession,
    settings::{AppSettings, ThemeMode},
    source::{MusicSource, RequestKind, SourceCompat},
};
use nsh_music_plugin::{PendingPluginInstall, PluginManager, PluginSummary};

use crate::state::artwork::ArtworkLoader;
use crate::ui::{overlays::ToastController, theme};

const CAPABILITY_KEYS: &[&str] = &[
    "home",
    "browse",
    "search",
    "suggest",
    "playlists",
    "playlist",
    "album",
    "artist",
    "liked",
    "isLiked",
    "like",
    "likeEpisode",
    "recent",
    "played",
    "createPlaylist",
    "renamePlaylist",
    "deletePlaylist",
    "addToPlaylist",
    "removeFromPlaylist",
    "movePlaylistTrack",
    "savePlaylist",
    "followArtist",
    "saveAlbum",
    "savedAlbums",
    "shows",
    "show",
    "subscribeShow",
    "stations",
    "stationNext",
    "comments",
    "likeComment",
    "postComment",
    "lyrics",
    "stream",
    "account",
    "signIn",
    "signOut",
    "qrSignIn",
    "remoteDevices",
    "remoteUse",
    "remotePlay",
];

/// Top-level navigation destinations across PC, Pad, and Phone shells.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Route {
    #[default]
    Home,
    Browse {
        page: String,
        title: String,
    },
    Search,
    Library,
    Liked,
    Playlists,
    PlaylistDetail {
        id: String,
    },
    AlbumDetail {
        id: String,
    },
    ArtistDetail {
        id: String,
    },
    Radio,
    Podcasts,
    ShowDetail {
        id: String,
    },
    Recent,
    Local,
    Settings,
    Plugins,
}

impl Route {
    /// Stable navigation key for NSH navigation stacks.
    pub fn key(&self) -> String {
        match self {
            Self::Home => "home".to_owned(),
            Self::Browse { page, .. } => format!("browse:{page}"),
            Self::Search => "search".to_owned(),
            Self::Library => "library".to_owned(),
            Self::Liked => "liked".to_owned(),
            Self::Playlists => "playlists".to_owned(),
            Self::PlaylistDetail { id } => format!("playlist:{id}"),
            Self::AlbumDetail { id } => format!("album:{id}"),
            Self::ArtistDetail { id } => format!("artist:{id}"),
            Self::Radio => "radio".to_owned(),
            Self::Podcasts => "podcasts".to_owned(),
            Self::ShowDetail { id } => format!("show:{id}"),
            Self::Recent => "recent".to_owned(),
            Self::Local => "local".to_owned(),
            Self::Settings => "settings".to_owned(),
            Self::Plugins => "plugins".to_owned(),
        }
    }

    /// Default i18n key for this route's page title.
    pub fn title_key(&self) -> &'static str {
        match self {
            Self::Home => "nav.home",
            Self::Browse { .. } => "nav.browse",
            Self::Search => "nav.search",
            Self::Library => "nav.library",
            Self::Liked => "nav.liked",
            Self::Playlists => "nav.playlists",
            Self::PlaylistDetail { .. } => "nav.playlists",
            Self::AlbumDetail { .. } => "nav.albums",
            Self::ArtistDetail { .. } => "nav.artists",
            Self::Radio => "nav.radio",
            Self::Podcasts => "nav.podcasts",
            Self::ShowDetail { .. } => "nav.podcasts",
            Self::Recent => "nav.recent",
            Self::Local => "nav.local",
            Self::Settings => "nav.settings",
            Self::Plugins => "nav.plugins",
        }
    }
}

/// Centerpiece visual presentation on the Now Playing stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NowPlayingMode {
    #[default]
    Cover,
    Vinyl,
    Cassette,
}

impl From<PlayerStyle> for NowPlayingMode {
    fn from(style: PlayerStyle) -> Self {
        match style {
            PlayerStyle::Cover => Self::Cover,
            PlayerStyle::Vinyl => Self::Vinyl,
            PlayerStyle::Cassette => Self::Cassette,
        }
    }
}

impl From<NowPlayingMode> for PlayerStyle {
    fn from(mode: NowPlayingMode) -> Self {
        match mode {
            NowPlayingMode::Cover => Self::Cover,
            NowPlayingMode::Vinyl => Self::Vinyl,
            NowPlayingMode::Cassette => Self::Cassette,
        }
    }
}

/// Lyrics visualizer stage styles on the Now Playing screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NowPlayingStageStyle {
    Classic,
    #[default]
    Flow,
    Steps,
    Tilt,
    Wall,
}

impl From<StageStyle> for NowPlayingStageStyle {
    fn from(style: StageStyle) -> Self {
        match style {
            StageStyle::Flow => Self::Flow,
            StageStyle::Steps => Self::Steps,
            StageStyle::Tilt => Self::Tilt,
        }
    }
}

impl NowPlayingStageStyle {
    pub fn to_core(self) -> StageStyle {
        match self {
            Self::Steps => StageStyle::Steps,
            Self::Tilt => StageStyle::Tilt,
            Self::Classic | Self::Flow | Self::Wall => StageStyle::Flow,
        }
    }
}

/// Side / bottom panel selection inside Now Playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NowPlayingPanel {
    #[default]
    Lyrics,
    Queue,
    Comments,
    Podcast,
}

/// Active app-level modal dialog or mobile sheet.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActiveModal {
    #[default]
    None,
    InstallPlugin,
    ConfirmPluginInstall {
        pending: PendingPluginInstall,
    },
    PluginConfig {
        plugin_id: String,
    },
    QrSignIn {
        source_id: String,
        prompt: SignInPrompt,
    },
    WebSignIn {
        source_id: String,
        url: String,
        title: String,
    },
    CreatePlaylist {
        initial_track_id: Option<String>,
    },
    RenamePlaylist {
        id: String,
        current_name: String,
    },
    SourceSwitcher,
}

/// Source entry exposed in source switchers, sidebars, and plugin management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub info: SourceInfo,
    pub compat: SourceCompat,
    pub is_local: bool,
    pub account: Option<Account>,
    pub capabilities: HashSet<String>,
}

type PendingSignInSlot = Arc<Mutex<Option<(String, SignInPrompt)>>>;

/// Copyable reactive application store owning all fine-grained UI signals and
/// coordinating backend services (`LocalMusicSource`, `PluginManager`,
/// `AudioEngine`, `ArtworkLoader`).
#[derive(Clone, Copy)]
pub struct AppStore {
    // Navigation & shell
    pub route: Signal<Route>,
    pub nav_history: Signal<Vec<Route>>,
    pub nav_forward: Signal<Vec<Route>>,
    pub page_scrolled: Signal<bool>,
    pub sidebar_collapsed: Signal<bool>,
    pub mobile_drawer_open: Signal<bool>,

    // Sources & plugins
    pub active_source_id: Signal<String>,
    pub sources: Signal<Vec<SourceEntry>>,
    pub plugins: Signal<Vec<PluginSummary>>,
    pub plugin_busy: Signal<bool>,
    pub plugin_error: Signal<Option<String>>,
    pub backend_caps: Signal<AudioBackendCapabilities>,

    // Playback & audio
    pub playback_status: Signal<PlaybackStatus>,
    pub is_playing: Signal<bool>,
    pub current_track: Signal<Option<Track>>,
    pub queue: Signal<Vec<Track>>,
    pub current_index: Signal<Option<usize>>,
    pub position_ms: Signal<u64>,
    pub duration_ms: Signal<u64>,
    pub buffered_ms: Signal<u64>,
    pub volume: Signal<f32>,
    pub muted: Signal<bool>,
    pub shuffle: Signal<bool>,
    pub repeat: Signal<RepeatMode>,
    pub spectrum: Signal<[f32; SPECTRUM_BANDS]>,
    pub current_artwork_uri: Signal<SharedString>,
    pub current_accent: Signal<Hsla>,
    pub remote_devices: Signal<Vec<RemoteDevice>>,
    pub active_remote_device: Signal<Option<String>>,
    pub cache_used_bytes: Signal<u64>,

    // Lyrics & comments
    pub lyrics: Signal<ParsedLyrics>,
    pub active_lyric: Signal<Option<ActiveLyric>>,
    pub show_translation: Signal<bool>,
    pub show_romaji: Signal<bool>,
    pub comments: Signal<Vec<Comment>>,
    pub comments_total: Signal<usize>,

    // Now Playing overlay
    pub now_playing_open: Signal<bool>,
    pub now_playing_mode: Signal<NowPlayingMode>,
    pub stage_style: Signal<NowPlayingStageStyle>,
    pub now_playing_panel: Signal<NowPlayingPanel>,
    pub stage_fullscreen: Signal<bool>,

    // Search
    pub search_query: Signal<String>,
    pub search_tab: Signal<SearchKind>,
    pub search_results: Signal<SearchResult>,
    pub search_loading: Signal<bool>,
    pub recent_searches: Signal<Vec<String>>,
    pub search_suggestions: Signal<Vec<String>>,
    pub search_suggest_open: Signal<bool>,

    // Page catalogs & details
    pub home_data: Signal<Browse>,
    pub browse_data: Signal<Browse>,
    pub browse_loading: Signal<bool>,
    pub playlists: Signal<Vec<Playlist>>,
    pub active_playlist: Signal<Option<Playlist>>,
    pub active_album: Signal<Option<AlbumPage>>,
    pub active_artist: Signal<Option<ArtistPage>>,
    pub liked_tracks: Signal<Vec<Track>>,
    pub liked_ids: Signal<HashSet<String>>,
    pub recent_tracks: Signal<Vec<Track>>,
    pub saved_albums: Signal<Vec<Album>>,
    pub stations: Signal<Vec<Station>>,
    pub podcasts: Signal<Vec<Show>>,
    pub active_show: Signal<Option<ShowPage>>,

    // Settings, locale, theme, modals, toasts
    pub settings: Signal<AppSettings>,
    pub locale: Signal<Locale>,
    pub theme_override: Signal<ThemeMode>,
    pub active_modal: Signal<ActiveModal>,
    pub toasts: ToastController,

    // Non-reactive backend handles (`Copy` via `Stored`)
    local: Stored<Arc<LocalMusicSource>>,
    plugin_mgr: Stored<Arc<PluginManager>>,
    audio: Stored<AudioEngine>,
    artwork: Stored<ArtworkLoader>,
    session: Stored<PlaybackSession>,
    last_track_id: Stored<Option<String>>,
    last_snapshot: Stored<Option<PlaybackSnapshot>>,
    active_station_id: Stored<Option<String>>,
    pending_sign_in_prompt: Stored<PendingSignInSlot>,
    signing_in_source_id: Stored<Option<String>>,
    search_suggest_gen: Stored<u64>,
    root_owner: Owner,
}

/// Provides an [`AppStore`] to the current NSH component subtree.
pub fn provide_store(store: AppStore) {
    provide_context(store);
}

/// Reads the [`AppStore`] provided by the root component.
pub fn use_store() -> AppStore {
    use_context::<AppStore>().expect("root App component must call provide_store(AppStore::new())")
}

impl Default for AppStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AppStore {
    /// Initializes the store, loads persisted settings/session, mounts
    /// `LocalMusicSource`, `PluginManager`, `AudioEngine`, and `ArtworkLoader`,
    /// starts deep-link handling, and runs the audio engine sync ticker.
    pub fn new() -> Self {
        let mut settings = AppSettings::load().unwrap_or_default();
        let session = PlaybackSession::load().unwrap_or_default();

        let local = Arc::new(LocalMusicSource::with_default_storage(
            settings.local_folders.clone(),
        ));
        for f in local.folders() {
            settings.add_local_folder(f);
        }

        let plugin_mgr = Arc::new(
            PluginManager::with_default_dirs()
                .or_else(|_| {
                    let fallback = std::env::temp_dir().join("nsh-music-plugins");
                    PluginManager::new(fallback.join("installed"), fallback.join("data"))
                })
                .expect("initialize plugin manager"),
        );
        plugin_mgr.set_locale(settings.locale.code());
        let pending_sign_in_prompt: PendingSignInSlot = Arc::new(Mutex::new(None));
        {
            let slot = Arc::clone(&pending_sign_in_prompt);
            plugin_mgr.set_sign_in_prompt(Some(Arc::new(move |prompt: SignInPrompt| {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some((String::new(), prompt));
                }
            })));
        }
        let _ = plugin_mgr.load_all();

        let mut audio_cfg = AudioEngineConfig {
            initial_volume: session.volume,
            replay_gain_enabled: settings.replay_gain,
            crossfade_ms: settings.clamped_crossfade_ms(),
            quality: settings.audio_quality,
            ..AudioEngineConfig::default()
        };
        audio_cfg.cache.max_bytes = settings.cache_max_mb.max(128) * 1024 * 1024;
        let audio = AudioEngine::new(audio_cfg)
            .or_else(|_| AudioEngine::default_engine())
            .expect("initialize audio engine");

        {
            let local_ref = Arc::clone(&local);
            let mgr_ref = Arc::clone(&plugin_mgr);
            audio.set_stream_resolver(Some(Arc::new(move |track, quality| {
                if track.source.is_empty() || track.source == LocalMusicSource::SOURCE_ID {
                    local_ref.stream(&track.id, quality)
                } else if let Some(plugin) = mgr_ref.get(&track.source) {
                    plugin.stream(&track.id, quality)
                } else {
                    local_ref.stream(&track.id, quality)
                }
            })));
        }

        let artwork = ArtworkLoader::default_location();
        let initial_cache_bytes = audio.cache_used_bytes();
        let initial_locale = settings.locale;
        let initial_theme = settings.theme_mode;
        let initial_mode = NowPlayingMode::from(settings.default_player_style);
        let initial_stage = NowPlayingStageStyle::from(settings.default_stage_style);
        let initial_source_id = if session.active_source_id.is_empty() {
            LocalMusicSource::SOURCE_ID.to_owned()
        } else {
            session.active_source_id.clone()
        };

        let store = Self {
            route: signal(Route::Home),
            nav_history: signal(vec![Route::Home]),
            nav_forward: signal(Vec::new()),
            page_scrolled: signal(false),
            sidebar_collapsed: signal(false),
            mobile_drawer_open: signal(false),

            active_source_id: signal(initial_source_id),
            sources: signal(Vec::new()),
            plugins: signal(Vec::new()),
            plugin_busy: signal(false),
            plugin_error: signal(None),
            backend_caps: signal(audio_backend_capabilities()),

            playback_status: signal(PlaybackStatus::Stopped),
            is_playing: signal(false),
            current_track: signal(session.current_track().cloned()),
            queue: signal(session.queue.clone()),
            current_index: signal(session.current_index),
            position_ms: signal(session.position_ms.max(0) as u64),
            duration_ms: signal(
                session
                    .current_track()
                    .map(|t| t.duration.max(0) as u64)
                    .unwrap_or(0),
            ),
            buffered_ms: signal(0),
            volume: signal(session.volume),
            muted: signal(session.muted),
            shuffle: signal(session.shuffle),
            repeat: signal(session.repeat),
            spectrum: signal([0.0; SPECTRUM_BANDS]),
            current_artwork_uri: signal(SharedString::default()),
            current_accent: signal(theme::default_artwork_accent()),
            remote_devices: signal(Vec::new()),
            active_remote_device: signal(None),
            cache_used_bytes: signal(initial_cache_bytes),

            lyrics: signal(ParsedLyrics::default()),
            active_lyric: signal(None),
            show_translation: signal(true),
            show_romaji: signal(false),
            comments: signal(Vec::new()),
            comments_total: signal(0),

            now_playing_open: signal(false),
            now_playing_mode: signal(initial_mode),
            stage_style: signal(initial_stage),
            now_playing_panel: signal(NowPlayingPanel::Lyrics),
            stage_fullscreen: signal(false),

            search_query: signal(String::new()),
            search_tab: signal(SearchKind::Tracks),
            search_results: signal(SearchResult::default()),
            search_loading: signal(false),
            recent_searches: signal(session.recent_searches.clone()),
            search_suggestions: signal(Vec::new()),
            search_suggest_open: signal(false),

            home_data: signal(Browse::default()),
            browse_data: signal(Browse::default()),
            browse_loading: signal(false),
            playlists: signal(Vec::new()),
            active_playlist: signal(None),
            active_album: signal(None),
            active_artist: signal(None),
            liked_tracks: signal(Vec::new()),
            liked_ids: signal(HashSet::new()),
            recent_tracks: signal(Vec::new()),
            saved_albums: signal(Vec::new()),
            stations: signal(Vec::new()),
            podcasts: signal(Vec::new()),
            active_show: signal(None),

            settings: signal(settings),
            locale: signal(initial_locale),
            theme_override: signal(initial_theme),
            active_modal: signal(ActiveModal::None),
            toasts: ToastController::new(),

            local: stored(local),
            plugin_mgr: stored(plugin_mgr),
            audio: stored(audio),
            artwork: stored(artwork),
            session: stored(session),
            last_track_id: stored(None),
            last_snapshot: stored(None),
            active_station_id: stored(None),
            pending_sign_in_prompt: stored(pending_sign_in_prompt),
            signing_in_source_id: stored(None),
            search_suggest_gen: stored(0),
            root_owner: Owner::current(),
        };

        store.apply_theme_mode(initial_theme);
        store.refresh_sources();
        store.hydrate_active_source_sync();
        store.refresh_cache_stats();

        if let Some(track) = store.current_track.get_untracked() {
            store.load_track_extras(&track);
        }

        // Sync playback snapshots & 16-band spectrum from AudioEngine on a 60ms interval.
        set_interval(Duration::from_millis(60), move || {
            store.sync_from_audio_engine();
        });

        // Listen for `mygo-music://install-plugin?repo=...` and `nsh-music://...` links.
        store.spawn_app(async move {
            loop {
                let url = next_url_scheme_open().await;
                if url.is_empty() {
                    break;
                }
                store.handle_deep_link(&url);
            }
        });

        store
    }

    fn spawn_app(&self, future: impl std::future::Future<Output = ()> + 'static) {
        self.root_owner.run(|| {
            spawn(future);
        });
    }

    /// Translates `key` in the current reactive locale.
    pub fn t(&self, key: &str) -> SharedString {
        let loc = self.locale.get();
        SharedString::from(I18n::t(loc, key).to_owned())
    }

    /// Formats a localized string template with `{name}` substitutions.
    pub fn t_fmt(&self, key: &str, args: &[(&str, &str)]) -> SharedString {
        let loc = self.locale.get();
        SharedString::from(I18n::t_fmt(loc, key, args))
    }

    /// Returns the underlying `LocalMusicSource`.
    pub fn local_source(&self) -> Arc<LocalMusicSource> {
        self.local.get()
    }

    /// Returns the underlying `PluginManager`.
    pub fn plugin_manager(&self) -> Arc<PluginManager> {
        self.plugin_mgr.get()
    }

    /// Returns the underlying `AudioEngine`.
    pub fn audio_engine(&self) -> AudioEngine {
        self.audio.get()
    }

    /// Returns the underlying `ArtworkLoader`.
    pub fn artwork_loader(&self) -> ArtworkLoader {
        self.artwork.get()
    }

    /// Returns the localized display name for `entry` (`Local Files` localized via i18n).
    pub fn source_display_name(&self, info: &SourceInfo) -> SharedString {
        if info.id == LocalMusicSource::SOURCE_ID || info.id.is_empty() {
            self.t("source.local")
        } else {
            SharedString::from(info.name.clone())
        }
    }

    /// Localized label for the currently active source.
    pub fn active_source_name(&self) -> SharedString {
        self.active_source_entry()
            .map(|e| self.source_display_name(&e.info))
            .unwrap_or_else(|| self.t("source.local"))
    }

    /// Returns the `SourceEntry` metadata for the currently selected source.
    pub fn active_source_entry(&self) -> Option<SourceEntry> {
        let id = self.active_source_id.get();
        self.sources
            .with(|list| list.iter().find(|s| s.info.id == id).cloned())
    }

    /// Reactively checks whether the active source supports `cap` (e.g. `"stations"`, `"comments"`).
    pub fn active_can(&self, cap: &str) -> bool {
        let id = self.active_source_id.get();
        self.sources.with(|list| {
            list.iter()
                .find(|s| s.info.id == id)
                .is_some_and(|e| e.capabilities.contains(cap))
        })
    }

    /// Resolves the currently active [`MusicSource`].
    pub fn active_source(&self) -> Arc<dyn MusicSource> {
        let id = self.active_source_id.get_untracked();
        self.source_by_id(&id)
    }

    /// Resolves a [`MusicSource`] by ID, falling back to `LocalMusicSource`.
    pub fn source_by_id(&self, source_id: &str) -> Arc<dyn MusicSource> {
        if source_id.is_empty() || source_id == LocalMusicSource::SOURCE_ID {
            return self.local.get() as Arc<dyn MusicSource>;
        }
        if let Some(plugin) = self.plugin_mgr.with(|m| m.get(source_id)) {
            return plugin as Arc<dyn MusicSource>;
        }
        self.local.get() as Arc<dyn MusicSource>
    }

    // -------------------------------------------------------------------------
    // Navigation & Shell
    // -------------------------------------------------------------------------

    /// Navigates to `next`, clearing forward history and loading page data.
    pub fn navigate(&self, next: Route) {
        let current = self.route.get_untracked();
        if current != next {
            batch(|| {
                self.route.set(next.clone());
                self.nav_forward.update(Vec::clear);
                self.page_scrolled.set(false);
                self.search_suggest_open.set(false);
                self.nav_history.update(|hist| {
                    if hist.last() != Some(&next) {
                        hist.push(next.clone());
                        if hist.len() > 32 {
                            let overflow = hist.len() - 32;
                            hist.drain(..overflow);
                        }
                    }
                });
            });
        }
        self.mobile_drawer_open.set(false);
        self.load_data_for_route(&next);
    }

    /// Pops one entry from navigation history into the forward stack.
    pub fn go_back(&self) -> bool {
        let current = self.route.get_untracked();
        let prev = self.nav_history.with_untracked(|hist| {
            if hist.len() > 1 {
                hist.get(hist.len() - 2).cloned()
            } else {
                None
            }
        });
        if let Some(target) = prev {
            batch(|| {
                self.nav_history.update(|hist| {
                    if hist.len() > 1 {
                        hist.pop();
                    }
                });
                self.nav_forward.update(|fwd| fwd.push(current));
                self.route.set(target.clone());
                self.page_scrolled.set(false);
                self.search_suggest_open.set(false);
            });
            self.load_data_for_route(&target);
            true
        } else {
            false
        }
    }

    /// Navigates forward along previously popped routes.
    pub fn go_forward(&self) -> bool {
        let next = self.nav_forward.with_untracked(|fwd| fwd.last().cloned());
        if let Some(target) = next {
            batch(|| {
                self.nav_forward.update(|fwd| {
                    fwd.pop();
                });
                self.nav_history.update(|hist| {
                    if hist.last() != Some(&target) {
                        hist.push(target.clone());
                    }
                });
                self.route.set(target.clone());
                self.page_scrolled.set(false);
                self.search_suggest_open.set(false);
            });
            self.load_data_for_route(&target);
            true
        } else {
            false
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.nav_history.with(|h| h.len() > 1)
    }

    pub fn can_go_forward(&self) -> bool {
        self.nav_forward.with(|f| !f.is_empty())
    }

    pub fn toggle_sidebar(&self) {
        self.sidebar_collapsed.update(|c| *c = !*c);
    }

    pub fn set_mobile_drawer_open(&self, open: bool) {
        self.mobile_drawer_open.set(open);
    }

    // -------------------------------------------------------------------------
    // Sources & Plugins
    // -------------------------------------------------------------------------

    /// Rebuilds the `sources` and `plugins` signals and asynchronously refreshes
    /// account status for any loaded plugin supporting `account`.
    pub fn refresh_sources(&self) {
        let existing_accounts: std::collections::HashMap<String, Account> = self
            .sources
            .with_untracked(|list| {
                list.iter()
                    .filter_map(|e| e.account.clone().map(|a| (e.info.id.clone(), a)))
                    .collect()
            });

        let local = self.local.get();
        let local_entry = SourceEntry {
            info: local.info(),
            compat: local.compatibility_info(),
            is_local: true,
            account: None,
            capabilities: probe_capabilities(local.as_ref()),
        };

        let mut list = vec![local_entry];
        let mut account_probes: Vec<(String, Arc<dyn MusicSource>)> = Vec::new();
        let plugin_sources = self.plugin_mgr.with(|m| m.sources());
        for src in plugin_sources {
            let info = src.info();
            let caps = probe_capabilities(src.as_ref());
            if caps.contains("account") {
                account_probes.push((info.id.clone(), Arc::clone(&src) as Arc<dyn MusicSource>));
            }
            let cached_acc = existing_accounts.get(&info.id).cloned();
            list.push(SourceEntry {
                info,
                compat: src.compatibility_info(),
                is_local: false,
                account: cached_acc,
                capabilities: caps,
            });
        }

        let summaries = self.plugin_mgr.with(|m| m.plugins());

        batch(|| {
            self.sources.set(list);
            self.plugins.set(summaries);
        });

        // Probe plugin accounts in parallel off the UI thread.
        if !account_probes.is_empty() {
            let store = *self;
            self.spawn_app(async move {
                let results = blocking(move || {
                    account_probes
                        .into_iter()
                        .map(|(id, src)| (id, src.account().ok().flatten()))
                        .collect::<Vec<_>>()
                })
                .await;
                if let Ok(resolved) = results {
                    store.sources.update(|entries| {
                        for (id, acc) in resolved {
                            if let Some(entry) = entries.iter_mut().find(|e| e.info.id == id) {
                                entry.account = acc;
                            }
                        }
                    });
                }
            });
        }
    }

    /// Switches the active music source and refreshes home/library/catalog data.
    pub fn select_source(&self, source_id: &str) {
        let target = if source_id.is_empty() {
            LocalMusicSource::SOURCE_ID.to_owned()
        } else {
            source_id.to_owned()
        };
        batch(|| {
            self.active_source_id.set(target.clone());
            self.search_suggestions.set(Vec::new());
            self.search_suggest_open.set(false);
            self.search_results.set(SearchResult::default());
            self.active_playlist.set(None);
            self.active_album.set(None);
            self.active_artist.set(None);
            self.active_show.set(None);
            self.remote_devices.set(Vec::new());
            self.active_remote_device.set(None);
        });
        if self.now_playing_panel.get_untracked() == NowPlayingPanel::Comments
            && !self.active_can("comments")
        {
            self.now_playing_panel.set(NowPlayingPanel::Lyrics);
        }
        self.session.with(|s| {
            s.active_source_id = target;
            let _ = s.save();
        });
        self.hydrate_active_source_async();
    }

    pub fn refresh_home(&self) {
        self.hydrate_active_source_async();
    }

    pub fn refresh_browse(&self, page: &str, title: &str) {
        let src = self.active_source();
        let page_str = page.to_owned();
        let fallback_title = title.to_owned();
        self.browse_loading.set(true);
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || {
                if page_str.is_empty() {
                    src.home()
                } else {
                    src.browse(&page_str)
                }
            })
            .await;
            store.browse_loading.set(false);
            if let Ok(Ok(mut browse)) = res {
                if browse.title.is_empty() && !fallback_title.is_empty() {
                    browse.title = fallback_title;
                }
                store.browse_data.set(browse);
            }
        });
    }

    /// Installs a plugin synchronously (without confirmation UI), returning the display name.
    pub fn install_plugin(&self, spec: &str) -> Result<String, String> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("empty plugin source".to_owned());
        }
        let path = Path::new(trimmed);
        let res = self.plugin_mgr.with(|mgr| {
            if path.is_dir() {
                mgr.install_from_dir(path)
            } else if path.is_file() && trimmed.to_ascii_lowercase().ends_with(".zip") {
                mgr.install_from_zip(path)
            } else {
                mgr.install_from_github(trimmed)
            }
        });

        match res {
            Ok(installed) => {
                let name = installed.info().name;
                self.refresh_sources();
                self.toasts
                    .success(self.t_fmt("plugins.installed_fmt", &[("name", &name)]));
                Ok(name)
            }
            Err(err) => {
                let msg = err.to_string();
                self.toasts.error(msg.clone());
                Err(msg)
            }
        }
    }

    /// Opens the native folder picker to select and stage a local plugin folder for install preview.
    pub fn pick_and_stage_plugin_folder(&self) {
        let store = *self;
        let title = self.t("plugins.from_folder").to_string();
        let btn = self.t("common.open").to_string();
        self.spawn_app(async move {
            match pick_paths(OpenDialog::folders().title(title).button_label(btn)).await {
                Ok(Some(paths)) => {
                    if let Some(first) = paths.into_iter().next() {
                        store.stage_local_plugin_dir(first);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    store.toasts.error(err);
                }
            }
        });
    }

    /// Opens the native file picker to select and stage a `.zip` plugin archive for install preview.
    pub fn pick_and_stage_plugin_zip(&self) {
        let store = *self;
        let title = self.t("plugins.from_zip").to_string();
        let btn = self.t("common.open").to_string();
        self.spawn_app(async move {
            match pick_paths(
                OpenDialog::files()
                    .filter(FileFilter::new("ZIP", ["zip"]))
                    .title(title)
                    .button_label(btn),
            )
            .await
            {
                Ok(Some(paths)) => {
                    if let Some(first) = paths.into_iter().next() {
                        store.stage_zip_plugin(first);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    store.toasts.error(err);
                }
            }
        });
    }

    /// Stages a local plugin directory off the UI thread and opens the permission confirmation modal.
    pub fn stage_local_plugin_dir(&self, dir: PathBuf) {
        if self.plugin_busy.get_untracked() {
            return;
        }
        batch(|| {
            self.plugin_busy.set(true);
            self.plugin_error.set(None);
        });
        let mgr = self.plugin_mgr.get();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || mgr.stage_from_dir(&dir)).await;
            store.plugin_busy.set(false);
            match res {
                Ok(Ok(pending)) => {
                    store.open_modal(ActiveModal::ConfirmPluginInstall { pending });
                }
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
                Err(err) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
            }
        });
    }

    /// Stages a local `.zip` plugin archive off the UI thread and opens the confirmation modal.
    pub fn stage_zip_plugin(&self, zip_path: PathBuf) {
        if self.plugin_busy.get_untracked() {
            return;
        }
        batch(|| {
            self.plugin_busy.set(true);
            self.plugin_error.set(None);
        });
        let mgr = self.plugin_mgr.get();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || mgr.stage_from_zip(&zip_path)).await;
            store.plugin_busy.set(false);
            match res {
                Ok(Ok(pending)) => {
                    store.open_modal(ActiveModal::ConfirmPluginInstall { pending });
                }
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
                Err(err) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
            }
        });
    }

    /// Downloads and stages a GitHub plugin spec (or detects local path/zip if typed) for confirmation.
    pub fn stage_github_plugin(&self, spec: &str) {
        let trimmed = spec.trim().to_owned();
        if trimmed.is_empty() || self.plugin_busy.get_untracked() {
            return;
        }
        let path = Path::new(&trimmed);
        if path.is_dir() {
            self.stage_local_plugin_dir(path.to_path_buf());
            return;
        }
        if path.is_file() && trimmed.to_ascii_lowercase().ends_with(".zip") {
            self.stage_zip_plugin(path.to_path_buf());
            return;
        }

        batch(|| {
            self.plugin_busy.set(true);
            self.plugin_error.set(None);
        });
        let mgr = self.plugin_mgr.get();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || mgr.stage_from_github(&trimmed)).await;
            store.plugin_busy.set(false);
            match res {
                Ok(Ok(pending)) => {
                    store.open_modal(ActiveModal::ConfirmPluginInstall { pending });
                }
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
                Err(err) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.toasts.error(msg);
                }
            }
        });
    }

    /// Confirms and activates a staged plugin installation with explicit program grant state.
    pub fn confirm_plugin_install(&self, pending: PendingPluginInstall, granted: bool) {
        if self.plugin_busy.get_untracked() {
            return;
        }
        batch(|| {
            self.plugin_busy.set(true);
            self.plugin_error.set(None);
        });
        let mgr = self.plugin_mgr.get();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || mgr.confirm_staged(&pending, granted)).await;
            store.plugin_busy.set(false);
            match res {
                Ok(Ok(src)) => {
                    let name = src.info().name;
                    store.refresh_sources();
                    store.close_modal();
                    store
                        .toasts
                        .success(store.t_fmt("plugins.installed_fmt", &[("name", &name)]));
                }
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.refresh_sources();
                    store.toasts.error(msg);
                    store.close_modal();
                }
                Err(err) => {
                    let msg = err.to_string();
                    store.plugin_error.set(Some(msg.clone()));
                    store.refresh_sources();
                    store.toasts.error(msg);
                    store.close_modal();
                }
            }
        });
    }

    /// Cancels a staged plugin installation and cleans temporary staging files.
    pub fn cancel_plugin_install(&self, pending: PendingPluginInstall) {
        let mgr = self.plugin_mgr.get();
        self.spawn_app(async move {
            let _ = blocking(move || {
                mgr.cancel_staged(&pending);
            })
            .await;
        });
        self.close_modal();
    }

    pub fn set_plugin_granted(&self, plugin_id: &str, granted: bool) {
        let res = self.plugin_mgr.with(|m| m.set_granted(plugin_id, granted));
        match res {
            Ok(()) => self.refresh_sources(),
            Err(e) => self.toasts.error(e.to_string()),
        }
    }

    pub fn check_plugin_update(&self, plugin_id: &str) {
        let mgr = self.plugin_mgr.get();
        let pid = plugin_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || mgr.check_update(&pid)).await;
            match res {
                Ok(Ok(Some(ver))) => {
                    store.toasts.info(
                        store.t_fmt("plugins.update_available", &[("version", &ver)]),
                    );
                }
                Ok(Ok(None)) => {
                    store.toasts.info(store.t("plugins.up_to_date"));
                }
                Ok(Err(err)) => store.toasts.error(err.to_string()),
                Err(err) => store.toasts.error(err.to_string()),
            }
        });
    }

    pub fn open_plugins_folder(&self) {
        let dir = self.plugin_mgr.with(|m| m.plugins_dir().to_path_buf());
        let store = *self;
        self.spawn_app(async move {
            if let Err(err) = open_path(dir).await {
                store.toasts.error(err);
            }
        });
    }

    pub fn reveal_plugin_folder(&self, plugin_id: &str) {
        let dir = self.plugins.with_untracked(|list| {
            list.iter()
                .find(|p| p.id == plugin_id)
                .map(|p| p.dir.clone())
        });
        if let Some(path) = dir {
            let store = *self;
            self.spawn_app(async move {
                if let Err(err) = reveal_path(path).await {
                    store.toasts.error(err);
                }
            });
        }
    }

    pub fn uninstall_plugin(&self, plugin_id: &str) {
        let display_name = self
            .plugins
            .with_untracked(|list| {
                list.iter()
                    .find(|p| p.id == plugin_id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| plugin_id.to_owned());

        let res = self.plugin_mgr.with(|m| m.uninstall(plugin_id));
        match res {
            Ok(()) => {
                if self.active_source_id.get_untracked() == plugin_id {
                    self.select_source(LocalMusicSource::SOURCE_ID);
                }
                self.refresh_sources();
                self.toasts
                    .info(self.t_fmt("plugins.removed_fmt", &[("name", &display_name)]));
            }
            Err(e) => self.toasts.error(e.to_string()),
        }
    }

    pub fn set_plugin_enabled(&self, plugin_id: &str, enabled: bool) {
        let res = self.plugin_mgr.with(|m| m.set_enabled(plugin_id, enabled));
        match res {
            Ok(()) => {
                self.settings.update(|s| {
                    s.set_plugin_enabled(plugin_id, enabled);
                    let _ = s.save();
                });
                self.refresh_sources();
            }
            Err(e) => self.toasts.error(e.to_string()),
        }
    }

    pub fn reload_plugin(&self, plugin_id: &str) {
        let res = self.plugin_mgr.with(|m| m.reload(plugin_id));
        match res {
            Ok(src) => {
                let name = src.info().name;
                self.refresh_sources();
                self.toasts
                    .success(self.t_fmt("plugins.reloaded_fmt", &[("name", &name)]));
            }
            Err(e) => self.toasts.error(e.to_string()),
        }
    }

    pub fn plugin_setting(&self, plugin_id: &str, key: &str) -> String {
        self.source_by_id(plugin_id).setting(key)
    }

    pub fn set_plugin_setting(&self, plugin_id: &str, key: &str, value: &str) {
        let src = self.source_by_id(plugin_id);
        match src.set_setting(key, value) {
            Ok(()) => {
                self.toasts.success(self.t("common.save"));
            }
            Err(e) => self.toasts.error(e.to_string()),
        }
    }

    pub fn sign_in_source(&self, source_id: &str) {
        let src = self.source_by_id(source_id);
        let id = source_id.to_owned();
        let store = *self;
        self.signing_in_source_id.replace(Some(id.clone()));
        self.pending_sign_in_prompt.with(|m| {
            if let Ok(mut guard) = m.lock() {
                *guard = None;
            }
        });
        self.spawn_app(async move {
            let res = blocking(move || src.sign_in()).await;
            store.signing_in_source_id.replace(None);
            store.pending_sign_in_prompt.with(|m| {
                if let Ok(mut guard) = m.lock() {
                    *guard = None;
                }
            });
            match res {
                Ok(Ok(account)) => {
                    store.refresh_sources();
                    if store.active_source_id.get_untracked() == id {
                        store.hydrate_active_source_async();
                    }
                    store.close_modal();
                    store.toasts.success(store.t_fmt(
                        "plugins.signed_in_as",
                        &[("name", &account.name)],
                    ));
                }
                Ok(Err(err)) => {
                    store.toasts.error(format!("{id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(err.to_string());
                }
            }
        });
    }

    pub fn sign_out_source(&self, source_id: &str) {
        let src = self.source_by_id(source_id);
        let store = *self;
        self.spawn_app(async move {
            let _ = blocking(move || src.sign_out()).await;
            store.refresh_sources();
            store.toasts.info(store.t("plugins.not_signed_in"));
        });
    }

    /// Handles files or directories dragged and dropped onto the application window.
    pub fn handle_dropped_uris(&self, uris: &[String]) {
        for raw in uris {
            let path = url::Url::parse(raw)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .unwrap_or_else(|| PathBuf::from(raw.strip_prefix("file://").unwrap_or(raw)));
            if path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
            {
                self.stage_zip_plugin(path);
                return;
            }
            if path.is_dir() {
                if looks_like_plugin_dir(&path) {
                    self.stage_local_plugin_dir(path);
                    return;
                }
                self.add_local_folder(path);
                return;
            }
        }
    }

    /// Parses `mygo-music://install-plugin?repo=...` and `nsh-music://install-plugin?repo=...` links.
    pub fn handle_deep_link(&self, raw_url: &str) {
        let Ok(parsed) = url::Url::parse(raw_url.trim()) else {
            return;
        };
        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "mygo-music" && scheme != "nsh-music" {
            return;
        }
        let action = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        if action == "install-plugin"
            && let Some((_, repo)) = parsed.query_pairs().find(|(k, _)| k == "repo")
        {
            let trimmed = repo.trim().to_owned();
            if !trimmed.is_empty() {
                self.navigate(Route::Plugins);
                self.stage_github_plugin(&trimmed);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Playback & Queue
    // -------------------------------------------------------------------------

    /// Plays a single track immediately (replacing the queue with `[track]`).
    pub fn play_track(&self, track: Track) {
        self.active_station_id.replace(None);
        self.audio.with(|a| {
            a.set_radio_refill(None);
            a.play_track(track.clone());
        });
        batch(|| {
            self.queue.set(vec![track.clone()]);
            self.current_index.set(Some(0));
            self.current_track.set(Some(track.clone()));
            self.playback_status.set(PlaybackStatus::Playing);
            self.is_playing.set(true);
            self.position_ms.set(0);
            self.duration_ms.set(track.duration.max(0) as u64);
        });
        self.on_track_changed(&track);
    }

    /// Plays `tracks` starting from `start_index`.
    pub fn play_list(&self, tracks: Vec<Track>, start_index: usize) {
        if tracks.is_empty() {
            return;
        }
        let idx = start_index.min(tracks.len() - 1);
        let selected = tracks[idx].clone();
        self.active_station_id.replace(None);
        self.audio.with(|a| {
            a.set_radio_refill(None);
            a.play_queue(tracks.clone(), idx);
        });
        batch(|| {
            self.queue.set(tracks);
            self.current_index.set(Some(idx));
            self.current_track.set(Some(selected.clone()));
            self.playback_status.set(PlaybackStatus::Playing);
            self.is_playing.set(true);
            self.position_ms.set(0);
            self.duration_ms.set(selected.duration.max(0) as u64);
        });
        self.on_track_changed(&selected);
    }

    /// Tunes into a radio station and installs its auto-refill callback.
    pub fn play_station(&self, station: Station) {
        let src = self.active_source();
        let station_id = station.id.clone();
        self.active_station_id.replace(Some(station_id.clone()));

        let refill_src = Arc::clone(&src);
        let refill_id = station_id.clone();
        self.audio.with(|a| {
            a.set_radio_refill(Some(Arc::new(move |_| {
                refill_src.station_next(&refill_id).unwrap_or_default()
            })));
        });

        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.station_next(&station_id)).await;
            if let Ok(Ok(tracks)) = res
                && !tracks.is_empty()
            {
                store.play_list(tracks, 0);
            }
        });
    }

    /// Jumps playback to an existing item in the queue.
    pub fn play_from_queue(&self, index: usize) {
        let q = self.queue.get_untracked();
        if index < q.len() {
            self.play_list(q, index);
        }
    }

    pub fn enqueue(&self, track: Track) {
        self.audio.with(|a| a.enqueue(track.clone()));
        self.queue.update(|q| q.push(track));
        self.toasts.info(self.t("player.add_to_queue"));
    }

    pub fn enqueue_next(&self, track: Track) {
        self.audio.with(|a| a.enqueue_next(track.clone()));
        let insert_at = self
            .current_index
            .get_untracked()
            .map(|i| i + 1)
            .unwrap_or(0);
        self.queue.update(|q| {
            let pos = insert_at.min(q.len());
            q.insert(pos, track);
        });
        self.toasts.info(self.t("player.play_next"));
    }

    pub fn remove_from_queue(&self, index: usize) {
        self.audio.with(|a| a.remove_from_queue(index));
        self.queue.update(|q| {
            if index < q.len() {
                q.remove(index);
            }
        });
    }

    pub fn clear_queue(&self) {
        self.audio.with(|a| a.clear_queue());
        batch(|| {
            self.queue.set(Vec::new());
            self.current_index.set(None);
            self.current_track.set(None);
            self.playback_status.set(PlaybackStatus::Stopped);
            self.is_playing.set(false);
            self.position_ms.set(0);
            self.duration_ms.set(0);
        });
    }

    pub fn toggle_play(&self) {
        let currently_playing = self.is_playing.get_untracked();
        if !currently_playing {
            let status = self.playback_status.get_untracked();
            if matches!(status, PlaybackStatus::Stopped) {
                let q = self.queue.get_untracked();
                let idx = self.current_index.get_untracked().unwrap_or(0);
                if !q.is_empty() {
                    self.play_list(q, idx);
                    return;
                }
            }
        }
        self.audio.with(|a| a.toggle_play());
        let next_playing = !currently_playing;
        batch(|| {
            self.is_playing.set(next_playing);
            self.playback_status.set(if next_playing {
                PlaybackStatus::Playing
            } else {
                PlaybackStatus::Paused
            });
        });
    }

    pub fn play(&self) {
        if !self.is_playing.get_untracked() {
            self.toggle_play();
        }
    }

    pub fn pause(&self) {
        self.audio.with(|a| a.pause());
        batch(|| {
            self.is_playing.set(false);
            self.playback_status.set(PlaybackStatus::Paused);
        });
    }

    pub fn next_track(&self) {
        self.audio.with(|a| a.next_track());
        let q = self.queue.get_untracked();
        if q.is_empty() {
            return;
        }
        let cur = self.current_index.get_untracked().unwrap_or(0);
        let next_idx = if cur + 1 < q.len() { cur + 1 } else { 0 };
        let next_track = q[next_idx].clone();
        batch(|| {
            self.current_index.set(Some(next_idx));
            self.current_track.set(Some(next_track.clone()));
            self.position_ms.set(0);
            self.duration_ms.set(next_track.duration.max(0) as u64);
            self.is_playing.set(true);
            self.playback_status.set(PlaybackStatus::Playing);
        });
        self.on_track_changed(&next_track);
    }

    pub fn prev_track(&self) {
        self.audio.with(|a| a.prev_track());
        let q = self.queue.get_untracked();
        if q.is_empty() {
            return;
        }
        let cur = self.current_index.get_untracked().unwrap_or(0);
        let prev_idx = if cur > 0 { cur - 1 } else { q.len() - 1 };
        let prev_track = q[prev_idx].clone();
        batch(|| {
            self.current_index.set(Some(prev_idx));
            self.current_track.set(Some(prev_track.clone()));
            self.position_ms.set(0);
            self.duration_ms.set(prev_track.duration.max(0) as u64);
            self.is_playing.set(true);
            self.playback_status.set(PlaybackStatus::Playing);
        });
        self.on_track_changed(&prev_track);
    }

    pub fn seek(&self, position_ms: u64) {
        let clamped = position_ms.min(self.duration_ms.get_untracked().max(position_ms));
        self.audio.with(|a| a.seek(clamped));
        self.position_ms.set(clamped);
        self.update_active_lyric(clamped);
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.audio.with(|a| a.set_volume(clamped));
        self.volume.set(clamped);
        self.session.with(|s| {
            s.set_volume(clamped);
            let _ = s.save();
        });
    }

    pub fn toggle_mute(&self) {
        self.audio.with(|a| a.toggle_mute());
        let next = !self.muted.get_untracked();
        self.muted.set(next);
        self.session.with(|s| {
            s.muted = next;
            let _ = s.save();
        });
    }

    pub fn toggle_shuffle(&self) {
        self.audio.with(|a| a.toggle_shuffle());
        let next = !self.shuffle.get_untracked();
        self.shuffle.set(next);
        self.session.with(|s| {
            s.shuffle = next;
            let _ = s.save();
        });
    }

    pub fn cycle_repeat(&self) {
        self.audio.with(|a| a.cycle_repeat());
        let next = match self.repeat.get_untracked() {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.repeat.set(next);
        self.session.with(|s| {
            s.repeat = next;
            let _ = s.save();
        });
    }

    /// Refreshes available remote playback devices from the active source.
    pub fn refresh_remote_devices(&self) {
        if !self.active_can("remoteDevices") {
            return;
        }
        let src = self.active_source();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.remote_devices()).await;
            if let Ok(Ok(devices)) = res {
                let active_id = devices.iter().find(|d| d.active).map(|d| d.id.clone());
                batch(|| {
                    store.remote_devices.set(devices);
                    store.active_remote_device.set(active_id);
                });
            }
        });
    }

    pub fn select_remote_device(&self, device_id: Option<String>) {
        let src = self.active_source();
        self.active_remote_device.set(device_id.clone());
        let store = *self;
        if let Some(id) = device_id {
            self.spawn_app(async move {
                if let Ok(Err(err)) = blocking(move || src.remote_use(&id)).await {
                    store.toasts.error(err.to_string());
                } else {
                    store.refresh_remote_devices();
                }
            });
        }
    }

    // -------------------------------------------------------------------------
    // Likes, Playlists, Comments & Local Library
    // -------------------------------------------------------------------------

    /// Reactive check whether `track_id` is liked.
    pub fn is_liked(&self, track_id: &str) -> bool {
        self.liked_ids.with(|set| set.contains(track_id))
    }

    pub fn toggle_like(&self, track: Track) {
        let id = track.id.clone();
        let now_liked = !self.liked_ids.with_untracked(|s| s.contains(&id));

        batch(|| {
            self.liked_ids.update(|s| {
                if now_liked {
                    s.insert(id.clone());
                } else {
                    s.remove(&id);
                }
            });
            self.liked_tracks.update(|list| {
                list.retain(|t| t.id != id);
                if now_liked {
                    list.insert(0, track.clone());
                }
            });
        });

        self.session.with(|s| {
            let _ = s.toggle_liked_track(track.clone());
            let _ = s.save();
        });

        let src = self.source_by_id(&track.source);
        let track_id = track.id.clone();
        let is_episode = track.is_episode();
        self.spawn_app(async move {
            let _ = blocking(move || {
                if is_episode {
                    src.like_episode(&track_id, now_liked)
                } else {
                    src.like(&track_id, now_liked)
                }
            })
            .await;
        });
    }

    pub fn toggle_like_current(&self) {
        if let Some(track) = self.current_track.get_untracked() {
            self.toggle_like(track);
        }
    }

    pub fn create_playlist(&self, name: &str) {
        self.create_playlist_and_add(name, None);
    }

    pub fn create_playlist_and_add(&self, name: &str, initial_track_id: Option<String>) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let src = self.active_source();
        let name_owned = trimmed.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || -> Result<Playlist, nsh_music_core::model::SourceError> {
                let pl = src.create_playlist(&name_owned)?;
                if let Some(tid) = initial_track_id {
                    let _ = src.add_to_playlist(&pl.id, &[tid]);
                    return Ok(src.playlist(&pl.id).unwrap_or(pl));
                }
                Ok(pl)
            })
            .await;
            if let Ok(Ok(pl)) = res {
                store.playlists.update(|list| {
                    list.retain(|p| p.id != pl.id);
                    list.insert(0, pl.clone());
                });
                store.close_modal();
                store.toasts.success(pl.name);
            }
        });
    }

    pub fn rename_playlist(&self, id: &str, name: &str) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let src = self.active_source();
        let pid = id.to_owned();
        let new_name = trimmed.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let pid_bg = pid.clone();
            let name_bg = new_name.clone();
            let res = blocking(move || src.rename_playlist(&pid_bg, &name_bg)).await;
            if matches!(res, Ok(Ok(()))) {
                batch(|| {
                    store.playlists.update(|list| {
                        if let Some(p) = list.iter_mut().find(|p| p.id == pid) {
                            p.name = new_name.clone();
                        }
                    });
                    store.active_playlist.update(|ap| {
                        if let Some(p) = ap.as_mut().filter(|p| p.id == pid) {
                            p.name = new_name.clone();
                        }
                    });
                });
                store.close_modal();
            }
        });
    }

    pub fn delete_playlist(&self, id: &str) {
        let src = self.active_source();
        let id_owned = id.to_owned();
        self.playlists.update(|list| list.retain(|p| p.id != id_owned));
        let is_viewing = matches!(
            self.route.get_untracked(),
            Route::PlaylistDetail { ref id } if id == &id_owned
        );
        if is_viewing {
            self.active_playlist.set(None);
            if !self.go_back() {
                self.navigate(Route::Library);
            }
        }
        self.spawn_app(async move {
            let _ = blocking(move || src.delete_playlist(&id_owned)).await;
        });
    }

    pub fn add_to_playlist(&self, playlist_id: &str, track: Track) {
        let src = self.active_source();
        let pid = playlist_id.to_owned();
        let tid = track.id.clone();
        let store = *self;
        self.spawn_app(async move {
            let pid_bg = pid.clone();
            let res = blocking(move || src.add_to_playlist(&pid_bg, &[tid])).await;
            if matches!(res, Ok(Ok(()))) {
                store.reload_playlist(&pid);
                store.toasts.success(store.t("playlist.add_to"));
            }
        });
    }

    pub fn remove_from_playlist(&self, playlist_id: &str, track_id: &str) {
        let src = self.active_source();
        let pid = playlist_id.to_owned();
        let tid = track_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let pid_bg = pid.clone();
            let tid_bg = tid.clone();
            let res = blocking(move || src.remove_from_playlist(&pid_bg, &[tid_bg])).await;
            if matches!(res, Ok(Ok(()))) {
                store.reload_playlist(&pid);
            }
        });
    }

    pub fn move_playlist_track(&self, playlist_id: &str, from: usize, to: usize) {
        let src = self.active_source();
        let pid = playlist_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let pid_bg = pid.clone();
            let res = blocking(move || src.move_playlist_track(&pid_bg, from, to)).await;
            if matches!(res, Ok(Ok(()))) {
                store.reload_playlist(&pid);
            }
        });
    }

    fn reload_playlist(&self, playlist_id: &str) {
        let src = self.active_source();
        let pid = playlist_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let pid_bg = pid.clone();
            if let Ok(Ok(pl)) = blocking(move || src.playlist(&pid_bg)).await {
                batch(|| {
                    store.playlists.update(|list| {
                        if let Some(existing) = list.iter_mut().find(|p| p.id == pid) {
                            *existing = pl.clone();
                        }
                    });
                    if matches!(
                        store.route.get_untracked(),
                        Route::PlaylistDetail { ref id } if id == &pid
                    ) {
                        store.active_playlist.set(Some(pl));
                    }
                });
            }
        });
    }

    pub fn toggle_save_album(&self, album_id: &str, on: bool) {
        let src = self.active_source();
        let aid = album_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            if matches!(blocking(move || src.save_album(&aid, on)).await, Ok(Ok(()))) {
                store.active_album.update(|ap| {
                    if let Some(page) = ap {
                        page.album.saved = on;
                    }
                });
            }
        });
    }

    pub fn toggle_follow_artist(&self, artist_id: &str, on: bool) {
        let src = self.active_source();
        let aid = artist_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            if matches!(
                blocking(move || src.follow_artist(&aid, on)).await,
                Ok(Ok(()))
            ) {
                store.active_artist.update(|ap| {
                    if let Some(page) = ap {
                        page.artist.following = on;
                    }
                });
            }
        });
    }

    pub fn post_comment(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(track) = self.current_track.get_untracked() else {
            return;
        };
        let src = self.source_by_id(&track.source);
        let track_id = track.id;
        let body = trimmed.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.post_comment(&track_id, &body)).await;
            if let Ok(Ok(comment)) = res {
                batch(|| {
                    store.comments.update(|list| list.insert(0, comment));
                    store.comments_total.update(|n| *n += 1);
                });
            }
        });
    }

    pub fn toggle_like_comment(&self, comment_id: &str) {
        let Some(track) = self.current_track.get_untracked() else {
            return;
        };
        let mut next_state = false;
        self.comments.update(|list| {
            if let Some(c) = list.iter_mut().find(|c| c.id == comment_id) {
                c.liked = !c.liked;
                next_state = c.liked;
                c.likes = if c.liked {
                    c.likes + 1
                } else {
                    (c.likes - 1).max(0)
                };
            }
        });
        let src = self.source_by_id(&track.source);
        let tid = track.id;
        let cid = comment_id.to_owned();
        self.spawn_app(async move {
            let _ = blocking(move || src.like_comment(&tid, &cid, next_state)).await;
        });
    }

    /// Opens native folder picker (`multiple = true`) and adds chosen music directories.
    pub fn pick_local_music_folders(&self) {
        let store = *self;
        let title = self.t("library.add_folder").to_string();
        let btn = self.t("common.add").to_string();
        self.spawn_app(async move {
            match pick_paths(
                OpenDialog::folders()
                    .multiple(true)
                    .title(title)
                    .button_label(btn),
            )
            .await
            {
                Ok(Some(paths)) if !paths.is_empty() => {
                    store.settings.update(|s| {
                        for p in paths {
                            s.add_local_folder(p);
                        }
                        let _ = s.save();
                    });
                    store.rescan_local_folders();
                }
                Ok(_) => {}
                Err(err) => {
                    store.toasts.error(err);
                }
            }
        });
    }

    pub fn add_local_folder(&self, path: PathBuf) {
        self.settings.update(|s| {
            s.add_local_folder(path);
            let _ = s.save();
        });
        self.rescan_local_folders();
    }

    pub fn remove_local_folder(&self, path: &Path) {
        self.settings.update(|s| {
            s.remove_local_folder(path);
            let _ = s.save();
        });
        self.rescan_local_folders();
    }

    pub fn rescan_local_folders(&self) {
        let folders = self.settings.with_untracked(|s| s.local_folders.clone());
        let local = self.local.get();
        let store = *self;
        self.spawn_app(async move {
            let report = blocking(move || local.scan_folders(&folders)).await;
            if let Ok(rep) = report {
                store.hydrate_active_source_async();
                let count = rep.added_tracks.to_string();
                store
                    .toasts
                    .success(store.t_fmt("plugins.scan_done_fmt", &[("count", &count)]));
            }
        });
    }

    // -------------------------------------------------------------------------
    // Now Playing Stage
    // -------------------------------------------------------------------------

    pub fn open_now_playing(&self) {
        self.now_playing_open.set(true);
    }

    pub fn close_now_playing(&self) {
        self.now_playing_open.set(false);
        self.stage_fullscreen.set(false);
    }

    pub fn set_now_playing_mode(&self, mode: NowPlayingMode) {
        self.now_playing_mode.set(mode);
        self.settings.update(|s| {
            s.default_player_style = PlayerStyle::from(mode);
            let _ = s.save();
        });
    }

    pub fn set_stage_style(&self, style: NowPlayingStageStyle) {
        self.stage_style.set(style);
        self.settings.update(|s| {
            s.default_stage_style = style.to_core();
            let _ = s.save();
        });
    }

    pub fn set_now_playing_panel(&self, panel: NowPlayingPanel) {
        self.now_playing_panel.set(panel);
    }

    pub fn toggle_stage_fullscreen(&self) {
        self.stage_fullscreen.update(|v| *v = !*v);
    }

    pub fn toggle_translation(&self) {
        self.show_translation.update(|v| *v = !*v);
    }

    pub fn toggle_romaji(&self) {
        self.show_romaji.update(|v| *v = !*v);
    }

    // -------------------------------------------------------------------------
    // Search & Debounced Live Suggestions
    // -------------------------------------------------------------------------

    pub fn update_search_query(&self, query: String) {
        self.search_query.set(query.clone());
        let next_gen = self.search_suggest_gen.with(|g| {
            *g += 1;
            *g
        });

        let trimmed = query.trim().to_owned();
        if trimmed.is_empty() || !self.active_can("suggest") {
            batch(|| {
                self.search_suggestions.set(Vec::new());
                self.search_suggest_open.set(false);
            });
            return;
        }

        let src = self.active_source();
        let store = *self;
        self.spawn_app(async move {
            background(async {
                tokio::time::sleep(Duration::from_millis(220)).await;
            })
            .await
            .ok();
            if store.search_suggest_gen.with(|g| *g != next_gen) {
                return;
            }
            if let Ok(Ok(suggestions)) = blocking(move || src.suggest(&trimmed)).await
                && store.search_suggest_gen.with(|g| *g == next_gen)
            {
                let open = !suggestions.is_empty();
                batch(|| {
                    store.search_suggestions.set(suggestions);
                    store.search_suggest_open.set(open);
                });
            }
        });
    }

    pub fn search(&self, query: &str) {
        let trimmed = query.trim().to_owned();
        batch(|| {
            self.search_query.set(trimmed.clone());
            self.search_suggest_open.set(false);
        });
        if trimmed.is_empty() {
            self.search_results.set(SearchResult::default());
            self.search_loading.set(false);
            return;
        }

        self.session.with(|s| {
            s.record_search(&trimmed);
            let _ = s.save();
        });
        self.recent_searches
            .set(self.session.with(|s| s.recent_searches.clone()));

        let kind = self.search_tab.get_untracked();
        let src = self.active_source();
        self.search_loading.set(true);
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.search(&trimmed, kind, 0)).await;
            store.search_loading.set(false);
            if let Ok(Ok(result)) = res {
                store.search_results.set(result);
            }
        });
    }

    pub fn set_search_tab(&self, kind: SearchKind) {
        self.search_tab.set(kind);
        let query = self.search_query.get_untracked();
        if !query.trim().is_empty() {
            self.search(&query);
        }
    }

    pub fn clear_recent_searches(&self) {
        self.session.with(|s| {
            s.clear_recent_searches();
            let _ = s.save();
        });
        self.recent_searches.set(Vec::new());
    }

    // -------------------------------------------------------------------------
    // Settings, Cache, Experiments, Locale, Theme & Modals
    // -------------------------------------------------------------------------

    pub fn set_locale(&self, locale: Locale) {
        self.locale.set(locale);
        self.plugin_mgr.with(|m| m.set_locale(locale.code()));
        self.settings.update(|s| {
            s.locale = locale;
            let _ = s.save();
        });
        self.refresh_sources();
    }

    pub fn set_theme(&self, mode: ThemeMode) {
        self.theme_override.set(mode);
        self.apply_theme_mode(mode);
        self.settings.update(|s| {
            s.theme_mode = mode;
            let _ = s.save();
        });
    }

    pub fn set_quality(&self, quality: Quality) {
        self.audio.with(|a| a.set_quality(quality));
        self.settings.update(|s| {
            s.audio_quality = quality;
            let _ = s.save();
        });
    }

    pub fn set_replay_gain(&self, enabled: bool) {
        self.audio.with(|a| a.set_replay_gain_enabled(enabled));
        self.settings.update(|s| {
            s.replay_gain = enabled;
            let _ = s.save();
        });
    }

    pub fn set_crossfade_ms(&self, ms: u32) {
        let clamped = ms.min(nsh_music_core::MAX_CROSSFADE_MS);
        self.audio.with(|a| a.set_crossfade_ms(clamped));
        self.settings.update(|s| {
            s.crossfade_ms = clamped;
            let _ = s.save();
        });
    }

    pub fn set_cache_max_mb(&self, mb: u64) {
        let clamped = mb.clamp(128, 4096);
        let _ = self
            .audio
            .with(|a| a.set_cache_max_bytes(clamped * 1024 * 1024));
        self.settings.update(|s| {
            s.cache_max_mb = clamped;
            let _ = s.save();
        });
        self.refresh_cache_stats();
    }

    pub fn refresh_cache_stats(&self) {
        let audio_bytes = self.audio.with(|a| a.cache_used_bytes());
        let art_dir = self.artwork.with(|a| a.cache_dir().to_path_buf());
        let store = *self;
        self.spawn_app(async move {
            let art_bytes = blocking(move || dir_file_bytes(&art_dir))
                .await
                .unwrap_or(0);
            store.cache_used_bytes.set(audio_bytes.saturating_add(art_bytes));
        });
    }

    pub fn clear_audio_cache(&self) {
        let _ = self.audio.with(|a| a.clear_cache());
        let art_dir = self.artwork.with(|a| a.cache_dir().to_path_buf());
        let store = *self;
        self.spawn_app(async move {
            let _ = blocking(move || {
                if let Ok(entries) = std::fs::read_dir(&art_dir) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            })
            .await;
            store.refresh_cache_stats();
            store.toasts.success(store.t("settings.cache.cleared"));
        });
    }

    pub fn set_hot_comments(&self, on: bool) {
        self.settings.update(|s| {
            s.hot_comments = on;
            let _ = s.save();
        });
    }

    pub fn set_stage_sparks(&self, on: bool) {
        self.settings.update(|s| {
            s.stage_sparks = on;
            let _ = s.save();
        });
    }

    pub fn copy_track_title(&self, track: &Track) {
        if !track.name.is_empty() {
            set_clipboard(track.name.clone());
            self.toasts.info(self.t("common.copied"));
        }
    }

    pub fn copy_track_title_and_artists(&self, track: &Track) {
        let artists = track.artist_names();
        let text = if artists.is_empty() {
            track.name.clone()
        } else {
            format!("{} - {}", track.name, artists)
        };
        if !text.is_empty() {
            set_clipboard(text);
            self.toasts.info(self.t("common.copied"));
        }
    }

    pub fn copy_track_link(&self, track: &Track) {
        if !track.url.is_empty() {
            set_clipboard(track.url.clone());
            self.toasts.info(self.t("common.copied"));
        }
    }

    pub fn open_external_url(&self, url: &str) {
        let trimmed = url.trim();
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            let target = trimmed.to_owned();
            self.spawn_app(async move {
                let _ = open_url(target).await;
            });
        }
    }

    pub fn can_download_track(&self, track: &Track) -> bool {
        if track.unavailable {
            return false;
        }
        let sid = if track.source.is_empty() {
            self.active_source_id.get()
        } else {
            track.source.clone()
        };
        self.sources.with(|list| {
            list.iter()
                .find(|s| s.info.id == sid)
                .is_some_and(|e| !e.is_local && e.capabilities.contains("stream"))
        })
    }

    pub fn download_track(&self, track: Track) {
        if !self.can_download_track(&track) {
            return;
        }
        let store = *self;
        let source_id = if track.source.is_empty() {
            self.active_source_id.get_untracked()
        } else {
            track.source.clone()
        };
        let src = self.source_by_id(&source_id);
        let quality = self.settings.with_untracked(|s| s.audio_quality);
        let title_label = self.t("track.download").to_string();
        let btn_label = self.t("common.save").to_string();
        let downloading_msg = self.t("track.downloading");

        self.spawn_app(async move {
            let track_id = track.id.clone();
            let stream_res = blocking(move || src.stream(&track_id, quality)).await;
            let Ok(Ok(stream)) = stream_res else {
                store.toasts.error(store.t("track.download_failed"));
                return;
            };
            if stream.url.trim().is_empty() {
                store.toasts.error(store.t("track.download_failed"));
                return;
            }
            let ext = infer_audio_ext(&stream.media_type, &stream.url);
            let artists = track.artist_names();
            let raw_stem = if artists.is_empty() {
                track.name.clone()
            } else {
                format!("{} - {}", track.name, artists)
            };
            let suggested = format!("{}.{}", sanitize_filename(&raw_stem), ext);

            let mut dialog = SaveDialog::new()
                .file_name(suggested)
                .title(title_label)
                .button_label(btn_label)
                .filter(FileFilter::new(ext.to_ascii_uppercase(), [ext.as_str()]));
            if let Some(home) =
                std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            {
                let dl_dir = PathBuf::from(home).join("Downloads");
                if dl_dir.is_dir() {
                    dialog = dialog.directory(dl_dir);
                }
            }

            let Ok(Some(dest)) = pick_save_path(dialog).await else {
                return;
            };

            store.toasts.info(downloading_msg);
            let src_dl = store.source_by_id(&source_id);
            let url = stream.url.clone();
            let stream_headers = stream.headers.clone();
            let write_res = blocking(move || -> Result<(), String> {
                if let Some(local_path) = url.strip_prefix("file://") {
                    std::fs::copy(local_path, &dest).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                let local_p = Path::new(&url);
                if local_p.exists() && local_p.is_file() {
                    std::fs::copy(local_p, &dest).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                let mut hdrs = src_dl.request_headers_for(&url, RequestKind::Audio);
                for (k, v) in stream_headers {
                    hdrs.push((k, v));
                }
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                rt.block_on(async move {
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::limited(5))
                        .build()
                        .map_err(|e| e.to_string())?;
                    let mut req = client.get(&url);
                    for (k, v) in hdrs {
                        req = req.header(k, v);
                    }
                    let resp = req
                        .send()
                        .await
                        .and_then(|r| r.error_for_status())
                        .map_err(|e| e.to_string())?;
                    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                    std::fs::write(&dest, &bytes).map_err(|e| e.to_string())
                })
            })
            .await;

            match write_res {
                Ok(Ok(())) => {
                    store
                        .toasts
                        .success(store.t_fmt("track.downloaded", &[("name", &track.name)]));
                }
                _ => {
                    store.toasts.error(store.t("track.download_failed"));
                }
            }
        });
    }

    pub fn set_fog_backdrop(&self, on: bool) {
        self.settings.update(|s| {
            s.fog_backdrop = on;
            let _ = s.save();
        });
    }

    pub fn set_smoke_progress(&self, on: bool) {
        self.settings.update(|s| {
            s.smoke_progress = on;
            let _ = s.save();
        });
    }

    pub fn set_grow_from_cover(&self, on: bool) {
        self.settings.update(|s| {
            s.grow_from_cover = on;
            let _ = s.save();
        });
    }

    pub fn toggle_sidebar_mine(&self) {
        self.settings.update(|s| {
            s.sidebar_mine_collapsed = !s.sidebar_mine_collapsed;
            let _ = s.save();
        });
    }

    pub fn toggle_sidebar_saved(&self) {
        self.settings.update(|s| {
            s.sidebar_saved_collapsed = !s.sidebar_saved_collapsed;
            let _ = s.save();
        });
    }

    pub fn open_modal(&self, modal: ActiveModal) {
        if matches!(modal, ActiveModal::InstallPlugin) {
            self.plugin_error.set(None);
        }
        self.active_modal.set(modal);
    }

    pub fn close_modal(&self) {
        self.active_modal.set(ActiveModal::None);
    }

    // -------------------------------------------------------------------------
    // Internal synchronization & data loading helpers
    // -------------------------------------------------------------------------

    fn apply_theme_mode(&self, mode: ThemeMode) {
        let appearance = match mode {
            ThemeMode::System => Appearance::System,
            ThemeMode::Light => Appearance::Light,
            ThemeMode::Dark => Appearance::Dark,
        };
        set_color_scheme(appearance);
    }

    fn hydrate_active_source_sync(&self) {
        let src = self.active_source();
        let home = src.home().unwrap_or_default();
        let playlists = src.playlists().unwrap_or_default();
        let liked = src.liked(0).map(|p| p.tracks).unwrap_or_default();
        let recent = src.recent().unwrap_or_default();
        let albums = src.saved_albums(0).map(|a| a.albums).unwrap_or_default();
        let stations = src.stations().unwrap_or_default();
        let shows = src.shows().unwrap_or_default();

        let liked_set: HashSet<String> = liked.iter().map(|t| t.id.clone()).collect();

        batch(|| {
            self.home_data.set(home.clone());
            self.browse_data.set(home);
            self.playlists.set(playlists);
            self.liked_tracks.set(liked);
            self.liked_ids.set(liked_set);
            self.recent_tracks.set(recent);
            self.saved_albums.set(albums);
            self.stations.set(stations);
            self.podcasts.set(shows);
        });
    }

    fn hydrate_active_source_async(&self) {
        let src = self.active_source();
        let store = *self;
        self.browse_loading.set(true);
        self.spawn_app(async move {
            let bundle = blocking(move || {
                (
                    src.home().unwrap_or_default(),
                    src.playlists().unwrap_or_default(),
                    src.liked(0).map(|p| p.tracks).unwrap_or_default(),
                    src.recent().unwrap_or_default(),
                    src.saved_albums(0).map(|a| a.albums).unwrap_or_default(),
                    src.stations().unwrap_or_default(),
                    src.shows().unwrap_or_default(),
                )
            })
            .await;

            store.browse_loading.set(false);
            if let Ok((home, playlists, liked, recent, albums, stations, shows)) = bundle {
                let liked_set: HashSet<String> = liked.iter().map(|t| t.id.clone()).collect();
                batch(|| {
                    store.home_data.set(home.clone());
                    store.browse_data.set(home);
                    store.playlists.set(playlists);
                    store.liked_tracks.set(liked);
                    store.liked_ids.set(liked_set);
                    store.recent_tracks.set(recent);
                    store.saved_albums.set(albums);
                    store.stations.set(stations);
                    store.podcasts.set(shows);
                });
            }
        });
    }

    fn load_data_for_route(&self, route: &Route) {
        let src = self.active_source();
        let store = *self;
        match route {
            Route::Home => {
                if self.home_data.with_untracked(|h| h.sections.is_empty()) {
                    self.hydrate_active_source_async();
                }
            }
            Route::Browse { page, title } => {
                self.refresh_browse(page, title);
            }
            Route::PlaylistDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    if let Ok(Ok(pl)) = blocking(move || src.playlist(&id)).await {
                        store.active_playlist.set(Some(pl));
                    }
                });
            }
            Route::AlbumDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    if let Ok(Ok(alb)) = blocking(move || src.album(&id)).await {
                        store.active_album.set(Some(alb));
                    }
                });
            }
            Route::ArtistDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    if let Ok(Ok(art)) = blocking(move || src.artist(&id)).await {
                        store.active_artist.set(Some(art));
                    }
                });
            }
            Route::ShowDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    if let Ok(Ok(show_page)) = blocking(move || src.show(&id, 0)).await {
                        store.active_show.set(Some(show_page));
                    }
                });
            }
            Route::Radio => {
                self.spawn_app(async move {
                    if let Ok(Ok(stations)) = blocking(move || src.stations()).await {
                        store.stations.set(stations);
                    }
                });
            }
            Route::Podcasts => {
                self.spawn_app(async move {
                    if let Ok(Ok(shows)) = blocking(move || src.shows()).await {
                        store.podcasts.set(shows);
                    }
                });
            }
            Route::Settings => {
                self.refresh_cache_stats();
            }
            Route::Plugins => {
                self.refresh_sources();
            }
            _ => {}
        }
    }

    fn on_track_changed(&self, track: &Track) {
        let src = self.source_by_id(&track.source);
        src.played(track);
        self.recent_tracks.update(|list| {
            list.retain(|t| t.id != track.id);
            list.insert(0, track.clone());
            if list.len() > 50 {
                list.truncate(50);
            }
        });
        if track.is_episode() {
            self.now_playing_panel.set(NowPlayingPanel::Podcast);
        } else if self.now_playing_panel.get_untracked() == NowPlayingPanel::Podcast {
            self.now_playing_panel.set(NowPlayingPanel::Lyrics);
        }
        self.load_track_extras(track);
    }

    fn load_track_extras(&self, track: &Track) {
        let track_id = track.id.clone();
        self.last_track_id.replace(Some(track_id.clone()));

        let seed = if !track.album.id.is_empty() {
            track.album.id.clone()
        } else {
            track.id.clone()
        };
        let raw_cover = if !track.local_cover.is_empty() {
            track.local_cover.clone()
        } else {
            track.album.image_url.clone()
        };

        self.current_accent
            .set(ArtworkLoader::accent_for_seed(&seed));

        let src = self.source_by_id(&track.source);
        let headers = src.request_headers_for(&raw_cover, RequestKind::Image);
        let loader = self.artwork.get();
        let store = *self;

        self.spawn_app(async move {
            let resolved =
                background(async move { loader.load(&raw_cover, 600, &seed, &headers).await }).await;
            if let Ok(art) = resolved
                && store.last_track_id.with(|id| id.as_deref() == Some(&track_id))
            {
                batch(|| {
                    store.current_artwork_uri.set(art.uri);
                    store.current_accent.set(art.accent);
                });
            }
        });

        let src_lrc = self.source_by_id(&track.source);
        let tid_lrc = track.id.clone();
        let tid_check = track.id.clone();
        let store_lrc = *self;
        let can_comments = src_lrc.can("comments");
        self.spawn_app(async move {
            let res = blocking(move || {
                let lyrics = src_lrc
                    .lyrics(&tid_lrc)
                    .map(|l| parse_lyrics(&l))
                    .unwrap_or_default();
                let comments = if can_comments {
                    src_lrc.comments(&tid_lrc, 0).unwrap_or_default()
                } else {
                    nsh_music_core::model::CommentPage::default()
                };
                (lyrics, comments)
            })
            .await;

            if let Ok((parsed, comment_page)) = res
                && store_lrc
                    .last_track_id
                    .with(|id| id.as_deref() == Some(tid_check.as_str()))
            {
                let pos = store_lrc.position_ms.get_untracked();
                let active = parsed.active_position(pos as i64);
                batch(|| {
                    store_lrc.lyrics.set(parsed);
                    store_lrc.active_lyric.set(active);
                    store_lrc.comments.set(comment_page.comments);
                    store_lrc.comments_total.set(comment_page.total);
                    if !can_comments
                        && store_lrc.now_playing_panel.get_untracked() == NowPlayingPanel::Comments
                    {
                        store_lrc.now_playing_panel.set(NowPlayingPanel::Lyrics);
                    }
                });
            }
        });
    }

    fn update_active_lyric(&self, position_ms: u64) {
        let next = self
            .lyrics
            .with_untracked(|lrc| lrc.active_position(position_ms as i64));
        if self.active_lyric.get_untracked() != next {
            self.active_lyric.set(next);
        }
    }

    fn sync_from_audio_engine(&self) {
        if let Some((source_id, prompt)) = self
            .pending_sign_in_prompt
            .with(|m| m.lock().ok().and_then(|mut g| g.take()))
        {
            let sid = if !source_id.is_empty() {
                source_id
            } else {
                self.signing_in_source_id
                    .with(|s| s.clone())
                    .unwrap_or_else(|| self.active_source_id.get_untracked())
            };
            self.active_modal
                .set(ActiveModal::QrSignIn { source_id: sid, prompt });
        }

        let snap = self.audio.with(|a| a.snapshot());
        let spec = self.audio.with(|a| a.spectrum());

        let snapshot_changed = self
            .last_snapshot
            .with(|prev| prev.as_ref() != Some(&snap));

        if snapshot_changed {
            self.last_snapshot.replace(Some(snap.clone()));

            let engine_has_track = snap.current_track.is_some();
            let engine_playing = snap.status.is_playing();

            batch(|| {
                if engine_has_track || snap.status != PlaybackStatus::Stopped {
                    self.playback_status.set(snap.status.clone());
                    self.is_playing.set(engine_playing);
                    self.position_ms.set(snap.position_ms);
                    if snap.duration_ms > 0 {
                        self.duration_ms.set(snap.duration_ms);
                    }
                    self.buffered_ms.set(snap.buffered_ms);
                    self.volume.set(snap.volume);
                    self.muted.set(snap.muted);
                    self.shuffle.set(snap.shuffle);
                    self.repeat.set(snap.repeat);
                }
                if !snap.queue.is_empty() {
                    self.queue.set(snap.queue.clone());
                    self.current_index.set(snap.current_index);
                }
                if let Some(ref tr) = snap.current_track {
                    let changed_track = self
                        .last_track_id
                        .with(|prev| prev.as_deref() != Some(tr.id.as_str()));
                    self.current_track.set(Some(tr.clone()));
                    if changed_track {
                        self.on_track_changed(tr);
                    }
                }
            });

            if engine_has_track {
                self.update_active_lyric(snap.position_ms);
            }
        }

        if self.is_playing.get_untracked() {
            self.spectrum.set(spec);
        } else if self.spectrum.with_untracked(|s| s.iter().any(|&v| v > 0.001)) {
            self.spectrum.set([0.0; SPECTRUM_BANDS]);
        }
    }
}

fn probe_capabilities(src: &dyn MusicSource) -> HashSet<String> {
    CAPABILITY_KEYS
        .iter()
        .filter(|&&k| src.can(k))
        .map(|&k| k.to_owned())
        .collect()
}

fn looks_like_plugin_dir(dir: &Path) -> bool {
    if dir.join("plugin.json").is_file() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.') && n != "__MACOSX")
        })
        .collect();
    subdirs.len() == 1 && subdirs[0].join("plugin.json").is_file()
}

fn dir_file_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn infer_audio_ext(media_type: &str, url: &str) -> String {
    let mt = media_type.trim().trim_start_matches('.').to_ascii_lowercase();
    for ext in ["flac", "m4a", "mp3", "ogg", "opus", "wav", "aac", "alac"] {
        if mt == ext || mt.ends_with(&format!("/{ext}")) {
            return ext.to_owned();
        }
    }
    let path_part = url.split('?').next().unwrap_or(url);
    if let Some((_, ext)) = path_part.rsplit_once('.') {
        let e = ext.to_ascii_lowercase();
        if matches!(
            e.as_str(),
            "flac" | "m4a" | "mp3" | "ogg" | "opus" | "wav" | "aac"
        ) {
            return e;
        }
    }
    "mp3".to_owned()
}

fn sanitize_filename(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = clean.trim().trim_matches('.');
    if trimmed.is_empty() {
        "track".to_owned()
    } else {
        trimmed.to_owned()
    }
}
