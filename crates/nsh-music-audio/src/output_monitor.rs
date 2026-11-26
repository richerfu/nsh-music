//! Cross-platform hardware audio output routing, disconnect, and system interruption
//! monitors wired into [`CpalOutput`](super::CpalOutput).
//!
//! Each target platform maps native OS notifications onto [`AudioOutputEvent`]:
//! - **macOS**: CoreAudio `AudioObjectAddPropertyListener` for default output changes,
//!   device liveness (`DeviceIsAlive`), and wired headphone jack transitions
//!   (`JackIsConnected`).
//! - **iOS**: `AVAudioSession` playback configuration + `NSNotificationCenter` observers
//!   for `AVAudioSessionRouteChangeNotification` and
//!   `AVAudioSessionInterruptionNotification`.
//! - **Windows**: WASAPI COM `IMMNotificationClient` registered with `IMMDeviceEnumerator`.
//! - **Linux**: Cooperative `cpal` device-list polling only while playback is active
//!   (0% CPU when paused).
//! - **Android**: Cooperative JNI `AudioManager` polling (`getDevices(2)` + `getMode()`)
//!   while active or awaiting post-call resume.

use std::sync::{Arc, Mutex};

use super::{AudioOutputEvent, OutputEventCallback};

fn emit_event(event_cb: &Arc<Mutex<Option<OutputEventCallback>>>, event: AudioOutputEvent) {
    if let Some(cb) = event_cb.lock().ok().and_then(|g| g.clone()) {
        cb(event);
    }
}

/// Platform-independent pure decision logic compiled and unit-tested on every host.
#[allow(dead_code)]
pub(crate) mod decision {
    use super::AudioOutputEvent;

    // -------------------------------------------------------------------------
    // macOS CoreAudio transport / alive / jack decision rules
    // -------------------------------------------------------------------------

    pub(crate) const MACOS_TRANSPORT_UNKNOWN: u32 = 0;
    pub(crate) const MACOS_TRANSPORT_BUILT_IN: u32 = 1_651_274_862; // 'bltn'
    pub(crate) const MACOS_TRANSPORT_AGGREGATE: u32 = 1_735_554_416; // 'grup'
    pub(crate) const MACOS_TRANSPORT_VIRTUAL: u32 = 1_986_622_068; // 'virt'
    pub(crate) const MACOS_TRANSPORT_PCI: u32 = 1_885_563_168; // 'pci '
    pub(crate) const MACOS_TRANSPORT_USB: u32 = 1_970_496_032; // 'usb '
    pub(crate) const MACOS_TRANSPORT_FIREWIRE: u32 = 825_440_564; // '1394'
    pub(crate) const MACOS_TRANSPORT_BLUETOOTH: u32 = 1_651_275_109; // 'blue'
    pub(crate) const MACOS_TRANSPORT_BLUETOOTH_LE: u32 = 1_651_271_009; // 'blea'
    pub(crate) const MACOS_TRANSPORT_HDMI: u32 = 1_751_412_073; // 'hdmi'
    pub(crate) const MACOS_TRANSPORT_DISPLAYPORT: u32 = 1_685_090_932; // 'dprt'
    pub(crate) const MACOS_TRANSPORT_AIRPLAY: u32 = 1_634_300_528; // 'airp'
    pub(crate) const MACOS_TRANSPORT_AVB: u32 = 1_700_886_114; // 'eavb'
    pub(crate) const MACOS_TRANSPORT_THUNDERBOLT: u32 = 1_953_002_862; // 'thun'

    /// Returns `true` when `transport` represents a removable/wireless/external audio
    /// transport whose removal as default output should auto-pause playback.
    pub(crate) fn macos_is_removable_transport(transport: u32) -> bool {
        matches!(
            transport,
            MACOS_TRANSPORT_BLUETOOTH
                | MACOS_TRANSPORT_BLUETOOTH_LE
                | MACOS_TRANSPORT_USB
                | MACOS_TRANSPORT_AIRPLAY
                | MACOS_TRANSPORT_HDMI
                | MACOS_TRANSPORT_DISPLAYPORT
                | MACOS_TRANSPORT_THUNDERBOLT
                | MACOS_TRANSPORT_FIREWIRE
                | MACOS_TRANSPORT_AVB
        )
    }

    /// Evaluates `kAudioDevicePropertyDeviceIsAlive` (`0` = dead/disconnected).
    pub(crate) fn macos_eval_is_alive(is_alive: u32) -> Option<AudioOutputEvent> {
        if is_alive == 0 {
            Some(AudioOutputEvent::DeviceDisconnected)
        } else {
            None
        }
    }

    /// Evaluates wired `kAudioDevicePropertyJackIsConnected` (`1 -> 0` unplug transition).
    pub(crate) fn macos_eval_jack_transition(
        prev_connected: Option<bool>,
        now_connected: bool,
    ) -> Option<AudioOutputEvent> {
        if prev_connected == Some(true) && !now_connected {
            Some(AudioOutputEvent::DeviceDisconnected)
        } else {
            None
        }
    }

    /// Evaluates `kAudioHardwarePropertyDefaultOutputDevice` switching `prev_id -> new_id`.
    ///
    /// Emits `DeviceDisconnected` only when the previous device went offline (`!prev_alive`)
    /// or was a removable external/wireless transport (`Bluetooth`, `BluetoothLE`, `USB`,
    /// `AirPlay`, etc.). Does **not** pause when switching away from an alive Built-in Speaker
    /// (for example when Bluetooth headphones are newly connected).
    pub(crate) fn macos_eval_default_device_change(
        prev_id: u32,
        new_id: u32,
        prev_alive: bool,
        prev_transport: u32,
    ) -> Option<AudioOutputEvent> {
        if prev_id == 0 || new_id == prev_id {
            return None;
        }
        if !prev_alive || macos_is_removable_transport(prev_transport) {
            Some(AudioOutputEvent::DeviceDisconnected)
        } else {
            None
        }
    }

    // -------------------------------------------------------------------------
    // iOS AVAudioSession route change & interruption decoding
    // -------------------------------------------------------------------------

    pub(crate) const IOS_ROUTE_CHANGE_OLD_DEVICE_UNAVAILABLE: usize = 2;
    pub(crate) const IOS_INTERRUPTION_ENDED: usize = 0;
    pub(crate) const IOS_INTERRUPTION_BEGAN: usize = 1;
    pub(crate) const IOS_INTERRUPTION_OPTION_SHOULD_RESUME: usize = 1;

    pub(crate) fn ios_eval_route_change(reason: usize) -> Option<AudioOutputEvent> {
        if reason == IOS_ROUTE_CHANGE_OLD_DEVICE_UNAVAILABLE {
            Some(AudioOutputEvent::DeviceDisconnected)
        } else {
            None
        }
    }

