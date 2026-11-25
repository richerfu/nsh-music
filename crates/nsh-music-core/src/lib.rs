//! Core domain models, [`MusicSource`](source::MusicSource) trait, lyrics parser,
//! localization catalog, settings/session persistence, and local/demo music source.

pub mod i18n;
pub mod local;
pub mod lyrics;
pub mod model;
pub mod session;
pub mod settings;
pub mod source;

pub use i18n::{I18n, Locale};
pub use local::{LocalMusicSource, SUPPORTED_AUDIO_EXTENSIONS, ScanReport};
pub use lyrics::{
    ActiveLyric, DEFAULT_ALIGN_TOLERANCE_MS, LyricMeta, ParsedLyricLine, ParsedLyrics,
    align_secondary_lines, find_active_line, find_active_word, line_progress, parse_lrc,
    parse_lrc_timestamp, parse_lyrics, parse_lyrics_with_romaji, word_progress,
};
pub use model::{
    Account, Album, AlbumList, AlbumPage, Artist, ArtistPage, Browse, BrowseLink, Comment,
    CommentPage, LyricLine, LyricWord, Lyrics, NavItem, PlaybackMode, PlayerStyle, Playlist,
    Quality, RemoteDevice, RemoteState, RepeatMode, SearchKind, SearchResult, Section, SettingDef,
    SettingOption, Show, ShowPage, SignInPrompt, SourceError, SourceInfo, StageStyle, Station,
    StreamInfo, Track, TrackPage,
};
pub use session::{MAX_RECENT_SEARCHES, PlaybackSession, SessionError};
pub use settings::{AppSettings, MAX_CROSSFADE_MS, SettingsError, ThemeMode};
pub use source::{MusicSource, PluginCompatLevel, RequestKind, SignInPromptCallback, SourceCompat};
