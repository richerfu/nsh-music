//! `Cover Wall` atmospheric mosaic (`NowPlayingStageStyle::Wall`) arranging
//! queue & catalog album covers behind the active track controls.

use std::collections::HashSet;

use nsh_core::prelude::*;

use crate::{
    state::AppStore,
    ui::{
        icons::{Icon, icon},
        motion,
        theme::{self, DeviceForm, colors, radius},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct WallTile {
    key: String,
    seed: SharedString,
    uri: SharedString,
    is_current: bool,
}

/// Renders the atmospheric Cover Wall album mosaic layered behind the active
/// track controls whenever [`crate::state::NowPlayingStageStyle::Wall`] is active.
pub fn cover_wall_mosaic(store: AppStore, form: DeviceForm) -> View {
    let (rows, cols, tile_pt) = match form {
        DeviceForm::Desktop => (4_usize, 6_usize, 152.0_f32),
        DeviceForm::Pad { landscape: true } => (4_usize, 5_usize, 134.0_f32),
        DeviceForm::Pad { landscape: false } => (5_usize, 4_usize, 128.0_f32),
        DeviceForm::Phone => (5_usize, 3_usize, 114.0_f32),
    };
    let needed = rows * cols;

    let tiles_memo = computed(move || collect_wall_tiles(store, needed));

    view()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .pointer_events(PointerEvents::None)
        .items_center()
        .justify_center()
        // Slightly oversized tilted masonry grid so edges never reveal gaps
        .child(
            view()
                .gap_3()
                .items_center()
                .justify_center()
                .rotate(move || {
                    if reduce_motion() {
                        deg(0.0)
                    } else {
                        deg(-5.5)
                    }
                })
                .scale(1.12)
                .children((0..rows).map(move |row_idx| {
                    let row_offset = if row_idx.is_multiple_of(2) {
                        -22.0_f32
                    } else {
                        22.0_f32
                    };
                    view()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .translate_x(move || {
                            if reduce_motion() {
                                px(row_offset)
                            } else {
                                let playing = store.is_playing.get();
                                let drift = if playing {
                                    let secs = store.position_ms.get() as f32 * 0.001;
                                    let dir = if row_idx.is_multiple_of(2) { 1.0 } else { -1.0 };
                                    (secs * 0.16).sin() * 14.0 * dir
                                } else {
                                    0.0
                                };
                                px(row_offset + drift)
                            }
                        })
                        .transition_transform()
                        .duration(move || motion::transition_ms(480.0))
                        .children((0..cols).map(move |col_idx| {
                            let flat_idx = row_idx * cols + col_idx;
                            wall_tile_view(tiles_memo, flat_idx, tile_pt)
                        }))
                })),
        )
        // Soft atmospheric vignette over the cover wall so controls & lyrics stay crisp
        .child(
            view()
                .absolute()
                .inset_0()
                .bg(move || {
                    AdaptiveColor::from(
                        theme::artwork_stage_bg(store.current_accent.get()).alpha(0.62),
                    )
                })
                .pointer_events(PointerEvents::None),
        )
        .child(
            view()
                .absolute()
                .inset_0()
                .bg_linear_to_b()
                .gradient_from(move || theme::np_bg().opacity(0.65))
                .gradient_via(colors::TRANSPARENT)
                .gradient_to(move || theme::np_bg().opacity(0.84))
                .pointer_events(PointerEvents::None),
        )
}

fn wall_tile_view(tiles_memo: Memo<Vec<WallTile>>, index: usize, tile_pt: f32) -> View {
    let tile = computed(move || {
        tiles_memo.with(|list| {
            list.get(index).cloned().unwrap_or_else(|| WallTile {
                key: format!("wall-fallback-{index}"),
                seed: SharedString::from(format!("wall-{index}")),
                uri: SharedString::default(),
                is_current: false,
            })
        })
    });

    let has_img = move || tile.with(|t| !t.uri.is_empty());

    view()
        .size(px(tile_pt))
        .flex_shrink_0()
        .rounded(px(radius::LG))
        .overflow_hidden()
        .border_1()
        .border_color(move || {
            if tile.with(|t| t.is_current) {
                theme::np_fg().opacity(0.45)
            } else {
                theme::np_border()
            }
        })
        .opacity(move || {
            if tile.with(|t| t.is_current) {
                0.85_f32
            } else {
                0.44_f32
            }
        })
        .bg_linear_to_br()
        .gradient_from(move || {
            let (a, _) = tile.with(|t| theme::fallback_cover_gradient(&t.seed));
            AdaptiveColor::from(a)
        })
        .gradient_to(move || {
            let (_, b) = tile.with(|t| theme::fallback_cover_gradient(&t.seed));
            AdaptiveColor::from(b)
        })
        .items_center()
        .justify_center()
        .child(icon(Icon::Disc, (tile_pt * 0.34).clamp(24.0, 48.0), colors::NP_MUTED).opacity(0.55))
        .child(when(has_img, move || {
            image(move || tile.with(|t| t.uri.clone()))
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
        }))
}

fn collect_wall_tiles(store: AppStore, needed: usize) -> Vec<WallTile> {
    let mut seen = HashSet::new();
    let mut base: Vec<WallTile> = Vec::with_capacity(needed);

    let current_id = store
        .current_track
        .with(|t| t.as_ref().map(|tr| tr.id.clone()).unwrap_or_default());
    let current_art = store.current_artwork_uri.get();

    // 1. Active track first
    if let Some(track) = store.current_track.get() {
        let seed = if !track.album.id.is_empty() {
            track.album.id.clone()
        } else {
            track.id.clone()
        };
        seen.insert(seed.clone());
        base.push(WallTile {
            key: format!("cur-{seed}"),
            seed: SharedString::from(seed),
            uri: current_art,
            is_current: true,
        });
    }

    // 2. Queue tracks
    store.queue.with(|queue| {
        for tr in queue {
            let seed = if !tr.album.id.is_empty() {
                tr.album.id.clone()
            } else {
                tr.id.clone()
            };
            if seen.insert(seed.clone()) {
                let raw = if !tr.local_cover.is_empty() {
                    tr.local_cover.clone()
                } else {
                    tr.album.image_url.clone()
                };
                base.push(WallTile {
                    key: format!("q-{seed}"),
                    seed: SharedString::from(seed),
                    uri: SharedString::from(raw),
                    is_current: tr.id == current_id,
                });
            }
        }
    });

    // 3. Saved albums & playlists from catalog
    store.saved_albums.with(|albums| {
        for alb in albums {
            let seed = if !alb.id.is_empty() {
                alb.id.clone()
            } else {
                alb.name.clone()
            };
            if seen.insert(seed.clone()) {
                base.push(WallTile {
                    key: format!("alb-{seed}"),
                    seed: SharedString::from(seed),
                    uri: SharedString::from(alb.image_url.clone()),
                    is_current: false,
                });
            }
        }
    });

    store.playlists.with(|playlists| {
        for pl in playlists {
            let seed = if !pl.id.is_empty() {
                pl.id.clone()
            } else {
                pl.name.clone()
            };
            if seen.insert(seed.clone()) {
                base.push(WallTile {
                    key: format!("pl-{seed}"),
                    seed: SharedString::from(seed),
                    uri: SharedString::from(pl.image_url.clone()),
                    is_current: false,
                });
            }
        }
    });

    // 4. Home sections covers (albums, playlists, tracks, stations, shows)
    store.home_data.with(|home| {
        for sec in &home.sections {
            for alb in &sec.albums {
                if seen.insert(alb.id.clone()) {
                    base.push(WallTile {
                        key: format!("hal-{}", alb.id),
                        seed: SharedString::from(alb.id.clone()),
                        uri: SharedString::from(alb.image_url.clone()),
                        is_current: false,
                    });
                }
            }
            for tr in &sec.tracks {
                let seed = if !tr.album.id.is_empty() {
                    tr.album.id.clone()
                } else {
                    tr.id.clone()
                };
                if seen.insert(seed.clone()) {
                    base.push(WallTile {
                        key: format!("htr-{seed}"),
                        seed: SharedString::from(seed),
                        uri: SharedString::from(tr.album.image_url.clone()),
                        is_current: tr.id == current_id,
                    });
                }
            }
        }
    });

    // Guarantee a rich full mosaic even when the library is minimal
    if base.is_empty() {
        for idx in 0..needed {
            base.push(WallTile {
                key: format!("seed-{idx}"),
                seed: SharedString::from(format!("nsh-wall-{idx}")),
                uri: SharedString::default(),
                is_current: idx == needed / 2,
            });
        }
        return base;
    }

    let mut filled = Vec::with_capacity(needed);
    for i in 0..needed {
        let src = &base[i % base.len()];
        filled.push(WallTile {
            key: format!("{}-{i}", src.key),
            seed: src.seed.clone(),
            uri: src.uri.clone(),
            is_current: src.is_current && i < base.len(),
        });
    }
    filled
}
