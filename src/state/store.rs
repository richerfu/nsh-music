//! Reactive `AppStore` (`#[derive(Clone, Copy)]`) holding fine-grained NSH
//! `Signal`s and coordinating `LocalMusicSource`, `PluginManager`,
//! `AudioEngine`, and `ArtworkLoader`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nsh_core::file_dialog::{FileFilter, OpenDialog, SaveDialog};
use nsh_core::prelude::*;
use nsh_music_audio::{
    AudioBackendCapabilities, AudioEngine, AudioEngineConfig, PlaybackSnapshot, PlaybackStatus,
    RemoteCommand, RemoteLoopMode, SPECTRUM_BANDS, audio_backend_capabilities,
};
use nsh_music_core::{
    i18n::{I18n, Locale},
    local::LocalMusicSource,
    lyrics::{ActiveLyric, ParsedLyrics, parse_lyrics},
    model::{
        Account, Album, AlbumPage, Artist, ArtistPage, Browse, Comment, PlayerStyle, Playlist,
        Quality, RemoteDevice, RepeatMode, SearchKind, SearchResult, Show, ShowPage, SignInPrompt,
        SourceError, SourceInfo, StageStyle, Station, Track,
    },
    session::PlaybackSession,
    settings::{AppSettings, ThemeMode},
    source::{MusicSource, RequestKind, SourceCompat},
};
use nsh_music_plugin::{
    PendingPluginInstall, PluginManager, PluginSummary,
    host::{WebSignInRequest, WebSignInResult},
};

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

/// Sections inside the Settings window (Desktop), Settings split view (Pad),
/// and Settings drill-down detail pages (Phone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SettingsSection {
    #[default]
    General,
    Plugins,
    Playback,
    Cache,
    Experiments,
    About,
}

