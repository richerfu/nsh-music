//! Interactive full-window `Wall` panel (`NowPlayingPanel::Wall`) aligned 1:1 with
//! `egoist/mygo-music` (`internal/app/ui_wall.go`):
//! - Upper hero (`wallSong`): breathing cover (`1.0` playing / `0.92` paused), title,
//!   Like heart, `···` song menu, clickable artist links, current & next lyric lines
//! - Lower gently-swaying scrollable interactive cover grid of the queue:
//!   * Active track elevated (`scale 1.06`) with crisp `2pt` white ring
//!   * Hover lifts (`1.04`) and reveals dark bottom-gradient caption overlay
//!   * Click jumps straight to that track (`store.play_from_queue(idx)`)
//!   * Lazy 7s GPU-composited row sway — ZERO 60ms `position_ms` rebuilds per row!
//! - Slim `wallBar` scrubber + transport along the foot

use std::time::Duration;

use nsh_base::button;
use nsh_core::{
    animation::{Curve, EaseMode},
    prelude::*,
};
use nsh_music_audio::PlaybackStatus;

use super::{
    buffering_badge, clickable_artist_links, phone_scrubber_meta_row, phone_thumb_transport,
    song_more_menu, stage_like_button, stage_scrubber_bar, stage_transport,
};
use crate::{
    state::{AppStore, ArtworkLoader},
    ui::{
        icons::{Icon, icon},
        motion,
        theme::{self, DeviceForm, Elevated, Elevation, colors, font_size},
        track_row::format_duration_ms,
    },
};

/// Duration of one half-sway across a Wall row (`wallSwayHalf = 7s`).
const WALL_SWAY_HALF_MS: f32 = 7_000.0;
const WALL_SWAY_AMPLITUDE: f32 = 24.0;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WallQueueEntry {
    queue_idx: usize,
    id: String,
    name: SharedString,
    artists: SharedString,
    cover_uri: SharedString,
    seed: SharedString,
}

/// Renders the full-window interactive `Wall` panel (`ui_wall.go:3420-3709`).
pub fn wall_panel(store: AppStore, form: DeviceForm) -> View {
    view()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        // Upper song hero (`wallSong`)
        .child(wall_song_hero(store, form))
        // Lower gently-swaying interactive queue cover grid
        .child(wall_cover_grid(store, form))
        // Slim `wallBar` scrubber + transport at foot
        .child(wall_bottom_bar(store, form))
}

