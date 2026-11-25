use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::lyrics::parse_lyrics;
use crate::model::{
    Album, AlbumPage, Artist, ArtistPage, Browse, Lyrics, PlaybackMode, Playlist, Quality,
    SearchKind, SearchResult, Section, SourceError, SourceInfo, StreamInfo, Track, TrackPage,
};
use crate::settings::atomic_write_bytes;
use crate::source::{MusicSource, PluginCompatLevel, SourceCompat};

/// Audio file extensions recognized by the local library scanner.
pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "wav", "opus"];

const PAGE_SIZE: usize = 20;
const MAX_SCAN_DEPTH: usize = 16;

/// Summary returned after scanning local music folders.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanReport {
    pub scanned_files: usize,
    pub added_tracks: usize,
    pub skipped_files: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct LocalTrackMeta {
    bit_rate_kbps: i64,
    file_size: i64,
    media_type: String,
    embedded_lyrics: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedPlaylist {
    id: String,
    name: String,
    track_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedLibrary {
    folders: Vec<PathBuf>,
    liked_ids: Vec<String>,
    playlists: Vec<PersistedPlaylist>,
    recent_ids: Vec<String>,
}

struct LocalLibraryState {
    library_path: Option<PathBuf>,
    cover_cache_dir: PathBuf,
    folders: Vec<PathBuf>,

    local_tracks: Vec<Track>,
    local_meta: HashMap<String, LocalTrackMeta>,

    user_playlists: Vec<PersistedPlaylist>,
    liked_ids: HashSet<String>,
    recent_ids: Vec<String>,
    next_id_counter: u64,
}

/// Built-in local folder scanner (`lofty`) aligned with `mygo-music/internal/local`.
///
/// Starts empty until the user adds local music folders, and persists folders,
/// likes, user playlists, and recently played tracks to `library.json`.
pub struct LocalMusicSource {
    state: RwLock<LocalLibraryState>,
}

impl Default for LocalMusicSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalMusicSource {
    /// Canonical source ID used across `nsh-music`.
    pub const SOURCE_ID: &'static str = "local";

    /// Returns the default OS path for `library.json`.
    pub fn default_library_path() -> PathBuf {
        default_library_path()
    }

    /// Creates a `LocalMusicSource` backed by the default OS `library.json`, starting
    /// empty unless persisted folders already exist.
    pub fn new() -> Self {
        Self::with_storage(Vec::new(), default_cover_cache_dir(), Some(default_library_path()))
    }

    /// Creates an in-memory `LocalMusicSource` scanning `folders` immediately.
    pub fn with_folders(folders: Vec<PathBuf>) -> Self {
        Self::with_storage(folders, default_cover_cache_dir(), None)
    }

    /// Creates a `LocalMusicSource` backed by the default `library.json`, merging any
    /// initial `folders` supplied by settings.
    pub fn with_default_storage(folders: Vec<PathBuf>) -> Self {
        Self::with_storage(folders, default_cover_cache_dir(), Some(default_library_path()))
    }

    /// Creates a `LocalMusicSource` with explicit `folders`, cover cache directory,
    /// and optional `library.json` persistence path.
    pub fn with_storage(
        folders: Vec<PathBuf>,
        cover_cache_dir: PathBuf,
        library_path: Option<PathBuf>,
    ) -> Self {
        let mut persisted = library_path
            .as_deref()
            .and_then(load_persisted_library)
            .unwrap_or_default();

        for f in folders {
            if !persisted.folders.iter().any(|existing| existing == &f) {
                persisted.folders.push(f);
            }
        }

        let mut max_seq = 0u64;
        for pl in &persisted.playlists {
            if let Some(num) = pl
                .id
                .strip_prefix("local-pl-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                max_seq = max_seq.max(num);
            }
        }

        let initial_folders = persisted.folders.clone();
        let state = LocalLibraryState {
            library_path,
            cover_cache_dir,
            folders: initial_folders.clone(),
            local_tracks: Vec::new(),
            local_meta: HashMap::new(),
            user_playlists: persisted.playlists,
            liked_ids: persisted.liked_ids.into_iter().collect(),
            recent_ids: persisted.recent_ids,
            next_id_counter: max_seq,
        };

        let source = Self {
            state: RwLock::new(state),
        };
        if !initial_folders.is_empty() {
            source.scan_folders(&initial_folders);
        }
        source
    }

    /// Returns whether `path` has a supported local audio extension.
    pub fn is_supported_audio_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                SUPPORTED_AUDIO_EXTENSIONS
                    .iter()
                    .any(|supported| supported.eq_ignore_ascii_case(ext))
            })
    }

    /// Replaces the configured local folders, scans them for audio files, and persists `library.json`.
    pub fn scan_folders(&self, folders: &[PathBuf]) -> ScanReport {
        let cover_cache_dir = {
            let mut guard = self.state.write().expect("local library lock poisoned");
            guard.folders = folders.to_vec();
            guard.cover_cache_dir.clone()
        };

        let mut report = ScanReport::default();
        let mut tracks = Vec::new();
        let mut metas = HashMap::new();
        let mut visited_dirs = HashSet::new();

        for folder in folders {
            if !folder.exists() {
                report
                    .errors
                    .push(format!("folder not found: {}", folder.display()));
                continue;
            }
            walk_audio_folder(
                folder,
                0,
                &cover_cache_dir,
                &mut visited_dirs,
                &mut tracks,
                &mut metas,
                &mut report,
            );
        }

        tracks.sort_by(|a, b| {
            a.album
                .name
                .to_lowercase()
                .cmp(&b.album.name.to_lowercase())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let mut guard = self.state.write().expect("local library lock poisoned");
        guard.local_tracks = tracks;
        guard.local_meta = metas;
        persist_state(&guard);
        report
    }

    /// Re-scans the currently configured folders.
    pub fn rescan(&self) -> ScanReport {
        let folders = self.folders();
        self.scan_folders(&folders)
    }

    /// Returns currently configured local library folders.
    pub fn folders(&self) -> Vec<PathBuf> {
        self.state
            .read()
            .expect("local library lock poisoned")
            .folders
            .clone()
    }

    /// Returns the number of scanned local audio tracks.
    pub fn local_track_count(&self) -> usize {
        self.state
            .read()
            .expect("local library lock poisoned")
            .local_tracks
            .len()
    }

    /// Returns all currently scanned local music tracks.
    pub fn all_tracks(&self) -> Vec<Track> {
        self.state
            .read()
            .expect("local library lock poisoned")
            .local_tracks
            .clone()
    }
}

impl MusicSource for LocalMusicSource {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: Self::SOURCE_ID.to_owned(),
            name: "Local Files".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            playback: PlaybackMode::Stream,
            settings: Vec::new(),
            navigation: Vec::new(),
        }
    }

    fn can(&self, what: &str) -> bool {
        matches!(
            what,
            "home"
                | "getHome"
                | "browse"
                | "getBrowse"
                | "search"
                | "playlists"
                | "getUserPlaylists"
                | "playlist"
                | "getPlaylist"
                | "album"
                | "getAlbum"
                | "artist"
                | "getArtist"
                | "liked"
                | "getLikedTracks"
                | "isLiked"
                | "like"
                | "toggleLike"
                | "recent"
                | "getRecent"
                | "played"
                | "onPlayed"
                | "createPlaylist"
                | "create_playlist"
                | "renamePlaylist"
                | "rename_playlist"
                | "deletePlaylist"
                | "delete_playlist"
                | "addToPlaylist"
                | "add_to_playlist"
                | "removeFromPlaylist"
                | "remove_from_playlist"
                | "movePlaylistTrack"
                | "move_playlist_track"
                | "lyrics"
                | "getLyrics"
                | "stream"
                | "getStreamUrl"
        )
    }

    fn compatibility_info(&self) -> SourceCompat {
        SourceCompat {
            level: PluginCompatLevel::FullNative,
            network_hosts: Vec::new(),
            process_commands: Vec::new(),
            granted: true,
            summary: String::new(),
        }
    }

    fn setting(&self, key: &str) -> String {
        let guard = self.state.read().expect("local library lock poisoned");
        match key {
            "folders" => guard
                .folders
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(";"),
            _ => String::new(),
        }
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<(), SourceError> {
        match key {
            "folders" => {
                let folders: Vec<PathBuf> = value
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
                self.scan_folders(&folders);
                Ok(())
            }
            _ => Err(SourceError::Unsupported),
        }
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        offset: usize,
    ) -> Result<SearchResult, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        let q = query.trim().to_ascii_lowercase();
        let mut res = SearchResult::default();
        if q.is_empty() {
            return Ok(res);
        }

        match kind {
            SearchKind::Tracks => {
                let matched: Vec<Track> = guard
                    .local_tracks
                    .iter()
                    .filter(|t| {
                        t.name.to_ascii_lowercase().contains(&q)
                            || t.artist_names().to_ascii_lowercase().contains(&q)
                            || t.album.name.to_ascii_lowercase().contains(&q)
                    })
                    .cloned()
                    .collect();
                let (page, more) = paginate(&matched, offset, PAGE_SIZE);
                res.tracks = page;
                res.more = more;
            }
            SearchKind::Albums => {
                let matched: Vec<Album> = collect_all_albums(&guard)
                    .into_iter()
                    .filter(|a| {
                        a.name.to_ascii_lowercase().contains(&q)
                            || a.artists
                                .iter()
                                .any(|ar| ar.name.to_ascii_lowercase().contains(&q))
                    })
                    .collect();
                let (page, more) = paginate(&matched, offset, PAGE_SIZE);
                res.albums = page;
                res.more = more;
            }
            SearchKind::Artists => {
                let matched: Vec<Artist> = collect_all_artists(&guard)
                    .into_iter()
                    .filter(|a| a.name.to_ascii_lowercase().contains(&q))
                    .collect();
                let (page, more) = paginate(&matched, offset, PAGE_SIZE);
                res.artists = page;
                res.more = more;
            }
            SearchKind::Playlists => {
                let matched: Vec<Playlist> = resolve_playlists(&guard)
                    .into_iter()
                    .filter(|p| p.name.to_ascii_lowercase().contains(&q))
                    .collect();
                let (page, more) = paginate(&matched, offset, PAGE_SIZE);
                res.playlists = page;
                res.more = more;
            }
            SearchKind::Shows => {
                // Local library does not provide podcasts.
            }
        }

        Ok(res)
    }

    fn home(&self) -> Result<Browse, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        if guard.local_tracks.is_empty() {
            return Ok(Browse::default());
        }

        let albums = collect_all_albums(&guard);
        let recent = resolve_recent_tracks(&guard);
        let mut sections = Vec::new();

        if !albums.is_empty() {
            sections.push(Section {
                title: "Albums".to_owned(),
                albums,
                ..Section::default()
            });
        }
        if !recent.is_empty() {
            sections.push(Section {
                title: "Recently Played".to_owned(),
                tracks: recent,
                ..Section::default()
            });
        }
        sections.push(Section {
            title: "Tracks".to_owned(),
            tracks: guard.local_tracks.clone(),
            ..Section::default()
        });

        Ok(Browse {
            title: "Local Files".to_owned(),
            subtitle: String::new(),
            image_url: String::new(),
            description: String::new(),
            sections,
        })
    }

    fn browse(&self, page: &str) -> Result<Browse, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        match page {
            "" | "home" => {
                drop(guard);
                self.home()
            }
            "albums" => Ok(Browse {
                title: "Albums".to_owned(),
                sections: vec![Section {
                    title: "Albums".to_owned(),
                    albums: collect_all_albums(&guard),
                    ..Section::default()
                }],
                ..Browse::default()
            }),
            "local" | "tracks" => Ok(Browse {
                title: "Local Files".to_owned(),
                sections: vec![Section {
                    title: "Tracks".to_owned(),
                    tracks: guard.local_tracks.clone(),
                    ..Section::default()
                }],
                ..Browse::default()
            }),
            other => Err(SourceError::Unavailable(format!("unknown browse page: {other}"))),
        }
    }

    fn playlists(&self) -> Result<Vec<Playlist>, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        Ok(resolve_playlists(&guard))
    }

    fn playlist(&self, id: &str) -> Result<Playlist, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        resolve_playlists(&guard)
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("playlist not found: {id}")))
    }

    fn album(&self, id: &str) -> Result<AlbumPage, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        let album = collect_all_albums(&guard)
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("album not found: {id}")))?;
        let tracks = guard
            .local_tracks
            .iter()
            .filter(|t| t.album.id == id)
            .cloned()
            .collect();
        Ok(AlbumPage { album, tracks })
    }

    fn artist(&self, id: &str) -> Result<ArtistPage, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        let artist = collect_all_artists(&guard)
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("artist not found: {id}")))?;
        let tracks = guard
            .local_tracks
            .iter()
            .filter(|t| t.artists.iter().any(|a| a.id == id))
            .cloned()
            .collect();
        let albums = collect_all_albums(&guard)
            .into_iter()
            .filter(|al| al.artists.iter().any(|a| a.id == id))
            .collect();
        Ok(ArtistPage {
            artist,
            tracks,
            albums,
        })
    }

    fn liked(&self, offset: usize) -> Result<TrackPage, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        let items: Vec<Track> = guard
            .local_tracks
            .iter()
            .filter(|t| guard.liked_ids.contains(&t.id))
            .cloned()
            .collect();
        let (tracks, more) = paginate(&items, offset, PAGE_SIZE);
        Ok(TrackPage { tracks, more })
    }

    fn recent(&self) -> Result<Vec<Track>, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        Ok(resolve_recent_tracks(&guard))
    }

    fn played(&self, track: &Track) {
        let mut guard = self.state.write().expect("local library lock poisoned");
        guard.recent_ids.retain(|id| id != &track.id);
        guard.recent_ids.insert(0, track.id.clone());
        guard.recent_ids.truncate(50);
        persist_state(&guard);
    }

    fn is_liked(&self, ids: &[String]) -> Result<HashMap<String, bool>, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");
        let mut map = HashMap::with_capacity(ids.len());
        for id in ids {
            map.insert(id.clone(), guard.liked_ids.contains(id));
        }
        Ok(map)
    }

    fn like(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let mut guard = self.state.write().expect("local library lock poisoned");
        if on {
            guard.liked_ids.insert(id.to_owned());
        } else {
            guard.liked_ids.remove(id);
        }
        persist_state(&guard);
        Ok(())
    }

    fn create_playlist(&self, name: &str) -> Result<Playlist, SourceError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(SourceError::Message("playlist name cannot be empty".to_owned()));
        }
        let mut guard = self.state.write().expect("local library lock poisoned");
        guard.next_id_counter += 1;
        let id = format!("local-pl-{}", guard.next_id_counter);
        let persisted = PersistedPlaylist {
            id: id.clone(),
            name: trimmed.to_owned(),
            track_ids: Vec::new(),
        };
        guard.user_playlists.push(persisted.clone());
        persist_state(&guard);
        Ok(hydrate_playlist(&persisted, &track_lookup(&guard)))
    }

    fn rename_playlist(&self, id: &str, name: &str) -> Result<(), SourceError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(SourceError::Message("playlist name cannot be empty".to_owned()));
        }
        let mut guard = self.state.write().expect("local library lock poisoned");
        let pl = guard
            .user_playlists
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("playlist not found: {id}")))?;
        pl.name = trimmed.to_owned();
        persist_state(&guard);
        Ok(())
    }

    fn delete_playlist(&self, id: &str) -> Result<(), SourceError> {
        let mut guard = self.state.write().expect("local library lock poisoned");
        let before = guard.user_playlists.len();
        guard.user_playlists.retain(|p| p.id != id);
        if guard.user_playlists.len() == before {
            return Err(SourceError::Unavailable(format!("playlist not found: {id}")));
        }
        persist_state(&guard);
        Ok(())
    }

    fn add_to_playlist(&self, id: &str, track_ids: &[String]) -> Result<(), SourceError> {
        let mut guard = self.state.write().expect("local library lock poisoned");
        let known_ids: HashSet<String> = guard.local_tracks.iter().map(|t| t.id.clone()).collect();
        let pl = guard
            .user_playlists
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("playlist not found: {id}")))?;

        for tid in track_ids {
            if known_ids.contains(tid) && !pl.track_ids.iter().any(|existing| existing == tid) {
                pl.track_ids.push(tid.clone());
            }
        }
        persist_state(&guard);
        Ok(())
    }

    fn remove_from_playlist(&self, id: &str, track_ids: &[String]) -> Result<(), SourceError> {
        let mut guard = self.state.write().expect("local library lock poisoned");
        let remove_set: HashSet<&str> = track_ids.iter().map(String::as_str).collect();
        let pl = guard
            .user_playlists
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("playlist not found: {id}")))?;
        pl.track_ids.retain(|tid| !remove_set.contains(tid.as_str()));
        persist_state(&guard);
        Ok(())
    }

    fn move_playlist_track(&self, id: &str, from: usize, to: usize) -> Result<(), SourceError> {
        let mut guard = self.state.write().expect("local library lock poisoned");
        let pl = guard
            .user_playlists
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| SourceError::Unavailable(format!("playlist not found: {id}")))?;
        if from >= pl.track_ids.len() || to >= pl.track_ids.len() {
            return Err(SourceError::Message("track index out of range".to_owned()));
        }
        let item = pl.track_ids.remove(from);
        pl.track_ids.insert(to, item);
        persist_state(&guard);
        Ok(())
    }

    fn lyrics(&self, id: &str) -> Result<Lyrics, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");

        if let Some(track) = guard.local_tracks.iter().find(|t| t.id == id) {
            if let Some(embedded) = guard
                .local_meta
                .get(id)
                .and_then(|m| m.embedded_lyrics.clone())
                .filter(|s| !s.trim().is_empty())
            {
                let mut raw = Lyrics {
                    lrc: embedded,
                    translation: String::new(),
                    lines: Vec::new(),
                };
                raw.lines = parse_lyrics(&raw).to_model_lyrics().lines;
                return Ok(raw);
            }

            if !track.local_path.is_empty() {
                let sidecar = Path::new(&track.local_path).with_extension("lrc");
                if let Ok(content) = fs::read_to_string(&sidecar) {
                    let mut raw = Lyrics {
                        lrc: content,
                        translation: String::new(),
                        lines: Vec::new(),
                    };
                    raw.lines = parse_lyrics(&raw).to_model_lyrics().lines;
                    return Ok(raw);
                }
            }
        }

        Ok(Lyrics::default())
    }

    fn stream(&self, id: &str, quality: Quality) -> Result<StreamInfo, SourceError> {
        let guard = self.state.read().expect("local library lock poisoned");

        if let Some(track) = guard.local_tracks.iter().find(|t| t.id == id) {
            let meta = guard.local_meta.get(id).cloned().unwrap_or_default();
            let file_url = if !track.url.is_empty() {
                track.url.clone()
            } else {
                path_to_file_uri(Path::new(&track.local_path))
            };
            return Ok(StreamInfo {
                url: file_url,
                media_type: meta.media_type,
                quality: quality.as_str().to_owned(),
                bit_rate: meta.bit_rate_kbps,
                size: meta.file_size,
                peak: 0.92,
                gain: 0.0,
                headers: HashMap::new(),
            });
        }

        Err(SourceError::Unavailable(format!("track not found: {id}")))
    }
}

