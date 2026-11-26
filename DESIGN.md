# nsh-music Design System & Motion Inventory

## Supported Platforms & Targets

- **Tier-1 Mobile**: HarmonyOS / OpenHarmony (`nsh-platform-harmonyos`, `OH_AudioRenderer`, ArkUI C-API), iOS / iPadOS (UIKit + CoreAudio), Android (Android Views + AAudio/Oboe).
- **Tier-1 Desktop**: macOS (AppKit + CoreAudio), Windows (Win32/Direct2D + WASAPI), Linux (GTK 4 + ALSA/Pulse).

## Adaptive Form Factors (`DeviceForm`)

1. **PC (`Desktop`, `desktop_idiom && w >= 900pt`)**: Custom 44pt hidden title bar (`TitleBarStyle::Hidden`, `.window_drag(true)`), collapsible `232pt / 68pt` sidebar, `84pt` bottom Player Bar with comet scrubber, compact `32pt` controls and `52pt` list rows.
2. **Pad (`Pad`, `Idiom::Pad | Idiom::Mac || (600..900)pt`)**: Landscape collapsible `72pt / 220pt` navigation rail + 4-column grid; Portrait left drawer `Sheet` + 3-column grid; `72pt` floating capsule player bar; all touch targets `>= 44pt`.
3. **Phone (`Phone`, `w < 600pt`)**: Safe-area header with Source Switcher capsule (opens bottom `Sheet`), horizontal plugin nav pills, 4-tab bottom navigation (`发现`, `电台播客`, `资料库`, `搜索`), `58pt` floating Mini-Player capsule, `>= 44pt` touch targets, `56–60pt` track rows.

## Motion Inventory (Required by NSH Design Guides)

| Screen / Element                                               | State Change                            | Tool & Tokens                                                                                             | Reduced Motion (`reduce_motion()`)    |
| -------------------------------------------------------------- | --------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| Controls & Buttons (`src/ui/controls.rs`)                      | Hover wash & press scale                | `transition_bg` (`120ms`), `transition_transform` (`180ms`, soft settle) + `.active(\|s\| s.scale(0.97))` | Instant background, scale `1.0`       |
| Play / Pause Icon (`PlayPauseButton`)                          | Play <-> Pause glyph morph              | `AnimatedValue::spring` (`SETTLE`) driving cross-scale & opacity                                          | Instant icon swap                     |
| Like Heart (`HeartButton`)                                     | Toggle liked state                      | `AnimatedValue::spring` (`POP`, stiffness `220`, damping `11`)                                            | Instant color change                  |
| Sidebar / Tab Selection (`desktop.rs`, `pad.rs`)               | Active destination changes              | `AnimatedValue::timing` (`240ms`, ease-in-out) / `LayoutAnimation` on collapse                            | Instant jump                          |
| Menus & Popovers (`src/ui/overlays.rs`)                        | Open / close anchored menu              | `popup_style` with opacity & scale (`180ms` out / `240ms` in)                                             | Instant show/hide                     |
| Now Playing Overlay (`now_playing/mod.rs`)                     | Open / dismiss                          | `AnimatedValue::spring` (`SETTLE`) vertical slide / bottom sheet                                          | Instant / short opacity fade          |
| Now Playing Ambient Backdrop (`backdrop.rs`)                   | Playback running / track tint change    | GPU `Shader` (or gradient fallback), `paused` when not playing                                            | Static radial gradient                |
| Retro Vinyl / Cassette (`retro.rs`)                            | Spinning platter & tonearm / tape reels | Canvas driven by playback position & `AnimatedValue` tonearm swing                                        | Static platter / engaged tonearm      |
| Track Row Equalizer & Scrubber (`track_row.rs`, `scrubber.rs`) | Active audio playback                   | Canvas 16-band FFT bar levels & comet glow head                                                           | Static play mark / solid progress bar |
| Lyrics Stage (`stage.rs`)                                      | Active line & word progression          | `AnimatedValue` word sweep & focus scale, `virtual_list.scroll_to_index`                                  | Highlight color & bold text only      |