/// Upper hero (`wallSong`, `ui_wall.go:3455-3511`): breathing cover, title/like/more,
/// clickable artists, and active + next lyric lines (subscribed ONLY to `active_lyric`).
/// On Phone, `phone_mini_track_bar` already presents cover/title/artist/quick-pause at top,
/// so `wall_song_hero` renders a clean centered 2-line active & next lyric banner with 20pt margins.
fn wall_song_hero(store: AppStore, form: DeviceForm) -> View {
    let is_phone = form.is_phone();
    let touch = form.is_touch();

    let sung_line = computed(move || {
        let Some(cur) = store.active_lyric.get() else {
            return SharedString::default();
        };
        store.lyrics.with(|l| {
            l.lines
                .get(cur.line_index)
                .map(|line| SharedString::from(line.text.clone()))
                .unwrap_or_default()
        })
    });

    let next_line = computed(move || {
        let idx = store
            .active_lyric
            .get()
            .map(|a| a.line_index + 1)
            .unwrap_or(0);
        store.lyrics.with(|l| {
            l.lines
                .get(idx)
                .map(|line| SharedString::from(line.text.clone()))
                .unwrap_or_default()
        })
    });

    if is_phone {
        let has_lyrics = computed(move || {
            sung_line.with(|s| !s.is_empty()) || next_line.with(|s| !s.is_empty())
        });
        return view().w_full().child(when(has_lyrics, move || {
            view()
                .w_full()
                .px(px(20.0))
                .pt_1()
                .pb_2()
                .items_center()
                .gap_0p5()
                .child(when(
                    move || sung_line.with(|s| !s.is_empty()),
                    move || {
                        text(move || sung_line.get())
                            .text_center()
                            .number_of_lines(2)
                            .text_size(px(15.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(move || theme::np_fg().opacity(0.90))
                    },
                ))
                .child(when(
                    move || next_line.with(|s| !s.is_empty()),
                    move || {
                        text(move || next_line.get())
                            .text_center()
                            .number_of_lines(1)
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || theme::np_fg().opacity(0.45))
                    },
                ))
        }));
    }

    let cover_size = 196.0;
    let initial_playing = untracked(|| store.is_playing.get());
    let breathe = animated_value(if initial_playing || reduce_motion() {
        1.0
    } else {
        0.92
    });
    let prev_playing = stored(initial_playing);

    effect(move || {
        let playing = store.is_playing.get();
        if prev_playing.replace(playing) != playing {
            let target = if playing || reduce_motion() {
                1.0
            } else {
                0.92
            };
            motion::animate_spring(&breathe, target, motion::spring_gentle());
        }
    });

    let has_art = computed(move || store.current_artwork_uri.with(|u| !u.is_empty()));
    let is_desktop = form.is_desktop();

    view()
        .w_full()
        .pl(move || {
            px(if is_desktop {
                72.0 + safe_area().left.0
            } else {
                72.0
            })
        })
        .pr(move || {
            px(if is_desktop {
                72.0 + safe_area().right.0
            } else {
                72.0
            })
        })
        .pt(px(14.0))
        .pb(px(20.0))
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(36.0))
        // Breathing cover (`1.0` playing / `0.92` paused)
        .child(
            Elevated::elevation(
                view()
                    .size(px(cover_size))
                    .flex_shrink_0()
                    .rounded(px(16.0))
                    .overflow_hidden()
                    .bg(theme::np_surface)
                    .bind_scale(&breathe)
                    .items_center()
                    .justify_center(),
                Elevation::Modal,
            )
            .child(when(
                move || !has_art.get(),
                move || {
                    icon(
                        Icon::Disc,
                        (cover_size * 0.30).clamp(26.0, 56.0),
                        colors::NP_FG,
                    )
                    .opacity(0.32)
                },
            ))
            .child(when(has_art, move || {
                image(move || store.current_artwork_uri.get())
                    .object_fit(ResizeMode::Cover)
                    .absolute()
                    .inset_0()
                    .size_full()
            })),
        )
        // Right metadata & 2-line lyric preview
        .child(
            view()
                .flex_1()
                .max_w(px(440.0))
                .min_w(px(0.0))
                .gap_1()
                .child(
                    view()
                        .w_full()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            text(move || {
                                store.current_track.with(|t| {
                                    t.as_ref()
                                        .map(|tr| SharedString::from(tr.name.clone()))
                                        .unwrap_or_else(|| store.t("player.idle_title"))
                                })
                            })
                            .flex_1()
                            .min_w(px(0.0))
                            .number_of_lines(1)
                            .text_size(px(26.0))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .letter_spacing(px(-0.4))
                            .text_color(theme::np_fg),
                        )
                        .child(stage_like_button(store, 20.0, touch))
                        .child(song_more_menu(store, 20.0, touch)),
                )
                .child(clickable_artist_links(store, 14.0, 0.60))
                // Active + next lyric line
                .child(when(
                    move || sung_line.with(|s| !s.is_empty()),
                    move || {
                        text(move || sung_line.get())
                            .mt(px(14.0))
                            .number_of_lines(2)
                            .text_size(px(17.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(move || theme::np_fg().opacity(0.85))
                    },
                ))
                .child(when(
                    move || next_line.with(|s| !s.is_empty()),
                    move || {
                        text(move || next_line.get())
                            .number_of_lines(1)
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || theme::np_fg().opacity(0.38))
                    },
                )),
        )
}

