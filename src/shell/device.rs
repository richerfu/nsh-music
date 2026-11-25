//! Reactive `DeviceForm` shell dispatcher mounting:
//! - Root `.on_file_drop(...)` forwarding dropped music folders and `.zip`/plugin directories
//! - Adaptive `Desktop`, `Pad` (Landscape/Portrait), or `Phone` shell
//! - Persistent animated Now Playing overlay (`now-playing-host`) honoring `reduce_motion()`
//!   and `grow_from_cover`, with vertical drag-down-to-dismiss responder handling
//! - Global adaptive modal host `pages::render_active_modal(store)`
//! - Global toast notification stack `ui::overlays::toast_overlay(store.toasts)`

use std::time::Duration;

use nsh_core::prelude::*;

use crate::{
    pages,
    shell::{desktop, pad, phone},
    state::AppStore,
    ui::{
        motion,
        overlays::toast_overlay,
        theme::{self, DeviceForm},
    },
};

/// Root reactive shell adapting across Desktop, Pad, and Phone viewports.
pub fn device_shell(store: AppStore) -> View {
    let form = computed(theme::detect_device_form);

    // Persistent mount state and UI-thread animation values for Now Playing.
    let np_mounted = signal(false);
    let np_slide_y = animated_value(0.0);
    let np_scale = animated_value(1.0);
    let np_fade = animated_value(0.0);
    let drag_offset = stored(0.0_f32);
    let close_gen = stored(0_u64);

    effect(move || {
        let open = store.now_playing_open.get();
        let close_token = close_gen.with(|g| {
            *g += 1;
            *g
        });

        if open {
            untracked(|| {
                let _ = drag_offset.replace(0.0);
                np_mounted.set(true);
                if reduce_motion() {
                    np_slide_y.set(0.0);
                    np_scale.set(1.0);
                    np_fade.set(1.0);
                } else {
                    let h = window().height.0.max(480.0);
                    let grow = store.settings.with_untracked(|s| s.grow_from_cover);
                    if grow {
                        np_scale.set(0.86);
                        np_slide_y.set((h * 0.16).clamp(64.0, 132.0));
                    } else {
                        np_scale.set(1.0);
                        np_slide_y.set((h * 0.28).clamp(140.0, 260.0));
                    }
                    np_fade.set(0.0);
                    motion::animate_spring(&np_slide_y, 0.0, motion::spring_gentle());
                    motion::animate_spring(&np_scale, 1.0, motion::spring_gentle());
                    motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                }
            });
        } else if np_mounted.get_untracked() {
            untracked(|| {
                let start_y = drag_offset.replace(0.0);
                if reduce_motion() {
                    np_mounted.set(false);
                    np_slide_y.set(0.0);
                    np_scale.set(1.0);
                    np_fade.set(0.0);
                } else {
                    let h = window().height.0.max(480.0);
                    let grow = store.settings.with_untracked(|s| s.grow_from_cover);
                    if grow {
                        let target_y = (start_y + 110.0).max(110.0);
                        motion::animate_timing(&np_slide_y, target_y, motion::timing_fast());
                        motion::animate_timing(&np_scale, 0.88, motion::timing_fast());
                    } else {
                        let target_y = (start_y + 220.0).max((h * 0.36).clamp(180.0, 320.0));
                        motion::animate_timing(&np_slide_y, target_y, motion::timing_fast());
                    }
                    motion::animate_timing(&np_fade, 0.0, motion::timing_fast());
                    spawn(async move {
                        timer(Duration::from_millis(210)).await;
                        if close_gen.with(|g| *g == close_token)
                            && !store.now_playing_open.get_untracked()
                        {
                            np_mounted.set(false);
                            np_slide_y.set(0.0);
                            np_scale.set(1.0);
                            np_fade.set(0.0);
                        }
                    });
                }
            });
        }
    });

    view()
        .test_id("nsh-music-root")
        .flex_1()
        .w_full()
        .h_full()
        .bg(theme::bg)
        .text_color(theme::fg)
        .on_file_drop(move |ev: &FileDropEvent| store.handle_dropped_uris(&ev.uris))
        // Reactive status bar contrast on mobile/immersive platforms:
        // Light foreground over dark theme or the obsidian Now Playing stage,
        // Dark foreground over Warm Porcelain light theme.
        .child(when(
            move || !os().is_desktop(),
            move || {
                nsh_core::element::screen()
                    .header_visible(false)
                    .status_bar(move || {
                        let style = if store.now_playing_open.get() || dark() {
                            StatusBarStyle::Light
                        } else {
                            StatusBarStyle::Dark
                        };
                        StatusBarSetting::default()
                            .foreground(style)
                            .animated(!reduce_motion())
                    })
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(px(0.0))
                    .h(px(0.0))
                    .opacity(0.0)
                    .pointer_events(PointerEvents::None)
            },
        ))
        // Primary adaptive workspace shell (Desktop / Pad / Phone).
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(dynamic(move || {
                    let current_form = form.get();
                    untracked(move || match current_form {
                        DeviceForm::Desktop => {
                            desktop::desktop_shell(store).key("device-shell-desktop")
                        }
                        DeviceForm::Pad { landscape } => {
                            pad::pad_shell(store, landscape).key(if landscape {
                                "device-shell-pad-landscape"
                            } else {
                                "device-shell-pad-portrait"
                            })
                        }
                        DeviceForm::Phone => phone::phone_shell(store).key("device-shell-phone"),
                    })
                })),
        )
        // Persistent animated Now Playing overlay with upstream-matched drag-down-to-dismiss.
        .child(when(np_mounted, move || {
            view()
                .test_id("now-playing-host")
                .absolute()
                .inset_0()
                .pointer_events(move || {
                    if store.now_playing_open.get() {
                        PointerEvents::Auto
                    } else {
                        PointerEvents::None
                    }
                })
                .bind_translate_y(&np_slide_y)
                .bind_scale(&np_scale)
                .bind_opacity(&np_fade)
                .claim_touches(TouchClaim::on_pan(PanActivation::vertical(
                    px(10.0),
                    px(12.0),
                )))
                .on_responder_move(move |ev: &ResponderEvent| {
                    if !store.now_playing_open.get_untracked() {
                        return;
                    }
                    let h = untracked(|| window().height.0.max(480.0));
                    let shift = rubberband_up(ev.gesture.delta.y.0);
                    let down = shift.max(0.0);
                    let _ = drag_offset.replace(down);
                    np_slide_y.set(shift);
                    let pulled = (down / h).clamp(0.0, 1.0);
                    np_fade.set((1.0 - pulled * 0.35).clamp(0.65, 1.0));
                    if store.settings.with_untracked(|s| s.grow_from_cover) {
                        np_scale.set((1.0 - pulled * 0.12).clamp(0.86, 1.0));
                    }
                })
                .on_responder_release(move |ev: &ResponderEvent| {
                    if !store.now_playing_open.get_untracked() {
                        return;
                    }
                    let h = untracked(|| window().height.0.max(480.0));
                    let dy = ev.gesture.delta.y.0.max(0.0);
                    let threshold = (h * 0.20).clamp(96.0, 180.0);
                    if dy > threshold || (dy > 24.0 && ev.gesture.vy > 700.0) {
                        let _ = drag_offset.replace(dy);
                        store.close_now_playing();
                    } else {
                        let _ = drag_offset.replace(0.0);
                        motion::animate_spring(&np_slide_y, 0.0, motion::spring_gentle());
                        motion::animate_spring(&np_scale, 1.0, motion::spring_gentle());
                        motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                    }
                })
                .on_responder_terminate(move |_: &ResponderEvent| {
                    let _ = drag_offset.replace(0.0);
                    motion::animate_spring(&np_slide_y, 0.0, motion::spring_gentle());
                    motion::animate_spring(&np_scale, 1.0, motion::spring_gentle());
                    motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                })
                .child(dynamic(move || {
                    let current_form = form.get();
                    untracked(move || pages::now_playing::now_playing_overlay(store, current_form))
                }))
        }))
        // App-level adaptive modal host (`SourceSwitcher`, `CreatePlaylist`, `RenamePlaylist`,
        // `InstallPlugin`, `ConfirmPluginInstall`, `PluginConfig`, `QrSignIn`, `WebSignIn`).
        .child(pages::render_active_modal(store))
        // App-level transient toast overlay.
        .child(toast_overlay(store.toasts))
}

fn rubberband_up(dy: f32) -> f32 {
    if dy >= 0.0 {
        dy
    } else {
        -((-dy) * 0.22).min(36.0)
    }
}