    pub(crate) fn ios_eval_interruption(
        interruption_type: usize,
        options: usize,
    ) -> Option<AudioOutputEvent> {
        match interruption_type {
            IOS_INTERRUPTION_BEGAN => Some(AudioOutputEvent::SystemPause),
            IOS_INTERRUPTION_ENDED if (options & IOS_INTERRUPTION_OPTION_SHOULD_RESUME) != 0 => {
                Some(AudioOutputEvent::SystemResume)
            }
            _ => None,
        }
    }

    // -------------------------------------------------------------------------
    // Windows WASAPI IMMNotificationClient state machine
    // -------------------------------------------------------------------------

    pub(crate) const WIN_DEVICE_STATE_ACTIVE: u32 = 1;

    #[derive(Debug, Default)]
    pub(crate) struct WindowsEndpointTracker {
        active_id: Option<String>,
    }

    impl WindowsEndpointTracker {
        pub(crate) fn new(active_id: Option<String>) -> Self {
            Self { active_id }
        }

        pub(crate) fn active_id(&self) -> Option<&str> {
            self.active_id.as_deref()
        }

        fn matches_active(&self, device_id: &str) -> bool {
            self.active_id
                .as_deref()
                .is_some_and(|cur| cur.eq_ignore_ascii_case(device_id))
        }

        pub(crate) fn on_device_removed(&mut self, removed_id: &str) -> Option<AudioOutputEvent> {
            if self.matches_active(removed_id) {
                self.active_id = None;
                Some(AudioOutputEvent::DeviceDisconnected)
            } else {
                None
            }
        }

        pub(crate) fn on_device_state_changed(
            &mut self,
            device_id: &str,
            new_state: u32,
        ) -> Option<AudioOutputEvent> {
            if self.matches_active(device_id) && new_state != WIN_DEVICE_STATE_ACTIVE {
                self.active_id = None;
                Some(AudioOutputEvent::DeviceDisconnected)
            } else {
                None
            }
        }

        pub(crate) fn on_default_device_changed(
            &mut self,
            is_render_multimedia: bool,
            new_id: Option<&str>,
            prev_still_active: bool,
        ) -> Option<AudioOutputEvent> {
            if !is_render_multimedia {
                return None;
            }
            let normalized_new = new_id.filter(|s| !s.is_empty());
            let changed = match (self.active_id.as_deref(), normalized_new) {
                (Some(prev), Some(next)) => !prev.eq_ignore_ascii_case(next),
                (Some(_), None) => true,
                (None, _) => false,
            };
            let ev = if changed && !prev_still_active {
                Some(AudioOutputEvent::DeviceDisconnected)
            } else {
                None
            };
            self.active_id = normalized_new.map(str::to_owned);
            ev
        }
    }

    // -------------------------------------------------------------------------
    // Linux cpal output device disappearance tracker
    // -------------------------------------------------------------------------

    #[derive(Debug, Default)]
    pub(crate) struct LinuxDeviceWatcher {
        active_name: Option<String>,
    }

    impl LinuxDeviceWatcher {
        pub(crate) fn new(active_name: Option<String>) -> Self {
            Self { active_name }
        }

        pub(crate) fn reset(&mut self, active_name: Option<String>) {
            self.active_name = active_name;
        }

        pub(crate) fn has_active(&self) -> bool {
            self.active_name.is_some()
        }

        pub(crate) fn poll(
            &mut self,
            available_names: &[String],
            has_default: bool,
        ) -> Option<AudioOutputEvent> {
            if !has_default {
                return self
                    .active_name
                    .take()
                    .map(|_| AudioOutputEvent::DeviceDisconnected);
            }
            if let Some(ref active) = self.active_name
                && !available_names.iter().any(|name| name == active)
            {
                self.active_name = None;
                return Some(AudioOutputEvent::DeviceDisconnected);
            }
            None
        }
    }

    // -------------------------------------------------------------------------
    // Android AudioManager external output & call-mode state machine
    // -------------------------------------------------------------------------

    pub(crate) const ANDROID_TYPE_WIRED_HEADSET: i32 = 3;
    pub(crate) const ANDROID_TYPE_WIRED_HEADPHONES: i32 = 4;
    pub(crate) const ANDROID_TYPE_BLUETOOTH_SCO: i32 = 7;
    pub(crate) const ANDROID_TYPE_BLUETOOTH_A2DP: i32 = 8;
    pub(crate) const ANDROID_TYPE_USB_DEVICE: i32 = 11;
    pub(crate) const ANDROID_TYPE_USB_HEADSET: i32 = 22;
    pub(crate) const ANDROID_TYPE_BLE_HEADSET: i32 = 26;

    pub(crate) const ANDROID_MODE_NORMAL: i32 = 0;
    pub(crate) const ANDROID_MODE_RINGTONE: i32 = 1;
    pub(crate) const ANDROID_MODE_IN_CALL: i32 = 2;
    pub(crate) const ANDROID_MODE_IN_COMMUNICATION: i32 = 3;

    pub(crate) fn android_is_external_output_type(device_type: i32) -> bool {
        matches!(
            device_type,
            ANDROID_TYPE_WIRED_HEADSET
                | ANDROID_TYPE_WIRED_HEADPHONES
                | ANDROID_TYPE_BLUETOOTH_SCO
                | ANDROID_TYPE_BLUETOOTH_A2DP
                | ANDROID_TYPE_USB_DEVICE
                | ANDROID_TYPE_USB_HEADSET
                | ANDROID_TYPE_BLE_HEADSET
        )
    }

    pub(crate) fn android_is_call_mode(mode: i32) -> bool {
        matches!(
            mode,
            ANDROID_MODE_RINGTONE | ANDROID_MODE_IN_CALL | ANDROID_MODE_IN_COMMUNICATION
        )
    }

    #[derive(Debug, Default)]
    pub(crate) struct AndroidAudioState {
        active_external_ids: Vec<i32>,
        paused_for_call: bool,
    }

    impl AndroidAudioState {
        pub(crate) fn seed(&mut self, external_ids: Vec<i32>, mode: i32) {
            self.active_external_ids = external_ids;
            self.paused_for_call = android_is_call_mode(mode);
        }

        pub(crate) fn reseed_devices(&mut self, external_ids: Vec<i32>) {
            self.active_external_ids = external_ids;
            self.paused_for_call = false;
        }

        pub(crate) fn is_paused_for_call(&self) -> bool {
            self.paused_for_call
        }

        pub(crate) fn update(
            &mut self,
            current_external_ids: &[i32],
            current_mode: i32,
        ) -> Vec<AudioOutputEvent> {
            let mut events = Vec::new();
            let external_removed = self
                .active_external_ids
                .iter()
                .any(|prev_id| !current_external_ids.contains(prev_id));
            if external_removed {
                events.push(AudioOutputEvent::DeviceDisconnected);
            }
            self.active_external_ids.clear();
            self.active_external_ids
                .extend_from_slice(current_external_ids);

            if android_is_call_mode(current_mode) {
                if !self.paused_for_call {
                    self.paused_for_call = true;
                    events.push(AudioOutputEvent::SystemPause);
                }
            } else if current_mode == ANDROID_MODE_NORMAL && self.paused_for_call {
                self.paused_for_call = false;
                events.push(AudioOutputEvent::SystemResume);
            }

            events
        }
    }
}

