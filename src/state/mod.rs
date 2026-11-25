//! Application state store and artwork cache loader for `nsh-music`.

pub mod artwork;
pub mod store;
pub mod web_cookies;

pub use artwork::{ArtworkLoader, ResolvedArtwork};
pub use store::{
    ActiveModal, AppStore, NowPlayingMode, NowPlayingPanel, NowPlayingStageStyle, Route,
    SettingsSection, SourceEntry, provide_store, use_store,
};