fn default_cover_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nsh-music")
        .join("local-covers")
}

fn default_library_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("nsh-music")
        .join("library.json")
}

fn load_persisted_library(path: &Path) -> Option<PersistedLibrary> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<PersistedLibrary>(&raw).ok()
}

fn persist_state(state: &LocalLibraryState) {
    let Some(path) = state.library_path.as_deref() else {
        return;
    };
    let mut liked_ids: Vec<String> = state.liked_ids.iter().cloned().collect();
    liked_ids.sort();
    let payload = PersistedLibrary {
        folders: state.folders.clone(),
        liked_ids,
        playlists: state.user_playlists.clone(),
        recent_ids: state.recent_ids.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
        let _ = atomic_write_bytes(path, &bytes);
    }
}

fn track_lookup(state: &LocalLibraryState) -> HashMap<String, Track> {
    state
        .local_tracks
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect()
}

fn hydrate_playlist(pl: &PersistedPlaylist, lookup: &HashMap<String, Track>) -> Playlist {
    let tracks: Vec<Track> = pl
        .track_ids
        .iter()
        .filter_map(|id| lookup.get(id).cloned())
        .collect();
    let image_url = tracks
        .iter()
        .find(|t| !t.album.image_url.is_empty())
        .map(|t| t.album.image_url.clone())
        .unwrap_or_default();
    Playlist {
        id: pl.id.clone(),
        name: pl.name.clone(),
        image_url,
        description: String::new(),
        owner: String::new(),
        track_count: tracks.len(),
        mine: true,
        saved: true,
        fixed: false,
        tracks,
    }
}