// =============================================================================
// macOS CoreAudio implementation
// =============================================================================

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::mem;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use coreaudio_sys::{
        AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectHasProperty,
        AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener, OSStatus,
        UInt32, kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyJackIsConnected,
        kAudioDevicePropertyScopeOutput, kAudioDevicePropertyTransportType,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioObjectUnknown,
    };

    use super::decision::{
        MACOS_TRANSPORT_UNKNOWN, macos_eval_default_device_change, macos_eval_is_alive,
        macos_eval_jack_transition,
    };
    use super::{OutputEventCallback, emit_event};

    const NO_ERR: OSStatus = 0;

    fn addr_default_output() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn addr_device_is_alive() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceIsAlive,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn addr_device_transport() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyTransportType,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn addr_jack_connected() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyJackIsConnected,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn query_u32_property(
        object_id: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Option<u32> {
        if object_id == kAudioObjectUnknown {
            return None;
        }
        let mut val: u32 = 0;
        let mut size = mem::size_of::<u32>() as UInt32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                address,
                0,
                ptr::null(),
                &mut size,
                (&mut val as *mut u32).cast::<c_void>(),
            )
        };
        if status == NO_ERR { Some(val) } else { None }
    }

    fn query_default_output_device() -> AudioObjectID {
        query_u32_property(kAudioObjectSystemObject, &addr_default_output())
            .unwrap_or(kAudioObjectUnknown)
    }

    fn query_device_is_alive(device_id: AudioObjectID) -> bool {
        query_u32_property(device_id, &addr_device_is_alive())
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    fn query_device_transport(device_id: AudioObjectID) -> u32 {
        query_u32_property(device_id, &addr_device_transport()).unwrap_or(MACOS_TRANSPORT_UNKNOWN)
    }

    fn device_has_jack_property(device_id: AudioObjectID) -> bool {
        if device_id == kAudioObjectUnknown {
            return false;
        }
        let addr = addr_jack_connected();
        unsafe { AudioObjectHasProperty(device_id, &addr) != 0 }
    }

    fn query_jack_connected(device_id: AudioObjectID) -> Option<bool> {
        if !device_has_jack_property(device_id) {
            return None;
        }
        query_u32_property(device_id, &addr_jack_connected()).map(|v| v != 0)
    }

    struct MacOsInner {
        active_device_id: AudioObjectID,
        active_transport: u32,
        jack_connected: Option<bool>,
        default_listener_installed: bool,
        alive_listener_installed: bool,
        jack_listener_installed: bool,
    }

    struct MacOsMonitorState {
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
        stopping: AtomicBool,
        inner: Mutex<MacOsInner>,
    }

    fn attach_device_listeners(
        device_id: AudioObjectID,
        client_data: *mut c_void,
    ) -> (bool, bool, Option<bool>) {
        if device_id == kAudioObjectUnknown {
            return (false, false, None);
        }
        let alive_addr = addr_device_is_alive();
        let alive_ok = unsafe {
            AudioObjectAddPropertyListener(
                device_id,
                &alive_addr,
                Some(macos_property_listener),
                client_data,
            ) == NO_ERR
        };
        let jack_val = query_jack_connected(device_id);
        let jack_ok = if jack_val.is_some() {
            let jack_addr = addr_jack_connected();
            unsafe {
                AudioObjectAddPropertyListener(
                    device_id,
                    &jack_addr,
                    Some(macos_property_listener),
                    client_data,
                ) == NO_ERR
            }
        } else {
            false
        };
        (alive_ok, jack_ok, jack_val)
    }

    fn detach_device_listeners(
        device_id: AudioObjectID,
        alive_installed: bool,
        jack_installed: bool,
        client_data: *mut c_void,
    ) {
        if device_id == kAudioObjectUnknown {
            return;
        }
        if alive_installed {
            let alive_addr = addr_device_is_alive();
            let _ = unsafe {
                AudioObjectRemovePropertyListener(
                    device_id,
                    &alive_addr,
                    Some(macos_property_listener),
                    client_data,
                )
            };
        }
        if jack_installed {
            let jack_addr = addr_jack_connected();
            let _ = unsafe {
                AudioObjectRemovePropertyListener(
                    device_id,
                    &jack_addr,
                    Some(macos_property_listener),
                    client_data,
                )
            };
        }
    }

    unsafe extern "C" fn macos_property_listener(
        in_object_id: AudioObjectID,
        in_number_addresses: UInt32,
        in_addresses: *const AudioObjectPropertyAddress,
        in_client_data: *mut c_void,
    ) -> OSStatus {
        if in_client_data.is_null() || in_addresses.is_null() || in_number_addresses == 0 {
            return NO_ERR;
        }
        let _ = std::panic::catch_unwind(|| {
            let state = unsafe { &*(in_client_data as *const MacOsMonitorState) };
            if state.stopping.load(Ordering::SeqCst) {
                return;
            }
            let addresses =
                unsafe { std::slice::from_raw_parts(in_addresses, in_number_addresses as usize) };

            for addr in addresses {
                if state.stopping.load(Ordering::SeqCst) {
                    return;
                }
                if addr.mSelector == kAudioDevicePropertyDeviceIsAlive {
                    let is_alive =
                        query_u32_property(in_object_id, &addr_device_is_alive()).unwrap_or(0);
                    let event = state.inner.lock().ok().and_then(|inner| {
                        if inner.active_device_id == in_object_id {
                            macos_eval_is_alive(is_alive)
                        } else {
                            None
                        }
                    });
                    if let Some(ev) = event {
                        log::info!(
                            "CoreAudio active device {in_object_id} is_alive=0 -> DeviceDisconnected"
                        );
                        emit_event(&state.event_cb, ev);
                    }
                } else if addr.mSelector == kAudioDevicePropertyJackIsConnected {
                    let Some(now_connected) = query_jack_connected(in_object_id) else {
                        continue;
                    };
                    let event = state.inner.lock().ok().and_then(|mut inner| {
                        if inner.active_device_id != in_object_id {
                            return None;
                        }
                        let ev = macos_eval_jack_transition(inner.jack_connected, now_connected);
                        inner.jack_connected = Some(now_connected);
                        ev
                    });
                    if let Some(ev) = event {
                        log::info!(
                            "CoreAudio wired jack disconnected on device {in_object_id} -> DeviceDisconnected"
                        );
                        emit_event(&state.event_cb, ev);
                    }
                } else if addr.mSelector == kAudioHardwarePropertyDefaultOutputDevice {
                    let new_id = query_default_output_device();
                    let snapshot = state.inner.lock().ok().map(|inner| {
                        (
                            inner.active_device_id,
                            inner.active_transport,
                            inner.alive_listener_installed,
                            inner.jack_listener_installed,
                        )
                    });
                    let Some((prev_id, prev_transport, prev_alive_l, prev_jack_l)) = snapshot
                    else {
                        continue;
                    };
                    if new_id == prev_id {
                        continue;
                    }

                    let prev_alive = query_device_is_alive(prev_id);
                    let ev = macos_eval_default_device_change(
                        prev_id,
                        new_id,
                        prev_alive,
                        prev_transport,
                    );

                    detach_device_listeners(prev_id, prev_alive_l, prev_jack_l, in_client_data);

                    let (new_transport, new_alive_l, new_jack_l, new_jack_val) = if !state
                        .stopping
                        .load(Ordering::SeqCst)
                        && new_id != kAudioObjectUnknown
                    {
                        let transport = query_device_transport(new_id);
                        let (a, j, jv) = attach_device_listeners(new_id, in_client_data);
                        if state.stopping.load(Ordering::SeqCst) {
                            detach_device_listeners(new_id, a, j, in_client_data);
                            (MACOS_TRANSPORT_UNKNOWN, false, false, None)
                        } else {
                            (transport, a, j, jv)
                        }
                    } else {
                        (MACOS_TRANSPORT_UNKNOWN, false, false, None)
                    };

                    if let Ok(mut inner) = state.inner.lock() {
                        inner.active_device_id = new_id;
                        inner.active_transport = new_transport;
                        inner.jack_connected = new_jack_val;
                        inner.alive_listener_installed = new_alive_l;
                        inner.jack_listener_installed = new_jack_l;
                    }

                    if let Some(ev) = ev {
                        log::info!(
                            "CoreAudio default output changed {prev_id} -> {new_id} (prev_alive={prev_alive}, prev_transport={prev_transport:#x}) -> {ev:?}"
                        );
                        emit_event(&state.event_cb, ev);
                    } else {
                        log::debug!(
                            "CoreAudio default output switched {prev_id} -> {new_id} without disconnect pause (prev_alive={prev_alive}, prev_transport={prev_transport:#x})"
                        );
                    }
                }
            }
        });
        NO_ERR
    }

    pub(crate) struct OutputMonitor {
        state_ptr: *mut MacOsMonitorState,
    }

    impl OutputMonitor {
        pub(crate) fn start(
            event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            _active_device_name: Option<String>,
        ) -> Self {
            let active_device_id = query_default_output_device();
            let active_transport = query_device_transport(active_device_id);
            let boxed = Box::new(MacOsMonitorState {
                event_cb,
                stopping: AtomicBool::new(false),
                inner: Mutex::new(MacOsInner {
                    active_device_id,
                    active_transport,
                    jack_connected: None,
                    default_listener_installed: false,
                    alive_listener_installed: false,
                    jack_listener_installed: false,
                }),
            });
            let state_ptr = Box::into_raw(boxed);
            let client_data = state_ptr.cast::<c_void>();

            let default_addr = addr_default_output();
            let default_ok = unsafe {
                AudioObjectAddPropertyListener(
                    kAudioObjectSystemObject,
                    &default_addr,
                    Some(macos_property_listener),
                    client_data,
                ) == NO_ERR
            };
            let (alive_ok, jack_ok, jack_val) =
                attach_device_listeners(active_device_id, client_data);

            if let Ok(mut inner) = unsafe { &*state_ptr }.inner.lock() {
                inner.default_listener_installed = default_ok;
                inner.alive_listener_installed = alive_ok;
                inner.jack_listener_installed = jack_ok;
                inner.jack_connected = jack_val;
            }

            Self { state_ptr }
        }

        pub(crate) fn set_active(&mut self, _active: bool) {}

        pub(crate) fn poll_interval(&self, _stream_running: bool) -> Option<Duration> {
            None
        }

        pub(crate) fn poll_tick(&mut self) {}
    }

    impl Drop for OutputMonitor {
        fn drop(&mut self) {
            if self.state_ptr.is_null() {
                return;
            }
            let client_data = self.state_ptr.cast::<c_void>();
            let state = unsafe { &*self.state_ptr };
            state.stopping.store(true, Ordering::SeqCst);

            let default_installed = state
                .inner
                .lock()
                .map(|mut inner| {
                    let was = inner.default_listener_installed;
                    inner.default_listener_installed = false;
                    was
                })
                .unwrap_or(false);
            if default_installed {
                let default_addr = addr_default_output();
                let _ = unsafe {
                    AudioObjectRemovePropertyListener(
                        kAudioObjectSystemObject,
                        &default_addr,
                        Some(macos_property_listener),
                        client_data,
                    )
                };
            }

            let per_device = state.inner.lock().ok().map(|mut inner| {
                let res = (
                    inner.active_device_id,
                    inner.alive_listener_installed,
                    inner.jack_listener_installed,
                );
                inner.alive_listener_installed = false;
                inner.jack_listener_installed = false;
                res
            });
            if let Some((dev_id, alive_l, jack_l)) = per_device {
                detach_device_listeners(dev_id, alive_l, jack_l, client_data);
            }

            unsafe {
                drop(Box::from_raw(self.state_ptr));
            }
            self.state_ptr = ptr::null_mut();
        }
    }
}

