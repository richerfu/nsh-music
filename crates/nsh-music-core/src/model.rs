use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Music artist metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub following: bool,
}

/// Music album metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub artists: Vec<Artist>,
    pub released: String,
    pub saved: bool,
}

/// Track or podcast episode metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Track {
    pub id: String,
    pub source: String,
    pub name: String,
    pub artists: Vec<Artist>,
    pub album: Album,
    /// Track duration in milliseconds.
    pub duration: i64,
    pub url: String,
    pub unavailable: bool,
    /// Podcast show ID when this track is an episode.
    pub show_id: String,
    pub description: String,
    pub published: String,
    /// Optional local file URI / path when sourced from local storage.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub local_path: String,
    /// Optional embedded cover cache key / path for local tracks.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub local_cover: String,
}

impl Track {
    /// Joins artist names separated by commas.
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn is_episode(&self) -> bool {
        !self.show_id.is_empty()
    }
}

/// Playlist metadata and optional track list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub description: String,
    pub owner: String,
    pub track_count: usize,
    pub mine: bool,
    pub saved: bool,
    pub fixed: bool,
    pub tracks: Vec<Track>,
}

/// Artist detail page payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistPage {
    pub artist: Artist,
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
}

/// Album detail page payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumPage {
    pub album: Album,
    pub tracks: Vec<Track>,
}

/// Signed-in account metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub page: String,
}

/// Supported search categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchKind {
    #[default]
    Tracks,
    Albums,
    Artists,
    Playlists,
    Shows,
}

impl SearchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
            Self::Shows => "shows",
        }
    }
}

/// Paginated search result payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchResult {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
    pub shows: Vec<Show>,
    pub more: bool,
}

impl SearchResult {
    pub fn len(&self) -> usize {
        self.tracks.len()
            + self.albums.len()
            + self.artists.len()
            + self.playlists.len()
            + self.shows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Paginated track list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrackPage {
    pub tracks: Vec<Track>,
    pub more: bool,
}

/// Paginated album list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumList {
    pub albums: Vec<Album>,
    pub more: bool,
}

/// Podcast show metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Show {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub description: String,
    pub publisher: String,
    pub publisher_page: String,
    pub episode_count: usize,
    pub subscribed: bool,
}

/// Podcast detail page with paginated episodes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShowPage {
    pub show: Show,
    pub episodes: Vec<Track>,
    pub more: bool,
}

/// Endless radio station.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub description: String,
    pub image_url: String,
}

/// Track listener comment.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub avatar_url: String,
    pub text: String,
    pub time: i64,
    pub likes: i64,
    pub liked: bool,
}

/// Paginated comments payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommentPage {
    pub comments: Vec<Comment>,
    pub total: usize,
    pub more: bool,
}

/// Plugin-defined browse page.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Browse {
    pub title: String,
    pub subtitle: String,
    pub image_url: String,
    pub description: String,
    pub sections: Vec<Section>,
}

/// Section inside a browse page.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Section {
    pub title: String,
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
    pub shows: Vec<Show>,
    pub stations: Vec<Station>,
    pub links: Vec<BrowseLink>,
    pub more_page: String,
    pub empty: String,
    pub page_title: String,
    pub subtitle: String,
}

/// Link card opening another browse page.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrowseLink {
    pub title: String,
    pub subtitle: String,
    pub image_url: String,
    pub page: String,
}

/// Sidebar navigation entry declared in plugin.json.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NavItem {
    pub title: String,
    pub page: String,
    pub builtin: String,
    pub icon: String,
}

/// Deserializes an `i64` leniently from JSON integers, finite floats (rounded, e.g. from Go/JS
/// `float64`/`Number`), numeric strings, or `null` (`0`).
fn deserialize_i64_lenient<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct LenientI64Visitor;

    impl<'de> serde::de::Visitor<'de> for LenientI64Visitor {
        type Value = i64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an integer, finite number, numeric string, or null")
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            i64::try_from(v).map_err(|_| E::custom(format!("integer out of i64 range: {v}")))
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if !v.is_finite() {
                return Err(E::custom("non-finite float cannot be converted to i64"));
            }
            let rounded = v.round();
            if rounded < (i64::MIN as f64) || rounded >= 9_223_372_036_854_775_808.0 {
                return Err(E::custom(format!("float out of i64 range: {v}")));
            }
            Ok(rounded as i64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(0);
            }
            if let Ok(i) = trimmed.parse::<i64>() {
                return Ok(i);
            }
            let f = trimmed
                .parse::<f64>()
                .map_err(|_| E::custom(format!("invalid numeric string: {v}")))?;
            self.visit_f64(f)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(0)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(0)
        }
    }

    deserializer.deserialize_any(LenientI64Visitor)
}

/// Stream response returned by a playback=stream source.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamInfo {
    pub url: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub quality: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub bit_rate: i64,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub size: i64,
    pub peak: f64,
    pub gain: f64,
    pub headers: std::collections::HashMap<String, String>,
}