fn resolve_playlists(state: &LocalLibraryState) -> Vec<Playlist> {
    let lookup = track_lookup(state);
    state
        .user_playlists
        .iter()
        .map(|pl| hydrate_playlist(pl, &lookup))
        .collect()
}

fn resolve_recent_tracks(state: &LocalLibraryState) -> Vec<Track> {
    let lookup = track_lookup(state);
    state
        .recent_ids
        .iter()
        .filter_map(|id| lookup.get(id).cloned())
        .collect()
}

fn collect_all_artists(state: &LocalLibraryState) -> Vec<Artist> {
    let mut map: HashMap<String, Artist> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for track in &state.local_tracks {
        for a in &track.artists {
            if !map.contains_key(&a.id) {
                order.push(a.id.clone());
                map.insert(a.id.clone(), a.clone());
            }
        }
    }
    order.into_iter().filter_map(|id| map.remove(&id)).collect()
}

fn collect_all_albums(state: &LocalLibraryState) -> Vec<Album> {
    let mut map: HashMap<String, Album> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for track in &state.local_tracks {
        let al = &track.album;
        if !al.id.is_empty() && !map.contains_key(&al.id) {
            order.push(al.id.clone());
            map.insert(al.id.clone(), al.clone());
        }
    }
    order.into_iter().filter_map(|id| map.remove(&id)).collect()
}