// =============================================================================
// iOS AVAudioSession + NSNotificationCenter implementation
// =============================================================================

#[cfg(target_os = "ios")]
mod platform {
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, NSObjectProtocol, ProtocolObject};
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSNumber, NSString};

    use super::decision::{ios_eval_interruption, ios_eval_route_change};
    use super::{OutputEventCallback, emit_event};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    fn configure_ios_audio_session() {
        let Some(cls) = AnyClass::get(c"AVAudioSession") else {
            log::warn!("iOS AVAudioSession class unavailable");
            return;
        };
        unsafe {
            let session: *mut AnyObject = msg_send![cls, sharedInstance];
            if session.is_null() {
                log::warn!("iOS AVAudioSession.sharedInstance returned nil");
                return;
            }
            let category = NSString::from_str("AVAudioSessionCategoryPlayback");
            let mut cat_err: *mut AnyObject = std::ptr::null_mut();
            let ok_cat: Bool = msg_send![session, setCategory: &*category, error: &mut cat_err];
            if !ok_cat.as_bool() {
                log::warn!("iOS AVAudioSession setCategory(Playback) failed");
            }

            let mut act_err: *mut AnyObject = std::ptr::null_mut();
            let ok_act: Bool = msg_send![session, setActive: Bool::YES, error: &mut act_err];
            if !ok_act.as_bool() {
                log::warn!("iOS AVAudioSession setActive(YES) failed");
            }
        }
    }

    fn user_info_usize(notification: &NSNotification, key: &str) -> Option<usize> {
        let user_info = notification.userInfo()?;
        let ns_key = NSString::from_str(key);
        let raw_val = user_info.objectForKey(ns_key.as_ref())?;
        let num = raw_val.downcast::<NSNumber>().ok()?;
        Some(num.unsignedIntegerValue())
    }

    pub(crate) struct OutputMonitor {
        route_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
        interruption_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
        _route_block: Option<RcBlock<dyn Fn(NonNull<NSNotification>)>>,
        _interruption_block: Option<RcBlock<dyn Fn(NonNull<NSNotification>)>>,
    }

    impl OutputMonitor {
        pub(crate) fn start(
            event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            _active_device_name: Option<String>,
        ) -> Self {
            configure_ios_audio_session();

            let center = NSNotificationCenter::defaultCenter();

            let route_cb = Arc::clone(&event_cb);
            let route_block = RcBlock::new(move |note_ptr: NonNull<NSNotification>| {
                let note = unsafe { note_ptr.as_ref() };
                if let Some(reason) = user_info_usize(note, "AVAudioSessionRouteChangeReasonKey")
                    && let Some(ev) = ios_eval_route_change(reason)
                {
                    log::info!("iOS AVAudioSession route change reason={reason} -> {ev:?}");
                    emit_event(&route_cb, ev);
                }
            });
            let route_name = NSString::from_str("AVAudioSessionRouteChangeNotification");
            let route_observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(&route_name),
                    None,
                    None,
                    &route_block,
                )
            };

            let intr_cb = event_cb;
            let interruption_block = RcBlock::new(move |note_ptr: NonNull<NSNotification>| {
                let note = unsafe { note_ptr.as_ref() };
                if let Some(intr_type) = user_info_usize(note, "AVAudioSessionInterruptionTypeKey")
                {
                    let options =
                        user_info_usize(note, "AVAudioSessionInterruptionOptionKey").unwrap_or(0);
                    if let Some(ev) = ios_eval_interruption(intr_type, options) {
                        log::info!(
                            "iOS AVAudioSession interruption type={intr_type} options={options} -> {ev:?}"
                        );
                        emit_event(&intr_cb, ev);
                    }
                }
            });
            let intr_name = NSString::from_str("AVAudioSessionInterruptionNotification");
            let interruption_observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(&intr_name),
                    None,
                    None,
                    &interruption_block,
                )
            };

            Self {
                route_observer: Some(route_observer),
                interruption_observer: Some(interruption_observer),
                _route_block: Some(route_block),
                _interruption_block: Some(interruption_block),
            }
        }

        pub(crate) fn set_active(&mut self, _active: bool) {}

        pub(crate) fn poll_interval(&self, _stream_running: bool) -> Option<Duration> {
            None
        }

        pub(crate) fn poll_tick(&mut self) {}
    }

    impl Drop for OutputMonitor {
        fn drop(&mut self) {
            let center = NSNotificationCenter::defaultCenter();
            if let Some(obs) = self.route_observer.take() {
                unsafe { center.removeObserver(obs.as_ref()) };
            }
            if let Some(obs) = self.interruption_observer.take() {
                unsafe { center.removeObserver(obs.as_ref()) };
            }
        }
    }
}

