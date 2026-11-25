//! Reusable `nsh-music` UI kit built cleanly atop `nsh_core` and `nsh_base`.

pub mod controls;
pub mod icons;
pub mod media_card;
pub mod motion;
pub mod overlays;
pub mod scrubber;
pub mod theme;
pub mod track_row;

pub use controls::{
    artwork_box, compat_badge, compat_badge_text, empty_state, empty_state_with_actions,
    ghost_button, heart_button, icon_button, icon_button_sized, icon_button_tinted,
    loading_spinner, pill_button, play_pause_button, play_pause_button_buffering, primary_button,
    section_header, shimmer_block, shimmer_block_on_dark, skeleton_card, source_pill, styled_input,
    styled_select, styled_slider, styled_switch,
};
pub use icons::{Icon, icon, icon_reactive};
pub use media_card::{MediaCard, media_card};
pub use motion::{
    SPRING_GENTLE, SPRING_POP, SPRING_SETTLE, SPRING_SNAPPY, TIMING_AMBIENT, TIMING_BASE,
    TIMING_FAST, TIMING_INSTANT, animate_spring, animate_timing, press_scale, pulse_pop,
    spring_gentle, spring_pop, spring_settle, spring_snappy, timing_ambient, timing_base,
    timing_fast, timing_instant, transition_ms,
};
pub use overlays::{
    ContextMenuAction, ToastController, ToastKind, ToastPayload, adaptive_modal,
    confirm_destructive, styled_context_menu, styled_menu, toast_overlay,
};
pub use scrubber::{Scrubber, scrubber};
pub use theme::{
    DeviceForm, Elevated, Elevation, artwork_backdrop_colors, artwork_fog_colors,
    artwork_np_accent, artwork_stage_bg, artwork_tint, artwork_wash, colors,
    default_artwork_accent, detect_device_form, font_size, format_bytes, lossless_gold, mix_color,
    radius, raw_accent_from_rgb, space,
};
pub use track_row::{TrackRow, format_duration_ms, mini_equalizer, track_row};