fn walk_audio_folder(
    dir: &Path,
    depth: usize,
    cover_cache_dir: &Path,
    visited_dirs: &mut HashSet<PathBuf>,
    tracks: &mut Vec<Track>,
    metas: &mut HashMap<String, LocalTrackMeta>,
    report: &mut ScanReport,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }

    let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited_dirs.insert(canonical) {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(err) => {
            report
                .errors
                .push(format!("failed to read {}: {err}", dir.display()));
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            walk_audio_folder(
                &path,
                depth + 1,
                cover_cache_dir,
                visited_dirs,
                tracks,
                metas,
                report,
            );
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        if !LocalMusicSource::is_supported_audio_file(&path) {
            report.skipped_files += 1;
            continue;
        }

        report.scanned_files += 1;
        match scan_audio_file(&path, cover_cache_dir) {
            Ok((track, meta)) => {
                metas.insert(track.id.clone(), meta);
                tracks.push(track);
                report.added_tracks += 1;
            }
            Err(err) => {
                report
                    .errors
                    .push(format!("failed to scan {}: {err}", path.display()));
            }
        }
    }
}

fn scan_audio_file(
    path: &Path,
    cover_cache_dir: &Path,
) -> Result<(Track, LocalTrackMeta), String> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let file_size = fs::metadata(&canonical).map(|m| m.len() as i64).unwrap_or(0);
    let media_type = mime_for_extension(canonical.extension().and_then(|e| e.to_str())).to_owned();

    let tagged_file = Probe::open(&canonical)
        .map_err(|e| e.to_string())?
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;

    let props = tagged_file.properties();
    let duration_ms = props.duration().as_millis() as i64;
    let bit_rate_kbps = props
        .audio_bitrate()
        .or_else(|| props.overall_bitrate())
        .unwrap_or(0) as i64;

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let fallback_title = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled Track")
        .to_owned();
    let fallback_album = canonical
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();

    let title = tag
        .and_then(|t| t.title().map(|c| c.into_owned()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_title);
    let artist_name = tag
        .and_then(|t| t.artist().map(|c| c.into_owned()))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let album_name = tag
        .and_then(|t| t.album().map(|c| c.into_owned()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_album);
    let released = tag
        .and_then(|t| t.year())
        .map(|y| y.to_string())
        .unwrap_or_default();
    let embedded_lyrics = tag
        .and_then(|t| t.get_string(&ItemKey::Lyrics))
        .map(str::to_owned);

    let local_cover = tag.and_then(|t| t.pictures().first()).and_then(|pic| {
        cache_embedded_cover(
            pic.data(),
            pic.mime_type().and_then(|m| m.ext()),
            cover_cache_dir,
        )
    });

    let cover_uri = local_cover
        .as_deref()
        .map(path_to_file_uri)
        .unwrap_or_default();
    let local_cover_str = local_cover
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let artists = match artist_name {
        Some(name) => vec![Artist {
            id: format!("local-artist-{}", short_hash(name.as_bytes())),
            name,
            image_url: cover_uri.clone(),
            following: false,
        }],
        None => Vec::new(),
    };

    let album_artist_key = artists
        .first()
        .map(|a| a.name.as_str())
        .unwrap_or("");
    let album_id = if album_name.is_empty() {
        String::new()
    } else {
        format!(
            "local-album-{}",
            short_hash(format!("{album_artist_key}::{album_name}").as_bytes())
        )
    };
    let track_id = format!(
        "local-track-{}",
        short_hash(canonical.to_string_lossy().as_bytes())
    );

    let album = Album {
        id: album_id,
        name: album_name,
        image_url: cover_uri,
        artists: artists.clone(),
        released,
        saved: false,
    };

    let track = Track {
        id: track_id,
        source: LocalMusicSource::SOURCE_ID.to_owned(),
        name: title,
        artists,
        album,
        duration: duration_ms,
        url: path_to_file_uri(&canonical),
        unavailable: false,
        show_id: String::new(),
        description: String::new(),
        published: String::new(),
        local_path: canonical.to_string_lossy().into_owned(),
        local_cover: local_cover_str,
    };

    let meta = LocalTrackMeta {
        bit_rate_kbps,
        file_size,
        media_type,
        embedded_lyrics,
    };

    Ok((track, meta))
}

fn cache_embedded_cover(
    bytes: &[u8],
    ext: Option<&str>,
    cover_cache_dir: &Path,
) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    if fs::create_dir_all(cover_cache_dir).is_err() {
        return None;
    }
    let digest = short_hash(bytes);
    let ext = ext.unwrap_or("jpg");
    let target = cover_cache_dir.join(format!("{digest}.{ext}"));
    if !target.exists() && fs::write(&target, bytes).is_err() {
        return None;
    }
    Some(target)
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let full = hex::encode(hasher.finalize());
    full[..16].to_owned()
}