// =============================================================================
// Windows WASAPI IMMNotificationClient implementation
// =============================================================================

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDeviceEnumerator,
        IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eMultimedia,
        eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows_core::{PCWSTR, Result as WinResult, implement};

    use super::decision::WindowsEndpointTracker;
    use super::{OutputEventCallback, emit_event};

    fn query_default_render_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
        let dev = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }.ok()?;
        let pwstr = unsafe { dev.GetId() }.ok()?;
        let id = unsafe { pwstr.to_string() }.ok();
        unsafe { CoTaskMemFree(Some(pwstr.0.cast())) };
        id
    }

    fn query_endpoint_is_active(id: &str) -> bool {
        let Ok(enumerator): WinResult<IMMDeviceEnumerator> =
            (unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) })
        else {
            return false;
        };
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        let Ok(dev) = (unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }) else {
            return false;
        };
        unsafe { dev.GetState() }
            .map(|state| state == DEVICE_STATE_ACTIVE)
            .unwrap_or(false)
    }

    #[implement(IMMNotificationClient)]
    struct WinNotificationClient {
        tracker: Mutex<WindowsEndpointTracker>,
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
    }

    impl IMMNotificationClient_Impl for WinNotificationClient_Impl {
        fn OnDeviceStateChanged(
            &self,
            pwstrdeviceid: &PCWSTR,
            dwnewstate: DEVICE_STATE,
        ) -> WinResult<()> {
            if pwstrdeviceid.is_null() {
                return Ok(());
            }
            if let Ok(id) = unsafe { pwstrdeviceid.to_string() } {
                let ev = self
                    .tracker
                    .lock()
                    .ok()
                    .and_then(|mut t| t.on_device_state_changed(&id, dwnewstate.0));
                if let Some(ev) = ev {
                    log::info!(
                        "WASAPI endpoint state changed ({id} -> state={}) -> {ev:?}",
                        dwnewstate.0
                    );
                    emit_event(&self.event_cb, ev);
                }
            }
            Ok(())
        }

        fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> WinResult<()> {
            Ok(())
        }

        fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> WinResult<()> {
            if pwstrdeviceid.is_null() {
                return Ok(());
            }
            if let Ok(id) = unsafe { pwstrdeviceid.to_string() } {
                let ev = self
                    .tracker
                    .lock()
                    .ok()
                    .and_then(|mut t| t.on_device_removed(&id));
                if let Some(ev) = ev {
                    log::info!("WASAPI active endpoint removed ({id}) -> {ev:?}");
                    emit_event(&self.event_cb, ev);
                }
            }
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            pwstrdefaultdeviceid: &PCWSTR,
        ) -> WinResult<()> {
            let is_render_multimedia = flow == eRender && role == eMultimedia;
            if !is_render_multimedia {
                return Ok(());
            }
            let new_id = if pwstrdefaultdeviceid.is_null() {
                None
            } else {
                unsafe { pwstrdefaultdeviceid.to_string() }.ok()
            };
            let prev_id = self
                .tracker
                .lock()
                .ok()
                .and_then(|t| t.active_id().map(str::to_owned));
            let prev_still_active = prev_id
                .as_deref()
                .map(query_endpoint_is_active)
                .unwrap_or(false);

            let ev = self.tracker.lock().ok().and_then(|mut t| {
                t.on_default_device_changed(true, new_id.as_deref(), prev_still_active)
            });
            if let Some(ev) = ev {
                log::info!(
                    "WASAPI default render endpoint changed (prev_still_active={prev_still_active}) -> {ev:?}"
                );
                emit_event(&self.event_cb, ev);
            }
            Ok(())
        }

        fn OnPropertyValueChanged(
            &self,
            _pwstrdeviceid: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> WinResult<()> {
            Ok(())
        }
    }

    pub(crate) struct OutputMonitor {
        enumerator: Option<IMMDeviceEnumerator>,
        client: Option<IMMNotificationClient>,
        com_initialized: bool,
    }

    impl OutputMonitor {
        pub(crate) fn start(
            event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            _active_device_name: Option<String>,
        ) -> Self {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let com_initialized = hr == S_OK || hr == S_FALSE;
            if !com_initialized && hr != RPC_E_CHANGED_MODE {
                log::warn!("WASAPI monitor CoInitializeEx failed: {hr:?}");
                return Self {
                    enumerator: None,
                    client: None,
                    com_initialized: false,
                };
            }

            let enumerator: IMMDeviceEnumerator =
                match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
                    Ok(e) => e,
                    Err(err) => {
                        log::warn!("WASAPI monitor CoCreateInstance failed: {err}");
                        if com_initialized {
                            unsafe { CoUninitialize() };
                        }
                        return Self {
                            enumerator: None,
                            client: None,
                            com_initialized: false,
                        };
                    }
                };

            let active_id = query_default_render_id(&enumerator);
            let client: IMMNotificationClient = WinNotificationClient {
                tracker: Mutex::new(WindowsEndpointTracker::new(active_id)),
                event_cb,
            }
            .into();

            if let Err(err) = unsafe { enumerator.RegisterEndpointNotificationCallback(&client) } {
                log::warn!("WASAPI RegisterEndpointNotificationCallback failed: {err}");
                drop(client);
                drop(enumerator);
                if com_initialized {
                    unsafe { CoUninitialize() };
                }
                return Self {
                    enumerator: None,
                    client: None,
                    com_initialized: false,
                };
            }

            Self {
                enumerator: Some(enumerator),
                client: Some(client),
                com_initialized,
            }
        }

        pub(crate) fn set_active(&mut self, _active: bool) {}

        pub(crate) fn poll_interval(&self, _stream_running: bool) -> Option<Duration> {
            None
        }

        pub(crate) fn poll_tick(&mut self) {}
    }

    impl Drop for OutputMonitor {
        fn drop(&mut self) {
            if let (Some(enumerator), Some(client)) = (self.enumerator.take(), self.client.take()) {
                let _ = unsafe { enumerator.UnregisterEndpointNotificationCallback(&client) };
            }
            if self.com_initialized {
                unsafe { CoUninitialize() };
            }
        }
    }
}

