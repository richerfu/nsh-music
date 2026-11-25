use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Playlist, RepeatMode, Track};
use crate::settings::atomic_write_bytes;

/// Maximum number of recent search queries retained in [`PlaybackSession`].
pub const MAX_RECENT_SEARCHES: usize = 20;

/// Errors encountered while loading or persisting [`PlaybackSession`].
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent playback & personal library session state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaybackSession {
    pub queue: Vec<Track>,
    pub current_index: Option<usize>,
    pub position_ms: i64,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub volume: f32,
    pub muted: bool,
    pub active_source_id: String,
    pub recent_searches: Vec<String>,
    pub liked_tracks: Vec<Track>,
    pub user_playlists: Vec<Playlist>,
}

impl Default for PlaybackSession {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            current_index: None,
            position_ms: 0,
            repeat: RepeatMode::Off,
            shuffle: false,
            volume: 0.85,
            muted: false,
            active_source_id: "local".to_owned(),
            recent_searches: Vec::new(),
            liked_tracks: Vec::new(),
            user_playlists: Vec::new(),
        }
    }
}

impl PlaybackSession {
    /// Returns the default OS data path (`<data_local_dir>/nsh-music/session.json`).
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .or_else(dirs::config_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nsh-music")
            .join("session.json")
    }

    /// Loads session from [`Self::default_path`], returning defaults if missing.
    pub fn load() -> Result<Self, SessionError> {
        Self::load_from(Self::default_path())
    }

    /// Loads session from `path`, normalizing bounds and returning defaults if missing.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        match fs::read_to_string(path.as_ref()) {
            Ok(raw) => {
                let mut session: Self = serde_json::from_str(&raw)?;
                session.normalize();
                Ok(session)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(SessionError::Io(err)),
        }
    }

    /// Atomically saves session to [`Self::default_path`].
    pub fn save(&self) -> Result<(), SessionError> {
        self.save_to(Self::default_path())
    }

    /// Atomically saves session to `path` via sibling temp file + rename.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        let json = serde_json::to_vec_pretty(self)?;
        atomic_write_bytes(path.as_ref(), &json)?;
        Ok(())
    }

    /// Ensures volume, position, and queue index invariants hold and drops any legacy demo entries.
    pub fn normalize(&mut self) {
        self.volume = self.volume.clamp(0.0, 1.0);
        if self.position_ms < 0 {
            self.position_ms = 0;
        }
        let had_demo = self.queue.iter().any(is_demo_track);
        if had_demo {
            let current_id = self.current_track().map(|t| t.id.clone());
            self.queue.retain(|t| !is_demo_track(t));
            self.current_index = current_id
                .and_then(|cid| self.queue.iter().position(|t| t.id == cid));
            if self.current_index.is_none() {
                self.position_ms = 0;
            }
        }
        self.liked_tracks.retain(|t| !is_demo_track(t));
        for pl in &mut self.user_playlists {
            pl.tracks.retain(|t| !is_demo_track(t));
            pl.track_count = pl.tracks.len();
        }
        if let Some(idx) = self.current_index
            && idx >= self.queue.len()
        {
            self.current_index = if self.queue.is_empty() { None } else { Some(0) };
        }
        if self.active_source_id.trim().is_empty() {
            self.active_source_id = "local".to_owned();
        }
        if self.recent_searches.len() > MAX_RECENT_SEARCHES {
            self.recent_searches.truncate(MAX_RECENT_SEARCHES);
        }
    }

    /// Returns the currently selected track in `queue`, if any.
    pub fn current_track(&self) -> Option<&Track> {
        self.current_index.and_then(|idx| self.queue.get(idx))
    }

    /// Sets volume clamped to `0.0..=1.0`.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Records a search query at the front of `recent_searches`, deduplicating case-insensitively.
    pub fn record_search(&mut self, query: impl AsRef<str>) {
        let trimmed = query.as_ref().trim();
        if trimmed.is_empty() {
            return;
        }
        let lower = trimmed.to_ascii_lowercase();
        self.recent_searches
            .retain(|existing| existing.to_ascii_lowercase() != lower);
        self.recent_searches.insert(0, trimmed.to_owned());
        self.recent_searches.truncate(MAX_RECENT_SEARCHES);
    }

    /// Clears all recent search history.
    pub fn clear_recent_searches(&mut self) {
        self.recent_searches.clear();
    }

    /// Checks whether a track with `track_id` is in `liked_tracks`.
    pub fn is_track_liked(&self, track_id: &str) -> bool {
        self.liked_tracks.iter().any(|t| t.id == track_id)
    }

    /// Toggles liked status for `track`, returning the new liked boolean.
    pub fn toggle_liked_track(&mut self, track: Track) -> bool {
        if let Some(pos) = self.liked_tracks.iter().position(|t| t.id == track.id) {
            self.liked_tracks.remove(pos);
            false
        } else {
            self.liked_tracks.insert(0, track);
            true
        }
    }

    /// Creates and stores a new user playlist.
    pub fn create_user_playlist(&mut self, name: impl Into<String>) -> Playlist {
        let trimmed = name.into().trim().to_owned();
        let id = format!(
            "user-pl-{}-{}",
            chrono::Utc::now().timestamp_millis(),
            self.user_playlists.len() + 1
        );
        let playlist = Playlist {
            id,
            name: if trimmed.is_empty() {
                "Untitled Playlist".to_owned()
            } else {
                trimmed
            },
            image_url: String::new(),
            description: String::new(),
            owner: "You".to_owned(),
            track_count: 0,
            mine: true,
            saved: true,
            fixed: false,
            tracks: Vec::new(),
        };
        self.user_playlists.push(playlist.clone());
        playlist
    }

    /// Renames a user playlist by ID. Returns `true` if found.
    pub fn rename_user_playlist(&mut self, playlist_id: &str, new_name: impl Into<String>) -> bool {
        let trimmed = new_name.into().trim().to_owned();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(pl) = self.user_playlists.iter_mut().find(|p| p.id == playlist_id) {
            pl.name = trimmed;
            true
        } else {
            false
        }
    }

    /// Deletes a user playlist by ID. Returns `true` if removed.
    pub fn delete_user_playlist(&mut self, playlist_id: &str) -> bool {
        let before = self.user_playlists.len();
        self.user_playlists.retain(|p| p.id != playlist_id);
        self.user_playlists.len() != before
    }

    /// Adds a track to a user playlist if not already present. Returns `true` if added.
    pub fn add_track_to_playlist(&mut self, playlist_id: &str, track: Track) -> bool {
        let Some(pl) = self.user_playlists.iter_mut().find(|p| p.id == playlist_id) else {
            return false;
        };
        if pl.tracks.iter().any(|t| t.id == track.id) {
            return false;
        }
        if pl.image_url.is_empty() && !track.album.image_url.is_empty() {
            pl.image_url = track.album.image_url.clone();
        }
        pl.tracks.push(track);
        pl.track_count = pl.tracks.len();
        true
    }

    /// Removes a track by ID from a user playlist. Returns `true` if removed.
    pub fn remove_track_from_playlist(&mut self, playlist_id: &str, track_id: &str) -> bool {
        let Some(pl) = self.user_playlists.iter_mut().find(|p| p.id == playlist_id) else {
            return false;
        };
        let before = pl.tracks.len();
        pl.tracks.retain(|t| t.id != track_id);
        pl.track_count = pl.tracks.len();
        pl.tracks.len() != before
    }
}