/// Lower gently-swaying scrollable interactive cover grid of `store.queue`.
/// Uses lazy 7s `AnimatedValue` row sway (ZERO 60ms `position_ms` rebuilds!).
fn wall_cover_grid(store: AppStore, form: DeviceForm) -> View {
    let is_phone = form.is_phone();
    let gap_pt = if is_phone { 10.0_f32 } else { 14.0_f32 };
    let tile_pt = if is_phone {
        ((window().width.0 - 40.0 - 2.0 * gap_pt) / 3.0).clamp(96.0, 118.0)
    } else {
        112.0_f32
    };

    // Two opposing UI-thread animated sway channels (even vs odd rows), stepping once every 7s
    let sway_even = animated_value(0.0_f32);
    let sway_odd = animated_value(0.0_f32);
    let sway_running = stored(false);
    let sway_dir = stored(1.0_f32);

    effect(move || {
        let should_sway =
            store.now_playing_open.get() && store.is_playing.get() && !reduce_motion();
        if !should_sway {
            sway_running.with(|r| *r = false);
            motion::animate_spring(&sway_even, 0.0, motion::spring_gentle());
            motion::animate_spring(&sway_odd, 0.0, motion::spring_gentle());
            return;
        }
        let started = sway_running.with(|r| {
            if *r {
                false
            } else {
                *r = true;
                true
            }
        });
        if started {
            spawn(async move {
                let timing = Timing::new(
                    WALL_SWAY_HALF_MS,
                    Easing::Curve(Curve::Sin, EaseMode::InOut),
                );
                loop {
                    let active = store.now_playing_open.get_untracked()
                        && store.is_playing.get_untracked()
                        && !reduce_motion();
                    if !active {
                        sway_running.with(|r| *r = false);
                        motion::animate_spring(&sway_even, 0.0, motion::spring_gentle());
                        motion::animate_spring(&sway_odd, 0.0, motion::spring_gentle());
                        break;
                    }
                    let dir = sway_dir.with(|d| {
                        let cur = *d;
                        *d = -cur;
                        cur
                    });
                    motion::animate_timing(&sway_even, dir * WALL_SWAY_AMPLITUDE, timing);
                    motion::animate_timing(&sway_odd, -dir * WALL_SWAY_AMPLITUDE, timing);
                    timer(Duration::from_millis(WALL_SWAY_HALF_MS as u64)).await;
                }
            });
        }
    });

    let rows_memo = computed(move || {
        let w = window().width.0;
        let pad = if form.is_phone() { 20.0 } else { 64.0 };
        let pitch = tile_pt + gap_pt;
        let cols = (((w - 2.0 * pad + gap_pt) / pitch).floor() as usize).clamp(3, 10);

        let unknown = store.t("track.unknown_artist");
        let entries: Vec<WallQueueEntry> = store.queue.with(|q| {
            q.iter()
                .enumerate()
                .map(|(i, tr)| {
                    let raw = if !tr.local_cover.is_empty() {
                        tr.local_cover.clone()
                    } else {
                        tr.album.image_url.clone()
                    };
                    let cover_uri = if raw.is_empty() {
                        SharedString::default()
                    } else {
                        SharedString::from(ArtworkLoader::resolve_template(&raw, 240))
                    };
                    let seed = if !tr.album.id.is_empty() {
                        tr.album.id.clone()
                    } else {
                        tr.id.clone()
                    };
                    let names = tr.artist_names();
                    let artists = if names.is_empty() {
                        unknown.clone()
                    } else {
                        SharedString::from(names)
                    };
                    WallQueueEntry {
                        queue_idx: i,
                        id: tr.id.clone(),
                        name: SharedString::from(tr.name.clone()),
                        artists,
                        cover_uri,
                        seed: SharedString::from(seed),
                    }
                })
                .collect()
        });

        entries
            .chunks(cols)
            .enumerate()
            .map(|(ri, chunk)| (ri, chunk.to_vec()))
            .collect::<Vec<_>>()
    });

    let is_empty = computed(move || rows_memo.with(Vec::is_empty));

    view()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .overflow_hidden()
        .child(
            when(is_empty, move || {
                view()
                    .flex_1()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .child(
                        text(move || store.t("player.queue_empty"))
                            .text_size(px(font_size::HEADING))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || theme::np_fg().opacity(0.42)),
                    )
            })
            .otherwise(move || {
                scroll().flex_1().w_full().scrollbar(false).child(
                    view()
                        .w_full()
                        .px(px(if is_phone { 20.0 } else { 48.0 }))
                        .pt(px(if is_phone { 8.0 } else { 16.0 }))
                        .pb_6()
                        .gap(px(gap_pt))
                        .items_center()
                        .child(each(
                            rows_memo,
                            |(ri, _): &(usize, Vec<WallQueueEntry>)| *ri,
                            move |row_sig, _| {
                                let row_idx = row_sig.with_untracked(|(ri, _)| *ri);
                                let sway_ref = if row_idx.is_multiple_of(2) {
                                    &sway_even
                                } else {
                                    &sway_odd
                                };
                                let items_memo =
                                    computed(move || row_sig.with(|(_, items)| items.clone()));

                                view()
                                    .flex_row()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(gap_pt))
                                    .bind_translate_x(sway_ref)
                                    .child(each(
                                        items_memo,
                                        |e: &WallQueueEntry| (e.queue_idx, e.id.clone()),
                                        move |entry_sig, _| {
                                            wall_tile_button(store, entry_sig, tile_pt)
                                        },
                                    ))
                            },
                        )),
                )
            }),
        )
}

