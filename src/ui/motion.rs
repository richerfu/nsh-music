//! Motion presets and reduced-motion-safe animation helpers matching the
//! Motion Inventory in `DESIGN.md`.

use nsh_core::{
    animation::{Curve, EaseMode},
    prelude::*,
};

/// Snappy spring for buttons, interactive controls, and quick micro-interactions.
pub const SPRING_SNAPPY: Spring = Spring {
    stiffness: 460.0,
    damping: 34.0,
    mass: 1.0,
    velocity: None,
    frame_rate: None,
};

/// Gentle settle spring for Now Playing overlay slides, play/pause morphs, and
/// sheet/panel transitions.
pub const SPRING_GENTLE: Spring = Spring {
    stiffness: 280.0,
    damping: 29.0,
    mass: 1.0,
    velocity: None,
    frame_rate: None,
};

/// Playful bounce spring for the Like Heart toggle (`stiffness: 220, damping: 11`).
pub const SPRING_POP: Spring = Spring {
    stiffness: 220.0,
    damping: 11.0,
    mass: 1.0,
    velocity: None,
    frame_rate: None,
};

/// Critically-settled spring for indicator slides and Play/Pause glyph transitions.
pub const SPRING_SETTLE: Spring = Spring {
    stiffness: 320.0,
    damping: 30.0,
    mass: 1.0,
    velocity: None,
    frame_rate: None,
};

/// Fast hover/color response (`120ms`).
pub const TIMING_INSTANT: Timing = Timing::new(120.0, Easing::Curve(Curve::Cubic, EaseMode::Out));

/// Fast UI state transition (`180ms`, soft cubic settle).
pub const TIMING_FAST: Timing = Timing::new(180.0, Easing::Curve(Curve::Cubic, EaseMode::Out));

/// Standard navigation / sidebar / popup transition (`240ms`, ease-in-out).
pub const TIMING_BASE: Timing = Timing::new(240.0, Easing::Curve(Curve::Cubic, EaseMode::InOut));

/// Slow ambient mood crossfade for artwork tint & backdrop transitions (`480ms`).
pub const TIMING_AMBIENT: Timing = Timing::new(480.0, Easing::Curve(Curve::Cubic, EaseMode::InOut));

/// Snappy spring preset, returning `None` when the user enabled Reduce Motion.
pub fn spring_snappy() -> Option<Spring> {
    if reduce_motion() {
        None
    } else {
        Some(SPRING_SNAPPY)
    }
}

/// Gentle spring preset, returning `None` when Reduce Motion is active.
pub fn spring_gentle() -> Option<Spring> {
    if reduce_motion() {
        None
    } else {
        Some(SPRING_GENTLE)
    }
}

/// Pop spring preset (e.g. Like Heart), returning `None` when Reduce Motion is active.
pub fn spring_pop() -> Option<Spring> {
    if reduce_motion() {
        None
    } else {
        Some(SPRING_POP)
    }
}

/// Settle spring preset (Play/Pause morph, Now Playing overlay), returning
/// `None` when Reduce Motion is active.
pub fn spring_settle() -> Option<Spring> {
    if reduce_motion() {
        None
    } else {
        Some(SPRING_SETTLE)
    }
}

/// Fast timing preset (`180ms`, collapsed to `0ms` under Reduce Motion).
pub fn timing_fast() -> Timing {
    if reduce_motion() {
        Timing::new(0.0, Easing::Linear)
    } else {
        TIMING_FAST
    }
}

/// Instant hover/feedback timing preset (`120ms`, collapsed to `0ms` under Reduce Motion).
pub fn timing_instant() -> Timing {
    if reduce_motion() {
        Timing::new(0.0, Easing::Linear)
    } else {
        TIMING_INSTANT
    }
}

/// Base navigation timing preset (`240ms`, collapsed to `0ms` under Reduce Motion).
pub fn timing_base() -> Timing {
    if reduce_motion() {
        Timing::new(0.0, Easing::Linear)
    } else {
        TIMING_BASE
    }
}

/// Ambient backdrop/mood timing preset (`480ms`, collapsed to `0ms` under Reduce Motion).
pub fn timing_ambient() -> Timing {
    if reduce_motion() {
        Timing::new(0.0, Easing::Linear)
    } else {
        TIMING_AMBIENT
    }
}

/// Returns `ms` normally, or `0.0` when Reduce Motion is enabled.
pub fn transition_ms(ms: f32) -> f32 {
    if reduce_motion() { 0.0 } else { ms }
}

/// Active press scale (`0.97` normally, `1.0` under Reduce Motion).
pub fn press_scale() -> f32 {
    if reduce_motion() { 1.0 } else { 0.97 }
}

/// Animates `value` to `target` with `spring`, snapping instantaneously when
/// `reduce_motion()` is active or `spring` is `None`.
pub fn animate_spring(value: &AnimatedValue, target: f32, spring: Option<Spring>) {
    match spring {
        Some(s) if !reduce_motion() => {
            let _ = value.spring(target, s);
        }
        _ => value.set(target),
    }
}

/// Animates `value` to `target` with `timing`, snapping instantaneously when
/// `reduce_motion()` is active or `timing.duration_ms <= 0.0`.
pub fn animate_timing(value: &AnimatedValue, target: f32, timing: Timing) {
    if reduce_motion() || timing.duration_ms <= 0.0 {
        value.set(target);
    } else {
        let _ = value.timing_with(target, timing);
    }
}

/// Triggers the heart-like pop impulse (`1.0 -> 1.26 -> 1.0`) unless Reduce Motion
/// is enabled.
pub fn pulse_pop(scale: &AnimatedValue) {
    if reduce_motion() {
        scale.set(1.0);
        return;
    }
    scale.set(1.26);
    let _ = scale.spring(1.0, SPRING_POP);
}
