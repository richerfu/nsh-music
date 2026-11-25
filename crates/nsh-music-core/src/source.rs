use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::*;

/// Request resource kind used when applying plugin requestHeaders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestKind {
    Image,
    Audio,
}

/// Cross-platform plugin compatibility summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginCompatLevel {
    /// Pure JS/TS or built-in source; runs identically across all platforms.
    #[default]
    FullNative,
    /// Uses `ffmpeg` process calls which are transparently handled in-process on mobile and desktop.
    VirtualFfmpeg,
    /// Declares non-ffmpeg external binaries (e.g., yt-dlp, custom helper); restricted on mobile sandboxes.
    DesktopProcessLimited,
}

/// Compatibility metadata surfaced in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceCompat {
    pub level: PluginCompatLevel,
    pub network_hosts: Vec<String>,
    pub process_commands: Vec<String>,
    pub granted: bool,
    pub summary: String,
}

/// Callback used while a plugin performs an interactive sign-in.
pub type SignInPromptCallback = Arc<dyn Fn(SignInPrompt) + Send + Sync>;

/// Unified abstraction over local library and JS/TS plugins.
///
/// Methods other than `info`, `can`, `compatibility_info`, and `request_headers_for`
/// may perform network or disk I/O and should be invoked off the UI thread.
pub trait MusicSource: Send + Sync {
    fn info(&self) -> SourceInfo;
    fn can(&self, what: &str) -> bool;

    fn compatibility_info(&self) -> SourceCompat {
        SourceCompat::default()
    }

    fn request_headers_for(&self, _url: &str, _kind: RequestKind) -> Vec<(String, String)> {
        Vec::new()
    }

    fn setting(&self, _key: &str) -> String {
        String::new()
    }

    fn set_setting(&self, _key: &str, _value: &str) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn set_sign_in_prompt(&self, _cb: Option<SignInPromptCallback>) {}

    fn sign_in(&self) -> Result<Account, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn sign_out(&self) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn account(&self) -> Result<Option<Account>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn search(
        &self,
        _query: &str,
        _kind: SearchKind,
        _offset: usize,
    ) -> Result<SearchResult, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn suggest(&self, _query: &str) -> Result<Vec<String>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn home(&self) -> Result<Browse, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn browse(&self, _page: &str) -> Result<Browse, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn playlist(&self, _id: &str) -> Result<Playlist, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn album(&self, _id: &str) -> Result<AlbumPage, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn artist(&self, _id: &str) -> Result<ArtistPage, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn liked(&self, _offset: usize) -> Result<TrackPage, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn recent(&self) -> Result<Vec<Track>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn played(&self, _track: &Track) {}

    fn is_liked(&self, _ids: &[String]) -> Result<HashMap<String, bool>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn like(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn like_episode(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn create_playlist(&self, _name: &str) -> Result<Playlist, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn rename_playlist(&self, _id: &str, _name: &str) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn delete_playlist(&self, _id: &str) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn add_to_playlist(&self, _id: &str, _track_ids: &[String]) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remove_from_playlist(&self, _id: &str, _track_ids: &[String]) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn move_playlist_track(&self, _id: &str, _from: usize, _to: usize) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn save_playlist(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn follow_artist(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn save_album(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn saved_albums(&self, _offset: usize) -> Result<AlbumList, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn shows(&self) -> Result<Vec<Show>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn show(&self, _id: &str, _offset: usize) -> Result<ShowPage, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn subscribe_show(&self, _id: &str, _on: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn stations(&self) -> Result<Vec<Station>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn station_next(&self, _id: &str) -> Result<Vec<Track>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn comments(&self, _track_id: &str, _offset: usize) -> Result<CommentPage, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn like_comment(
        &self,
        _track_id: &str,
        _comment_id: &str,
        _on: bool,
    ) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn post_comment(&self, _track_id: &str, _text: &str) -> Result<Comment, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn lyrics(&self, _id: &str) -> Result<Lyrics, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn stream(&self, _id: &str, _quality: Quality) -> Result<StreamInfo, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_play(
        &self,
        _ids: &[String],
        _start: usize,
        _position: i64,
    ) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_pause(&self) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_resume(&self) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_seek(&self, _position: i64) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_volume(&self, _percent: i32) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_state(&self) -> Result<Option<RemoteState>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_devices(&self) -> Result<Vec<RemoteDevice>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn remote_use(&self, _device_id: &str) -> Result<(), SourceError> {
        Err(SourceError::Unsupported)
    }
}
