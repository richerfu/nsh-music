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
    let close_gen = stored(0_u64);

    effect(move || {
        let open = store.now_playing_open.get();
        let close_token = close_gen.with(|g| {
            *g += 1;
            *g
        });

        if open {
            untracked(|| {
                np_mounted.set(true);
                if reduce_motion() {
                    np_slide_y.set(0.0);
                    np_scale.set(1.0);
                    np_fade.set(1.0);
                } else {
                    let grow = store.settings.with_untracked(|s| s.grow_from_cover);
                    if grow {
                        np_scale.set(0.90);
                        np_slide_y.set(64.0);
                    } else {
                        np_scale.set(1.0);
                        np_slide_y.set(48.0);
                    }
                    np_fade.set(0.0);
                    motion::animate_spring(&np_slide_y, 0.0, motion::spring_settle());
                    motion::animate_spring(&np_scale, 1.0, motion::spring_settle());
                    motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                }
            });
        } else if np_mounted.get_untracked() {
            untracked(|| {
                if reduce_motion() {
                    np_mounted.set(false);
                    np_slide_y.set(0.0);
                    np_scale.set(1.0);
                    np_fade.set(0.0);
                } else {
                    let grow = store.settings.with_untracked(|s| s.grow_from_cover);
                    motion::animate_timing(&np_slide_y, 72.0, motion::timing_fast());
                    if grow {
                        motion::animate_timing(&np_scale, 0.94, motion::timing_fast());
                    }
                    motion::animate_timing(&np_fade, 0.0, motion::timing_fast());
                    spawn(async move {
                        timer(Duration::from_millis(240)).await;
                        if close_gen.with(|g| *g == close_token)
                            && !store.now_playing_open.get_untracked()
                        {
                            np_mounted.set(false);
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
                        DeviceForm::Pad { landscape } => pad::pad_shell(store, landscape).key(
                            if landscape {
                                "device-shell-pad-landscape"
                            } else {
                                "device-shell-pad-portrait"
                            },
                        ),
                        DeviceForm::Phone => phone::phone_shell(store).key("device-shell-phone"),
                    })
                })),
        )
        // Persistent animated Now Playing overlay with drag-down-to-dismiss.
        .child(when(np_mounted, move || {
            view()
                .test_id("now-playing-host")
                .absolute()
                .inset_0()
                .bind_translate_y(&np_slide_y)
                .bind_scale(&np_scale)
                .bind_opacity(&np_fade)
                .claim_touches(TouchClaim::on_pan(PanActivation::vertical(
                    px(10.0),
                    px(12.0),
                )))
                .on_responder_move(move |ev: &ResponderEvent| {
                    let dy = ev.gesture.delta.y.0.max(0.0);
                    np_slide_y.set(dy);
                    let progress = (dy / 260.0).clamp(0.0, 1.0);
                    np_fade.set((1.0 - progress * 0.45).clamp(0.55, 1.0));
                    if store.settings.with_untracked(|s| s.grow_from_cover) {
                        np_scale.set((1.0 - progress * 0.05).clamp(0.94, 1.0));
                    }
                })
                .on_responder_release(move |ev: &ResponderEvent| {
                    let dy = ev.gesture.delta.y.0.max(0.0);
                    if dy > 110.0 || ev.gesture.vy > 700.0 {
                        store.close_now_playing();
                    } else {
                        motion::animate_spring(&np_slide_y, 0.0, motion::spring_settle());
                        motion::animate_spring(&np_scale, 1.0, motion::spring_settle());
                        motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                    }
                })
                .on_responder_terminate(move |_: &ResponderEvent| {
                    motion::animate_spring(&np_slide_y, 0.0, motion::spring_settle());
                    motion::animate_spring(&np_scale, 1.0, motion::spring_settle());
                    motion::animate_timing(&np_fade, 1.0, motion::timing_fast());
                })
                .child(dynamic(move || {
                    let current_form = form.get();
                    untracked(move || {
                        pages::now_playing::now_playing_overlay(store, current_form)
                    })
                }))
        }))
        // App-level adaptive modal host (`SourceSwitcher`, `CreatePlaylist`, `RenamePlaylist`,
        // `InstallPlugin`, `ConfirmPluginInstall`, `PluginConfig`, `QrSignIn`, `WebSignIn`).
        .child(pages::render_active_modal(store))
        // App-level transient toast overlay.
        .child(toast_overlay(store.toasts))
}