fn wall_tile_button(store: AppStore, entry_sig: ReadSignal<WallQueueEntry>, tile_pt: f32) -> View {
    let queue_idx = entry_sig.with_untracked(|e| e.queue_idx);
    let is_playing_tile = computed(move || store.current_index.get() == Some(queue_idx));
    let hovered = signal(false);

    let tile_scale = animated_value(if untracked(|| is_playing_tile.get()) && !reduce_motion() {
        1.07
    } else {
        1.0
    });
    let prev_scale = stored(if untracked(|| is_playing_tile.get()) && !reduce_motion() {
        1.07_f32
    } else {
        1.0_f32
    });

    effect(move || {
        let active = is_playing_tile.get();
        let over = hovered.get();
        let target = if reduce_motion() {
            1.0
        } else if active {
            1.07
        } else if over {
            1.04
        } else {
            1.0
        };
        if (prev_scale.get() - target).abs() > 0.001 {
            prev_scale.replace(target);
            motion::animate_spring(&tile_scale, target, motion::spring_snappy());
        }
    });

    let has_img = move || entry_sig.with(|e| !e.cover_uri.is_empty());

    view().child(
        Elevated::elevation(
            button()
                .selected(is_playing_tile)
                .accessibility_label(move || Some(entry_sig.with(|e| e.name.clone())))
                .on_press(move || {
                    if !is_playing_tile.get_untracked() {
                        store.play_from_queue(queue_idx);
                    }
                })
                .on_mouse_enter(move |_: &MouseEvent| hovered.set(true))
                .on_mouse_leave(move |_: &MouseEvent| hovered.set(false))
                .relative()
                .size(px(tile_pt))
                .flex_shrink_0()
                .rounded(px(12.0))
                .overflow_hidden()
                .bind_scale(&tile_scale)
                .bg_linear_to_br()
                .gradient_from(move || {
                    let (a, _) = entry_sig.with(|e| theme::fallback_cover_gradient(&e.seed));
                    AdaptiveColor::from(a)
                })
                .gradient_to(move || {
                    let (_, b) = entry_sig.with(|e| theme::fallback_cover_gradient(&e.seed));
                    AdaptiveColor::from(b)
                })
                .items_center()
                .justify_center()
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring)),
            Elevation::Card,
        )
        .child(when(
            move || !has_img(),
            move || {
                icon(
                    Icon::Disc,
                    (tile_pt * 0.30).clamp(24.0, 42.0),
                    colors::NP_FG,
                )
                .opacity(0.40)
            },
        ))
        .child(when(has_img, move || {
            image(move || entry_sig.with(|e| e.cover_uri.clone()))
                .object_fit(ResizeMode::Cover)
                .absolute()
                .inset_0()
                .size_full()
        }))
        // Hover dark bottom-gradient caption overlay (`ui_wall.go:3674-3686`)
        .child(when(hovered, move || {
            view()
                .absolute()
                .inset_0()
                .p_2()
                .gap_0p5()
                .justify_end()
                .bg_linear_to_t()
                .gradient_from(move || theme::np_bg().opacity(0.82))
                .gradient_to(colors::TRANSPARENT)
                .pointer_events(PointerEvents::None)
                .child(
                    text(move || entry_sig.with(|e| e.name.clone()))
                        .number_of_lines(1)
                        .text_size(px(12.0))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::np_fg),
                )
                .child(
                    text(move || entry_sig.with(|e| e.artists.clone()))
                        .number_of_lines(1)
                        .text_size(px(10.5))
                        .text_color(move || theme::np_fg().opacity(0.72)),
                )
        }))
        // Crisp 2pt white ring on the active playing track (`ui_wall.go:3687-3689`)
        .child(when(is_playing_tile, || {
            view()
                .absolute()
                .inset_0()
                .rounded(px(12.0))
                .border_2()
                .border_color(move || theme::np_fg().opacity(0.90))
                .pointer_events(PointerEvents::None)
        })),
    )
}