/// Returns `true` when `track` came from the removed legacy offline demo catalog.
fn is_demo_track(track: &Track) -> bool {
    track.url.starts_with("demo://")
        || track.id.starts_with("demo-")
        || track.id.starts_with("demo:")
        || track.album.image_url.starts_with("demo://")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::model::{Album, Artist};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_session_path(name: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("nsh-music-core-session-test-{}-{seq}", std::process::id()))
            .join(name)
    }

    fn sample_track(id: &str, name: &str) -> Track {
        Track {
            id: id.to_owned(),
            source: "local".to_owned(),
            name: name.to_owned(),
            artists: vec![Artist {
                id: "a1".to_owned(),
                name: "Luna Echo".to_owned(),
                ..Artist::default()
            }],
            album: Album {
                id: "al1".to_owned(),
                name: "Midnight Glass".to_owned(),
                image_url: "https://cdn.example.test/cover.jpg".to_owned(),
                ..Album::default()
            },
            duration: 215_000,
            url: format!("file:///tmp/{id}.wav"),
            ..Track::default()
        }
    }

    #[test]
    fn missing_session_returns_default() {
        let path = temp_session_path("missing.json");
        let s = PlaybackSession::load_from(&path).expect("load default");
        assert_eq!(s.active_source_id, "local");
        assert_eq!(s.repeat, RepeatMode::Off);
        assert!(s.queue.is_empty());
    }

    #[test]
    fn session_persistence_and_mutations_roundtrip() {
        let path = temp_session_path("session.json");
        let mut session = PlaybackSession::default();
        let t1 = sample_track("t1", "Neon Drift");
        let t2 = sample_track("t2", "Harbor Rain");

        session.queue = vec![t1.clone(), t2.clone()];
        session.current_index = Some(1);
        session.position_ms = 42_500;
        session.repeat = RepeatMode::All;
        session.shuffle = true;
        session.set_volume(1.5); // clamped to 1.0
        session.muted = true;
        session.active_source_id = "netease".to_owned();

        session.record_search("City Pop");
        session.record_search("Lo-Fi");
        session.record_search("city pop"); // deduplicates & moves to front
        assert_eq!(session.recent_searches, vec!["city pop", "Lo-Fi"]);

        assert!(session.toggle_liked_track(t1.clone()));
        assert!(session.is_track_liked("t1"));

        let pl = session.create_user_playlist("Late Night");
        assert!(session.add_track_to_playlist(&pl.id, t1.clone()));
        assert!(!session.add_track_to_playlist(&pl.id, t1.clone()));
        assert!(session.rename_user_playlist(&pl.id, "Late Night Mix"));

        session.save_to(&path).expect("save session");
        let reloaded = PlaybackSession::load_from(&path).expect("reload session");

        assert_eq!(reloaded.volume, 1.0);
        assert_eq!(reloaded.current_track().map(|t| t.id.as_str()), Some("t2"));
        assert_eq!(reloaded.repeat, RepeatMode::All);
        assert!(reloaded.shuffle);
        assert!(reloaded.muted);
        assert_eq!(reloaded.active_source_id, "netease");
        assert_eq!(reloaded.user_playlists.len(), 1);
        assert_eq!(reloaded.user_playlists[0].name, "Late Night Mix");
        assert_eq!(reloaded.user_playlists[0].track_count, 1);

        assert!(session.remove_track_from_playlist(&pl.id, "t1"));
        assert!(session.delete_user_playlist(&pl.id));
        assert!(!session.toggle_liked_track(t1));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn normalize_fixes_out_of_bounds_index() {
        let mut session = PlaybackSession {
            queue: vec![sample_track("t1", "One")],
            current_index: Some(99),
            position_ms: -50,
            volume: -2.0,
            active_source_id: " ".into(),
            ..PlaybackSession::default()
        };
        session.normalize();
        assert_eq!(session.current_index, Some(0));
        assert_eq!(session.position_ms, 0);
        assert_eq!(session.volume, 0.0);
        assert_eq!(session.active_source_id, "local");
    }
}