// =============================================================================
// Linux cpal cooperative active-only polling implementation
// =============================================================================

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
mod platform {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait};

    use super::decision::LinuxDeviceWatcher;
    use super::{OutputEventCallback, emit_event};

    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    fn current_default_device_name() -> Option<String> {
        cpal::default_host()
            .default_output_device()
            .and_then(|d| d.name().ok())
    }

    pub(crate) struct OutputMonitor {
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
        watcher: LinuxDeviceWatcher,
    }

    impl OutputMonitor {
        pub(crate) fn start(
            event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            active_device_name: Option<String>,
        ) -> Self {
            let initial = active_device_name.or_else(current_default_device_name);
            Self {
                event_cb,
                watcher: LinuxDeviceWatcher::new(initial),
            }
        }

        pub(crate) fn set_active(&mut self, active: bool) {
            if active && !self.watcher.has_active() {
                self.watcher.reset(current_default_device_name());
            }
        }

        pub(crate) fn poll_interval(&self, stream_running: bool) -> Option<Duration> {
            if stream_running {
                Some(POLL_INTERVAL)
            } else {
                None
            }
        }

        pub(crate) fn poll_tick(&mut self) {
            let host = cpal::default_host();
            let has_default = host.default_output_device().is_some();
            let available: Vec<String> = host
                .output_devices()
                .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default();
            if let Some(ev) = self.watcher.poll(&available, has_default) {
                log::info!("Linux cpal output device disappeared -> {ev:?}");
                emit_event(&self.event_cb, ev);
            }
        }
    }
}

// =============================================================================
// Android JNI AudioManager cooperative polling implementation
// =============================================================================

#[cfg(target_os = "android")]
mod platform {
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use jni::JavaVM;
    use jni::objects::{JObjectArray, JValue};
    use jni::sys::{JNI_OK, jint, jsize};

    use super::decision::{AndroidAudioState, android_is_external_output_type};
    use super::{OutputEventCallback, emit_event};

    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const GET_DEVICES_OUTPUTS: jint = 2;

    static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

    fn discover_java_vm() -> Option<&'static JavaVM> {
        if let Some(vm) = JAVA_VM.get() {
            return Some(vm);
        }

        type GetCreatedVmsFn =
            unsafe extern "C" fn(*mut *mut jni::sys::JavaVM, jsize, *mut jsize) -> jint;