impl SettingsSection {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Plugins,
        Self::Playback,
        Self::Cache,
        Self::Experiments,
        Self::About,
    ];

    pub fn key(&self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Plugins => "plugins",
            Self::Playback => "playback",
            Self::Cache => "cache",
            Self::Experiments => "experiments",
            Self::About => "about",
        }
    }

    pub fn title_key(&self) -> &'static str {
        match self {
            Self::General => "settings.section.general",
            Self::Plugins => "settings.section.plugins",
            Self::Playback => "settings.section.playback",
            Self::Cache => "settings.section.cache",
            Self::Experiments => "settings.section.experiments",
            Self::About => "settings.section.about",
        }
    }
}

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
    SettingsDetail(SettingsSection),
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
            Self::SettingsDetail(section) => format!("settings:{}", section.key()),
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
            Self::SettingsDetail(section) => section.title_key(),
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
    Stage,
    Queue,
    Wall,
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
        cookies_of: Option<String>,
        until: Vec<String>,
        user_agent: Option<String>,
        allow_manual_cookie: bool,
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
type WebSignInResponder = std::sync::mpsc::SyncSender<Result<WebSignInResult, String>>;
type PendingWebSignInSlot = Arc<Mutex<Option<(WebSignInRequest, WebSignInResponder)>>>;
type ActiveWebSignInSlot = Arc<Mutex<Option<WebSignInResponder>>>;
type PendingOpenUrlsSlot = Arc<Mutex<Vec<String>>>;

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
    pub signing_in_source: Signal<Option<String>>,
    pub backend_caps: Signal<AudioBackendCapabilities>,

    // Playback & audio
    pub playback_status: Signal<PlaybackStatus>,
    pub is_playing: Signal<bool>,
    pub current_track: Signal<Option<Track>>,
    pub queue: Signal<Vec<Track>>,
    pub current_index: Signal<Option<usize>>,
    pub position_ms: Signal<u64>,
    pub scrubber_preview_ms: Signal<Option<u64>>,
    pub duration_ms: Signal<u64>,
    pub buffered_ms: Signal<u64>,
    pub volume: Signal<f32>,
    pub muted: Signal<bool>,
    pub shuffle: Signal<bool>,
    pub repeat: Signal<RepeatMode>,
    pub spectrum: Signal<[f32; SPECTRUM_BANDS]>,
    pub current_is_lossless: Signal<bool>,
    pub current_artwork_uri: Signal<SharedString>,
    pub current_accent: Signal<Hsla>,
    pub remote_devices: Signal<Vec<RemoteDevice>>,
    pub active_remote_device: Signal<Option<String>>,
    pub cache_used_bytes: Signal<u64>,

    // Lyrics & comments
    pub lyrics: Signal<ParsedLyrics>,
    pub lyrics_loading: Signal<bool>,
    pub active_lyric: Signal<Option<ActiveLyric>>,
    pub show_translation: Signal<bool>,
    pub show_romaji: Signal<bool>,
    pub comments: Signal<Vec<Comment>>,
    pub comments_total: Signal<usize>,
    pub comments_loading: Signal<bool>,
    pub comments_more: Signal<bool>,
    pub comments_loading_more: Signal<bool>,

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
    pub search_loading_more: Signal<bool>,
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
    pub liked_more: Signal<bool>,
    pub liked_loading_more: Signal<bool>,
    pub recent_tracks: Signal<Vec<Track>>,
    pub saved_albums: Signal<Vec<Album>>,
    pub saved_albums_more: Signal<bool>,
    pub saved_albums_loading_more: Signal<bool>,
    pub stations: Signal<Vec<Station>>,
    pub podcasts: Signal<Vec<Show>>,
    pub active_show: Signal<Option<ShowPage>>,
    pub show_episodes_loading_more: Signal<bool>,

    // Settings, locale, theme, modals, toasts
    pub settings: Signal<AppSettings>,
    pub main_window_open: Signal<bool>,
    pub settings_window_open: Signal<bool>,
    pub settings_window_focus_seq: Signal<u64>,
    pub settings_section: Signal<SettingsSection>,
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
    settings_path: Stored<PathBuf>,
    session_path: Stored<PathBuf>,
    last_track_id: Stored<Option<String>>,
    last_played_reported_id: Stored<Option<String>>,
    last_snapshot: Stored<Option<PlaybackSnapshot>>,
    pending_seek: Stored<Option<(u64, Instant)>>,
    active_station_id: Stored<Option<String>>,
    pending_sign_in_prompt: Stored<PendingSignInSlot>,
    has_sign_in_prompt: Stored<Arc<AtomicBool>>,
    pending_web_sign_in: Stored<PendingWebSignInSlot>,
    has_web_sign_in_request: Stored<Arc<AtomicBool>>,
    active_web_sign_in: Stored<ActiveWebSignInSlot>,
    pending_open_urls: Stored<PendingOpenUrlsSlot>,
    has_open_url_request: Stored<Arc<AtomicBool>>,
    pending_remote_toggle_favorite: Stored<Arc<AtomicBool>>,
    signing_in_source_id: Stored<Option<String>>,
    search_suggest_gen: Stored<u64>,
    browse_cache: Stored<HashMap<(String, String), Browse>>,
    browse_req_gen: Stored<u64>,
    spectrum_is_zero: Stored<bool>,
    pending_settings_save: Stored<Arc<Mutex<Option<AppSettings>>>>,
    settings_saving: Stored<Arc<AtomicBool>>,
    pending_session_save: Stored<Arc<Mutex<Option<PlaybackSession>>>>,
    session_saving: Stored<Arc<AtomicBool>>,
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
        let sandbox_root = sandbox_root_dir();
        let sandbox = sandbox_paths_from_root(&sandbox_root);
        let use_sandbox = cfg!(any(
            target_env = "ohos",
            target_os = "android",
            target_os = "ios"
        ));

        let settings_path = if use_sandbox {
            sandbox.settings_path.clone()
        } else {
            AppSettings::default_path()
        };
        let session_path = if use_sandbox {
            sandbox.session_path.clone()
        } else {
            PlaybackSession::default_path()
        };

        let mut settings = AppSettings::load_from(&settings_path).unwrap_or_default();
        let session = PlaybackSession::load_from(&session_path).unwrap_or_default();

        let local = Arc::new(if use_sandbox {
            LocalMusicSource::with_storage(
                settings.local_folders.clone(),
                sandbox.local_covers_dir.clone(),
                Some(sandbox.local_library_path.clone()),
            )
        } else {
            LocalMusicSource::with_default_storage(settings.local_folders.clone())
        });
        for f in local.folders() {
            settings.add_local_folder(f);
        }

        let plugin_mgr = Arc::new(if use_sandbox {
            PluginManager::new(&sandbox.plugins_dir, &sandbox.plugin_data_dir)
                .or_else(|_| {
                    let fallback = std::env::temp_dir().join("nsh-music-plugins");
                    PluginManager::new(fallback.join("installed"), fallback.join("data"))
                })
                .expect("initialize plugin manager")
        } else {
            PluginManager::with_default_dirs()
                .or_else(|_| PluginManager::new(&sandbox.plugins_dir, &sandbox.plugin_data_dir))
                .or_else(|_| {
                    let fallback = std::env::temp_dir().join("nsh-music-plugins");
                    PluginManager::new(fallback.join("installed"), fallback.join("data"))
                })
                .expect("initialize plugin manager")
        });
        plugin_mgr.set_locale(settings.locale.code());
        let pending_sign_in_prompt: PendingSignInSlot = Arc::new(Mutex::new(None));
        let has_sign_in_prompt = Arc::new(AtomicBool::new(false));
        {
            let slot = Arc::clone(&pending_sign_in_prompt);
            let flag = Arc::clone(&has_sign_in_prompt);
            plugin_mgr.set_sign_in_prompt(Some(Arc::new(move |prompt: SignInPrompt| {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some((String::new(), prompt));
                    flag.store(true, Ordering::Release);
                }
            })));
        }

        let pending_web_sign_in: PendingWebSignInSlot = Arc::new(Mutex::new(None));
        let has_web_sign_in_request = Arc::new(AtomicBool::new(false));
        let active_web_sign_in: ActiveWebSignInSlot = Arc::new(Mutex::new(None));
        {
            let slot = Arc::clone(&pending_web_sign_in);
            let flag = Arc::clone(&has_web_sign_in_request);
            let active_slot = Arc::clone(&active_web_sign_in);
            plugin_mgr.set_web_sign_in_handler(Some(Arc::new(move |req: WebSignInRequest| {
                // First attempt native isolated popup WebView sign-in (e.g. macOS NSWindow + WKWebView
                // capturing HttpOnly cookies directly from its nonPersistentDataStore).
                let native_opts = nsh_web_signin::WebSignInOptions {
                    url: req.url.clone(),
                    title: req.title.clone(),
                    cookies_of: req.cookies_of.clone(),
                    until: req.until.clone(),
                    match_url: req.match_url.clone(),
                    user_agent: req.user_agent.clone(),
                    incognito: true,
                    timeout_ms: req.timeout_ms,
                };
                match nsh_web_signin::open_blocking(native_opts) {
                    Ok(outcome) => {
                        return Ok(WebSignInResult {
                            ok: outcome.ok,
                            url: outcome.url,
                            cookies: outcome.cookies,
                            cookie: outcome.cookie,
                            eval_result: None,
                            cancelled: outcome.cancelled,
                            manual_fallback: false,
                        });
                    }
                    Err(nsh_web_signin::SignInError::Unavailable(_)) => {
                        // Fall through cleanly to the embedded NSH modal sheet (HarmonyOS, headless tests, etc.).
                    }
                    Err(err) => {
                        return Err(err.to_string());
                    }
                }

                let timeout = if req.timeout_ms == 0 {
                    Duration::from_secs(300)
                } else {
                    Duration::from_millis(req.timeout_ms)
                };
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                if let Ok(mut guard) = slot.lock() {
                    if let Some((_, prev_tx)) = guard.replace((req, tx)) {
                        let _ = prev_tx.try_send(Ok(WebSignInResult {
                            cancelled: true,
                            ..WebSignInResult::default()
                        }));
                    }
                    flag.store(true, Ordering::Release);
                }
                match rx.recv_timeout(timeout) {
                    Ok(res) => res,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if let Ok(mut guard) = active_slot.lock() {
                            guard.take();
                        }
                        Err("web sign-in timed out".to_owned())
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(WebSignInResult {
                        cancelled: true,
                        ..WebSignInResult::default()
                    }),
                }
            })));
        }

        let pending_open_urls: PendingOpenUrlsSlot = Arc::new(Mutex::new(Vec::new()));
        let has_open_url_request = Arc::new(AtomicBool::new(false));
        {
            let slot = Arc::clone(&pending_open_urls);
            let flag = Arc::clone(&has_open_url_request);
            plugin_mgr.set_open_url_handler(Some(Arc::new(move |url: &str| {
                if let Ok(mut guard) = slot.lock() {
                    guard.push(url.to_owned());
                    flag.store(true, Ordering::Release);
                    Ok(())
                } else {
                    Err("open URL queue unavailable".to_owned())
                }
            })));
        }
        plugin_mgr.set_mobile_virtual_compat(settings.mobile_virtual_compat);
        let _ = plugin_mgr.load_all();

        let mut audio_cfg = AudioEngineConfig {
            initial_volume: session.volume,
            replay_gain_enabled: settings.replay_gain,
            crossfade_ms: settings.clamped_crossfade_ms(),
            quality: settings.audio_quality,
            ..AudioEngineConfig::default()
        };
        if use_sandbox {
            audio_cfg.cache.cache_dir = sandbox.audio_cache_dir.clone();
        }
        audio_cfg.cache.max_bytes = settings.cache_max_mb.max(128) * 1024 * 1024;
        let audio = AudioEngine::new(audio_cfg.clone())
            .or_else(|_| {
                let mut retry_cfg = audio_cfg.clone();
                retry_cfg.cache.cache_dir = sandbox.audio_cache_dir.clone();
                AudioEngine::new(retry_cfg)
            })
            .or_else(|_| {
                let mut retry_cfg = audio_cfg.clone();
                retry_cfg.cache.cache_dir = std::env::temp_dir().join("nsh-music-audio-cache");
                AudioEngine::new(retry_cfg)
            })
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
                    Err(SourceError::Message(format!(
                        "music source '{}' is not loaded",
                        track.source
                    )))
                }
            })));
        }

        let pending_remote_toggle_favorite = Arc::new(AtomicBool::new(false));
        {
            let remote = audio.remote_controller();
            crate::platform::install_platform_remote(remote);
            remote.update_loop_mode(RemoteLoopMode::from_repeat_shuffle(
                session.repeat,
                session.shuffle,
            ));
            let fav_flag = Arc::clone(&pending_remote_toggle_favorite);
            remote.on_command(Arc::new(move |cmd| {
                if matches!(cmd, RemoteCommand::ToggleFavorite) {
                    fav_flag.store(true, Ordering::Release);
                }
            }));
        }

        let artwork = if use_sandbox {
            ArtworkLoader::new(sandbox.artwork_cache_dir.clone())
        } else {
            ArtworkLoader::default_location()
        };
        let initial_cache_bytes = audio.cache_used_bytes();
        let initial_locale = settings.locale;
        let initial_theme = settings.theme_mode;
        let initial_mode = NowPlayingMode::from(settings.default_player_style);
        let initial_stage = NowPlayingStageStyle::from(settings.default_stage_style);
        let initial_panel = if settings.lyrics_stage {
            NowPlayingPanel::Stage
        } else {
            NowPlayingPanel::Lyrics
        };
        let initial_source_id = if session.active_source_id.is_empty() {
            LocalMusicSource::SOURCE_ID.to_owned()
        } else {
            session.active_source_id.clone()
        };
        let initial_is_local = initial_source_id == LocalMusicSource::SOURCE_ID;

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
            signing_in_source: signal(None),
            backend_caps: signal(audio_backend_capabilities()),

            playback_status: signal(PlaybackStatus::Stopped),
            is_playing: signal(false),
            current_track: signal(session.current_track().cloned()),
            queue: signal(session.queue.clone()),
            current_index: signal(session.current_index),
            position_ms: signal(session.position_ms.max(0) as u64),
            scrubber_preview_ms: signal(None),
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
            current_is_lossless: signal(false),
            current_artwork_uri: signal(SharedString::default()),
            current_accent: signal(theme::default_artwork_accent()),
            remote_devices: signal(Vec::new()),
            active_remote_device: signal(None),
            cache_used_bytes: signal(initial_cache_bytes),

            lyrics: signal(ParsedLyrics::default()),
            lyrics_loading: signal(false),
            active_lyric: signal(None),
            show_translation: signal(true),
            show_romaji: signal(false),
            comments: signal(Vec::new()),
            comments_total: signal(0),
            comments_loading: signal(false),
            comments_more: signal(false),
            comments_loading_more: signal(false),

            now_playing_open: signal(false),
            now_playing_mode: signal(initial_mode),
            stage_style: signal(initial_stage),
            now_playing_panel: signal(initial_panel),
            stage_fullscreen: signal(false),

            search_query: signal(String::new()),
            search_tab: signal(SearchKind::Tracks),
            search_results: signal(SearchResult::default()),
            search_loading: signal(false),
            search_loading_more: signal(false),
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
            liked_more: signal(false),
            liked_loading_more: signal(false),
            recent_tracks: signal(Vec::new()),
            saved_albums: signal(Vec::new()),
            saved_albums_more: signal(false),
            saved_albums_loading_more: signal(false),
            stations: signal(Vec::new()),
            podcasts: signal(Vec::new()),
            active_show: signal(None),
            show_episodes_loading_more: signal(false),

            settings: signal(settings),
            main_window_open: signal(true),
            settings_window_open: signal(false),
            settings_window_focus_seq: signal(0),
            settings_section: signal(SettingsSection::General),
            locale: signal(initial_locale),
            theme_override: signal(initial_theme),
            active_modal: signal(ActiveModal::None),
            toasts: ToastController::new(),

            local: stored(local),
            plugin_mgr: stored(plugin_mgr),
            audio: stored(audio),
            artwork: stored(artwork),
            session: stored(session),
            settings_path: stored(settings_path),
            session_path: stored(session_path),
            last_track_id: stored(None),
            last_played_reported_id: stored(None),
            last_snapshot: stored(None),
            pending_seek: stored(None),
            active_station_id: stored(None),
            pending_sign_in_prompt: stored(pending_sign_in_prompt),
            has_sign_in_prompt: stored(has_sign_in_prompt),
            pending_web_sign_in: stored(pending_web_sign_in),
            has_web_sign_in_request: stored(has_web_sign_in_request),
            active_web_sign_in: stored(active_web_sign_in),
            pending_open_urls: stored(pending_open_urls),
            has_open_url_request: stored(has_open_url_request),
            pending_remote_toggle_favorite: stored(pending_remote_toggle_favorite),
            signing_in_source_id: stored(None),
            search_suggest_gen: stored(0),
            browse_cache: stored(HashMap::new()),
            browse_req_gen: stored(0),
            spectrum_is_zero: stored(true),
            pending_settings_save: stored(Arc::new(Mutex::new(None))),
            settings_saving: stored(Arc::new(AtomicBool::new(false))),
            pending_session_save: stored(Arc::new(Mutex::new(None))),
            session_saving: stored(Arc::new(AtomicBool::new(false))),
            root_owner: Owner::current(),
        };

        store.apply_theme_mode(initial_theme);
        store.refresh_sources();
        if initial_is_local {
            store.hydrate_active_source_sync();
        } else {
            store.hydrate_active_source_async();
        }
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
        let next = match next {
            Route::Plugins => {
                self.settings_section.set(SettingsSection::Plugins);
                let form = untracked(theme::detect_device_form);
                match form {
                    theme::DeviceForm::Desktop => {
                        if self.mobile_drawer_open.get_untracked() {
                            self.mobile_drawer_open.set(false);
                        }
                        self.open_settings_window(SettingsSection::Plugins);
                        return;
                    }
                    theme::DeviceForm::Pad { .. } => Route::Settings,
                    theme::DeviceForm::Phone => Route::SettingsDetail(SettingsSection::Plugins),
                }
            }
            Route::SettingsDetail(section) => {
                self.settings_section.set(section);
                Route::SettingsDetail(section)
            }
            other => other,
        };

        let current = self.route.get_untracked();
        if current != next {
            batch(|| {
                self.route.set(next.clone());
                if self.nav_forward.with_untracked(|f| !f.is_empty()) {
                    self.nav_forward.update(Vec::clear);
                }
                if self.page_scrolled.get_untracked() {
                    self.page_scrolled.set(false);
                }
                if self.search_suggest_open.get_untracked() {
                    self.search_suggest_open.set(false);
                }
                if self.mobile_drawer_open.get_untracked() {
                    self.mobile_drawer_open.set(false);
                }
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
            self.load_data_for_route(&next);
        } else if self.mobile_drawer_open.get_untracked() {
            self.mobile_drawer_open.set(false);
        }
    }

    /// Switches directly to a Phone/mobile root tab, resetting navigation history
    /// so top-level root tabs never show a back button.
    pub fn switch_tab(&self, root: Route) {
        if self.route.get_untracked() == root {
            batch(|| {
                let history_matches = self
                    .nav_history
                    .with_untracked(|hist| hist.len() == 1 && hist.first() == Some(&root));
                if !history_matches {
                    self.nav_history.set(vec![root.clone()]);
                }
                if self.nav_forward.with_untracked(|f| !f.is_empty()) {
                    self.nav_forward.update(Vec::clear);
                }
                if self.page_scrolled.get_untracked() {
                    self.page_scrolled.set(false);
                }
                if self.search_suggest_open.get_untracked() {
                    self.search_suggest_open.set(false);
                }
                if self.mobile_drawer_open.get_untracked() {
                    self.mobile_drawer_open.set(false);
                }
            });
            return;
        }

        batch(|| {
            self.route.set(root.clone());
            self.nav_history.set(vec![root.clone()]);
            if self.nav_forward.with_untracked(|f| !f.is_empty()) {
                self.nav_forward.update(Vec::clear);
            }
            if self.page_scrolled.get_untracked() {
                self.page_scrolled.set(false);
            }
            if self.search_suggest_open.get_untracked() {
                self.search_suggest_open.set(false);
            }
            if self.mobile_drawer_open.get_untracked() {
                self.mobile_drawer_open.set(false);
            }
        });
        self.load_data_for_route(&root);
    }

    /// Returns whether `route` is a pushed secondary screen on Phone (shows `<` back bar).
    pub fn is_phone_secondary_route(route: &Route) -> bool {
        match route {
            Route::Home | Route::Library | Route::Settings => false,
            Route::Browse { page, .. } => !page.is_empty(),
            _ => true,
        }
    }

    /// Returns whether this binary was compiled for a mobile sandbox target (iOS, Android, HarmonyOS).
    pub fn is_mobile_runtime(&self) -> bool {
        cfg!(any(
            target_os = "ios",
            target_os = "android",
            target_env = "ohos"
        ))
    }

    /// Opens the desktop Settings window (or selects `section` inside Settings) and refreshes stats.
    pub fn open_settings_window(&self, section: SettingsSection) {
        batch(|| {
            self.settings_section.set(section);
            self.settings_window_open.set(true);
            self.settings_window_focus_seq
                .update(|seq| *seq = seq.saturating_add(1));
        });
        self.refresh_cache_stats();
        if section == SettingsSection::Plugins {
            self.refresh_sources();
        }
    }

    /// Closes the desktop Settings window.
    pub fn close_settings_window(&self) {
        self.settings_window_open.set(false);
    }

    /// Opens (or re-opens) the primary desktop window.
    pub fn open_main_window(&self) {
        self.main_window_open.set(true);
    }

    /// Closes the primary desktop window along with the Settings child window.
    pub fn close_main_window(&self) {
        batch(|| {
            self.main_window_open.set(false);
            self.settings_window_open.set(false);
        });
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
        let existing_accounts: std::collections::HashMap<String, Account> =
            self.sources.with_untracked(|list| {
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
                        .map(|(id, src)| (id, src.account()))
                        .collect::<Vec<_>>()
                })
                .await;
                if let Ok(resolved) = results {
                    store.sources.update(|entries| {
                        for (id, acc_res) in resolved {
                            if let Ok(acc) = acc_res
                                && let Some(entry) = entries.iter_mut().find(|e| e.info.id == id)
                            {
                                entry.account = acc;
                            }
                        }
                    });
                }
            });
        }
    }

    /// Refreshes the `plugins` summary signal (including per-plugin `runtime_error` banners).
    pub fn refresh_plugin_summaries(&self) {
        let summaries = self.plugin_mgr.with(|m| m.plugins());
        self.plugins.set(summaries);
    }

    /// Toggles in-process mobile virtualization compatibility (`ffmpeg`/`yt-dlp` shims vs system `PATH`).
    pub fn set_mobile_virtual_compat(&self, enabled: bool) {
        self.settings.update(|s| {
            s.mobile_virtual_compat = enabled;
        });
        self.persist_settings();
        self.plugin_mgr
            .with(|m| m.set_mobile_virtual_compat(enabled));
        self.invalidate_browse_cache();
        self.refresh_sources();
    }

    fn invalidate_browse_cache(&self) {
        self.browse_cache.with(HashMap::clear);
        self.browse_req_gen.with(|g| *g = g.wrapping_add(1));
    }

    /// Switches the active music source and refreshes home/library/catalog data.
    pub fn select_source(&self, source_id: &str) {
        let target = if source_id.is_empty() {
            LocalMusicSource::SOURCE_ID.to_owned()
        } else {
            source_id.to_owned()
        };
        self.invalidate_browse_cache();
        batch(|| {
            self.active_source_id.set(target.clone());
            self.home_data.set(Browse::default());
            self.browse_data.set(Browse::default());
            self.playlists.set(Vec::new());
            self.liked_tracks.set(Vec::new());
            self.liked_ids.set(HashSet::new());
            self.liked_more.set(false);
            self.liked_loading_more.set(false);
            self.recent_tracks.set(Vec::new());
            self.saved_albums.set(Vec::new());
            self.saved_albums_more.set(false);
            self.saved_albums_loading_more.set(false);
            self.stations.set(Vec::new());
            self.podcasts.set(Vec::new());
            self.search_suggestions.set(Vec::new());
            self.search_suggest_open.set(false);
            self.search_results.set(SearchResult::default());
            self.search_loading_more.set(false);
            self.active_playlist.set(None);
            self.active_album.set(None);
            self.active_artist.set(None);
            self.active_show.set(None);
            self.show_episodes_loading_more.set(false);
            self.remote_devices.set(Vec::new());
            self.active_remote_device.set(None);
        });
        if self.now_playing_panel.get_untracked() == NowPlayingPanel::Comments
            && !self.active_can("comments")
        {
            self.now_playing_panel.set(self.preferred_lyrics_panel());
        }
        self.session.with(|s| {
            s.active_source_id = target;
        });
        self.persist_session();
        self.hydrate_active_source_async();
    }

    pub fn refresh_home(&self) {
        self.hydrate_active_source_async();
    }

    fn load_browse_if_needed(&self, page: &str, title: &str) {
        let source_id = self.active_source_id.get_untracked();
        let cached = self
            .browse_cache
            .with(|cache| cache.get(&(source_id, page.to_owned())).cloned());
        if let Some(mut entry) = cached {
            if entry.title.is_empty() && !title.is_empty() {
                entry.title = title.to_owned();
            }
            self.browse_req_gen.with(|g| *g = g.wrapping_add(1));
            batch(|| {
                if self.browse_loading.get_untracked() {
                    self.browse_loading.set(false);
                }
                if self.browse_data.with_untracked(|cur| cur != &entry) {
                    self.browse_data.set(entry);
                }
            });
            return;
        }
        self.fetch_browse(page, title);
    }

    pub fn refresh_browse(&self, page: &str, title: &str) {
        self.fetch_browse(page, title);
    }

    fn fetch_browse(&self, page: &str, title: &str) {
        let src = self.active_source();
        let source_id = self.active_source_id.get_untracked();
        let page_str = page.to_owned();
        let fallback_title = title.to_owned();
        let token = self.browse_req_gen.with(|g| {
            *g = g.wrapping_add(1);
            *g
        });
        if !self.browse_loading.get_untracked() {
            self.browse_loading.set(true);
        }
        let store = *self;
        self.spawn_app(async move {
            let page_bg = page_str.clone();
            let res = blocking(move || {
                if page_bg.is_empty() {
                    src.home()
                } else {
                    src.browse(&page_bg)
                }
            })
            .await;
            let still_current = store.browse_req_gen.with(|g| *g == token)
                && store.active_source_id.get_untracked() == source_id;
            if !still_current {
                return;
            }
            let mut err_msg: Option<String> = None;
            let mut plugin_err = false;
            let mut plugin_ok = false;
            batch(|| {
                if store.browse_loading.get_untracked() {
                    store.browse_loading.set(false);
                }
                match res {
                    Ok(Ok(mut browse)) => {
                        plugin_ok = source_id != LocalMusicSource::SOURCE_ID;
                        if browse.title.is_empty() && !fallback_title.is_empty() {
                            browse.title = fallback_title;
                        }
                        if !browse.sections.is_empty() || source_id == LocalMusicSource::SOURCE_ID {
                            store.browse_cache.with(|cache| {
                                cache.insert((source_id.clone(), page_str), browse.clone())
                            });
                        }
                        if store.browse_data.with_untracked(|cur| cur != &browse) {
                            store.browse_data.set(browse);
                        }
                    }
                    Ok(Err(err)) => {
                        plugin_err = source_id != LocalMusicSource::SOURCE_ID;
                        err_msg = Some(format!("{source_id}: {err}"));
                    }
                    Err(err) => {
                        err_msg = Some(format!("{source_id}: {err}"));
                    }
                }
            });
            if plugin_err
                || (plugin_ok
                    && store.plugins.with_untracked(|list| {
                        list.iter()
                            .any(|p| p.id == source_id && p.runtime_error.is_some())
                    }))
            {
                store.refresh_plugin_summaries();
            }
            if let Some(msg) = err_msg {
                store.toasts.error(msg);
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
                    store
                        .toasts
                        .info(store.t_fmt("plugins.update_available", &[("version", &ver)]));
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
                self.invalidate_browse_cache();
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
                });
                self.persist_settings();
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
                self.invalidate_browse_cache();
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
        let key_owned = key.to_owned();
        let val_owned = value.to_owned();
        let store = *self;
        self.spawn_app(async move {
            match blocking(move || src.set_setting(&key_owned, &val_owned)).await {
                Ok(Ok(())) => {
                    store.toasts.success(store.t("common.save"));
                }
                Ok(Err(e)) => store.toasts.error(e.to_string()),
                Err(e) => store.toasts.error(e.to_string()),
            }
        });
    }

    /// Immediately applies a freshly authenticated account onto the matching source entry.
    pub fn apply_signed_in_account(&self, source_id: &str, account: Account) {
        self.invalidate_browse_cache();
        self.sources.update(|entries| {
            if let Some(entry) = entries.iter_mut().find(|e| e.info.id == source_id) {
                entry.account = Some(account);
            }
        });
    }

    pub fn sign_in_source(&self, source_id: &str) {
        let src = self.source_by_id(source_id);
        let id = source_id.to_owned();
        let store = *self;
        self.signing_in_source.set(Some(id.clone()));
        self.signing_in_source_id.replace(Some(id.clone()));
        self.has_sign_in_prompt
            .with(|flag| flag.store(false, Ordering::Release));
        self.pending_sign_in_prompt.with(|m| {
            if let Ok(mut guard) = m.lock() {
                *guard = None;
            }
        });
        self.spawn_app(async move {
            let res = blocking(move || src.sign_in()).await;
            store.signing_in_source.set(None);
            store.signing_in_source_id.replace(None);
            store
                .has_sign_in_prompt
                .with(|flag| flag.store(false, Ordering::Release));
            store.pending_sign_in_prompt.with(|m| {
                if let Ok(mut guard) = m.lock() {
                    *guard = None;
                }
            });
            match res {
                Ok(Ok(account)) => {
                    store.apply_signed_in_account(&id, account.clone());
                    store.refresh_sources();
                    if store.active_source_id.get_untracked() == id {
                        store.hydrate_active_source_async();
                        if let Route::Browse { page, title } = store.route.get_untracked()
                            && !page.is_empty()
                        {
                            store.refresh_browse(&page, &title);
                        }
                    }
                    store.close_modal();
                    store
                        .toasts
                        .success(store.t_fmt("plugins.signed_in_as", &[("name", &account.name)]));
                }
                Ok(Err(err)) => {
                    if matches!(
                        store.active_modal.get_untracked(),
                        ActiveModal::WebSignIn { .. } | ActiveModal::QrSignIn { .. }
                    ) {
                        store.active_modal.set(ActiveModal::None);
                    }
                    store.toasts.error(format!("{id}: {err}"));
                }
                Err(err) => {
                    if matches!(
                        store.active_modal.get_untracked(),
                        ActiveModal::WebSignIn { .. } | ActiveModal::QrSignIn { .. }
                    ) {
                        store.active_modal.set(ActiveModal::None);
                    }
                    store.toasts.error(err.to_string());
                }
            }
        });
    }

    pub fn sign_out_source(&self, source_id: &str) {
        let src = self.source_by_id(source_id);
        let id = source_id.to_owned();
        let store = *self;
        self.spawn_app(async move {
            let _ = blocking(move || src.sign_out()).await;
            store.invalidate_browse_cache();
            store.sources.update(|entries| {
                if let Some(entry) = entries.iter_mut().find(|e| e.info.id == id) {
                    entry.account = None;
                }
            });
            store.refresh_sources();
            if store.active_source_id.get_untracked() == id {
                store.hydrate_active_source_async();
                if let Route::Browse { page, title } = store.route.get_untracked()
                    && !page.is_empty()
                {
                    store.refresh_browse(&page, &title);
                }
            }
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

    /// Parses `mygo-music://`, `mygo-music-dev://`, and `nsh-music://` `install-plugin` deep links.
    pub fn handle_deep_link(&self, raw_url: &str) {
        let Some(repo) = parse_install_plugin_deep_link(raw_url) else {
            return;
        };
        if self.now_playing_open.get_untracked() {
            self.close_now_playing();
        }
        if !self.main_window_open.get_untracked() {
            self.open_main_window();
        }
        self.navigate(Route::Plugins);
        self.toasts
            .info(self.t_fmt("plugins.staging_repo", &[("repo", &repo)]));
        self.stage_github_plugin(&repo);
    }

    // -------------------------------------------------------------------------
    // Playback & Queue
    // -------------------------------------------------------------------------

    /// Plays a single track immediately (replacing the queue with `[track]`).
    pub fn play_track(&self, track: Track) {
        self.active_station_id.replace(None);
        self.pending_seek.replace(None);
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
            self.scrubber_preview_ms.set(None);
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
        self.pending_seek.replace(None);
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
            self.scrubber_preview_ms.set(None);
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

    /// Alias for [`Self::enqueue`], matching Now Playing and context menu actions.
    pub fn add_to_queue(&self, track: Track) {
        self.enqueue(track);
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

    /// Alias for [`Self::enqueue_next`], matching Now Playing and context menu actions.
    pub fn play_next(&self, track: Track) {
        self.enqueue_next(track);
    }

    /// Returns a snapshot of the currently loaded track without creating a reactive dependency.
    pub fn current_track_untracked(&self) -> Option<Track> {
        self.current_track.get_untracked()
    }

    /// Navigates to an artist detail page (closing Now Playing if open).
    pub fn go_to_artist(&self, artist_id: &str) {
        let id = artist_id.trim();
        if id.is_empty() {
            return;
        }
        if self.now_playing_open.get_untracked() {
            self.close_now_playing();
        }
        self.navigate(Route::ArtistDetail { id: id.to_owned() });
    }

    /// Navigates to an album detail page (closing Now Playing if open).
    pub fn go_to_album(&self, album_id: &str) {
        let id = album_id.trim();
        if id.is_empty() {
            return;
        }
        if self.now_playing_open.get_untracked() {
            self.close_now_playing();
        }
        self.navigate(Route::AlbumDetail { id: id.to_owned() });
    }

    /// Navigates to a podcast show detail page (closing Now Playing if open).
    pub fn go_to_show(&self, show_id: &str) {
        let id = show_id.trim();
        if id.is_empty() {
            return;
        }
        if self.now_playing_open.get_untracked() {
            self.close_now_playing();
        }
        self.navigate(Route::ShowDetail { id: id.to_owned() });
    }

    /// Navigates to the primary artist of the currently playing track.
    pub fn go_to_current_artist(&self) {
        if let Some(id) = self.current_track.with_untracked(|t| {
            t.as_ref().and_then(|tr| {
                tr.artists
                    .iter()
                    .find(|a| !a.id.is_empty())
                    .map(|a| a.id.clone())
            })
        }) {
            self.go_to_artist(&id);
        }
    }

    /// Navigates to the album of the currently playing track.
    pub fn go_to_current_album(&self) {
        if let Some(id) = self.current_track.with_untracked(|t| {
            t.as_ref()
                .filter(|tr| !tr.album.id.is_empty())
                .map(|tr| tr.album.id.clone())
        }) {
            self.go_to_album(&id);
        }
    }

    /// Navigates to the podcast show of the currently playing episode.
    pub fn go_to_current_show(&self) {
        if let Some(id) = self.current_track.with_untracked(|t| {
            t.as_ref()
                .filter(|tr| !tr.show_id.is_empty())
                .map(|tr| tr.show_id.clone())
        }) {
            self.go_to_show(&id);
        }
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
        self.last_track_id.replace(None);
        self.pending_seek.replace(None);
        batch(|| {
            self.queue.set(Vec::new());
            self.current_index.set(None);
            self.current_track.set(None);
            self.playback_status.set(PlaybackStatus::Stopped);
            self.is_playing.set(false);
            self.current_is_lossless.set(false);
            self.scrubber_preview_ms.set(None);
            self.position_ms.set(0);
            self.duration_ms.set(0);
            self.lyrics.set(ParsedLyrics::default());
            self.active_lyric.set(None);
            self.lyrics_loading.set(false);
            self.comments.set(Vec::new());
            self.comments_total.set(0);
            self.comments_more.set(false);
            self.comments_loading_more.set(false);
            self.comments_loading.set(false);
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
        self.pending_seek.replace(None);
        self.audio.with(|a| a.next_track());
        let q = self.queue.get_untracked();
        if q.is_empty() {
            self.scrubber_preview_ms.set(None);
            return;
        }
        let cur = self.current_index.get_untracked().unwrap_or(0);
        let next_idx = if cur + 1 < q.len() { cur + 1 } else { 0 };
        let next_track = q[next_idx].clone();
        batch(|| {
            self.current_index.set(Some(next_idx));
            self.current_track.set(Some(next_track.clone()));
            self.scrubber_preview_ms.set(None);
            self.position_ms.set(0);
            self.duration_ms.set(next_track.duration.max(0) as u64);
            self.is_playing.set(true);
            self.playback_status.set(PlaybackStatus::Playing);
        });
        self.on_track_changed(&next_track);
    }

    pub fn prev_track(&self) {
        self.pending_seek.replace(None);
        self.audio.with(|a| a.prev_track());
        let q = self.queue.get_untracked();
        if q.is_empty() {
            self.scrubber_preview_ms.set(None);
            return;
        }
        let cur = self.current_index.get_untracked().unwrap_or(0);
        let prev_idx = if cur > 0 { cur - 1 } else { q.len() - 1 };
        let prev_track = q[prev_idx].clone();
        batch(|| {
            self.current_index.set(Some(prev_idx));
            self.current_track.set(Some(prev_track.clone()));
            self.scrubber_preview_ms.set(None);
            self.position_ms.set(0);
            self.duration_ms.set(prev_track.duration.max(0) as u64);
            self.is_playing.set(true);
            self.playback_status.set(PlaybackStatus::Playing);
        });
        self.on_track_changed(&prev_track);
    }

    pub fn seek(&self, position_ms: u64) {
        let dur = self.duration_ms.get_untracked();
        let clamped = if dur > 0 {
            position_ms.min(dur)
        } else {
            position_ms
        };
        self.pending_seek.replace(Some((clamped, Instant::now())));
        log::info!(
            target: "nsh_music_ui",
            "seek requested: {position_ms}ms -> clamped={clamped}ms"
        );
        self.audio.with(|a| a.seek(clamped));
        batch(|| {
            self.scrubber_preview_ms.set(None);
            self.position_ms.set(clamped);
        });
        self.update_active_lyric(clamped);
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        if (self.volume.get_untracked() - clamped).abs() <= 1e-4 {
            return;
        }
        self.audio.with(|a| a.set_volume(clamped));
        self.volume.set(clamped);
        self.session.with(|s| {
            s.set_volume(clamped);
        });
        self.persist_session();
    }

    pub fn toggle_mute(&self) {
        self.audio.with(|a| a.toggle_mute());
        let next = !self.muted.get_untracked();
        self.muted.set(next);
        self.session.with(|s| {
            s.muted = next;
        });
        self.persist_session();
    }

    pub fn toggle_shuffle(&self) {
        self.audio.with(|a| a.toggle_shuffle());
        let next = !self.shuffle.get_untracked();
        self.shuffle.set(next);
        self.session.with(|s| {
            s.shuffle = next;
        });
        self.persist_session();
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
        });
        self.persist_session();
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
        });
        self.persist_session();

        if self
            .current_track
            .with_untracked(|cur| cur.as_ref().is_some_and(|t| t.id == id))
        {
            self.audio
                .with(|a| a.remote_controller().update_favorite(&id, now_liked));
        }

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
            let res = blocking(
                move || -> Result<Playlist, nsh_music_core::model::SourceError> {
                    let pl = src.create_playlist(&name_owned)?;
                    if let Some(tid) = initial_track_id {
                        let _ = src.add_to_playlist(&pl.id, &[tid]);
                        return Ok(src.playlist(&pl.id).unwrap_or(pl));
                    }
                    Ok(pl)
                },
            )
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
        self.playlists
            .update(|list| list.retain(|p| p.id != id_owned));
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
                    store.invalidate_browse_cache();
                    store.settings.update(|s| {
                        for p in paths {
                            s.add_local_folder(p);
                        }
                    });
                    store.persist_settings();
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
        });
        self.persist_settings();
        self.rescan_local_folders();
    }

    pub fn remove_local_folder(&self, path: &Path) {
        self.settings.update(|s| {
            s.remove_local_folder(path);
        });
        self.persist_settings();
        self.rescan_local_folders();
    }

    pub fn rescan_local_folders(&self) {
        self.invalidate_browse_cache();
        let folders = self.settings.with_untracked(|s| s.local_folders.clone());
        let local = self.local.get();
        let store = *self;
        self.spawn_app(async move {
            let report = blocking(move || local.scan_folders(&folders)).await;
            if let Ok(rep) = report {
                store.invalidate_browse_cache();
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

    /// Returns the listener's last chosen lyrics presentation (`Stage` vs `Lyrics`),
    /// matching `egoist/mygo-music`'s `lyricsPaneChosen`.
    pub fn preferred_lyrics_panel(&self) -> NowPlayingPanel {
        if self.settings.with_untracked(|s| s.lyrics_stage) {
            NowPlayingPanel::Stage
        } else {
            NowPlayingPanel::Lyrics
        }
    }

    pub fn open_now_playing(&self) {
        if !self.now_playing_open.get_untracked() {
            self.now_playing_open.set(true);
        }
    }

    /// Opens Now Playing directly to `panel`.
    pub fn open_now_playing_panel(&self, panel: NowPlayingPanel) {
        self.set_now_playing_panel(panel);
        self.open_now_playing();
    }

    pub fn close_now_playing(&self) {
        batch(|| {
            if self.now_playing_open.get_untracked() {
                self.now_playing_open.set(false);
            }
            if self.stage_fullscreen.get_untracked() {
                self.stage_fullscreen.set(false);
            }
        });
    }

    pub fn set_now_playing_mode(&self, mode: NowPlayingMode) {
        if self.now_playing_mode.get_untracked() == mode {
            return;
        }
        self.now_playing_mode.set(mode);
        self.settings.update(|s| {
            s.default_player_style = PlayerStyle::from(mode);
        });
        self.persist_settings();
    }

    pub fn set_stage_style(&self, style: NowPlayingStageStyle) {
        if self.stage_style.get_untracked() == style {
            return;
        }
        self.stage_style.set(style);
        self.settings.update(|s| {
            s.default_stage_style = style.to_core();
        });
        self.persist_settings();
    }

    pub fn set_now_playing_panel(&self, panel: NowPlayingPanel) {
        if self.now_playing_panel.get_untracked() != panel {
            self.now_playing_panel.set(panel);
        }
        if matches!(panel, NowPlayingPanel::Lyrics | NowPlayingPanel::Stage) {
            let want_stage = panel == NowPlayingPanel::Stage;
            if self.settings.with_untracked(|s| s.lyrics_stage) != want_stage {
                self.settings.update(|s| {
                    s.lyrics_stage = want_stage;
                });
                self.persist_settings();
            }
        }
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
        self.search_suggest_gen.with(|g| *g = g.wrapping_add(1));
        batch(|| {
            self.search_query.set(trimmed.clone());
            self.search_suggest_open.set(false);
            self.search_loading_more.set(false);
        });
        if trimmed.is_empty() {
            self.search_results.set(SearchResult::default());
            self.search_loading.set(false);
            return;
        }

        self.session.with(|s| {
            s.record_search(&trimmed);
        });
        self.persist_session();
        self.recent_searches
            .set(self.session.with(|s| s.recent_searches.clone()));

        let kind = self.search_tab.get_untracked();
        let source_id = self.active_source_id.get_untracked();
        let src = self.active_source();
        self.search_loading.set(true);
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.search(&trimmed, kind, 0)).await;
            if store.active_source_id.get_untracked() != source_id {
                return;
            }
            store.search_loading.set(false);
            match res {
                Ok(Ok(result)) => {
                    store.search_results.set(result);
                    if source_id != LocalMusicSource::SOURCE_ID
                        && store.plugins.with_untracked(|list| {
                            list.iter()
                                .any(|p| p.id == source_id && p.runtime_error.is_some())
                        })
                    {
                        store.refresh_plugin_summaries();
                    }
                }
                Ok(Err(err)) => {
                    if source_id != LocalMusicSource::SOURCE_ID {
                        store.refresh_plugin_summaries();
                    }
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
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
        });
        self.persist_session();
        self.recent_searches.set(Vec::new());
    }

    // -------------------------------------------------------------------------
    // Pagination (`load_more_*`)
    // -------------------------------------------------------------------------

    /// Loads the next page of search results for the active query & tab, deduplicating by ID.
    pub fn load_more_search(&self) {
        if self.search_loading.get_untracked() || self.search_loading_more.get_untracked() {
            return;
        }
        let query = self.search_query.get_untracked();
        let trimmed = query.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        let kind = self.search_tab.get_untracked();
        let (has_more, offset) = self.search_results.with_untracked(|r| {
            let len = match kind {
                SearchKind::Tracks => r.tracks.len(),
                SearchKind::Albums => r.albums.len(),
                SearchKind::Artists => r.artists.len(),
                SearchKind::Playlists => r.playlists.len(),
                SearchKind::Shows => r.shows.len(),
            };
            (r.more, len)
        });
        if !has_more {
            return;
        }

        let source_id = self.active_source_id.get_untracked();
        let src = self.active_source();
        self.search_loading_more.set(true);
        let store = *self;
        self.spawn_app(async move {
            let q_bg = trimmed.clone();
            let res = blocking(move || src.search(&q_bg, kind, offset)).await;
            let still_current = store.active_source_id.get_untracked() == source_id
                && store.search_query.get_untracked().trim() == trimmed
                && store.search_tab.get_untracked() == kind;
            if !still_current {
                return;
            }
            store.search_loading_more.set(false);
            match res {
                Ok(Ok(page)) => {
                    store.search_results.update(|cur| {
                        let added = match kind {
                            SearchKind::Tracks => {
                                let mut seen: HashSet<String> =
                                    cur.tracks.iter().map(|t| t.id.clone()).collect();
                                let mut count = 0usize;
                                for item in page.tracks {
                                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                                        cur.tracks.push(item);
                                        count += 1;
                                    }
                                }
                                count
                            }
                            SearchKind::Albums => {
                                let mut seen: HashSet<String> =
                                    cur.albums.iter().map(|a| a.id.clone()).collect();
                                let mut count = 0usize;
                                for item in page.albums {
                                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                                        cur.albums.push(item);
                                        count += 1;
                                    }
                                }
                                count
                            }
                            SearchKind::Artists => {
                                let mut seen: HashSet<String> =
                                    cur.artists.iter().map(|a| a.id.clone()).collect();
                                let mut count = 0usize;
                                for item in page.artists {
                                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                                        cur.artists.push(item);
                                        count += 1;
                                    }
                                }
                                count
                            }
                            SearchKind::Playlists => {
                                let mut seen: HashSet<String> =
                                    cur.playlists.iter().map(|p| p.id.clone()).collect();
                                let mut count = 0usize;
                                for item in page.playlists {
                                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                                        cur.playlists.push(item);
                                        count += 1;
                                    }
                                }
                                count
                            }
                            SearchKind::Shows => {
                                let mut seen: HashSet<String> =
                                    cur.shows.iter().map(|s| s.id.clone()).collect();
                                let mut count = 0usize;
                                for item in page.shows {
                                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                                        cur.shows.push(item);
                                        count += 1;
                                    }
                                }
                                count
                            }
                        };
                        cur.more = page.more && added > 0;
                    });
                }
                Ok(Err(err)) => {
                    if source_id != LocalMusicSource::SOURCE_ID {
                        store.refresh_plugin_summaries();
                    }
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
            }
        });
    }

    /// Loads the next page of liked tracks for the active source, deduplicating by ID.
    pub fn load_more_liked(&self) {
        if !self.liked_more.get_untracked() || self.liked_loading_more.get_untracked() {
            return;
        }
        let source_id = self.active_source_id.get_untracked();
        let offset = self.liked_tracks.with_untracked(Vec::len);
        let src = self.active_source();
        self.liked_loading_more.set(true);
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.liked(offset)).await;
            if store.active_source_id.get_untracked() != source_id {
                return;
            }
            store.liked_loading_more.set(false);
            match res {
                Ok(Ok(page)) => {
                    let mut added_ids = Vec::new();
                    let mut added = 0usize;
                    store.liked_tracks.update(|list| {
                        let mut seen: HashSet<String> = list.iter().map(|t| t.id.clone()).collect();
                        for track in page.tracks {
                            if track.id.is_empty() || seen.insert(track.id.clone()) {
                                if !track.id.is_empty() {
                                    added_ids.push(track.id.clone());
                                }
                                list.push(track);
                                added += 1;
                            }
                        }
                    });
                    batch(|| {
                        if !added_ids.is_empty() {
                            store.liked_ids.update(|ids| {
                                for id in added_ids {
                                    ids.insert(id);
                                }
                            });
                        }
                        store.liked_more.set(page.more && added > 0);
                    });
                }
                Ok(Err(err)) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
            }
        });
    }

    /// Loads the next page of saved albums for the active source, deduplicating by ID.
    pub fn load_more_saved_albums(&self) {
        if !self.saved_albums_more.get_untracked() || self.saved_albums_loading_more.get_untracked()
        {
            return;
        }
        let source_id = self.active_source_id.get_untracked();
        let offset = self.saved_albums.with_untracked(Vec::len);
        let src = self.active_source();
        self.saved_albums_loading_more.set(true);
        let store = *self;
        self.spawn_app(async move {
            let res = blocking(move || src.saved_albums(offset)).await;
            if store.active_source_id.get_untracked() != source_id {
                return;
            }
            store.saved_albums_loading_more.set(false);
            match res {
                Ok(Ok(page)) => {
                    let mut added = 0usize;
                    store.saved_albums.update(|list| {
                        let mut seen: HashSet<String> = list.iter().map(|a| a.id.clone()).collect();
                        for album in page.albums {
                            if album.id.is_empty() || seen.insert(album.id.clone()) {
                                list.push(album);
                                added += 1;
                            }
                        }
                    });
                    store.saved_albums_more.set(page.more && added > 0);
                }
                Ok(Err(err)) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
            }
        });
    }

    /// Loads the next page of episodes for the currently open podcast show, deduplicating by ID.
    pub fn load_more_show_episodes(&self) {
        if self.show_episodes_loading_more.get_untracked() {
            return;
        }
        let target = self.active_show.with_untracked(|opt| {
            opt.as_ref()
                .filter(|p| p.more && !p.show.id.is_empty())
                .map(|p| (p.show.id.clone(), p.episodes.len()))
        });
        let Some((show_id, offset)) = target else {
            return;
        };
        let source_id = self.active_source_id.get_untracked();
        let src = self.active_source();
        self.show_episodes_loading_more.set(true);
        let store = *self;
        self.spawn_app(async move {
            let sid_bg = show_id.clone();
            let res = blocking(move || src.show(&sid_bg, offset)).await;
            let still_current = store.active_source_id.get_untracked() == source_id
                && store
                    .active_show
                    .with_untracked(|opt| opt.as_ref().is_some_and(|p| p.show.id == show_id));
            if !still_current {
                return;
            }
            store.show_episodes_loading_more.set(false);
            match res {
                Ok(Ok(page)) => {
                    store.active_show.update(|opt| {
                        if let Some(cur) = opt
                            && cur.show.id == show_id
                        {
                            let mut seen: HashSet<String> =
                                cur.episodes.iter().map(|e| e.id.clone()).collect();
                            let mut added = 0usize;
                            for ep in page.episodes {
                                if ep.id.is_empty() || seen.insert(ep.id.clone()) {
                                    cur.episodes.push(ep);
                                    added += 1;
                                }
                            }
                            cur.more = page.more && added > 0;
                        }
                    });
                }
                Ok(Err(err)) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
            }
        });
    }

    /// Loads the next page of listener comments for the currently playing track, deduplicating by ID.
    pub fn load_more_comments(&self) {
        if !self.comments_more.get_untracked()
            || self.comments_loading.get_untracked()
            || self.comments_loading_more.get_untracked()
        {
            return;
        }
        let Some(track) = self.current_track.get_untracked() else {
            return;
        };
        let track_id = track.id.clone();
        let source_id = if track.source.is_empty() {
            self.active_source_id.get_untracked()
        } else {
            track.source.clone()
        };
        let src = self.source_by_id(&source_id);
        let offset = self.comments.with_untracked(Vec::len);
        self.comments_loading_more.set(true);
        let store = *self;
        self.spawn_app(async move {
            let tid_bg = track_id.clone();
            let res = blocking(move || src.comments(&tid_bg, offset)).await;
            let still_current = store
                .last_track_id
                .with(|id| id.as_deref() == Some(track_id.as_str()));
            if !still_current {
                return;
            }
            store.comments_loading_more.set(false);
            match res {
                Ok(Ok(page)) => {
                    let mut added = 0usize;
                    store.comments.update(|list| {
                        let mut seen: HashSet<String> = list.iter().map(|c| c.id.clone()).collect();
                        for c in page.comments {
                            if c.id.is_empty() || seen.insert(c.id.clone()) {
                                list.push(c);
                                added += 1;
                            }
                        }
                    });
                    batch(|| {
                        if page.total > store.comments_total.get_untracked() {
                            store.comments_total.set(page.total);
                        }
                        store.comments_more.set(page.more && added > 0);
                    });
                }
                Ok(Err(err)) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
                Err(err) => {
                    store.toasts.error(format!("{source_id}: {err}"));
                }
            }
        });
    }

    // -------------------------------------------------------------------------
    // Settings, Cache, Experiments, Locale, Theme & Modals
    // -------------------------------------------------------------------------

    pub fn set_locale(&self, locale: Locale) {
        if self.locale.get_untracked() == locale {
            return;
        }
        self.locale.set(locale);
        self.plugin_mgr.with(|m| m.set_locale(locale.code()));
        self.settings.update(|s| {
            s.locale = locale;
        });
        self.persist_settings();
        self.refresh_sources();
    }

    pub fn set_theme(&self, mode: ThemeMode) {
        if self.theme_override.get_untracked() == mode {
            return;
        }
        self.theme_override.set(mode);
        self.apply_theme_mode(mode);
        self.settings.update(|s| {
            s.theme_mode = mode;
        });
        self.persist_settings();
    }

    pub fn set_quality(&self, quality: Quality) {
        self.audio.with(|a| a.set_quality(quality));
        self.settings.update(|s| {
            s.audio_quality = quality;
        });
        self.persist_settings();
    }

    pub fn set_replay_gain(&self, enabled: bool) {
        self.audio.with(|a| a.set_replay_gain_enabled(enabled));
        self.settings.update(|s| {
            s.replay_gain = enabled;
        });
        self.persist_settings();
    }

    pub fn set_crossfade_ms(&self, ms: u32) {
        let clamped = ms.min(nsh_music_core::MAX_CROSSFADE_MS);
        self.audio.with(|a| a.set_crossfade_ms(clamped));
        self.settings.update(|s| {
            s.crossfade_ms = clamped;
        });
        self.persist_settings();
    }

    pub fn set_cache_max_mb(&self, mb: u64) {
        let clamped = mb.clamp(128, 4096);
        let _ = self
            .audio
            .with(|a| a.set_cache_max_bytes(clamped * 1024 * 1024));
        self.settings.update(|s| {
            s.cache_max_mb = clamped;
        });
        self.persist_settings();
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
            store
                .cache_used_bytes
                .set(audio_bytes.saturating_add(art_bytes));
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
        });
        self.persist_settings();
    }

    pub fn set_stage_sparks(&self, on: bool) {
        self.settings.update(|s| {
            s.stage_sparks = on;
        });
        self.persist_settings();
    }

    pub fn copy_track_title(&self, track: &Track) {
        if !track.name.is_empty() {
            let name = track.name.clone();
            self.spawn_app(async move {
                let _ = set_clipboard(name).await;
            });
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
            self.spawn_app(async move {
                let _ = set_clipboard(text).await;
            });
            self.toasts.info(self.t("common.copied"));
        }
    }

    pub fn copy_track_link(&self, track: &Track) {
        if !track.url.is_empty() {
            let url = track.url.clone();
            self.spawn_app(async move {
                let _ = set_clipboard(url).await;
            });
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
                if source_id != LocalMusicSource::SOURCE_ID {
                    store.refresh_plugin_summaries();
                }
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
            if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
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
            let cached_path = store.audio.with(|a| a.cache().get_cached_path(&url));
            let write_res = blocking(move || -> Result<(), String> {
                if let Some(cached) = cached_path
                    && cached.is_file()
                {
                    std::fs::copy(&cached, &dest).map_err(|e| e.to_string())?;
                    return Ok(());
                }
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
        });
        self.persist_settings();
    }

    pub fn set_smoke_progress(&self, on: bool) {
        self.settings.update(|s| {
            s.smoke_progress = on;
        });
        self.persist_settings();
    }

    pub fn set_grow_from_cover(&self, on: bool) {
        self.settings.update(|s| {
            s.grow_from_cover = on;
        });
        self.persist_settings();
    }

    pub fn toggle_sidebar_mine(&self) {
        self.settings.update(|s| {
            s.sidebar_mine_collapsed = !s.sidebar_mine_collapsed;
        });
        self.persist_settings();
    }

    pub fn toggle_sidebar_saved(&self) {
        self.settings.update(|s| {
            s.sidebar_saved_collapsed = !s.sidebar_saved_collapsed;
        });
        self.persist_settings();
    }

    pub fn open_modal(&self, modal: ActiveModal) {
        if matches!(modal, ActiveModal::InstallPlugin) {
            self.plugin_error.set(None);
        }
        self.active_modal.set(modal);
    }

    /// Resolves an active plugin `host.webSignIn(...)` request with `result` and closes the modal.
    pub fn submit_web_sign_in_result(&self, result: WebSignInResult) {
        let responder = self
            .active_web_sign_in
            .with(|slot| slot.lock().ok().and_then(|mut g| g.take()))
            .or_else(|| {
                self.pending_web_sign_in.with(|slot| {
                    slot.lock()
                        .ok()
                        .and_then(|mut g| g.take().map(|(_, tx)| tx))
                })
            });
        if let Some(tx) = responder {
            let _ = tx.try_send(Ok(result));
        }
        if matches!(
            self.active_modal.get_untracked(),
            ActiveModal::WebSignIn { .. }
        ) {
            self.active_modal.set(ActiveModal::None);
        }
    }

    /// Cancels any active or pending plugin `host.webSignIn(...)` request without hanging the plugin worker.
    pub fn cancel_web_sign_in(&self) {
        let active_tx = self
            .active_web_sign_in
            .with(|slot| slot.lock().ok().and_then(|mut g| g.take()));
        if let Some(tx) = active_tx {
            let _ = tx.try_send(Ok(WebSignInResult {
                cancelled: true,
                ..WebSignInResult::default()
            }));
        }
        let pending_tx = self.pending_web_sign_in.with(|slot| {
            slot.lock()
                .ok()
                .and_then(|mut g| g.take().map(|(_, tx)| tx))
        });
        if let Some(tx) = pending_tx {
            let _ = tx.try_send(Ok(WebSignInResult {
                cancelled: true,
                ..WebSignInResult::default()
            }));
        }
        self.has_web_sign_in_request
            .with(|flag| flag.store(false, Ordering::Release));
    }

    pub fn close_modal(&self) {
        if matches!(
            self.active_modal.get_untracked(),
            ActiveModal::WebSignIn { .. }
        ) {
            self.cancel_web_sign_in();
        }
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
        let source_id = self.active_source_id.get_untracked();
        let src = self.active_source();
        let home = src.home().unwrap_or_default();
        let playlists = src.playlists().unwrap_or_default();
        let liked_page = src.liked(0).unwrap_or_default();
        let liked = liked_page.tracks;
        let liked_more = liked_page.more;
        let recent = src.recent().unwrap_or_default();
        let albums_page = src.saved_albums(0).unwrap_or_default();
        let albums = albums_page.albums;
        let albums_more = albums_page.more;
        let stations = src.stations().unwrap_or_default();
        let shows = src.shows().unwrap_or_default();

        let liked_set: HashSet<String> = liked.iter().map(|t| t.id.clone()).collect();
        if let Some(cur) = self.current_track.get_untracked() {
            let is_fav = liked_set.contains(&cur.id);
            self.audio
                .with(|a| a.remote_controller().update_favorite(&cur.id, is_fav));
        }
        self.browse_cache
            .with(|cache| cache.insert((source_id, String::new()), home.clone()));

        batch(|| {
            self.home_data.set(home.clone());
            self.browse_data.set(home);
            self.playlists.set(playlists);
            self.liked_tracks.set(liked);
            self.liked_ids.set(liked_set);
            self.liked_more.set(liked_more);
            self.liked_loading_more.set(false);
            self.recent_tracks.set(recent);
            self.saved_albums.set(albums);
            self.saved_albums_more.set(albums_more);
            self.saved_albums_loading_more.set(false);
            self.stations.set(stations);
            self.podcasts.set(shows);
        });
    }

    pub fn hydrate_active_source_async(&self) {
        let source_id = self.active_source_id.get_untracked();
        let src = self.active_source();
        let store = *self;
        if !self.browse_loading.get_untracked() {
            self.browse_loading.set(true);
        }
        self.spawn_app(async move {
            // Phase 1: Fetch `home()` first so Home renders promptly without waiting
            // on secondary library calls (which may fail or block when signed out).
            let src_home = Arc::clone(&src);
            let home_res = blocking(move || src_home.home()).await;

            if store.active_source_id.get_untracked() != source_id {
                return;
            }

            let mut home_err: Option<String> = None;
            let mut plugin_err = false;
            let mut plugin_ok = false;
            batch(|| {
                if store.browse_loading.get_untracked() {
                    store.browse_loading.set(false);
                }
                match home_res {
                    Ok(Ok(home)) => {
                        plugin_ok = source_id != LocalMusicSource::SOURCE_ID;
                        if !home.sections.is_empty() || source_id == LocalMusicSource::SOURCE_ID {
                            store.browse_cache.with(|cache| {
                                cache.insert((source_id.clone(), String::new()), home.clone());
                            });
                        }
                        if store.home_data.with_untracked(|cur| cur != &home) {
                            store.home_data.set(home.clone());
                        }
                        let on_browse_subpage = matches!(
                            store.route.get_untracked(),
                            Route::Browse { ref page, .. } if !page.is_empty()
                        );
                        if !on_browse_subpage
                            && store.browse_data.with_untracked(|cur| cur != &home)
                        {
                            store.browse_data.set(home);
                        }
                    }
                    Ok(Err(err)) => {
                        plugin_err = source_id != LocalMusicSource::SOURCE_ID;
                        home_err = Some(format!("{source_id}: {err}"));
                    }
                    Err(err) => {
                        home_err = Some(format!("{source_id}: {err}"));
                    }
                }
            });
            if plugin_err
                || (plugin_ok
                    && store.plugins.with_untracked(|list| {
                        list.iter()
                            .any(|p| p.id == source_id && p.runtime_error.is_some())
                    }))
            {
                store.refresh_plugin_summaries();
            }
            if let Some(msg) = home_err {
                store.toasts.error(msg);
            }

            // Phase 2: Hydrate secondary library slices asynchronously after Home settles.
            let lib_bundle = blocking(move || {
                (
                    src.playlists().unwrap_or_default(),
                    src.liked(0).unwrap_or_default(),
                    src.recent().unwrap_or_default(),
                    src.saved_albums(0).unwrap_or_default(),
                    src.stations().unwrap_or_default(),
                    src.shows().unwrap_or_default(),
                )
            })
            .await;

            if store.active_source_id.get_untracked() != source_id {
                return;
            }

            if let Ok((playlists, liked_page, recent, albums_page, stations, shows)) = lib_bundle {
                let liked = liked_page.tracks;
                let liked_more = liked_page.more;
                let albums = albums_page.albums;
                let albums_more = albums_page.more;
                let liked_set: HashSet<String> = liked.iter().map(|t| t.id.clone()).collect();
                if let Some(cur) = store.current_track.get_untracked() {
                    let is_fav = liked_set.contains(&cur.id);
                    store
                        .audio
                        .with(|a| a.remote_controller().update_favorite(&cur.id, is_fav));
                }
                batch(|| {
                    store.playlists.set(playlists);
                    store.liked_tracks.set(liked);
                    store.liked_ids.set(liked_set);
                    store.liked_more.set(liked_more);
                    store.liked_loading_more.set(false);
                    store.recent_tracks.set(recent);
                    store.saved_albums.set(albums);
                    store.saved_albums_more.set(albums_more);
                    store.saved_albums_loading_more.set(false);
                    store.stations.set(stations);
                    store.podcasts.set(shows);
                });
            }
        });
    }

    fn load_data_for_route(&self, route: &Route) {
        let src = self.active_source();
        let source_id = self.active_source_id.get_untracked();
        let store = *self;
        match route {
            Route::Home => {
                let cached = self
                    .browse_cache
                    .with(|cache| cache.contains_key(&(source_id, String::new())));
                if !cached && self.home_data.with_untracked(|h| h.sections.is_empty()) {
                    self.hydrate_active_source_async();
                }
            }
            Route::Browse { page, title } => {
                self.load_browse_if_needed(page, title);
            }
            Route::PlaylistDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    let id_bg = id.clone();
                    let res = blocking(move || src.playlist(&id_bg)).await;
                    if store.active_source_id.get_untracked() != source_id {
                        return;
                    }
                    match res {
                        Ok(Ok(mut pl)) => {
                            if pl.id.is_empty() {
                                pl.id = id;
                            }
                            store.active_playlist.set(Some(pl));
                        }
                        Ok(Err(err)) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_playlist.set(Some(Playlist {
                                id,
                                ..Default::default()
                            }));
                        }
                        Err(err) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_playlist.set(Some(Playlist {
                                id,
                                ..Default::default()
                            }));
                        }
                    }
                });
            }
            Route::AlbumDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    let id_bg = id.clone();
                    let res = blocking(move || src.album(&id_bg)).await;
                    if store.active_source_id.get_untracked() != source_id {
                        return;
                    }
                    match res {
                        Ok(Ok(mut alb)) => {
                            if alb.album.id.is_empty() {
                                alb.album.id = id;
                            }
                            store.active_album.set(Some(alb));
                        }
                        Ok(Err(err)) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_album.set(Some(AlbumPage {
                                album: Album {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
                        Err(err) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_album.set(Some(AlbumPage {
                                album: Album {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
                    }
                });
            }
            Route::ArtistDetail { id } => {
                let id = id.clone();
                self.spawn_app(async move {
                    let id_bg = id.clone();
                    let res = blocking(move || src.artist(&id_bg)).await;
                    if store.active_source_id.get_untracked() != source_id {
                        return;
                    }
                    match res {
                        Ok(Ok(mut art)) => {
                            if art.artist.id.is_empty() {
                                art.artist.id = id;
                            }
                            store.active_artist.set(Some(art));
                        }
                        Ok(Err(err)) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_artist.set(Some(ArtistPage {
                                artist: Artist {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
                        Err(err) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_artist.set(Some(ArtistPage {
                                artist: Artist {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
                    }
                });
            }
            Route::ShowDetail { id } => {
                let id = id.clone();
                self.show_episodes_loading_more.set(false);
                self.spawn_app(async move {
                    let id_bg = id.clone();
                    let res = blocking(move || src.show(&id_bg, 0)).await;
                    if store.active_source_id.get_untracked() != source_id {
                        return;
                    }
                    match res {
                        Ok(Ok(mut show_page)) => {
                            if show_page.show.id.is_empty() {
                                show_page.show.id = id;
                            }
                            store.active_show.set(Some(show_page));
                        }
                        Ok(Err(err)) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_show.set(Some(ShowPage {
                                show: Show {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
                        Err(err) => {
                            store.toasts.error(format!("{source_id}: {err}"));
                            store.active_show.set(Some(ShowPage {
                                show: Show {
                                    id,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }));
                        }
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
            Route::SettingsDetail(section) => {
                self.refresh_cache_stats();
                if *section == SettingsSection::Plugins {
                    self.refresh_sources();
                }
            }
            Route::Plugins => {
                self.refresh_sources();
            }
            _ => {}
        }
    }

    fn persist_settings(&self) {
        let snapshot = self.settings.get_untracked();
        let path = self.settings_path.get();
        let slot = self.pending_settings_save.get();
        let saving = self.settings_saving.get();
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(snapshot);
        }
        if !saving.swap(true, Ordering::AcqRel) {
            self.spawn_app(async move {
                let _ = blocking(move || {
                    loop {
                        let next = slot.lock().ok().and_then(|mut g| g.take());
                        match next {
                            Some(cfg) => {
                                let _ = cfg.save_to(&path);
                            }
                            None => {
                                saving.store(false, Ordering::Release);
                                let has_more = slot.lock().ok().is_some_and(|g| g.is_some());
                                if !has_more || saving.swap(true, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                    }
                })
                .await;
            });
        }
    }

    fn persist_session(&self) {
        let snapshot = self.session.with(|s| s.clone());
        let path = self.session_path.get();
        let slot = self.pending_session_save.get();
        let saving = self.session_saving.get();
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(snapshot);
        }
        if !saving.swap(true, Ordering::AcqRel) {
            self.spawn_app(async move {
                let _ = blocking(move || {
                    loop {
                        let next = slot.lock().ok().and_then(|mut g| g.take());
                        match next {
                            Some(sess) => {
                                let _ = sess.save_to(&path);
                            }
                            None => {
                                saving.store(false, Ordering::Release);
                                let has_more = slot.lock().ok().is_some_and(|g| g.is_some());
                                if !has_more || saving.swap(true, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                    }
                })
                .await;
            });
        }
    }

    fn on_track_changed(&self, track: &Track) {
        self.last_played_reported_id.replace(None);
        self.scrubber_preview_ms.set(None);

        self.recent_tracks.update(|list| {
            list.retain(|t| t.id != track.id);
            list.insert(0, track.clone());
            if list.len() > 50 {
                list.truncate(50);
            }
        });
        if track.is_episode() {
            if self.now_playing_panel.get_untracked() != NowPlayingPanel::Podcast {
                self.now_playing_panel.set(NowPlayingPanel::Podcast);
            }
        } else if self.now_playing_panel.get_untracked() == NowPlayingPanel::Podcast {
            self.now_playing_panel.set(self.preferred_lyrics_panel());
        }
        self.load_track_extras(track);
    }

    fn load_track_extras(&self, track: &Track) {
        let track_id = track.id.clone();
        self.last_track_id.replace(Some(track_id.clone()));

        let source_id = if track.source.is_empty() {
            self.active_source_id.get_untracked()
        } else {
            track.source.clone()
        };
        let src_lrc = self.source_by_id(&source_id);
        let can_comments = src_lrc.can("comments");

        let liked = self.liked_ids.with_untracked(|s| s.contains(&track.id));
        self.audio.with(|a| {
            let remote = a.remote_controller();
            remote.update_favorite(&track.id, liked);
            remote.update_lyric(&track_id, "");
        });

        batch(|| {
            self.lyrics.set(ParsedLyrics::default());
            self.active_lyric.set(None);
            self.lyrics_loading.set(true);
            self.comments.set(Vec::new());
            self.comments_total.set(0);
            self.comments_more.set(false);
            self.comments_loading_more.set(false);
            self.comments_loading.set(can_comments);
        });

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

        let trimmed_cover = raw_cover.trim();
        let mut artwork_resolved_sync = false;
        if trimmed_cover.is_empty() {
            // No real cover URL to resolve; apply deterministic fallback accent immediately.
            let fallback = ArtworkLoader::accent_for_seed(&seed);
            if self.current_accent.get_untracked() != fallback {
                self.current_accent.set(fallback);
            }
        } else {
            // Preserve the previous track's accent until the real cover resolves so the
            // Now Playing backdrop never flashes a random seed color (matching mygo-music).
            // If already cached in memory, apply immediately without waiting.
            let expanded = ArtworkLoader::resolve_template(trimmed_cover, 600);
            if let Some(hit) = self.artwork.with(|l| l.cached(&expanded, 600)) {
                artwork_resolved_sync = true;
                self.audio.with(|a| {
                    a.remote_controller()
                        .update_artwork(&track_id, hit.uri.as_str());
                });
                batch(|| {
                    if self.current_artwork_uri.get_untracked() != hit.uri {
                        self.current_artwork_uri.set(hit.uri);
                    }
                    if self.current_accent.get_untracked() != hit.accent {
                        self.current_accent.set(hit.accent);
                    }
                });
            }
        }

        if !artwork_resolved_sync {
            let src = self.source_by_id(&track.source);
            let headers = src.request_headers_for(&raw_cover, RequestKind::Image);
            let loader = self.artwork.get();
            let store = *self;

            self.spawn_app(async move {
                let resolved =
                    background(async move { loader.load(&raw_cover, 600, &seed, &headers).await })
                        .await;
                if let Ok(art) = resolved
                    && store
                        .last_track_id
                        .with(|id| id.as_deref() == Some(&track_id))
                {
                    store.audio.with(|a| {
                        a.remote_controller()
                            .update_artwork(&track_id, art.uri.as_str());
                    });
                    batch(|| {
                        if store.current_artwork_uri.get_untracked() != art.uri {
                            store.current_artwork_uri.set(art.uri);
                        }
                        if store.current_accent.get_untracked() != art.accent {
                            store.current_accent.set(art.accent);
                        }
                    });
                }
            });
        }

        let tid_lrc = track.id.clone();
        let tid_check = track.id.clone();
        let store_lrc = *self;
        self.spawn_app(async move {
            let res = blocking(move || {
                let (lyrics, lrc_for_remote) = match src_lrc.lyrics(&tid_lrc) {
                    Ok(raw_lyrics) => {
                        let parsed = parse_lyrics(&raw_lyrics);
                        let remote_lrc = if !raw_lyrics.lrc.trim().is_empty() {
                            raw_lyrics.lrc
                        } else {
                            parsed.to_lrc_string()
                        };
                        (parsed, remote_lrc)
                    }
                    Err(err) => {
                        if !matches!(err, nsh_music_core::model::SourceError::Unsupported) {
                            log::warn!(
                                target: "nsh_music",
                                "lyrics fetch failed for `{}` on `{}`: {err}",
                                tid_lrc,
                                source_id
                            );
                        }
                        (ParsedLyrics::default(), String::new())
                    }
                };
                let comments = if can_comments {
                    src_lrc.comments(&tid_lrc, 0).unwrap_or_default()
                } else {
                    nsh_music_core::model::CommentPage::default()
                };
                (lyrics, lrc_for_remote, comments)
            })
            .await;

            if store_lrc
                .last_track_id
                .with(|id| id.as_deref() == Some(tid_check.as_str()))
            {
                match res {
                    Ok((parsed, lrc_for_remote, comment_page)) => {
                        store_lrc.audio.with(|a| {
                            a.remote_controller()
                                .update_lyric(&tid_check, &lrc_for_remote);
                        });
                        let pos = store_lrc.position_ms.get_untracked();
                        let active = parsed.active_position(pos as i64);
                        batch(|| {
                            store_lrc.lyrics.set(parsed);
                            store_lrc.active_lyric.set(active);
                            store_lrc.comments.set(comment_page.comments);
                            store_lrc.comments_total.set(comment_page.total);
                            store_lrc.comments_more.set(comment_page.more);
                            store_lrc.comments_loading_more.set(false);
                            store_lrc.lyrics_loading.set(false);
                            store_lrc.comments_loading.set(false);
                            if !can_comments
                                && store_lrc.now_playing_panel.get_untracked()
                                    == NowPlayingPanel::Comments
                            {
                                store_lrc
                                    .now_playing_panel
                                    .set(store_lrc.preferred_lyrics_panel());
                            }
                        });
                    }
                    Err(_) => {
                        batch(|| {
                            store_lrc.lyrics_loading.set(false);
                            store_lrc.comments_loading.set(false);
                            store_lrc.comments_more.set(false);
                            store_lrc.comments_loading_more.set(false);
                        });
                    }
                }
            }
        });
    }

    fn update_active_lyric(&self, position_ms: u64) {
        let (next, word_synced) = self
            .lyrics
            .with_untracked(|lrc| (lrc.active_position(position_ms as i64), lrc.word_synced));
        let prev = self.active_lyric.get_untracked();
        let should_update = match (prev, next) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(p), Some(n)) => {
                if p.line_index != n.line_index || p.word_index != n.word_index {
                    true
                } else {
                    let np_open = self.now_playing_open.get_untracked();
                    let panel = self.now_playing_panel.get_untracked();
                    let wants_progress =
                        np_open && (panel == NowPlayingPanel::Stage || word_synced);
                    wants_progress
                        && ((p.word_progress - n.word_progress).abs() >= 0.015
                            || (p.line_progress - n.line_progress).abs() >= 0.02)
                }
            }
        };
        if should_update {
            self.active_lyric.set(next);
        }
    }

    fn needs_spectrum(&self) -> bool {
        if self.now_playing_open.get_untracked() {
            return true;
        }
        if self.settings.with_untracked(|s| s.smoke_progress) {
            return true;
        }
        !matches!(
            self.route.get_untracked(),
            Route::Settings
                | Route::SettingsDetail(_)
                | Route::Plugins
                | Route::Radio
                | Route::Podcasts
                | Route::Playlists
        )
    }

    fn sync_from_audio_engine(&self) {
        if self
            .has_sign_in_prompt
            .with(|flag| flag.swap(false, Ordering::AcqRel))
            && let Some((source_id, prompt)) = self
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
            self.active_modal.set(ActiveModal::QrSignIn {
                source_id: sid,
                prompt,
            });
        }

        if self
            .has_web_sign_in_request
            .with(|flag| flag.swap(false, Ordering::AcqRel))
            && let Some((req, tx)) = self
                .pending_web_sign_in
                .with(|m| m.lock().ok().and_then(|mut g| g.take()))
        {
            self.active_web_sign_in.with(|slot| {
                if let Ok(mut guard) = slot.lock()
                    && let Some(prev_tx) = guard.replace(tx)
                {
                    let _ = prev_tx.try_send(Ok(WebSignInResult {
                        cancelled: true,
                        ..WebSignInResult::default()
                    }));
                }
            });
            let sid = if !req.plugin_id.is_empty() {
                req.plugin_id
            } else {
                self.signing_in_source_id
                    .with(|s| s.clone())
                    .unwrap_or_else(|| self.active_source_id.get_untracked())
            };
            self.active_modal.set(ActiveModal::WebSignIn {
                source_id: sid,
                url: req.url,
                title: req.title,
                cookies_of: req.cookies_of,
                until: req.until,
                user_agent: req.user_agent,
                allow_manual_cookie: req.allow_manual_cookie,
            });
        }

        if self
            .has_open_url_request
            .with(|flag| flag.swap(false, Ordering::AcqRel))
        {
            let urls = self.pending_open_urls.with(|m| {
                m.lock()
                    .map(|mut g| std::mem::take(&mut *g))
                    .unwrap_or_default()
            });
            for url in urls {
                self.spawn_app(async move {
                    let _ = nsh_core::element::open_url(url).await;
                });
            }
        }

        if self
            .pending_remote_toggle_favorite
            .with(|f| f.swap(false, Ordering::AcqRel))
        {
            self.toggle_like_current();
        }

        let snap = self.audio.with(|a| a.snapshot());

        let snapshot_changed = self.last_snapshot.with(|prev| prev.as_ref() != Some(&snap));

        if snapshot_changed {
            let engine_has_track = snap.current_track.is_some();
            let engine_playing = snap.status.is_playing();
            let status_changed = self.playback_status.with_untracked(|s| *s != snap.status);
            let track_changed = snap.current_track.as_ref().is_some_and(|tr| {
                self.last_track_id
                    .with(|prev| prev.as_deref() != Some(tr.id.as_str()))
            });
            let queue_changed = !snap.queue.is_empty()
                && self
                    .last_snapshot
                    .with(|prev| prev.as_ref().is_none_or(|p| p.queue != snap.queue));
            let track_meta_changed = track_changed
                || self.last_snapshot.with(|prev| {
                    prev.as_ref().and_then(|p| p.current_track.as_ref())
                        != snap.current_track.as_ref()
                });

            self.last_snapshot.replace(Some(snap.clone()));

            let mut pos_updated = false;
            let mut playback_error_toast: Option<String> = None;
            let mut loop_mode_changed = false;
            batch(|| {
                if engine_has_track || snap.status != PlaybackStatus::Stopped {
                    if status_changed {
                        self.playback_status.set(snap.status.clone());
                        if let PlaybackStatus::Error(ref msg) = snap.status {
                            log::warn!(target: "nsh_music_ui", "playback error: {msg}");
                            playback_error_toast = Some(msg.clone());
                        }
                    }
                    if self.is_playing.get_untracked() != engine_playing {
                        self.is_playing.set(engine_playing);
                    }

                    let skip_stale_pos = if let Some((target, at)) = self.pending_seek.with(|p| *p)
                    {
                        let settled_or_expired = track_changed
                            || matches!(
                                snap.status,
                                PlaybackStatus::Stopped | PlaybackStatus::Error(_)
                            )
                            || at.elapsed() >= Duration::from_millis(1500)
                            || snap.position_ms.abs_diff(target) <= 2_500;
                        if settled_or_expired {
                            self.pending_seek.replace(None);
                            false
                        } else {
                            true
                        }
                    } else {
                        false
                    };

                    let cur_pos = self.position_ms.get_untracked();
                    if !skip_stale_pos && snap.position_ms != cur_pos {
                        let np_open = self.now_playing_open.get_untracked();
                        let panel = self.now_playing_panel.get_untracked();
                        let fine_pos = np_open
                            && (panel == NowPlayingPanel::Stage
                                || (panel == NowPlayingPanel::Lyrics
                                    && self.lyrics.with_untracked(|l| l.word_synced))
                                || self.settings.with_untracked(|s| s.smoke_progress));
                        let pos_threshold: u64 = if fine_pos { 30 } else { 100 };
                        let boundary_hit = snap.position_ms == 0
                            || (snap.duration_ms > 0 && snap.position_ms >= snap.duration_ms);
                        if status_changed
                            || track_changed
                            || boundary_hit
                            || snap.position_ms < cur_pos
                            || snap.position_ms.abs_diff(cur_pos) >= pos_threshold
                        {
                            self.position_ms.set(snap.position_ms);
                            pos_updated = true;
                        }
                    }

                    if snap.duration_ms > 0 && self.duration_ms.get_untracked() != snap.duration_ms
                    {
                        self.duration_ms.set(snap.duration_ms);
                    }
                    let cur_buf = self.buffered_ms.get_untracked();
                    if snap.buffered_ms != cur_buf
                        && (snap.buffered_ms.abs_diff(cur_buf) >= 250
                            || snap.buffered_ms == snap.duration_ms
                            || snap.buffered_ms == 0)
                    {
                        self.buffered_ms.set(snap.buffered_ms);
                    }
                    if (self.volume.get_untracked() - snap.volume).abs() > 1e-4 {
                        self.volume.set(snap.volume);
                    }
                    if self.muted.get_untracked() != snap.muted {
                        self.muted.set(snap.muted);
                    }
                    if self.shuffle.get_untracked() != snap.shuffle {
                        self.shuffle.set(snap.shuffle);
                        loop_mode_changed = true;
                    }
                    if self.repeat.get_untracked() != snap.repeat {
                        self.repeat.set(snap.repeat);
                        loop_mode_changed = true;
                    }
                    if self.current_is_lossless.get_untracked() != snap.is_lossless {
                        self.current_is_lossless.set(snap.is_lossless);
                    }
                } else if self.current_is_lossless.get_untracked() {
                    self.current_is_lossless.set(false);
                }

                if queue_changed {
                    self.queue.set(snap.queue.clone());
                }
                if !snap.queue.is_empty()
                    && self.current_index.get_untracked() != snap.current_index
                {
                    self.current_index.set(snap.current_index);
                }
                if let Some(ref tr) = snap.current_track {
                    if track_meta_changed {
                        self.current_track.set(Some(tr.clone()));
                    }
                    if track_changed {
                        self.on_track_changed(tr);
                    }
                } else if snap.status == PlaybackStatus::Stopped {
                    if self.lyrics_loading.get_untracked() {
                        self.lyrics_loading.set(false);
                    }
                    if self.comments_loading.get_untracked() {
                        self.comments_loading.set(false);
                    }
                }
            });

            if let Some(msg) = playback_error_toast {
                self.toasts.error(msg);
            }
            if status_changed
                && (matches!(snap.status, PlaybackStatus::Error(_))
                    || (snap.status == PlaybackStatus::Playing
                        && self
                            .plugins
                            .with_untracked(|list| list.iter().any(|p| p.runtime_error.is_some()))))
            {
                self.refresh_plugin_summaries();
            }
            if loop_mode_changed {
                self.session.with(|s| {
                    s.shuffle = snap.shuffle;
                    s.repeat = snap.repeat;
                });
                self.persist_session();
            }

            if snap.status == PlaybackStatus::Playing
                && let Some(ref tr) = snap.current_track
                && self
                    .last_played_reported_id
                    .with(|prev| prev.as_deref() != Some(tr.id.as_str()))
            {
                self.last_played_reported_id.replace(Some(tr.id.clone()));
                let src = self.source_by_id(&tr.source);
                let played_track = tr.clone();
                self.spawn_app(async move {
                    let _ = blocking(move || {
                        src.played(&played_track);
                    })
                    .await;
                });
            }

            if engine_has_track && (pos_updated || track_changed || status_changed) {
                self.update_active_lyric(snap.position_ms);
            }
        }

        if self.is_playing.get_untracked() && self.needs_spectrum() {
            let spec = self.audio.with(|a| a.spectrum());
            let changed = self.spectrum.with_untracked(|prev| {
                prev.iter()
                    .zip(spec.iter())
                    .any(|(a, b)| (a - b).abs() >= 0.015)
            });
            if changed {
                self.spectrum.set(spec);
                self.spectrum_is_zero.replace(false);
            }
        } else if !self.spectrum_is_zero.get() {
            self.spectrum.set([0.0; SPECTRUM_BANDS]);
            self.spectrum_is_zero.replace(true);
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
    let mt = media_type
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SandboxPaths {
    settings_path: PathBuf,
    session_path: PathBuf,
    local_covers_dir: PathBuf,
    local_library_path: PathBuf,
    plugins_dir: PathBuf,
    plugin_data_dir: PathBuf,
    audio_cache_dir: PathBuf,
    artwork_cache_dir: PathBuf,
}

fn sandbox_root_dir() -> PathBuf {
    if let Some(dir) = nsh_core::element::data_dir()
        && std::fs::create_dir_all(&dir).is_ok()
    {
        return dir;
    }
    #[cfg(target_env = "ohos")]
    {
        let fallback = PathBuf::from("/data/storage/el2/base/haps/entry/files/nsh-music");
        if std::fs::create_dir_all(&fallback).is_ok() {
            return fallback;
        }
    }
    let fallback = std::env::temp_dir().join("nsh-music");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

fn sandbox_paths_from_root(root: &Path) -> SandboxPaths {
    SandboxPaths {
        settings_path: root.join("settings.json"),
        session_path: root.join("session.json"),
        local_covers_dir: root.join("local-covers"),
        local_library_path: root.join("local-library.json"),
        plugins_dir: root.join("plugins"),
        plugin_data_dir: root.join("plugin-data"),
        audio_cache_dir: root.join("audio-cache"),
        artwork_cache_dir: root.join("artwork-cache"),
    }
}

#[cfg(test)]
impl AppStore {
    fn new_isolated_for_test(root: &Path) -> Self {
        let sandbox = sandbox_paths_from_root(root);
        let settings = AppSettings::default();
        let session = PlaybackSession::default();
        let local = Arc::new(LocalMusicSource::with_storage(
            Vec::new(),
            sandbox.local_covers_dir.clone(),
            Some(sandbox.local_library_path.clone()),
        ));
        let plugin_mgr = Arc::new(
            PluginManager::new(&sandbox.plugins_dir, &sandbox.plugin_data_dir)
                .expect("init isolated plugin manager"),
        );
        plugin_mgr.set_mobile_virtual_compat(settings.mobile_virtual_compat);
        let mut audio_cfg = AudioEngineConfig::default();
        audio_cfg.cache.cache_dir = sandbox.audio_cache_dir.clone();
        let audio =
            AudioEngine::with_output(audio_cfg, Box::new(nsh_music_audio::NullOutput::new()))
                .expect("init isolated audio engine");
        let artwork = ArtworkLoader::new(sandbox.artwork_cache_dir.clone());

        Self {
            route: signal(Route::Home),
            nav_history: signal(vec![Route::Home]),
            nav_forward: signal(Vec::new()),
            page_scrolled: signal(false),
            sidebar_collapsed: signal(false),
            mobile_drawer_open: signal(false),

            active_source_id: signal(LocalMusicSource::SOURCE_ID.to_owned()),
            sources: signal(Vec::new()),
            plugins: signal(Vec::new()),
            plugin_busy: signal(false),
            plugin_error: signal(None),
            signing_in_source: signal(None),
            backend_caps: signal(audio_backend_capabilities()),

            playback_status: signal(PlaybackStatus::Stopped),
            is_playing: signal(false),
            current_track: signal(None),
            queue: signal(Vec::new()),
            current_index: signal(None),
            position_ms: signal(0),
            scrubber_preview_ms: signal(None),
            duration_ms: signal(0),
            buffered_ms: signal(0),
            volume: signal(1.0),
            muted: signal(false),
            shuffle: signal(false),
            repeat: signal(RepeatMode::Off),
            spectrum: signal([0.0; SPECTRUM_BANDS]),
            current_is_lossless: signal(false),
            current_artwork_uri: signal(SharedString::default()),
            current_accent: signal(theme::default_artwork_accent()),
            remote_devices: signal(Vec::new()),
            active_remote_device: signal(None),
            cache_used_bytes: signal(0),

            lyrics: signal(ParsedLyrics::default()),
            lyrics_loading: signal(false),
            active_lyric: signal(None),
            show_translation: signal(true),
            show_romaji: signal(false),
            comments: signal(Vec::new()),
            comments_total: signal(0),
            comments_loading: signal(false),
            comments_more: signal(false),
            comments_loading_more: signal(false),

            now_playing_open: signal(false),
            now_playing_mode: signal(NowPlayingMode::Cover),
            stage_style: signal(NowPlayingStageStyle::Flow),
            now_playing_panel: signal(NowPlayingPanel::Lyrics),
            stage_fullscreen: signal(false),

            search_query: signal(String::new()),
            search_tab: signal(SearchKind::Tracks),
            search_results: signal(SearchResult::default()),
            search_loading: signal(false),
            search_loading_more: signal(false),
            recent_searches: signal(Vec::new()),
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
            liked_more: signal(false),
            liked_loading_more: signal(false),
            recent_tracks: signal(Vec::new()),
            saved_albums: signal(Vec::new()),
            saved_albums_more: signal(false),
            saved_albums_loading_more: signal(false),
            stations: signal(Vec::new()),
            podcasts: signal(Vec::new()),
            active_show: signal(None),
            show_episodes_loading_more: signal(false),

            settings: signal(settings),
            main_window_open: signal(true),
            settings_window_open: signal(false),
            settings_window_focus_seq: signal(0),
            settings_section: signal(SettingsSection::General),
            locale: signal(Locale::En),
            theme_override: signal(ThemeMode::System),
            active_modal: signal(ActiveModal::None),
            toasts: ToastController::new(),

            local: stored(local),
            plugin_mgr: stored(plugin_mgr),
            audio: stored(audio),
            artwork: stored(artwork),
            session: stored(session),
            settings_path: stored(sandbox.settings_path),
            session_path: stored(sandbox.session_path),
            last_track_id: stored(None),
            last_played_reported_id: stored(None),
            last_snapshot: stored(None),
            pending_seek: stored(None),
            active_station_id: stored(None),
            pending_sign_in_prompt: stored(Arc::new(Mutex::new(None))),
            has_sign_in_prompt: stored(Arc::new(AtomicBool::new(false))),
            pending_web_sign_in: stored(Arc::new(Mutex::new(None))),
            has_web_sign_in_request: stored(Arc::new(AtomicBool::new(false))),
            active_web_sign_in: stored(Arc::new(Mutex::new(None))),
            pending_open_urls: stored(Arc::new(Mutex::new(Vec::new()))),
            has_open_url_request: stored(Arc::new(AtomicBool::new(false))),
            pending_remote_toggle_favorite: stored(Arc::new(AtomicBool::new(false))),
            signing_in_source_id: stored(None),
            search_suggest_gen: stored(0),
            browse_cache: stored(HashMap::new()),
            browse_req_gen: stored(0),
            spectrum_is_zero: stored(true),
            pending_settings_save: stored(Arc::new(Mutex::new(None))),
            settings_saving: stored(Arc::new(AtomicBool::new(false))),
            pending_session_save: stored(Arc::new(Mutex::new(None))),
            session_saving: stored(Arc::new(AtomicBool::new(false))),
            root_owner: Owner::current(),
        }
    }
}

/// Parses an upstream `mygo-music` / `nsh-music` `install-plugin` deep link and
/// returns the trimmed repository specifier or URL when valid.
///
/// Accepts case-insensitive schemes `mygo-music`, `mygo-music-dev`, and
/// `nsh-music`, with action `install-plugin` in either hierarchical authority
/// form (`mygo-music://install-plugin?repo=...`, `mygo-music://install-plugin/?repo=...`)
/// or opaque / empty-host path form (`mygo-music:install-plugin?repo=...`,
/// `mygo-music:///install-plugin?repo=...`). Prefers query param `repo` and
/// falls back to `url`.
pub fn parse_install_plugin_deep_link(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url.trim()).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "mygo-music" | "mygo-music-dev" | "nsh-music"
    ) {
        return None;
    }

    let is_install_plugin = match parsed.host_str() {
        Some(host) if !host.is_empty() => {
            host.eq_ignore_ascii_case("install-plugin")
                && parsed.path().trim_matches('/').is_empty()
        }
        _ => parsed
            .path()
            .trim_matches('/')
            .eq_ignore_ascii_case("install-plugin"),
    };
    if !is_install_plugin {
        return None;
    }

    let mut repo_val: Option<String> = None;
    let mut url_val: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            continue;
        }
        if k.eq_ignore_ascii_case("repo") && repo_val.is_none() {
            repo_val = Some(trimmed.to_owned());
        } else if k.eq_ignore_ascii_case("url") && url_val.is_none() {
            url_val = Some(trimmed.to_owned());
        }
    }
    repo_val.or(url_val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_music_audio::NullOutput;

    #[component]
    fn AuthStoreTestApp() -> impl IntoElement {
        let root =
            std::env::temp_dir().join(format!("nsh-music-auth-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = AppStore::new_isolated_for_test(&root);

        assert!(store.signing_in_source.get_untracked().is_none());

        store.sources.set(vec![SourceEntry {
            info: SourceInfo {
                id: "ytm-test".to_owned(),
                name: "YouTube Music".to_owned(),
                ..SourceInfo::default()
            },
            compat: SourceCompat::default(),
            is_local: false,
            account: None,
            capabilities: HashSet::new(),
        }]);

        store.apply_signed_in_account(
            "ytm-test",
            Account {
                id: "u42".to_owned(),
                name: "Alice".to_owned(),
                image_url: String::new(),
                page: String::new(),
            },
        );

        let applied = store.sources.with_untracked(|list| {
            list.iter()
                .find(|e| e.info.id == "ytm-test")
                .and_then(|e| e.account.clone())
        });
        assert_eq!(
            applied.as_ref().map(|a| (a.id.as_str(), a.name.as_str())),
            Some(("u42", "Alice"))
        );

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        store.active_web_sign_in.with(|slot| {
            *slot.lock().unwrap() = Some(tx);
        });
        store.open_modal(ActiveModal::WebSignIn {
            source_id: "ytm-test".to_owned(),
            url: "https://music.youtube.com/".to_owned(),
            title: "Sign In".to_owned(),
            cookies_of: Some("https://music.youtube.com".to_owned()),
            until: vec!["SAPISID".to_owned()],
            user_agent: None,
            allow_manual_cookie: true,
        });
        assert!(matches!(
            store.active_modal.get_untracked(),
            ActiveModal::WebSignIn { .. }
        ));

        store.submit_web_sign_in_result(WebSignInResult {
            ok: true,
            url: "https://music.youtube.com/".to_owned(),
            cookie: "SAPISID=tok".to_owned(),
            ..WebSignInResult::default()
        });
        assert_eq!(store.active_modal.get_untracked(), ActiveModal::None);
        let received = rx
            .try_recv()
            .expect("responder channel receives result")
            .expect("result is Ok");
        assert!(received.ok);
        assert_eq!(received.cookie, "SAPISID=tok");

        store.audio.with(|a| a.shutdown());
        let _ = std::fs::remove_dir_all(&root);

        view()
            .test_id("auth-store-test-ok")
            .w(px(120.0))
            .h(px(40.0))
            .child(text("ok"))
    }

    #[test]
    fn apply_signed_in_account_and_submit_web_sign_in_result_work() {
        let viewport = nsh_core::geometry::Size::new(
            nsh_core::geometry::Points(400.0),
            nsh_core::geometry::Points(300.0),
        );
        let root = nsh_engine::Root::of::<AuthStoreTestApp>()
            .tokens(crate::app_tokens())
            .app_id("com.richerfu.nshmusic.auth_store_test");
        let mut config = nsh_engine::EngineConfig::new(root);
        config.viewport = viewport;
        config.environment = nsh_engine::Environment {
            window: viewport,
            ..nsh_engine::Environment::default()
        };
        let mut app = nsh_testing::Headless::start(config).expect("headless auth test starts");
        app.wait_for("id:auth-store-test-ok")
            .expect("auth store assertions passed");
    }

    #[component]
    fn BrowseCacheStoreTestApp() -> impl IntoElement {
        let root = std::env::temp_dir().join(format!(
            "nsh-music-browse-cache-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = AppStore::new_isolated_for_test(&root);
        store.hydrate_active_source_sync();

        let browse_root = Route::Browse {
            page: String::new(),
            title: String::new(),
        };

        // Track reactive route notifications to verify repeated switch_tab is a true no-op.
        let route_writes = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let route_writes_eff = std::rc::Rc::clone(&route_writes);
        effect(move || {
            let _ = store.route.get();
            route_writes_eff.set(route_writes_eff.get() + 1);
        });

        // 1. Home -> Browse hits cache populated by hydrate_active_source_sync (browse_loading stays false).
        store.switch_tab(browse_root.clone());
        assert_eq!(store.route.get_untracked(), browse_root);
        assert!(
            !store.browse_loading.get_untracked(),
            "Home -> Browse must hit browse_cache without setting browse_loading"
        );

        // 2. Repeated switch_tab to active Browse tab is a no-op with zero redundant route updates.
        let writes_before = route_writes.get();
        store.switch_tab(browse_root.clone());
        store.switch_tab(browse_root.clone());
        store.navigate(browse_root.clone());
        assert_eq!(
            route_writes.get(),
            writes_before,
            "repeated switch_tab / navigate to active Browse must not emit redundant route writes"
        );
        assert!(!store.browse_loading.get_untracked());

        // 3. Home -> Browse -> Library -> Browse hits cache with browse_loading == false every step.
        store.switch_tab(Route::Library);
        assert_eq!(store.route.get_untracked(), Route::Library);
        assert!(!store.browse_loading.get_untracked());

        store.switch_tab(browse_root.clone());
        assert_eq!(store.route.get_untracked(), browse_root);
        assert!(
            !store.browse_loading.get_untracked(),
            "Browse -> Library -> Browse must hit cache with browse_loading == false"
        );

        store.audio.with(|a| a.shutdown());
        let _ = std::fs::remove_dir_all(&root);

        view()
            .test_id("browse-cache-test-ok")
            .w(px(120.0))
            .h(px(40.0))
            .child(text("ok"))
    }

    #[test]
    fn browse_tab_switching_is_idempotent_and_uses_cache() {
        let viewport = nsh_core::geometry::Size::new(
            nsh_core::geometry::Points(400.0),
            nsh_core::geometry::Points(300.0),
        );
        let root = nsh_engine::Root::of::<BrowseCacheStoreTestApp>()
            .tokens(crate::app_tokens())
            .app_id("com.richerfu.nshmusic.browse_cache_test");
        let mut config = nsh_engine::EngineConfig::new(root);
        config.viewport = viewport;
        config.environment = nsh_engine::Environment {
            window: viewport,
            ..nsh_engine::Environment::default()
        };
        let mut app =
            nsh_testing::Headless::start(config).expect("headless browse cache test starts");
        app.wait_for("id:browse-cache-test-ok")
            .expect("browse cache & idempotent switch_tab assertions passed");
    }

    #[test]
    fn sandbox_paths_stay_inside_root_and_audio_engine_initializes() {
        let root = std::env::temp_dir().join(format!(
            "nsh-music-sandbox-paths-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let paths = sandbox_paths_from_root(&root);
        let expected = [
            (&paths.settings_path, "settings.json"),
            (&paths.session_path, "session.json"),
            (&paths.local_covers_dir, "local-covers"),
            (&paths.local_library_path, "local-library.json"),
            (&paths.plugins_dir, "plugins"),
            (&paths.plugin_data_dir, "plugin-data"),
            (&paths.audio_cache_dir, "audio-cache"),
            (&paths.artwork_cache_dir, "artwork-cache"),
        ];
        for (path, leaf) in expected {
            assert!(
                path.starts_with(&root),
                "{} must stay inside {}",
                path.display(),
                root.display()
            );
            assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(leaf));
        }

        let mut cfg = AudioEngineConfig::default();
        cfg.cache.cache_dir = paths.audio_cache_dir.clone();
        let engine = AudioEngine::with_output(cfg, Box::new(NullOutput::new()))
            .expect("AudioEngine initializes with sandbox audio_cache_dir");
        assert!(paths.audio_cache_dir.is_dir());
        engine.shutdown();

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_section_and_phone_secondary_route_classification() {
        for section in SettingsSection::ALL {
            assert!(!section.key().is_empty());
            assert!(section.title_key().starts_with("settings.section."));
            let route = Route::SettingsDetail(section);
            assert_eq!(route.key(), format!("settings:{}", section.key()));
            assert_eq!(route.title_key(), section.title_key());
            assert!(AppStore::is_phone_secondary_route(&route));
        }

        // Root Phone tabs are never secondary routes.
        assert!(!AppStore::is_phone_secondary_route(&Route::Home));
        assert!(!AppStore::is_phone_secondary_route(&Route::Browse {
            page: String::new(),
            title: String::new(),
        }));
        assert!(!AppStore::is_phone_secondary_route(&Route::Library));
        assert!(!AppStore::is_phone_secondary_route(&Route::Settings));

        // Drilled-down / sub-pages ARE secondary routes.
        for secondary in [
            Route::Browse {
                page: "charts".to_owned(),
                title: "Charts".to_owned(),
            },
            Route::Search,
            Route::Liked,
            Route::Playlists,
            Route::PlaylistDetail { id: "p1".into() },
            Route::AlbumDetail { id: "a1".into() },
            Route::ArtistDetail { id: "ar1".into() },
            Route::ShowDetail { id: "s1".into() },
            Route::Recent,
            Route::Local,
            Route::Radio,
            Route::Podcasts,
        ] {
            assert!(
                AppStore::is_phone_secondary_route(&secondary),
                "{secondary:?} should be a secondary route on Phone"
            );
        }
    }

    #[component]
    fn MobileVirtualCompatStoreTestApp() -> impl IntoElement {
        let root = std::env::temp_dir().join(format!(
            "nsh-music-mobile-compat-store-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = AppStore::new_isolated_for_test(&root);

        assert!(store.settings.with_untracked(|s| s.mobile_virtual_compat));
        assert!(store.plugin_manager().mobile_virtual_compat());

        store.set_mobile_virtual_compat(false);
        assert!(!store.settings.with_untracked(|s| s.mobile_virtual_compat));
        assert!(!store.plugin_manager().mobile_virtual_compat());

        store.refresh_plugin_summaries();
        store.set_mobile_virtual_compat(true);
        assert!(store.settings.with_untracked(|s| s.mobile_virtual_compat));
        assert!(store.plugin_manager().mobile_virtual_compat());

        store.audio.with(|a| a.shutdown());
        let _ = std::fs::remove_dir_all(&root);

        view()
            .test_id("mobile-compat-store-test-ok")
            .w(px(120.0))
            .h(px(40.0))
            .child(text("ok"))
    }

    #[test]
    fn mobile_virtual_compat_toggle_updates_settings_and_plugin_manager() {
        let viewport = nsh_core::geometry::Size::new(
            nsh_core::geometry::Points(400.0),
            nsh_core::geometry::Points(300.0),
        );
        let root = nsh_engine::Root::of::<MobileVirtualCompatStoreTestApp>()
            .tokens(crate::app_tokens())
            .app_id("com.richerfu.nshmusic.mobile_compat_store_test");
        let mut config = nsh_engine::EngineConfig::new(root);
        config.viewport = viewport;
        config.environment = nsh_engine::Environment {
            window: viewport,
            ..nsh_engine::Environment::default()
        };
        let mut app = nsh_testing::Headless::start(config)
            .expect("headless mobile virtual compat test starts");
        app.wait_for("id:mobile-compat-store-test-ok")
            .expect("mobile virtual compat store assertions passed");
    }

    #[test]
    fn parse_install_plugin_deep_link_supports_all_mygo_and_nsh_formats() {
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://install-plugin?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://install-plugin/?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music:install-plugin?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music:///install-plugin?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music-dev://install-plugin?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "nsh-music://install-plugin?repo=egoist/mygo-music-youtube-music"
            ),
            Some("egoist/mygo-music-youtube-music".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://install-plugin?url=https://github.com/owner/repo"
            ),
            Some("https://github.com/owner/repo".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://install-plugin?url=https://github.com/fallback/repo&repo=owner/preferred"
            ),
            Some("owner/preferred".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://install-plugin?repo=&url=https://github.com/owner/repo"
            ),
            Some("https://github.com/owner/repo".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link("MYGO-MUSIC://INSTALL-PLUGIN?repo=Owner/Repo"),
            Some("Owner/Repo".to_owned())
        );
        assert_eq!(
            parse_install_plugin_deep_link("MYGO-MUSIC:INSTALL-PLUGIN?URL=https://github.com/a/b"),
            Some("https://github.com/a/b".to_owned())
        );

        // Reject missing/empty repo, unknown actions, and unrelated schemes.
        assert_eq!(
            parse_install_plugin_deep_link("mygo-music://install-plugin"),
            None
        );
        assert_eq!(
            parse_install_plugin_deep_link("mygo-music://install-plugin?repo="),
            None
        );
        assert_eq!(
            parse_install_plugin_deep_link("mygo-music://install-plugin?repo=%20%20"),
            None
        );
        assert_eq!(
            parse_install_plugin_deep_link("mygo-music://other-action?repo=owner/repo"),
            None
        );
        assert_eq!(
            parse_install_plugin_deep_link(
                "mygo-music://other-host/install-plugin?repo=owner/repo"
            ),
            None
        );
        assert_eq!(
            parse_install_plugin_deep_link("https://example.com/install-plugin?repo=owner/repo"),
            None
        );
        assert_eq!(parse_install_plugin_deep_link("not a url"), None);
    }

    #[component]
    fn SettingsFocusSeqStoreTestApp() -> impl IntoElement {
        let root = std::env::temp_dir().join(format!(
            "nsh-music-settings-focus-seq-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = AppStore::new_isolated_for_test(&root);

        assert_eq!(store.settings_window_focus_seq.get_untracked(), 0);
        assert!(!store.settings_window_open.get_untracked());

        store.open_settings_window(SettingsSection::Plugins);
        assert!(store.settings_window_open.get_untracked());
        assert_eq!(
            store.settings_section.get_untracked(),
            SettingsSection::Plugins
        );
        assert_eq!(store.settings_window_focus_seq.get_untracked(), 1);

        // Re-opening while already open bumps focus_seq so Desktop raises the child window.
        store.open_settings_window(SettingsSection::Plugins);
        assert_eq!(store.settings_window_focus_seq.get_untracked(), 2);

        store.audio.with(|a| a.shutdown());
        let _ = std::fs::remove_dir_all(&root);

        view()
            .test_id("settings-focus-seq-test-ok")
            .w(px(120.0))
            .h(px(40.0))
            .child(text("ok"))
    }

    #[test]
    fn open_settings_window_bumps_focus_seq_when_already_open() {
        let viewport = nsh_core::geometry::Size::new(
            nsh_core::geometry::Points(1280.0),
            nsh_core::geometry::Points(800.0),
        );
        let root = nsh_engine::Root::of::<SettingsFocusSeqStoreTestApp>()
            .tokens(crate::app_tokens())
            .app_id("com.richerfu.nshmusic.settings_focus_seq_test");
        let mut config = nsh_engine::EngineConfig::new(root);
        config.viewport = viewport;
        config.environment = nsh_engine::Environment {
            window: viewport,
            ..nsh_engine::Environment::default()
        };
        let mut app =
            nsh_testing::Headless::start(config).expect("headless settings focus seq test starts");
        app.wait_for("id:settings-focus-seq-test-ok")
            .expect("settings focus seq assertions passed");
    }
}