fn path_to_file_uri(path: &Path) -> String {
    Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

fn mime_for_extension(ext: Option<&str>) -> &'static str {
    match ext.unwrap_or("").to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "opus" => "audio/opus",
        _ => "application/octet-stream",
    }
}

fn paginate<T: Clone>(items: &[T], offset: usize, limit: usize) -> (Vec<T>, bool) {
    if offset >= items.len() {
        return (Vec::new(), false);
    }
    let end = (offset + limit).min(items.len());
    (items[offset..end].to_vec(), end < items.len())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Tag, TagExt, TagType};

    use super::*;

    static LOCAL_TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let seq = LOCAL_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nsh-music-core-local-{label}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    fn create_minimal_wav(path: &Path) {
        let sample_rate: u32 = 8_000;
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let num_samples: u32 = sample_rate;
        let data_size = num_samples * u32::from(channels) * u32::from(bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        let block_align = channels * (bits_per_sample / 8);
        let riff_chunk_size = 36 + data_size;

        let mut buf = Vec::with_capacity((44 + data_size) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        buf.resize(buf.len() + data_size as usize, 0);

        fs::write(path, buf).expect("write wav");
    }

    #[test]
    fn empty_local_library_has_no_mock_tracks_and_strict_capabilities() {
        let cover_dir = temp_dir("empty-covers");
        let lib_path = temp_dir("empty-lib").join("library.json");
        let src = LocalMusicSource::with_storage(Vec::new(), cover_dir.clone(), Some(lib_path.clone()));

        assert_eq!(src.local_track_count(), 0);
        assert!(src.all_tracks().is_empty());
        assert!(src.home().unwrap().sections.is_empty());

        let info = src.info();
        assert_eq!(info.id, LocalMusicSource::SOURCE_ID);
        assert_eq!(info.name, "Local Files");
        assert!(info.settings.is_empty());
        assert!(info.navigation.is_empty());

        assert!(src.can("home"));
        assert!(src.can("search"));
        assert!(src.can("playlists"));
        assert!(src.can("createPlaylist"));
        assert!(src.can("renamePlaylist"));
        assert!(src.can("deletePlaylist"));
        assert!(src.can("addToPlaylist"));
        assert!(src.can("removeFromPlaylist"));
        assert!(src.can("movePlaylistTrack"));
        assert!(!src.can("stations"));
        assert!(!src.can("shows"));
        assert!(!src.can("comments"));
        assert!(!src.can("suggest"));
        assert!(!src.can("signIn"));
        assert!(!src.can("account"));
        assert!(!src.can("remoteDevices"));

        let _ = fs::remove_dir_all(&cover_dir);
        let _ = fs::remove_dir_all(lib_path.parent().unwrap());
    }

    #[test]
    fn scans_local_wav_persists_library_and_supports_playlist_crud() {
        let music_dir = temp_dir("music");
        let cover_dir = temp_dir("cover-cache");
        let lib_dir = temp_dir("lib-store");
        let lib_path = lib_dir.join("library.json");

        let wav1 = music_dir.join("track_one.wav");
        let wav2 = music_dir.join("track_two.wav");
        let lrc1 = music_dir.join("track_one.lrc");
        let ignored = music_dir.join("notes.txt");

        create_minimal_wav(&wav1);
        create_minimal_wav(&wav2);
        fs::write(&ignored, "not audio").expect("write txt");
        fs::write(&lrc1, "[00:00.20]Hello from sidecar\n").expect("write lrc");

        let mut tag1 = Tag::new(TagType::Id3v2);
        tag1.set_title("Local Acoustic Test".to_owned());
        tag1.set_artist("Test Performer".to_owned());
        tag1.set_album("Test Session EP".to_owned());
        tag1.set_year(2026);
        let fake_png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        tag1.push_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Png),
            None,
            fake_png,
        ));
        tag1.save_to_path(&wav1, lofty::config::WriteOptions::default())
            .expect("save id3v2 tag");

        let mut tag2 = Tag::new(TagType::Id3v2);
        tag2.set_title("Second Wave".to_owned());
        tag2.set_artist("Test Performer".to_owned());
        tag2.set_album("Test Session EP".to_owned());
        tag2.save_to_path(&wav2, lofty::config::WriteOptions::default())
            .expect("save id3v2 tag 2");

        {
            let src = LocalMusicSource::with_storage(
                vec![music_dir.clone()],
                cover_dir.clone(),
                Some(lib_path.clone()),
            );
            assert_eq!(src.local_track_count(), 2);

            let tracks = src.all_tracks();
            let first = tracks.iter().find(|t| t.name == "Local Acoustic Test").unwrap().clone();
            let second = tracks.iter().find(|t| t.name == "Second Wave").unwrap().clone();
            assert_eq!(first.artist_names(), "Test Performer");
            assert!(first.url.starts_with("file://"));
            assert!(!first.local_cover.is_empty());

            let stream = src.stream(&first.id, Quality::Lossless).expect("local stream");
            assert_eq!(stream.media_type, "audio/wav");

            let lyrics = src.lyrics(&first.id).expect("sidecar lyrics");
            assert_eq!(lyrics.lines[0].text, "Hello from sidecar");

            let home = src.home().expect("home");
            assert!(!home.sections.is_empty());

            let search_res = src.search("acoustic", SearchKind::Tracks, 0).expect("search");
            assert_eq!(search_res.tracks.len(), 1);

            src.like(&first.id, true).expect("like");
            src.played(&first);

            let pl = src.create_playlist("Session Favorites").expect("create playlist");
            src.add_to_playlist(&pl.id, &[first.id.clone(), second.id.clone()])
                .expect("add");
            src.move_playlist_track(&pl.id, 0, 1).expect("move");
            assert_eq!(src.playlist(&pl.id).unwrap().tracks[0].id, second.id);
            src.rename_playlist(&pl.id, "Renamed Mix").expect("rename");
            src.remove_from_playlist(&pl.id, std::slice::from_ref(&second.id))
                .expect("remove");
            assert_eq!(src.playlist(&pl.id).unwrap().tracks.len(), 1);
        }

        // Reload from persisted library.json and verify state survived intact.
        let reloaded = LocalMusicSource::with_storage(
            Vec::new(),
            cover_dir.clone(),
            Some(lib_path.clone()),
        );
        assert_eq!(reloaded.local_track_count(), 2);
        assert_eq!(reloaded.liked(0).unwrap().tracks.len(), 1);
        assert_eq!(reloaded.recent().unwrap().len(), 1);
        let pls = reloaded.playlists().unwrap();
        assert_eq!(pls.len(), 1);
        assert_eq!(pls[0].name, "Renamed Mix");
        assert_eq!(pls[0].track_count, 1);

        reloaded.delete_playlist(&pls[0].id).expect("delete");
        assert!(reloaded.playlists().unwrap().is_empty());

        let _ = fs::remove_dir_all(&music_dir);
        let _ = fs::remove_dir_all(&cover_dir);
        let _ = fs::remove_dir_all(&lib_dir);
    }
}