        let mut sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"JNI_GetCreatedJavaVMs".as_ptr()) };
        if sym.is_null() {
            let art = unsafe { libc::dlopen(c"libart.so".as_ptr(), libc::RTLD_LAZY) };
            if !art.is_null() {
                sym = unsafe { libc::dlsym(art, c"JNI_GetCreatedJavaVMs".as_ptr()) };
            }
        }
        if sym.is_null() {
            return None;
        }

        let get_vms: GetCreatedVmsFn = unsafe { std::mem::transmute(sym) };
        let mut vm_ptr: *mut jni::sys::JavaVM = std::ptr::null_mut();
        let mut count: jsize = 0;
        let status = unsafe { get_vms(&mut vm_ptr, 1, &mut count) };
        if status != JNI_OK || count < 1 || vm_ptr.is_null() {
            return None;
        }

        let wrapped = unsafe { JavaVM::from_raw(vm_ptr) }.ok()?;
        let _ = JAVA_VM.set(wrapped);
        JAVA_VM.get()
    }

    fn clear_exception(env: &mut jni::JNIEnv<'_>) -> bool {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            true
        } else {
            false
        }
    }

    fn query_android_audio_snapshot() -> Option<(Vec<i32>, i32)> {
        let vm = discover_java_vm()?;
        let mut env = vm.attach_current_thread().ok()?;

        let app_val = env.call_static_method(
            "android/app/ActivityThread",
            "currentApplication",
            "()Landroid/app/Application;",
            &[],
        );
        if clear_exception(&mut env) {
            return None;
        }
        let app_obj = app_val.ok()?.l().ok()?;
        if app_obj.is_null() {
            return None;
        }

        let service_key = env.new_string("audio").ok()?;
        let mgr_val = env.call_method(
            &app_obj,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service_key)],
        );
        if clear_exception(&mut env) {
            return None;
        }
        let audio_mgr = mgr_val.ok()?.l().ok()?;
        if audio_mgr.is_null() {
            return None;
        }

        let mode_val = env.call_method(&audio_mgr, "getMode", "()I", &[]);
        if clear_exception(&mut env) {
            return None;
        }
        let mode = mode_val.ok()?.i().ok()?;

        let devices_val = env.call_method(
            &audio_mgr,
            "getDevices",
            "(I)[Landroid/media/AudioDeviceInfo;",
            &[JValue::Int(GET_DEVICES_OUTPUTS)],
        );
        if clear_exception(&mut env) {
            return None;
        }
        let devices_obj = devices_val.ok()?.l().ok()?;
        let mut external_ids = Vec::new();
        if !devices_obj.is_null() {
            let devices_arr = JObjectArray::from(devices_obj);
            let len = env.get_array_length(&devices_arr).unwrap_or(0);
            for idx in 0..len {
                let Ok(elem) = env.get_object_array_element(&devices_arr, idx) else {
                    clear_exception(&mut env);
                    continue;
                };
                if elem.is_null() {
                    continue;
                }
                let dev_type = env
                    .call_method(&elem, "getType", "()I", &[])
                    .ok()
                    .and_then(|v| v.i().ok());
                if clear_exception(&mut env) {
                    let _ = env.delete_local_ref(elem);
                    continue;
                }
                if let Some(t) = dev_type
                    && android_is_external_output_type(t)
                {
                    let dev_id = env
                        .call_method(&elem, "getId", "()I", &[])
                        .ok()
                        .and_then(|v| v.i().ok());
                    if !clear_exception(&mut env)
                        && let Some(id) = dev_id
                    {
                        external_ids.push(id);
                    }
                }
                let _ = env.delete_local_ref(elem);
            }
        }

        Some((external_ids, mode))
    }

    pub(crate) struct OutputMonitor {
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
        state: AndroidAudioState,
    }

    impl OutputMonitor {
        pub(crate) fn start(
            event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            _active_device_name: Option<String>,
        ) -> Self {
            let mut state = AndroidAudioState::default();
            if let Some((external_ids, mode)) = query_android_audio_snapshot() {
                state.seed(external_ids, mode);
            }
            Self { event_cb, state }
        }

        pub(crate) fn set_active(&mut self, active: bool) {
            if active && let Some((external_ids, _)) = query_android_audio_snapshot() {
                self.state.reseed_devices(external_ids);
            }
        }

        pub(crate) fn poll_interval(&self, stream_running: bool) -> Option<Duration> {
            if stream_running || self.state.is_paused_for_call() {
                Some(POLL_INTERVAL)
            } else {
                None
            }
        }

        pub(crate) fn poll_tick(&mut self) {
            let Some((external_ids, mode)) = query_android_audio_snapshot() else {
                return;
            };
            for ev in self.state.update(&external_ids, mode) {
                log::info!("Android AudioManager state change (mode={mode}) -> {ev:?}");
                emit_event(&self.event_cb, ev);
            }
        }
    }
}

// =============================================================================
// Fallback no-op monitor for other non-OHOS / non-wasm cpal targets
// =============================================================================

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "android"
)))]
mod platform {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::OutputEventCallback;

    pub(crate) struct OutputMonitor;

    impl OutputMonitor {
        pub(crate) fn start(
            _event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
            _active_device_name: Option<String>,
        ) -> Self {
            Self
        }

        pub(crate) fn set_active(&mut self, _active: bool) {}

        pub(crate) fn poll_interval(&self, _stream_running: bool) -> Option<Duration> {
            None
        }

        pub(crate) fn poll_tick(&mut self) {}
    }
}

pub(crate) use platform::OutputMonitor;

#[cfg(test)]
mod tests {
    use super::AudioOutputEvent;
    use super::decision::{
        ANDROID_MODE_IN_CALL, ANDROID_MODE_IN_COMMUNICATION, ANDROID_MODE_NORMAL,
        ANDROID_MODE_RINGTONE, ANDROID_TYPE_BLE_HEADSET, ANDROID_TYPE_BLUETOOTH_A2DP,
        ANDROID_TYPE_BLUETOOTH_SCO, ANDROID_TYPE_USB_DEVICE, ANDROID_TYPE_USB_HEADSET,
        ANDROID_TYPE_WIRED_HEADPHONES, ANDROID_TYPE_WIRED_HEADSET, AndroidAudioState,
        IOS_INTERRUPTION_BEGAN, IOS_INTERRUPTION_ENDED, IOS_INTERRUPTION_OPTION_SHOULD_RESUME,
        IOS_ROUTE_CHANGE_OLD_DEVICE_UNAVAILABLE, LinuxDeviceWatcher, MACOS_TRANSPORT_AIRPLAY,
        MACOS_TRANSPORT_BLUETOOTH, MACOS_TRANSPORT_BLUETOOTH_LE, MACOS_TRANSPORT_BUILT_IN,
        MACOS_TRANSPORT_USB, MACOS_TRANSPORT_VIRTUAL, WIN_DEVICE_STATE_ACTIVE,
        WindowsEndpointTracker, android_is_call_mode, android_is_external_output_type,
        ios_eval_interruption, ios_eval_route_change, macos_eval_default_device_change,
        macos_eval_is_alive, macos_eval_jack_transition, macos_is_removable_transport,
    };

