//! Adaptive application shells across Desktop, Pad, and Phone form factors.

pub mod desktop;
pub mod device;
pub mod pad;
pub mod phone;

pub use device::device_shell;