/// Track lyrics payload returned by a source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Lyrics {
    pub lrc: String,
    pub translation: String,
    pub lines: Vec<LyricLine>,
}

/// Single timed line of lyrics (times in milliseconds).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LyricLine {
    pub start: i64,
    pub end: i64,
    pub text: String,
    pub translation: String,
    pub words: Vec<LyricWord>,
}

/// Word-by-word timed token inside a lyric line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LyricWord {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

/// Remote playback device target.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    pub active: bool,
}

/// Remote playback status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteState {
    pub playing: bool,
    pub track_id: String,
    pub position: i64,
    pub duration: i64,
    pub volume: i32,
    pub device: String,
}

/// How a source plays audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackMode {
    #[default]
    Stream,
    Remote,
}

/// Setting option for select controls.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingOption {
    pub value: String,
    pub label: String,
}

/// Setting schema declared by a source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingDef {
    pub key: String,
    pub label: String,
    pub help: String,
    pub link: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub options: Vec<SettingOption>,
}

/// Source identity and configuration descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub playback: PlaybackMode,
    pub settings: Vec<SettingDef>,
    pub navigation: Vec<NavItem>,
}

/// QR/instruction prompt emitted during plugin signIn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SignInPrompt {
    pub qr: String,
    pub text: String,
}

/// Audio quality preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Quality {
    Standard,
    #[default]
    High,
    Lossless,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::High => "high",
            Self::Lossless => "lossless",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "standard" => Self::Standard,
            "lossless" => Self::Lossless,
            _ => Self::High,
        }
    }
}

/// Queue repeat behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// Now Playing centerpiece visual style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerStyle {
    #[default]
    Cover,
    Vinyl,
    Cassette,
}

impl PlayerStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "",
            Self::Vinyl => "turntable",
            Self::Cassette => "cassette",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "turntable" | "vinyl" => Self::Vinyl,
            "cassette" => Self::Cassette,
            _ => Self::Cover,
        }
    }
}

/// Now Playing full-screen lyrics visualizer style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StageStyle {
    #[default]
    Flow,
    Steps,
    Tilt,
}

impl StageStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flow => "",
            Self::Steps => "steps",
            Self::Tilt => "tilt",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "steps" => Self::Steps,
            "tilt" => Self::Tilt,
            _ => Self::Flow,
        }
    }
}

/// Structured error returned by music sources (matching mygo-music codes).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SourceError {
    #[error("sign in required: {0}")]
    SignIn(String),
    #[error("setup required: {0}")]
    Setup(String),
    #[error("track unavailable: {0}")]
    Unavailable(String),
    #[error("unsupported by this source")]
    Unsupported,
    #[error("{0}")]
    Message(String),
}

impl SourceError {
    pub fn from_code(code: &str, message: impl Into<String>) -> Self {
        let msg = message.into();
        match code {
            "signIn" => Self::SignIn(msg),
            "setup" => Self::Setup(msg),
            "unavailable" => Self::Unavailable(msg),
            "unsupported" => Self::Unsupported,
            _ => Self::Message(if msg.is_empty() {
                "the plugin failed".to_owned()
            } else {
                msg
            }),
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::SignIn(r) | Self::Setup(r) | Self::Unavailable(r) | Self::Message(r) => r,
            Self::Unsupported => "unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_info_deserializes_floats_strings_and_null_leniently() {
        let from_floats: StreamInfo = serde_json::from_str(
            r#"{
                "url": "http://127.0.0.1:43210/track",
                "type": "flac",
                "quality": "lossless",
                "bitRate": 320128.6,
                "size": 1048576.0
            }"#,
        )
        .expect("float bitRate and size should deserialize");
        assert_eq!(from_floats.bit_rate, 320_129);
        assert_eq!(from_floats.size, 1_048_576);
        assert_eq!(from_floats.media_type, "flac");

        let from_strings: StreamInfo = serde_json::from_str(
            r#"{
                "url": "https://cdn.example.com/a.m4a",
                "type": "m4a",
                "bitRate": "128000",
                "size": "2048000.4"
            }"#,
        )
        .expect("numeric string bitRate and size should deserialize");
        assert_eq!(from_strings.bit_rate, 128_000);
        assert_eq!(from_strings.size, 2_048_000);

        let from_null_and_missing: StreamInfo = serde_json::from_str(
            r#"{
                "url": "https://cdn.example.com/b.mp3",
                "bitRate": null
            }"#,
        )
        .expect("null and omitted fields should default to 0");
        assert_eq!(from_null_and_missing.bit_rate, 0);
        assert_eq!(from_null_and_missing.size, 0);

        let from_ints: StreamInfo = serde_json::from_str(
            r#"{
                "url": "https://cdn.example.com/c.mp3",
                "bitRate": 320000,
                "size": 4096
            }"#,
        )
        .expect("integer bitRate and size should deserialize");
        assert_eq!(from_ints.bit_rate, 320_000);
        assert_eq!(from_ints.size, 4_096);
    }
}