/// Slim `wallBar` along the foot (`ui_wall.go:3695-3709`):
/// - Desktop & Pad: smoke scrubber + row (`elapsed` · centered borderless white transport · `-remaining`).
/// - Phone: stacked `20pt`-padded scrubber + `elapsed / Lossless-or-quality / remaining` + evenly distributed thumb transport.
fn wall_bottom_bar(store: AppStore, form: DeviceForm) -> View {
    let is_phone = form.is_phone();

    if is_phone {
        return view()
            .w_full()
            .px(px(20.0))
            .pt_1()
            .gap_1p5()
            .child(
                view()
                    .w_full()
                    .gap_0p5()
                    .child(stage_scrubber_bar(store, false))
                    .child(phone_scrubber_meta_row(store)),
            )
            .child(phone_thumb_transport(store, 54.0));
    }

    let is_desktop = form.is_desktop();
    view()
        .w_full()
        .pl(move || {
            px(if is_desktop {
                48.0 + safe_area().left.0
            } else {
                48.0
            })
        })
        .pr(move || {
            px(if is_desktop {
                48.0 + safe_area().right.0
            } else {
                48.0
            })
        })
        .pb(move || {
            px(if is_desktop {
                24.0 + safe_area().bottom.0
            } else {
                24.0
            })
        })
        .pt_2()
        .gap_2p5()
        .child(stage_scrubber_bar(store, false))
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3p5()
                .child(
                    view().flex_1().min_w(px(0.0)).child(
                        text(move || format_duration_ms(store.position_ms.get() as i64))
                            .tabular_nums()
                            .text_size(px(font_size::MICRO))
                            .text_color(move || theme::np_fg().opacity(0.50)),
                    ),
                )
                .child(stage_transport(store, form, 22.0, false))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .child(
                            when(
                                move || store.playback_status.with(PlaybackStatus::is_loading),
                                move || buffering_badge(store),
                            )
                            .otherwise(move || {
                                text(move || {
                                    let dur = store.duration_ms.get();
                                    let pos = store.position_ms.get().min(dur);
                                    let rem = dur.saturating_sub(pos);
                                    format!("-{}", format_duration_ms(rem as i64))
                                })
                                .tabular_nums()
                                .text_size(px(font_size::MICRO))
                                .text_color(move || theme::np_fg().opacity(0.50))
                            }),
                        ),
                ),
        )
}