    #[test]
    fn test_macos_transport_alive_jack_and_default_switch_rules() {
        assert!(macos_is_removable_transport(MACOS_TRANSPORT_BLUETOOTH));
        assert!(macos_is_removable_transport(MACOS_TRANSPORT_BLUETOOTH_LE));
        assert!(macos_is_removable_transport(MACOS_TRANSPORT_USB));
        assert!(macos_is_removable_transport(MACOS_TRANSPORT_AIRPLAY));
        assert!(!macos_is_removable_transport(MACOS_TRANSPORT_BUILT_IN));
        assert!(!macos_is_removable_transport(MACOS_TRANSPORT_VIRTUAL));

        // DeviceIsAlive 0 -> DeviceDisconnected; non-zero -> None.
        assert_eq!(
            macos_eval_is_alive(0),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(macos_eval_is_alive(1), None);

        // Wired JackIsConnected: only 1 -> 0 triggers disconnect.
        assert_eq!(
            macos_eval_jack_transition(Some(true), false),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(macos_eval_jack_transition(Some(false), true), None);
        assert_eq!(macos_eval_jack_transition(None, false), None);
        assert_eq!(macos_eval_jack_transition(Some(true), true), None);

        // Switching from alive Built-in Speaker to newly connected Bluetooth headphones must NOT pause.
        assert_eq!(
            macos_eval_default_device_change(10, 20, true, MACOS_TRANSPORT_BUILT_IN),
            None
        );
        // Switching away from removable Bluetooth/USB/AirPlay device pauses even if still reported alive briefly.
        assert_eq!(
            macos_eval_default_device_change(20, 10, true, MACOS_TRANSPORT_BLUETOOTH),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(
            macos_eval_default_device_change(21, 10, true, MACOS_TRANSPORT_USB),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        // Switching away from a dead device always pauses regardless of transport.
        assert_eq!(
            macos_eval_default_device_change(10, 20, false, MACOS_TRANSPORT_BUILT_IN),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        // Same device ID or unknown previous ID -> no-op.
        assert_eq!(
            macos_eval_default_device_change(10, 10, false, MACOS_TRANSPORT_BLUETOOTH),
            None
        );
        assert_eq!(
            macos_eval_default_device_change(0, 20, false, MACOS_TRANSPORT_BLUETOOTH),
            None
        );
    }

    #[test]
    fn test_ios_route_change_and_interruption_decoding() {
        // Route change reason 2 = OldDeviceUnavailable -> DeviceDisconnected; others ignored.
        assert_eq!(
            ios_eval_route_change(IOS_ROUTE_CHANGE_OLD_DEVICE_UNAVAILABLE),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(ios_eval_route_change(1), None); // NewDeviceAvailable
        assert_eq!(ios_eval_route_change(3), None); // CategoryChange

        // Interruption began -> SystemPause.
        assert_eq!(
            ios_eval_interruption(IOS_INTERRUPTION_BEGAN, 0),
            Some(AudioOutputEvent::SystemPause)
        );
        // Interruption ended with ShouldResume bit -> SystemResume.
        assert_eq!(
            ios_eval_interruption(
                IOS_INTERRUPTION_ENDED,
                IOS_INTERRUPTION_OPTION_SHOULD_RESUME
            ),
            Some(AudioOutputEvent::SystemResume)
        );
        // Interruption ended without ShouldResume bit -> stay paused.
        assert_eq!(ios_eval_interruption(IOS_INTERRUPTION_ENDED, 0), None);
    }

    #[test]
    fn test_windows_endpoint_tracker_transitions() {
        let mut tracker = WindowsEndpointTracker::new(Some("{0.0.0.00000000}.{bt-headset}".into()));

        // Non-matching device removal does not disconnect.
        assert_eq!(tracker.on_device_removed("{0.0.0.00000000}.{other}"), None);
        // Active device removed (case-insensitive) -> DeviceDisconnected.
        assert_eq!(
            tracker.on_device_removed("{0.0.0.00000000}.{BT-HEADSET}"),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(tracker.active_id(), None);

        // State change from ACTIVE -> UNPLUGGED (8) pauses.
        let mut tracker = WindowsEndpointTracker::new(Some("{ep-1}".into()));
        assert_eq!(
            tracker.on_device_state_changed("{ep-1}", WIN_DEVICE_STATE_ACTIVE),
            None
        );
        assert_eq!(
            tracker.on_device_state_changed("{ep-1}", 8),
            Some(AudioOutputEvent::DeviceDisconnected)
        );

        // Default device switch: plugging in headphones while speakers stay active must NOT pause.
        let mut tracker = WindowsEndpointTracker::new(Some("{speakers}".into()));
        assert_eq!(
            tracker.on_default_device_changed(false, Some("{mic}"), false),
            None
        );
        assert_eq!(
            tracker.on_default_device_changed(true, Some("{bt-headphones}"), true),
            None
        );
        assert_eq!(tracker.active_id(), Some("{bt-headphones}"));

        // Default device switch when previous BT headphones are no longer active -> DeviceDisconnected.
        assert_eq!(
            tracker.on_default_device_changed(true, Some("{speakers}"), false),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(tracker.active_id(), Some("{speakers}"));
    }

    #[test]
    fn test_linux_device_watcher_removal_and_default_vanished() {
        let mut watcher = LinuxDeviceWatcher::new(Some("AirPods Pro".into()));
        let present = vec![
            "Built-in Analog Stereo".to_owned(),
            "AirPods Pro".to_owned(),
        ];
        assert_eq!(watcher.poll(&present, true), None);

        // Active device disappears from output_devices() -> DeviceDisconnected (once).
        let after_unplug = vec!["Built-in Analog Stereo".to_owned()];
        assert_eq!(
            watcher.poll(&after_unplug, true),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(watcher.poll(&after_unplug, true), None);

        // Re-seed on speakers; default output vanishes completely -> DeviceDisconnected.
        watcher.reset(Some("Built-in Analog Stereo".into()));
        assert_eq!(
            watcher.poll(&[], false),
            Some(AudioOutputEvent::DeviceDisconnected)
        );
        assert_eq!(watcher.poll(&[], false), None);
    }

    #[test]
    fn test_android_external_device_diff_and_call_mode_transitions() {
        for ty in [
            ANDROID_TYPE_WIRED_HEADSET,
            ANDROID_TYPE_WIRED_HEADPHONES,
            ANDROID_TYPE_BLUETOOTH_SCO,
            ANDROID_TYPE_BLUETOOTH_A2DP,
            ANDROID_TYPE_USB_DEVICE,
            ANDROID_TYPE_USB_HEADSET,
            ANDROID_TYPE_BLE_HEADSET,
        ] {
            assert!(
                android_is_external_output_type(ty),
                "expected external: {ty}"
            );
        }
        assert!(!android_is_external_output_type(2)); // TYPE_BUILTIN_SPEAKER
        assert!(!android_is_external_output_type(1)); // TYPE_BUILTIN_EARPIECE

        assert!(android_is_call_mode(ANDROID_MODE_RINGTONE));
        assert!(android_is_call_mode(ANDROID_MODE_IN_CALL));
        assert!(android_is_call_mode(ANDROID_MODE_IN_COMMUNICATION));
        assert!(!android_is_call_mode(ANDROID_MODE_NORMAL));

        let mut state = AndroidAudioState::default();
        state.seed(vec![101], ANDROID_MODE_NORMAL);

        // Connecting a second external device does not pause.
        assert!(state.update(&[101, 202], ANDROID_MODE_NORMAL).is_empty());

        // Removing device 101 emits DeviceDisconnected.
        assert_eq!(
            state.update(&[202], ANDROID_MODE_NORMAL),
            vec![AudioOutputEvent::DeviceDisconnected]
        );
        // Steady state after removal emits nothing.
        assert!(state.update(&[202], ANDROID_MODE_NORMAL).is_empty());

        // Incoming phone call -> SystemPause (once), stays paused across RINGTONE -> IN_CALL -> IN_COMMUNICATION.
        assert_eq!(
            state.update(&[202], ANDROID_MODE_RINGTONE),
            vec![AudioOutputEvent::SystemPause]
        );
        assert!(state.is_paused_for_call());
        assert!(state.update(&[202], ANDROID_MODE_IN_CALL).is_empty());
        assert!(
            state
                .update(&[202], ANDROID_MODE_IN_COMMUNICATION)
                .is_empty()
        );

        // Call ends -> MODE_NORMAL -> SystemResume (once).
        assert_eq!(
            state.update(&[202], ANDROID_MODE_NORMAL),
            vec![AudioOutputEvent::SystemResume]
        );
        assert!(!state.is_paused_for_call());
        assert!(state.update(&[202], ANDROID_MODE_NORMAL).is_empty());
    }
}
